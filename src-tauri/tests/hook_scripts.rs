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
use std::time::Duration;

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

// --- PermissionRequest hook (SPEC §16, R-16.1) ------------------------------

/// On Windows the ps1 always builds the perm file; the `.sh` needs a working
/// python3 or jq (see the script's parser detection). Skip the parser-dependent
/// perm assertions on a unix box lacking both (CI Linux has them).
#[cfg(windows)]
fn perm_supported() -> bool {
    true
}
#[cfg(not(windows))]
fn perm_supported() -> bool {
    let py = Command::new("python3")
        .args(["-c", "import json"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let jq = Command::new("jq")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    py || jq
}

/// Spawn the platform hook on a `PermissionRequest` payload with a short poll
/// deadline, optionally writing a decision once the perm file appears (from a
/// background thread, since the perm id is generated at runtime). Returns
/// `(exit_code, stdout, stderr)`.
fn run_perm_hook(
    data_dir: &Path,
    stdin: &str,
    deadline_ms: u64,
    answer: Option<(&str, Option<&str>)>,
) -> (i32, String, String) {
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
        .env("QUARTERDECK_PERM_POLL_DEADLINE_MS", deadline_ms.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn perm hook");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");

    let poller = answer.map(|(decision, reason)| {
        let dir = data_dir.to_path_buf();
        let decision = decision.to_string();
        let reason = reason.map(str::to_string);
        std::thread::spawn(move || {
            let perms = dir.join("perms");
            for _ in 0..500 {
                if let Ok(rd) = std::fs::read_dir(&perms) {
                    if let Some(path) = rd
                        .flatten()
                        .map(|e| e.path())
                        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
                    {
                        let id = path.file_stem().unwrap().to_string_lossy().into_owned();
                        let ans_dir = dir.join("perm-answers");
                        let _ = std::fs::create_dir_all(&ans_dir);
                        let body = match &reason {
                            Some(r) => format!(r#"{{"decision":"{decision}","reason":"{r}"}}"#),
                            None => format!(r#"{{"decision":"{decision}"}}"#),
                        };
                        let tmp = ans_dir.join(format!("{id}.json.tmp"));
                        let fin = ans_dir.join(format!("{id}.json"));
                        let _ = std::fs::write(&tmp, body);
                        let _ = std::fs::rename(&tmp, &fin);
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    });

    let out = child.wait_with_output().expect("wait for perm hook");
    if let Some(p) = poller {
        let _ = p.join();
    }
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const PERM_PAYLOAD: &str = r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm -rf ./build"},"session_id":"perm-sess-1","cwd":"/tmp/proj"}"#;

#[test]
fn permission_request_times_out_fail_open_and_writes_perm_file() {
    // R-16.1: no answer by the deadline → exit 0 with NO stdout (fail-open,
    // Claude Code falls through to its terminal dialog); the perm file lands in
    // <data>/perms/ (deck-down proof) and NOT in the spool.
    if !perm_supported() {
        eprintln!("skipping perm hook test: no python3/jq for the .sh");
        return;
    }
    let data = unique_data_dir("perm-timeout");
    let (code, stdout, stderr) = run_perm_hook(&data, PERM_PAYLOAD, 600, None);
    assert_eq!(code, 0, "perm hook fails open with exit 0; stderr={stderr}");
    assert!(
        stdout.trim().is_empty(),
        "no decision output on timeout; got {stdout:?}"
    );

    let perm_files: Vec<_> = std::fs::read_dir(data.join("perms"))
        .expect("perms dir exists")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    assert_eq!(perm_files.len(), 1, "one perm file written");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&perm_files[0]).unwrap()).unwrap();
    assert_eq!(v["kind"], "perm");
    assert_eq!(v["tool_name"], "Bash");
    assert_eq!(v["session_id"], "perm-sess-1");
    // tool_input is serialized as a JSON string (pretty-printed on the ps1/python
    // paths, compact on the jq fallback — the deck re-indents for display via
    // pretty_tool_input, R-16.2), capped to 2KB (R-16.1).
    let ti = v["tool_input"].as_str().expect("tool_input is a string");
    assert!(ti.contains("rm -rf ./build"), "tool_input preserved: {ti}");
    assert!(ti.len() <= 2048);
    // A perm never spools.
    assert!(spool_json_files(&data).is_empty(), "perm does not spool");

    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn permission_request_allow_emits_decision_json() {
    // R-16.1: an allow answer → the documented decision JSON on stdout, exit 0.
    if !perm_supported() {
        eprintln!("skipping perm hook test: no python3/jq for the .sh");
        return;
    }
    let data = unique_data_dir("perm-allow");
    let (code, stdout, _stderr) = run_perm_hook(&data, PERM_PAYLOAD, 20_000, Some(("allow", None)));
    assert_eq!(code, 0);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout is the decision JSON");
    assert_eq!(
        v["hookSpecificOutput"]["hookEventName"], "PermissionRequest",
        "decision names the event"
    );
    assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "allow");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn permission_request_deny_emits_decision_with_reason() {
    // R-16.1: a deny answer → deny behavior + the user's optional reason.
    if !perm_supported() {
        eprintln!("skipping perm hook test: no python3/jq for the .sh");
        return;
    }
    let data = unique_data_dir("perm-deny");
    let (code, stdout, _stderr) = run_perm_hook(
        &data,
        PERM_PAYLOAD,
        20_000,
        Some(("deny", Some("not safe here"))),
    );
    assert_eq!(code, 0);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout is the decision JSON");
    assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "deny");
    assert_eq!(
        v["hookSpecificOutput"]["decision"]["reason"], "not safe here",
        "deny carries the reason"
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn permission_request_defer_is_silent_fail_open() {
    // R-16.2/R-16.3: an "In terminal" / auto-defer decision → no output, exit 0
    // (the terminal dialog appears).
    if !perm_supported() {
        eprintln!("skipping perm hook test: no python3/jq for the .sh");
        return;
    }
    let data = unique_data_dir("perm-defer");
    let (code, stdout, _stderr) = run_perm_hook(&data, PERM_PAYLOAD, 20_000, Some(("defer", None)));
    assert_eq!(code, 0);
    assert!(
        stdout.trim().is_empty(),
        "defer emits nothing; got {stdout:?}"
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
