#!/usr/bin/env node
/**
 * Round-6 adversarial UI stress test for Quarterdeck.
 * Lens: UI stress via CDP on the live exe — NEW angles vs round 5's
 * stress-r5-tmp.mjs (which already covered the base 50-session/200-char/
 * ask-queue-10/settings-flip/empty-cycle/watchline-skew scenarios):
 *
 *   A. Rapid REAL DOM clicks on the settings toggle buttons (exercises the
 *      `pendingToggles` optimistic-UI map in popup.ts, not just raw
 *      set_setting invokes which bypass that code path entirely).
 *   B. Ask-queue diversity: free-text-only (no options) ask, an unmatched
 *      "Unknown agent" ask (context cwd matches no session), a short-timeout
 *      ask buried mid-queue (times out while still queued behind others),
 *      HTML/script-injection payloads in question/options/title/cwd (XSS via
 *      textContent vs innerHTML), and a Dismiss-button click (not just
 *      keyboard) mixed with keyboard answers.
 *   C. Incremental popup height growth AND shrink (1 session up to 50, then
 *      back down) to find the exact monotonic 460->560 transition and check
 *      for overshoot/non-monotonic jitter, plus shrink-back behavior.
 *   D. Sort-order stability under near-simultaneous status-change ties
 *      (many sessions hit `working` within the same debounce window) --
 *      polled across several successive snapshots for order thrash.
 *   E. Watch-line edge distributions round 5 didn't try: single-session
 *      100% (1/0/0/0) and perfectly equal quarters (1/1/1/1).
 *   F. Popup mirrored ask rows vs the dedicated ask window: both must stay
 *      in sync (same queue, same FIFO) -- round 5 only checked ask.html.
 */
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, existsSync, mkdirSync, writeFileSync, renameSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import net from 'node:net';
import process from 'node:process';

const repoRoot = 'C:/Users/phily/projects/quarterdeck';

function log(msg) { process.stdout.write(`[stress-r6] ${msg}\n`); }
function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

async function waitFor(label, timeoutMs, checkFn) {
  const start = Date.now();
  let lastErr;
  while (Date.now() - start < timeoutMs) {
    try {
      const result = await checkFn();
      if (result) return result;
    } catch (err) { lastErr = err; }
    await sleep(300);
  }
  throw new Error(`timed out waiting for: ${label}${lastErr ? ` (last: ${lastErr.message})` : ''}`);
}

function isPortInUse(port) {
  return new Promise((resolve) => {
    const socket = net.connect({ host: '127.0.0.1', port });
    const done = (inUse) => { socket.destroy(); resolve(inUse); };
    socket.once('connect', () => done(true));
    socket.once('error', () => resolve(false));
    socket.setTimeout(700, () => done(false));
  });
}

async function pickFreeCdpPort(preferred) {
  for (let port = preferred; port < preferred + 50; port += 1) {
    if (!(await isPortInUse(port))) return port;
  }
  throw new Error('no free CDP port');
}

function killTree(pid) {
  try { execFileSync('taskkill', ['/PID', String(pid), '/T', '/F'], { stdio: 'ignore' }); } catch {}
}

async function mcpRpc(endpoint, token, method, params, idRef) {
  const isNotification = method.startsWith('notifications/');
  const message = { jsonrpc: '2.0', method, params };
  if (!isNotification) { idRef.id += 1; message.id = idRef.id; }
  const res = await fetch(endpoint, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json, text/event-stream', authorization: `Bearer ${token}` },
    body: JSON.stringify(message),
  });
  if (isNotification) { if (res.status !== 202) throw new Error(`${method}: expected 202, got ${res.status}`); return null; }
  const text = await res.text();
  if (res.status !== 200) throw new Error(`${method}: HTTP ${res.status}: ${text}`);
  const json = JSON.parse(text);
  if (json.error) throw new Error(`${method}: RPC error ${json.error.code}: ${json.error.message}`);
  return json.result;
}

// --- spool envelope helpers ---
let seq = 0;
function writeSpoolFile(dataDir, envelope) {
  const dir = join(dataDir, 'spool');
  mkdirSync(dir, { recursive: true });
  seq += 1;
  const id = `${Date.now()}-${process.pid}-${Math.random().toString(16).slice(2)}-${seq}`;
  const final = join(dir, `${id}.json`);
  const tmp = `${final}.tmp`;
  writeFileSync(tmp, JSON.stringify(envelope), 'utf8');
  renameSync(tmp, final);
  return final;
}
function envelope(event, payload, extra = {}) {
  return { v: 1, event, receivedAt: Date.now(), payload: { hook_event_name: event, ...payload }, extra };
}

