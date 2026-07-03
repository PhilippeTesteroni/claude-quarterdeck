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
  /**
   * R-8.7: true when this ask was recovered from disk after a restart — it can
   * never be answered (its MCP connection is gone), so it renders as expired
   * with only a Dismiss action.
   */
  orphaned?: boolean;
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
  /**
   * Popup pin-on-top state (SPEC R-14.2): persisted so the header pin toggle
   * reflects it across restarts. Pinned disables hide-on-blur and skips
   * anchor-to-tray on open; toggling it is a `set_setting('popupPinned', …)`
   * call, same mechanism as `mcpEnabled` (R-8.6).
   */
  popupPinned: boolean;
  /** Agent-questions (MCP) enabled, R-8.6. */
  mcpEnabled: boolean;
  /**
   * R-8.6: whether the `claude` CLI is on PATH. When false, the settings pane
   * shows `mcpCommand` for the user to run by hand ("else shows the exact
   * command to copy").
   */
  mcpCliAvailable: boolean;
  /**
   * R-8.6: the exact `claude mcp add …` command (with the real port + token).
   * Undefined until the MCP server is up.
   */
  mcpCommand?: string;
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
  /**
   * Popup content self-measurement for the grow-then-scroll window (R-7.1). The
   * frontend reports its content height; the shell clamps it to 460..=560 and
   * resizes/re-anchors the window (all sizing logic stays in Rust, R-3.4).
   */
  resize_popup: (args: { contentHeight: number }) => Promise<void>;
  /**
   * Brings the ask window forward without stealing focus (SPEC R-18.1 "(or
   * via popup mirror click)"): clicking a mirrored ask row in the popup calls
   * this to re-surface the ask window after it was closed via its own X
   * button while asks are still pending. A no-op if already visible.
   */
  show_ask_window: () => Promise<void>;
  /**
   * Focuses the terminal window hosting a session (SPEC R-15.4): a row click or
   * the "Focus terminal" context-menu item. Best-effort — rejects with
   * "Couldn't find the terminal window" when no window could be focused, which
   * the popup shows as an inline notice (R-15.4b).
   */
  focus_terminal: (args: { sessionId: string }) => Promise<void>;
}

export type CommandName = keyof Commands;
