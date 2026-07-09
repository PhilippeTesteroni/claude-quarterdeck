/**
 * Env-activated fake Tauri backend.
 *
 * `ipc-client.ts` picks this module instead of the real `@tauri-apps/api`
 * whenever `isTauri()` is false — i.e. whenever the UI is running in a plain
 * browser against the Vite dev server, with no Tauri host present. That makes
 * every window (popup + ask) fully explorable and screenshot-able without
 * building/running the Rust shell.
 *
 * Scenario selection: `?scenario=<name>` in the page URL (see `SCENARIOS`
 * below for the list). Defaults to `default` for popup.html and `ask` for
 * ask.html. `?story=off` freezes the background timeline (used by the
 * screenshot script for deterministic frames).
 */

import { isTauri } from '@tauri-apps/api/core';

import type {
  AskAnswerKind,
  AskQuestion,
  AskRow,
  Commands,
  PermRow,
  SessionRow,
  SessionStatus,
  SettingsState,
  StateSnapshot,
} from './ipc-contract';

interface InternalSession {
  id: string;
  project: string;
  title: string;
  /** R-27.1: the non-override title (registry/session/prompt-derived). An empty
   * rename restores this; a non-empty rename overrides `title`. Defaults to the
   * initial `title` the first time a rename touches the row. */
  baseTitle?: string;
  branch?: string;
  status: SessionStatus;
  /** Status held before a pending ask forced `attention` (R-2.4 recompute-on-clear). */
  preAskStatus?: SessionStatus;
  inferred: boolean;
  cwd: string;
  statusChangedAt: number;
  /** R-21.2: active background subagents → `⛭ N` badge. */
  subagents?: number;
  /** R-22.3: total session age in ms → tooltip "session 2h 14m". */
  ageMs?: number;
  /** R-23.4: context fill percent → row second line `ctx {n}%`. */
  ctxPercent?: number;
  /** R-23.4: compact session spend → row second line `· {spend}`. */
  spend?: string;
  /** R-23.1: spend is a lower bound → rendered with a "≥" prefix. */
  spendApprox?: boolean;
  /** R-23.3: compact subagent group spend → `⛭ N · {spend}` badge. */
  subagentSpend?: string;
}

interface InternalAsk {
  id: string;
  sessionId?: string;
  project?: string;
  context?: string;
  question: string;
  options?: string[];
  /** SPEC §29 (R-29.5): multi-question / multi-select form blocks. */
  questions?: AskQuestion[];
  /** R-19.1: long-form rationale rendered muted under the question. */
  detail?: string;
  timeoutAt?: number;
  createdAt: number;
  /** R-8.7: recovered-after-restart ask — renders as expired, Dismiss-only. */
  orphaned?: boolean;
}

interface InternalPerm {
  id: string;
  sessionId?: string;
  project?: string;
  context?: string;
  toolName: string;
  toolInput: string;
  /** Arrival time for the shared ask/perm FIFO (R-16.2). */
  createdAt?: number;
  /** SPEC R-32.1: epoch ms the perm expires — past it the UI disables Allow/Deny. */
  expiresAt?: number;
}

const params = new URLSearchParams(location.search);
const storyEnabled = params.get('story') !== 'off';

let sessions: InternalSession[] = [];
let asks: InternalAsk[] = [];
let perms: InternalPerm[] = [];
let hooksInstalled = true;
let installShouldFail = false;
/** When set (via `?scenario=focus-fail`), the next `focus_terminal` call rejects
 * so a spec can exercise the inline "Couldn't find the terminal window" notice
 * (SPEC R-15.4b). Declared here (not with the other window-op counters below) so
 * it exists before `loadScenario` runs at module load. */
let focusTerminalShouldFail = false;
let settings: SettingsState = defaultSettings();
let listeners: Array<(s: StateSnapshot) => void> = [];
let askCounter = 0;

function defaultSettings(): SettingsState {
  return {
    notifyIdle: true,
    notifyAttention: true,
    notifyReminder: false,
    launchAtLogin: false,
    onboardingDone: true,
    popupPinned: false,
    takeoverPermissions: true,
    showTokenStats: true,
    popupMode: 'list',
    mcpEnabled: true,
    mcpCliAvailable: true,
    mcpCommand:
      'claude mcp add --transport http --scope user quarterdeck http://127.0.0.1:53017/mcp --header "Authorization: Bearer mock-token"',
    dataDir: 'C:/Users/phily/AppData/Roaming/quarterdeck',
    version: '0.1.0',
  };
}

