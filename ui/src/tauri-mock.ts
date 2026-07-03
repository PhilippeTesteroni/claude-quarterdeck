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
  AskRow,
  Commands,
  SessionRow,
  SessionStatus,
  SettingsState,
  StateSnapshot,
} from './ipc-contract';

interface InternalSession {
  id: string;
  project: string;
  title: string;
  branch?: string;
  status: SessionStatus;
  /** Status held before a pending ask forced `attention` (R-2.4 recompute-on-clear). */
  preAskStatus?: SessionStatus;
  inferred: boolean;
  cwd: string;
  statusChangedAt: number;
}

interface InternalAsk {
  id: string;
  sessionId?: string;
  project?: string;
  context?: string;
  question: string;
  options?: string[];
  timeoutAt?: number;
  createdAt: number;
  /** R-8.7: recovered-after-restart ask — renders as expired, Dismiss-only. */
  orphaned?: boolean;
}

const params = new URLSearchParams(location.search);
const storyEnabled = params.get('story') !== 'off';

let sessions: InternalSession[] = [];
let asks: InternalAsk[] = [];
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
const SCENARIOS: Record<string, () => { sessions: InternalSession[]; asks: InternalAsk[]; hooksInstalled: boolean; settings: SettingsState }> = {
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
  }));
  const askRows: AskRow[] = asks.map((a) => ({
    id: a.id,
    sessionId: a.sessionId,
    project: a.project,
    question: a.question,
    options: a.options,
    timeoutAt: a.timeoutAt,
    context: a.context,
    orphaned: a.orphaned,
  }));
  const counts = {
    attention: sessions.filter((s) => s.status === 'attention').length,
    working: sessions.filter((s) => s.status === 'working').length,
    idle: sessions.filter((s) => s.status === 'idle').length,
    dead: sessions.filter((s) => s.status === 'dead').length,
  };
  return { sessions: rows, asks: askRows, hooksInstalled, counts, settings: { ...settings } };
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

/** Test-only: records a `hideCurrentWindow()` call (SPEC R-18.1). Exported so
 * `ipc-client.ts` can call it from the mock branch of `hideCurrentWindow`. */
export function hideCurrentWindowMock(): void {
  hideCurrentWindowCalls += 1;
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
      clearAskAndMaybeRestore(a.askId as string);
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
    case 'set_setting': {
      const key = a.key as keyof SettingsState;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (settings as any)[key] = a.value;
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
      // height (SPEC R-14.3) so a spec can assert it, and otherwise ignore.
      lastResizeContentHeight = a.contentHeight as number;
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

// Test-only headless hooks for the Playwright e2e harness, attached only in mock
// mode (no Tauri host). The suite uses `answerAsk` to drive an unrelated
// `deck://state` push (answering a queued, non-primary ask) and prove a
// re-render preserves an in-progress ask answer + focus (R-8).
if (!isTauri()) {
  (window as unknown as { __qdMock?: Record<string, unknown> }).__qdMock = {
    answerAsk: _mockAnswerAsk,
    scenarioNames: _mockScenarioNames,
    removeAllSessions: _mockRemoveAllSessions,
    // R-14.3: last content height the UI reported via `resize_popup`.
    lastResizeContentHeight: () => lastResizeContentHeight,
    // R-18.1: counters for the cross-window "reopen" and "close" actions,
    // which have no observable real-window effect in mock/browser mode.
    showAskWindowCallCount: () => showAskWindowCalls,
    hideCurrentWindowCallCount: () => hideCurrentWindowCalls,
    // R-15.4: click-to-focus counters (no real terminal in mock mode).
    focusTerminalCallCount: () => focusTerminalCalls,
    lastFocusTerminalId: () => lastFocusTerminalId,
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
