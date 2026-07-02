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

export function truncate(text: string, max: number): string {
  if (text.length <= max) return text;
  return `${text.slice(0, Math.max(0, max - 1)).trimEnd()}…`;
}
