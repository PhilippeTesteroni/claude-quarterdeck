/**
 * Popup window controller (SPEC §7). Snapshot-driven: the shell (or the mock)
 * pushes full `StateSnapshot`s; this file only renders + sends intent back
 * through `ipc-client.ts` commands. No business logic lives here (R-3.4).
 */

import { invoke, onState, startDragging, usingMock } from './ipc-client';
import type { AskAnswerKind, AskRow, PermDecision, PermRow, SessionRow, SessionStatus, SettingsState, StateSnapshot } from './ipc-contract';
import { footerText, formatAge, formatDuration, truncate, worstStatus } from './format';
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
const elLampDot = document.getElementById('qd-lamp-dot') as HTMLElement;
const elLampBadge = document.getElementById('qd-lamp-badge') as HTMLElement;

let latest: StateSnapshot | null = null;
let receivedAtPerf = 0;
let settingsOpen = false;
let ctxMenuEl: HTMLElement | null = null;
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

const WATCHLINE_ORDER: SessionStatus[] = ['attention', 'working', 'idle', 'dead'];
const watchlineSegs = new Map<string, HTMLElement>();
for (const status of WATCHLINE_ORDER) {
  const seg = h('div', { className: 'qd-watchline-seg', 'data-status': status });
  watchlineSegs.set(status, seg);
  elWatchline.append(seg);
}
const watchlineNone = h('div', { className: 'qd-watchline-seg', 'data-status': 'none', style: 'flex-basis:0%' });
elWatchline.append(watchlineNone);

function renderWatchline(counts: StateSnapshot['counts']): void {
  const total = counts.attention + counts.working + counts.idle + counts.dead;
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
    // SPEC R-15.4: "Focus terminal" is the first context-menu item.
    h(
      'button',
      {
        className: 'qd-ctx-item',
        type: 'button',
        onclick: (ev: Event) => {
          ev.stopPropagation();
          focusTerminal(row.id);
          closeCtxMenu();
        },
      },
      ['Focus terminal'],
    ),
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
  ]);
  document.body.append(menu);
  const rect = menu.getBoundingClientRect();
  const maxX = window.innerWidth - rect.width - 4;
  const maxY = window.innerHeight - rect.height - 4;
  menu.style.left = `${Math.min(x, Math.max(4, maxX))}px`;
  menu.style.top = `${Math.min(y, Math.max(4, maxY))}px`;
  ctxMenuEl = menu;
}

/** SPEC R-15.4b: a transient inline notice shown in the popup when the terminal
 * window couldn't be focused ("toast-in-window"). Auto-dismisses. */
let focusNoticeEl: HTMLElement | null = null;
let focusNoticeTimer: ReturnType<typeof setTimeout> | null = null;

function showFocusNotice(message: string): void {
  focusNoticeEl?.remove();
  if (focusNoticeTimer) clearTimeout(focusNoticeTimer);
  const el = h(
    'div',
    {
      className: 'qd-focus-notice',
      style:
        'position:fixed;left:50%;bottom:12px;transform:translateX(-50%);z-index:50;' +
        'max-width:92%;padding:6px 12px;border-radius:6px;font-size:12px;' +
        'background:var(--surface,#161b22);color:var(--text,#e6edf3);' +
        'border:1px solid var(--border,#30363d);box-shadow:0 4px 14px rgba(0,0,0,.35);',
    },
    [message],
  );
  document.body.append(el);
  focusNoticeEl = el;
  focusNoticeTimer = setTimeout(() => {
    el.remove();
    if (focusNoticeEl === el) focusNoticeEl = null;
  }, 2600);
}

/** SPEC R-15.4: focus the terminal hosting a session. On failure the shell
 * rejects with "Couldn't find the terminal window", shown inline (R-15.4b). */
function focusTerminal(sessionId: string): void {
  void invoke('focus_terminal', { sessionId }).catch((err) => {
    showFocusNotice(err instanceof Error ? err.message : String(err));
  });
}

