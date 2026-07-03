# Quarterdeck — Specification v1.0 (LOCKED 2026-07-02)

**Quarterdeck** — the deck from which the captain commands the ship. A cross-platform (Windows + macOS) tray/menubar app that monitors every running **Claude Code** session on the machine, shows live statuses, fires native notifications when an agent finishes or needs a human — and lets agents **ask the user questions** through an always-on-top popup, with the answer routed back into the session via MCP.

This spec is locked; the canonical locked copy lives in Notion, this file mirrors it for the repo and implementation agents. Every implementation task and QA scenario traces to a numbered requirement (`R-*`).

**Decisions log:** stack = Tauri v2; name = Quarterdeck (GitHub niche verified free 2026-07-02); design = Mission Control, adaptive light/dark, GitHub/Claude-Code-inspired; sounds = system sounds, distinct per type; autostart = ask on first run; ask-feature = **in v1, MCP-first**; click-to-focus terminal = deferred to v2; publish = local until QA passes, then public GitHub (MIT).

---

## 1. Product overview

**Problem.** People run multiple Claude Code agents in parallel terminals. There is no glanceable answer to "who needs me?" — users alt-tab through terminals, miss permission prompts for minutes, and lose the thread of parallel work.

**Solution.**
- Tray icon shows the **aggregate status** (worst-of: red > yellow > green > gray).
- Click → compact popup: all sessions with project, task title, status, time-in-status.
- Native notifications: "finished, awaiting instructions" (standard) and "needs attention" (alert-styled, distinct sound).
- **Agent questions**: a Claude Code agent calls the `ask_user` MCP tool served by Quarterdeck → always-on-top popup over whatever the user is doing → the answer returns as the tool result. A bundled skill teaches agents when/how to use it.

**Competitive position.** Existing projects (hydropix/claude-deck, XueTianyu24/ClaudeDeck, etc.) are passive Windows-only monitors, all ≤7 stars. Quarterdeck differentiates on: Win+Mac, hook-driven precision (no transcript polling for status), native two-tier notifications, and the interactive ask channel — a monitor you can be *reached through*, not just a dashboard.

**Non-goals v1** (§13): terminal focusing, subagent rows, Linux, history/analytics, auto-update, code signing, i18n (UI English-only).

## 2. Statuses

| Status | Color | Meaning | Entered when |
|---|---|---|---|
| `working` | 🟡 | Executing a turn | `UserPromptSubmit`; or transcript activity while `attention`/`idle` (R-2.2); or MCP ask answered |
| `attention` | 🔴 | Blocked on a human | `Notification` with `notification_type` ∈ {`permission_prompt`, `elicitation_dialog`}; or pending Quarterdeck ask (R-8) |
| `idle` | 🟢 | Turn finished, awaiting instructions | `Stop`; or `SessionStart` |
| `dead` | ⚪ | Process gone without `SessionEnd` | Liveness poll fails (R-6) |
| *(removed)* | — | Clean end | `SessionEnd` (any `reason`) → row removed |

- **R-2.1** Transitions follow the table; unknown `notification_type` values are ignored for status (logged).
- **R-2.2** `attention → working` recovery: while `attention` (from hooks, not from a pending ask), if the transcript file's size/mtime advances ≥2 s after the Notification timestamp → `working`. (Claude Code emits no event on permission-grant; we stat the file, never parse it for this.)
- **R-2.3** `idle_prompt` notifications do NOT change status (session is already `idle` via `Stop`); optional "still waiting" reminder toast, default **off**.
- **R-2.4** A pending ask forces `attention` regardless of hook-derived status; on answer/timeout the status recomputes from last hook state.
- **R-2.5** `dead` rows persist 5 min, then are removed. `SessionEnd` always wins immediately.
- **R-2.6** Tray icon = worst status; zero sessions → neutral/gray icon.

## 3. Architecture (Tauri v2)

```
crates/deck-core/        Pure Rust lib (no Tauri deps): events.rs, engine.rs,
                         discovery.rs, naming.rs, liveness.rs, hooks_config.rs, ask.rs
src-tauri/               Tauri shell: main.rs, tray.rs, windows.rs, watcher.rs,
                         notify.rs, mcp_server.rs, ipc.rs (commands+events), settings.rs
ui/                      Vite + vanilla TypeScript + CSS (no framework): popup + ask window + settings
hooks/                   quarterdeck-hook.ps1 (Win), quarterdeck-hook.sh (mac/linux)
skills/quarterdeck/      SKILL.md — the bundled Claude Code skill
assets/                  tray icons (4 statuses × light/dark), app icon
.github/workflows/ci.yml fmt+clippy+cargo test+UI tests (win/mac), tauri build artifacts
```

