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
 * Tray test hook (SPEC §11 "assert tray icon changes (via test hook)"): in the
 * fake-notifier mode this run uses, `tray::update` appends the status it just
 * passed to `TrayIcon::set_icon` to `<data>/tray-state.jsonl` (see
 * `src-tauri/src/tray.rs`). This script asserts that file's last entry matches
 * the worst-of status of the injected fleet — a direct check on the real icon
 * swap, not just the popup DOM. The popup DOM read below is kept as a
 * complementary, per-row check.
 *
 * MCP round-trip (SPEC §11 "MCP: scripted Node client calls ask_user, test
 * answers via answer_ask command, asserts returned value"): this script also
 * drives the real MCP HTTP server the running exe serves — it calls `ask_user`
 * (blocking), answers it through the real `answer_ask` Tauri command (invoked
 * over the popup's CDP page), and asserts the value the blocked call returns.
 * That exercises the whole §3.1 ask channel against the built app: MCP ->
 * gateway -> ask -> answer_ask -> answers/ -> disk-watch -> MCP unblock.
 *
 * Usage:
 *   node e2e/real-app-smoke.mjs [--exe <path-to-quarterdeck.exe>] [--keep]
 *
 * Exit code 0 on pass, 1 on any assertion failure. Always attempts to kill
 * the spawned app and clean up its temp dirs, even on failure.
 */

import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, readFileSync, writeFileSync, existsSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import net from 'node:net';
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

/**
 * Resolves whether *something* is already listening on 127.0.0.1:<port>. Used to
 * pre-flight the WebView2 CDP debug port: a stray quarterdeck.exe (or anything
 * else) bound to the same port would otherwise silently steal our CDP connection,
 * so the DOM/screenshot assertions would run against the wrong process — or, if
 * they were non-fatal, be skipped entirely while the script still printed PASS.
 */
function isPortInUse(port) {
  return new Promise((resolve) => {
    const socket = net.connect({ host: '127.0.0.1', port });
    const done = (inUse) => {
      socket.destroy();
      resolve(inUse);
    };
    socket.once('connect', () => done(true));
    socket.once('error', () => resolve(false));
    socket.setTimeout(700, () => done(false));
  });
}

/**
 * Picks a CDP debug port that is provably free right now, starting from
 * `preferred` and probing upward. Avoids the hardcoded-port collision the old
 * script had: with a stray instance already on 9333, connecting there found no
 * popup.html target, which used to be swallowed as a warning (a false PASS).
 */
async function pickFreeCdpPort(preferred) {
  for (let port = preferred; port < preferred + 50; port += 1) {
    // eslint-disable-next-line no-await-in-loop
    if (!(await isPortInUse(port))) return port;
    log(`CDP port ${port} is already in use (a stray quarterdeck.exe?) — trying ${port + 1}`);
  }
  throw new Error(`no free CDP port found in ${preferred}..${preferred + 49}`);
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

/**
 * The Tauri-forced WebView2 user-data folder for this app lives under
 * `%LOCALAPPDATA%\pro.philippgross.quarterdeck` and is SHARED by every run of
 * quarterdeck.exe on this machine. WebView2 runs ONE browser process per
 * user-data folder, and `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` (our
 * `--remote-debugging-port`) is only applied when that browser process is
 * *created* — a new app instance that finds an existing/still-exiting browser
 * process for the same folder joins it and the debug-port argument is
 * silently dropped, so the CDP endpoint never opens no matter how long we
 * poll. That was the ~1-in-3 smoke flake: `taskkill /T /F` returns before the
 * msedgewebview2 process tree has fully exited, so a back-to-back run raced
 * the previous run's dying browser process.
 */
const WEBVIEW_UDF_MARKER = 'pro.philippgross.quarterdeck';

/**
 * Lists quarterdeck-related processes that could steal/deny the WebView2
 * debug port: any `quarterdeck.exe`, and any `msedgewebview2.exe` whose
 * command line references the quarterdeck WebView2 user-data folder
 * (browser/gpu/renderer/utility processes all carry `--user-data-dir=...`).
 * Windows-only; returns [] elsewhere (the smoke targets the Windows exe).
 */
function listQuarterdeckProcesses() {
  if (process.platform !== 'win32') return [];
  const script =
    `$ErrorActionPreference='SilentlyContinue';` +
    `Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe' OR Name='quarterdeck.exe'" | ` +
    `ForEach-Object { if ($_.Name -eq 'quarterdeck.exe' -or ($_.CommandLine -like '*${WEBVIEW_UDF_MARKER}*')) { "$($_.Name)|$($_.ProcessId)" } }`;
  const out = execFileSync('powershell.exe', [
    '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-Command', script,
  ], { encoding: 'utf8' });
  return out
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => {
      const [name, pid] = l.split('|');
      return { name, pid: Number(pid) };
    })
    .filter((p) => Number.isInteger(p.pid) && p.pid > 0 && p.pid !== process.pid);
}

/**
 * Kills every stray quarterdeck.exe / quarterdeck-owned msedgewebview2.exe and
 * WAITS until they are actually gone. Called (a) before launch, so our fresh
 * app instance is guaranteed to create a brand-new WebView2 browser process
 * that honours `--remote-debugging-port`, and (b) after killing our own child,
 * so this script never leaves a lingering browser process behind to flake the
 * next run. Throws after `timeoutMs` — pre-launch that is a real environment
 * problem worth failing loudly on, not a race to paper over.
 */
async function ensureNoQuarterdeckProcesses(phase, timeoutMs = 15_000) {
  if (process.platform !== 'win32') return;
  const start = Date.now();
  let firstPass = true;
  for (;;) {
    const procs = listQuarterdeckProcesses();
    if (procs.length === 0) {
      if (!firstPass) log(`${phase}: quarterdeck webview processes fully exited`);
      return;
    }
    if (firstPass) {
      log(`${phase}: found ${procs.length} leftover process(es): ${procs.map((p) => `${p.name}(${p.pid})`).join(', ')} — killing and waiting for exit`);
      firstPass = false;
    }
    // quarterdeck.exe first (with /T so its own webview children go too),
    // then any orphaned msedgewebview2 stragglers directly.
    for (const p of procs.filter((x) => x.name.toLowerCase() === 'quarterdeck.exe')) killTree(p.pid);
    for (const p of procs.filter((x) => x.name.toLowerCase() !== 'quarterdeck.exe')) {
      try {
        execFileSync('taskkill', ['/PID', String(p.pid), '/F'], { stdio: 'ignore' });
      } catch {
        // Already gone — fine.
      }
    }
    if (Date.now() - start > timeoutMs) {
      throw new Error(`${phase}: quarterdeck webview processes still alive after ${timeoutMs} ms: ${listQuarterdeckProcesses().map((p) => `${p.name}(${p.pid})`).join(', ')}`);
    }
    await sleep(500);
  }
}

/**
 * One MCP JSON-RPC call against the running app's streamable-HTTP server.
 * Returns the `result` object (throws on RPC error / non-200). Notifications
 * (method `notifications/*`) resolve to `null` after asserting a 202.
 */
async function mcpRpc(endpoint, token, method, params, idRef) {
  const isNotification = method.startsWith('notifications/');
  const message = { jsonrpc: '2.0', method, params };
  if (!isNotification) {
    idRef.id += 1;
    message.id = idRef.id;
  }
  const res = await fetch(endpoint, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      accept: 'application/json, text/event-stream',
      authorization: `Bearer ${token}`,
    },
    body: JSON.stringify(message),
  });
  if (isNotification) {
    if (res.status !== 202) throw new Error(`${method}: expected 202, got ${res.status}`);
    return null;
  }
  const text = await res.text();
  if (res.status !== 200) throw new Error(`${method}: HTTP ${res.status}: ${text}`);
  const json = JSON.parse(text);
  if (json.error) throw new Error(`${method}: RPC error ${json.error.code}: ${json.error.message}`);
  return json.result;
}

