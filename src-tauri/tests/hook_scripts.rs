//! SPEC §11 "Hook script tests (real machine)": pipe fixture stdin into the real
//! platform hook script (`quarterdeck-hook.ps1` on Windows, `.sh` elsewhere) and
//! assert the spool envelope shape, atomicity (tmp+rename), exit-0-on-garbage,
//! and stdout/stderr silence (R-4.3). Plus `shellcheck` on the `.sh` when the
//! tool is available (a no-op skip otherwise, so the suite stays green on dev
//! machines without it — CI runs shellcheck explicitly on Linux).
//!
//! This runs the actual scripts a real Claude Code install would call, so a
//! regression in their contract is caught by `cargo test`, closing the gap the
//! spec's testing strategy names but nothing previously automated.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn hooks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("hooks")
}

fn unique_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "quarterdeck-hookscript-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs the platform hook script with `stdin`, isolated to `data_dir`. Returns
/// `(exit_code, stdout, stderr)`.
fn run_hook(data_dir: &Path, stdin: &str) -> (i32, String, String) {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("powershell.exe");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(hooks_dir().join("quarterdeck-hook.ps1"));
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new("bash");
        c.arg(hooks_dir().join("quarterdeck-hook.sh"));
        c
    };

    cmd.env("QUARTERDECK_DATA_DIR", data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn hook script");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait for hook script");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn spool_json_files(data_dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(data_dir.join("spool")) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn valid_event_writes_one_well_formed_spool_file_silently() {
    let data = unique_data_dir("valid");
    let fixture = r#"{"hook_event_name":"SessionStart","session_id":"hook-test-1","cwd":"/tmp/proj","source":"startup","transcript_path":"/tmp/t.jsonl"}"#;

    let (code, stdout, stderr) = run_hook(&data, fixture);
    assert_eq!(code, 0, "hook must always exit 0 (R-4.3); stderr={stderr}");
    assert!(
        stdout.is_empty(),
        "silent on stdout (R-4.3); got {stdout:?}"
    );
    assert!(
        stderr.is_empty(),
        "silent on stderr (R-4.3); got {stderr:?}"
    );

    let files = spool_json_files(&data);
    assert_eq!(files.len(), 1, "exactly one spool file written");
    let text = std::fs::read_to_string(&files[0]).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).expect("spool file is valid JSON");
    assert_eq!(v["v"], 1, "envelope version");
    assert_eq!(v["event"], "SessionStart");
    assert_eq!(v["payload"]["session_id"], "hook-test-1");
    assert!(v.get("extra").is_some(), "envelope carries extra{{}}");

    // Atomic tmp+rename leaves no stray temp file behind.
    let leftover: Vec<_> = std::fs::read_dir(data.join("spool"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftover.is_empty(), "no tmp file left behind: {leftover:?}");

    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn garbage_stdin_writes_nothing_and_exits_zero() {
    let data = unique_data_dir("garbage");
    for junk in [
        "not json at all",
        "{ this is not valid json at all }",
        "",
        "   \n  ",
        "[1,2,3]",
    ] {
        let (code, stdout, stderr) = run_hook(&data, junk);
        assert_eq!(
            code, 0,
            "garbage still exits 0 (R-4.3); input={junk:?} stderr={stderr}"
        );
        assert!(
            stdout.is_empty() && stderr.is_empty(),
            "silent on garbage (R-4.3); input={junk:?}"
        );
    }
    assert!(
        spool_json_files(&data).is_empty(),
        "garbage stdin writes nothing (R-4.3)"
    );
    let _ = std::fs::remove_dir_all(&data);
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn session_start_ancestor_walk_finds_a_real_claude_node_bun_pid() {
    // SPEC §11 "Hook script tests (real machine)": "SessionStart ancestor walk
    // finds a real claude PID." R-4.3's walk matches claude|node|bun, so drive
    // the real hook *through a real `node` process* and assert extra.claudePid
    // is that live node PID (the only claude/node/bun process in the chain).
    if !node_available() {
        eprintln!("skipping ancestor-walk test: `node` not on PATH");
        return;
    }
    let data = unique_data_dir("ancestor");
    let fixture = r#"{"hook_event_name":"SessionStart","session_id":"ancestor-test","cwd":"/tmp/proj","source":"startup","transcript_path":"/tmp/t.jsonl"}"#;

    // The platform hook invocation the Node wrapper will spawn as its child.
    #[cfg(windows)]
    let (prog, args): (String, Vec<String>) = (
        "powershell.exe".to_string(),
        vec![
            "-NoProfile".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-File".into(),
            hooks_dir()
                .join("quarterdeck-hook.ps1")
                .to_string_lossy()
                .into_owned(),
        ],
    );
    #[cfg(not(windows))]
    let (prog, args): (String, Vec<String>) = (
        "bash".to_string(),
        vec![hooks_dir()
            .join("quarterdeck-hook.sh")
            .to_string_lossy()
            .into_owned()],
    );

    // A tiny Node wrapper that spawns the hook as a DIRECT child and pipes the
    // fixture to its stdin. Because `node` spawns the hook directly (no shell),
    // the hook's nearest ancestor is this `node` process — exactly the
    // claude|node|bun match R-4.3's walk should capture.
    let wrapper = data.join("ancestor-wrapper.mjs");
    let wrapper_js = format!(
        "import {{ spawn }} from 'node:child_process';\n\
         const child = spawn({prog}, {args}, {{ stdio: ['pipe', 'ignore', 'ignore'] }});\n\
         child.stdin.write({fixture});\n\
         child.stdin.end();\n\
         child.on('exit', () => process.exit(0));\n\
         child.on('error', () => process.exit(0));\n",
        prog = serde_json::to_string(&prog).unwrap(),
        args = serde_json::to_string(&args).unwrap(),
        fixture = serde_json::to_string(fixture).unwrap(),
    );
    std::fs::write(&wrapper, wrapper_js).unwrap();

    let mut child = Command::new("node")
        .arg(&wrapper)
        .env("QUARTERDECK_DATA_DIR", &data)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn node wrapper");
    let node_pid = child.id();
    let status = child.wait().expect("wait node wrapper");
    assert!(status.success(), "node wrapper should exit 0");

    let files = spool_json_files(&data);
    assert_eq!(files.len(), 1, "the hook wrote exactly one spool file");
    let text = std::fs::read_to_string(&files[0]).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).expect("spool file is valid JSON");
    assert_eq!(v["event"], "SessionStart");
    let claude_pid = v["extra"]["claudePid"].as_u64();
    assert_eq!(
        claude_pid,
        Some(u64::from(node_pid)),
        "SessionStart ancestor walk must capture the live `node` ancestor PID (R-4.3); extra={:?}",
        v["extra"],
    );

    let _ = std::fs::remove_dir_all(&data);
}

#[cfg(unix)]
#[test]
fn shellcheck_passes_on_the_sh_when_available() {
    let available = Command::new("shellcheck")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !available {
        eprintln!("skipping shellcheck: `shellcheck` not on PATH (CI runs it on Linux)");
        return;
    }
    let out = Command::new("shellcheck")
        .arg(hooks_dir().join("quarterdeck-hook.sh"))
        .output()
        .expect("run shellcheck");
    assert!(
        out.status.success(),
        "shellcheck reported issues:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