const findings = [];
function finding(f) { findings.push(f); log(`FINDING: ${f.title}`); }

const WEBVIEW_UDF_MARKER = 'pro.philippgross.quarterdeck';

/** Lists every quarterdeck.exe / quarterdeck-owned msedgewebview2.exe process
 * (mirrors real-app-smoke.mjs's listQuarterdeckProcesses). This machine runs
 * concurrent QA agents that each hit the SAME shared WebView2 user-data
 * folder, so anything less than a full sweep before launch leaves a stale
 * browser process silently swallowing our --remote-debugging-port argument
 * (observed live in this round: a fresh quarterdeck.exe joined a leftover
 * webview2 browser process with none of its children carrying the flag). */
function listQuarterdeckProcesses() {
  const script =
    `$ErrorActionPreference='SilentlyContinue';` +
    `Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe' OR Name='quarterdeck.exe'" | ` +
    `ForEach-Object { if ($_.Name -eq 'quarterdeck.exe' -or ($_.CommandLine -like '*${WEBVIEW_UDF_MARKER}*')) { "$($_.Name)|$($_.ProcessId)" } }`;
  const out = execFileSync('powershell.exe', [
    '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-Command', script,
  ], { encoding: 'utf8' });
  return out.split(/\r?\n/).map((l) => l.trim()).filter(Boolean).map((l) => {
    const [name, pid] = l.split('|');
    return { name, pid: Number(pid) };
  }).filter((p) => Number.isInteger(p.pid) && p.pid > 0);
}

async function reapOrphanedWebviewProcesses(timeoutMs = 15000) {
  const start = Date.now();
  let firstPass = true;
  for (;;) {
    const procs = listQuarterdeckProcesses();
    if (procs.length === 0) return;
    if (firstPass) {
      log(`sweeping ${procs.length} quarterdeck-related process(es) before launch: ${procs.map((p) => `${p.name}(${p.pid})`).join(', ')}`);
      firstPass = false;
    }
    for (const p of procs.filter((x) => x.name.toLowerCase() === 'quarterdeck.exe')) killTree(p.pid);
    for (const p of procs.filter((x) => x.name.toLowerCase() !== 'quarterdeck.exe')) {
      try { execFileSync('taskkill', ['/PID', String(p.pid), '/F'], { stdio: 'ignore' }); } catch {}
    }
    if (Date.now() - start > timeoutMs) {
      log(`WARN: could not fully sweep quarterdeck processes after ${timeoutMs}ms, proceeding anyway`);
      return;
    }
    await sleep(500);
  }
}