/**
 * Fire a blocking `ask_user` over the streamable-HTTP SSE path (R-19.3): sends a
 * `progressToken` so the server interleaves `notifications/progress` keepalive
 * frames while it waits, and reads the event stream incrementally. Returns
 * immediately with `{ id, progressFrames, result }` where `progressFrames` fills
 * as keepalives arrive and `result` resolves with the final JSON-RPC response
 * once the ask is answered/dismissed/cancelled/times-out. Does NOT block on the
 * answer, so the caller can observe survival across keepalive ticks, then answer.
 */
async function startSseAsk(endpoint, token, args, idRef) {
  idRef.id += 1;
  const id = idRef.id;
  const message = {
    jsonrpc: '2.0',
    id,
    method: 'tools/call',
    params: { name: 'ask_user', arguments: args, _meta: { progressToken: `ka-${id}` } },
  };
  const res = await fetch(endpoint, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      accept: 'text/event-stream',
      authorization: `Bearer ${token}`,
    },
    body: JSON.stringify(message),
  });
  if (res.status !== 200) throw new Error(`ask_user SSE: expected 200, got ${res.status}`);

  const progressFrames = [];
  let resolveResult;
  let rejectResult;
  const result = new Promise((resolve, reject) => { resolveResult = resolve; rejectResult = reject; });

  (async () => {
    try {
      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let buf = '';
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let sep;
        // SSE frames are separated by a blank line; each carries `data:` lines.
        while ((sep = buf.indexOf('\n\n')) >= 0 || (sep = buf.indexOf('\r\n\r\n')) >= 0) {
          const rawFrame = buf.slice(0, sep);
          buf = buf.slice(sep + (buf.startsWith('\r\n\r\n', sep) ? 4 : 2));
          const data = rawFrame
            .split(/\r?\n/)
            .filter((l) => l.startsWith('data:'))
            .map((l) => l.slice(5).trim())
            .join('\n');
          if (!data) continue;
          const json = JSON.parse(data);
          if (json.method === 'notifications/progress') {
            progressFrames.push(json);
          } else if (json.id === id) {
            resolveResult(json);
            return;
          }
        }
      }
      rejectResult(new Error('SSE stream closed before a final ask_user result'));
    } catch (err) {
      rejectResult(err);
    }
  })();

  return { id, progressFrames, result };
}

