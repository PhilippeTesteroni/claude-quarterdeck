/**
 * Popup window controller (SPEC §7). Snapshot-driven: the shell (or the mock)
 * pushes full `StateSnapshot`s; this file only renders + sends intent back
 * through `ipc-client.ts` commands. No business logic lives here (R-3.4).
 */

import { invoke, onState, startDragging, usingMock } from './ipc-client';
import type { AskAnswerKind, AskRow, PermDecision, PermRow, SessionRow, SessionStatus, SettingsState, StateSnapshot } from './ipc-contract';
import { footerText, formatAge, formatDuration, truncate } from './format';
import { clear, h } from './dom';
import { installMockScenarioSwitcher } from './mock-switcher';

const elWatchline = document.getElementById('qd-watchline') as HTMLElement;
const elContent = document.getElementById('qd-content') as HTMLElement;
const elFooter = document.getElementById('qd-footer') as HTMLElement;
const elSettings = document.getElementById('qd-settings') as HTMLElement;
const elGear = document.getElementById('qd-gear') as HTMLButtonElement;
const elPin = document.getElementById('qd-pin') as HTMLButtonElement;
const elCollapse = document.getElementById('qd-collapse') as HTMLButtonElement;
const elApp = document.getElementById('app') as HTMLElement;
const elLamp = document.getElementById('qd-lamp') as HTMLButtonElement;
const elLampPie = document.getElementById('qd-lamp-pie') as unknown as SVGSVGElement;
const elLampBadge = document.getElementById('qd-lamp-badge') as HTMLElement;

let latest: StateSnapshot | null = null;
let receivedAtPerf = 0;
let settingsOpen = false;
/** Last content height reported to the shell via `resize_popup`. Seeds the
 * settings open/close tween (so it animates from where the window actually is)
 * and lets us skip redundant reports (R-14.3 / R-31.2). */
let lastReportedHeight: number | null = null;
/** Handle for an in-flight settings-resize tween, so a rapid re-toggle cancels
 * the previous one instead of two rAF loops fighting over the height. */
let resizeTweenRaf: number | null = null;
let ctxMenuEl: HTMLElement | null = null;
/** SPEC §27 R-27.5: the in-progress rename editor. When set, the matching
 * session row renders an `<input>` instead of its title span, and the edit
 * survives every `renderContent` rebuild (a background `deck://state` push must
 * not wipe a name the user is typing) — the module-level guard is the analog of
 * `captureAskInputs` for the single rename field. */
interface RenameEdit {
  id: string;
  value: string;
  focused: boolean;
  selStart: number | null;
  selEnd: number | null;
}
let renameEdit: RenameEdit | null = null;
/** Set while `renderContent` tears down the DOM: removing the focused rename
 * `<input>` fires a synchronous `blur`, which must NOT be mistaken for a user
 * commit (that would close the editor on every background push — the exact thing
 * R-27.5 says must survive). The real user-blur path never runs through the
 * rebuild, so it still commits. */
let suppressRenameBlur = false;
let installBusy = false;
let installError: string | null = null;
let uninstallBusy = false;
/** Local-only echo of the onboarding "launch at login?" choice, so the Yes/No
 * pair only highlights after this session's explicit click (R-10.2). */
let lastLaunchChoice: boolean | null = null;

// (session row time element, base sinceMs at time of snapshot, and the `~`
// estimate prefix, R-22.4) — updated by the 1s ticker below without touching
// the rest of the DOM (keeps focus/menus).
let timeTicks: Array<{ el: HTMLElement; base: number; prefix: string }> = [];

// §43: `waiting` (blue) slots between working and idle in the watch line too.
const WATCHLINE_ORDER: SessionStatus[] = ['attention', 'working', 'waiting', 'idle', 'dead'];
const watchlineSegs = new Map<string, HTMLElement>();
for (const status of WATCHLINE_ORDER) {
  const seg = h('div', { className: 'qd-watchline-seg', 'data-status': status });
  watchlineSegs.set(status, seg);
  elWatchline.append(seg);
}
const watchlineNone = h('div', { className: 'qd-watchline-seg', 'data-status': 'none', style: 'flex-basis:0%' });
elWatchline.append(watchlineNone);

// R-31.2 fixed settings height: the settings overlay sizes the window to
// `header + SETTINGS_ROW_COUNT × row + footer` so it's a stable pane regardless
// of how many (or few) sessions sit behind it. Rows use a nominal constant
// rather than a measured `.qd-row` on purpose — the empty state has no row to
// measure, and a constant keeps the height byte-identical at 0/2/8 sessions.
// The final window is still clamped to 160..560 in `windows.rs`.
const SETTINGS_ROW_COUNT = 5;
const SETTINGS_ROW_H = 36; // ≈ one single-line row: 13px/1.4 text + 16px pad + 1px border
const SETTINGS_FOOTER_H = 30; // nominal footer allowance (it's covered by the overlay anyway)
/** Duration of the settings open/close height tween, ms (snapped instantly
 * under reduced motion — see `tweenPopupHeight`). */
const SETTINGS_RESIZE_MS = 180;

function renderWatchline(counts: StateSnapshot['counts']): void {
  const total = counts.attention + counts.working + counts.waiting + counts.idle + counts.dead;
  if (total === 0) {
    for (const status of WATCHLINE_ORDER) {
      watchlineSegs.get(status)!.style.flexBasis = '0%';
    }
    watchlineNone.style.flexBasis = '100%';
    return;
  }
  watchlineNone.style.flexBasis = '0%';
  for (const status of WATCHLINE_ORDER) {
    const pct = (counts[status] / total) * 100;
    watchlineSegs.get(status)!.style.flexBasis = `${pct}%`;
  }
}

function statusDot(status: SessionStatus): HTMLElement {
  return h('span', { className: 'qd-row-dot', 'data-status': status });
}

function closeCtxMenu(): void {
  ctxMenuEl?.remove();
  ctxMenuEl = null;
}
document.addEventListener('click', closeCtxMenu);
document.addEventListener('scroll', closeCtxMenu, true);

function openCtxMenu(x: number, y: number, row: SessionRow): void {
  closeCtxMenu();
  const menu = h('div', { className: 'qd-ctx-menu', style: `left:${x}px;top:${y}px` }, [
    h(
      'button',
      {
        className: 'qd-ctx-item',
        type: 'button',
        onclick: (ev: Event) => {
          ev.stopPropagation();
          void navigator.clipboard?.writeText(row.id).catch(() => undefined);
          closeCtxMenu();
        },
      },
      ['Copy session id'],
    ),
    // SPEC §27 R-27.5/R-27.6: rename inline, or reset back to the derived name.
    h(
      'button',
      {
        className: 'qd-ctx-item',
        type: 'button',
        onclick: (ev: Event) => {
          ev.stopPropagation();
          closeCtxMenu();
          beginRename(ev, row);
        },
      },
      ['Rename'],
    ),
    h(
      'button',
      {
        className: 'qd-ctx-item',
        type: 'button',
        onclick: (ev: Event) => {
          ev.stopPropagation();
          void invoke('rename_session', { sessionId: row.id, name: '' }).catch(() => undefined);
          closeCtxMenu();
        },
      },
      ['Reset name'],
    ),
    h(
      'button',
      {
        className: 'qd-ctx-item danger',
        type: 'button',
        onclick: (ev: Event) => {
          ev.stopPropagation();
          void invoke('remove_row', { sessionId: row.id });
          closeCtxMenu();
        },
      },
      ['Remove row'],
    ),
    // §38 kill-agent-process: only offered for a row whose Claude host pid is
    // known. Confirm-free but clearly labelled — force-terminates the process
    // and removes the row.
    row.pid != null
      ? h(
          'button',
          {
            className: 'qd-ctx-item danger',
            type: 'button',
            onclick: (ev: Event) => {
              ev.stopPropagation();
              void invoke('kill_session', { sessionId: row.id });
              closeCtxMenu();
            },
          },
          ['Kill process'],
        )
      : null,
  ]);
  document.body.append(menu);
  const rect = menu.getBoundingClientRect();
  const maxX = window.innerWidth - rect.width - 4;
  const maxY = window.innerHeight - rect.height - 4;
  menu.style.left = `${Math.min(x, Math.max(4, maxX))}px`;
  menu.style.top = `${Math.min(y, Math.max(4, maxY))}px`;
  ctxMenuEl = menu;
}

