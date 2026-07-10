# Quarterdeck v1.5 — round-4: render + auto-size + type-aware Claude→user modal (§35)

Goal: everything Claude sends to the user (permission requests, ask_user questions) is PARSED and RENDERED nicely in the ask window with type-appropriate buttons, and the window SCALES to content. Today the perm modal dumps `<pre>{toolInput}</pre>` (raw JSON) and the ask window is fixed-size (420px) with scroll.

Hard constraint (locked, already agreed): a native `AskUserQuestion` arriving through the permission-takeover channel CANNOT be answered in-deck (the PermissionRequest hook only returns allow/deny/defer). Real in-deck answering happens only via our `ask_user` MCP tool (§29 form). So: perm modal RENDERS a question + offers "In terminal"; the skill (T3) pushes Claude to use `ask_user` for real answering.

Sequential build (T1 & T2 both touch ask.ts). Order: T1 render+buttons -> T2 auto-size -> T3 skill. No git commits by agents.

---

## T1 — §35.1 structured render of permission requests + type-aware controls
Files: `ui/src/ask.ts` (renderPerm ~:330, the `<pre class="qd-perm-input">` at ~:374), `ui/src/styles.css`, and if needed `ui/src/ipc-contract.ts` (PermRow already carries `toolName` + `toolInput`).

- **Parse + render `perm.toolInput`** (a JSON string, already sanitized + §28-decoded by the shell) by `perm.toolName`, replacing the raw `<pre>` dump:
  - `Bash` -> a "Command" block: the `command` field in a mono, newline-preserving box; show `description` if present.
  - `Write` -> file path (`file_path`) + a truncated preview of `content`.
  - `Edit` / `MultiEdit` -> file path + old->new string(s) (truncated), or the edits list.
  - `Read` / `Glob` / `Grep` / `LS` -> the path/pattern fields on one line.
  - `AskUserQuestion` -> render each question: header (if any) + question text + its options as a readable bulleted/numbered list (READ-ONLY — this is the permission view, not answerable here).
  - Unknown/other tool -> parse the JSON and render as `key: value` rows (mono values), NOT a raw JSON blob.
  - **Fallback**: if `JSON.parse(perm.toolInput)` throws (truncated/oversized input), keep the existing `<pre>` block so a fragment still shows something. Reuse `truncate` (ask.ts already imports it) for long values; keep bidi safety (values are already shell-sanitized).
- **Type-aware controls** in renderPerm:
  - When `perm.toolName === 'AskUserQuestion'` (a question, not answerable via permission): controls = a grey/secondary **"In terminal"** button (the `defer` decision — primary path) + **"Deny"**. Do NOT show "Allow". Add a one-line hint that the answer is given in the terminal (or via `ask_user`).
  - All other tools: keep **Allow** / **Deny** / **In terminal** exactly as today (incl. the §32 expired-deadline disabling of Allow/Deny).
- Keep keyboard map behavior (A/D/Esc) working for the non-question case; for the question case, Esc = In terminal, and disable the A (allow) key.
AC: Playwright — a Bash perm renders the command (not JSON); an AskUserQuestion perm renders the question + options and shows "In terminal"+"Deny" (no Allow); an unknown tool renders key/value rows; a truncated/unparseable input falls back to `<pre>`. `npm run ui:build` + tsc + eslint clean; existing perm-flow specs still green (update them for the new markup).

## T2 — §35.2 auto-size the ask window to content
Files: `src-tauri/src/windows.rs`, `src-tauri/src/ipc.rs`, `ui/src/ask.ts`, `src-tauri/src/lib.rs` (handler registration), `ui/src/ipc-contract.ts`.

Mirror the popup's content-driven resize (`resize_popup_to_content` windows.rs:~524 + `resize_popup` ipc command + popup.ts measureAndResize) for the ASK window:
- Add `resize_ask_to_content(app, content_px)` in windows.rs: clamp to a band (floor ~140, cap ~640, then the content area scrolls), `set_size` the ask window (keep width per spec ~420, or reuse the ask window's current width). The ask window is always-on-top + centered (R-8.3); after a resize keep it on the active display (re-center horizontally is fine; avoid yanking it around under the user mid-answer — only resize height, keep top/position stable, like the popup keeps its top-left).
- Add `resize_ask` #[tauri::command] in ipc.rs mirroring `resize_popup`; register in the generate_handler! list in lib.rs.
- In ask.ts, after each `render()`, measure the real content height (header + content + actions, like popup.ts measureAndResize with scrollHeight) and `invoke('resize_ask', { contentHeight })`. Debounce/guard against churn (only when height meaningfully changed), and respect reduced-motion (snap). Add the command to ipc-contract.ts Commands.
AC: Playwright/unit — the ask window grows for a large form / long perm input and shrinks for a short one, within the band; a very tall content scrolls at the cap. Rust: unit-test the clamp like `popup_target_height`. cargo build/clippy/test + npm build green.

## T3 — §35.3 skill routes questions to ask_user
File: `skills/quarterdeck/SKILL.md`.
- Strengthen the guidance so that when Quarterdeck is active and the agent is running autonomously / out of the user's terminal focus, it prefers the `ask_user` MCP tool (rendered + answerable in-deck, incl. the §29 multi-question form) over the native `AskUserQuestion`. Keep the honest note that native `AskUserQuestion` is fine when the user is actively in the terminal, but only `ask_user` can be answered from Quarterdeck. Add a short example contrasting the two.
AC: SKILL.md reads coherently; no code changes; the ask_user schema/description already reflects §29.

## T4 — integration gate
`cargo build/clippy/test --workspace` + `npm run ui:build` + Playwright, all green from clean. Report honestly; no commits.
