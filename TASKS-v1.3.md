# Quarterdeck v1.3 — ordered task list (dogfooding batch 2)

Spec: Notion "Quarterdeck — v1.3 (2026-07-04)" (393d2970a7f2816eb2faf512780d414e) §26–§32, mirrored below by ref.
Build sequentially (heavy shared-file overlap on lib.rs / ipc.rs / engine.rs / ask.rs — NO parallel file mutation).
Each task: build → independent spec-self-check → fix loop until the checker passes. No git commits by agents (Philipp commits at the end).

Order (highest-value / dependency-first): T1 §30 → T2 §26.1 → T3 §26.2 → T4 §28 → T5 §31 → T6 §27 → T7 §29 → T8 §32.

---

## T1 — §30 status-stuck reverse gear (BUG, highest priority)
Owns: `crates/deck-core/src/engine.rs` (+ `crates/deck-core/tests/engine_recovery.rs`).
Decision LOCKED: **R-30.1 reverse gear** (not R-30.2).
- Add to `struct Session` (engine.rs:146): `recovery_promoted: bool` (init false in `Session::new` :225) and `last_transcript_mtime_ms: Option<u64>` (init None).
- In `set_hook_status` (:345): at the top, set `self.recovery_promoted = false; self.last_transcript_mtime_ms = None;` so EVERY real event (SessionStart/UserPromptSubmit/Stop/ask/dead) resets the flag. `poll_recovery` re-sets it AFTER its own `set_hook_status` call.
- `poll_recovery` (:912): in the Idle→Working promote (:945-946) and the Attention→Working promote (:932), after `set_hook_status(Working, now)` set `s.recovery_promoted = true; s.last_transcript_mtime_ms = Some(mtime);`.
- Add a new arm `Status::Working if s.recovery_promoted =>` in `poll_recovery`: read `mtime_of(tp)`; if `Some(m)` and `Some(m) != s.last_transcript_mtime_ms` → transcript advanced, still working, `s.last_transcript_mtime_ms = Some(m)`; else (no advance / mtime gone) → demote: `s.set_hook_status(Status::Idle, now)` (which clears the flag). Pending-ask guard at :917 already precedes the match.
- R-30.4: add `tracing::debug!(session_id, entered_at, mtime, now, "transcript recovery promote/demote")` at the promote + demote sites.
AC (R-30.3): new test in engine_recovery.rs — promote idle→working via a transcript advance, then hold the transcript quiescent across ≥1 further tick (registry idle) and assert the row returns to Idle; existing promote tests still green. `cargo test -p deck-core` + `cargo clippy -p deck-core -- -D warnings` clean.

## T2 — §26.1 no PowerShell console flash (BUG)
Owns: `src-tauri/src/focus.rs`, `src-tauri/src/foreground.rs`, `src-tauri/src/lib.rs` (spawn helper + the two `claude` spawns).
- Add a Windows-gated helper (e.g. in a small `util`/inline): apply `.creation_flags(0x08000000)` (CREATE_NO_WINDOW) via `std::os::windows::process::CommandExt` to a `Command`. Apply at: `foreground.rs:95`, `focus.rs:141`, `lib.rs:2386`, `lib.rs:2411`.
- Keep behavior identical otherwise (stdin pipe, stdout capture). macOS/linux paths unchanged.
AC: unit test (Windows-gated) asserting the helper sets the flag; `cargo build` + clippy clean. Manual note for Philipp: no black window on Stop/ask/perm/click/enable-questions.

## T3 — §26.2 click-to-focus raises terminal (BUG)
Owns: `src-tauri/src/focus.rs` (PS script only).
- In `build_focus_script` `Focus-Hwnd` (:89-92), before `SetForegroundWindow`, add a foreground-unlock: get the current foreground window's thread (`GetForegroundWindow`+`GetWindowThreadProcessId`), `AttachThreadInput(curThread, targetThread, true)`, `ShowWindow(SW_RESTORE)` + `BringWindowToTop` + `SetForegroundWindow`, then `AttachThreadInput(..., false)`. Fallback: a synthetic ALT keydown/up (`keybd_event 0x12`) around the call. Keep pid-validated HWND path + title-substring fallback.
- Keep the existing unit tests (`script_embeds_*`) green; extend the assertion to include the unlock call names.
AC: `cargo test -p` focus tests green; the snippet still runs and reports notfound with no window. **Live verification deferred to Philipp** across Windows Terminal / VS Code / conhost.