function minutesAgo(m: number): number {
  return Date.now() - m * 60_000;
}
function secondsAgo(s: number): number {
  return Date.now() - s * 1000;
}

function session(partial: Omit<InternalSession, 'statusChangedAt'> & { since: number }): InternalSession {
  const { since, ...rest } = partial;
  return { ...rest, statusChangedAt: since };
}

/** Named fixture states. See module doc for how to select one. */
const SCENARIOS: Record<string, () => { sessions: InternalSession[]; asks: InternalAsk[]; perms?: InternalPerm[]; hooksInstalled: boolean; settings: SettingsState }> = {
  default: () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Implement watch line component',
        branch: 'feature/watchline',
        status: 'attention',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(47),
      }),
      session({
        id: 's2',
        project: 'dream-book-web',
        title: 'Fix locale-native generator cron',
        branch: 'main',
        status: 'working',
        inferred: false,
        cwd: 'C:/Users/phily/projects/dream-book-web',
        since: secondsAgo(12),
      }),
      session({
        id: 's3',
        project: 'dating-coach',
        title: 'P4 red/green flag redesign',
        branch: 'p4-redesign',
        status: 'working',
        inferred: false,
        cwd: 'C:/Users/phily/projects/dating-coach',
        since: minutesAgo(4) - 20_000,
      }),
      session({
        id: 's4',
        project: 'shitty-apps-back',
        title: 'Config-service S3 whitelist',
        status: 'idle',
        inferred: false,
        cwd: 'C:/Users/phily/projects/shitty_apps_back',
        since: minutesAgo(4) - 7_000,
      }),
      session({
        id: 's5',
        project: 'legacy-tool',
        title: '(no title)',
        status: 'dead',
        inferred: true,
        cwd: 'C:/Users/phily/projects/legacy-tool',
        since: minutesAgo(20),
      }),
    ],
    asks: [
      {
        id: 'a1',
        sessionId: 's1',
        project: 'quarterdeck',
        question: 'Which approach for the watch line segments: CSS grid columns or flex-basis percentages?',
        options: ['CSS grid columns', 'Flex-basis percentages', 'Either, pick for me'],
        timeoutAt: Date.now() + 90_000,
        createdAt: Date.now() - 4_000,
      },
      {
        id: 'a2',
        sessionId: 's1',
        project: 'quarterdeck',
        question: 'Should the empty state link straight to the docs, or just name the command?',
        createdAt: Date.now() - 1_000,
      },
    ],
  }),
  empty: () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [],
    asks: [],
  }),
  // SPEC R-14.3 "true auto-height ... regression-tested: 50 rows → 0": a
  // fleet large enough to push the popup content well past the 560 cap, so a
  // Playwright spec can drive `remove_row` down to zero and assert the
  // content-height report the UI sends via `resize_popup` shrinks back down
  // instead of sticking at the grown size.
  'many-sessions': () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: Array.from({ length: 50 }, (_, i) =>
      session({
        id: `m${i}`,
        project: `project-${i}`,
        title: `Session number ${i}`,
        status: 'idle',
        inferred: false,
        cwd: `C:/Users/phily/projects/project-${i}`,
        since: secondsAgo(i),
      }),
    ),
    asks: [],
  }),
  // SPEC §21 (R-21.1/R-21.2/R-21.4): a session whose Stop hook fired but whose
  // registry says busy displays yellow (working) with a `⛭ N` subagent badge,
  // while an estimated row shows the `~` time + age tooltip (§22).
  'background-busy': () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Fan-out refactor across 6 crates',
        branch: 'main',
        // Displayed working via the busy-override even though the turn's Stop
        // fired — the shell computes this; the mock just reflects the result.
        status: 'working',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(38),
        subagents: 3,
        ageMs: 2 * 3_600_000 + 14 * 60_000, // "session 2h 14m"
      }),
      session({
        id: 's2',
        project: 'dream-book-web',
        title: 'Locale-native generator sweep',
        status: 'idle',
        // Pre-existing at launch → estimated time (`~`) + a discovery age.
        inferred: true,
        cwd: 'C:/Users/phily/projects/dream-book-web',
        since: minutesAgo(12) - 40_000, // renders as ~12m 40s
        ageMs: 47 * 60_000,
      }),
    ],
    asks: [],
  }),
  // SPEC §23 (R-23.4): per-session token telemetry on rows — the `ctx {n}% ·
  // {spend}` second line with amber/red context-health coloring, a "≥"
  // lower-bound spend, and the `⛭ N · {spend}` subagent-group badge.
  'token-stats': () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Fan-out refactor across crates',
        status: 'working',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(38),
        subagents: 3,
        ctxPercent: 62,
        spend: '1.4M',
        subagentSpend: '2.1M',
      }),
      session({
        id: 's2',
        project: 'dream-book-web',
        title: 'Near-full context window',
        status: 'idle',
        inferred: false,
        cwd: 'C:/Users/phily/projects/dream-book-web',
        since: secondsAgo(9),
        ctxPercent: 93,
        spend: '812k',
      }),
      session({
        id: 's3',
        project: 'dating-coach',
        title: 'Amber-band context',
        status: 'working',
        inferred: false,
        cwd: 'C:/Users/phily/projects/dating-coach',
        since: secondsAgo(4),
        ctxPercent: 80,
        spend: '120k',
        spendApprox: true,
      }),
    ],
    asks: [],
  }),
  // R-23.6: same rows, `showTokenStats` off — no usage line renders anywhere.
  'token-stats-off': () => ({
    hooksInstalled: true,
    settings: { ...defaultSettings(), showTokenStats: false },
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Fan-out refactor across crates',
        status: 'working',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(38),
        subagents: 3,
        ctxPercent: 62,
        spend: '1.4M',
        subagentSpend: '2.1M',
      }),
    ],
    asks: [],
  }),
  // SPEC §25 (R-25.1/R-25.2): the pinned popup collapsed into the compact
  // traffic-light lamp. One attention session drives a red lamp + "1" badge
  // (the worst-of aggregate, mirroring `TrayStatus::worst_of` in `tray.rs`).
  lamp: () => ({
    hooksInstalled: true,
    settings: { ...defaultSettings(), popupPinned: true, popupMode: 'lamp' },
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Fan-out refactor across crates',
        status: 'attention',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(20),
      }),
      session({
        id: 's2',
        project: 'dream-book-web',
        title: 'Locale-native generator sweep',
        status: 'working',
        inferred: false,
        cwd: 'C:/Users/phily/projects/dream-book-web',
        since: secondsAgo(5),
      }),
    ],
    // R-25.3: a pending ask alongside the collapsed lamp proves nothing
    // auto-expands it on arrival (the ask window handles the urgency instead).
    asks: [
      {
        id: 'a1',
        sessionId: 's1',
        project: 'quarterdeck',
        question: 'Ship the migration in this PR, or split it out?',
        options: ['This PR', 'Split it out'],
        createdAt: Date.now() - 3_000,
      },
    ],
  }),
  nohooks: () => ({
    hooksInstalled: false,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Draft README hero section',
        status: 'working',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(30),
      }),
      session({
        id: 's2',
        project: 'dream-book-web',
        title: 'Regional pricing follow-up',
        status: 'idle',
        inferred: true,
        cwd: 'C:/Users/phily/projects/dream-book-web',
        since: minutesAgo(2),
      }),
    ],
    asks: [],
  }),
  onboarding: () => ({
    hooksInstalled: false,
    settings: { ...defaultSettings(), onboardingDone: false, mcpEnabled: false, launchAtLogin: false },
    sessions: [],
    asks: [],
  }),
  // Onboarding still incomplete for THIS data dir (e.g. a reinstall / new data
  // dir) but hooks already work and sessions flow. The onboarding card must not
  // stack above the live list (R-10.2); the list wins.
  'onboarding-with-sessions': () => ({
    hooksInstalled: true,
    settings: { ...defaultSettings(), onboardingDone: false },
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Wire the composition root',
        status: 'working',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(12),
      }),
    ],
    asks: [],
  }),
  cyrillic: () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'сон-книга',
        title: 'Исправить генератор снов — юникод и кириллица ✓',
        branch: 'фикс/юникод',
        status: 'attention',
        inferred: false,
        cwd: 'C:/Users/phily/projects/сон-книга 📖',
        since: secondsAgo(80),
      }),
      session({
        id: 's2',
        project: '知识库',
        title: '修复本地化生成器',
        status: 'working',
        inferred: false,
        cwd: 'C:/Users/phily/projects/知识库',
        since: secondsAgo(15),
      }),
    ],
    asks: [
      {
        id: 'a1',
        sessionId: 's1',
        project: 'сон-книга',
        question: 'Использовать московский часовой пояс для крон-джобы?',
        options: ['Да', 'Нет, UTC'],
        timeoutAt: Date.now() + 45_000,
        createdAt: Date.now() - 2_000,
      },
    ],
  }),
  'ask-unknown': () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Implement watch line component',
        status: 'working',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(20),
      }),
    ],
    asks: [
      {
        id: 'a1',
        context: 'C:/Users/phily/projects/some-untracked-script',
        question: 'This script is about to overwrite output.csv — proceed?',
        options: ['Overwrite', 'Cancel'],
        timeoutAt: Date.now() + 30_000,
        createdAt: Date.now() - 1_000,
      },
    ],
  }),
  'ask-orphaned': () => ({
    // R-8.7: an ask recovered from disk after a restart. Its MCP connection is
    // gone, so it can never be answered — it renders as expired with only a
    // Dismiss action (no options, no free-text field, no live countdown).
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Long autonomous refactor',
        status: 'attention',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(40),
      }),
    ],
    asks: [
      {
        id: 'a1',
        sessionId: 's1',
        project: 'quarterdeck',
        question: 'Migrate the settings schema now, or defer to the next release?',
        options: ['Migrate now', 'Defer'],
        // No timeoutAt: the recovered ask is already expired, not counting down.
        createdAt: Date.now() - 120_000,
        orphaned: true,
      },
    ],
  }),
  // SPEC R-19.1/R-19.2: an ask carrying long-form `detail` and NO timeout
  // (persistent — renders with no countdown).
  'ask-detail': () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Long autonomous refactor',
        status: 'attention',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(30),
      }),
    ],
    asks: [
      {
        id: 'a1',
        sessionId: 's1',
        project: 'quarterdeck',
        question: 'Drop the legacy v1 answer file format?',
        detail:
          'The v1 format has been dual-written since 1.0. No sessions have read it in the last 30 days. Removing it simplifies the answers watcher, but a downgrade to 1.0 would then fail to read new answer files.',
        options: ['Drop it', 'Keep dual-write'],
        // No timeoutAt: persistent (R-19.2) — the window shows no countdown.
        createdAt: Date.now() - 5_000,
      },
    ],
  }),
  // SPEC §29 (R-29.4): a multi-question / multi-select ask. The window renders a
  // form (radio + checkbox blocks + Submit); the popup mirror shows "N questions
  // — Answer in window". A second single-question ask queues behind it.
  'ask-form': () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Release cut',
        status: 'attention',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(20),
      }),
    ],
    asks: [
      {
        id: 'a1',
        sessionId: 's1',
        project: 'quarterdeck',
        // Synthesized headline = the first question (as the shell produces it).
        question: 'Which environment?',
        questions: [
          { header: 'Environment', question: 'Which environment?', options: ['prod', 'staging'] },
          {
            header: 'Flags',
            question: 'Extra flags?',
            multiSelect: true,
            options: ['--fast', '--safe', '--verbose'],
          },
        ],
        timeoutAt: Date.now() + 120_000,
        createdAt: Date.now() - 3_000,
      },
      {
        id: 'a2',
        sessionId: 's1',
        project: 'quarterdeck',
        question: 'Tag the release after merge?',
        options: ['Yes', 'No'],
        createdAt: Date.now() - 1_000,
      },
    ],
  }),
  // SPEC §16 (R-16.2): a pending permission request rendered in the ask window
  // (amber) and mirrored in the popup, with an ask queued behind it. The perm
  // arrived BEFORE the ask (older `createdAt`), so under the shared ask/perm
  // FIFO it holds the primary slot.
  perm: () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Long autonomous refactor',
        status: 'attention',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(6),
      }),
    ],
    asks: [
      {
        id: 'a1',
        sessionId: 's1',
        project: 'quarterdeck',
        question: 'Ship the migration in this PR, or split it out?',
        options: ['This PR', 'Split it out'],
        createdAt: Date.now() - 3_000,
      },
    ],
    perms: [
      {
        id: 'p1',
        sessionId: 's1',
        project: 'quarterdeck',
        toolName: 'Bash',
        toolInput: '{"command":"rm -rf ./dist && npm run build","timeout":120000}',
        createdAt: Date.now() - 5_000,
      },
    ],
  }),
  // SPEC §16 (R-16.2) FIFO: an ask that arrived BEFORE a later perm keeps the
  // primary slot — perms do NOT preempt an already-queued ask. The perm queues
  // behind it ("1 more waiting") and still renders as an answerable mirror row.
  'perm-after-ask': () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Long autonomous refactor',
        status: 'attention',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(6),
      }),
    ],
    asks: [
      {
        id: 'a1',
        sessionId: 's1',
        project: 'quarterdeck',
        question: 'Ship the migration in this PR, or split it out?',
        options: ['This PR', 'Split it out'],
        createdAt: Date.now() - 8_000,
      },
    ],
    perms: [
      {
        id: 'p1',
        sessionId: 's1',
        project: 'quarterdeck',
        toolName: 'Bash',
        toolInput: '{"command":"rm -rf ./dist && npm run build","timeout":120000}',
        createdAt: Date.now() - 2_000,
      },
    ],
  }),
  // SPEC R-32.1: a perm whose deadline has already passed renders with the
  // "expired" tag and disabled Allow/Deny (the hook has given up); the shell's
  // tick sweep will shortly remove it. "In terminal" stays live.
  'perm-expired': () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [
      session({
        id: 's1',
        project: 'quarterdeck',
        title: 'Long autonomous refactor',
        status: 'attention',
        inferred: false,
        cwd: 'C:/Users/phily/projects/quarterdeck',
        since: secondsAgo(95),
      }),
    ],
    asks: [],
    perms: [
      {
        id: 'p1',
        sessionId: 's1',
        project: 'quarterdeck',
        toolName: 'Bash',
        toolInput: '{"command":"rm -rf ./dist && npm run build","timeout":120000}',
        createdAt: Date.now() - 95_000,
        // Deadline already in the past → Allow/Deny disabled.
        expiresAt: Date.now() - 5_000,
      },
    ],
  }),
  // R-16.2 / R-8.2: an unmatched perm shows "Unknown agent (<context>)".
  'perm-unknown': () => ({
    hooksInstalled: true,
    settings: defaultSettings(),
    sessions: [],
    asks: [],
    perms: [
      {
        id: 'p1',
        context: 'C:/Users/phily/projects/some-untracked-script',
        toolName: 'Write',
        toolInput: '{"file_path":"/etc/hosts","content":"127.0.0.1 example.com"}',
      },
    ],
  }),
  error: () => ({
    hooksInstalled: false,
    settings: defaultSettings(),
    sessions: [],
    asks: [],
  }),
};

