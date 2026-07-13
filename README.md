<div align="center">

<img src="docs/hero.png" alt="Quarterdeck — a command bridge watching over a fleet of Claude Code agents" width="100%">

# Quarterdeck

**The deck from which the captain commands the ship.**

*A Windows + macOS tray app that watches every Claude Code session on your machine, and lets your agents reach back.*

[![License: MIT](https://img.shields.io/badge/license-MIT-D97757.svg)](LICENSE)
&nbsp;![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-3a3a3a)
&nbsp;![Built with Tauri v2](https://img.shields.io/badge/built%20with-Tauri%20v2-D97757)
&nbsp;[![CI](https://github.com/philippgross/quarterdeck/actions/workflows/ci.yml/badge.svg)](https://github.com/philippgross/quarterdeck/actions/workflows/ci.yml)

</div>

Quarterdeck is a Windows + macOS tray app that watches every **Claude Code**
session running on your machine, shows a live, glanceable status for each one,
fires a native notification when an agent finishes or needs you — and, unlike
every passive monitor out there, lets an agent **reach you back**: it can ask
you a question through an always-on-top popup while it works, and the answer
is routed straight back into that session.

Run several agents in parallel terminals and you already know the problem:
there's no answer to "who needs me right now?" without alt-tabbing through
every window. Quarterdeck puts that answer in your tray.

![Quarterdeck popup — five sessions and two pending asks, dark theme](docs/screenshots/popup-dark.png)

## Why Quarterdeck (the ask-channel differentiator)

Other Claude Code trackers (hydropix/claude-deck, ClaudeDeck, and similar,
all Windows-only, all under a handful of stars) are read-only dashboards:
they show you a status and stop there. Quarterdeck adds a channel *back* into
the conversation:

- A running agent calls the bundled `ask_user` MCP tool with a question
  (optionally with quick-pick options and a timeout).
- Quarterdeck pops an always-on-top window over whatever you're doing,
  without stealing keyboard focus, so it never interrupts what you're typing.
- Your answer — a button tap or free text — is written back and returned as
  the tool's result, and the agent continues.

That turns Quarterdeck from a dashboard you glance at into a monitor you can
actually be *reached through*: a long autonomous run can stop and ask "which
of these two approaches?" instead of guessing or stalling.

Beyond that, Quarterdeck is Windows **and** macOS (most alternatives are
Windows-only), status is driven by Claude Code's own hook events rather than
polling/parsing transcripts, and notifications are two-tier: a quiet "finished"
toast and a distinct, harder-to-miss "needs you" alert.

## Features

- **Live fleet view.** One popup lists every Claude Code session: project,
  task title, status, and time-in-status, sorted so what needs you floats to
  the top.
- **Tray status at a glance.** The tray icon shows the worst status across all
  sessions (needs-you beats working beats idle beats dead) — you don't even
  need to open the popup to know if everything's fine.
- **The watch line.** A thin segmented bar under the header shows the fleet's
  status mix (red/yellow/green/gray) proportionally, live — a one-glance read
  on how many sessions need you, are working, or are idle.
- **Move it, pin it, shrink it.** Drag the popup anywhere by its header. Pin
  it (the icon next to the gear) and it stays open — no more hide-on-blur —
  until you unpin it, hit Esc, or click the tray icon again. While pinned, a
  second button collapses the popup down to a small traffic-light lamp: one
  big status dot plus a count badge when something needs you. Click the lamp
  to expand back to the full list in place; drag it around like the popup.
  The popup's height also now tracks its content exactly (no fixed floor),
  growing and shrinking as rows come and go.
- **Session names come from Claude Code itself.** Titles are read live from
  Claude Code's own session registry, so a `/rename` mid-session updates the
  row within seconds — falling back to the prompt text or transcript only
  when the registry doesn't have a name yet.
- **Honest time-in-status.** Sessions that were already running before you
  started Quarterdeck get their timer seeded from the session's own registry
  or transcript data, not from when the app happened to launch — estimated
  times are marked with `~` until an exact hook event replaces them. Hovering
  a row also shows the session's total age.
- **Click a row to focus its terminal.** Best-effort: Quarterdeck brings the
  terminal window running that session to the front. It can't jump to a
  specific tab inside Windows Terminal (Windows Terminal doesn't expose
  per-tab focusing), so with multiple sessions in one Windows Terminal window
  you may still need to pick the right tab yourself.
- **Take over permission prompts.** When Claude Code is about to ask
  permission for a tool, Quarterdeck can show that prompt in its own popup —
  Allow, Deny, or "In terminal" to fall back to the normal dialog. This is
  fail-open by design: if Quarterdeck isn't running or you don't answer in
  time, Claude Code just falls through to its regular terminal prompt, so it
  can never leave an agent stuck. Toggle it off anytime in Settings.
- **Quiet when you're already looking.** If the terminal running a session is
  the window in front, Quarterdeck skips toasts and doesn't pop the ask/
  permission window for that session — you're already watching it, so there's
  nothing to interrupt you with. It still updates status and keeps anything
  pending queued for when you look away.
- **Background work shows as working.** A session waiting on subagents or a
  background workflow no longer looks falsely idle — it displays as working
  with a small badge (`⛭ N`) showing how many subagents are active.
- **Token stats per row.** Each row can show context fill (as a percentage of
  the model's context window, turning amber near 75% and red near 90%) and
  cumulative session spend, with a combined total next to the subagent badge
  when background work is running. Toggle "Show token usage on rows" in
  Settings.
- **Native notifications, two tiers, clearly Quarterdeck's.** A standard toast
  when a session finishes, quoting the model's actual last message as the
  body (falling back to "waiting for new instructions" if that's not
  available) — and a distinct, alert-styled toast with its own sound when a
  session is blocked on you (permission prompt, elicitation dialog, or an
  agent question). On Windows, toasts are branded as "Quarterdeck" with its
  own icon, not as PowerShell or whatever shell is hosting the session.
- **Agent questions (`ask_user` / `notify_user`).** A local MCP server lets an
  agent ask a blocking question (with options, an optional long-form detail,
  and a timeout — or no timeout at all, for a question that waits until
  answered) or send a fire-and-forget notice — see
  [Agent questions](#agent-questions-ask-channel) below.
- **Hook-driven precision.** Status comes from Claude Code's own hooks, not
  from polling or parsing transcript contents — cheap, accurate, and it
  recovers automatically once a permission prompt is answered in the
  terminal.
- **Cyrillic/Unicode safe.** Project paths and titles in any script render and
  round-trip correctly end to end.
- **Everything local.** No accounts, no telemetry, no network calls other than
  the `127.0.0.1` MCP endpoint your own agents talk to. See
  [Privacy](#privacy).

![Watch line + fleet rows: one session waiting, one working with a subagent badge, the rest idle](docs/screenshots/popup-waiting-dark.png)

<table>
<tr>
<td width="50%" valign="top">
<img src="docs/screenshots/lamp-dark.png" alt="Collapsed lamp mode — a single status pie for the whole fleet"><br>
<sub><b>Lamp mode.</b> Pin the popup, then collapse it to a tiny traffic-light lamp: one pie of your fleet's status mix, always on top, out of the way.</sub>
</td>
<td width="50%" valign="top">
<img src="docs/screenshots/context-menu-dark.png" alt="Per-row context menu — copy session id, rename, reset name, remove row, kill process"><br>
<sub><b>Per-row actions.</b> Right-click any session to rename it, copy its id, drop the row, or kill the process.</sub>
</td>
</tr>
</table>

## Install

1. Download the installer for your platform from the
   [releases](https://github.com/philippgross/quarterdeck/releases) page (or
   build it yourself — see [Development](#development--build)):
   - Windows: `Quarterdeck_<version>_x64-setup.exe` (NSIS installer).
   - macOS: `Quarterdeck_<version>_<arch>.dmg`.
2. Launch Quarterdeck. On first run it shows a one-time onboarding card and
   makes **no system changes until you say so** — it explains what it wants
   to do and asks explicitly:
   - **Install hooks** — required for sessions to show up at all (see below).
   - **Launch at login?** — explicit yes/no, off by default.
   - **Enable agent questions** — sets up the MCP tool so agents can ask you
     things (see [Agent questions](#agent-questions-ask-channel)).

   You can run every one of these later from the gear icon → Settings.

### Installing hooks

Quarterdeck's status tracking depends on Claude Code's hook events. "Install
hooks" (first run, or Settings → "Install hooks" / "Repair hooks"):

- Adds `SessionStart`, `UserPromptSubmit`, `Notification`, `Stop`,
  `SubagentStart`, `SubagentStop`, and `SessionEnd` entries to your
  **user-level** `~/.claude/settings.json`, so they apply to every project on
  the machine. It deliberately does *not* hook `PreToolUse`/`PostToolUse` — no
  extra latency on the hot path.
- If "Take over permission prompts" is on (default, see below), it also adds
  a `PermissionRequest` entry — opt-in and toggled independently in Settings.
- Never touches hooks it didn't add: it merges non-destructively, keeping any
  hooks you already have on those events, and only adds its own entries where
  none tagged `quarterdeck` already exist.
- Takes a timestamped backup of your `settings.json`
  (`settings.json.quarterdeck-backup-<timestamp>`, latest 3 kept) before the
  first write, and writes atomically (temp file + rename) so a crash mid-write
  can't corrupt your config.
- If your `settings.json` doesn't parse, it stops and shows an error instead
  of overwriting anything.
- "Uninstall hooks" removes exactly the entries Quarterdeck added and leaves
  everything else untouched.

The hook scripts themselves (`quarterdeck-hook.ps1` on Windows,
`quarterdeck-hook.sh` on macOS) are copied into Quarterdeck's own data
directory at install time, so the path Claude Code calls stays stable across
app updates. They read the hook's stdin JSON, write it to Quarterdeck's spool
directory, and always exit `0` — a malformed or unexpected hook payload is
swallowed silently rather than breaking your Claude Code session.

### Agent questions (ask channel)

"Enable agent questions" in Settings does two things, idempotently:

1. Registers Quarterdeck's local MCP server with the Claude CLI (if `claude`
   is on your `PATH`), equivalent to running:

   ```
   claude mcp add --transport http --scope user quarterdeck http://127.0.0.1:<port>/mcp --header "Authorization: Bearer <token>"
   ```

   If the CLI isn't found, Settings shows you this exact command (with your
   real port and token filled in) to run yourself. `<port>` is chosen once and
   persisted; `<token>` is a generated bearer token — requests to the MCP
   server without it are rejected.
2. Copies the bundled skill to `~/.claude/skills/quarterdeck/`, which teaches
   an agent *when* it's appropriate to ask (blocked on a human decision, about
   to do something risky/irreversible, or facing an ambiguity it can't resolve
   itself) and when not to (anything it can reasonably decide on its own) —
   and to degrade gracefully (proceed on its best judgment, and say so) if a
   question times out or is dismissed.

"Disable agent questions" reverses both. Once enabled, any Claude Code session
on the machine can call:

- `ask_user(question, options?, detail?, context, timeout_seconds?)` — blocks
  on your answer. `detail` is an optional longer rationale shown under the
  question in muted type. `timeout_seconds` is optional too: omit it (or pass
  `0`) for a persistent question that waits until you answer, dismiss it, or
  an agent cancels it — no expiry. Returns `{answer, kind, ask_id}`, where
  `kind` is `option`, `text`, `timeout`, `dismissed`, or `cancelled`.
  Dismissing a question always resolves the waiting call (it never hangs
  until a transport timeout).
- `update_ask(ask_id, question?, options?, detail?)` / `cancel_ask(ask_id)` —
  revise or withdraw a still-pending question from a parallel tool call or a
  different session (the blocked `ask_user` call can't do it to itself).
- `notify_user(message, context)` — fires off a one-line, no-reply heads-up
  and returns immediately with `{delivered: true, id}`.

While a question is blocked, Quarterdeck keeps the MCP connection alive with a
periodic heartbeat so long or indefinite waits don't get dropped by Claude
Code's own idle timeout. The bundled skill
([`skills/quarterdeck/SKILL.md`](skills/quarterdeck/SKILL.md)) documents all
of this for the agent side: when to ask, how to write a good question, and
how to use `update_ask`/`cancel_ask` correctly.

![Agent question popup, always-on-top, keyboard 1–9 for options](docs/screenshots/ask-dark.png)

## How statuses work

Every session shown in Quarterdeck is in exactly one of four states, driven
by Claude Code's own hook events (not by polling or parsing your transcripts):

| Status | Meaning | Enters when |
|---|---|---|
| 🟡 Working | Executing a turn | You submit a prompt, or transcript activity resumes while blocked/idle, or an agent question gets answered |
| 🔴 Needs you | Blocked on a human | A permission prompt / elicitation dialog notification fires, or an agent is waiting on an `ask_user` question |
| 🟢 Idle | Turn finished, awaiting instructions | Claude finishes responding, or a session starts |
| ⚪ Dead | Process is gone | A liveness check fails to find the process anymore |

A few details worth knowing:

- **Recovery without a dedicated event.** Claude Code doesn't emit a hook when
  you grant a permission prompt in the terminal, so Quarterdeck watches the
  transcript file's size/mtime: once it advances after a "needs you" moment,
  the session flips back to "working" — no polling of the terminal, no
  parsing of message contents.
- **Dead rows fade, they don't vanish instantly.** A session with no live
  process sticks around for 5 minutes (in case it's a blip) before being
  removed; a clean `SessionEnd` removes the row immediately regardless.
- **Cold-start discovery.** On launch, Quarterdeck also scans your recent
  `~/.claude/projects/*/*.jsonl` transcripts (last 6 hours) and Claude Code's
  own session registry for sessions it missed while it wasn't running, and
  shows them flagged as inferred (`~`) — best-effort, since it has no live
  process to track for those.
- **Background work counts as working.** If a session's `Stop` hook fired but
  it's actually still running subagents or a background workflow, Quarterdeck
  reads that from the session registry and keeps showing it as working
  (with the `⛭ N` badge) instead of falsely idle.
- **The tray icon is always the worst status across your fleet** — one
  session needing you turns the whole tray icon red, so you never have to
  open the popup just to check.

## Development / build

Requirements: Node 20+, Rust (stable, MSVC toolchain on Windows), and on
macOS the Xcode command line tools (for signing-free local builds).

```bash
npm install                 # installs UI + Tauri CLI deps
npm run dev                 # tauri dev — live app with hot-reloading UI
npm run build                # tauri build — packaged installer (NSIS on Windows, dmg on macOS)
npm run ui:dev               # Vite dev server only (popup/ask UI in a browser, mocked IPC)
npm run ui:build             # production UI bundle (ui/dist)
npm run ui:test              # UI test suite
npm run gen-icons            # regenerate tray/app icons from scratch
```

Rust-side, from the repo root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

`crates/deck-core` is a pure-Rust library with no Tauri/GUI dependency — the
engine, hook-config merging, naming, discovery, and liveness logic all live
there and are unit/integration tested without needing a window. `src-tauri`
is the thin OS-integration shell (tray, windows, notifications, the MCP
server) on top of it.

Useful environment variables for local testing (all optional):

- `QUARTERDECK_DATA_DIR` — override the data root (default: `%APPDATA%/quarterdeck`
  on Windows, `~/Library/Application Support/quarterdeck` on macOS).
- `QUARTERDECK_CLAUDE_DIR` — override the `~/.claude` directory used for hook
  install and cold-start discovery.
- `QUARTERDECK_FAKE_NOTIFIER=1` — replace real OS toasts with a JSONL trail
  at `<data>/notifier-calls.jsonl`, for scripted assertions.
- `QUARTERDECK_DEBUG=1` — verbose logging.

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy -D
warnings`, `cargo test`, the UI test suite, and a full `tauri build` on both
`windows-latest` and `macos-latest`, uploading the resulting installer/bundle
as a build artifact on every push and pull request.

## Limitations

Quarterdeck is intentionally scoped tight. Not (yet) included:

- **Windows Terminal tab focus.** Click-to-focus (see above) brings the
  right terminal *window* to the front, but Windows Terminal doesn't expose a
  way to focus a specific *tab* inside it — if several sessions live as tabs
  in one Windows Terminal window, focusing jumps to the window, not
  necessarily the right tab.
- **No per-tab / subagent rows.** The fleet view tracks top-level sessions,
  not one row per subagent — background subagent activity shows as a count
  badge on its parent session's row, not as separate rows.
- **Windows and macOS only.** No Linux tray support.
- **No history or analytics.** Quarterdeck shows current state, not a log of
  past sessions.
- **No auto-update.** Update by downloading and reinstalling.
- **Unsigned builds.** Installers aren't code-signed/notarized yet — expect
  the usual first-run SmartScreen/Gatekeeper prompts.
- **English-only UI.** No localization.
- **No sound customization.** Notification sounds are fixed system sounds,
  distinct per notification tier, not user-configurable yet.

## Privacy

Everything Quarterdeck does runs on your machine. There is no telemetry, no
account, and no outbound network call — the only network activity is the
local MCP server bound to `127.0.0.1`, which only your own Claude Code agents
on this machine can reach (and only with the generated bearer token). Session
data (spool events, ask/answer history, settings) lives entirely under your
local Quarterdeck data directory and is never sent anywhere.

## License

MIT — see [LICENSE](LICENSE). © Philipp Gross.
