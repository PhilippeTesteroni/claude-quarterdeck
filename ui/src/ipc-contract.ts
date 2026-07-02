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
}

export interface Counts {
  attention: number;
  working: number;
  idle: number;
  dead: number;
}

export interface StateSnapshot {
  sessions: SessionRow[];
  asks: AskRow[];
  hooksInstalled: boolean;
  counts: Counts;
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
  set_setting: (args: { key: string; value: boolean | string }) => Promise<void>;
  install_hooks: () => Promise<void>;
  uninstall_hooks: () => Promise<void>;
  get_state: () => Promise<StateSnapshot>;
}

export type CommandName = keyof Commands;