function loadScenario(name: string): void {
  const build = SCENARIOS[name] ?? SCENARIOS.default;
  const fixture = build();
  sessions = fixture.sessions;
  asks = fixture.asks;
  perms = fixture.perms ?? [];
  hooksInstalled = fixture.hooksInstalled;
  settings = fixture.settings;
  installShouldFail = name === 'error';
  // SPEC R-15.4b: `?scenario=focus-fail` makes focus_terminal reject so a spec
  // can assert the inline notice appears (the fixture itself is the default).
  focusTerminalShouldFail = name === 'focus-fail';
  askCounter = asks.length;
}

loadScenario(params.get('scenario') ?? 'default');

// R-7.3 status priority; mirrors the Rust engine so the mock — standing in for
// the backend — emits snapshots already in the engine's canonical order. The
// dumb frontend (R-3.4) renders this order verbatim and never re-sorts.
const STATUS_PRIORITY: Record<SessionStatus, number> = {
  attention: 0,
  working: 1,
  idle: 2,
  dead: 3,
};

function snapshot(): StateSnapshot {
  const now = Date.now();
  // Sort exactly as `SessionStore::view` does: by status priority, then
  // most-recently-active first (the mock has only `statusChangedAt`, its analog
  // of the engine's `last_activity_ms`), then a stable id tiebreak.
  const ordered = [...sessions].sort((a, b) => {
    const byStatus = STATUS_PRIORITY[a.status] - STATUS_PRIORITY[b.status];
    if (byStatus !== 0) return byStatus;
    const byActivity = b.statusChangedAt - a.statusChangedAt;
    if (byActivity !== 0) return byActivity;
    return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
  });
  const rows: SessionRow[] = ordered.map((s) => ({
    id: s.id,
    project: s.project,
    title: s.title,
    branch: s.branch,
    status: s.status,
    inferred: s.inferred,
    sinceMs: Math.max(0, now - s.statusChangedAt),
    cwd: s.cwd,
    subagents: s.subagents ?? 0,
    ageMs: s.ageMs,
    ctxPercent: s.ctxPercent,
    spend: s.spend,
    spendApprox: s.spendApprox,
    subagentSpend: s.subagentSpend,
  }));
  const askRows: AskRow[] = asks.map((a) => ({
    id: a.id,
    sessionId: a.sessionId,
    project: a.project,
    question: a.question,
    options: a.options,
    questions: a.questions,
    detail: a.detail,
    timeoutAt: a.timeoutAt,
    context: a.context,
    orphaned: a.orphaned,
    // R-16.2: arrival time for the shared ask/perm FIFO.
    queuedAt: a.createdAt,
  }));
  const permRows: PermRow[] = perms.map((p) => ({
    id: p.id,
    sessionId: p.sessionId,
    project: p.project,
    toolName: p.toolName,
    toolInput: p.toolInput,
    context: p.context,
    // R-16.2: a perm with no explicit arrival is treated as just-now (newest).
    queuedAt: p.createdAt ?? Date.now(),
    // R-32.1: mirror the shell's deadline so the UI can disable Allow/Deny.
    expiresAt: p.expiresAt,
  }));
  const counts = {
    attention: sessions.filter((s) => s.status === 'attention').length,
    working: sessions.filter((s) => s.status === 'working').length,
    idle: sessions.filter((s) => s.status === 'idle').length,
    dead: sessions.filter((s) => s.status === 'dead').length,
  };
  return { sessions: rows, asks: askRows, perms: permRows, hooksInstalled, counts, settings: { ...settings } };
}