function renderSessionRow(row: SessionRow, showTokens: boolean): HTMLElement {
  // R-22.4: a seeded (estimated) time renders with the inferred `~` convention
  // (e.g. `~12m 40s`) until an exact hook event arrives (R-22.2 clears it).
  const timePrefix = row.inferred ? '~' : '';
  const timeEl = h('span', { className: `qd-row-time mono${row.inferred ? ' estimated' : ''}` }, [
    timePrefix + formatDuration(row.sinceMs),
  ]);
  timeTicks.push({ el: timeEl, base: row.sinceMs, prefix: timePrefix });

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

  // R-21.2 / R-23.3: a compact `⛭ N` badge while background subagents are
  // running, with the combined subagent spend appended (`⛭ 3 · 2.1M`) when known.
  const subagents = row.subagents ?? 0;
  const subagentSpend = showTokens ? row.subagentSpend : undefined;
  const badgeText =
    subagentSpend != null && subagentSpend !== ''
      ? `⛭ ${subagents} · ${subagentSpend}`
      : `⛭ ${subagents}`;
  const badge =
    subagents > 0
      ? h(
          'span',
          {
            className: 'qd-row-subagents mono',
            title: `${subagents} background ${subagents === 1 ? 'subagent' : 'subagents'} running`,
          },
          [badgeText],
        )
      : null;

  const el = h(
    'div',
    {
      className: 'qd-row',
      title: tooltip,
      // SPEC R-15.4: a row click focuses the terminal window hosting the
      // session (the former row-click no-op is gone).
      onclick: () => focusTerminal(row.id),
      oncontextmenu: (ev: Event) => {
        ev.preventDefault();
        const mouse = ev as MouseEvent;
        openCtxMenu(mouse.clientX, mouse.clientY, row);
      },
    },
    [
      statusDot(row.status),
      h('div', { className: 'qd-row-main' }, [
        h('span', { className: 'qd-row-project' }, [row.project]),
        h('span', { className: 'qd-row-title' }, [row.title]),
        row.branch ? h('span', { className: 'qd-row-branch mono' }, [row.branch]) : null,
        badge,
      ]),
      // R-23.4: the right block is a vertical stack (time on top, usage below).
      h('div', { className: 'qd-row-right' }, [timeEl, usageEl]),
    ],
  );
  return el;
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
  return h('div', { className: 'qd-ask-row qd-perm-row', onclick: reopenAskWindowUnlessInteractive }, [
    h('div', { className: 'qd-ask-row-head' }, [
      h('span', { className: 'qd-ask-row-agent' }, [agent]),
      h('span', { style: 'color:var(--muted)' }, ['requests permission']),
    ]),
    h('div', { className: 'qd-ask-row-question qd-perm-row-tool' }, [`Run ${perm.toolName}?`]),
    h('div', { className: 'qd-ask-row-actions' }, [
      h('button', { className: 'qd-btn qd-btn-primary qd-perm-row-allow', type: 'button', onclick: () => decide('allow') }, ['Allow']),
      h('button', { className: 'qd-btn qd-perm-row-deny', type: 'button', onclick: () => decide('deny') }, ['Deny']),
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

/** SPEC R-25.1/R-25.3: renders the lamp — worst-of aggregate color (mirrors
 * the tray icon, R-2.6), an attention-count badge when > 0, and a hover
 * tooltip carrying the same counts line as the popup footer. */
function renderLamp(snap: StateSnapshot): void {
  elLampDot.setAttribute('data-status', worstStatus(snap.counts));
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
  clear(elContent);
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
            settingsOpen = false;
            elSettings.classList.remove('open');
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
    // pane left open from before collapsing doesn't render on top of it.
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
 * while the settings overlay is open (it has its own scroll). */
function syncPopupHeight(): void {
  if (settingsOpen) return;
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

  const total = headerH + contentH + footerH;
  void invoke('resize_popup', { contentHeight: total }).catch(() => undefined);
}

elGear.addEventListener('click', () => {
  settingsOpen = !settingsOpen;
  if (settingsOpen && latest) renderSettings(latest);
  elSettings.classList.toggle('open', settingsOpen);
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
      settingsOpen = false;
      elSettings.classList.remove('open');
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
