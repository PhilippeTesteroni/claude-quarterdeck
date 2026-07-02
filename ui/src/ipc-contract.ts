/**
 * Quarterdeck IPC contract (T0).
 *
 * The Rust shell is the single source of truth: it pushes full StateSnapshot
 * objects to the frontend over the `deck://state` event, and the frontend sends
 * intent back through the typed commands below. Keep this file in lockstep with
 * the serde types in `src-tauri/src/ipc.rs` (SPEC R-3.4).
 */

export type SessionStatus = 'working' | 'attention' | 'idle' | 'dead';

export interface SessionRow {
  id: string;
  project: string;
  title: string;
  branch?: string;
  status: SessionStatus;
  /** True when the row was inferred from cold-start discovery (UI shows `~`). */
  inferred: boolean;
  /** Milliseconds spent in the current status. */
  sinceMs: number;
  cwd: string;
}

export interface AskRow {
  id: string;
  sessionId?: string;
  project?: string;
  question: string;
  options?: string[];
  /** Epoch milliseconds when the ask times out, if a timeout was set. */
  timeoutAt?: number;
  /**
   * T4 addition: the raw `context` (agent cwd) the MCP call carried, needed
   * verbatim for the R-8.2 unmatched-ask display: "Unknown agent (<context>)".
   * Present only when `sessionId`/`project` could not be matched. NOTE FOR
   * T3/T7: mirror on the Rust `AskRow` in `src-tauri/src/ipc.rs`.
   */
  context?: string;
}

export interface Counts {
  attention: number;
  working: number;
  idle: number;
  dead: number;
}

/**
 * Persisted user settings (SPEC R-10.1) plus a couple of read-only facts the
 * settings pane needs to render (R-7.4: data dir path, version) and the
 * agent-questions toggle state (R-8.6). This is a T4 addition on top of the T0
 * contract: `get_state`/`deck://state` did not originally carry settings.
 * NOTE FOR T3/T7: mirror this as an (optionally-defaulted) field on the Rust
 * `StateSnapshot` in `src-tauri/src/ipc.rs` — the UI treats it as optional and
 * falls back to safe defaults (see `ui/src/tauri-mock.ts` for the shape) so a
 * backend that hasn't added it yet won't crash the frontend, but onboarding
 * and the settings pane are non-functional until it's wired up.
 */
export interface SettingsState {
  notifyIdle: boolean;
  notifyAttention: boolean;
  notifyReminder: boolean;
  launchAtLogin: boolean;
  onboardingDone: boolean;
  /** Agent-questions (MCP) enabled, R-8.6. */
  mcpEnabled: boolean;
  dataDir: string;
  version: string;
}

export interface StateSnapshot {
  sessions: SessionRow[];
  asks: AskRow[];
  hooksInstalled: boolean;
  counts: Counts;
  /** Optional until the backend mirrors `SettingsState` (see note above). */
  settings?: SettingsState;
}

/** Tauri event channel the shell emits full snapshots on. */
export const STATE_EVENT = 'deck://state';

/** Result kind of an MCP `ask_user` call, mirrored to the UI. */
export type AskAnswerKind = 'option' | 'text' | 'timeout' | 'dismissed';

/**
 * Tauri commands exposed by the shell (implemented in T3). Argument objects are
 * serialized to the matching Rust `#[tauri::command]` parameters.
 */
export interface Commands {
  answer_ask: (args: { askId: string; answer: string; kind: AskAnswerKind }) => Promise<void>;
  remove_row: (args: { sessionId: string }) => Promise<void>;
  /**
   * Generic settings setter. `key` is one of the `SettingsState` keys above
   * (`notifyIdle` | `notifyAttention` | `notifyReminder` | `launchAtLogin` |
   * `onboardingDone`), or `mcpEnabled` — setting `mcpEnabled` true/false is
   * how the UI drives "Enable/Disable agent questions" (R-8.6) without a
   * dedicated command; the backend runs the `claude mcp add`/remove + skill
   * copy side effect on that key's change.
   */
  set_setting: (args: { key: string; value: boolean | string }) => Promise<void>;
  install_hooks: () => Promise<void>;
  uninstall_hooks: () => Promise<void>;
  get_state: () => Promise<StateSnapshot>;
}

export type CommandName = keyof Commands;
