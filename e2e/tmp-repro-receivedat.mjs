#!/usr/bin/env node
/**
 * Repro for: "24h spool freshness cut (R-3.5) is silently bypassed when a
 * spool event omits `receivedAt`".
 *
 * Writes a spool file missing `receivedAt` entirely (should be treated per
 * the actual on-disk mtime, which we'll also backdate) alongside a sibling
 * with an explicit stale receivedAt (25h old) to confirm that one IS
 * discarded while the no-receivedAt one is NOT.
 */
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, existsSync, mkdirSync, writeFileSync, renameSync, utimesSync } from 'node:fs';
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

function writeSpoolFile(dataDir, envelopeObj, mtimeMs) {
  const dir = join(dataDir, 'spool');
  mkdirSync(dir, { recursive: true });
  const id = `${Date.now()}-${process.pid}-${Math.random().toString(16).slice(2)}`;
  const final = join(dir, `${id}.json`);
  const tmp = `${final}.tmp`;
  writeFileSync(tmp, JSON.stringify(envelopeObj), 'utf8');
  renameSync(tmp, final);
  if (mtimeMs !== undefined) {
    const t = mtimeMs / 1000;
    utimesSync(final, t, t);
  }
  return final;
}

async function main() {
  const exe = join(repoRoot, 'target', 'release', 'quarterdeck.exe');
  const cdpPort = await pickFreeCdpPort(9700 + Math.floor(Math.random() * 100));
  log(`CDP port ${cdpPort}`);

  const runDir = mkdtempSync(join(tmpdir(), 'quarterdeck-repro-receivedat-'));
  const dataDir = join(runDir, 'data');
  const claudeDir = join(runDir, 'claude');
  mkdirSync(dataDir, { recursive: true });
  mkdirSync(join(claudeDir, 'projects'), { recursive: true });
  log(`data dir: ${dataDir}`);

  const now = Date.now();
  const old25h = now - 25 * 60 * 60 * 1000;

  // File A: has receivedAt, 25h old -> per code SHOULD be discarded (TooOld).
  writeSpoolFile(dataDir, {
    v: 1, event: 'SessionStart', receivedAt: old25h,
    payload: { hook_event_name: 'SessionStart', session_id: 'old-with-timestamp', cwd: 'C:/x/a', session_title: 'a', source: 'startup' },
    extra: {},
  });

  // File B: NO receivedAt field at all, but the file's own mtime is backdated
  // 25h to simulate a genuinely old/stale file dropped into the spool dir.
  writeSpoolFile(dataDir, {
    v: 1, event: 'SessionStart',
    payload: { hook_event_name: 'SessionStart', session_id: 'old-no-timestamp', cwd: 'C:/x/b', session_title: 'b', source: 'startup' },
    extra: {},
  }, old25h);

  // File C: control - fresh event, no receivedAt, fresh mtime -> should apply (this is fine/expected).
  writeSpoolFile(dataDir, {
    v: 1, event: 'SessionStart',
    payload: { hook_event_name: 'SessionStart', session_id: 'fresh-no-timestamp', cwd: 'C:/x/c', session_title: 'c', source: 'startup' },
    extra: {},
  });

  const env = {
    ...process.env,
    QUARTERDECK_DATA_DIR: dataDir,
    QUARTERDECK_CLAUDE_DIR: claudeDir,
    QUARTERDECK_FAKE_NOTIFIER: '1',
    QUARTERDECK_DEBUG: '1',
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
  };

  const mcpJsonPath = join(dataDir, 'mcp.json');
  let child;
  let childExited = false;
  const maxAttempts = 20;
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
      if (attempt === maxAttempts) throw new Error(`app never started after ${maxAttempts} attempts`);
      if (!childExited) killTree(child.pid);
      await sleep(2000 + Math.random() * 2000);
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

    await sleep(2000);
    const state = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    log(`sessions: ${JSON.stringify(state.sessions.map((s) => s.sessionId || s.session_id))}`);
    log(`counts: ${JSON.stringify(state.counts)}`);

    const ids = state.sessions.map((s) => s.sessionId || s.session_id);
    const hasOldWithTs = ids.includes('old-with-timestamp');
    const hasOldNoTs = ids.includes('old-no-timestamp');
    const hasFreshNoTs = ids.includes('fresh-no-timestamp');

    log(`old-with-timestamp present: ${hasOldWithTs} (expect false: discarded as too old)`);
    log(`old-no-timestamp present: ${hasOldNoTs} (finding claims: true = BUG, freshness cut bypassed)`);
    log(`fresh-no-timestamp present: ${hasFreshNoTs} (expect true: legitimately fresh)`);

    // Check the spool dir - was the old-no-timestamp file deleted (consumed = applied) or does it remain (discarded)?
    const fs2 = await import('node:fs');
    const spoolFiles = fs2.readdirSync(join(dataDir, 'spool'));
    log(`remaining spool files: ${JSON.stringify(spoolFiles)}`);

  } finally {
    try { if (browser) await browser.close(); } catch {}
    // Only kill OUR OWN spawned child tree — never sweep all quarterdeck.exe/
    // msedgewebview2.exe processes, since a concurrent QA session on this
    // shared machine may legitimately have its own instances running.
    if (child && !childExited) killTree(child.pid);
    try { rmSync(runDir, { recursive: true, force: true }); } catch {}
  }
}

main().catch((err) => {
  console.error('[repro] FAILED:', err);
  process.exitCode = 1;
});
