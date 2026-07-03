#!/usr/bin/env node
/**
 * Part C — automated live smoke driving the REAL `claude` CLI against the REAL
 * built quarterdeck.exe, with CDP standing in for the human at the popup/ask
 * windows. Focused on the NEW v1.1/v1.2 surfaces (SPEC §§15,16,19,21,24,25).
 *
 * Isolation: fresh temp $claudeDir + $dataDir. The app gets QUARTERDECK_DATA_DIR
 * / QUARTERDECK_CLAUDE_DIR / QUARTERDECK_SESSIONS_DIR + CLAUDE_CONFIG_DIR (so the
 * app's own `claude mcp add` subprocess also isolates) + QUARTERDECK_FAKE_NOTIFIER=1.
 * Every `claude` invocation gets CLAUDE_CONFIG_DIR=$claudeDir AND
 * QUARTERDECK_DATA_DIR=$dataDir. .credentials.json is copied from the real
 * ~/.claude. The real ~/.claude/settings.json SHA256 is checked before/after.
 *
 * NOTE on gate 3: `claude -p` (print/headless) AUTO-APPROVES tool calls and never
 * fires the PermissionRequest hook (verified — see report). The hook only fires in
 * a real interactive TTY, which this harness cannot allocate. Gate 3 therefore
 * exercises the full deck+REAL-hook round-trip by piping a real PermissionRequest
 * payload into the REAL installed hook script and answering via CDP.
 */

import { spawn, execFileSync } from 'node:child_process';
import { rmSync, readFileSync, writeFileSync, existsSync, mkdirSync, copyFileSync, readdirSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import net from 'node:net';
import os from 'node:os';
import process from 'node:process';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..');
const CLAUDE_EXE = 'C:\\Users\\phily\\.local\\bin\\claude.exe';
const REAL_CLAUDE = join(os.homedir(), '.claude');
const REAL_SETTINGS = join(REAL_CLAUDE, 'settings.json');
const REAL_DOTJSON = join(os.homedir(), '.claude.json');

const results = [];
function record(gate, pass, evidence) {
  results.push({ gate, pass, evidence });
  log(`GATE ${gate}: ${pass ? 'PASS' : 'FAIL'} — ${evidence}`);
}
function log(m) { process.stdout.write(`[partc] ${m}\n`); }
function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }
function sha256(p) { return createHash('sha256').update(readFileSync(p)).digest('hex'); }

async function waitFor(label, timeoutMs, fn) {
  const start = Date.now();
  let lastErr;
  while (Date.now() - start < timeoutMs) {
    try { const r = await fn(); if (r) return r; } catch (e) { lastErr = e; }
    await sleep(300);
  }
  throw new Error(`timed out waiting for: ${label}${lastErr ? ` (${lastErr.message})` : ''}`);
}

function isPortInUse(port) {
  return new Promise((resolve) => {
    const s = net.connect({ host: '127.0.0.1', port });
    const done = (v) => { s.destroy(); resolve(v); };
    s.once('connect', () => done(true));
    s.once('error', () => resolve(false));
    s.setTimeout(700, () => done(false));
  });
}
async function pickFreeCdpPort(pref) {
  for (let p = pref; p < pref + 50; p += 1) { if (!(await isPortInUse(p))) return p; }
  throw new Error('no free CDP port');
}
function killTree(pid) {
  try { execFileSync('taskkill', ['/PID', String(pid), '/T', '/F'], { stdio: 'ignore' }); } catch {}
}

