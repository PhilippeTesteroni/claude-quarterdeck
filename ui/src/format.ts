/**
 * Formatting + sorting helpers shared by the popup and ask windows (SPEC §7).
 * Pure functions, no DOM — kept separate so they're trivial to unit-test later.
 */

import type { SessionRow, SessionStatus } from './ipc-contract';

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

/** R-7.3 sort: attention -> working -> idle -> dead; within group, latest activity first. */
const STATUS_RANK: Record<SessionStatus, number> = {
  attention: 0,
  working: 1,
  idle: 2,
  dead: 3,
};

export function sortSessions(rows: SessionRow[]): SessionRow[] {
  return [...rows].sort((a, b) => {
    const rankDiff = STATUS_RANK[a.status] - STATUS_RANK[b.status];
    if (rankDiff !== 0) return rankDiff;
    // Latest activity first == least time spent in the current status.
    return a.sinceMs - b.sinceMs;
  });
}

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
