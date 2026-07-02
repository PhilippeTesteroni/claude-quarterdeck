#!/usr/bin/env node
/**
 * Quarterdeck spool fixture injector (T8, SPEC §3.1/§3.5/R-4.3).
 *
 * Writes valid spool envelopes straight into a target data dir's `spool/`
 * directory, exactly the way `hooks/quarterdeck-hook.ps1`/`.sh` do (SPEC
 * R-4.3): `{v:1, event, receivedAt, payload, extra}`, atomically (tmp file in
 * the same directory, then rename) so the running app's `notify`-rs watcher
 * (`src-tauri/src/watcher.rs`) never sees a partial write. Used to drive the
 * real, built app (not the mocked UI) for the T8 real-app smoke test
 * (`e2e/real-app-smoke.mjs`) and for ad hoc manual testing
 * (`scripts/live-smoke.md`).
 *
 * Usage:
 *   node scripts/inject-events.mjs --data-dir <dir> <command> [options]
 *
 * Commands (one spool file each; every option maps 1:1 onto a hook payload
 * field from `docs/hooks-facts.md`):
 *
 *   session-start --session <id> [--cwd <path>] [--title <text>]
 *                 [--source startup|resume|clear|compact] [--pid <n>]
 *                 [--transcript <path>]
 *   prompt        --session <id> [--cwd <path>] --prompt <text>
 *                 [--transcript <path>]
 *   notification  --session <id> [--cwd <path>] --type <permission_prompt|
 *                 idle_prompt|elicitation_dialog|...> [--message <text>]
 *                 [--transcript <path>]
 *   stop          --session <id> [--cwd <path>] [--transcript <path>]
 *   session-end   --session <id> [--cwd <path>] [--reason <clear|resume|
 *                 logout|prompt_input_exit|bypass_permissions_disabled|other>]
 *
 *   raw --file <envelope.json>   Writes one pre-built envelope verbatim (must
 *                                already be `{v, event, payload, ...}` shaped).
 *   raw --json '<json>'          Same, inline.
 *
 *   --preset fleet   --project <name> --cwd <path> [--session-prefix <id>]
 *       Three sessions in one shot, covering `working` / `attention` /
 *       `idle` (a session each), for a quick "does the deck render a fleet"
 *       smoke check.
 *
 *   --preset lifecycle --session <id> --project <name> --cwd <path>
 *       [--transcript <path>]
 *       Emits SessionStart -> UserPromptSubmit (working) -> Notification
 *       permission_prompt (attention), in one call, and PRINTS the follow-up
 *       commands (recovery via transcript touch, stop, session-end) to run
 *       next — see `scripts/live-smoke.md` for the full choreography (R-2.2
 *       recovery needs the engine's next 10s tick to observe the touched
 *       transcript, so it can't usefully be scripted as a single instant
 *       call).
 *
 * Every command prints the spool file path it wrote, one per line, and exits
 * 0. Unknown flags / missing required fields exit 1 with a message on
 * stderr (this tool intentionally does NOT swallow errors the way the hook
 * scripts must — a fixture writer that silently no-ops is a bad test tool).
 */

import { randomBytes } from 'node:crypto';
import { mkdirSync, renameSync, writeFileSync, readFileSync, existsSync, utimesSync, closeSync, openSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import process from 'node:process';

const SELF = fileURLToPath(import.meta.url);

function usageError(msg) {
  process.stderr.write(`inject-events: ${msg}\n`);
  process.stderr.write(`Run 'node ${SELF} --help' for usage.\n`);
  process.exit(1);
}

function parseArgs(argv) {
  /** @type {Record<string, string>} */
  const flags = {};
  const positional = [];
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg.startsWith('--')) {
      const key = arg.slice(2);
      const next = argv[i + 1];
      if (next === undefined || next.startsWith('--')) {
        flags[key] = 'true';
      } else {
        flags[key] = next;
        i += 1;
      }
    } else {
      positional.push(arg);
    }
  }
  return { flags, positional };
}

// --- atomic spool write (mirrors the hook scripts' tmp+rename contract) ---

let seq = 0;

function spoolDir(dataDir) {
  return join(dataDir, 'spool');
}

/** Writes `envelope` to `<dataDir>/spool/<id>.json` atomically. Returns the
 * final path. */
function writeSpoolFile(dataDir, envelope) {
  const dir = spoolDir(dataDir);
  mkdirSync(dir, { recursive: true });
  seq += 1;
  const id = `${Date.now()}-${process.pid}-${randomBytes(4).toString('hex')}-${seq}`;
  const final = join(dir, `${id}.json`);
  const tmp = `${final}.tmp`;
  writeFileSync(tmp, JSON.stringify(envelope), 'utf8');
  renameSync(tmp, final);
  return final;
}