async function main() {
  const flags = parseArgs(process.argv.slice(2));
  const exe = flags.exe ?? join(repoRoot, 'target', 'release', 'quarterdeck.exe');
  const keep = flags.keep === 'true';

  if (!existsSync(exe)) {
    log(`FAIL: built exe not found at ${exe} (run 'npm run tauri build' first — a bare 'cargo build --release' binary loads the dev URL and fails the UI assertions; or pass --exe)`);
    process.exitCode = 1;
    return;
  }

  // Pre-flight 1: make sure NO quarterdeck WebView2 browser process exists.
  // The user-data folder is shared machine-wide, so a leftover (or still
  // exiting) browser process from a previous run/launch would make the fresh
  // app instance JOIN it — silently dropping our --remote-debugging-port and
  // guaranteeing a CDP-endpoint timeout (see WEBVIEW_UDF_MARKER doc comment).
  await ensureNoQuarterdeckProcesses('pre-launch');

  // Pre-flight 2: resolve a CDP debug port that is actually free, so a stray
  // listener can't steal the connection and turn the DOM/screenshot gate into a
  // silent no-op (SPEC §11 requires all three assertions to run).
  const cdpPort = await pickFreeCdpPort(Number(flags['cdp-port'] ?? 9333));
  log(`using CDP debug port ${cdpPort}`);

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
    // R-19.3 / §20: time-compress the keepalive so a persistent ask's
    // notifications/progress cadence (normally 30s) fires several times within
    // the smoke's wall-clock budget — the "time-compressed via env knob" the
    // spec's §20 gate calls for. The persistent-ask survival check below spans
    // many of these intervals to prove the call is NOT idle-aborted.
    QUARTERDECK_MCP_KEEPALIVE_MS: '300',
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
  };

  log(`launching ${exe}`);
  const child = spawn(exe, [], { env, stdio: 'ignore', windowsHide: true });
  let childExited = false;
  child.on('exit', () => { childExited = true; });

  const hardFailures = []; // any entry flips the exit code to 1

  try {
    // --- 1. wait for startup (mcp.json is only written once the MCP server
    //    has bound, i.e. well past spool replay + cold-start discovery) ----
    const mcpJsonPath = join(dataDir, 'mcp.json');
    // Generous startup budget: a freshly built exe on a cold machine (AV scan of
    // the new binary, WebView2 first-run bootstrap, spool replay + cold-start
    // discovery) can take well past 20 s to reach `setup()` and bind the MCP
    // server. `waitFor` polls every 300 ms, so a fast start still returns fast;
    // the ceiling only bounds a genuine hang, so keep it high to avoid a
    // timing-only FAIL (SPEC §11 hard gate must not be flaky).
    await waitFor('app startup (<data>/mcp.json written)', 60_000, () => {
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
    for (const c of calls) log(`  ${c.kind} [${c.sessionId}] "${c.title}" / "${c.body}" [${c.bodySource ?? '?'}]`);

    // --- 3a. R-24.1/R-24.4: the finished-toast body is the model's last words -
    // Give a session a transcript with a known assistant tail, drive it to
    // working then Stop, and assert its idle toast body == those words with
    // body_source "assistant" (the §23 incremental reader feeding the toast).
    try {
      const wordsSession = 'smoke-words';
      const wordsCwd = join(cwdBase, `${project}-words`);
      const transcriptPath = join(runDir, 'transcripts', `${wordsSession}.jsonl`);
      mkdirSync(dirname(transcriptPath), { recursive: true });
      const lastWords = 'All done — shipped the widget and added tests.';
      // A minimal but real-shaped assistant record (matches usage.rs parsing).
      const record = {
        type: 'assistant',
        message: {
          role: 'assistant',
          model: 'claude-sonnet-4-5',
          content: [{ type: 'text', text: lastWords }],
          usage: { input_tokens: 120, cache_creation_input_tokens: 0, cache_read_input_tokens: 40000, output_tokens: 300 },
        },
      };
      writeFileSync(transcriptPath, `${JSON.stringify(record)}\n`, 'utf8');

      for (const [cmd, extra] of [
        ['session-start', ['--title', 'words smoke']],
        ['prompt', ['--prompt', 'do the thing']],
        ['stop', []],
      ]) {
        execFileSync('node', [
          injector, '--data-dir', dataDir, cmd,
          '--session', wordsSession, '--cwd', wordsCwd, '--transcript', transcriptPath,
          ...extra,
        ], { stdio: 'ignore' });
        // Space the writes past the 250ms watcher debounce so SessionStart ->
        // prompt (working) -> Stop (idle) apply in order (the live ingest path
        // drains a coalesced burst in nondeterministic order).
        await sleep(500);
      }

      const wordsCall = await waitFor('idle toast body = assistant last words (R-24.1/R-24.4)', 15_000, () => {
        if (!existsSync(notifierPath)) return null;
        const lines = readFileSync(notifierPath, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l));
        return lines.find((l) => l.kind === 'idle' && l.sessionId === wordsSession) ?? null;
      });
      if (wordsCall.body !== lastWords || wordsCall.bodySource !== 'assistant') {
        hardFailures.push(`R-24.1: idle toast body/source mismatch — body="${wordsCall.body}" source="${wordsCall.bodySource}"`);
      } else {
        log(`idle-toast last-words OK — body="${wordsCall.body}" [${wordsCall.bodySource}]`);
      }
    } catch (err) {
      hardFailures.push(`R-24.1 last-words check failed: ${err.message}`);
    }

    // --- 3b. assert the tray icon actually changed (SPEC §11 test hook) -----
    // The fleet has an `attention` session, so the worst-of tray status (R-2.6)
    // must be `attention`. `tray::update` records each real `set_icon` swap to
    // <data>/tray-state.jsonl in fake-notifier mode.
    const trayStatePath = join(dataDir, 'tray-state.jsonl');
    const trayStatus = await waitFor('tray-state.jsonl showing attention (worst-of)', 15_000, () => {
      if (!existsSync(trayStatePath)) return null;
      const lines = readFileSync(trayStatePath, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l));
      if (lines.length === 0) return null;
      const last = lines[lines.length - 1];
      return last.status === 'attention' ? last : null;
    });
    log(`tray-state.jsonl OK — tray icon swapped to "${trayStatus.status}" (counts ${JSON.stringify(trayStatus.counts)})`);
  } catch (err) {
    hardFailures.push(`notifier/tray assertion: ${err.message}`);
    log(`FAIL: ${err.message}`);
  }

  // --- 4. required: CDP screenshot + DOM read of the popup (SPEC §11; see
  //    header comment — substitutes for the nonexistent tray test hook). Any
  //    failure below is a hard failure (see the catch), not a warning. -------
  try {
    const { chromium } = await import('@playwright/test');
    // WebView2 opens its `--remote-debugging-port` CDP endpoint only after the
    // browser process behind the (hidden) popup webview has finished spinning
    // up, which on a cold/first-run machine (AV scan, WebView2 bootstrap) can
    // lag the mcp.json startup signal by well over 10 s — poll (every 300 ms)
    // up to 60 s so a slow-but-healthy handshake still connects. Note the
    // endpoint appearing AT ALL is guaranteed by the pre-launch
    // `ensureNoQuarterdeckProcesses` sweep: with no pre-existing browser
    // process for the shared user-data folder, this run's app must create a
    // fresh one, which is the only case where WebView2 applies our debug-port
    // argument. `isPortInUse` first, so we only attempt the (noisier) CDP
    // connect once the port is actually listening.
    const browser = await waitFor('WebView2 CDP endpoint', 60_000, async () => {
      if (!(await isPortInUse(cdpPort))) return null;
      try {
        return await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`);
      } catch {
        return null;
      }
    });
    try {
      // WebView2 registers the hidden popup window's CDP page target a beat
      // after the debug endpoint starts answering, so poll (re-listing
      // contexts/pages) rather than enumerating once — checking a single time
      // races the target registration and makes the assertion silently not run.
      const popupPage = await waitFor('popup.html CDP target', 20_000, () => {
        for (const ctx of browser.contexts()) {
          for (const p of ctx.pages()) {
            if (p.url().includes('popup.html')) return p;
          }
        }
        return null;
      });

      // R-3.4 live push gate: `event.listen('deck://state')` MUST be permitted
      // by Tauri's ACL, otherwise the frontend never live-updates and the popup
      // is permanently stale (onboarding never renders, rows never change). This
      // requires a `src-tauri/capabilities/*.json` granting `core:event` to the
      // popup/ask windows. A blocked listen is a HARD failure here (R-11.1: a
      // green smoke must mean the core architecture actually works), not a
      // silently-tolerated warning.
      const listenResult = await popupPage.evaluate(async () => {
        try {
          const unlisten = await window.__TAURI__.event.listen('deck://state', () => {});
          unlisten();
          return { ok: true };
        } catch (err) {
          return { ok: false, error: String(err) };
        }
      });
      if (!listenResult.ok) {
        log(`FAIL: event.listen is blocked by Tauri ACL (${listenResult.error}) — the live deck://state push (R-3.4) never reaches any window.`);
        hardFailures.push(
          `event.listen blocked by ACL ("${listenResult.error}") — R-3.4 live push (deck://state) does not reach ` +
          'any window; the popup can never live-update. Needs a src-tauri/capabilities file granting core:event.',
        );
        // Still reload to read a get_state() snapshot so the DOM checks below can
        // run and surface any additional issues, but the run has already failed.
        await popupPage.reload({ waitUntil: 'load' });
        await popupPage.waitForSelector('#qd-content', { state: 'attached' });
      } else {
        log('event.listen succeeded (deck://state live push works) — R-3.4 verified.');
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

      // --- MCP ask_user round-trip answered via the real answer_ask command --
      // (SPEC §11): scripted client -> ask_user (blocks) -> answer through the
      // real Tauri `answer_ask` command (invoked over CDP, writing to
      // <data>/answers/) -> the disk-watch unblocks the MCP call -> assert the
      // returned {answer, kind}. This exercises the full §3.1 ask channel on the
      // real built exe, which nothing else in the repo covers end-to-end.
      try {
        const mcp = JSON.parse(readFileSync(join(dataDir, 'mcp.json'), 'utf8'));
        const endpoint = `http://127.0.0.1:${mcp.port}/mcp`;
        const idRef = { id: 0 };
        await mcpRpc(endpoint, mcp.token, 'initialize', {
          protocolVersion: '2025-06-18',
          capabilities: {},
          clientInfo: { name: 'quarterdeck-real-app-smoke', version: '1.0.0' },
        }, idRef);
        await mcpRpc(endpoint, mcp.token, 'notifications/initialized', {}, idRef);

        const question = 'Real-app smoke: proceed with the deploy?';
        const options = ['Yes', 'No'];
        // Fire the blocking ask_user; do NOT await yet — it stays open until we
        // answer it via answer_ask below.
        const askPromise = mcpRpc(endpoint, mcp.token, 'tools/call', {
          name: 'ask_user',
          arguments: { question, options, context: join(runDir, 'projects'), timeout_seconds: 60 },
        }, idRef);

        // Wait for the ask to surface in the real Shell state, then read its id
        // from a real get_state snapshot over the popup's Tauri bridge.
        const askId = await waitFor('ask_user question in app state', 20_000, async () => {
          const snap = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
          const ask = (snap.asks || []).find((a) => a.question === question);
          return ask ? ask.id : null;
        });
        log(`ask_user surfaced as ask "${askId}"`);

        // Answer it through the REAL answer_ask command (not a mock).
        await popupPage.evaluate(
          ({ id }) => window.__TAURI__.core.invoke('answer_ask', { askId: id, answer: 'Yes', kind: 'option' }),
          { id: askId },
        );

        // The blocked MCP call must now return the answer we submitted.
        const call = await askPromise;
        const structured = call.structuredContent || JSON.parse(call.content[0].text);
        if (structured.answer !== 'Yes' || structured.kind !== 'option') {
          hardFailures.push(
            `MCP ask_user round-trip: expected {answer:"Yes", kind:"option"}, got ${JSON.stringify(structured)}`,
          );
          log(`FAIL: MCP ask_user returned ${JSON.stringify(structured)}`);
        } else {
          log(`MCP ask_user round-trip OK -> {answer:"${structured.answer}", kind:"${structured.kind}"} (via real answer_ask)`);
        }
      } catch (err) {
        hardFailures.push(`MCP ask_user round-trip did not complete: ${err.message}`);
        log(`FAIL: MCP ask_user round-trip: ${err.message}`);
      }

      // --- persistent ask survives many keepalive intervals (R-19.3 / §20) ---
      // A persistent ask (no timeout_seconds) must NOT be idle-aborted: while it
      // is blocked, the server streams notifications/progress every keepalive
      // interval (time-compressed to 300ms via QUARTERDECK_MCP_KEEPALIVE_MS in
      // env). Prove the call stays open across many intervals (stand-in for the
      // ">6min" the spec names, at 30s real cadence), then answer it and assert
      // the real result — the §20 keepalive-survival gate against the real exe.
      try {
        const mcp = JSON.parse(readFileSync(join(dataDir, 'mcp.json'), 'utf8'));
        const endpoint = `http://127.0.0.1:${mcp.port}/mcp`;
        const idRef = { id: 100 };
        await mcpRpc(endpoint, mcp.token, 'initialize', {
          protocolVersion: '2025-06-18',
          capabilities: {},
          clientInfo: { name: 'quarterdeck-keepalive-smoke', version: '1.0.0' },
        }, idRef);
        await mcpRpc(endpoint, mcp.token, 'notifications/initialized', {}, idRef);

        const question = 'Persistent keepalive smoke: keep waiting?';
        // No timeout_seconds → persistent (R-19.2); progressToken → keepalive SSE.
        const sse = await startSseAsk(
          endpoint,
          mcp.token,
          { question, context: join(runDir, 'projects') },
          idRef,
        );

        // It must surface as a persistent ask — NO countdown (timeoutAt absent).
        const askId = await waitFor('persistent ask in app state (no timeout)', 20_000, async () => {
          const snap = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
          const ask = (snap.asks || []).find((a) => a.question === question);
          if (!ask) return null;
          if (ask.timeoutAt !== undefined && ask.timeoutAt !== null) {
            throw new Error(`persistent ask must carry no timeout, got timeoutAt=${ask.timeoutAt}`);
          }
          return ask.id;
        });

        // Span many keepalive intervals (300ms each) — well past when a
        // non-keepalived call would look idle — and require the ask to still be
        // pending with several progress frames received (proof of survival).
        const survived = await waitFor('>=6 keepalive progress frames while still pending', 15_000, async () => {
          if (sse.progressFrames.length < 6) return null;
          const snap = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
          const stillPending = (snap.asks || []).some((a) => a.id === askId);
          return stillPending ? sse.progressFrames.length : null;
        });
        log(`persistent ask survived ${survived} keepalive frame(s) while still pending (R-19.3)`);

        // Answer through the REAL answer_ask command; the blocked SSE call must
        // then deliver the final result over the same stream.
        await popupPage.evaluate(
          ({ id }) => window.__TAURI__.core.invoke('answer_ask', { askId: id, answer: 'keep going', kind: 'text' }),
          { id: askId },
        );
        const final = await sse.result;
        const structured = final.result?.structuredContent || JSON.parse(final.result.content[0].text);
        if (structured.answer !== 'keep going' || structured.kind !== 'text') {
          hardFailures.push(
            `R-19.3 persistent ask: expected {answer:"keep going", kind:"text"}, got ${JSON.stringify(structured)}`,
          );
          log(`FAIL: R-19.3 persistent ask returned ${JSON.stringify(structured)}`);
        } else {
          log(`persistent ask answered after keepalive survival -> {answer:"${structured.answer}", kind:"${structured.kind}"} (R-19.3 OK)`);
        }
      } catch (err) {
        hardFailures.push(`R-19.3 persistent-ask keepalive survival did not complete: ${err.message}`);
        log(`FAIL: R-19.3 persistent-ask keepalive: ${err.message}`);
      }
    } finally {
      await browser.close().catch(() => {});
    }
  } catch (err) {
    // SPEC §11 requires the E2E smoke to assert the tray-equivalent per-row
    // statuses AND capture the popup screenshot — not just the notifier trail.
    // A failure to connect over CDP, find the popup target, or write the
    // screenshot means those two assertions never ran, so it is a HARD failure
    // (R-11.1: a green smoke must mean all three checks actually passed), not a
    // silently-tolerated warning. The port is pre-flighted free above, so this
    // is a real regression signal, not a routine port collision.
    log(`FAIL: screenshot/DOM check did not complete: ${err.message}`);
    hardFailures.push(`screenshot/DOM check did not complete: ${err.message}`);
  }

  // --- cleanup --------------------------------------------------------------
  if (!childExited) {
    log(`stopping app (pid ${child.pid})`);
    killTree(child.pid);
  }
  // Wait until the WebView2 browser process tree has actually exited —
  // `taskkill /T /F` returns before the msedgewebview2 children are gone, and
  // a lingering browser process makes the NEXT run's webview join it and drop
  // its --remote-debugging-port (the old ~1-in-3 flake). Non-fatal here: the
  // assertions above already decided pass/fail, and the next run's pre-launch
  // sweep is the hard gate.
  try {
    await ensureNoQuarterdeckProcesses('post-run');
  } catch (err) {
    log(`WARN: ${err.message}`);
  }
  if (!keep) {
    rmSync(runDir, { recursive: true, force: true });
  } else {
    log(`--keep set: leaving ${runDir} in place`);
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
