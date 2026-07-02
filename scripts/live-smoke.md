# Quarterdeck live-smoke procedure (T8, SPEC §11)

Three layers, cheapest/most automated first:

- **Part A — UI suite (mocked IPC).** Fully automated, Playwright, runs in
  seconds against the Vite dev server. No build required.
- **Part B — E2E smoke (real built app, synthetic events).** Fully
  automated, launches the actual `quarterdeck.exe` with isolated dirs and
  injects fake hook events. This is what CI/a release checklist should run.
- **Part C — Final live smoke (real `claude` session).** Manual, needs a
  human at the keyboard and a real Claude Code CLI session. Run this once
  before any public release, and after any change to `hooks/**`,
  `mcp_server.rs`, `notify.rs`, or the hook-installer path in `lib.rs`.

Read the **Known issues** section at the end before you run Part C — there
is one currently-open bug (no Tauri capabilities file → the popup never
live-updates) that will make the popup *look* broken during manual testing
even though the underlying engine/toast/MCP pipeline is fine.

---

## Part A — UI suite (mocked IPC)

```powershell
cd e2e
npm install            # first time only (own package.json — see below)
npx playwright install chromium   # first time only
npm test                # == npx playwright test
```

`e2e/` is a deliberately separate npm project (its own `package.json` +
`node_modules`) so this suite never has to touch the shared root
`package.json`/`package-lock.json` just to add `@playwright/test`. It starts
`npm run ui:dev` (the same Vite dev server T4 uses for manual scenario
browsing) itself via Playwright's `webServer` config and tears it down after.

Covers: empty state, the 3-session lifecycle (working / attention /
recovery / idle / end — see the comment at the top of
`e2e/tests/lifecycle.spec.ts` for exactly what "recovery" means when driven
through the *mocked* IPC layer, since the real R-2.2 hook-recovery state
machine lives in Rust and is unit-tested there), the ask window (options,
keyboard 1-9, free text, dismiss, countdown, "N more waiting", unmatched
"Unknown agent"), settings toggles + hooks install/repair/uninstall +
onboarding, Cyrillic/CJK rendering, and dark/light/reduced-motion.

23 tests, ~15s, all green as of this writing.

## Part B — E2E smoke (real built app, synthetic events)

```powershell
cargo build --release   # (or a full `npm run tauri build`)
cd e2e
node real-app-smoke.mjs
```

What it does (see the header comment in `e2e/real-app-smoke.mjs` for the
full detail):

1. Launches `target/release/quarterdeck.exe` with `QUARTERDECK_DATA_DIR` and
   `QUARTERDECK_CLAUDE_DIR` pointed at a fresh temp dir, and
   `QUARTERDECK_FAKE_NOTIFIER=1` (SPEC R-3.2 — toast *decisions* are
   appended as JSON lines to `<data>/notifier-calls.jsonl` instead of firing
   real OS toasts).
2. Waits for startup (`<data>/mcp.json` gets written once the MCP server has
   bound — a reliable "the app finished `setup()`" signal).
3. Runs `node scripts/inject-events.mjs --data-dir <dir> --preset fleet ...`
   to drop a synthetic 3-session fleet (one `working`, one `attention`, one
   `idle`) straight into `<data>/spool/` — exactly the shape
   `hooks/quarterdeck-hook.ps1`/`.sh` produce (see that script for the
   options reference).
4. Asserts `<data>/notifier-calls.jsonl` contains the expected `attention`
   and `idle` toast decisions. **This is the hard pass/fail gate** — proof
   the spool → engine → notifier pipeline works end-to-end in the real
   built app, independent of anything UI-side.
5. Best-effort: connects to the popup's webview over Chrome DevTools
   Protocol (WebView2 is Chromium; the app is launched with
   `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<n>` for
   this), reads the rendered rows, and saves a screenshot to
   `docs/screenshots/popup-live-smoke.png`. This substitutes for the "tray
   test hook" SPEC §11 mentions — no such hook exists in the codebase (see
   Known issues), so the popup's rendered `.qd-row` statuses (driven by the
   same `Shell`/`StateSnapshot` the tray icon reads) stand in for it.
6. Kills the app and deletes the temp dir (pass `--keep` to skip cleanup for
   post-mortem debugging, `--exe <path>` to point at a different binary,
   `--cdp-port <n>` if 9333 is taken).

Exit code 0 on pass. As of this writing: **PASS**, with one warning (the ACL
issue below, worked around automatically inside the script).

