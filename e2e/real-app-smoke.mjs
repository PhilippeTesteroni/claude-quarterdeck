#!/usr/bin/env node
/**
 * T8 real-app smoke (SPEC §11 "E2E smoke (real built app on this machine)").
 *
 * Launches the actual built `quarterdeck.exe` (not the mocked-IPC dev
 * server) with an isolated `QUARTERDECK_DATA_DIR`/`QUARTERDECK_CLAUDE_DIR`
 * and the fake notifier (`QUARTERDECK_FAKE_NOTIFIER=1`, SPEC R-3.2), injects
 * a small synthetic session fleet via `scripts/inject-events.mjs`, then
 * asserts:
 *
 *   1. `<data>/notifier-calls.jsonl` gets the expected toast decisions
 *      (an `attention` toast for the session that hit a permission prompt,
 *      an `idle` toast for the one that got a `Stop`) — the hard pass/fail
 *      gate, and proof the spool -> engine -> notifier pipeline works
 *      end-to-end in the real app.
 *   2. A screenshot of the popup window, saved to
 *      `docs/screenshots/popup-live-smoke.png` (new file — T4's own
 *      dark/light screenshots in that directory are untouched).
 *
 * How the screenshot works without ever showing the (frameless, tray-only,
 * `visible:false`) popup window on screen: WebView2 honours
 * `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<n>`
 * (it's Chromium under the hood), so this script launches the app with that
 * env var set and drives the popup's webview over CDP via
 * `chromium.connectOverCDP` — the same trick used to test hidden/tray-only
 * Electron and Tauri apps. `Page.captureScreenshot` renders straight from
 * the compositor, independent of OS-level window visibility.
 *
 * There is no dedicated Rust-side "tray test hook" in this codebase (grepped
 * for one; none of T3/T7's modules expose the aggregate tray status via a
 * file, command, or debug event) — see the notesForIntegrator in this task's
 * report. This script substitutes for it by reading the popup DOM over the
 * same CDP connection: `StateSnapshot` (and therefore the tray's
 * `TrayStatus::worst_of`, which is a pure function of the identical
 * `Counts`) is derived from the same in-memory `Shell` the popup renders, so
 * asserting the popup's rendered row/dot statuses is an equivalent,
 * evidence-based check on the aggregate state the tray icon would show.
 *
 * Usage:
 *   node e2e/real-app-smoke.mjs [--exe <path-to-quarterdeck.exe>] [--keep]
 *
 * Exit code 0 on pass, 1 on any assertion failure. Always attempts to kill
 * the spawned app and clean up its temp dirs, even on failure.
 */

import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, readFileSync, existsSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import process from 'node:process';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..');

function parseArgs(argv) {
  const flags = {};
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i].startsWith('--')) {
      const key = argv[i].slice(2);
      const next = argv[i + 1];
      if (next === undefined || next.startsWith('--')) flags[key] = 'true';
      else { flags[key] = next; i += 1; }
    }
  }
  return flags;
}

