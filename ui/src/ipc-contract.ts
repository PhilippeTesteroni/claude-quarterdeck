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
  /**
   * True when the row is a cold-start estimate (R-5.4/R-22): its time-in-status
   * is seeded, not hook-exact, so the UI renders the time with a `~` prefix
   * (R-22.4). Cleared on the first status-marking hook event (R-22.2).
   */
  inferred: boolean;
  /** Milliseconds spent in the current status. */
  sinceMs: number;
  cwd: string;
  /**
   * Active background subagents (SPEC R-21.2): the row shows a compact `⛭ N`
   * badge while > 0. Absent/0 hides it.
   */
  subagents?: number;
  /**
   * Total session age in ms when an anchor is known (SPEC R-22.3): shown in the
   * row's hover tooltip alongside the cwd as "session 2h 14m".
   */
  ageMs?: number;
  /**
   * Context fill percent (SPEC R-23.2a/R-23.4): the row's second line reads
   * `ctx {ctxPercent}% · …`, amber ≥75, red ≥90 (+ a "context nearly full"
   * tooltip). Absent until a usage record is read, or when `showTokenStats` off.
   */
  ctxPercent?: number;
  /**
   * Session spend, compact (SPEC R-23.2b/R-23.4): the `· {spend}` half of the
   * second line (e.g. `1.4M`). Absent when zero/unavailable.
   */
  spend?: string;
  /** True when `spend` is a lower bound after a truncation/overflow rescan
   * (SPEC R-23.1) — rendered with a "≥" prefix. */
  spendApprox?: boolean;
  /**
   * Combined subagent/sidechain spend, compact (SPEC R-23.3): the `· {spend}`
   * suffix on the `⛭ N` badge (`⛭ 3 · 2.1M`). Absent when zero.
   */
  subagentSpend?: string;
}

/**
 * One question inside a multi-question / multi-select `ask_user` form (SPEC §29,
 * R-29.1). Mirror of `AskQuestion` in `crates/deck-core/src/ask.rs`. Every string
 * is bidi-stripped + grapheme-capped by the shell before it reaches here.
 */
export interface AskQuestion {
  /** Optional short header/label rendered above the question. */
  header?: string;
  /** The question text (always present). */
  question: string;
  /** true → multiple options selectable (checkboxes); false/absent → one (radio). */
  multiSelect?: boolean;
  /** The offered choices (may be empty for a free-text-only sub-question). */
  options: string[];
}

export interface AskRow {
  id: string;
  sessionId?: string;
  project?: string;
  question: string;
  options?: string[];
  /**
   * Multi-question / multi-select form (SPEC §29, R-29.5): when present and
   * non-empty, the ask window renders a form of these blocks and the popup mirror
   * shows "N questions — Answer in window". Absent for a legacy single-question ask.
   */
  questions?: AskQuestion[];
  /** Long rationale/body (R-19.1), rendered muted under the question. */
  detail?: string;
  /**
   * Epoch milliseconds when the ask times out, if a timeout was set. Absent for
   * persistent asks (R-19.2), which render with no countdown.
   */
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
  /**
   * Epoch ms the ask was enqueued (arrival time). The shared ask/perm FIFO
   * (R-16.2): the ask window's primary slot goes to whichever of the front ask
   * / front perm has the smaller `queuedAt` — arrival order, not perms-first.
   */
  queuedAt: number;
}

/**
 * A pending permission request (SPEC §16, R-16.2). Shares the always-on-top ask
 * window with {@link AskRow} but renders distinctly (amber) with Allow / Deny /
 * In terminal actions. Mirror of `PermRow` in `src-tauri/src/ipc.rs`.
 */