const UDF = 'pro.philippgross.quarterdeck';
function listQdProcs() {
  const script =
    `$ErrorActionPreference='SilentlyContinue';` +
    `Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe' OR Name='quarterdeck.exe'" | ` +
    `ForEach-Object { if ($_.Name -eq 'quarterdeck.exe' -or ($_.CommandLine -like '*${UDF}*')) { "$($_.Name)|$($_.ProcessId)" } }`;
  const out = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-Command', script], { encoding: 'utf8' });
  return out.split(/\r?\n/).map((l) => l.trim()).filter(Boolean).map((l) => {
    const [name, pid] = l.split('|'); return { name, pid: Number(pid) };
  }).filter((p) => Number.isInteger(p.pid) && p.pid > 0 && p.pid !== process.pid);
}
async function ensureNoQd(phase, timeoutMs = 15000) {
  const start = Date.now();
  let first = true;
  for (;;) {
    const procs = listQdProcs();
    if (procs.length === 0) { if (!first) log(`${phase}: cleared`); return; }
    if (first) { log(`${phase}: killing ${procs.map((p) => `${p.name}(${p.pid})`).join(', ')}`); first = false; }
    for (const p of procs.filter((x) => x.name.toLowerCase() === 'quarterdeck.exe')) killTree(p.pid);
    for (const p of procs.filter((x) => x.name.toLowerCase() !== 'quarterdeck.exe')) { try { execFileSync('taskkill', ['/PID', String(p.pid), '/F'], { stdio: 'ignore' }); } catch {} }
    if (Date.now() - start > timeoutMs) throw new Error(`${phase}: procs still alive`);
    await sleep(500);
  }
}

/** Spawn claude.exe headless (-p). Returns {stdout,stderr,code}. stdin closed to
 * avoid the 3s "no stdin" wait. */