function log(msg) {
  process.stdout.write(`[smoke] ${msg}\n`);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(label, timeoutMs, checkFn) {
  const start = Date.now();
  let lastErr;
  while (Date.now() - start < timeoutMs) {
    try {
      const result = await checkFn();
      if (result) return result;
    } catch (err) {
      lastErr = err;
    }
    await sleep(300);
  }
  throw new Error(`timed out waiting for: ${label}${lastErr ? ` (last error: ${lastErr.message})` : ''}`);
}

function killTree(pid) {
  if (process.platform === 'win32') {
    try {
      execFileSync('taskkill', ['/PID', String(pid), '/T', '/F'], { stdio: 'ignore' });
    } catch {
      // Already gone — fine.
    }
  } else {
    try {
      process.kill(pid, 'SIGKILL');
    } catch {
      // Already gone.
    }
  }
}

async function main() {
  const flags = parseArgs(process.argv.slice(2));
  const exe = flags.exe ?? join(repoRoot, 'target', 'release', 'quarterdeck.exe');
  const keep = flags.keep === 'true';
  const cdpPort = Number(flags['cdp-port'] ?? 9333);

  if (!existsSync(exe)) {
    log(`FAIL: built exe not found at ${exe} (run 'cargo build --release' first, or pass --exe)`);
    process.exitCode = 1;
    return;
  }

  const runDir = mkdtempSync(join(tmpdir(), 'quarterdeck-smoke-'));
  const dataDir = join(runDir, 'data');
  const claudeDir = join(runDir, 'claude');
  mkdirSync(dataDir, { recursive: true });
  mkdirSync(join(claudeDir, 'projects'), { recursive: true });
  log(`isolated data dir: ${dataDir}`);
  log(`isolated claude dir: ${claudeDir}`);

  const env = {
    ...process.env,
    QUARTERDECK_DATA_DIR: dataDir,
    QUARTERDECK_CLAUDE_DIR: claudeDir,
    QUARTERDECK_FAKE_NOTIFIER: '1',
    QUARTERDECK_DEBUG: '1',
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
  };

  log(`launching ${exe}`);
  const child = spawn(exe, [], { env, stdio: 'ignore', windowsHide: true });
  let childExited = false;
  child.on('exit', () => { childExited = true; });

  const hardFailures = []; // flip the exit code
  const warnings = []; // reported, but a documented/worked-around issue

  try {
    // --- 1. wait for startup (mcp.json is only written once the MCP server
    //    has bound, i.e. well past spool replay + cold-start discovery) ----
    const mcpJsonPath = join(dataDir, 'mcp.json');
    await waitFor('app startup (<data>/mcp.json written)', 20_000, () => {
      if (childExited) throw new Error('app process exited before starting up');
      return existsSync(mcpJsonPath);
    });
    log('app started (mcp.json present)');

    // --- 2. inject a 3-session fleet (working / attention / idle) ---------
    const injector = join(repoRoot, 'scripts', 'inject-events.mjs');
    const project = 'quarterdeck-smoke';
    const cwdBase = join(runDir, 'projects');
    execFileSync('node', [
      injector, '--data-dir', dataDir, '--preset', 'fleet',
      '--project', project, '--cwd', cwdBase, '--session-prefix', 'smoke',
    ], { stdio: 'inherit' });
    log('injected fleet fixture (working / attention / idle sessions)');

    // --- 3. assert notifier-calls.jsonl gets the expected toast decisions -
    const notifierPath = join(dataDir, 'notifier-calls.jsonl');
    const calls = await waitFor('notifier-calls.jsonl with attention + idle toasts', 15_000, () => {
      if (!existsSync(notifierPath)) return null;
      const lines = readFileSync(notifierPath, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l));
      const hasAttention = lines.some((l) => l.kind === 'attention' && l.sessionId === 'smoke-attention');
      const hasIdle = lines.some((l) => l.kind === 'idle' && l.sessionId === 'smoke-idle');
      return hasAttention && hasIdle ? lines : null;
    });
    log(`notifier-calls.jsonl OK — ${calls.length} call(s) recorded:`);
    for (const c of calls) log(`  ${c.kind} [${c.sessionId}] "${c.title}" / "${c.body}"`);
  } catch (err) {
    hardFailures.push(`notifier assertion: ${err.message}`);
    log(`FAIL: ${err.message}`);
  }

  // --- 4. best-effort: CDP screenshot + DOM read of the popup (see header
  //    comment — substitutes for the nonexistent tray test hook) ----------
  try {
    const { chromium } = await import('@playwright/test');
    const browser = await waitFor('WebView2 CDP endpoint', 10_000, async () => {
      try {
        return await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`);
      } catch {
        return null;
      }
    });
    try {
      const contexts = browser.contexts();
      let popupPage;
      for (const ctx of contexts) {
        for (const p of ctx.pages()) {
          if (p.url().includes('popup.html')) popupPage = p;
        }
      }
      if (!popupPage) throw new Error('no popup.html target found over CDP (windows may not have painted yet)');

      // KNOWN ISSUE (discovered by this script, not fixed here — outside
      // T8's owned paths): this repo ships with no `src-tauri/capabilities/`
      // file at all, so Tauri v2's default-deny ACL blocks every
      // plugin-namespaced JS->Rust call, INCLUDING `event.listen`. That
      // means the frontend's live `deck://state` push (R-3.4) never
      // arrives — the popup only ever reflects whatever `get_state()`
      // returned at initial page load, and then goes stale forever. Custom
      // `#[tauri::command]`s (get_state, remove_row, answer_ask, ...) are
      // NOT gated the same way and work fine, which is why a fresh
      // `page.reload()` (re-running the initial `get_state()` priming
      // call) is a faithful, if unfortunate, way to observe current
      // backend state here. See this task's report for the full repro and
      // the suggested fix (add a capabilities file granting `core:default`
      // to the popup/ask windows).
      const listenResult = await popupPage.evaluate(async () => {
        try {
          await window.__TAURI__.event.listen('deck://state', () => {});
          return { ok: true };
        } catch (err) {
          return { ok: false, error: String(err) };
        }
      });
      if (!listenResult.ok) {
        log(`WARN: event.listen is blocked by Tauri ACL (${listenResult.error}) — no capabilities file exists in src-tauri/. ` +
          'The popup will never live-update; reloading to read a fresh get_state() snapshot instead.');
        warnings.push(
          `event.listen blocked by ACL ("${listenResult.error}") — R-3.4 live push (deck://state) does not reach ` +
          'any window; worked around here via page.reload() (get_state() itself is unaffected). Needs a ' +
          'src-tauri/capabilities file (outside T8-owned paths) — see notesForIntegrator.',
        );
        await popupPage.reload({ waitUntil: 'load' });
        await popupPage.waitForSelector('#qd-content', { state: 'attached' });
      } else {
        log('event.listen succeeded (deck://state push works) — no reload needed.');
      }

      const rowStatuses = await popupPage.evaluate(() =>
        Array.from(document.querySelectorAll('.qd-row')).map((r) => ({
          project: r.querySelector('.qd-row-project')?.textContent,
          status: r.querySelector('.qd-row-dot')?.getAttribute('data-status'),
        })),
      );
      log(`popup DOM (tray-equivalent aggregate state): ${JSON.stringify(rowStatuses)}`);
      const expected = { 'quarterdeck-smoke-working': 'working', 'quarterdeck-smoke-attention': 'attention', 'quarterdeck-smoke-idle': 'idle' };
      let domOk = true;
      for (const [project, status] of Object.entries(expected)) {
        const row = rowStatuses.find((r) => r.project === project);
        if (!row || row.status !== status) {
          domOk = false;
          hardFailures.push(`popup DOM: expected row "${project}" status "${status}", DOM had: ${JSON.stringify(row)}`);
        }
      }
      if (domOk) {
        log('popup DOM reflects the injected fleet with the correct per-row statuses');
      }

      const screenshotDir = join(repoRoot, 'docs', 'screenshots');
      mkdirSync(screenshotDir, { recursive: true });
      const screenshotPath = join(screenshotDir, 'popup-live-smoke.png');
      await popupPage.screenshot({ path: screenshotPath });
      log(`screenshot saved: ${screenshotPath}`);
    } finally {
      await browser.close().catch(() => {});
    }
  } catch (err) {
    // The CDP connection itself (WebView2 debug port never responding, no
    // popup.html target found at all) is a harness/environment problem
    // distinct from an actual state-content mismatch — reported, but kept
    // out of hardFailures so a machine without loopback CDP access still
    // gets a clean signal from the (already-passed) notifier assertion.
    log(`WARN: screenshot/DOM check did not complete: ${err.message}`);
    warnings.push(`screenshot/DOM check did not complete: ${err.message}`);
  }

  // --- cleanup --------------------------------------------------------------
  if (!childExited) {
    log(`stopping app (pid ${child.pid})`);
    killTree(child.pid);
    await sleep(500);
  }
  if (!keep) {
    rmSync(runDir, { recursive: true, force: true });
  } else {
    log(`--keep set: leaving ${runDir} in place`);
  }

  if (warnings.length > 0) {
    log(`${warnings.length} warning(s) (did not fail the run):`);
    for (const w of warnings) log(`  - ${w}`);
  }
  if (hardFailures.length === 0) {
    log('PASS');
  } else {
    log(`FAIL — ${hardFailures.length} issue(s):`);
    for (const f of hardFailures) log(`  - ${f}`);
    process.exitCode = 1;
  }
}

main().catch((err) => {
  log(`FAIL: unhandled error: ${err.stack ?? err.message}`);
  process.exitCode = 1;
});