function renderSessionRow(row: SessionRow, showTokens: boolean): HTMLElement {
  // §36 working-time timer: while 🟡 working, show a live counter of time spent
  // on the current turn, anchored at the real work start (UserPromptSubmit) —
  // NOT the time-in-status, so §30 reverse-gear / §21 busy-override flips don't
  // reset it. When it stops (🟢 idle after a Stop) freeze it as "took <dur>"
  // instead of a running idle timer. Every other state (or a seeded row with no
  // real work-start) falls back to the R-22.4 time-in-status, with the `~`
  // estimate prefix while inferred.
  const timePrefix = row.inferred ? '~' : '';
  let timeEl: HTMLElement;
  if (!row.inferred && row.status === 'working' && row.workStartedMs != null) {
    // Live counter: base = elapsed-since-work-start captured now, then ticked up
    // by the shared 1s ticker (same mechanism as time-in-status).
    const base = Math.max(0, Date.now() - row.workStartedMs);
    timeEl = h('span', { className: 'qd-row-time mono' }, [formatDuration(base)]);
    timeTicks.push({ el: timeEl, base, prefix: '' });
  } else if (row.status === 'idle' && row.lastWorkMs != null) {
    // Frozen total of the just-finished turn — no ticker entry (it must not run).
    timeEl = h('span', { className: 'qd-row-time mono took' }, [
      `took ${formatDuration(row.lastWorkMs)}`,
    ]);
  } else {
    timeEl = h('span', { className: `qd-row-time mono${row.inferred ? ' estimated' : ''}` }, [
      timePrefix + formatDuration(row.sinceMs),
    ]);
    timeTicks.push({ el: timeEl, base: row.sinceMs, prefix: timePrefix });
  }

  // R-22.3: the tooltip shows total session age alongside the cwd when known.
  let tooltip = row.ageMs != null ? `${row.cwd}\nsession ${formatAge(row.ageMs)}` : row.cwd;

  // R-23.4: a second line under the time — `ctx {pct}% · {spend}`, mono/muted,
  // amber ≥75% and red ≥90% context fill (with a "nearly full" tooltip line).
  const ctx = showTokens ? row.ctxPercent : undefined;
  const spend = showTokens ? row.spend : undefined;
  let usageEl: HTMLElement | null = null;
  if (ctx != null || (spend != null && spend !== '')) {
    const parts: HTMLElement[] = [];
    if (ctx != null) {
      const level = ctx >= 90 ? ' crit' : ctx >= 75 ? ' warn' : '';
      parts.push(h('span', { className: `qd-row-ctx${level}` }, [`ctx ${ctx}%`]));
    }
    if (spend != null && spend !== '') {
      const prefix = row.spendApprox ? '≥' : '';
      parts.push(h('span', { className: 'qd-row-spend' }, [`${prefix}${spend}`]));
    }
    // Join the parts with a middot separator.
    const joined: Array<HTMLElement | string> = [];
    parts.forEach((p, i) => {
      if (i > 0) joined.push(' · ');
      joined.push(p);
    });
    usageEl = h('span', { className: 'qd-row-usage mono' }, joined);
    if (ctx != null && ctx >= 90) {
      tooltip += '\ncontext nearly full — consider /compact or a fresh session';
    }
  }

  // §37 R-37: a plain multi-agent glyph while background subagents are running —
  // just an icon meaning "multi-agent activity", carrying no count and no spend.
  // The old `⛭ N · {spend}` chip (R-21.2 / R-23.3) surfaced a cumulative token
  // total that read as per-flow and confused more than it informed; the glyph
  // shows whenever `active_subagents > 0` (the §43 WaitingWorkflow /
  // working-with-subagents case). The per-session `ctx% · spend` line is
  // unchanged. The exact count survives in the tooltip only.
  const subagents = row.subagents ?? 0;
  const badge =
    subagents > 0
      ? h(
          'span',
          {
            className: 'qd-row-subagents',
            title: `${subagents} background ${subagents === 1 ? 'subagent' : 'subagents'} running`,
            'aria-label': 'multi-agent activity',
          },
          ['⛭'],
        )
      : null;

  const el = h(
    'div',
    {
      className: 'qd-row',
      title: tooltip,
      oncontextmenu: (ev: Event) => {
        ev.preventDefault();
        const mouse = ev as MouseEvent;
        openCtxMenu(mouse.clientX, mouse.clientY, row);
      },
    },
    [
      // §42 R-42: a roomier two-line row (Mission Control, dense-but-calm).
      // Line 1 is the primary read: the status dot, the session name, and the
      // right-aligned §36 working-time. Line 2 carries the subordinate detail —
      // project/branch and the §23 `ctx% · spend` usage with the §37 multi-agent
      // glyph, grouped hard-right and indented under the name.
      h('div', { className: 'qd-row-line1' }, [
        statusDot(row.status),
        renameEdit?.id === row.id ? buildRenameInput(row) : renderRowTitle(row),
        timeEl,
      ]),
      h('div', { className: 'qd-row-line2' }, [
        h('span', { className: 'qd-row-project' }, [row.project]),
        row.branch ? h('span', { className: 'qd-row-branch mono' }, [row.branch]) : null,
        h('div', { className: 'qd-row-line2-end' }, [usageEl, badge]),
      ]),
    ],
  );
  return el;
}

/** The normal (non-editing) row title span. SPEC §27 R-27.5: double-clicking it
 * opens the inline rename editor. The title's own click is swallowed so a
 * click on it never bubbles to the row (which no longer does anything on click). */
function renderRowTitle(row: SessionRow): HTMLElement {
  return h(
    'span',
    {
      className: 'qd-row-title',
      title: 'Double-click to rename',
      onclick: (ev: Event) => ev.stopPropagation(),
      ondblclick: (ev: Event) => beginRename(ev, row),
    },
    [row.title],
  );
}

/** Build the inline rename `<input>` (SPEC §27 R-27.5): seeded with the current
 * title, autofocused, committing on Enter/blur and cancelling on Escape. Every
 * pointer gesture on it `stopPropagation`s so it never triggers the row's
 * focus-terminal click. Text-node-only via `h()` — no injection surface. */