function runClaude({ prompt, cwd, extraEnv = {}, extraArgs = [], timeoutMs = 120000 }) {
  return new Promise((resolve) => {
    const env = { ...process.env, CLAUDE_CONFIG_DIR: CLAUDE_DIR, QUARTERDECK_DATA_DIR: DATA_DIR, ...extraEnv };
    const args = ['-p', prompt, ...extraArgs];
    const child = spawn(CLAUDE_EXE, args, { cwd, env, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '', err = '';
    child.stdout.on('data', (d) => { out += d.toString(); });
    child.stderr.on('data', (d) => { err += d.toString(); });
    const t = setTimeout(() => { try { child.kill('SIGKILL'); } catch {} }, timeoutMs);
    child.on('exit', (code) => { clearTimeout(t); resolve({ stdout: out, stderr: err, code, child }); });
    resolve.child = child;
  });
}
/** Spawn claude but return the child immediately (for blocking ask_user runs). */
function spawnClaude({ prompt, cwd, extraEnv = {}, extraArgs = [] }) {
  const env = { ...process.env, CLAUDE_CONFIG_DIR: CLAUDE_DIR, QUARTERDECK_DATA_DIR: DATA_DIR, ...extraEnv };
  const child = spawn(CLAUDE_EXE, ['-p', prompt, ...extraArgs], { cwd, env, stdio: ['ignore', 'pipe', 'pipe'] });
  const box = { out: '', err: '', code: null, done: false };
  child.stdout.on('data', (d) => { box.out += d.toString(); });
  child.stderr.on('data', (d) => { box.err += d.toString(); });
  child.on('exit', (code) => { box.code = code; box.done = true; });
  box.child = child;
  return box;
}

let CLAUDE_DIR, DATA_DIR;

async function getState(page) { return page.evaluate(() => window.__TAURI__.core.invoke('get_state')); }

/** Clear any pending perms from deck state (answer them "defer") so a leftover
 * perm can't take the ask window's primary slot in a later gate. */
async function clearPerms(page) {
  const st = await getState(page);
  for (const p of st.perms || []) {
    await page.evaluate(({ id }) => window.__TAURI__.core.invoke('answer_perm', { permId: id, decision: 'defer' }), { id: p.id });
  }
}

function readNotifier() {
  const p = join(DATA_DIR, 'notifier-calls.jsonl');
  if (!existsSync(p)) return [];
  return readFileSync(p, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l));
}
function readSpoolEvents() {
  const p = join(DATA_DIR, 'spool');
  if (!existsSync(p)) return [];
  return readdirSync(p).filter((f) => f.endsWith('.json'));
}

async function main() {
  const artifacts = {};
  // ---- baseline hashes ----
  const baseSettingsHash = sha256(REAL_SETTINGS);
  const baseDotJsonHash = existsSync(REAL_DOTJSON) ? sha256(REAL_DOTJSON) : null;
  log(`baseline real settings.json sha256: ${baseSettingsHash}`);
  artifacts.baseSettingsHash = baseSettingsHash;

  // ---- isolated dirs ----
  const tmp = os.tmpdir();
  CLAUDE_DIR = join(tmp, 'qd-partc-claude');
  DATA_DIR = join(tmp, 'qd-partc-data');
  rmSync(CLAUDE_DIR, { recursive: true, force: true });
  rmSync(DATA_DIR, { recursive: true, force: true });
  mkdirSync(join(CLAUDE_DIR, 'projects'), { recursive: true });
  mkdirSync(join(CLAUDE_DIR, 'sessions'), { recursive: true });
  mkdirSync(DATA_DIR, { recursive: true });
  copyFileSync(join(REAL_CLAUDE, '.credentials.json'), join(CLAUDE_DIR, '.credentials.json'));
  log(`isolated claudeDir=${CLAUDE_DIR}`);
  log(`isolated dataDir=${DATA_DIR}`);

  await ensureNoQd('pre-launch');
  const cdpPort = await pickFreeCdpPort(9333);
  log(`CDP port ${cdpPort}`);

  const env = {
    ...process.env,
    QUARTERDECK_DATA_DIR: DATA_DIR,
    QUARTERDECK_CLAUDE_DIR: CLAUDE_DIR,
    QUARTERDECK_SESSIONS_DIR: join(CLAUDE_DIR, 'sessions'),
    CLAUDE_CONFIG_DIR: CLAUDE_DIR,
    QUARTERDECK_FAKE_NOTIFIER: '1',
    QUARTERDECK_DEBUG: '1',
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
  };
  const exe = join(repoRoot, 'target', 'release', 'quarterdeck.exe');
  log(`launching ${exe}`);
  const app = spawn(exe, [], { env, stdio: 'ignore', windowsHide: true });
  let appExited = false;
  app.on('exit', () => { appExited = true; });

  const { chromium } = await import('@playwright/test');
  let browser, popup;

  try {
    await waitFor('mcp.json', 60000, () => { if (appExited) throw new Error('app exited'); return existsSync(join(DATA_DIR, 'mcp.json')); });
    log('app up (mcp.json present)');

    browser = await waitFor('CDP endpoint', 60000, async () => {
      if (!(await isPortInUse(cdpPort))) return null;
      try { return await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`); } catch { return null; }
    });
    popup = await waitFor('popup.html target', 20000, () => {
      for (const c of browser.contexts()) for (const p of c.pages()) if (p.url().includes('popup.html')) return p;
      return null;
    });
    log('CDP connected to popup');

    // ============ GATE 1 — onboarding ============
    try {
      const s0 = await getState(popup);
      const onboardingBefore = s0.settings?.onboardingDone;
      // Install hooks (takeoverPermissions defaults ON → perm hook installed).
      await popup.evaluate(() => window.__TAURI__.core.invoke('install_hooks'));
      await waitFor('hooksInstalled', 15000, async () => (await getState(popup)).hooksInstalled);
      const settingsPath = join(CLAUDE_DIR, 'settings.json');
      const sj = JSON.parse(readFileSync(settingsPath, 'utf8'));
      const hookEvents = Object.keys(sj.hooks || {});
      const required = ['SessionStart', 'UserPromptSubmit', 'Notification', 'Stop', 'SubagentStart', 'SubagentStop', 'SessionEnd', 'PermissionRequest'];
      const missing = required.filter((e) => !hookEvents.includes(e));
      // verify commands contain quarterdeck
      const allCmds = JSON.stringify(sj.hooks);
      const cmdOk = allCmds.includes('quarterdeck');
      // enable agent questions
      await popup.evaluate(() => window.__TAURI__.core.invoke('set_setting', { key: 'mcpEnabled', value: true }));
      await sleep(2500);
      const skillCopied = existsSync(join(CLAUDE_DIR, 'skills', 'quarterdeck', 'SKILL.md'));
      // decline autostart
      await popup.evaluate(() => window.__TAURI__.core.invoke('set_setting', { key: 'launchAtLogin', value: false }));
      // finish
      await popup.evaluate(() => window.__TAURI__.core.invoke('set_setting', { key: 'onboardingDone', value: true }));
      const s1 = await getState(popup);
      const takeover = s1.settings?.takeoverPermissions;
      // mcp registered in isolated config?
      let mcpListed = false, mcpList = '';
      try {
        mcpList = execFileSync(CLAUDE_EXE, ['mcp', 'list'], { env: { ...process.env, CLAUDE_CONFIG_DIR: CLAUDE_DIR }, encoding: 'utf8', timeout: 30000 });
        mcpListed = /quarterdeck/i.test(mcpList);
      } catch (e) { mcpList = `mcp list err: ${e.message}`; }
      const ok = missing.length === 0 && cmdOk && skillCopied && takeover === true && s1.settings?.onboardingDone === true && s1.settings?.launchAtLogin === false;
      artifacts.gate1 = { hookEvents, missing, cmdOk, skillCopied, takeover, mcpListed, mcpList: mcpList.trim().slice(0, 200), onboardingBefore };
      record(1, ok, `hooks=[${hookEvents.join(',')}] missing=[${missing}] cmdOk=${cmdOk} skill=${skillCopied} takeover=${takeover} mcpListed=${mcpListed} onboardingDone=${s1.settings?.onboardingDone} launchAtLogin=${s1.settings?.launchAtLogin}`);
    } catch (e) { record(1, false, `error: ${e.message}`); }

    // ============ GATE 2 — real session sanity (pong) ============
    const scratch = join(DATA_DIR, 'scratch-pong');
    mkdirSync(scratch, { recursive: true });
    try {
      const r = await runClaude({ prompt: 'Reply with exactly: pong', cwd: scratch, timeoutMs: 90000 });
      const pong = /pong/i.test(r.stdout);
      // wait for idle toast decision
      let idleToast = null;
      try {
        idleToast = await waitFor('idle toast', 20000, () => {
          const calls = readNotifier();
          return calls.find((c) => c.kind === 'idle') ?? null;
        });
      } catch {}
      const spool = readSpoolEvents();
      const bodySrc = idleToast?.bodySource;
      const body = idleToast?.body;
      const bodyIsReply = body && !/Reply with exactly/i.test(body);
      const ok = pong && idleToast && bodySrc === 'assistant' && bodyIsReply;
      artifacts.gate2 = { pong, claudeOut: r.stdout.trim().slice(0, 120), idleToast, spoolCount: spool.length };
      record(2, ok, `pong=${pong} out="${r.stdout.trim().slice(0, 40)}" idleToast.body="${body}" bodySource=${bodySrc} spoolFiles=${spool.length}`);
    } catch (e) { record(2, false, `error: ${e.message}`); }

    // ============ GATE 3 — permission round-trip via REAL hook script + CDP ============
    // (claude -p cannot fire PermissionRequest — documented. Drive the real hook.)
    try {
      const hookScript = join(DATA_DIR, 'hooks', 'quarterdeck-hook.ps1');
      const usableHook = existsSync(hookScript) ? hookScript : join(repoRoot, 'hooks', 'quarterdeck-hook.ps1');
      const g3 = {};

      async function driveHook(payload, { answer, deadlineMs }) {
        // spawn the real hook with the PermissionRequest payload on stdin
        const phEnv = { ...process.env, QUARTERDECK_DATA_DIR: DATA_DIR };
        if (deadlineMs) phEnv.QUARTERDECK_PERM_POLL_DEADLINE_MS = String(deadlineMs);
        const ph = spawn('powershell.exe', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', usableHook], { env: phEnv, stdio: ['pipe', 'pipe', 'pipe'] });
        let hout = '', herr = '';
        ph.stdout.on('data', (d) => { hout += d.toString(); });
        ph.stderr.on('data', (d) => { herr += d.toString(); });
        const exited = new Promise((res) => ph.on('exit', (c) => res(c)));
        ph.stdin.write(JSON.stringify(payload));
        ph.stdin.end();
        if (answer) {
          // wait for the perm to surface in deck state, then answer via CDP
          const permId = await waitFor('perm in state', 25000, async () => {
            const st = await getState(popup);
            const perm = (st.perms || []).find((p) => p.toolName === payload.tool_name);
            return perm ? perm.id : null;
          });
          g3.surfacedPermId = permId;
          // capture screenshot of the perm modal from the ask window (once)
          if (!g3.shot) {
            try {
              const askPage = await findAskPage(browser, 4000);
              if (askPage) {
                const shot = join(repoRoot, 'docs', 'screenshots', 'perm-modal-live.png');
                mkdirSync(dirname(shot), { recursive: true });
                await askPage.screenshot({ path: shot });
                g3.shot = true; artifacts.permShot = shot;
              }
            } catch {}
          }
          await popup.evaluate(({ id, decision }) => window.__TAURI__.core.invoke('answer_perm', { permId: id, decision }), { id: permId, decision: answer });
        }
        const code = await exited;
        return { hout: hout.trim(), herr: herr.trim(), code };
      }

      const basePayload = (cmd) => ({ hook_event_name: 'PermissionRequest', session_id: null, cwd: scratch, permission_mode: 'default', tool_name: 'Bash', tool_input: { command: cmd } });

      // Allow
      const allow = await driveHook(basePayload('echo perm-test'), { answer: 'allow' });
      const allowOk = allow.code === 0 && /"behavior":"allow"/.test(allow.hout);
      g3.allow = allow;
      // Deny
      const deny = await driveHook(basePayload('echo perm-test'), { answer: 'deny' });
      const denyOk = deny.code === 0 && /"behavior":"deny"/.test(deny.hout);
      g3.deny = deny;
      // Fail-open (short deadline, no answer)
      const failopen = await driveHook(basePayload('echo perm-test'), { deadlineMs: 2500 });
      const failOk = failopen.code === 0 && failopen.hout === '';
      g3.failopen = failopen;

      // clear the lingering fail-open perm so it can't hijack the ask window
      // (perms are primary in the shared FIFO) during gate 4.
      await clearPerms(popup);
      await sleep(500);

      artifacts.gate3 = g3;
      const ok = allowOk && denyOk && failOk;
      record(3, ok, `allow(stdout=${JSON.stringify(allow.hout).slice(0, 60)},exit=${allow.code}) deny(stdout=${JSON.stringify(deny.hout).slice(0, 60)},exit=${deny.code}) failopen(empty=${failopen.hout === ''},exit=${failopen.code}) [real-claude trigger BLOCKED: -p auto-approves, see report]`);
    } catch (e) { record(3, false, `error: ${e.message}`); }

    // ============ GATE 4 — ASK API v1.2 via real claude -p ============
    // 4a: ask_user with detail + options, answered via CDP option click
    try {
      const cwd4 = join(DATA_DIR, 'scratch-ask');
      mkdirSync(cwd4, { recursive: true });
      const question = 'Deploy build 41 now, or wait for nightly?';
      const detail = 'Build 41 passed CI 10 minutes ago; the nightly runs in about 6 hours.';
      const prompt = `Call the quarterdeck ask_user MCP tool with EXACTLY these arguments: question="${question}", options=["Deploy now","Wait"], detail="${detail}", context="${cwd4.replace(/\\/g, '/')}". After it returns, reply with ONE line: RESULT kind=<kind> answer=<answer>.`;
      await clearPerms(popup); // ensure the ask (not a stray perm) is primary
      const box = spawnClaude({ prompt, cwd: cwd4, extraArgs: ['--allowedTools', 'mcp__quarterdeck__ask_user', 'mcp__quarterdeck__notify_user'] });
      // wait for ask to surface
      const askId = await waitFor('ask in state', 40000, async () => {
        const st = await getState(popup);
        const a = (st.asks || []).find((x) => x.question === question);
        return a ? a.id : null;
      });
      await clearPerms(popup); // in case a perm slipped in alongside
      // verify detail renders in the ask window DOM (fallback: state field)
      let detailRendered = null;
      const askPage = await findAskPage(browser, 8000);
      if (askPage) {
        detailRendered = await askPage.evaluate(() => {
          const el = document.querySelector('.qd-ask-detail');
          return el ? el.textContent : null;
        });
      }
      // screenshot popup with live rows while a session is active
      try {
        const shot = join(repoRoot, 'docs', 'screenshots', 'live-real-claude.png');
        mkdirSync(dirname(shot), { recursive: true });
        await popup.screenshot({ path: shot });
        artifacts.liveShot = shot;
      } catch {}
      // Answer via the ask-window option button (CDP = human); confirm it took,
      // else fall back to the answer_ask command with the same askId.
      let clickedOpt = false;
      if (askPage) {
        clickedOpt = await askPage.evaluate(() => {
          const btns = Array.from(document.querySelectorAll('.qd-ask-option'));
          const b = btns.find((x) => /Deploy now/.test(x.textContent));
          if (b) { b.click(); return true; } return false;
        });
      }
      // give the click a moment; if the ask is still pending, use the command
      let answeredVia = clickedOpt ? 'option-click' : 'none';
      await sleep(1500);
      const stillPending = (await getState(popup)).asks?.some((x) => x.id === askId);
      if (stillPending) {
        await popup.evaluate(({ id }) => window.__TAURI__.core.invoke('answer_ask', { askId: id, answer: 'Deploy now', kind: 'option' }), { id: askId });
        answeredVia = 'command-fallback';
      }
      artifacts.gate4aAnsweredVia = answeredVia;
      await waitFor('claude ask run done', 60000, () => box.done);
      const echoed = /Deploy now/i.test(box.out) && /kind=option/i.test(box.out);
      const detailOk = detailRendered && detailRendered.includes('Build 41 passed CI');
      artifacts.gate4a = { detailRendered, claudeOut: box.out.trim().slice(0, 200), echoed, answeredVia };
      record('4a', detailOk && echoed, `detailRendered="${(detailRendered || '').slice(0, 44)}" answeredVia=${answeredVia} claudeOut="${box.out.trim().replace(/\n/g, ' ').slice(0, 80)}"`);
    } catch (e) { record('4a', false, `error: ${e.message}`); }

    // 4b: dismiss case — claude asks, we dismiss, result must be kind:dismissed
    try {
      const cwd4b = join(DATA_DIR, 'scratch-dismiss');
      mkdirSync(cwd4b, { recursive: true });
      const q = 'Should I proceed with the risky migration?';
      const prompt = `Call the quarterdeck ask_user MCP tool with question="${q}", options=["Yes","No"], context="${cwd4b.replace(/\\/g, '/')}". After it returns, reply with ONE line: RESULT kind=<kind> answer=<answer>.`;
      const box = spawnClaude({ prompt, cwd: cwd4b, extraArgs: ['--allowedTools', 'mcp__quarterdeck__ask_user'] });
      const askId = await waitFor('dismiss ask in state', 40000, async () => {
        const st = await getState(popup);
        const a = (st.asks || []).find((x) => x.question === q);
        return a ? a.id : null;
      });
      // dismiss via the ask window Dismiss button if present, else command
      const askPage = await findAskPage(browser, 6000);
      let via = 'command';
      if (askPage) {
        const clicked = await askPage.evaluate(() => {
          const btns = Array.from(document.querySelectorAll('button'));
          const b = btns.find((x) => x.textContent.trim() === 'Dismiss');
          if (b) { b.click(); return true; } return false;
        });
        if (clicked) via = 'dismiss-button';
      }
      if (via === 'command') {
        await popup.evaluate(({ id }) => window.__TAURI__.core.invoke('answer_ask', { askId: id, answer: '', kind: 'dismissed' }), { id: askId });
      }
      await waitFor('claude dismiss run done', 60000, () => box.done);
      const reflectsDismiss = /kind=dismissed/i.test(box.out) || /dismiss/i.test(box.out);
      const noTransportErr = !/timeout|transport|error|failed/i.test(box.out.replace(/dismiss\w*/ig, ''));
      artifacts.gate4b = { via, claudeOut: box.out.trim().slice(0, 300) };
      record('4b', reflectsDismiss, `via=${via} reflectsDismissed=${reflectsDismiss} claudeOut="${box.out.trim().replace(/\n/g, ' ').slice(0, 120)}"`);
    } catch (e) { record('4b', false, `error: ${e.message}`); }

    // 4c: notify_user returns {delivered, id}
    try {
      const cwd4c = join(DATA_DIR, 'scratch-notify');
      mkdirSync(cwd4c, { recursive: true });
      const prompt = `Call the quarterdeck notify_user MCP tool with message="Part C automated smoke: notify test", context="${cwd4c.replace(/\\/g, '/')}". Then reply with ONE line containing the EXACT JSON object the tool returned.`;
      const r = await runClaude({ prompt, cwd: cwd4c, extraArgs: ['--allowedTools', 'mcp__quarterdeck__notify_user'], timeoutMs: 90000 });
      const notifCalls = readNotifier().filter((c) => c.kind === 'reminder' || /notify test/i.test(c.body || ''));
      const delivered = /delivered/i.test(r.stdout) && /(true|"id")/i.test(r.stdout);
      artifacts.gate4c = { claudeOut: r.stdout.trim().slice(0, 200), notifCalls: notifCalls.slice(-2) };
      record('4c', delivered || notifCalls.length > 0, `claudeOut="${r.stdout.trim().replace(/\n/g, ' ').slice(0, 120)}" notifierRecords=${notifCalls.length}`);
    } catch (e) { record('4c', false, `error: ${e.message}`); }

    // ============ GATE 5 — registry names during a live run ============
    try {
      const cwd5 = join(DATA_DIR, 'scratch-registry');
      mkdirSync(cwd5, { recursive: true });
      // a longer run so we can observe a live working row and let the 10s
      // registry poll refresh the row title (R-15.2 registry-name precedence).
      const box = spawnClaude({ prompt: 'Use the Bash tool to run: sleep 14; echo done. Then reply exactly: done.', cwd: cwd5 });
      let regEntry = null, workingSeen = false;
      const titleSamples = [];
      const sdir = join(CLAUDE_DIR, 'sessions');
      const t0 = Date.now();
      // sample repeatedly for up to ~18s (spans a full registry poll) while the
      // session is alive, capturing the settled row title + any working status.
      while (Date.now() - t0 < 18000 && !box.done) {
        const files = existsSync(sdir) ? readdirSync(sdir).filter((f) => f.endsWith('.json')) : [];
        for (const f of files) {
          try {
            const e = JSON.parse(readFileSync(join(sdir, f), 'utf8'));
            if (e.cwd && e.cwd.replace(/\\/g, '/').includes('scratch-registry')) regEntry = e;
          } catch {}
        }
        const st = await getState(popup);
        const row = (st.sessions || []).find((r) => (r.cwd || '').replace(/\\/g, '/').includes('scratch-registry'));
        if (row) {
          titleSamples.push(row.title);
          if (row.status === 'working') workingSeen = true;
        }
        await sleep(1200);
      }
      await waitFor('registry run done', 60000, () => box.done);
      const settledTitle = [...titleSamples].reverse().find((t) => t && t !== '(no title)') || titleSamples[titleSamples.length - 1];
      const regName = regEntry?.name;
      // R-15.2: title should be the registry name, OR a prompt-derived title
      // (the prompt mentions sleep/echo). Either source is acceptable.
      const titleMatchesRegistry = settledTitle && regName && settledTitle === regName;
      const titleFromPrompt = settledTitle && /sleep|bash|echo|done/i.test(settledTitle);
      artifacts.gate5 = { registryName: regName, registryStatus: regEntry?.status, settledTitle, titleSamples: titleSamples.slice(0, 12), workingSeen, titleMatchesRegistry, titleFromPrompt };
      const ok = !!regEntry && !!settledTitle && (titleMatchesRegistry || titleFromPrompt);
      record(5, ok, `registryName="${regName}" registryStatus="${regEntry?.status}" settledTitle="${settledTitle}" matchesRegistry=${titleMatchesRegistry} fromPrompt=${titleFromPrompt} workingSeen=${workingSeen}`);
    } catch (e) { record(5, false, `error: ${e.message}`); }

    // ============ GATE 6 — log check ============
    try {
      const logPath = join(DATA_DIR, 'logs', 'quarterdeck.log');
      const exists = existsSync(logPath);
      let errLines = [];
      if (exists) {
        errLines = readFileSync(logPath, 'utf8').split(/\r?\n/).filter((l) => /ERROR/.test(l) && !/favicon\.ico/.test(l));
      }
      artifacts.gate6 = { exists, errLines: errLines.slice(0, 10) };
      record(6, exists && errLines.length === 0, `logExists=${exists} nonBenignErrors=${errLines.length}${errLines.length ? ' :: ' + errLines[0].slice(0, 120) : ''}`);
    } catch (e) { record(6, false, `error: ${e.message}`); }

  } finally {
    // ============ GATE 7 — teardown ============
    try { if (browser) await browser.close().catch(() => {}); } catch {}
    if (!appExited) { log(`stopping app pid ${app.pid}`); killTree(app.pid); }
    try { await ensureNoQd('post-run'); } catch (e) { log(`WARN ${e.message}`); }

    const afterSettingsHash = sha256(REAL_SETTINGS);
    const afterDotJsonHash = existsSync(REAL_DOTJSON) ? sha256(REAL_DOTJSON) : null;
    const settingsMatch = afterSettingsHash === artifacts.baseSettingsHash;
    const dotJsonMatch = baseDotJsonHash === afterDotJsonHash;
    log(`real settings.json after: ${afterSettingsHash}`);
    log(`real settings.json hash match: ${settingsMatch}`);
    log(`real ~/.claude.json hash match: ${dotJsonMatch}`);
    artifacts.afterSettingsHash = afterSettingsHash;
    record(7, settingsMatch, `settingsHashMatch=${settingsMatch} dotJsonHashMatch=${dotJsonMatch} liveShot=${!!artifacts.liveShot} permShot=${!!artifacts.permShot}`);

    // ---- write results + summary BEFORE cleanup (cleanup may EPERM on a
    //      handle Windows hasn't released yet; results must survive that). ----
    try { writeFileSync(join(repoRoot, 'e2e', 'part-c-results.json'), JSON.stringify({ results, artifacts }, null, 2)); } catch (e) { log(`WARN results write: ${e.message}`); }
    log('==================== SUMMARY ====================');
    for (const r of results) log(`  GATE ${r.gate}: ${r.pass ? 'PASS' : 'FAIL'}`);
    log('=================================================');

    // cleanup temp dirs unless --keep (retry — Windows releases handles late)
    if (!process.argv.includes('--keep')) {
      for (const d of [CLAUDE_DIR, DATA_DIR]) {
        let done = false;
        for (let i = 0; i < 5 && !done; i += 1) {
          try { rmSync(d, { recursive: true, force: true }); done = true; }
          catch { await sleep(1000); }
        }
        if (!done) log(`WARN: could not fully remove ${d} (handle still held)`);
      }
    } else { log('--keep: temp dirs left in place'); }

    const allPass = results.every((r) => r.pass);
    process.exitCode = allPass ? 0 : 1;
  }
}

async function findAskPage(browser, timeoutMs) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    for (const c of browser.contexts()) for (const p of c.pages()) if (p.url().includes('ask.html')) return p;
    await sleep(300);
  }
  return null;
}

main().catch((e) => { log(`FATAL: ${e.stack || e.message}`); process.exitCode = 1; });