/** Builds the `{v, event, receivedAt, payload, extra}` envelope
 * (`crates/deck-core/src/events.rs::RawEnvelope`). `receivedAt` is epoch
 * millis (one of the several shapes `parse_envelope` tolerates). */
function envelope(event, payload, extra = {}) {
  return {
    v: 1,
    event,
    receivedAt: Date.now(),
    payload: { hook_event_name: event, ...payload },
    extra,
  };
}

function requireFlag(flags, name) {
  const value = flags[name];
  if (value === undefined) usageError(`--${name} is required for this command`);
  return value;
}

// --- per-event builders (fields per docs/hooks-facts.md) ------------------

function sessionStartEnvelope(flags) {
  const payload = {
    session_id: requireFlag(flags, 'session'),
    cwd: flags.cwd,
    transcript_path: flags.transcript,
    source: flags.source ?? 'startup',
    session_title: flags.title,
  };
  const extra = {};
  if (flags.pid !== undefined) extra.claudePid = Number(flags.pid);
  return envelope('SessionStart', payload, extra);
}

function promptEnvelope(flags) {
  return envelope('UserPromptSubmit', {
    session_id: requireFlag(flags, 'session'),
    cwd: flags.cwd,
    transcript_path: flags.transcript,
    prompt: requireFlag(flags, 'prompt'),
  });
}

function notificationEnvelope(flags) {
  return envelope('Notification', {
    session_id: requireFlag(flags, 'session'),
    cwd: flags.cwd,
    transcript_path: flags.transcript,
    notification_type: requireFlag(flags, 'type'),
    message: flags.message ?? '',
  });
}

function stopEnvelope(flags) {
  return envelope('Stop', {
    session_id: requireFlag(flags, 'session'),
    cwd: flags.cwd,
    transcript_path: flags.transcript,
  });
}

function sessionEndEnvelope(flags) {
  return envelope('SessionEnd', {
    session_id: requireFlag(flags, 'session'),
    cwd: flags.cwd,
    reason: flags.reason ?? 'other',
  });
}

// --- transcript touch helper (R-2.2 recovery needs real mtime advance) ----

/** Creates (if missing) and touches `path` so its mtime is "now" — the
 * signal `attention -> working` recovery (R-2.2) watches for. Used by the
 * `touch-transcript` command and the `lifecycle` preset's printed follow-up. */
function touchTranscript(path) {
  mkdirSync(dirname(path), { recursive: true });
  if (!existsSync(path)) {
    writeFileSync(path, '', 'utf8');
  } else {
    // Append a byte so size AND mtime both advance (belt and suspenders —
    // the engine only contracts to stat mtime, R-2.2).
    const fd = openSync(path, 'a');
    writeFileSync(fd, '\n');
    closeSync(fd);
  }
  const now = new Date();
  utimesSync(path, now, now);
}

// --- presets ----------------------------------------------------------------

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * `src-tauri/src/watcher.rs`'s `SpoolWatcher` coalesces every path that
 * arrives inside one 250ms debounce window into a single flush of a
 * `HashSet<PathBuf>` — iteration order for that flush is NOT the write
 * order. A multi-event story for the *same* session (e.g. SessionStart ->
 * UserPromptSubmit -> Stop) needs each event applied in order for its
 * intermediate transitions to be real (a `Stop` applied before its own
 * `SessionStart`/`UserPromptSubmit` would land on a session that's already
 * `idle` and never emit the R-9.1 toast). Spacing writes further apart than
 * the debounce window sidesteps this — also a closer approximation of how a
 * real Claude Code session actually paces hook events.
 */
const DEBOUNCE_SAFE_GAP_MS = 350;

async function presetFleet(flags) {
  const project = requireFlag(flags, 'project');
  const cwdBase = requireFlag(flags, 'cwd');
  const prefix = flags['session-prefix'] ?? 'fleet';
  const written = [];
  const write = async (env) => {
    written.push(writeSpoolFile(flags['data-dir'], env));
    await delay(DEBOUNCE_SAFE_GAP_MS);
  };

  await write(sessionStartEnvelope({
    session: `${prefix}-working`, cwd: `${cwdBase}/${project}-working`, title: `${project} (working)`,
  }));
  await write(promptEnvelope({
    session: `${prefix}-working`, cwd: `${cwdBase}/${project}-working`, prompt: 'Ship the feature',
  }));

  await write(sessionStartEnvelope({
    session: `${prefix}-attention`, cwd: `${cwdBase}/${project}-attention`, title: `${project} (attention)`,
  }));
  await write(notificationEnvelope({
    session: `${prefix}-attention`, cwd: `${cwdBase}/${project}-attention`,
    type: 'permission_prompt', message: 'Allow Bash to run `rm -rf build`?',
  }));

  await write(sessionStartEnvelope({
    session: `${prefix}-idle`, cwd: `${cwdBase}/${project}-idle`, title: `${project} (idle)`,
  }));
  // A fresh session already defaults to `idle` (SessionStart -> idle is a
  // no-op transition, engine.rs `Session::new`), so `Stop` alone wouldn't
  // actually change status and would NOT emit an Idle toast (R-9.1 fires
  // only on a real transition). Route through `working` first so the `Stop`
  // below is a genuine working -> idle transition.
  await write(promptEnvelope({
    session: `${prefix}-idle`, cwd: `${cwdBase}/${project}-idle`, prompt: 'Quick task',
  }));
  await write(stopEnvelope({
    session: `${prefix}-idle`, cwd: `${cwdBase}/${project}-idle`,
  }));

  return written;
}