function buildRenameInput(row: SessionRow): HTMLInputElement {
  const seed = renameEdit?.id === row.id ? renameEdit.value : row.title;
  const input = h('input', {
    className: 'qd-row-title-edit',
    type: 'text',
    'data-rename-id': row.id,
    onclick: (ev: Event) => ev.stopPropagation(),
    ondblclick: (ev: Event) => ev.stopPropagation(),
    oninput: () => {
      if (renameEdit?.id === row.id) renameEdit.value = input.value;
    },
    onkeydown: (ev: Event) => {
      const kev = ev as KeyboardEvent;
      if (kev.key === 'Enter') {
        kev.preventDefault();
        commitRename(row.id, input.value);
      } else if (kev.key === 'Escape') {
        kev.preventDefault();
        cancelRename();
      }
    },
    onblur: () => {
      if (suppressRenameBlur) return;
      commitRename(row.id, input.value);
    },
  }) as HTMLInputElement;
  // Set the property (not just the attribute) so the seed is the live value.
  input.value = seed;
  return input;
}

/** Enter the rename editor for a row (SPEC §27 R-27.5). */
function beginRename(ev: Event, row: SessionRow): void {
  ev.stopPropagation();
  ev.preventDefault();
  renameEdit = { id: row.id, value: row.title, focused: true, selStart: null, selEnd: null };
  if (latest) renderContent(latest);
}

/** Commit a rename (Enter/blur): send the (possibly empty → clears) name and
 * leave the editor. Single-flight via the `renameEdit` guard so a blur firing
 * right after Enter doesn't double-submit. */
function commitRename(id: string, raw: string): void {
  if (renameEdit?.id !== id) return;
  renameEdit = null;
  void invoke('rename_session', { sessionId: id, name: raw.trim() }).catch(() => undefined);
  if (latest) renderContent(latest);
}

/** Cancel a rename (Escape): discard the edit and restore the title span. */
function cancelRename(): void {
  if (!renameEdit) return;
  renameEdit = null;
  if (latest) renderContent(latest);
}

/** Capture the live rename-input value + focus/selection before an `elContent`
 * rebuild so a background state push can't wipe an in-progress name (R-27.5,
 * mirrors `captureAskInputs`). */
function captureRenameInput(): void {
  if (!renameEdit) return;
  for (const el of elContent.querySelectorAll<HTMLInputElement>('.qd-row-title-edit')) {
    if (el.getAttribute('data-rename-id') !== renameEdit.id) continue;
    renameEdit.value = el.value;
    renameEdit.focused = el === document.activeElement;
    renameEdit.selStart = el.selectionStart;
    renameEdit.selEnd = el.selectionEnd;
    return;
  }
}

/** Restore rename-input focus/selection after the rebuild. Clears the editor if
 * its row vanished (the session ended) so it can't wedge. */
function restoreRenameInput(): void {
  if (!renameEdit) return;
  for (const el of elContent.querySelectorAll<HTMLInputElement>('.qd-row-title-edit')) {
    if (el.getAttribute('data-rename-id') !== renameEdit.id) continue;
    el.value = renameEdit.value;
    if (renameEdit.focused) {
      el.focus();
      try {
        // A fresh open (null selection) selects all so a retype replaces cleanly.
        const start = renameEdit.selStart ?? 0;
        const end = renameEdit.selEnd ?? renameEdit.value.length;
        el.setSelectionRange(start, end);
      } catch {
        /* setSelectionRange can throw on some states; value is already restored. */
      }
    }
    return;
  }
  // The editing row is gone (session ended) — drop the editor.
  renameEdit = null;
}

function askAgentLabel(ask: AskRow): string {
  if (ask.project) return ask.project;
  if (ask.context) return `Unknown agent (${truncate(ask.context, 40)})`;
  return 'Unknown agent';
}

/** Ask ids already answered from a mirrored popup row, so a second answer for
 * the SAME ask (double-click, or a click racing a leftover Enter) is dropped:
 * both answer_ask writes target the same answers/<askId>.json, the second
 * overwriting the first, and the debounced watcher delivers only the last —
 * silently discarding the user's first answer. Single-flight prevents it. */
const answeredAsks = new Set<string>();

/** SPEC R-18.1 "(or via popup mirror click)": clicking a mirrored ask row
 * re-surfaces the ask window (a no-op if it's already visible) after it was
 * closed via its own X while asks are still pending. Ignored when the click
 * actually hit an interactive control (option/dismiss button, the free-text
 * input) so those keep their own behavior. */
function reopenAskWindowUnlessInteractive(ev: Event): void {
  const target = ev.target as HTMLElement | null;
  if (target?.closest('button, input')) return;
  void invoke('show_ask_window', undefined).catch(() => undefined);
}

function renderAskMirrorRow(ask: AskRow): HTMLElement {
  const send = (answer: string, kind: AskAnswerKind): void => {
    if (answeredAsks.has(ask.id)) return;
    answeredAsks.add(ask.id);
    // Let the user retry only if the answer never reached the backend.
    void invoke('answer_ask', { askId: ask.id, answer, kind }).catch(() => {
      answeredAsks.delete(ask.id);
    });
  };

  // R-8.7: an ask recovered after a restart can never be answered — show it as
  // expired with only a Dismiss action, "never answered into the void".
  if (ask.orphaned) {
    return h('div', { className: 'qd-ask-row qd-ask-row-expired', onclick: reopenAskWindowUnlessInteractive }, [
      h('div', { className: 'qd-ask-row-head' }, [
        h('span', { className: 'qd-ask-row-agent' }, [askAgentLabel(ask)]),
        h('span', { style: 'color:var(--muted)' }, ['· expired']),
      ]),
      h('div', { className: 'qd-ask-row-question' }, [ask.question]),
      h('div', { className: 'qd-ask-row-actions' }, [
        h('span', { className: 'qd-ask-row-expired-note' }, ['Expired while Quarterdeck was closed.']),
        h(
          'button',
          { className: 'qd-ask-row-dismiss', type: 'button', title: 'Dismiss', onclick: () => send('', 'dismissed') },
          ['Dismiss'],
        ),
      ]),
    ]);
  }

  // SPEC §29 (R-29.5): a multi-question form is too large to answer inline in the
  // popup — show a compact "N questions — Answer in window" summary that
  // re-surfaces the ask window instead of the option buttons / free-text input.
  // (No `.qd-ask-row-input` here, so `captureAskInputs` skips form rows.)
  if (ask.questions && ask.questions.length > 0) {
    const n = ask.questions.length;
    return h('div', { className: 'qd-ask-row qd-ask-row-form', onclick: reopenAskWindowUnlessInteractive }, [
      h('div', { className: 'qd-ask-row-head' }, [
        h('span', { className: 'qd-ask-row-agent' }, [askAgentLabel(ask)]),
        h('span', { style: 'color:var(--muted)' }, [`asks · ${n} question${n === 1 ? '' : 's'}`]),
      ]),
      h('div', { className: 'qd-ask-row-question' }, [ask.question]),
      h('div', { className: 'qd-ask-row-actions' }, [
        h(
          'button',
          {
            className: 'qd-btn qd-ask-row-opt',
            type: 'button',
            onclick: () => void invoke('show_ask_window', undefined).catch(() => undefined),
          },
          ['Answer in window'],
        ),
        h(
          'button',
          { className: 'qd-ask-row-dismiss', type: 'button', title: 'Dismiss', onclick: () => send('', 'dismissed') },
          ['Dismiss'],
        ),
      ]),
    ]);
  }

  const input = h('input', {
    className: 'qd-ask-row-input',
    type: 'text',
    placeholder: 'Type an answer…',
    'data-ask-id': ask.id,
    onkeydown: (ev: Event) => {
      const kev = ev as KeyboardEvent;
      if (kev.key === 'Enter') {
        const value = (input as HTMLInputElement).value.trim();
        if (value) send(value, 'text');
      }
    },
  }) as HTMLInputElement;

  const options = (ask.options ?? []).map((opt) =>
    h(
      'button',
      { className: 'qd-btn qd-ask-row-opt', type: 'button', onclick: () => send(opt, 'option') },
      [opt],
    ),
  );

  return h('div', { className: 'qd-ask-row', onclick: reopenAskWindowUnlessInteractive }, [
    h('div', { className: 'qd-ask-row-head' }, [
      h('span', { className: 'qd-ask-row-agent' }, [askAgentLabel(ask)]),
      h('span', { style: 'color:var(--muted)' }, ['asks:']),
    ]),
    h('div', { className: 'qd-ask-row-question' }, [ask.question]),
    // R-19.1: optional muted rationale under the question.
    ...(ask.detail ? [h('div', { className: 'qd-ask-row-detail' }, [ask.detail])] : []),
    h('div', { className: 'qd-ask-row-actions' }, [
      ...options,
      input,
      h(
        'button',
        {
          className: 'qd-ask-row-dismiss',
          type: 'button',
          title: 'Dismiss',
          onclick: () => send('', 'dismissed'),
        },
        ['Dismiss'],
      ),
    ]),
  ]);
}