- **R-3.1** `deck-core` compiles and passes `cargo test` with no Tauri/GUI dependencies (portable, heavy-tested core).
- **R-3.2** OS interactions (tray, toasts, windows) live in `src-tauri` behind traits defined in `deck-core` (`Notifier`, `Clock`) so the engine is testable with fakes; `notify.rs` has a fake mode `QUARTERDECK_FAKE_NOTIFIER=1` that appends calls to `<data>/notifier-calls.jsonl` (for e2e assertions).
- **R-3.3** Data root: `%APPDATA%/quarterdeck` (Win), `~/Library/Application Support/quarterdeck` (mac); override via `QUARTERDECK_DATA_DIR` (required for test isolation). Layout: `spool/`, `spool-quarantine/`, `asks/`, `answers/`, `hooks/`, `logs/`, `settings.json`, `mcp.json` (port+token).
- **R-3.4** Frontend is dumb: receives full state snapshots over Tauri events (`deck://state`), sends actions via commands (`answer_ask`, `remove_row`, `set_setting`, `install_hooks`, `uninstall_hooks`). All logic in Rust.
- **R-3.5** Spool: events consumed (parse → apply → delete); app-not-running events replay on startup; >24 h-old events discarded on replay; cap 5000 files (oldest deleted first). Malformed files → `spool-quarantine/` + log, never crash (also applies to asks/answers).
- **R-3.6** The popup window is created once and hidden/shown (never destroyed); timers for liveness/recovery run in Rust (immune to webview throttling).

### 3.1 Data flow

```
Claude Code ──hook──▶ quarterdeck-hook.{ps1,sh} ──atomic write──▶ <data>/spool/*.json
                                                                      │ notify-rs watcher
Claude Code ◀──MCP tool result── mcp_server.rs ◀── answers/ ◀── ask window
                                      │ ask_user()                     ▲
                                      ▼                                │
                                 deck-core engine ──▶ tray / popup UI / toasts
```

## 4. Hook integration (facts verified against official docs 2026-07-02 → `docs/hooks-facts.md`)

Installed into **user-level** `~/.claude/settings.json` (applies to all projects). Events: `SessionStart`, `UserPromptSubmit`, `Notification`, `Stop`, `SessionEnd`. Deliberately NOT per-tool events (PreToolUse/PostToolUse) — no added latency on the hot path.

- **R-4.1 Installer** (first run + Settings → "Repair hooks"): read `~/.claude/settings.json`; unparseable → visible error, never overwrite; timestamped backup `settings.json.quarterdeck-backup-<ts>` before first write (keep 3); merge non-destructively (preserve all foreign hooks; add ours only if no entry with marker `quarterdeck` exists per event); atomic write (tmp+rename). Hook entries use `"timeout": 10` and, on Notification, `"matcher": "permission_prompt|idle_prompt|elicitation_dialog"`.
- **R-4.2 Uninstaller**: removes exactly the entries whose command contains `quarterdeck`; foreign content preserved; UI confirms.
- **R-4.3 Hook script contract**: read stdin JSON fully → wrap `{v:1, event, receivedAt, payload, extra}` → atomic spool write. On `SessionStart` only: `extra.claudePid` = nearest ancestor process matching `claude|node|bun` (walk parents; PowerShell CIM on Win, `ps -o ppid=` loop on mac). Exit 0 always, ≤2 s typical, silent on stdout/stderr, swallow all errors. Garbage stdin → still exit 0 (write nothing).
- **R-4.4 Command lines**: absolute paths with **forward slashes** (survive both Git Bash and PowerShell — the documented Windows hook shells): `powershell.exe -NoProfile -ExecutionPolicy Bypass -File "C:/Users/…/quarterdeck/hooks/quarterdeck-hook.ps1"`. Scripts are copied to `<data>/hooks/` at install so the path is stable across app updates.
- **R-4.5 Forward compatibility**: unknown payload fields ignored; missing optional fields tolerated. Payload fields we rely on (verified): `session_id`, `transcript_path`, `cwd`, `hook_event_name`; `source` + optional `session_title` (SessionStart); `prompt` (UserPromptSubmit); `message` + `notification_type` (Notification); `reason` (SessionEnd).

## 5. Session identity & naming

