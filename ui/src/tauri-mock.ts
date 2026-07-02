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
}

const params = new URLSearchParams(location.search);
const storyEnabled = params.get('story') !== 'off';

let sessions: InternalSession[] = [];
let asks: InternalAsk[] = [];
let hooksInstalled = true;
let installShouldFail = false;
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
    mcpEnabled: true,
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
  askCounter = asks.length;
}

loadScenario(params.get('scenario') ?? 'default');

function snapshot(): StateSnapshot {
  const now = Date.now();
  const rows: SessionRow[] = sessions.map((s) => ({
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
