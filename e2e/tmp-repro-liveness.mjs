#!/usr/bin/env node
/**
 * Minimal isolated repro: does a session with claude_pid=None AND an
 * unresolvable/absent transcript_path get spuriously marked `dead` by the
 * ~10s liveness poll, even though its hook-derived status is `working`
 * (UserPromptSubmit was the last event, no Notification/Stop/dead PID)?
 *
 * Also includes a control session with claude_pid=None but a transcript
 * file that DOES exist and was just touched, to confirm the transcript
 * mtime path itself is fine and it's specifically the "no transcript info
 * at all" case that's the problem.
 */
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, existsSync, mkdirSync, writeFileSync, renameSync, utimesSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import net from 'node:net';
import process from 'node:process';

const repoRoot = 'C:/Users/phily/projects/quarterdeck';
function log(msg) { process.stdout.write(`[repro] ${msg}\n`); }
function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

async function waitFor(label, timeoutMs, checkFn) {
  const start = Date.now();
  let lastErr;
  while (Date.now() - start < timeoutMs) {
    try { const result = await checkFn(); if (result) return result; } catch (err) { lastErr = err; }
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
function killTree(pid) { try { execFileSync('taskkill', ['/PID', String(pid), '/T', '/F'], { stdio: 'ignore' }); } catch {} }

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

async function main() {
  const exe = join(repoRoot, 'target', 'release', 'quarterdeck.exe');
  const cdpPort = await pickFreeCdpPort(9900 + Math.floor(Math.random() * 90));
  const runDir = mkdtempSync(join(tmpdir(), 'quarterdeck-repro-liveness-'));
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

  const mcpJsonPath = join(dataDir, 'mcp.json');
  let child, childExited = false, browser;
  const { chromium } = await import('@playwright/test');

  outer: for (let outerAttempt = 1; outerAttempt <= 10; outerAttempt += 1) {
    for (let attempt = 1; attempt <= 40; attempt += 1) {
      log(`launching (outer ${outerAttempt}, attempt ${attempt})`);
      childExited = false;
      child = spawn(exe, [], { env, stdio: 'ignore', windowsHide: true });
      child.on('exit', () => { childExited = true; });
      try {
        await waitFor('mcp.json', 6000, () => existsSync(mcpJsonPath));
        break;
      } catch {
        if (!childExited) killTree(child.pid);
        await sleep(1500 + Math.random() * 2500);
      }
    }
    try {
      browser = await waitFor('CDP endpoint', 20000, async () => {
        if (!(await isPortInUse(cdpPort))) return null;
        try { return await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`); } catch { return null; }
      });
      break outer;
    } catch (err) {
      log(`outer ${outerAttempt} CDP failed: ${err.message}, re-rolling`);
      if (!childExited) killTree(child.pid);
      await sleep(1500);
    }
  }

  try {
    async function getPage(urlPart, timeoutMs = 20000) {
      return waitFor(`${urlPart} target`, timeoutMs, () => {
        for (const ctx of browser.contexts()) {
          for (const p of ctx.pages()) { if (p.url().includes(urlPart)) return p; }
        }
        return null;
      });
    }
    const popupPage = await getPage('popup.html');
    log('popup found');
    await popupPage.evaluate(async () => { await window.__TAURI__.core.invoke('set_setting', { key: 'onboardingDone', value: true }); });

    const cwdBase = join(runDir, 'projects');

    // A: no PID, no transcript_path at all -- the suspect case.
    writeSpoolFile(dataDir, envelope('SessionStart', { session_id: 'A-no-pid-no-transcript', cwd: join(cwdBase, 'A') }));
    writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: 'A-no-pid-no-transcript', cwd: join(cwdBase, 'A'), prompt: 'do the thing' }));

    // B: no PID, but a REAL transcript_path that exists and was just touched.
    const transcriptB = join(dataDir, 'fixtures-transcripts', 'B.jsonl');
    mkdirSync(join(dataDir, 'fixtures-transcripts'), { recursive: true });
    writeFileSync(transcriptB, '{}\n', 'utf8');
    const now = new Date();
    utimesSync(transcriptB, now, now);
    writeSpoolFile(dataDir, envelope('SessionStart', { session_id: 'B-no-pid-fresh-transcript', cwd: join(cwdBase, 'B'), transcript_path: transcriptB }));
    writeSpoolFile(dataDir, envelope('UserPromptSubmit', { session_id: 'B-no-pid-fresh-transcript', cwd: join(cwdBase, 'B'), transcript_path: transcriptB, prompt: 'do the thing' }));

    // C: control -- a genuinely-dead PID (999999), should legitimately go dead.
    writeSpoolFile(dataDir, envelope('SessionStart', { session_id: 'C-bogus-pid', cwd: join(cwdBase, 'C') }, { claudePid: 999999 }));

    await sleep(1500);
    const stateBefore = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    log(`statuses right after injection: ${JSON.stringify(stateBefore.sessions.map((s) => ({ id: s.id, status: s.status })))}`);

    // Wait past TWO liveness ticks (R-6.1 polls every 10s) to be sure.
    log('waiting 25s (past 2 liveness poll ticks)...');
    await sleep(25000);

    const stateAfter = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    const byId = Object.fromEntries(stateAfter.sessions.map((s) => [s.id, s.status]));
    log(`statuses after 25s: ${JSON.stringify(byId)}`);

    const results = {
      A_expected_working_got: byId['A-no-pid-no-transcript'],
      B_expected_working_got: byId['B-no-pid-fresh-transcript'],
      C_expected_dead_got: byId['C-bogus-pid'],
    };
    log(`RESULT: ${JSON.stringify(results, null, 2)}`);

    if (results.A_expected_working_got === 'dead') {
      log('CONFIRMED BUG: session with claude_pid=None AND no transcript_path was marked dead by the liveness poll despite being actively working (last event UserPromptSubmit, no Stop/Notification/dead-PID).');
    } else {
      log('NOT reproduced for case A (session stayed as expected) -- re-examine.');
    }
    if (results.B_expected_working_got === 'dead') {
      log('UNEXPECTED: even WITH a fresh, existing transcript file, the PID-less session went dead -- broader bug than hypothesized.');
    } else {
      log('Case B (PID-less but with a real fresh transcript) correctly stayed alive -- confirms the bug is specifically about missing/unresolvable transcript_path, not "PID-less" in general.');
    }
    if (results.C_expected_dead_got !== 'dead') {
      log('WARNING: control case C (bogus PID) did NOT go dead as expected -- liveness poll may not be running at all in this run.');
    } else {
      log('Control case C (bogus PID) correctly went dead -- confirms the liveness poll IS running/active during this test.');
    }

    process.stdout.write(`REPRO_RESULT=${JSON.stringify(results)}\n`);
  } finally {
    if (browser) await browser.close().catch(() => {});
    if (!childExited) killTree(child.pid);
  }
}

main().catch((err) => {
  log(`FATAL: ${err.stack || err.message}`);
  process.exitCode = 1;
});