/** SPEC §16 (R-16.2): a pending permission request mirrored as an amber row in
 * the popup, with Allow / Deny / In terminal. Clicking the row (off a button)
 * re-surfaces the ask window, same as the ask mirror. */
const answeredPerms = new Set<string>();
function renderPermMirrorRow(perm: PermRow): HTMLElement {
  const decide = (decision: PermDecision): void => {
    if (answeredPerms.has(perm.id)) return;
    answeredPerms.add(perm.id);
    void invoke('answer_perm', { permId: perm.id, decision }).catch(() => {
      answeredPerms.delete(perm.id);
    });
  };
  const agent = perm.project ?? (perm.context ? `Unknown agent (${truncate(perm.context, 32)})` : 'Unknown agent');
  // R-32.1: mirror the ask window — past the deadline Allow/Deny are disabled
  // (the hook has given up); "In terminal" stays live.
  const expired = perm.expiresAt !== undefined && Date.now() >= perm.expiresAt;
  const allow = h('button', { className: 'qd-btn qd-btn-primary qd-perm-row-allow', type: 'button', onclick: () => decide('allow') }, ['Allow']) as HTMLButtonElement;
  const deny = h('button', { className: 'qd-btn qd-perm-row-deny', type: 'button', onclick: () => decide('deny') }, ['Deny']) as HTMLButtonElement;
  if (expired) {
    allow.disabled = true;
    deny.disabled = true;
  }
  return h('div', { className: 'qd-ask-row qd-perm-row', onclick: reopenAskWindowUnlessInteractive }, [
    h('div', { className: 'qd-ask-row-head' }, [
      h('span', { className: 'qd-ask-row-agent' }, [agent]),
      h('span', { style: 'color:var(--muted)' }, [expired ? 'expired' : 'requests permission']),
    ]),
    h('div', { className: 'qd-ask-row-question qd-perm-row-tool' }, [`Run ${perm.toolName}?`]),
    h('div', { className: 'qd-ask-row-actions' }, [
      allow,
      deny,
      h('button', { className: 'qd-btn qd-ask-row-dismiss', type: 'button', title: 'Answer in the terminal instead', onclick: () => decide('defer') }, ['In terminal']),
    ]),
  ]);
}

function renderHooksBanner(): HTMLElement {
  return h('div', { className: 'qd-banner' }, [
    h('span', { className: 'qd-banner-text' }, ['Hooks not installed — sessions won’t be detected.']),
    h(
      'button',
      {
        className: 'qd-btn qd-btn-primary',
        type: 'button',
        disabled: installBusy,
        onclick: () => void doInstallHooks(),
      },
      [installBusy ? 'Installing…' : 'Install hooks'],
    ),
  ]);
}

async function doInstallHooks(): Promise<void> {
  installBusy = true;
  installError = null;
  renderAll();
  try {
    await invoke('install_hooks', undefined);
  } catch (err) {
    installError = err instanceof Error ? err.message : String(err);
  } finally {
    installBusy = false;
    renderAll();
  }
}

function setLaunchAtLogin(value: boolean): void {
  lastLaunchChoice = value;
  void invoke('set_setting', { key: 'launchAtLogin', value });
  renderAll();
}

async function doUninstallHooks(): Promise<void> {
  uninstallBusy = true;
  renderAll();
  try {
    await invoke('uninstall_hooks', undefined);
  } catch (err) {
    installError = err instanceof Error ? err.message : String(err);
  } finally {
    uninstallBusy = false;
    renderAll();
  }
}

function renderEmptyState(hooksInstalled: boolean): HTMLElement {
  return h('div', { className: 'qd-empty' }, [
    h('p', { className: 'qd-empty-title' }, ['No Claude Code sessions yet — start ', h('code', {}, ['claude']), ' in any terminal.']),
    h(
      'p',
      { className: 'qd-empty-health' },
      [hooksInstalled ? 'Hooks installed — waiting for a session.' : 'Hooks not installed — install them below to start monitoring.'],
    ),
  ]);
}

function onboardingStepButton(label: string, onClick: () => void, active: boolean, disabled = false): HTMLElement {
  return h(
    'button',
    {
      className: `qd-btn${active ? ' qd-btn-primary' : ''}`,
      type: 'button',
      disabled,
      onclick: onClick,
    },
    [label],
  );
}