export interface PermRow {
  id: string;
  sessionId?: string;
  project?: string;
  /** The tool Claude Code wants to run (e.g. `Bash`), sanitized. */
  toolName: string;
  /** Compact tool input, sanitized + capped (R-16.1/R-16.5), shown verbatim. */
  toolInput: string;
  /** Raw calling context for an unmatched perm ("Unknown agent (<context>)"). */
  context?: string;
  /**
   * Epoch ms the perm arrived — its position in the shared ask/perm FIFO
   * (R-16.2). Compared against a front ask's `queuedAt`.
   */
  queuedAt: number;
  /**
   * SPEC R-32.1: epoch ms at which this perm expires. Its `PermissionRequest`
   * hook (90 s timeout, R-16.1) has by then exited, so a deck decision could no
   * longer reach it. The shell sweeps the perm off its tick past this instant;
   * until then the UI disables the Allow/Deny buttons. Absent on an older
   * snapshot without the field.
   */
  expiresAt?: number;
}

/** The deck-side decision for a perm (SPEC R-16.2). `defer` = "In terminal" —
 * no decision, the hook falls through to the terminal dialog. */
export type PermDecision = 'allow' | 'deny' | 'defer';

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
  /**
   * Take over Claude Code permission prompts into the deck (SPEC §16, R-16.4).
   * Default ON after onboarding consent; the settings toggle + onboarding
   * consent line drive it via `set_setting('takeoverPermissions', …)`.
   */
  takeoverPermissions: boolean;
  /**
   * Show per-session token usage on rows (SPEC R-23.5). Default ON. Drives the
   * "Token stats" settings toggle and gates the row usage line + subagent-spend
   * badge suffix client-side.
   */
  showTokenStats: boolean;
  /**
   * Popup display mode (SPEC §25, R-25.2): `list` is the full popup; `lamp` is
   * the compact ~56x56 always-on-top traffic-light square (R-25.1). Driven by
   * `set_setting('popupMode', …)`, same mechanism as `popupPinned`.
   */
  popupMode: 'list' | 'lamp';
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
  /** Pending permission requests (SPEC §16), rendered in the ask window. */
  perms: PermRow[];
  hooksInstalled: boolean;
  counts: Counts;
  /** Optional until the backend mirrors `SettingsState` (see note above). */
  settings?: SettingsState;
}

/** Tauri event channel the shell emits full snapshots on. */
export const STATE_EVENT = 'deck://state';

/** Result kind of an MCP `ask_user` call, mirrored to the UI. `cancelled` (R-19.5)
 * is produced by a `cancel_ask` tool call, never by a UI action. */
export type AskAnswerKind = 'option' | 'text' | 'timeout' | 'dismissed' | 'cancelled' | 'form';

/**
 * Tauri commands exposed by the shell (implemented in T3). Argument objects are
 * serialized to the matching Rust `#[tauri::command]` parameters.
 */
export interface Commands {
  answer_ask: (args: { askId: string; answer: string; kind: AskAnswerKind }) => Promise<void>;
  /**
   * Answers a pending permission request (SPEC §16, R-16.2). `decision` is
   * `allow` / `deny` / `defer` ("In terminal"); `reason` is the optional deny
   * reason. The decision is persisted for the blocked `PermissionRequest` hook.
   */
  answer_perm: (args: { permId: string; decision: PermDecision; reason?: string }) => Promise<void>;
  remove_row: (args: { sessionId: string }) => Promise<void>;
  /**
   * Renames a session (SPEC §27 R-27.4): sets a user title override that wins
   * over every other title source (registry name, session title, prompt). An
   * empty/whitespace `name` clears the override, restoring the normal chain. The
   * name is bidi-stripped + capped to 60 grapheme clusters shell-side (R-27.7).
   */
  rename_session: (args: { sessionId: string; name: string }) => Promise<void>;
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
   * Ask-window content self-measurement (SPEC §35.2 auto-size). The frontend
   * reports its content height after each render; the shell clamps it to
   * 140..=640 and resizes the always-on-top ask window height-only, keeping its
   * position stable (all sizing logic stays in Rust, R-3.4).
   */
  resize_ask: (args: { contentHeight: number }) => Promise<void>;
  /**
   * Brings the ask window forward without stealing focus (SPEC R-18.1 "(or
   * via popup mirror click)"): clicking a mirrored ask row in the popup calls
   * this to re-surface the ask window after it was closed via its own X
   * button while asks are still pending. A no-op if already visible.
   */
  show_ask_window: () => Promise<void>;
}

export type CommandName = keyof Commands;
