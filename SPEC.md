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