function emit(): void {
  const snap = snapshot();
  for (const l of listeners) l(snap);
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// --- Test-observable window-op counters (no real window in mock/browser
// mode, R-14/R-18) ----------------------------------------------------------

/** Last `contentHeight` reported via `resize_popup` (SPEC R-14.3 auto-height:
 * lets a Playwright spec assert the reported height shrinks back down once
 * rows disappear, without a real OS window to measure). */
let lastResizeContentHeight: number | null = null;
/** Count of `resize_popup` calls (SPEC R-31.2): lets a spec prove the settings
 * open/close resize snaps in a single report under reduced motion vs. tweens
 * across many frames when motion is allowed. */
let resizePopupCalls = 0;
/** Count of `show_ask_window` invocations (SPEC R-18.1 "(or via popup mirror
 * click)"): the popup's ask-mirror row click calls this to re-surface the ask
 * window; there's no second real window in mock mode, so a call counter is
 * the observable. */
let showAskWindowCalls = 0;
/** Records `focus_terminal` calls (SPEC R-15.4): the row click / context-menu
 * "Focus terminal" invokes it; there's no real terminal in mock/browser mode,
 * so a spec asserts against this counter (and the last session id). */
let focusTerminalCalls = 0;
let lastFocusTerminalId: string | null = null;
/** Count of `hideCurrentWindow()` calls (SPEC R-18.1 close-X/Esc): the ask
 * window's own X button / Esc key hide the window directly via the Tauri
 * window API (see `ipc-client.ts`), bypassing `invoke` entirely — tracked
 * here instead so a spec can assert the close action fired without
 * dismissing the pending ask. */
let hideCurrentWindowCalls = 0;
/** Count of `startDragging()` calls (SPEC R-25.1): the lamp's manual
 * pointer-drag discrimination (see `ipc-client.ts`) calls this once movement
 * crosses the threshold; a plain click never should. No real OS window to
 * actually drag in mock/browser mode, so a spec asserts against this counter. */
let startDraggingCalls = 0;
/** Last decision sent via `answer_perm` (SPEC §16): lets a Playwright spec assert
 * A/D/Esc + the Allow/Deny/In-terminal buttons route the right decision, with no
 * real hook to observe. */
let lastPermDecision: string | null = null;
/** Last `answer_ask` (askId, kind) routed through the mock (SPEC R-19.4): lets a
 * spec assert Dismiss sends kind:"dismissed" from both the ask window and the
 * popup mirror, with no real MCP call to observe. */
let lastAnswerAsk: { askId: string; kind: string } | null = null;
/** Last `answer_ask` INCLUDING the answer payload (SPEC §29): the form spec
 * asserts the submitted `{answers:[...]}` document + kind:"form". Kept separate
 * from `lastAnswerAsk` so the existing R-19.4 specs' `{askId, kind}` shape is
 * unchanged. */
let lastAnswerAskFull: { askId: string; answer: string; kind: string } | null = null;

/** Test-only: records a `hideCurrentWindow()` call (SPEC R-18.1). Exported so
 * `ipc-client.ts` can call it from the mock branch of `hideCurrentWindow`. */
export function hideCurrentWindowMock(): void {
  hideCurrentWindowCalls += 1;
}

/** Test-only: records a `startDragging()` call (SPEC R-25.1). Exported so
 * `ipc-client.ts` can call it from the mock branch of `startDragging`. */
export function startDraggingMock(): void {
  startDraggingCalls += 1;
}

function clearAskAndMaybeRestore(askId: string): void {
  const ask = asks.find((a) => a.id === askId);
  asks = asks.filter((a) => a.id !== askId);
  if (!ask?.sessionId) return;
  const stillPending = asks.some((a) => a.sessionId === ask.sessionId);
  if (stillPending) return;
  const s = sessions.find((sess) => sess.id === ask.sessionId);
  if (s && s.status === 'attention') {
    s.status = s.preAskStatus ?? 'idle';
    s.statusChangedAt = Date.now();
    s.preAskStatus = undefined;
  }
}

export async function invoke<K extends keyof Commands>(
  cmd: K,
  args: Parameters<Commands[K]>[0],
): Promise<Awaited<ReturnType<Commands[K]>>> {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const a = args as any;
  switch (cmd) {
    case 'get_state':
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return snapshot() as any;
    case 'answer_ask': {
      lastAnswerAsk = { askId: a.askId as string, kind: a.kind as string };
      lastAnswerAskFull = { askId: a.askId as string, answer: a.answer as string, kind: a.kind as string };
      clearAskAndMaybeRestore(a.askId as string);
      emit();
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return undefined as any;
    }
    case 'answer_perm': {
      lastPermDecision = a.decision as string;
      const perm = perms.find((p) => p.id === a.permId);
      perms = perms.filter((p) => p.id !== a.permId);
      // Clearing the perm drops the attention override, same as an ask.
      if (perm?.sessionId) {
        const stillPending =
          perms.some((p) => p.sessionId === perm.sessionId) || asks.some((k) => k.sessionId === perm.sessionId);
        if (!stillPending) {
          const s = sessions.find((sess) => sess.id === perm.sessionId);
          if (s && s.status === 'attention') {
            s.status = s.preAskStatus ?? 'idle';
            s.statusChangedAt = Date.now();
            s.preAskStatus = undefined;
          }
        }
      }
      emit();
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return undefined as any;
    }
    case 'remove_row': {
      sessions = sessions.filter((s) => s.id !== a.sessionId);
      emit();
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return undefined as any;
    }
    case 'rename_session': {
      // SPEC §27 R-27.4: a user override wins over the normal title chain; an
      // empty/whitespace name clears it (restoring `baseTitle`). The real
      // backend also bidi-strips + caps at 60 graphemes; the mock just trims.
      const s = sessions.find((x) => x.id === a.sessionId);
      if (s) {
        if (s.baseTitle === undefined) s.baseTitle = s.title;
        const name = (a.name as string).trim();
        s.title = name.length > 0 ? name : (s.baseTitle ?? s.title);
      }
      emit();
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return undefined as any;
    }
    case 'set_setting': {
      const key = a.key as keyof SettingsState;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (settings as any)[key] = a.value;
      // R-25.2 "Unpin while in lamp mode → expand to list + revert to v1.0
      // tray-anchored behavior" (mirrors `should_force_list_on_unpin` in
      // `src-tauri/src/windows.rs`): the collapse button only shows while
      // pinned, so unpinning (e.g. via the lamp's right-click menu) is the
      // path back out for a user who didn't expand first.
      if (key === 'popupPinned' && a.value === false && settings.popupMode === 'lamp') {
        settings.popupMode = 'list';
      }
      emit();
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return undefined as any;
    }
    case 'install_hooks': {
      await delay(350);
      if (installShouldFail) {
        emit();
        throw new Error(
          "Could not read ~/.claude/settings.json: unexpected token at line 12. Fix the JSON and try again.",
        );
      }
      hooksInstalled = true;
      emit();
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return undefined as any;
    }
    case 'uninstall_hooks': {
      await delay(200);
      hooksInstalled = false;
      emit();
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return undefined as any;
    }
    case 'resize_popup':
      // No window to size in mock/browser mode — record the reported content
      // height (SPEC R-14.3) and the call count (R-31.2) so a spec can assert
      // them, and otherwise ignore.
      lastResizeContentHeight = a.contentHeight as number;
      resizePopupCalls += 1;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return undefined as any;
    case 'show_ask_window':
      // No second real window in mock/browser mode — record the call (SPEC
      // R-18.1 "(or via popup mirror click)") so a spec can assert it fired.
      showAskWindowCalls += 1;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return undefined as any;
    case 'focus_terminal':
      // SPEC R-15.4: no real terminal in mock/browser mode — record the call so
      // a spec can assert the row click fired it, and optionally reject to
      // exercise the inline "Couldn't find the terminal window" notice (R-15.4b).
      focusTerminalCalls += 1;
      lastFocusTerminalId = a.sessionId as string;
      if (focusTerminalShouldFail) {
        throw new Error("Couldn't find the terminal window");
      }
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return undefined as any;
    default:
      throw new Error(`quarterdeck mock: unknown command ${String(cmd)}`);
  }
}

export function onState(cb: (s: StateSnapshot) => void): () => void {
  listeners.push(cb);
  cb(snapshot());
  return () => {
    listeners = listeners.filter((l) => l !== cb);
  };
}

/** Expose a way for a screenshot/test harness to answer asks headlessly. */
export function _mockAnswerAsk(askId: string, answer: string, kind: AskAnswerKind): void {
  void invoke('answer_ask', { askId, answer, kind });
}

export function _mockScenarioNames(): string[] {
  return Object.keys(SCENARIOS);
}

/** Test-only: clears every session row in one shot (SPEC R-14.3 "50 rows →
 * 0" regression — bulk-removing via 50 individual `remove_row` calls would
 * work identically but slow the spec down for no extra coverage). */
export function _mockRemoveAllSessions(): void {
  sessions = [];
  emit();
}

/** Test-only: trims the fleet to its first `n` rows (SPEC R-31.2 — lets a spec
 * drive the `many-sessions` fixture down to 2 / 8 rows and prove the settings
 * pane stays a fixed 5-row height regardless of session count). */
export function _mockKeepFirstSessions(n: number): void {
  sessions = sessions.slice(0, n);
  emit();
}

// Test-only headless hooks for the Playwright e2e harness, attached only in mock
// mode (no Tauri host). The suite uses `answerAsk` to drive an unrelated
// `deck://state` push (answering a queued, non-primary ask) and prove a
// re-render preserves an in-progress ask answer + focus (R-8).
if (!isTauri()) {
  (window as unknown as { __qdMock?: Record<string, unknown> }).__qdMock = {
    answerAsk: _mockAnswerAsk,
    scenarioNames: _mockScenarioNames,
    removeAllSessions: _mockRemoveAllSessions,
    // R-31.2: trims the fleet so a spec can size the settings pane at 2/8 rows.
    keepFirstSessions: _mockKeepFirstSessions,
    // R-14.3: last content height the UI reported via `resize_popup`.
    lastResizeContentHeight: () => lastResizeContentHeight,
    // R-31.2: total `resize_popup` calls (snap vs. animated tween).
    resizePopupCallCount: () => resizePopupCalls,
    // R-18.1: counters for the cross-window "reopen" and "close" actions,
    // which have no observable real-window effect in mock/browser mode.
    showAskWindowCallCount: () => showAskWindowCalls,
    hideCurrentWindowCallCount: () => hideCurrentWindowCalls,
    // R-25.1: lamp drag-vs-click discrimination call count.
    startDraggingCallCount: () => startDraggingCalls,
    // R-15.4: click-to-focus counters (no real terminal in mock mode).
    focusTerminalCallCount: () => focusTerminalCalls,
    lastFocusTerminalId: () => lastFocusTerminalId,
    // R-16.2: last permission decision routed via `answer_perm`.
    lastPermDecision: () => lastPermDecision,
    // R-19.4: last (askId, kind) routed via `answer_ask` — asserts Dismiss
    // sends kind:"dismissed" from the ask window and the popup mirror.
    lastAnswerAsk: () => lastAnswerAsk,
    // R-29.4: last (askId, answer, kind) — asserts the form Submit sends the
    // `{answers:[...]}` document under kind:"form".
    lastAnswerAskFull: () => lastAnswerAskFull,
  };
}

// --- Background timeline (visual storytelling for manual dev use) ---------
// Disabled by ?story=off so the screenshot script gets a deterministic frame.
if (storyEnabled) {
  // Auto-expire asks whose countdown reaches zero, same as the real timeout path.
  setInterval(() => {
    const now = Date.now();
    const expired = asks.filter((a) => a.timeoutAt !== undefined && a.timeoutAt <= now);
    if (expired.length === 0) return;
    for (const a of expired) clearAskAndMaybeRestore(a.id);
    emit();
  }, 1000);

  // A couple of gentle state changes so the watch line/rows demonstrably animate.
  setTimeout(() => {
    const s = sessions.find((x) => x.id === 's2');
    if (s && s.status === 'working') {
      s.status = 'idle';
      s.statusChangedAt = Date.now();
      emit();
    }
  }, 9000);
}

export function _mockNewAskId(): string {
  askCounter += 1;
  return `a${askCounter}`;
}