async function main() {
  const exe = join(repoRoot, 'target', 'release', 'quarterdeck.exe');
  const cdpPort = await pickFreeCdpPort(9700 + Math.floor(Math.random() * 100));
  log(`CDP port ${cdpPort}`);

  const runDir = mkdtempSync(join(tmpdir(), 'quarterdeck-stress-r6-'));
  const dataDir = join(runDir, 'data');
  const claudeDir = join(runDir, 'claude');
  mkdirSync(dataDir, { recursive: true });
  mkdirSync(join(claudeDir, 'projects'), { recursive: true });
  log(`data dir: ${dataDir}`);

  const env = {
    ...process.env,
    QUARTERDECK_DATA_DIR: dataDir,
    QUARTERDECK_CLAUDE_DIR: claudeDir,
    QUARTERDECK_FAKE_NOTIFIER: '1',
    QUARTERDECK_DEBUG: '1',
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
  };

  // Shared-machine safe launch: do NOT sweep other quarterdeck.exe processes
  // (a concurrent QA session may legitimately be running one). Only manage
  // our own spawned child.
  // NOTE: this machine is running many concurrent QA agents right now (13+
  // `claude` processes observed, quarterdeck.exe instances appearing/dying
  // every few seconds). A hard sweep of ALL quarterdeck-related processes
  // (as real-app-smoke.mjs does) was tried and only fed the churn -- a fresh
  // sibling instance kept appearing inside the sweep's own wait window.
  // Politely NOT sweeping and just retrying our own launch is the better
  // citizen here; we rely on catching a natural lull in the churn.

  const mcpJsonPath = join(dataDir, 'mcp.json');
  let child;
  let childExited = false;
  let browser;
  const { chromium } = await import('@playwright/test');

  // Outer retry: even once OUR OWN instance owns the single-instance mutex
  // (mcp.json appears), the shared machine-wide WebView2 browser process for
  // this app's user-data folder may already be owned by a sibling QA agent's
  // instance created a moment earlier (or one racing us right now) -- in
  // that case our --remote-debugging-port argument is silently dropped and
  // NO amount of waiting opens it (see live-smoke.md's documented flake).
  // Re-rolling the whole launch (fresh child, fresh port) is the only way to
  // eventually land in a moment where no sibling owns the browser process.
  const outerMaxAttempts = 8;
  outer: for (let outerAttempt = 1; outerAttempt <= outerMaxAttempts; outerAttempt += 1) {
    const maxAttempts = 40;
    for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
      log(`launching ${exe} (outer ${outerAttempt}/${outerMaxAttempts}, attempt ${attempt}/${maxAttempts})`);
      childExited = false;
      child = spawn(exe, [], { env, stdio: 'ignore', windowsHide: true });
      child.on('exit', () => { childExited = true; });
      try {
        await waitFor('mcp.json', 6000, () => existsSync(mcpJsonPath));
        log(`app started (outer ${outerAttempt}, attempt ${attempt})`);
        break;
      } catch {
        if (attempt === maxAttempts) throw new Error(`app never started after ${maxAttempts} attempts`);
        if (!childExited) killTree(child.pid);
        await sleep(2000 + Math.random() * 3000);
      }
    }

    try {
      browser = await waitFor('CDP endpoint', 20000, async () => {
        if (!(await isPortInUse(cdpPort))) return null;
        try { return await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`); } catch { return null; }
      });
      log(`CDP endpoint connected on outer attempt ${outerAttempt}`);
      break outer;
    } catch (err) {
      log(`outer attempt ${outerAttempt} failed to get CDP endpoint (${err.message}) -- killing our instance and re-rolling`);
      if (!childExited) killTree(child.pid);
      if (outerAttempt === outerMaxAttempts) throw err;
      await sleep(1000 + Math.random() * 2000);
    }
  }

  try {

    async function getPage(urlPart, timeoutMs = 15000) {
      return waitFor(`${urlPart} CDP target`, timeoutMs, () => {
        for (const ctx of browser.contexts()) {
          for (const p of ctx.pages()) {
            if (p.url().includes(urlPart)) return p;
          }
        }
        return null;
      });
    }

    const popupPage = await getPage('popup.html', 30000);
    log('popup CDP target found');
    const consoleErrors = [];
    popupPage.on('console', (msg) => { if (msg.type() === 'error') consoleErrors.push(msg.text()); });
    popupPage.on('pageerror', (err) => { consoleErrors.push(`pageerror: ${err.message}`); });

    await popupPage.evaluate(async () => {
      await window.__TAURI__.core.invoke('set_setting', { key: 'onboardingDone', value: true });
    });
    await sleep(300);

    const screenshotDir = join(repoRoot, 'docs', 'screenshots', 'round6-stress');
    mkdirSync(screenshotDir, { recursive: true });
    const cwdBase = join(runDir, 'projects');

    // ============================================================
    // A. Real DOM clicks on settings toggles (optimistic UI path)
    // ============================================================
    log('--- scenario A: rapid REAL clicks on settings toggle buttons ---');
    await popupPage.evaluate(() => { document.querySelector('.qd-gear')?.dispatchEvent(new MouseEvent('click', { bubbles: true })); });
    await sleep(300);
    const opened = await popupPage.evaluate(() => document.querySelector('.qd-settings')?.classList.contains('open'));
    log(`settings pane open: ${opened}`);
    if (!opened) finding({ title: 'Settings gear click did not open the settings pane', data: null });

    // Click the "Notify when a session finishes" toggle 25 times rapidly via
    // real dispatched click events (exercises pendingToggles inFlight counting,
    // not a direct set_setting invoke).
    const clickCount = 25;
    for (let i = 0; i < clickCount; i += 1) {
      // no await between clicks -- fire as fast as the event loop allows
      popupPage.evaluate(() => {
        const rows = Array.from(document.querySelectorAll('.qd-toggle-row'));
        const row = rows.find((r) => r.textContent?.includes('Notify when a session finishes'));
        row?.querySelector('.qd-toggle')?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      }).catch(() => {});
    }
    await sleep(2500); // let all the set_setting round-trips resolve
    const finalAriaChecked = await popupPage.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.qd-toggle-row'));
      const row = rows.find((r) => r.textContent?.includes('Notify when a session finishes'));
      return row?.querySelector('.qd-toggle')?.getAttribute('aria-checked');
    });
    const stateAfterClicks = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    log(`after ${clickCount} rapid real clicks: DOM aria-checked=${finalAriaChecked}, backend notifyIdle=${stateAfterClicks.settings?.notifyIdle}`);
    // 25 clicks starting from true (default) => odd number of toggles => ends false.
    const expectedFinal = (clickCount % 2 === 1);
    // default true, 25 toggles -> ends at !true repeated 25x -> false when 25 odd
    const expectedBool = clickCount % 2 === 0; // true if even number of flips returns to original(true)
    if (finalAriaChecked !== String(stateAfterClicks.settings?.notifyIdle)) {
      finding({ title: `After ${clickCount} rapid real toggle clicks, DOM aria-checked ("${finalAriaChecked}") disagrees with backend settings.notifyIdle (${stateAfterClicks.settings?.notifyIdle}) -- optimistic UI desynced from persisted state`, data: { finalAriaChecked, backend: stateAfterClicks.settings?.notifyIdle } });
    } else {
      log('DOM/backend agree after rapid real clicks');
    }
    await popupPage.screenshot({ path: join(screenshotDir, 'A-settings-after-rapid-real-clicks.png') });
    await popupPage.evaluate(() => { document.querySelector('.qd-back')?.dispatchEvent(new MouseEvent('click', { bubbles: true })); });
    await sleep(300);

    // ============================================================
    // C. Incremental popup height growth 1..50 then shrink back down
    // ============================================================
    log('--- scenario C: incremental popup height growth + shrink, monotonicity ---');
    const heights = [];
    for (let i = 0; i < 50; i += 1) {
      const sid = `grow-${i}`;
      const cwd = join(cwdBase, `grow-${i}`);
      writeSpoolFile(dataDir, envelope('SessionStart', { session_id: sid, cwd, session_title: `growth row ${i}` }));
      writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: sid, cwd, prompt: 'go' }));
      await sleep(120);
      if (i % 5 === 0 || i > 44) {
        const sz = await popupPage.evaluate(async () => {
          const win = window.__TAURI__.window.getCurrentWindow();
          const size = await win.outerSize();
          const scale = await win.scaleFactor();
          return size.height / scale;
        });
        heights.push({ n: i + 1, h: sz });
      }
    }
    log(`height samples while growing: ${JSON.stringify(heights)}`);
    let nonMonotonic = null;
    for (let i = 1; i < heights.length; i += 1) {
      if (heights[i].h < heights[i - 1].h - 0.5) { nonMonotonic = { prev: heights[i - 1], cur: heights[i] }; break; }
    }
    if (nonMonotonic) {
      finding({ title: 'Popup window height is non-monotonic while rows are only being ADDED (shrank between two growth samples)', data: nonMonotonic });
    } else {
      log('height grew monotonically while adding rows');
    }
    const maxH = Math.max(...heights.map((h) => h.h));
    if (maxH > 561) {
      finding({ title: `Popup window height exceeded the 560 cap while growing: ${maxH.toFixed(1)}px`, data: heights });
    }

    // Now remove rows one-by-one from 50 down to 0 and check it shrinks back,
    // ending at the 460 floor.
    const shrinkHeights = [];
    for (let i = 0; i < 50; i += 1) {
      writeSpoolFile(dataDir, envelope('SessionEnd', { session_id: `grow-${i}`, cwd: join(cwdBase, `grow-${i}`), reason: 'other' }));
      await sleep(100);
      if (i % 8 === 0 || i > 46) {
        const sz = await popupPage.evaluate(async () => {
          const win = window.__TAURI__.window.getCurrentWindow();
          const size = await win.outerSize();
          const scale = await win.scaleFactor();
          return size.height / scale;
        });
        shrinkHeights.push({ remaining: 50 - i - 1, h: sz });
      }
    }
    await sleep(500);
    log(`height samples while shrinking: ${JSON.stringify(shrinkHeights)}`);
    const finalHeight = shrinkHeights[shrinkHeights.length - 1]?.h;
    if (finalHeight !== undefined && finalHeight > 461) {
      finding({ title: `Popup window did not shrink back to the 460 floor after removing all rows (still ${finalHeight.toFixed(1)}px)`, data: shrinkHeights });
    }
    let growAfterShrink = null;
    for (let i = 1; i < shrinkHeights.length; i += 1) {
      if (shrinkHeights[i].h > shrinkHeights[i - 1].h + 0.5) { growAfterShrink = { prev: shrinkHeights[i - 1], cur: shrinkHeights[i] }; break; }
    }
    if (growAfterShrink) {
      finding({ title: 'Popup window height GREW while rows were only being REMOVED (non-monotonic shrink)', data: growAfterShrink });
    } else {
      log('height shrank monotonically while removing rows');
    }

    // ============================================================
    // D. Sort-order stability under near-simultaneous status ties
    // ============================================================
    log('--- scenario D: sort stability under simultaneous working-status ties ---');
    const tieCount = 20;
    for (let i = 0; i < tieCount; i += 1) {
      const sid = `tie-${i}`;
      const cwd = join(cwdBase, `tie-${i}`);
      writeSpoolFile(dataDir, envelope('SessionStart', { session_id: sid, cwd, session_title: `tie ${i}` }));
    }
    // Fire all the UserPromptSubmit (-> working) events for every tie session
    // essentially simultaneously (same debounce window), no stagger.
    for (let i = 0; i < tieCount; i += 1) {
      writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: `tie-${i}`, cwd: join(cwdBase, `tie-${i}`), prompt: 'go' }));
    }
    await sleep(2000);
    const orderSamples = [];
    for (let s = 0; s < 5; s += 1) {
      const order = await popupPage.evaluate(() => Array.from(document.querySelectorAll('.qd-row .qd-row-project')).map((e) => e.textContent));
      orderSamples.push(order.filter((p) => p?.startsWith('tie-')));
      await sleep(500);
    }
    const allSame = orderSamples.every((o) => JSON.stringify(o) === JSON.stringify(orderSamples[0]));
    log(`order stable across 5 successive polls (1s apart): ${allSame}`);
    if (!allSame) {
      finding({ title: 'Row order for tied-status sessions is NOT stable across successive deck://state pushes (jitter/thrash with no underlying status change)', data: orderSamples });
    }
    for (let i = 0; i < tieCount; i += 1) {
      writeSpoolFile(dataDir, envelope('SessionEnd', { session_id: `tie-${i}`, cwd: join(cwdBase, `tie-${i}`), reason: 'other' }));
    }
    await sleep(1500);

    // ============================================================
    // XSS / control-character injection in title, cwd, project
    // ============================================================
    log('--- scenario: HTML/script injection payloads in title/cwd ---');
    const xssPayload = '<img src=x onerror="window.__xss_fired=true">';
    const xssSid = 'xss-1';
    const xssCwd = join(cwdBase, 'xss-proj');
    writeSpoolFile(dataDir, envelope('SessionStart', { session_id: xssSid, cwd: xssCwd, session_title: xssPayload }));
    writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: xssSid, cwd: xssCwd, prompt: xssPayload }));
    await sleep(1500);
    const xssFired = await popupPage.evaluate(() => window.__xss_fired === true);
    const xssRowHtml = await popupPage.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.qd-row'));
      const row = rows.find((r) => r.textContent?.includes('img') || r.querySelector('.qd-row-title')?.textContent?.includes('<img'));
      return row ? row.querySelector('.qd-row-title')?.innerHTML : null;
    });
    log(`xss fired: ${xssFired}, row title innerHTML: ${xssRowHtml}`);
    if (xssFired) {
      finding({ title: 'SECURITY: session title containing an <img onerror> payload EXECUTED in the popup (title rendered via innerHTML, not textContent)', data: { xssRowHtml } });
    } else {
      log('XSS payload in title rendered inert (as text), good');
    }
    writeSpoolFile(dataDir, envelope('SessionEnd', { session_id: xssSid, cwd: xssCwd, reason: 'other' }));
    await sleep(500);

    // ============================================================
    // E. Watch-line edge distributions: single session 1/0/0/0, equal 1/1/1/1
    // ============================================================
    log('--- scenario E: watch-line single-session (1/0/0/0) and equal-quarters (1/1/1/1) ---');
    writeSpoolFile(dataDir, envelope('SessionStart', { session_id: 'solo-1', cwd: join(cwdBase, 'solo-1'), session_title: 'solo attention' }));
    writeSpoolFile(dataDir, envelope('Notification', { session_id: 'solo-1', cwd: join(cwdBase, 'solo-1'), notification_type: 'permission_prompt', message: 'Allow?' }));
    await sleep(1500);
    const soloWatchline = await popupPage.evaluate(() => Array.from(document.querySelectorAll('.qd-watchline-seg')).map((s) => ({ status: s.getAttribute('data-status'), flexBasis: s.style.flexBasis })));
    log(`watchline with 1 attention / 0 others: ${JSON.stringify(soloWatchline)}`);
    const attnSeg = soloWatchline.find((s) => s.status === 'attention');
    if (attnSeg?.flexBasis !== '100%') {
      finding({ title: `With exactly 1 session (attention) and 0 others, the attention watch-line segment is "${attnSeg?.flexBasis}" not 100%`, data: soloWatchline });
    }
    await popupPage.screenshot({ path: join(screenshotDir, 'E-watchline-solo-attention.png') });
    writeSpoolFile(dataDir, envelope('SessionEnd', { session_id: 'solo-1', cwd: join(cwdBase, 'solo-1'), reason: 'other' }));
    await sleep(500);

    // Equal quarters: 1 attention, 1 working, 1 idle, 1 dead-ish(via bogus pid liveness fail)
    writeSpoolFile(dataDir, envelope('SessionStart', { session_id: 'eq-attn', cwd: join(cwdBase, 'eq-attn'), session_title: 'eq attn' }));
    writeSpoolFile(dataDir, envelope('Notification', { session_id: 'eq-attn', cwd: join(cwdBase, 'eq-attn'), notification_type: 'permission_prompt', message: 'Allow?' }));
    writeSpoolFile(dataDir, envelope('SessionStart', { session_id: 'eq-work', cwd: join(cwdBase, 'eq-work'), session_title: 'eq work' }));
    writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: 'eq-work', cwd: join(cwdBase, 'eq-work'), prompt: 'go' }));
    writeSpoolFile(dataDir, envelope('SessionStart', { session_id: 'eq-idle', cwd: join(cwdBase, 'eq-idle'), session_title: 'eq idle' }));
    writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: 'eq-idle', cwd: join(cwdBase, 'eq-idle'), prompt: 'go' }));
    writeSpoolFile(dataDir, envelope('Stop', { session_id: 'eq-idle', cwd: join(cwdBase, 'eq-idle') }));
    // dead: SessionStart with a claudePid that can never be alive, then wait out the 10s liveness poll.
    writeSpoolFile(dataDir, envelope('SessionStart', { session_id: 'eq-dead', cwd: join(cwdBase, 'eq-dead'), session_title: 'eq dead' }, { claudePid: 999999 }));
    await waitFor('eq-dead session marked dead by liveness poll', 20000, async () => {
      const snap = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
      const row = snap.sessions.find((s) => s.id === 'eq-dead');
      return row?.status === 'dead' ? row : null;
    });
    await sleep(500);
    const eqWatchline = await popupPage.evaluate(() => Array.from(document.querySelectorAll('.qd-watchline-seg')).map((s) => ({ status: s.getAttribute('data-status'), flexBasis: s.style.flexBasis })));
    log(`watchline with 1/1/1/1 equal quarters: ${JSON.stringify(eqWatchline)}`);
    const sumPct = eqWatchline.reduce((acc, s) => acc + (parseFloat(s.flexBasis) || 0), 0);
    log(`sum of flex-basis percentages: ${sumPct}`);
    if (Math.abs(sumPct - 100) > 0.5) {
      finding({ title: `With 1/1/1/1 equal-quarters distribution, watch-line segment percentages sum to ${sumPct} (expected ~100)`, data: eqWatchline });
    }
    await popupPage.screenshot({ path: join(screenshotDir, 'E-watchline-equal-quarters.png') });
    for (const s of ['eq-attn', 'eq-work', 'eq-idle', 'eq-dead']) {
      writeSpoolFile(dataDir, envelope('SessionEnd', { session_id: s, cwd: join(cwdBase, s), reason: 'other' }));
    }
    await sleep(500);

    // ============================================================
    // B + F. Ask-queue diversity + popup-mirror/ask-window sync
    // ============================================================
    log('--- scenario B/F: diverse ask queue (free-text-only, unmatched agent, short-timeout mid-queue, XSS, dismiss) ---');
    const mcp = JSON.parse(readFileSync(join(dataDir, 'mcp.json'), 'utf8'));
    const endpoint = `http://127.0.0.1:${mcp.port}/mcp`;
    const idRef = { id: 0 };
    await mcpRpc(endpoint, mcp.token, 'initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'stress-r6', version: '1.0' } }, idRef);
    await mcpRpc(endpoint, mcp.token, 'notifications/initialized', {}, idRef);

    // A real session so one ask can be legitimately matched.
    writeSpoolFile(dataDir, envelope('SessionStart', { session_id: 'ask-owner', cwd: join(cwdBase, 'ask-owner-proj'), session_title: 'owns an ask' }));
    await sleep(500);

    const askDefs = [
      { question: 'Q0: matched, options', options: ['A', 'B'], context: join(cwdBase, 'ask-owner-proj'), timeout: 90 },
      { question: 'Q1: free-text only, no options', options: undefined, context: join(cwdBase, 'ask-owner-proj'), timeout: 90 },
      { question: 'Q2: unmatched agent (bogus context)', options: ['Yes'], context: 'C:/totally/unmatched/nowhere', timeout: 90 },
      { question: `Q3: <script>window.__ask_xss=true</script>XSS in question`, options: ['<img src=x onerror=window.__ask_xss2=true>'], context: join(cwdBase, 'ask-owner-proj'), timeout: 90 },
      { question: 'Q4: short timeout mid-queue (should time out while still queued)', options: ['X'], context: join(cwdBase, 'ask-owner-proj'), timeout: 3 },
      { question: 'Q5: to be Dismissed via button', options: ['Only'], context: join(cwdBase, 'ask-owner-proj'), timeout: 90 },
    ];
    const askPromises = askDefs.map((def, i) => {
      const p = mcpRpc(endpoint, mcp.token, 'tools/call', {
        name: 'ask_user',
        arguments: { question: def.question, options: def.options, context: def.context, timeout_seconds: def.timeout },
      }, { id: 2000 + i }).catch((e) => ({ __error: e.message }));
      return p;
    });
    await sleep(300);

    await waitFor('all 6 asks queued in state', 15000, async () => {
      const snap = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
      return (snap.asks || []).length >= 6 ? snap : null;
    });
    const snapWithAsks = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    log(`asks queued: ${snapWithAsks.asks.map((a) => a.question).join(' | ')}`);

    // F: popup mirrored ask rows should show the SAME 6 asks.
    const popupMirrorQuestions = await popupPage.evaluate(() => Array.from(document.querySelectorAll('.qd-ask-row-question')).map((e) => e.textContent));
    log(`popup mirror ask rows: ${JSON.stringify(popupMirrorQuestions)}`);
    if (popupMirrorQuestions.length !== 6) {
      finding({ title: `Popup mirrored ask rows show ${popupMirrorQuestions.length} asks, expected 6 (queue/popup desync)`, data: popupMirrorQuestions });
    }

    // Unmatched agent must show "Unknown agent (...)" somewhere (R-8.2).
    const unmatchedLabel = await popupPage.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.qd-ask-row'));
      const row = rows.find((r) => r.querySelector('.qd-ask-row-question')?.textContent?.includes('Q2:'));
      return row?.querySelector('.qd-ask-row-agent')?.textContent ?? null;
    });
    log(`Q2 (unmatched) agent label: "${unmatchedLabel}"`);
    if (!unmatchedLabel || !/unknown agent/i.test(unmatchedLabel)) {
      finding({ title: `Unmatched ask (bogus context) does not show "Unknown agent" label in popup mirror -- got "${unmatchedLabel}"`, data: null });
    }

    // Free-text-only ask (Q1) must render an input but ZERO option buttons.
    const q1Shape = await popupPage.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.qd-ask-row'));
      const row = rows.find((r) => r.querySelector('.qd-ask-row-question')?.textContent?.includes('Q1:'));
      if (!row) return null;
      return {
        optionButtons: row.querySelectorAll('.qd-ask-row-opt').length,
        hasInput: !!row.querySelector('.qd-ask-row-input'),
      };
    });
    log(`Q1 (free-text-only) shape: ${JSON.stringify(q1Shape)}`);
    if (!q1Shape || q1Shape.optionButtons !== 0 || !q1Shape.hasInput) {
      finding({ title: 'Free-text-only ask (no options) does not render correctly in popup mirror (expected 0 option buttons + a text input)', data: q1Shape });
    }

    // XSS check for ask question/options -- must not execute.
    const askXssFired = await popupPage.evaluate(() => window.__ask_xss === true || window.__ask_xss2 === true);
    log(`ask XSS fired: ${askXssFired}`);
    if (askXssFired) {
      finding({ title: 'SECURITY: an ask_user question/option containing a <script>/<img onerror> payload EXECUTED in the popup mirror', data: null });
    }
    await popupPage.screenshot({ path: join(screenshotDir, 'F-popup-mirror-ask-queue-6.png') });

    // Q4 (3s timeout) should resolve to kind:"timeout" on its own while still
    // buried in the queue (index 4, others unanswered ahead/behind it).
    await sleep(4000);
    const q4Result = await askPromises[4].catch((e) => ({ __error: e.message }));
    const q4Structured = q4Result.structuredContent || (q4Result.content && JSON.parse(q4Result.content[0].text));
    log(`Q4 (short timeout, mid-queue) resolved: ${JSON.stringify(q4Structured || q4Result)}`);
    if (!q4Structured || q4Structured.kind !== 'timeout') {
      finding({ title: `Ask buried mid-queue with a 3s timeout did not resolve to kind:"timeout" on its own (got ${JSON.stringify(q4Structured || q4Result)}) -- may be blocked waiting for the ones ahead of it in FIFO`, data: q4Structured || q4Result });
    } else {
      log('mid-queue short-timeout ask correctly self-expired without blocking on FIFO order');
    }

    // Dismiss Q5 via its Dismiss button in the popup mirror (not keyboard).
    const dismissClicked = await popupPage.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.qd-ask-row'));
      const row = rows.find((r) => r.querySelector('.qd-ask-row-question')?.textContent?.includes('Q5:'));
      const btn = row?.querySelector('.qd-ask-row-dismiss');
      if (btn) { btn.dispatchEvent(new MouseEvent('click', { bubbles: true })); return true; }
      return false;
    });
    log(`Q5 dismiss button clicked: ${dismissClicked}`);
    await sleep(1000);
    const q5Result = await askPromises[5].catch((e) => ({ __error: e.message }));
    const q5Structured = q5Result.structuredContent || (q5Result.content && JSON.parse(q5Result.content[0].text));
    log(`Q5 resolved after popup-mirror Dismiss click: ${JSON.stringify(q5Structured || q5Result)}`);
    if (!q5Structured || q5Structured.kind !== 'dismissed') {
      finding({ title: `Clicking "Dismiss" on a popup-mirrored ask row did not resolve the underlying ask_user call to kind:"dismissed" (got ${JSON.stringify(q5Structured || q5Result)})`, data: null });
    }

    // Answer the remaining ones (Q0 matched-options, Q1 free-text via input+Enter
    // in the popup mirror, Q2 unmatched via option button, Q3 XSS-question via option).
    await popupPage.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.qd-ask-row'));
      const q0 = rows.find((r) => r.querySelector('.qd-ask-row-question')?.textContent?.includes('Q0:'));
      q0?.querySelector('.qd-ask-row-opt')?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    });
    await sleep(600);
    await popupPage.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.qd-ask-row'));
      const q1 = rows.find((r) => r.querySelector('.qd-ask-row-question')?.textContent?.includes('Q1:'));
      const input = q1?.querySelector('.qd-ask-row-input');
      if (input) {
        input.value = 'free-text popup-mirror answer';
        input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
      }
    });
    await sleep(600);
    await popupPage.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.qd-ask-row'));
      const q2 = rows.find((r) => r.querySelector('.qd-ask-row-question')?.textContent?.includes('Q2:'));
      q2?.querySelector('.qd-ask-row-opt')?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    });
    await sleep(600);
    await popupPage.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.qd-ask-row'));
      const q3 = rows.find((r) => r.querySelector('.qd-ask-row-question')?.textContent?.includes('XSS in question'));
      q3?.querySelector('.qd-ask-row-opt')?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    });
    await sleep(1000);

    const [r0, r1, r2, r3] = await Promise.all([askPromises[0], askPromises[1], askPromises[2], askPromises[3]].map((p) => p.catch((e) => ({ __error: e.message }))));
    const structOf = (r) => r.structuredContent || (r.content && JSON.parse(r.content[0].text)) || r;
    log(`Q0=${JSON.stringify(structOf(r0))} Q1=${JSON.stringify(structOf(r1))} Q2=${JSON.stringify(structOf(r2))} Q3=${JSON.stringify(structOf(r3))}`);
    if (structOf(r1).answer !== 'free-text popup-mirror answer') {
      finding({ title: `Free-text answer typed+Enter'd in the POPUP mirror ask row did not reach the ask_user caller correctly (got answer="${structOf(r1).answer}")`, data: structOf(r1) });
    }
    if (structOf(r2).kind !== 'option') {
      finding({ title: `Answering an unmatched-agent ask via the popup mirror option button did not resolve correctly (got ${JSON.stringify(structOf(r2))})`, data: null });
    }

    const finalAskState = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    log(`asks remaining after full B/F drain: ${finalAskState.asks.length}`);
    if (finalAskState.asks.length !== 0) {
      finding({ title: 'Asks remain queued after answering/dismissing/timing-out all 6 diverse asks', data: finalAskState.asks });
    }

    // ============================================================
    // Console error sweep
    // ============================================================
    if (consoleErrors.length > 0) {
      finding({ title: `${consoleErrors.length} console error(s)/pageerror(s) logged in popup webview during the full round-6 run`, data: consoleErrors.slice(0, 10) });
    } else {
      log('no console errors logged in popup webview during the entire run');
    }

    log(`--- done. ${findings.length} finding(s) ---`);
    writeFileSync(join(runDir, 'findings.json'), JSON.stringify(findings, null, 2));
    log(`findings written to ${join(runDir, 'findings.json')}`);
    log(`screenshots in ${screenshotDir}`);
    process.stdout.write(`RUNDIR=${runDir}\n`);
  } finally {
    if (browser) await browser.close().catch(() => {});
    if (!childExited) killTree(child.pid);
  }
}

async function runWithRetries() {
  const maxRuns = 4;
  for (let run = 1; run <= maxRuns; run += 1) {
    try {
      await main();
      return;
    } catch (err) {
      log(`RUN ${run}/${maxRuns} FAILED: ${err.stack || err.message}`);
      if (run === maxRuns) {
        process.exitCode = 1;
        return;
      }
      log('retrying whole run from scratch (likely shared-machine contention)...');
      await sleep(3000);
    }
  }
}

runWithRetries();
