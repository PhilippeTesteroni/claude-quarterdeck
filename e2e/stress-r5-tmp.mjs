#!/usr/bin/env node
/**
 * Round-5 adversarial UI stress test for Quarterdeck.
 * Lens: UI stress via CDP on the live exe.
 * - 50 sessions (scroll, perf, sort stability)
 * - 200-char titles and cwd tooltips
 * - ask queue 10 deep (badge, FIFO, keyboard 1-9)
 * - settings toggles rapid flipping
 * - empty->many->empty cycles
 * - popup grow-then-scroll boundary (460 vs 560)
 * - watch-line proportions with extreme count skews (50/1/0/0)
 */
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, readFileSync, existsSync, mkdirSync, writeFileSync, renameSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import net from 'node:net';
import process from 'node:process';

const repoRoot = 'C:/Users/phily/projects/quarterdeck';

function log(msg) { process.stdout.write(`[stress-r5] ${msg}\n`); }
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

const WEBVIEW_UDF_MARKER = 'pro.philippgross.quarterdeck';

function listQuarterdeckProcesses() {
  const script =
    `$ErrorActionPreference='SilentlyContinue';` +
    `Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe' OR Name='quarterdeck.exe'" | ` +
    `ForEach-Object { if ($_.Name -eq 'quarterdeck.exe' -or ($_.CommandLine -like '*${WEBVIEW_UDF_MARKER}*')) { "$($_.Name)|$($_.ProcessId)" } }`;
  const out = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-Command', script], { encoding: 'utf8' });
  return out.split(/\r?\n/).map((l) => l.trim()).filter(Boolean).map((l) => {
    const [name, pid] = l.split('|');
    return { name, pid: Number(pid) };
  }).filter((p) => Number.isInteger(p.pid) && p.pid > 0);
}