function renderOnboarding(settings: SettingsState, hooksInstalled: boolean): HTMLElement {
  const finish = (): void => {
    void invoke('set_setting', { key: 'onboardingDone', value: true });
  };

  return h('div', { className: 'qd-onboarding' }, [
    h('div', { className: 'qd-onboarding-title' }, ['Welcome aboard']),
    h(
      'p',
      { className: 'qd-onboarding-body' },
      [
        'Quarterdeck watches Claude Code sessions through a small hook script it installs into ',
        h('code', {}, ['~/.claude/settings.json']),
        '. Nothing changes until you say so.',
      ],
    ),
    h('div', { className: 'qd-onboarding-step' }, [
      h('span', { className: 'qd-onboarding-step-label' }, ['Install hooks so sessions show up here']),
      onboardingStepButton(hooksInstalled ? 'Installed' : installBusy ? 'Installing…' : 'Install hooks', () => void doInstallHooks(), hooksInstalled, hooksInstalled || installBusy),
    ]),
    h('div', { className: 'qd-onboarding-step' }, [
      h('span', { className: 'qd-onboarding-step-label' }, ['Launch Quarterdeck at login?']),
      h('div', { className: 'qd-onboarding-actions' }, [
        onboardingStepButton('Yes', () => setLaunchAtLogin(true), lastLaunchChoice === true),
        onboardingStepButton('No', () => setLaunchAtLogin(false), lastLaunchChoice === false),
      ]),
    ]),
    h('div', { className: 'qd-onboarding-step' }, [
      h('span', { className: 'qd-onboarding-step-label' }, ['Let agents ask you questions (MCP)']),
      onboardingStepButton(settings.mcpEnabled ? 'Enabled' : 'Enable agent questions', () => void invoke('set_setting', { key: 'mcpEnabled', value: true }), settings.mcpEnabled, settings.mcpEnabled),
    ]),
    // SPEC R-16.4 / R-25.4: default-on consent line for taking over Claude Code
    // permission prompts into the deck (Allow/Deny without alt-tabbing).
    h('div', { className: 'qd-onboarding-step qd-onboarding-takeover' }, [
      h('span', { className: 'qd-onboarding-step-label' }, ['Take over permission prompts (Allow/Deny from the deck)']),
      onboardingStepButton(
        settings.takeoverPermissions ? 'On' : 'Off',
        () => void invoke('set_setting', { key: 'takeoverPermissions', value: !settings.takeoverPermissions }),
        settings.takeoverPermissions,
      ),
    ]),
    h('p', { className: 'qd-onboarding-hint' }, [
      'Claude Code will ask permission here instead of blocking the terminal — you can still answer “In terminal” any time.',
    ]),
    // R-25.4 closing tip line.
    h('p', { className: 'qd-onboarding-tip' }, ['Pin the window (📌) and click ◱ to shrink it to a traffic light.']),
    h('div', { className: 'qd-onboarding-footer' }, [
      h('button', { className: 'qd-btn qd-btn-primary', type: 'button', onclick: finish }, ['Continue']),
    ]),
  ]);
}

function renderGearIssue(hooksInstalled: boolean): void {
  elGear.classList.toggle('has-issue', !hooksInstalled);
}

/** SPEC R-14.2: reflects the persisted pin state on the header icon (filled +
 * clay accent when pinned). The click handler below sends the toggle; the
 * shell is the one deciding always-on-top/hide-on-blur (R-3.4), this only
 * mirrors what came back on the last snapshot. */
function renderPinState(pinned: boolean): void {
  elPin.classList.toggle('pinned', pinned);
  elPin.setAttribute('aria-pressed', String(pinned));
  elPin.title = pinned ? 'Unpin' : 'Pin on top';
}

/** SPEC R-25.2: the collapse-to-lamp button only shows while pinned (lamp mode
 * is only reachable from a pinned popup). */
function renderCollapseVisibility(pinned: boolean): void {
  elCollapse.hidden = !pinned;
}

const SVG_NS = 'http://www.w3.org/2000/svg';
/** Radius of the lamp pie in the 0..100 viewBox (a hair short of the edge so
 * the working-wedge pulse doesn't clip). */
const LAMP_PIE_R = 48;

/** §41 wedge path: the `index`-th of `total` equal slices, drawn clockwise from
 * 12 o'clock. Each slice spans ≤180° for `total` ≥ 2, so the arc's large-arc
 * flag is always 0 (`total === 1` is a full circle, handled separately). */
function lampWedgePath(index: number, total: number): string {
  const a0 = (index / total) * 2 * Math.PI - Math.PI / 2;
  const a1 = ((index + 1) / total) * 2 * Math.PI - Math.PI / 2;
  const x0 = (50 + LAMP_PIE_R * Math.cos(a0)).toFixed(3);
  const y0 = (50 + LAMP_PIE_R * Math.sin(a0)).toFixed(3);
  const x1 = (50 + LAMP_PIE_R * Math.cos(a1)).toFixed(3);
  const y1 = (50 + LAMP_PIE_R * Math.sin(a1)).toFixed(3);
  return `M 50 50 L ${x0} ${y0} A ${LAMP_PIE_R} ${LAMP_PIE_R} 0 0 1 ${x1} ${y1} Z`;
}

/** SPEC R-25.1/R-25.3 (§41): renders the lamp as a per-agent pie — one equal
 * wedge per session, each filled by that session's own status color (the same
 * status→token mapping as the row dots, incl. the §43 blue), an attention-count
 * badge when > 0, and a hover tooltip carrying the popup footer's counts line.
 * Zero agents fall back to a neutral gray ring. */
function renderLamp(snap: StateSnapshot): void {
  clear(elLampPie);
  const sessions = snap.sessions;
  const n = sessions.length;
  if (n === 0) {
    const ring = document.createElementNS(SVG_NS, 'circle');
    ring.setAttribute('cx', '50');
    ring.setAttribute('cy', '50');
    ring.setAttribute('r', String(LAMP_PIE_R));
    ring.setAttribute('class', 'qd-lamp-ring');
    elLampPie.append(ring);
  } else if (n === 1) {
    // A single agent fills the whole disc — a wedge path would degenerate.
    const disc = document.createElementNS(SVG_NS, 'circle');
    disc.setAttribute('cx', '50');
    disc.setAttribute('cy', '50');
    disc.setAttribute('r', String(LAMP_PIE_R));
    disc.setAttribute('class', 'qd-lamp-wedge');
    disc.setAttribute('data-status', sessions[0].status);
    elLampPie.append(disc);
  } else {
    sessions.forEach((session, i) => {
      const wedge = document.createElementNS(SVG_NS, 'path');
      wedge.setAttribute('d', lampWedgePath(i, n));
      wedge.setAttribute('class', 'qd-lamp-wedge');
      wedge.setAttribute('data-status', session.status);
      elLampPie.append(wedge);
    });
  }
  const attention = snap.counts.attention;
  elLampBadge.hidden = attention <= 0;
  elLampBadge.textContent = attention > 0 ? String(attention) : '';
  const tip = footerText(snap.counts);
  elLamp.title = tip.length > 0 ? tip : 'No sessions';
}

/** In-progress state of a mirrored ask-row free-text field, captured before a
 * rebuild so a `deck://state` push from an unrelated session can't wipe it. */
interface PreservedAskInput {
  value: string;
  focused: boolean;
  selStart: number | null;
  selEnd: number | null;
}

/** Snapshot every ask-row input's value + focus/selection, keyed by ask id, so
 * they survive the full `elContent` rebuild (R-8: the mirrored ask input is an
 * interactive surface; an unrelated session's state change must not clear it). */
function captureAskInputs(): Map<string, PreservedAskInput> {
  const active = document.activeElement;
  const preserved = new Map<string, PreservedAskInput>();
  for (const el of elContent.querySelectorAll<HTMLInputElement>('.qd-ask-row-input')) {
    const id = el.getAttribute('data-ask-id');
    if (!id) continue;
    preserved.set(id, {
      value: el.value,
      focused: el === active,
      selStart: el.selectionStart,
      selEnd: el.selectionEnd,
    });
  }
  return preserved;
}

/** Restore captured ask-row input state onto the freshly-rebuilt inputs. */
function restoreAskInputs(preserved: Map<string, PreservedAskInput>): void {
  if (preserved.size === 0) return;
  for (const el of elContent.querySelectorAll<HTMLInputElement>('.qd-ask-row-input')) {
    const id = el.getAttribute('data-ask-id');
    const prior = id ? preserved.get(id) : undefined;
    if (!prior) continue;
    el.value = prior.value;
    if (prior.focused) {
      el.focus();
      try {
        el.setSelectionRange(prior.selStart ?? prior.value.length, prior.selEnd ?? prior.value.length);
      } catch {
        /* setSelectionRange throws on some input types; value is already restored. */
      }
    }
  }
}