async function presetLifecycle(flags) {
  const session = requireFlag(flags, 'session');
  const project = requireFlag(flags, 'project');
  const cwd = requireFlag(flags, 'cwd');
  const transcript = flags.transcript ?? join(flags['data-dir'], 'fixtures-transcripts', `${session}.jsonl`);

  const written = [];
  const write = async (env) => {
    written.push(writeSpoolFile(flags['data-dir'], env));
    await delay(DEBOUNCE_SAFE_GAP_MS);
  };

  await write(sessionStartEnvelope({
    session, cwd, transcript, title: `${project}: lifecycle smoke`,
  }));
  touchTranscript(transcript);
  await write(promptEnvelope({
    session, cwd, transcript, prompt: 'Run the lifecycle smoke test',
  }));
  await write(notificationEnvelope({
    session, cwd, transcript, type: 'permission_prompt', message: 'Allow Bash to run the smoke script?',
  }));

  process.stderr.write(
    [
      '',
      `lifecycle preset: session ${session} is now SessionStart -> working -> attention.`,
      'To continue the story by hand:',
      `  1. Recovery (R-2.2, needs the transcript mtime to advance >= 2s after the notification,`,
      `     then wait for the engine's next 10s tick):`,
      `       node ${SELF} --data-dir "${flags['data-dir']}" touch-transcript --transcript "${transcript}"`,
      `  2. Stop (-> idle):`,
      `       node ${SELF} --data-dir "${flags['data-dir']}" stop --session ${session} --cwd "${cwd}" --transcript "${transcript}"`,
      `  3. SessionEnd (-> row removed):`,
      `       node ${SELF} --data-dir "${flags['data-dir']}" session-end --session ${session} --cwd "${cwd}" --reason other`,
      '',
    ].join('\n'),
  );

  return written;
}

// --- CLI ---------------------------------------------------------------------

const HELP = `Quarterdeck spool fixture injector (T8)

Usage:
  node scripts/inject-events.mjs --data-dir <dir> <command> [options]

Commands: session-start | prompt | notification | stop | session-end |
          touch-transcript | raw | --preset fleet | --preset lifecycle

See the header comment in this file for the full option reference per
command, and scripts/live-smoke.md for worked examples.`;

async function main() {
  const { flags, positional } = parseArgs(process.argv.slice(2));

  if (flags.help === 'true' || positional[0] === 'help') {
    process.stdout.write(`${HELP}\n`);
    return;
  }

  const dataDir = requireFlag(flags, 'data-dir');
  flags['data-dir'] = dataDir;

  if (flags.preset) {
    const written =
      flags.preset === 'fleet' ? await presetFleet(flags) :
      flags.preset === 'lifecycle' ? await presetLifecycle(flags) :
      usageError(`unknown --preset '${flags.preset}' (expected fleet|lifecycle)`);
    for (const p of written) process.stdout.write(`${p}\n`);
    return;
  }

  const command = positional[0];
  if (!command) usageError('missing command (session-start|prompt|notification|stop|session-end|touch-transcript|raw)');

  if (command === 'touch-transcript') {
    const transcript = requireFlag(flags, 'transcript');
    touchTranscript(transcript);
    process.stdout.write(`${transcript}\n`);
    return;
  }

  if (command === 'raw') {
    let text;
    if (flags.file) text = readFileSync(flags.file, 'utf8');
    else if (flags.json) text = flags.json;
    else usageError('raw requires --file <path> or --json <inline JSON>');
    const parsed = JSON.parse(text);
    const path = writeSpoolFile(dataDir, parsed);
    process.stdout.write(`${path}\n`);
    return;
  }

  const builders = {
    'session-start': sessionStartEnvelope,
    prompt: promptEnvelope,
    notification: notificationEnvelope,
    stop: stopEnvelope,
    'session-end': sessionEndEnvelope,
  };
  const build = builders[command];
  if (!build) usageError(`unknown command '${command}'`);

  const path = writeSpoolFile(dataDir, build(flags));
  process.stdout.write(`${path}\n`);
}

main().catch((err) => {
  process.stderr.write(`inject-events: ${err.stack ?? err.message}\n`);
  process.exit(1);
});