async function ensureNoQuarterdeckProcesses(phase, timeoutMs = 15000) {
  const start = Date.now();
  for (;;) {
    const procs = listQuarterdeckProcesses();
    if (procs.length === 0) return;
    log(`${phase}: killing ${procs.length} leftover proc(es)`);
    for (const p of procs.filter((x) => x.name.toLowerCase() === 'quarterdeck.exe')) killTree(p.pid);
    for (const p of procs.filter((x) => x.name.toLowerCase() !== 'quarterdeck.exe')) {
      try { execFileSync('taskkill', ['/PID', String(p.pid), '/F'], { stdio: 'ignore' }); } catch {}
    }
    if (Date.now() - start > timeoutMs) throw new Error(`${phase}: processes still alive`);
    await sleep(500);
  }
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

// --- spool envelope helpers (mirroring scripts/inject-events.mjs) ---
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

async function main() {
  const exe = join(repoRoot, 'target', 'release', 'quarterdeck.exe');
  const cdpPort = await pickFreeCdpPort(9600 + Math.floor(Math.random() * 100));
  log(`CDP port ${cdpPort}`);

  const runDir = mkdtempSync(join(tmpdir(), 'quarterdeck-stress-r5-'));
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

  // Launch with retry, WITHOUT sweeping other quarterdeck.exe processes: this
  // shared machine may have a concurrent QA session holding the OS-level
  // single-instance mutex (R-3.3, keyed off the app identifier, not the data
  // dir) — if so, our process hands off to theirs and exits(0) immediately
  // with no mcp.json ever written. Only kill OUR OWN spawned child (never a
  // sibling session's instance) and wait for the mutex to free up naturally.
  const mcpJsonPath = join(dataDir, 'mcp.json');
  let child;
  let childExited = false;
  const maxAttempts = 40;
  for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
    log(`launching ${exe} (attempt ${attempt}/${maxAttempts})`);
    childExited = false;
    child = spawn(exe, [], { env, stdio: 'ignore', windowsHide: true });
    child.on('exit', () => { childExited = true; });
    try {
      await waitFor('mcp.json', 6000, () => existsSync(mcpJsonPath));
      log(`app started on attempt ${attempt}`);
      break;
    } catch {
      if (attempt === maxAttempts) throw new Error(`app never started after ${maxAttempts} attempts (single-instance mutex likely held by a concurrent QA session on this machine the whole time)`);
      if (!childExited) killTree(child.pid); // our own attempt only
      await sleep(2000 + Math.random() * 3000);
    }
  }

  let browser;
  try {

    const { chromium } = await import('@playwright/test');
    browser = await waitFor('CDP endpoint', 60000, async () => {
      if (!(await isPortInUse(cdpPort))) return null;
      try { return await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`); } catch { return null; }
    });

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

    const popupPage = await getPage('popup.html');
    log('popup CDP target found');

    await popupPage.evaluate(async () => {
      await window.__TAURI__.core.invoke('set_setting', { key: 'onboardingDone', value: true });
    });
    await sleep(300);

    const screenshotDir = join(repoRoot, 'docs', 'screenshots', 'round5-stress');
    mkdirSync(screenshotDir, { recursive: true });

    log('--- scenario 1: empty state ---');
    await popupPage.screenshot({ path: join(screenshotDir, '01-empty.png') });

    log('--- scenario 2: 50 sessions, sort stability, 200-char titles, unicode cwd, deep paths ---');
    const cwdBase = join(runDir, 'projects');
    const longTitle = 'X'.repeat(200) + ' this is a very long title that should be truncated somewhere by the UI ellipsis logic';
    const unicodeProj = 'проект-кириллица-日本語-';
    const deepPath = join(cwdBase, 'a'.repeat(40), 'b'.repeat(40), 'c'.repeat(40), 'd'.repeat(40), unicodeProj);

    const N = 50;
    for (let i = 0; i < N; i += 1) {
      const sid = `stress-${i}`;
      let cwd, title;
      if (i === 0) { cwd = deepPath; title = longTitle; }
      else if (i === 1) { cwd = join(cwdBase, `unicode-${i}-${unicodeProj}`); title = `тест вопрос номер ${i} 日本語テスト`; }
      else { cwd = join(cwdBase, `proj-${i}`); title = `task ${i}`; }
      writeSpoolFile(dataDir, envelope('SessionStart', { session_id: sid, cwd, session_title: title, source: 'startup' }));
      if (i % 10 === 0) {
        writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: sid, cwd, prompt: title }));
      } else if (i % 17 === 0) {
        writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: sid, cwd, prompt: title }));
        writeSpoolFile(dataDir, envelope('Notification', { session_id: sid, cwd, notification_type: 'permission_prompt', message: `Allow something for ${i}?` }));
      } else {
        writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: sid, cwd, prompt: title }));
        writeSpoolFile(dataDir, envelope('Stop', { session_id: sid, cwd }));
      }
    }
    await sleep(3000);

    const stateAfter50 = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    log(`after 50 sessions: counts=${JSON.stringify(stateAfter50.counts)}, sessions.length=${stateAfter50.sessions.length}`);
    if (stateAfter50.sessions.length !== N) {
      finding({ title: `Expected ${N} sessions after injecting ${N}, got ${stateAfter50.sessions.length}`, data: stateAfter50.counts });
    }

    await popupPage.screenshot({ path: join(screenshotDir, '02-fifty-sessions-top.png') });

    const domRows = await popupPage.evaluate(() => Array.from(document.querySelectorAll('.qd-row')).map((r) => ({
      project: r.querySelector('.qd-row-project')?.textContent,
      status: r.querySelector('.qd-row-dot')?.getAttribute('data-status'),
    })));
    log(`DOM row count: ${domRows.length}`);
    const order = { attention: 0, working: 1, idle: 2, dead: 3 };
    let sortViolation = null;
    for (let i = 1; i < domRows.length; i += 1) {
      const prev = order[domRows[i - 1].status];
      const cur = order[domRows[i].status];
      if (cur < prev) { sortViolation = { i, prev: domRows[i - 1], cur: domRows[i] }; break; }
    }
    if (sortViolation) {
      finding({ title: 'Sort order (attention>working>idle>dead) violated with 50 sessions', data: sortViolation });
    } else {
      log('sort order OK across 50 rows');
    }

    const scrollCheck = await popupPage.evaluate(() => {
      const content = document.querySelector('.qd-content');
      if (!content) return null;
      content.scrollTop = content.scrollHeight;
      return { scrollTop: content.scrollTop, scrollHeight: content.scrollHeight, clientHeight: content.clientHeight };
    });
    log(`scroll check: ${JSON.stringify(scrollCheck)}`);
    await popupPage.screenshot({ path: join(screenshotDir, '03-fifty-sessions-scrolled.png') });

    const longTitleRowInfo = await popupPage.evaluate((needleTitleStart) => {
      const rows = Array.from(document.querySelectorAll('.qd-row'));
      const row = rows.find((r) => r.querySelector('.qd-row-title')?.textContent?.startsWith(needleTitleStart));
      if (!row) return null;
      return {
        found: true,
        titleAttr: row.getAttribute('title'),
        rowTitleTextLength: row.querySelector('.qd-row-title')?.textContent?.length,
        projectText: row.querySelector('.qd-row-project')?.textContent,
        rowOuterWidth: row.getBoundingClientRect().width,
      };
    }, 'XXXXXXXXXX');
    log(`long-title row info: ${JSON.stringify(longTitleRowInfo)}`);
    if (!longTitleRowInfo || !longTitleRowInfo.titleAttr) {
      finding({ title: 'Row with 200+ char title has no hover tooltip (title attribute) with full cwd', data: longTitleRowInfo });
    }

    const unicodeRowInfo = await popupPage.evaluate(() => {
      const rows = Array.from(document.querySelectorAll('.qd-row'));
      const row = rows.find((r) => /日本語|кирил/.test(r.textContent || ''));
      if (!row) return null;
      return { text: row.textContent, titleAttr: row.getAttribute('title') };
    });
    log(`unicode row info: ${JSON.stringify(unicodeRowInfo)}`);
    if (!unicodeRowInfo) {
      finding({ title: 'Unicode (Cyrillic/CJK) project/title row not found/rendered in DOM after 50-session injection', data: null });
    }

    log('--- scenario 3: popup height boundary 460 vs 560 ---');
    const winSizeMany = await popupPage.evaluate(async () => {
      const win = window.__TAURI__.window.getCurrentWindow();
      const size = await win.outerSize();
      const scale = await win.scaleFactor();
      return { width: size.width, height: size.height, scale };
    });
    log(`window size with 50 sessions: ${JSON.stringify(winSizeMany)}`);
    const logicalH = winSizeMany.height / winSizeMany.scale;
    if (logicalH > 561) {
      finding({ title: `Popup window height exceeds the 560 cap with 50 sessions: got ${logicalH.toFixed(1)}px logical`, data: winSizeMany });
    }
    if (logicalH < 559) {
      log(`NOTE: window did not reach 560 cap with 50 sessions (${logicalH.toFixed(1)}px) -- checking scrollability`);
      const contentScrolls = await popupPage.evaluate(() => {
        const c = document.querySelector('.qd-content');
        return c ? c.scrollHeight > c.clientHeight : null;
      });
      log(`content scrollable: ${contentScrolls}`);
      if (!contentScrolls) {
        finding({ title: 'With 50 sessions, popup window is below 560px cap AND content is not internally scrollable — some rows may be clipped/inaccessible', data: { logicalH, winSizeMany } });
      }
    }

    log('--- scenario 4: empty -> many -> empty cycles ---');
    for (let i = 0; i < N; i += 1) {
      writeSpoolFile(dataDir, envelope('SessionEnd', { session_id: `stress-${i}`, cwd: cwdBase, reason: 'other' }));
    }
    await sleep(3000);
    const stateEmptyAgain = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    log(`after removing all 50: sessions.length=${stateEmptyAgain.sessions.length}`);
    await popupPage.screenshot({ path: join(screenshotDir, '04-empty-again.png') });
    if (stateEmptyAgain.sessions.length !== 0) {
      finding({ title: 'Sessions remain after SessionEnd for all rows (empty->many->empty cycle)', data: { remaining: stateEmptyAgain.sessions.length } });
    }
    const winSizeEmptyAgain = await popupPage.evaluate(async () => {
      const win = window.__TAURI__.window.getCurrentWindow();
      const size = await win.outerSize();
      const scale = await win.scaleFactor();
      return { width: size.width, height: size.height, scale };
    });
    const logicalHEmpty = winSizeEmptyAgain.height / winSizeEmptyAgain.scale;
    log(`window height after going back to empty: ${logicalHEmpty.toFixed(1)}px`);
    if (logicalHEmpty > 461) {
      finding({ title: `Popup window did not shrink back to base 460px after returning to empty state (still ${logicalHEmpty.toFixed(1)}px)`, data: winSizeEmptyAgain });
    }

    for (let cycle = 0; cycle < 3; cycle += 1) {
      writeSpoolFile(dataDir, envelope('SessionStart', { session_id: `cyc-${cycle}`, cwd: join(cwdBase, `cyc-${cycle}`), session_title: `cycle ${cycle}` }));
      await sleep(400);
      writeSpoolFile(dataDir, envelope('SessionEnd', { session_id: `cyc-${cycle}`, cwd: join(cwdBase, `cyc-${cycle}`), reason: 'other' }));
      await sleep(400);
    }
    await sleep(1000);
    const stateAfterCycles = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    log(`after rapid empty<->1 cycles: sessions.length=${stateAfterCycles.sessions.length}`);
    if (stateAfterCycles.sessions.length !== 0) {
      finding({ title: 'Rapid create/end cycling left ghost rows behind', data: { remaining: stateAfterCycles.sessions } });
    }

    log('--- scenario 5: watch-line extreme skew 50 idle / 1 working / 0 attention / 0 dead ---');
    for (let i = 0; i < 50; i += 1) {
      const sid = `skew-idle-${i}`;
      const cwd = join(cwdBase, `skew-idle-${i}`);
      writeSpoolFile(dataDir, envelope('SessionStart', { session_id: sid, cwd, session_title: `idle ${i}` }));
      writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: sid, cwd, prompt: 'go' }));
      writeSpoolFile(dataDir, envelope('Stop', { session_id: sid, cwd }));
    }
    writeSpoolFile(dataDir, envelope('SessionStart', { session_id: 'skew-working-0', cwd: join(cwdBase, 'skew-working-0'), session_title: 'the one working session' }));
    writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: 'skew-working-0', cwd: join(cwdBase, 'skew-working-0'), prompt: 'go' }));
    await sleep(3000);

    const watchlineInfo = await popupPage.evaluate(() => {
      const segs = Array.from(document.querySelectorAll('.qd-watchline-seg'));
      return segs.map((s) => ({ status: s.getAttribute('data-status'), flexBasis: s.style.flexBasis || getComputedStyle(s).flexBasis, width: s.getBoundingClientRect().width }));
    });
    log(`watchline segments (50 idle / 1 working): ${JSON.stringify(watchlineInfo)}`);
    const counts50_1 = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state').then((s) => s.counts));
    log(`counts: ${JSON.stringify(counts50_1)}`);
    const workingSeg = watchlineInfo.find((s) => s.status === 'working');
    if (workingSeg && workingSeg.width < 1) {
      finding({ title: 'With extreme skew (50 idle vs 1 working), the working watch-line segment renders at ~0px width', data: { watchlineInfo, counts: counts50_1 } });
    }
    await popupPage.screenshot({ path: join(screenshotDir, '05-watchline-skew-50-1.png') });

    for (let i = 0; i < 50; i += 1) {
      writeSpoolFile(dataDir, envelope('SessionEnd', { session_id: `skew-idle-${i}`, cwd: join(cwdBase, `skew-idle-${i}`), reason: 'other' }));
    }
    writeSpoolFile(dataDir, envelope('SessionEnd', { session_id: 'skew-working-0', cwd: join(cwdBase, 'skew-working-0'), reason: 'other' }));
    await sleep(2000);
    log('cleared skew fixtures');

    log('--- scenario 6: rapid settings toggle flipping ---');
    const toggleKeys = ['notifyIdle', 'notifyAttention', 'notifyReminder', 'launchAtLogin'];
    await popupPage.evaluate(() => { document.querySelector('.qd-gear')?.dispatchEvent(new MouseEvent('click', { bubbles: true })); });
    await sleep(300);
    const settingsOpenCheck = await popupPage.evaluate(() => document.querySelector('.qd-settings')?.classList.contains('open'));
    log(`settings pane open: ${settingsOpenCheck}`);

    const rapidPromises = [];
    for (let round = 0; round < 20; round += 1) {
      for (const key of toggleKeys) {
        const val = round % 2 === 0;
        rapidPromises.push(popupPage.evaluate(({ k, v }) => window.__TAURI__.core.invoke('set_setting', { key: k, value: v }), { k: key, v: val }));
      }
    }
    const rapidResults = await Promise.allSettled(rapidPromises);
    const rapidErrors = rapidResults.filter((r) => r.status === 'rejected');
    log(`rapid toggle flips: ${rapidPromises.length} calls, ${rapidErrors.length} rejected`);
    if (rapidErrors.length > 0) {
      finding({ title: `${rapidErrors.length}/${rapidPromises.length} set_setting calls rejected during rapid flipping`, data: rapidErrors.slice(0, 3).map((r) => String(r.reason)) });
    }
    await sleep(1000);
    const settingsJsonPath = join(dataDir, 'settings.json');
    let settingsOnDisk = null;
    if (existsSync(settingsJsonPath)) {
      settingsOnDisk = JSON.parse(readFileSync(settingsJsonPath, 'utf8'));
    }
    const stateAfterFlip = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    log(`settings on disk: ${JSON.stringify(settingsOnDisk)}`);
    log(`settings in state: ${JSON.stringify(stateAfterFlip.settings)}`);
    for (const key of toggleKeys) {
      if (settingsOnDisk && stateAfterFlip.settings && settingsOnDisk[key] !== stateAfterFlip.settings[key]) {
        finding({ title: `After rapid toggle flipping, settings.json on disk disagrees with in-memory state for "${key}"`, data: { disk: settingsOnDisk[key], memory: stateAfterFlip.settings[key] } });
      }
    }
    for (const key of toggleKeys) {
      if (stateAfterFlip.settings && stateAfterFlip.settings[key] !== false) {
        finding({ title: `After rapid toggle flipping ending on "false", final in-memory value for "${key}" is "${stateAfterFlip.settings[key]}" not false — last-write-wins violated`, data: stateAfterFlip.settings });
      }
    }
    await popupPage.screenshot({ path: join(screenshotDir, '06-settings-after-rapid-flip.png') });
    await popupPage.evaluate(() => { document.querySelector('.qd-back')?.dispatchEvent(new MouseEvent('click', { bubbles: true })); });
    await sleep(300);

    log('--- scenario 7: 10 concurrent asks (queue, FIFO, badge, keyboard 1-9) ---');
    const mcp = JSON.parse(readFileSync(join(dataDir, 'mcp.json'), 'utf8'));
    const endpoint = `http://127.0.0.1:${mcp.port}/mcp`;
    const idRef = { id: 0 };
    await mcpRpc(endpoint, mcp.token, 'initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'stress-test', version: '1.0' } }, idRef);
    await mcpRpc(endpoint, mcp.token, 'notifications/initialized', {}, idRef);

    const askPromises = [];
    const askQuestions = [];
    for (let i = 0; i < 10; i += 1) {
      const question = `Stress ask #${i}: pick an option`;
      askQuestions.push(question);
      const p = mcpRpc(endpoint, mcp.token, 'tools/call', {
        name: 'ask_user',
        arguments: { question, options: ['Yes', 'No', 'Maybe'], context: join(cwdBase, `askctx-${i}`), timeout_seconds: 90 },
      }, { id: 1000 + i }).catch((e) => ({ __error: e.message }));
      askPromises.push(p);
      await sleep(60);
    }

    await waitFor('all 10 asks in state', 20000, async () => {
      const snap = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
      return (snap.asks || []).length >= 10 ? snap : null;
    });
    const stateWithAsks = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    log(`asks in state: ${stateWithAsks.asks.length}`);
    const fifoOk = stateWithAsks.asks.every((a, i) => a.question === askQuestions[i]);
    if (!fifoOk) {
      finding({ title: '10 concurrent asks are not in FIFO order in state.asks', data: stateWithAsks.asks.map((a) => a.question) });
    } else {
      log('FIFO order OK for 10 asks');
    }

    const askPage = await getPage('ask.html', 15000).catch((e) => { finding({ title: 'Ask window (ask.html) CDP target never appeared with 10 queued asks', data: String(e) }); return null; });
    if (askPage) {
      await sleep(500);
      await askPage.screenshot({ path: join(screenshotDir, '07-ask-window-queue-10.png') });
      const badgeText = await askPage.evaluate(() => document.querySelector('.qd-ask-badge')?.textContent ?? null);
      log(`ask window badge: "${badgeText}"`);
      if (badgeText && !/9/.test(badgeText)) {
        finding({ title: `Ask window badge with 10 queued asks reads "${badgeText}" — expected "9 more waiting"`, data: badgeText });
      }
      const shownQuestion = await askPage.evaluate(() => document.querySelector('.qd-ask-question')?.textContent ?? null);
      log(`ask window shows: "${shownQuestion}"`);
      if (shownQuestion !== askQuestions[0]) {
        finding({ title: `Ask window shows a question out of FIFO order: expected "${askQuestions[0]}", got "${shownQuestion}"`, data: null });
      }

      for (let i = 0; i < 10; i += 1) {
        const before = await askPage.evaluate(() => document.querySelector('.qd-ask-badge')?.textContent ?? null);
        await askPage.keyboard.press('1');
        await sleep(400);
        const after = i < 9 ? await askPage.evaluate(() => document.querySelector('.qd-ask-badge')?.textContent ?? null) : null;
        log(`step ${i}: badge before="${before}" after="${after}"`);
      }
      await sleep(1000);
      const settled = await Promise.all(askPromises.map((p) => p.catch((e) => ({ __error: e.message }))));
      let mismatchCount = 0;
      settled.forEach((call, i) => {
        if (call.__error) { finding({ title: `Ask #${i} MCP call errored during keyboard-1-9 queue drain: ${call.__error}`, data: null }); return; }
        const structured = call.structuredContent || (call.content && JSON.parse(call.content[0].text));
        if (!structured || structured.answer !== 'Yes' || structured.kind !== 'option') {
          mismatchCount += 1;
        }
      });
      if (mismatchCount > 0) {
        finding({ title: `${mismatchCount}/10 ask_user calls did not resolve to {answer:"Yes",kind:"option"} after pressing "1" for each in the 10-deep queue`, data: null });
      } else {
        log('all 10 asks resolved correctly via keyboard "1"');
      }

      const stateAfterAsksDrained = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
      log(`asks remaining after draining queue of 10: ${stateAfterAsksDrained.asks.length}`);
      if (stateAfterAsksDrained.asks.length !== 0) {
        finding({ title: 'Asks remain in state after answering all 10 via keyboard', data: { remaining: stateAfterAsksDrained.asks } });
      }
    }

    log(`--- done. ${findings.length} finding(s) ---`);
    writeFileSync(join(runDir, 'findings.json'), JSON.stringify(findings, null, 2));
    log(`findings written to ${join(runDir, 'findings.json')}`);
    log(`screenshots in ${screenshotDir}`);
    process.stdout.write(`RUNDIR=${runDir}\n`);
  } finally {
    if (browser) await browser.close().catch(() => {});
    // Only kill OUR OWN spawned child — never sweep system-wide here, a
    // concurrent QA session's own instance may legitimately be running.
    if (!childExited) killTree(child.pid);
  }
}

main().catch((err) => {
  log(`FATAL: ${err.stack || err.message}`);
  process.exitCode = 1;
});