### `scripts/inject-events.mjs` on its own

The injector is a standalone tool independent of the smoke script — use it
by hand against any Quarterdeck instance (real app or `tauri dev`) pointed
at an isolated `QUARTERDECK_DATA_DIR`:

```powershell
node scripts/inject-events.mjs --data-dir <dir> session-start --session s1 --cwd "C:/some/project" --title "Fix the bug"
node scripts/inject-events.mjs --data-dir <dir> prompt --session s1 --cwd "C:/some/project" --prompt "Do the thing"
node scripts/inject-events.mjs --data-dir <dir> notification --session s1 --cwd "C:/some/project" --type permission_prompt --message "Allow Bash to run rm -rf?"
node scripts/inject-events.mjs --data-dir <dir> stop --session s1 --cwd "C:/some/project"
node scripts/inject-events.mjs --data-dir <dir> session-end --session s1 --cwd "C:/some/project" --reason other
node scripts/inject-events.mjs --data-dir <dir> --preset fleet --project demo --cwd "C:/some/parent/dir"
node scripts/inject-events.mjs --data-dir <dir> --preset lifecycle --session s1 --project demo --cwd "C:/some/project"
node scripts/inject-events.mjs --help
```

Two things worth knowing if you script your own multi-event story for one
session:

- **Space writes out by >250ms.** `src-tauri/src/watcher.rs`'s debounce
  coalesces everything inside one 250ms window into a single flush of a
  `HashSet<PathBuf>` — iteration order for that flush is *not* write order.
  If two events for the same session (e.g. `SessionStart` then `Stop`) land
  in the same window, they can be applied out of order and produce the
  wrong final status / skip a toast. `--preset fleet`/`--preset lifecycle`
  already space their own writes 350ms apart for this reason; do the same
  for custom sequences (`touch-transcript` in between two calls is a
  natural place to add the gap).
- **A fresh session starts `idle`.** `SessionStart` alone is a no-op status
  transition (`engine.rs Session::new` already defaults to `idle`), so it
  never fires a toast, and neither does a `Stop` that follows it directly —
  route through `prompt` (→ `working`) first if you need a genuine `Stop`
  transition (and its R-9.1 toast) to fire.

## Part C — Final live smoke (real `claude` session)

Needs: the `claude` CLI on PATH, a terminal, and a human answering prompts.
Isolates the *real* Claude Code config with `CLAUDE_CONFIG_DIR` (a real
Claude Code env var — `docs/hooks-facts.md`) so this never touches your
actual `~/.claude/settings.json`.

```powershell
# 1. Pick one isolated dir for BOTH the real claude CLI and Quarterdeck to
#    agree on (Quarterdeck's own `QUARTERDECK_CLAUDE_DIR` override and
#    claude's own `CLAUDE_CONFIG_DIR` must point at the SAME path, or the
#    hooks Quarterdeck installs never reach the session claude actually runs).
$smokeClaudeDir = "$env:TEMP\quarterdeck-live-smoke-claude"
$smokeDataDir   = "$env:TEMP\quarterdeck-live-smoke-data"
Remove-Item -Recurse -Force $smokeClaudeDir, $smokeDataDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $smokeClaudeDir, $smokeDataDir | Out-Null

# 2. Launch Quarterdeck against the isolated dirs (real notifier this time —
#    no QUARTERDECK_FAKE_NOTIFIER — you want to hear/see the actual toasts).
$env:QUARTERDECK_DATA_DIR = $smokeDataDir
$env:QUARTERDECK_CLAUDE_DIR = $smokeClaudeDir
& .\target\release\quarterdeck.exe
# (leave this running in its own terminal/job)
```