- **R-5.1** Identity = `session_id`. `/clear` produces `SessionEnd(reason=clear)` + new `SessionStart` — old row removed, new row created (correct by construction).
- **R-5.2 Title precedence**: `session_title` (SessionStart, when present) → latest `UserPromptSubmit.prompt` (stripped, collapsed, ≤60 chars) → cold-start transcript fallback (first user text line, best-effort guarded parse) → `(no title)`. Row shows `<project> — <title>`, project = basename(cwd).
- **R-5.3** Cyrillic/Unicode paths MUST work end-to-end (developer's machine has them).
- **R-5.4 Cold-start discovery**: on startup, after spool replay, scan `~/.claude/projects/*/*.jsonl` (base dir overridable via `QUARTERDECK_CLAUDE_DIR` for tests) with mtime <6 h; unknown sessions get inferred rows: transcript grew <30 s ago → `working` else `idle`, flagged `inferred` (UI shows `~`). No PID → pruned by R-6.2.

## 6. Liveness

- **R-6.1** Rust-side poll every 10 s: sessions with `claudePid` → alive check + name still matches `claude|node|bun` (sysinfo crate); fail → `dead`.
- **R-6.2** Inferred sessions (no PID): `dead` when transcript untouched >6 h.

## 7. UI & design system

**Direction:** Mission Control. Density and calm of a cockpit instrument, GitHub-Primer-family palette with a Claude-clay brand accent, adaptive to OS light/dark. No emoji in chrome, no decoration that isn't information.

**Tokens (dark / light):**
- bg `#0d1117` / `#ffffff`; surface `#161b22` / `#f6f8fa`; border `#30363d` / `#d0d7de`; text `#e6edf3` / `#1f2328`; muted `#8b949e` / `#656d76`
- status: green `#3fb950`/`#1a7f37`, yellow `#d29922`/`#9a6700`, red `#f85149`/`#cf222e`, gray `#6e7681`/`#8c959f`
- accent (brand, sparing: wordmark, focus rings, primary buttons): clay `#D97757`
- type: system stack (`-apple-system, "Segoe UI Variable", "Segoe UI", sans-serif`) for text; `"Cascadia Code", "SF Mono", ui-monospace, Consolas, monospace` with tabular numerals for times, branches, session ids; wordmark `QUARTERDECK` 11px mono, letterspaced small-caps, clay.
- radius 8px cards / 6px controls; 150 ms ease transitions; `prefers-reduced-motion` respected (pulse/segment animations off).

**Signature element — the watch line.** A 2px bar under the header, segmented proportionally to session counts by status (red|yellow|green|gray), animating on changes. It's the fleet state as an instrument reading — visible even at a squint, and it's what screenshots get remembered by.

- **R-7.1** Popup window: frameless, 360×460 (max-height 560 then scroll), anchored to tray icon, hides on blur/Esc, absent from taskbar/Dock/alt-tab.
- **R-7.2** Row: status dot (soft pulse when `working`, steady otherwise), project (semibold), title (muted, 1-line ellipsis), git branch chip when known, right-aligned mono time-in-status (`4m 07s`, live tick 1 s). Hover → full cwd tooltip. Right-click → Copy session id / Remove row.
- **R-7.3** Sort: attention → working → idle → dead; within group by latest activity. Counts in footer (`1 needs you · 2 working · 1 idle`).
- **R-7.4** Header: wordmark, watch line below, gear → settings pane (slide-in, same window): notification toggles (idle/attention/reminder), autostart toggle, Install/Repair hooks, Uninstall hooks, Enable agent questions (MCP setup, R-8.6), data dir path, version.
- **R-7.5** Empty state: "No Claude Code sessions yet — start `claude` in any terminal." + hooks-health line. If hooks not installed: persistent banner with "Install hooks" button (also shown as a dot on the gear).
- **R-7.6** Copy register: sentence case, plain verbs, actions say what they do ("Install hooks", not "Setup"); errors state what happened + the fix, never apologize.

## 8. Agent questions (ask channel, MCP-first)

- **R-8.1 MCP server**: `src-tauri/mcp_server.rs` serves MCP over **streamable HTTP** on `127.0.0.1:<port>` (port random-stable, persisted in `<data>/mcp.json` with a generated bearer token; requests without the token → 401). Tools:
  - `ask_user(question: string, options?: string[], context?: string, timeout_seconds?: number≤600)` → blocks until answered/timeout/dismissed. Returns `{answer: string, kind: "option"|"text"|"timeout"|"dismissed"}`.
  - `notify_user(message: string, context?: string)` → fire-and-forget toast, returns immediately.
- **R-8.2 Session attribution**: `context` carries the calling agent's cwd (the skill instructs this). Deck matches cwd → known session row (basename fallback match); unmatched asks display as "Unknown agent (<context>)". Never dropped for being unmatched.
- **R-8.3 Ask window**: separate small always-on-top window (420 px wide, auto-height, centered on active display), NOT stealing keyboard focus on appear (user may be typing) — it takes focus on first click/Tab. Shows: agent identity line (status dot + project), the question, option buttons (stacked, keyboard 1–9), free-text field + "Send answer", "Dismiss" (returns `dismissed`), mono countdown when timeout set. Multiple pending asks queue FIFO; badge "2 more waiting". Also mirrored as rows-with-input in the main popup.
- **R-8.4** Pending ask → session `attention` (R-2.4) + alert toast "<project> asks: <question…>" (same channel as R-9.2; toast click opens ask window).
- **R-8.5 Bundled skill** (`skills/quarterdeck/SKILL.md`, copied to `~/.claude/skills/quarterdeck/` by "Enable agent questions"): teaches agents — in long autonomous tasks, when blocked on a human decision, call `ask_user` with cwd as `context`, sensible `timeout_seconds`, options where possible; degrade gracefully (proceed on best judgment) on timeout; never spam (batch questions); prefer built-in AskUserQuestion when the user is actively interactive.
- **R-8.6 Setup**: Settings → "Enable agent questions" runs `claude mcp add --transport http --scope user quarterdeck http://127.0.0.1:<port>/mcp --header "Authorization: Bearer <token>"` (via CLI if found on PATH; else shows the exact command to copy). Also copies the skill. Idempotent; "Disable" reverses both.
- **R-8.7** Answers persist to `<data>/answers/` for the blocked call to consume; if the app restarts while an ask is pending, the MCP connection is gone — the ask is marked orphaned and shown as expired, never answered into the void.

## 9. Notifications

- **R-9.1** `Stop` → standard toast: title "<project> finished", body = title + "Waiting for new instructions." System default notification sound.
- **R-9.2** `attention` (permission/elicitation/ask) → alert toast: title "<project> needs you", body = notification message or question. **Distinct system alert sound** (Win: a `ms-winsoundevent:Notification.*` alert-class sound — implementer picks the least obnoxious; mac: `Basso`/`Sosumi`-class system sound). Red-badged icon variant where the platform allows.
- **R-9.3** Windows: stable `AppUserModelID` (`pro.philippgross.quarterdeck`) registered so toasts work in dev and packaged modes.
- **R-9.4** Throttle: per session, max 1 toast per status-change per 10 s (bursts collapse); suppressed when the popup is visible AND focused. Ask toasts never suppressed.
- **R-9.5** Toggles per type (idle/attention/reminder), defaults on/on/off. No sound customization in v1 (v2).
- **R-9.6** Toast click: opens the popup (or the ask window for ask toasts). No terminal focusing in v1.

## 10. Settings, autostart, persistence

- **R-10.1** `<data>/settings.json`: `{notifyIdle, notifyAttention, notifyReminder, launchAtLogin, onboardingDone}`; unknown keys preserved.
- **R-10.2** First run: one-time onboarding card inside the popup — explains hooks, offers "Install hooks" + "Launch at login?" (explicit yes/no) + "Enable agent questions". No system changes before consent.
- **R-10.3** Autostart via tauri-plugin-autostart (Win registry Run key / mac LaunchAgent), toggle in settings.
- **R-10.4** Logs `<data>/logs/quarterdeck.log`, 1 MB × 3 rotation; `QUARTERDECK_DEBUG=1` → debug level.

## 11. Testing strategy (the QA fleet runs all of this)

- **Rust unit/integration (`cargo test`, bulk of coverage):** every R-2 transition (injectable clock); spool parse/quarantine incl. truncated/garbage/huge files; hooks_config merge/uninstall vs fixtures (missing, empty, foreign hooks, malformed, BOM, CRLF); naming precedence incl. Cyrillic; liveness with fake process table; ask lifecycle (queue, timeout, orphaning); throttle.
- **Hook script tests (real machine):** pipe fixture stdin → assert spool shape, atomicity, exit 0 on garbage, silence; SessionStart ancestor walk finds a real claude PID; `shellcheck` on the .sh.
- **UI tests (Playwright against Vite dev server, Tauri IPC mocked):** rows/sorting/watch-line/empty/settings/ask-window flows, light+dark, reduced-motion.
- **E2E smoke (real built app on this machine):** launch with isolated `QUARTERDECK_DATA_DIR` + `QUARTERDECK_CLAUDE_DIR`; inject synthetic spool events → assert tray icon changes (via test hook), fake-notifier jsonl, screenshot popup. MCP: scripted Node client calls `ask_user`, test answers via `answer_ask` command, asserts returned value.
- **Live smoke (final):** isolated `CLAUDE_CONFIG_DIR`, real `claude` session → hooks fire, row appears, Stop toast fires; `ask_user` round-trip from a real Claude Code session using the skill.
- **R-11.1** All green before the app is shown to the user. CI mirrors: fmt, clippy -D warnings, cargo test, UI tests, tauri build artifacts (win+mac, unsigned).

## 12. Privacy

Everything is local. No telemetry, no network calls except `127.0.0.1` MCP. README states this prominently.

## 13. Non-goals v1 / v2 backlog

Click-to-focus terminal (deliberately deferred); subagent rows (SubagentStart/Stop); Linux tray; history/analytics; sound customization; auto-update; signing/notarization; i18n; fine-grained `PermissionRequest`/`PermissionDenied` hooks; remote/mobile view.

---

# Spec v1.1 addendum (LOCKED 2026-07-03) — first-user feedback round

Nine items from Philipp's live dogfooding + agent-side API feedback. Same rules: R-numbers are law, every item gets tests, nothing here weakens v1.0 requirements. Facts verified against official docs 2026-07-03 (PreToolUse/PermissionRequest decision contracts, MCP timeout model) — see docs/hooks-facts.md addendum.

## 14. Window behavior (items 1, 2, 3)

- **R-14.1 Movable popup.** The popup header is a drag region (`data-tauri-drag-region` or CSS `-webkit-app-region: drag`); interactive header controls (gear, pin) remain clickable. The window can be dragged anywhere on any monitor.
- **R-14.2 Pin on top.** A pin toggle button in the header (icon, left of the gear): pinned → `always_on_top(true)`, hide-on-blur DISABLED (window stays until unpinned/Esc/tray click), visually indicated (filled pin, clay accent). Unpinned → v1.0 behavior (anchor to tray on open, hide on blur). Pin state persists in settings (`popupPinned`).
- **R-14.3 True auto-height.** Popup height tracks content: `height = clamp(header + watchline + rows + footer, 160, 560)`; beyond 560 the list scrolls (v1.0 R-7.1 max preserved). The 460 floor is REMOVED (empty state may be compact). Height shrinks back when rows disappear (regression-tested: 50 rows → 0). When the user manually moved the window (R-14.1), height changes keep the TOP edge fixed (grow downward) and never re-anchor to the tray.

## 15. Session names & focus (items 4, 5)

- **R-15.1 Live session registry.** New core source: `~/.claude/sessions/*.json` (undocumented internal registry: `{pid, sessionId, cwd, name, status, kind, entrypoint, ...}`). Parsed DEFENSIVELY (any missing field tolerated; unreadable file skipped; format drift must never crash — quarantine-free, just log+skip). Polled every 10s alongside liveness AND read at cold start.
- **R-15.2 Title precedence (replaces R-5.2 chain head).** `name` from the live registry (matched by sessionId) → `session_title` (SessionStart payload) → latest `UserPromptSubmit.prompt` (≤60 chars) → transcript fallback → `(no title)`. Registry names refresh on every poll (a /rename mid-session updates the row within ≤10s).
- **R-15.3 Registry-driven discovery.** Cold-start discovery (R-5.4) now ALSO creates rows for registry entries whose transcript is missing/stale (status from registry `status` field mapped: busy→working, else idle, flagged inferred). Registry pid feeds liveness directly (no ancestor walk needed for registry-known sessions).
- **R-15.4 Click-to-focus terminal (v1 deferral lifted).** Row click focuses the terminal window hosting that session, best-effort:
  (a) On `SessionStart` the hook script captures `extra.ancestor = {pid, hwnd, exe}` — nearest ancestor process with a real top-level window (Win: `MainWindowHandle != 0` via CIM walk; mac: `TERM_PROGRAM` + pid).
  (b) Focus (Win): validate hwnd still belongs to ancestor pid (`GetWindowThreadProcessId`), then `ShowWindow(SW_RESTORE)` + `SetForegroundWindow` via a spawned `powershell -NoProfile` P/Invoke snippet (no new native deps). Stale/missing hwnd → fallback: enumerate top-level windows, focus first whose title contains the project basename; none → inline toast-in-window "Couldn't find the terminal window".
  (c) Focus (mac): `osascript` activate by bundle id derived from `TERM_PROGRAM` (Terminal/iTerm/VS Code map). Code-complete, compile-gated, not live-tested (no mac hardware).
  (d) Windows Terminal TAB-level focus remains out of scope (README limitation).
  Row click = focus; the former row-click no-op is gone. Right-click menu gains "Focus terminal" as first item.

## 16. Permission requests in the deck (item 6)

- **R-16.1 PermissionRequest hook.** Installer adds a `PermissionRequest` entry (same marker/backup rules as R-4.1) whose script: writes `{v:1, kind:"perm", tool_name, tool_input (truncated to 2KB), session_id, cwd, receivedAt}` to `<data>/perms/`, then polls `<data>/perm-answers/<id>.json` until answered or its deadline (hook `timeout: 90`, poll exits at 85s), and:
  - answer allow → stdout `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}`, exit 0
  - answer deny → same with `"deny"` + the user's optional reason
  - no answer by deadline / deck not running / any error → exit 0 with NO output (Claude Code falls through to the normal terminal dialog; the hook MUST be fail-open).
- **R-16.2 Deck-side modal.** A pending perm renders in the SAME always-on-top ask window, visually distinct (amber left border vs clay for asks): title "<project> requests permission", body = tool name + compact pretty-printed input (truncated), buttons **Allow** / **Deny** / **In terminal** (explicitly answers "no decision" → hook exits silently → terminal dialog appears immediately). Keyboard: A / D / Esc(=In terminal). Perm pending forces session `attention` + alert toast (same class as R-9.2), and FIFO-queues with asks.
- **R-16.3 Focus-aware auto-defer (ties into §17).** If the session's terminal window is foreground when the perm arrives, the deck AUTO-ANSWERS "no decision" within 300ms (user is already looking at the terminal; the dialog shows there with near-zero added latency) and shows nothing.
- **R-16.4 Opt-in.** Settings toggle "Take over permission prompts" (default ON after onboarding consent; the onboarding card explains it). Toggle off → installer removes ONLY the PermissionRequest hook entry. Uninstall removes it with the rest.
- **R-16.5 Safety.** The perm modal displays tool_name + input VERBATIM but sanitized (bidi strip per QA round 5, length caps); Allow never auto-repeats (no "always allow" in v1.1 — explicit non-goal).

## 17. Focus-aware suppression (item 7)

- **R-17.1** Every 2s (and immediately before showing the ask window or firing any toast) the shell samples the foreground window's root process chain (Win: `GetForegroundWindow` → pid → ancestor chain; mac: frontmost app pid via `osascript`, best-effort).
- **R-17.2** If the foreground window belongs to the session's terminal (matches its `ancestor.pid`/hwnd from R-15.4a, or the registry pid's window), then for THAT session: the ask window does NOT auto-appear (ask stays queued + mirrored in popup; appears as soon as focus leaves), toasts (idle/attention/ask/perm) are suppressed, and perms auto-defer (R-16.3). Suppressed toasts refund the throttle slot (QA round-4 rule).
- **R-17.3** The popup itself being foreground keeps v1.0 R-9.4 suppression semantics. Suppression never LOSES anything: asks/perms stay pending, statuses still update.

