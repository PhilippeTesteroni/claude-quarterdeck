# Quarterdeck — Task breakdown & dependency graph

Each task lists: owned paths (exclusive), depends-on, spec refs, acceptance criteria (AC). Layers run sequentially; tasks inside a layer run in parallel with disjoint file ownership. Rule: `crates/deck-core/src/lib.rs`, `Cargo.toml`s, `src-tauri/src/main.rs` and all `mod` declarations are pre-created by T0 — later tasks only fill their own module files, never touch shared manifests (missing dep → note it in the task report for T7).

## L0 — Foundation (single agent)

### T0 Scaffold
- **Owns:** everything initially — cargo workspace (`crates/deck-core`, `src-tauri`), Tauri v2 init with tray + two windows (popup hidden-not-destroyed, ask), `ui/` Vite vanilla-TS multi-page (popup.html, ask.html), plugins wired (notification, autostart), ALL module files pre-created empty with `mod` declarations and public trait stubs (`Notifier`, `Clock`, `ProcessTable`), `ipc-contract.ts` + matching Rust types, placeholder tray icons (4 colored circles × light/dark), rustfmt/clippy/eslint configs, `.gitignore`, README stub, `scripts/` dir.
- **AC:** `cargo build`, `cargo clippy -- -D warnings`, `cargo test` (empty), `npm run tauri dev` shows tray + stub popup on this machine; `npm run tauri build` produces a runnable .exe. All deps declared in manifests up-front (from tasks' declared needs: serde, serde_json, notify, sysinfo, thiserror, tracing + appender, axum/tokio or rmcp, tempfile for tests).

## L1 — Modules (parallel after T0)

### T1 Core engine (deck-core)
- **Owns:** `crates/deck-core/src/{events,engine,naming,discovery,liveness}.rs` + `crates/deck-core/tests/**` (except hooks_config tests).
- **Spec:** §2 all, §3.5, §5, §6, R-4.5. **AC:** every R-2 rule has a test (injectable Clock); spool parse/quarantine (garbage, truncated, huge, unknown fields); naming precedence incl. Cyrillic + emoji cwd; discovery mtime logic; liveness via ProcessTable fake; 0 clippy warnings.

### T2 Hook scripts + installer
- **Owns:** `hooks/quarterdeck-hook.ps1`, `hooks/quarterdeck-hook.sh`, `crates/deck-core/src/hooks_config.rs`, its tests + `fixtures/settings/**`.
- **Spec:** §4, docs/hooks-facts.md. **AC:** merge/uninstall vs fixtures (missing, empty, foreign hooks on same events, malformed→refuse, BOM, CRLF); backups capped at 3; ps1 REALLY runs on this machine: piped fixture stdin → correct spool file, atomic (tmp+rename), silent, exit 0 on garbage; SessionStart parent-walk finds a plausible PID; sh passes shellcheck.

### T3 Tauri shell
- **Owns:** `src-tauri/src/{tray,windows,ipc,settings,watcher}.rs`.
- **Spec:** R-2.6, R-7.1, §3.3, §3.4, §3.6, §10.1. **AC:** tray swaps 5 icon variants at runtime; popup anchors/hides/no-taskbar; ask window always-on-top without stealing focus (verify `WS_EX_NOACTIVATE`-equivalent behavior); typed IPC matches `ipc-contract.ts`; settings load/save preserving unknown keys; watcher streams spool file paths (debounced) to a channel.

### T4 UI
- **Owns:** `ui/**`.
- **Spec:** §7 all (tokens, watch line, rows, settings pane, empty state, onboarding card R-10.2), R-8.3 visuals.
- **AC:** runs against mocked IPC (`ui/src/tauri-mock.ts`, activated by env) in a browser; all R-7 behaviors; light+dark via `prefers-color-scheme`; reduced-motion honored; the watch line implemented exactly as specced; screenshots (dark+light) saved to `docs/screenshots/`.

### T5 Notifications
- **Owns:** `src-tauri/src/notify.rs`.
- **Spec:** §9. **AC:** both toast classes fire on this Windows machine (manual script `scripts/demo-toasts.cmd` via `cargo run --example` or dev command); distinct sounds audible; AppUserModelID set; fake-notifier jsonl mode; throttle unit-tested with fake clock; mac branch code-complete behind `#[cfg(target_os)]` (compile-checked via `cargo check --target aarch64-apple-darwin` if toolchain allows, else code-reviewed).

### T6 MCP server + skill
- **Owns:** `src-tauri/src/mcp_server.rs`, `skills/quarterdeck/SKILL.md`, `scripts/mcp-client-test.mjs`.
- **Spec:** §8. **AC:** streamable-HTTP MCP server passes a handshake + `tools/list` + blocking `ask_user` round-trip driven by the Node test client (answer injected via an exposed test channel); 401 without bearer token; port/token persisted; SKILL.md follows the R-8.5 content contract; `notify_user` works.

## L2 — Integration (single agent, after all L1)

### T7 Composition
- **Owns:** `src-tauri/src/main.rs` (+lib.rs), seam edits marked `TODO(T7)`, missing-dep fixes in manifests.
- **Spec:** §3.1, R-5.4, §10. **AC:** dev run on this machine: hand-dropped fixture spool files drive rows/tray/toasts end-to-end; cold-start discovery picks up this machine's real recent sessions (read-only); onboarding gates all system changes; hook install writes correct entries into an isolated settings.json copy; ask flow works UI-side (popup + ask window + forced attention); logs rotate.

## L3 — Verification collateral (parallel after T7)

### T8 E2E suite + demo scripts
- **Owns:** `e2e/**`, `scripts/inject-events.mjs`, `scripts/live-smoke.md` (procedure).
- **Spec:** §11. **AC:** Playwright UI suite green (mocked IPC): empty→3-session lifecycle→attention recovery→ask flow→settings toggles→Cyrillic; real-app smoke script: launch built exe with isolated dirs, inject events, assert notifier-calls.jsonl + tray test hook, capture screenshots.

### T9 CI, packaging, docs
- **Owns:** `.github/workflows/ci.yml`, tauri.conf packaging polish (NSIS + dmg targets, icons), `README.md` (hero, features, install, hooks/MCP setup, limitations, privacy, screenshots), LICENSE (MIT, Philipp Gross).
- **AC:** README accurate against implementation; ci.yml: fmt+clippy+test+UI tests on windows-latest & macos-latest + `tauri build` artifacts; local `npm run tauri build` still green.

## Execution notes
- Commits: local only, author Philipp Gross <philyalapochka2@gmail.com>, no AI trailers, one commit per task (`T<N>: <summary>`).
- Models: T0/T3/T4/T5/T9 sonnet; T1/T2/T6/T7 stronger tier; verify/QA judges stronger tier.
- After L3: spec self-check loop (verifier per spec section → fixers → 2 clean rounds), then QA fleet (§11 + adversarial + exploratory), then live smoke, then report.