3. Click the tray icon. The popup should show the R-10.2 onboarding card
   (fresh data dir → `onboardingDone: false`). Click **Install hooks** —
   confirm the step flips to "Installed" and
   `$smokeClaudeDir\settings.json` now has `SessionStart`/
   `UserPromptSubmit`/`Notification`/`Stop`/`SessionEnd` entries whose
   command contains `quarterdeck` (R-4.1). Click **Enable agent
   questions** — confirm it copies `skills/quarterdeck/SKILL.md` to
   `$smokeClaudeDir\skills\quarterdeck\SKILL.md` (R-8.6); if the `claude`
   CLI is on PATH it also runs `claude mcp add` for you (check with
   `claude mcp list --scope user` under the same `CLAUDE_CONFIG_DIR`, see
   step 4). Click **Yes**/**No** on "Launch at login?", then **Continue**.

4. In a **second** terminal, point the real `claude` CLI at the same
   isolated config dir and start a session in some scratch project:

   ```powershell
   $env:CLAUDE_CONFIG_DIR = $smokeClaudeDir   # same path as step 1
   cd C:\path\to\some\scratch\project
   claude
   ```

   - Submit any prompt. Expect: a new row appears in the popup titled from
     the prompt (R-5.2), status `working` while Claude is executing.
   - Let it finish without needing a permission grant. Expect: status →
     `idle`, a standard toast "`<project>` finished" with the default
     system sound (R-9.1).
   - Ask Claude to do something that needs a permission prompt (e.g. "run
     `git status`" if you haven't pre-approved Bash, or any tool outside
     your existing allowlist). Expect: status → `attention`, an alert toast
     "`<project>` needs you" with the distinct alert sound (R-9.2). Grant
     the permission in the terminal; expect status to recover to `working`
     within ~10s (the engine's tick interval, R-2.2 — it needs the
     transcript file's mtime to have advanced ≥2s past the notification).
   - Ask Claude to use the bundled skill to ask you something (with agent
     questions enabled per step 3, prompt something like "ask me via
     quarterdeck whether you should use tabs or spaces, then proceed
     accordingly" — the skill teaches it when to reach for `ask_user`,
     `skills/quarterdeck/SKILL.md`). Expect: the always-on-top ask window
     appears (R-8.3) without stealing your terminal's keyboard focus,
     showing the question; answer it (click an option, or type + Enter);
     expect Claude's turn to continue using your answer, and the row's
     status to have shown `attention` while the ask was pending (R-2.4).
   - End the session (`/exit` or close the terminal, or `/clear`). Expect
     the row to disappear (R-2.5/R-5.1).

5. **Read `$smokeDataDir\logs\quarterdeck.log`** — confirm it exists, is
   rotating-sized-capped (R-10.4), and has no `ERROR`-level lines from the
   session you just ran (`WARN`s about e.g. a best-effort skill/MCP step
   failing are fine to eyeball, `ERROR`s are not).

6. Tear down: close Quarterdeck, `claude mcp remove --scope user quarterdeck`
   if step 3 registered it globally instead of into the isolated dir (check
   which `CLAUDE_CONFIG_DIR` `claude mcp list` used), delete
   `$smokeClaudeDir`/`$smokeDataDir`.

---

## Known issues found while building this procedure

**No Tauri v2 capabilities file exists (`src-tauri/capabilities/` is
entirely absent) → the frontend's `deck://state` live push never arrives in
any window.** Confirmed by `e2e/real-app-smoke.mjs` against the real built
app:

```
window.__TAURI__.event.listen('deck://state', () => {})
  → rejects: "Command plugin:event|listen not allowed by ACL"
window.__TAURI__.core.invoke('get_state')
  → resolves fine (custom app commands aren't gated the same way)
```

Practical effect: **the popup only ever shows whatever `get_state()`
returned the moment its webview first loaded** (once, at app startup,
before any session exists) **and then never updates again for the rest of
the app's lifetime** — not on new sessions, not on status changes, not on
new asks. Re-clicking the tray icon just re-shows the same stale, frozen
window (R-3.6 keeps it alive rather than destroying/recreating it, which
normally would have masked this by re-priming on every open). Toasts still
fire correctly (`notify.rs` calls the plugin from Rust, which isn't subject
to the JS-side ACL), and the MCP `ask_user`/`notify_user` round trip is
unaffected server-side — but the ask window and popup will not visibly
reflect either without a full app restart.

During Part C, if the popup looks frozen after step 4's first row should
have appeared, that is this bug, not a regression you introduced — restart
Quarterdeck to re-observe current state, or (if you have the CDP debug flag
set per Part B) `page.reload()` the webview.

**Suggested fix** (outside T8's owned paths — `e2e/**`,
`scripts/inject-events.mjs`, this file — so not applied here): add a
`src-tauri/capabilities/default.json` granting at least `core:default`
(bundles `core:event:default`, `core:window:default`, etc.) to the `popup`
and `ask` windows, e.g.:

```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "default",
  "windows": ["popup", "ask"],
  "permissions": ["core:default"]
}
```

then re-verify `event.listen` resolves and Esc-to-hide
(`windows.rs::setup_popup_behavior`'s injected
`window.__TAURI__.window.getCurrentWindow().hide()`, which very likely has
the same ACL problem — same root cause, not separately re-verified here)
works from a real keypress.