function renderContent(snap: StateSnapshot): void {
  const preservedAsks = captureAskInputs();
  captureRenameInput();
  // Removing the focused rename input below fires a synchronous blur; guard it
  // so the teardown doesn't commit the edit (R-27.5 editor-survives-rebuild).
  suppressRenameBlur = true;
  clear(elContent);
  suppressRenameBlur = false;
  timeTicks = [];
  const settings = snap.settings;
  const perms = snap.perms ?? [];
  const onboardingActive = settings ? settings.onboardingDone === false : false;
  const hasRows = snap.sessions.length > 0 || snap.asks.length > 0 || perms.length > 0;

  // R-10.2: the one-time first-run onboarding card must NOT coexist with a
  // populated session list. Hooks install into the shared, machine-wide
  // `~/.claude/settings.json` while `onboardingDone` is per-data-dir, so a
  // reinstall / new data dir can leave onboarding incomplete while sessions
  // already flow. Stacking "Install hooks so sessions show up here" above a
  // live, scrolling list both contradicts itself and fights the list for space.
  // When sessions exist, hooks are obviously working: show the list (the gear
  // issue-dot + hooks banner still guide any remaining setup); reserve the card
  // for a genuine empty first run.
  if (onboardingActive && !hasRows) {
    elContent.append(renderOnboarding(settings as SettingsState, snap.hooksInstalled));
    if (installError) {
      elContent.append(h('div', { className: 'qd-banner-error' }, [installError]));
    }
    elFooter.style.display = 'none';
    return;
  }

  if (!snap.hooksInstalled) {
    elContent.append(renderHooksBanner());
  }

  if (installError) {
    elContent.append(h('div', { className: 'qd-banner-error' }, [installError]));
  }

  if (!hasRows) {
    elContent.append(renderEmptyState(snap.hooksInstalled));
    elFooter.style.display = 'none';
    return;
  }

  elFooter.style.display = '';

  const rows = h('div', { className: 'qd-rows' }, []);
  // R-16.2: perms mirror above asks (they block a terminal; asks queue behind).
  for (const perm of perms) {
    rows.append(renderPermMirrorRow(perm));
  }
  for (const ask of snap.asks) {
    rows.append(renderAskMirrorRow(ask));
  }
  // R-23.5/R-23.6: token usage renders only when `showTokenStats` is on (the
  // backend also omits the fields when off; gating here too keeps the toggle
  // authoritative even against a stale row that still carries them).
  const showTokens = settings?.showTokenStats !== false;
  // R-3.4/R-7.3: `snap.sessions` already arrives in the engine's canonical
  // R-7.3 order (`SessionStore::view`); render it as-is, never re-sort here.
  for (const row of snap.sessions) {
    rows.append(renderSessionRow(row, showTokens));
  }
  elContent.append(rows);
  restoreAskInputs(preservedAsks);
  restoreRenameInput();
}

function renderFooter(counts: StateSnapshot['counts']): void {
  const text = footerText(counts);
  elFooter.textContent = text.length > 0 ? text : ' ';
}

/** Keys of the boolean settings driven by a `toggleControl`. */
type ToggleKey =
  | 'notifyIdle'
  | 'notifyAttention'
  | 'notifyReminder'
  | 'launchAtLogin'
  | 'takeoverPermissions'
  | 'showTokenStats';

/** Optimistic per-toggle state while the user's clicks are outrunning the
 * backend. `value` is the latest value the user's clicks asked for; `inFlight`
 * counts `set_setting` calls not yet resolved. While `inFlight > 0` this value
 * is authoritative for the control (over the server snapshot); once every one of
 * our own writes has resolved the backend provably reflects our last write, so
 * the entry is dropped and the server snapshot takes over again. This keeps N
 * rapid clicks == N flips even when the round-trip can't keep up — worst on
 * "Launch at login", whose real OS autostart I/O makes `set_setting` slow, so a
 * value captured at render time goes stale and repeated clicks would otherwise
 * compute the same next value and diverge from the click count. */
const pendingToggles = new Map<ToggleKey, { value: boolean; inFlight: number }>();

function toggleShownValue(key: ToggleKey, serverValue: boolean): boolean {
  const pending = pendingToggles.get(key);
  return pending ? pending.value : serverValue;
}

function toggleControl(key: ToggleKey, label: string, serverValue: boolean): HTMLElement {
  const btn = h('button', {
    className: 'qd-toggle',
    type: 'button',
    role: 'switch',
    'aria-checked': String(toggleShownValue(key, serverValue)),
    onclick: () => {
      const next = !toggleShownValue(key, serverValue);
      const pending = pendingToggles.get(key) ?? { value: next, inFlight: 0 };
      pending.value = next;
      pending.inFlight += 1;
      pendingToggles.set(key, pending);
      btn.setAttribute('aria-checked', String(next));
      // Hold the optimistic value authoritative until THIS write resolves; only
      // when no writes remain in flight do we let the server snapshot take over
      // (it then reflects our last write). `set_setting` resolves after the
      // backend has persisted + pushed, so this never hands back to a stale value.
      void invoke('set_setting', { key, value: next }).finally(() => {
        pending.inFlight -= 1;
        if (pending.inFlight <= 0) pendingToggles.delete(key);
      });
    },
  }) as HTMLButtonElement;
  return h('div', { className: 'qd-toggle-row' }, [h('span', { className: 'qd-toggle-label' }, [label]), btn]);
}

/** R-8.6: when the `claude` CLI isn't on PATH, show the exact `claude mcp add …`
 * command (with the real port + token) for the user to run by hand. */
function renderMcpCommandFallback(settings: SettingsState): HTMLElement | null {
  if (!settings.mcpEnabled || settings.mcpCliAvailable || !settings.mcpCommand) return null;
  const command = settings.mcpCommand;
  return h('div', { className: 'qd-mcp-command' }, [
    h('p', { className: 'qd-empty-health', style: 'color:var(--muted);margin:8px 0 6px' }, [
      'The claude CLI wasn’t found on your PATH. Run this command to finish setup:',
    ]),
    h('div', { className: 'qd-mcp-command-box' }, [
      h('code', { className: 'mono qd-mcp-command-text' }, [command]),
      h(
        'button',
        {
          className: 'qd-btn',
          type: 'button',
          onclick: () => void navigator.clipboard?.writeText(command).catch(() => undefined),
        },
        ['Copy'],
      ),
    ]),
  ]);
}