## 18. Ask window UX (item 8)

- **R-18.1 Close button.** The ask window gets an X (top-right): closes (hides) the WINDOW without dismissing pending asks — they remain queued + mirrored in the popup, badge intact; the window re-appears on the next new ask/perm (or via popup mirror click). This is distinct from per-ask "Dismiss" (which resolves that ask as dismissed). Esc = same as X when multiple pending; when exactly one pending, Esc also = X (never silently dismisses).
- **R-18.2** Window title bar area is draggable (same mechanism as R-14.1).

## 19. MCP API v1.1 (item 9)

- **R-19.1 `detail` field.** `ask_user` gains optional `detail: string` (long rationale/body, rendered under the question in muted smaller type, scrollable if long, sanitized). Skill updated: question = short, detail = the reasoning.
- **R-19.2 Persistent asks.** `timeout_seconds` becomes optional; **omitted/0 → persistent**: the ask lives until answered/dismissed/cancelled (no expiry sweep). Cap for explicit values raised 600 → 3600. UI: persistent asks show no countdown.
- **R-19.3 Keepalive.** While ANY ask_user/perm call is blocked, the MCP server emits `notifications/progress` (with the request's progressToken, when the client sent one) every 30s — this resets Claude Code's 5-minute idle abort (`CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`); wall-clock `MCP_TOOL_TIMEOUT` is unset by default (~28h) so persistent asks survive. The skill documents: for very long autonomy also set per-server `timeout` in mcp config. The `claude mcp add` command emitted by R-8.6 is unchanged (no timeout arg needed by default).
- **R-19.4 dismissed MUST resolve (bug fix).** Dismissing an ask resolves the blocked MCP call with `{answer:"", kind:"dismissed"}` immediately (regression test: dismiss via window button AND via popup mirror; assert the client receives kind=dismissed, not a transport timeout).
- **R-19.5 update/cancel.** `ask_user` result gains `ask_id`. New tools:
  - `update_ask(ask_id, question?, options?, detail?)` — mutates a PENDING ask in place (UI re-renders; queue position kept). Unknown/settled id → error result (not exception).
  - `cancel_ask(ask_id)` — resolves the pending ask as `{kind:"cancelled"}` toward the original caller and removes it from UI.
  Both are usable from parallel tool calls / a different session (skill documents the parallel-call pattern and warns the blocked call itself can't cancel itself).
- **R-19.6 notify_user returns `{delivered: true, id}`** (id = toast/notification record id, logged in notifier-calls.jsonl fake mode too).

## 20. v1.1 testing gates

Everything in §11 stays green. New: unit tests for registry parsing (fixtures incl. malformed/missing fields), R-14.3 shrink regression, R-19.2/19.3/19.4/19.5 lifecycle tests (fake clock), perm hook script piped-stdin tests (answer/timeout/fail-open), Playwright specs for pin/drag-region presence/close-X/detail rendering/perm modal, e2e real-app: dismiss round-trip asserting kind=dismissed, persistent ask surviving >6min with keepalive (time-compressed via env knob where possible), and Part C live re-run incl. a REAL permission round-trip (claude asks to run a tool → deck Allow → tool runs; deck Deny → claude sees denial; timeout → terminal dialog appears).

## 21. Background work must show as working (v1.1.1, LOCKED 2026-07-03)

Found live by the first user: a session waiting on background subagents/workflows shows 🟢 idle (its `Stop` hook fired) while heavy agent work is running. The session registry (§15, R-15.1) reflects this correctly: `status: "busy"` with a fresh `updatedAt` while background children run.

- **R-21.1 Registry busy-override.** If the hook-derived status is `idle` but the live registry entry for that session says `status: "busy"` with `updatedAt` fresher than 30s, the row displays `working`. `attention` (hook-derived, pending ask, pending perm) always outranks the override. Override clears when the registry reports non-busy or goes stale.
- **R-21.2 Subagent badge.** Subscribe to `SubagentStart`/`SubagentStop` hook events (same installer/marker rules; script writes them to the spool). The engine keeps a per-session active-subagent counter; the row shows a compact badge `⛭ N` while N > 0. Counter is SELF-CORRECTING: when the registry reports the session non-busy (or the session goes attention/idle from a fresh Stop with stale registry), reset to 0 — a lost SubagentStop must never wedge the badge.
- **R-21.3** No toast on idle→working via override (it is not a user-actionable event); the R-9.1 "finished" toast still fires on the hook-derived Stop even if the override immediately flips the row back to working (the turn DID finish; the user may still want to know). Tray color follows the displayed (overridden) status.
- **R-21.4** Tests: engine unit tests for override precedence/staleness/reset; registry fixture with busy/idle flips; e2e mock scenario showing badge + yellow row while "background" busy.

## 22. Honest time-in-status for pre-existing sessions (v1.1.1, LOCKED 2026-07-03)

Live-found: rows for sessions that were running before Quarterdeck started tick their time-in-status from APP LAUNCH, not from when the agent actually entered that status.

- **R-22.1 Seeding.** When a session row is created by discovery (not by a hook event), its status-entry timestamp seeds from, in order: registry `updatedAt` (matched by sessionId, fresh file) → transcript mtime → only then "now". Never the app start time as a semantic default.
- **R-22.2 Hook-born rows unchanged** (exact event receivedAt, as today). A later hook event for a discovered row replaces the estimate with exact times from then on.
- **R-22.3 Session age.** Row tooltip (hover, alongside cwd) shows total session age when known: registry `startedAt`, else SessionStart receivedAt, else transcript birth/first-seen; format "session 2h 14m".
- **R-22.4 Estimated marker.** Seeded (estimated) times render with the existing inferred "~" convention (e.g. `~12m 40s`) until an exact hook event arrives.
- **R-22.5 Tests:** discovery seeding precedence (registry vs transcript vs now), estimate→exact upgrade on first hook event, tooltip content.

## 23. Token usage & context health (v1.2, LOCKED 2026-07-03)

Per-session token telemetry sourced from transcript `usage` records (undocumented internal format — parse defensively, tolerate absence, never crash; a format drift disables the feature for that session with one WARN log, everything else keeps working).

- **R-23.1 Incremental usage reader.** Per known session, maintain a byte offset into its transcript; on transcript change events (existing watcher/tick) read ONLY appended bytes (cap 4MB per read; on overflow or offset invalidation — file truncated/rotated — rescan from tail 512KB and mark totals "≥"). Extract from assistant-message records: `message.usage` {input_tokens, cache_creation_input_tokens, cache_read_input_tokens, output_tokens} and `message.model`.
- **R-23.2 Metrics per session.**
  (a) **Context fill**: latest record's input_tokens + cache_read_input_tokens + cache_creation_input_tokens, as % of the model window (window: model id contains "[1m]" → 1,000,000; else 200,000; unknown → 200,000).
  (b) **Session spend**: cumulative sum of output_tokens + non-cached input_tokens (fresh input + cache_creation) across the session, formatted compactly (12k / 1.4M).
- **R-23.3 Subagent group aggregate.** Sum R-23.2b across all transcripts under `<projects>/<slug>/<session_id>/**/*.jsonl` (subagent/workflow sidechains), same incremental discipline; shown as the group spend beside the §21 subagent badge (`⛭ 3 · 2.1M`). Cap: track at most 64 sidechain files per session (newest by mtime; log when capped).
- **R-23.4 Row UI.** Right block gains a second line under time-in-status, mono, muted: `ctx 62% · 1.4M`. Context-health coloring: <75% muted, ≥75% amber, ≥90% red + row hover tooltip line "context nearly full — consider /compact or a fresh session". No toasts for context health in v1.2 (visual only).
- **R-23.5 Perf guard.** Usage reading must add no measurable jank: all parsing on the Rust side off the UI thread, ≤1 read per session per change-tick, and the whole feature behind a settings toggle `showTokenStats` (default ON).
- **R-23.6 Tests:** fixture transcripts (real-shape usage records incl. cache fields and [1m] model ids), incremental append/truncate/rotate cases, window inference, subagent aggregation with cap, UI badge rendering + thresholds (mock), toggle off hides all of it.

## 24. Toast content & identity (v1.2, LOCKED 2026-07-03)

- **R-24.1 Finished-toast body = the model's last words.** The R-9.1 idle toast body becomes the LAST assistant text message of that session (sourced from the §23 incremental transcript reader: last assistant record's text blocks, joined, whitespace-collapsed, sanitized per R-16.5), truncated to the SAME character budget as today's body. Fallback chain when unavailable (reader off, no text yet, format drift): current behavior (title + "Waiting for new instructions."). The attention/ask toast bodies are unchanged (they already carry the actual message/question).
- **R-24.2 Toast identity (Windows).** Toasts must be visibly Quarterdeck's, not the host shell's: register the AppUserModelID (`pro.philippgross.quarterdeck`) in HKCU\Software\Classes\AppUserModelId with DisplayName "Quarterdeck" and IconUri = the clay app icon (done at app startup, idempotent, HKCU only — no elevation; removed by Settings → Uninstall hooks → "also remove toast registration" line item and by the NSIS uninstaller). Toast XML uses appLogoOverride with the app icon; the alert class (R-9.2) uses the red-badged variant. Result: header shows "Quarterdeck" + icon in dev AND packaged runs — never "Windows PowerShell".
- **R-24.3 mac parity** best-effort: bundle identity already provides name/icon in Notification Center; no extra work beyond verifying the bundle icon is set (compile-gated).
- **R-24.4 Tests:** fake-notifier jsonl gains a `body_source: "assistant"|"fallback"` field asserted in e2e (inject a transcript with a known assistant tail → Stop → body matches); registry-key registration idempotency unit test (HKCU, guarded to test hive via env override); manual demo script extended to visually confirm both toast identities.