## T4 — §28 permission modal readability (BUG)
Owns: `hooks/quarterdeck-hook.ps1` (perm write), `src-tauri/src/lib.rs` (`pretty_tool_input` :464).
- ps1:85 → `ConvertTo-Json -Depth 20 -Compress` (compact; fits more under the 2KB cap). Keep the 2048 truncation but it now rarely triggers mid-JSON.
- `pretty_tool_input` (lib.rs:464): on the fallback (unparseable/verbatim) branch, decode the printable-ASCII `\uXXXX` escapes PowerShell over-produces — at least `'`→`'`, `<`→`<`, `>`→`>`, `&`→`&` (a small targeted replace, applied only when serde parse failed so valid JSON is untouched). Preserve QA round-6 truncation safety (never invent structure).
AC: unit test — a truncated/escaped `AskUserQuestion`-shaped input renders with real `'`/`<`/`>` and no `\uXXXX`; a valid short input still pretty-prints unchanged. cargo test + ps1 runs (shellcheck N/A for ps1; keep silent+exit-0). 

## T5 — §31 wordmark + fixed settings height (POLISH)
Owns: `ui/src/styles.css`, `ui/src/popup.ts`, `src-tauri/src/windows.rs` (only if a target-height helper is needed).
- R-31.1: `.qd-wordmark` (styles.css:140) 11px → ~13-14px; keep letterspacing/small-caps/clay/mono; verify header drag region + actions not crowded (popup + ask).
- R-31.2: `popup.ts:885` `if (settingsOpen) return;` → on settings-open resize to a FIXED `header + 5×rowH + footer`; on settings-close restore `measureAndResize` auto-height. Compute rowH from a measured `.qd-row` or a defined constant. Stay in the 160-560 band (windows.rs:36-37). Animate the resize (rAF tween driving `resize_popup`, or CSS on inner content); respect reduced-motion (styles.css:102 → snap instantly).
AC: Playwright — switching to settings shows a 5-row-tall pane at 0/2/8 sessions; switching back restores auto-height; reduced-motion snaps. `npm test` green; light+dark unaffected.

## T6 — §27 rename session by double-click (FEATURE)
Owns: `crates/deck-core/src/naming.rs`, `crates/deck-core/src/engine.rs`, `src-tauri/src/settings.rs`, `src-tauri/src/ipc.rs`, `src-tauri/src/lib.rs`, `ui/src/ipc-contract.ts`, `ui/src/popup.ts`, `ui/src/styles.css`.
Per spec §27 R-27.1..R-27.7. Override layer `<data>/session-names.json`; `override_name` highest in `title_with_override`; command `rename_session(sessionId,name)`; dblclick inline editor with stopPropagation + editor-survives-rebuild guard; prune on SessionEnd/remove_row; reuse `normalize_title`.
AC: cargo test (naming precedence with override wins; override survives via store map) + Playwright (dblclick → input → persists in mock) green; empty name clears; clippy clean.

## T7 — §29 multi-question / multi-select ask_user (FEATURE)
Owns: `src-tauri/src/mcp_server.rs`, `crates/deck-core/src/ask.rs`, `src-tauri/src/ipc.rs`, `src-tauri/src/lib.rs`, `ui/src/ipc-contract.ts`, `ui/src/ask.ts`, `ui/src/popup.ts`, `ui/src/styles.css`, `skills/quarterdeck/SKILL.md`.
Per spec §29 R-29.1..R-29.7. Additive `questions[]`; new `AskAnswerKind::Form`; answer = JSON string on existing channel; backward-compat (all new fields serde default/optional); caps + bidi-strip server-side; popup mirror shows "N questions — Answer in window".
AC: cargo test — legacy single-question path unchanged; multi-question parse + caps + form answer serialize; old ask/answer files still deserialize. Playwright — form renders radio/checkbox, Submit returns `{answers:[...]}` kind form. clippy + npm test green.

## T8 — §32 dismiss externally-resolved asks/perms (BUG)
Owns: `src-tauri/src/lib.rs` (perm sweep + SessionEnd cancel), `crates/deck-core/src/engine.rs` (SessionEnd hook + perm deadline data if in core), `src-tauri/src/mcp_server.rs` (drive_ask disconnect).
Per spec §32 R-32.1..R-32.4. Perm deadline `received_ms + ~90s` swept on the tick (mirror AskStore::sweep_expired); disable buttons past deadline; SessionEnd/dead cancels pending asks (reuse cancel_ask :1153) + drops perms; drive_ask disconnect dismisses; re-render FIFO.
AC: cargo test — a perm past its deadline is swept/expired; SessionEnd cancels a pending ask (caller gets kind:cancelled) + drops perms. clippy green.

## T9 — Integration + full gate (me + integration agent)
- `cargo build` + `cargo clippy --workspace -- -D warnings` + `cargo test --workspace` green.
- `npm run build` + Playwright suite green.
- Review full diff for spec compliance + regressions. Philipp commits as himself, per-group.