function renderSettings(snap: StateSnapshot): void {
  clear(elSettings);
  const settings: SettingsState =
    snap.settings ?? {
      notifyIdle: true,
      notifyAttention: true,
      notifyReminder: false,
      launchAtLogin: false,
      onboardingDone: true,
      popupPinned: false,
      takeoverPermissions: true,
      showTokenStats: true,
      popupMode: 'list',
      mcpEnabled: false,
      mcpCliAvailable: true,
      dataDir: '',
      version: '',
    };

  const set = (key: keyof SettingsState, value: boolean | string): void => {
    void invoke('set_setting', { key, value });
  };

  elSettings.append(
    h('div', { className: 'qd-settings-header' }, [
      h(
        'button',
        {
          className: 'qd-back',
          type: 'button',
          'aria-label': 'Back',
          onclick: () => {
            setSettingsOpen(false);
          },
        },
        ['←'],
      ),
      h('span', { className: 'qd-settings-title' }, ['Settings']),
    ]),
    h('div', { className: 'qd-settings-body' }, [
      h('div', { className: 'qd-settings-section' }, [
        h('p', { className: 'qd-settings-section-title' }, ['Notifications']),
        toggleControl('notifyIdle', 'Notify when a session finishes', settings.notifyIdle),
        toggleControl('notifyAttention', 'Notify when a session needs you', settings.notifyAttention),
        toggleControl('notifyReminder', 'Remind me if a session is still waiting', settings.notifyReminder),
      ]),
      h('div', { className: 'qd-settings-section' }, [
        h('p', { className: 'qd-settings-section-title' }, ['General']),
        toggleControl('launchAtLogin', 'Launch Quarterdeck at login', settings.launchAtLogin),
        // SPEC R-23.5: the token-stats toggle (default on).
        toggleControl('showTokenStats', 'Show token usage on rows', settings.showTokenStats),
      ]),
      h('div', { className: 'qd-settings-section' }, [
        h('p', { className: 'qd-settings-section-title' }, ['Permissions']),
        h(
          'p',
          { className: 'qd-empty-health', style: 'color:var(--muted);margin:0 0 8px' },
          ['Show Claude Code permission prompts here so you can Allow or Deny without switching to the terminal.'],
        ),
        toggleControl('takeoverPermissions', 'Take over permission prompts', settings.takeoverPermissions),
      ]),
      h('div', { className: 'qd-settings-section' }, [
        h('p', { className: 'qd-settings-section-title' }, ['Hooks']),
        h('div', { className: 'qd-settings-row' }, [
          h('span', { className: 'qd-toggle-label' }, [snap.hooksInstalled ? 'Hooks are installed' : 'Hooks are not installed']),
          h(
            'button',
            { className: 'qd-btn qd-btn-primary', type: 'button', disabled: installBusy, onclick: () => void doInstallHooks() },
            [installBusy ? 'Installing…' : snap.hooksInstalled ? 'Repair hooks' : 'Install hooks'],
          ),
        ]),
        h('div', { className: 'qd-settings-row' }, [
          h('span', { className: 'qd-toggle-label' }, ['Remove the Quarterdeck hook entries']),
          h(
            'button',
            {
              className: 'qd-btn',
              type: 'button',
              disabled: !snap.hooksInstalled || uninstallBusy,
              onclick: () => void doUninstallHooks(),
            },
            [uninstallBusy ? 'Removing…' : 'Uninstall hooks'],
          ),
        ]),
        installError ? h('div', { className: 'qd-banner-error' }, [installError]) : null,
      ]),
      h('div', { className: 'qd-settings-section' }, [
        h('p', { className: 'qd-settings-section-title' }, ['Agent questions']),
        h(
          'p',
          { className: 'qd-empty-health', style: 'color:var(--muted);margin:0 0 8px' },
          ['Lets a Claude Code agent ask you a question through an always-on-top window.'],
        ),
        h('div', { className: 'qd-settings-row' }, [
          h('span', { className: 'qd-toggle-label' }, [settings.mcpEnabled ? 'Agent questions are enabled' : 'Agent questions are disabled']),
          h(
            'button',
            { className: 'qd-btn', type: 'button', onclick: () => set('mcpEnabled', !settings.mcpEnabled) },
            [settings.mcpEnabled ? 'Disable agent questions' : 'Enable agent questions'],
          ),
        ]),
        renderMcpCommandFallback(settings),
      ]),
      h('div', { className: 'qd-settings-section' }, [
        h('p', { className: 'qd-settings-section-title' }, ['About']),
        h('div', { className: 'qd-settings-meta' }, [h('span', {}, ['Data directory']), h('span', { className: 'mono' }, [settings.dataDir])]),
        h('div', { className: 'qd-settings-meta' }, [h('span', {}, ['Version']), h('span', { className: 'mono' }, [settings.version])]),
      ]),
    ]),
  );
}

function renderAll(): void {
  if (!latest) return;

  // R-25.3 "Blur/Esc never hide the lamp": the injected Esc-hide script in
  // `windows.rs` (a plain string, no Rust state to read from) checks this
  // global mirror before hiding — the actual mode decision still lives in
  // Rust (R-3.4); this is only a display echo of it.
  const popupMode = latest.settings?.popupMode ?? 'list';
  (window as unknown as { __qdPopupMode?: string }).__qdPopupMode = popupMode;

  const lampMode = popupMode === 'lamp';
  elApp.classList.toggle('qd-app-lamp', lampMode);
  if (lampMode) {
    // The gear is hidden in lamp mode (no way to reach settings) — make sure a
    // pane left open from before collapsing doesn't render on top of it, and a
    // settings resize tween mid-flight doesn't keep resizing the fixed square.
    if (resizeTweenRaf !== null) {
      cancelAnimationFrame(resizeTweenRaf);
      resizeTweenRaf = null;
    }
    if (settingsOpen) {
      settingsOpen = false;
      elSettings.classList.remove('open');
    }
    renderLamp(latest);
    return; // R-25.1: a fixed-size square has nothing else to lay out/measure.
  }

  renderWatchline(latest.counts);
  renderContent(latest);
  renderFooter(latest.counts);
  renderGearIssue(latest.hooksInstalled);
  renderPinState(latest.settings?.popupPinned ?? false);
  renderCollapseVisibility(latest.settings?.popupPinned ?? false);
  if (settingsOpen) renderSettings(latest);
  syncPopupHeight();
}

/** R-14.3 true auto-height: report the intrinsic content height so the shell
 * can size the window (clamped to 160..=560 in Rust — the v1.0 460 floor is
 * removed). Reported (and recorded by the mock, for Playwright's R-14.3
 * shrink regression spec) even in mock/browser mode, since there's no window
 * to size there but the number itself is still test-observable; skipped
 * while the settings overlay is open (R-31.2 sizes the window to a fixed
 * 5-row height instead) or while a settings tween is still in flight. */
function syncPopupHeight(): void {
  if (settingsOpen) return;
  // Don't fight an in-flight settings-close tween: it lands on the correct
  // auto-height itself, and the next snapshot resumes normal sizing.
  if (resizeTweenRaf !== null) return;
  reportPopupHeight(measureAutoHeight());
}

/** Reports `contentHeight` to the shell and records it so the next tween can
 * start from the height the window is actually at. */
function reportPopupHeight(total: number): void {
  lastReportedHeight = total;
  void invoke('resize_popup', { contentHeight: total }).catch(() => undefined);
}

