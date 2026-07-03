/**
 * Formatting + sorting helpers shared by the popup and ask windows (SPEC §7).
 * Pure functions, no DOM — kept separate so they're trivial to unit-test later.
 */

import type { SessionStatus } from './ipc-contract';

/** mm 'm' ss 's' with zero-padded seconds; hours fold in above 60 minutes. */
export function formatDuration(ms: number): string {
  const totalSec = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) {
    return `${h}h ${String(m).padStart(2, '0')}m`;
  }
  return `${m}m ${String(s).padStart(2, '0')}s`;
}

/**
 * Coarse duration for the session-age tooltip (SPEC R-22.3), e.g. "2h 14m",
 * "14m", or "just now". Minutes granularity (age is a broad "how long has this
 * session been alive" signal, not a live-ticking timer).
 */
export function formatAge(ms: number): string {
  const totalMin = Math.max(0, Math.floor(ms / 60_000));
  if (totalMin < 1) return 'just now';
  const h = Math.floor(totalMin / 60);
  const m = totalMin % 60;
  if (h > 0) return `${h}h ${String(m).padStart(2, '0')}m`;
  return `${m}m`;
}

/** mm:ss countdown for the ask window timeout. */
export function formatCountdown(ms: number): string {
  const totalSec = Math.max(0, Math.ceil(ms / 1000));
  const m = Math.floor(totalSec / 60);
  const s = totalSec % 60;
  return `${m}:${String(s).padStart(2, '0')}`;
}

// R-7.3 session ordering (attention -> working -> idle -> dead; within a group,
// most-recently-active first) is computed ONCE, canonically, by the Rust engine
// (`SessionStore::view`) and delivered pre-sorted in every `StateSnapshot`. The
// frontend is dumb (R-3.4): it renders `snapshot.sessions` in the given order and
// never re-sorts. (An earlier client-side sort tie-broke on `sinceMs` — the R-7.2
// time-in-status field — which is a different quantity from the engine's
// `last_activity_ms` tie-break, so it diverged from the canonical order.)

const FOOTER_LABELS: Record<SessionStatus, string> = {
  attention: 'needs you',
  working: 'working',
  idle: 'idle',
  dead: 'dead',
};

/** R-7.3 footer copy, e.g. "1 needs you · 2 working · 1 idle". Omits zero groups. */
export function footerText(counts: { attention: number; working: number; idle: number; dead: number }): string {
  const order: SessionStatus[] = ['attention', 'working', 'idle', 'dead'];
  const parts = order
    .filter((status) => counts[status] > 0)
    .map((status) => `${counts[status]} ${FOOTER_LABELS[status]}`);
  return parts.join(' · ');
}

/**
 * Worst-of aggregate status (SPEC §25 R-25.1, mirrors `TrayStatus::worst_of`
 * in `src-tauri/src/tray.rs` and the tray icon it drives): the lamp shows this
 * as its single color. `'gray'` covers zero sessions or all-dead, same as the
 * tray icon (R-2.6). A pure, trivial derivation from `counts` — computed
 * client-side same as `footerText`/the watch line segments, not a violation of
 * "frontend is dumb" (R-3.4), which is about business logic, not re-deriving
 * a max() over data already on the wire.
 */
export function worstStatus(counts: { attention: number; working: number; idle: number }): SessionStatus | 'gray' {
  if (counts.attention > 0) return 'attention';
  if (counts.working > 0) return 'working';
  if (counts.idle > 0) return 'idle';
  return 'gray';
}

export function truncate(text: string, max: number): string {
  // Count by Unicode code points, not UTF-16 code units, so an astral character
  // (emoji, e.g. in a Cyrillic/OneDrive path shown as "Unknown agent (<context>)",
  // R-5.3/R-8.2) is never sliced through the middle into a lone surrogate that
  // renders as a broken/tofu glyph. `Array.from` splits on code points.
  const chars = Array.from(text);
  if (chars.length <= max) return text;
  return `${chars.slice(0, Math.max(0, max - 1)).join('').trimEnd()}…`;
}