/** The intrinsic auto-height: `header + true content height + footer`. */
function measureAutoHeight(): number {
  const appEl = document.getElementById('app') as HTMLElement | null;
  const header = document.querySelector('.qd-header') as HTMLElement | null;

  // `#app` is normally stretched to fill the current window height (`height:
  // 100%`, capped at `max-height: 560px`) — that's the actual OS window in
  // the real app, or just the browser viewport in mock/browser mode. Either
  // way, `.qd-content` (and, inside it, `.qd-empty`'s own centering flex box)
  // is `flex: 1 1 auto`, so it STRETCHES to fill that already-stretched
  // ancestor, making its `scrollHeight` report the ambient box size rather
  // than its true intrinsic content size. That's invisible while content is
  // GROWING past the box (scrollHeight still reports the larger real size),
  // but it silently masks SHRINKING: once rows disappear the box is still
  // whatever size it last was, and every flex-grow child just re-stretches to
  // fill it — so the reported height would never shrink back down (R-14.3
  // regression: "50 rows → 0"). Momentarily letting `#app` size to its own
  // content removes that ambient stretch for every descendant, so
  // `elContent.scrollHeight` reflects the *true* intrinsic content height;
  // restoring right after is a same-tick style read+write, so it never
  // paints an intermediate frame.
  const prevHeight = appEl?.style.height ?? '';
  const prevMaxHeight = appEl?.style.maxHeight ?? '';
  if (appEl) {
    appEl.style.height = 'auto';
    appEl.style.maxHeight = 'none';
  }
  const headerH = header?.offsetHeight ?? 0;
  const contentH = elContent.scrollHeight;
  const footerH = elFooter.style.display === 'none' ? 0 : elFooter.offsetHeight;
  if (appEl) {
    appEl.style.height = prevHeight;
    appEl.style.maxHeight = prevMaxHeight;
  }

  return headerH + contentH + footerH;
}

/** R-31.2 fixed settings height: `header + 5 rows + footer`. The header is
 * measured (its height is stable across scenarios); the rows and footer are
 * nominal constants so the pane is byte-identical at 0/2/8 sessions. */
function settingsFixedHeight(): number {
  const header = document.querySelector('.qd-header') as HTMLElement | null;
  const headerH = header?.offsetHeight ?? 0;
  return headerH + SETTINGS_ROW_COUNT * SETTINGS_ROW_H + SETTINGS_FOOTER_H;
}

/** Opens/closes the settings overlay and animates the window between its list
 * auto-height and the fixed 5-row settings height (R-31.2). Kept in one place
 * so the gear, the Back button, and Esc all drive the same resize. */
function setSettingsOpen(open: boolean): void {
  if (settingsOpen === open) return;
  settingsOpen = open;
  if (open && latest) renderSettings(latest);
  elSettings.classList.toggle('open', open);
  tweenPopupHeight(open ? settingsFixedHeight() : measureAutoHeight());
}

/** Animates the reported window height from its current value to `target` by
 * driving `resize_popup` once per frame (R-31.2). Snaps instantly under
 * reduced motion, on the first-ever report, or a negligible delta. */
function tweenPopupHeight(target: number): void {
  if (resizeTweenRaf !== null) {
    cancelAnimationFrame(resizeTweenRaf);
    resizeTweenRaf = null;
  }
  const from = lastReportedHeight;
  const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  if (reduce || from === null || Math.abs(target - from) < 1) {
    reportPopupHeight(target);
    return;
  }
  const start = performance.now();
  const step = (now: number): void => {
    const t = Math.min(1, (now - start) / SETTINGS_RESIZE_MS);
    // easeInOutQuad
    const eased = t < 0.5 ? 2 * t * t : 1 - (-2 * t + 2) ** 2 / 2;
    reportPopupHeight(Math.round(from + (target - from) * eased));
    resizeTweenRaf = t < 1 ? requestAnimationFrame(step) : null;
  };
  resizeTweenRaf = requestAnimationFrame(step);
}

elGear.addEventListener('click', () => {
  setSettingsOpen(!settingsOpen);
});

// SPEC R-14.2: toggles the persisted pin state; the shell applies
// always-on-top + disables hide-on-blur (R-3.4 keeps that logic in Rust).
elPin.addEventListener('click', () => {
  const next = !(latest?.settings?.popupPinned ?? false);
  void invoke('set_setting', { key: 'popupPinned', value: next });
});

// SPEC R-25.2: collapse to the lamp (only visible while pinned).
elCollapse.addEventListener('click', () => {
  void invoke('set_setting', { key: 'popupMode', value: 'lamp' });
});

function expandFromLamp(): void {
  void invoke('set_setting', { key: 'popupMode', value: 'list' });
}

function unpinFromLamp(): void {
  void invoke('set_setting', { key: 'popupPinned', value: false });
}

/** Pixels of pointer movement before a press-and-move on the lamp counts as a
 * drag rather than a click (SPEC R-25.1 "drag vs click discrimination"). */
const LAMP_DRAG_THRESHOLD_PX = 4;

// SPEC R-25.1: the lamp is a single clickable element (a real <button>, so
// Tauri's own `data-tauri-drag-region` mousedown handler never fires on it —
// see the `startDragging` doc in `ipc-client.ts`). A plain click (no
// meaningful movement) expands back to the list (R-25.2); movement past the
// threshold starts a native window drag instead, covering "drag anywhere".
elLamp.addEventListener('pointerdown', (ev: PointerEvent) => {
  if (ev.button !== 0) return;
  const startX = ev.clientX;
  const startY = ev.clientY;
  let dragging = false;
  const onMove = (mv: PointerEvent): void => {
    if (dragging) return;
    if (Math.hypot(mv.clientX - startX, mv.clientY - startY) > LAMP_DRAG_THRESHOLD_PX) {
      dragging = true;
      cleanup();
      startDragging();
    }
  };
  const onUp = (): void => {
    cleanup();
    if (!dragging) expandFromLamp();
  };
  const cleanup = (): void => {
    window.removeEventListener('pointermove', onMove);
    window.removeEventListener('pointerup', onUp);
  };
  window.addEventListener('pointermove', onMove);
  window.addEventListener('pointerup', onUp);
});

// SPEC R-25.2 "unpin-from-lamp path": the header (and its pin button) is
// hidden while collapsed, so a right-click menu is the way back out without
// first expanding.
elLamp.addEventListener('contextmenu', (ev: MouseEvent) => {
  ev.preventDefault();
  closeCtxMenu();
  const menu = h('div', { className: 'qd-ctx-menu', style: `left:${ev.clientX}px;top:${ev.clientY}px` }, [
    h(
      'button',
      {
        className: 'qd-ctx-item',
        type: 'button',
        onclick: (e: Event) => {
          e.stopPropagation();
          unpinFromLamp();
          closeCtxMenu();
        },
      },
      ['Unpin'],
    ),
    h(
      'button',
      {
        className: 'qd-ctx-item',
        type: 'button',
        onclick: (e: Event) => {
          e.stopPropagation();
          expandFromLamp();
          closeCtxMenu();
        },
      },
      ['Expand to list'],
    ),
  ]);
  document.body.append(menu);
  ctxMenuEl = menu;
});

document.addEventListener('keydown', (ev) => {
  if (ev.key === 'Escape') {
    if (settingsOpen) {
      setSettingsOpen(false);
    } else if (!usingMock) {
      // Real app: Esc hides the popup window (R-7.1). Left to the shell via a
      // window-level listener in a Tauri build; nothing to do in mock mode.
    }
  }
});

onState((snap) => {
  latest = snap;
  receivedAtPerf = performance.now();
  renderAll();
});

setInterval(() => {
  if (ctxMenuEl) return; // don't yank a right-click menu out from under the user
  const elapsed = performance.now() - receivedAtPerf;
  for (const tick of timeTicks) {
    tick.el.textContent = tick.prefix + formatDuration(tick.base + elapsed);
  }
}, 1000);

if (usingMock) {
  installMockScenarioSwitcher();
  // Screenshot convenience only: `?openSettings=1` opens the pane on load.
  if (new URLSearchParams(location.search).get('openSettings') === '1') {
    elGear.click();
  }
}
