/**
 * Popup window controller (SPEC §7). Snapshot-driven: the shell (or the mock)
 * pushes full `StateSnapshot`s; this file only renders + sends intent back
 * through `ipc-client.ts` commands. No business logic lives here (R-3.4).
 */

import { invoke, onState, usingMock } from './ipc-client';
import type { AskAnswerKind, AskRow, SessionRow, SessionStatus, SettingsState, StateSnapshot } from './ipc-contract';
import { footerText, formatDuration, truncate } from './format';
import { clear, h } from './dom';
import { installMockScenarioSwitcher } from './mock-switcher';

const elWatchline = document.getElementById('qd-watchline') as HTMLElement;
const elContent = document.getElementById('qd-content') as HTMLElement;
const elFooter = document.getElementById('qd-footer') as HTMLElement;
const elSettings = document.getElementById('qd-settings') as HTMLElement;
const elGear = document.getElementById('qd-gear') as HTMLButtonElement;
const elPin = document.getElementById('qd-pin') as HTMLButtonElement;

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

// (session row time element, base sinceMs at time of snapshot) — updated by the
// 1s ticker below without touching the rest of the DOM (keeps focus/menus).
let timeTicks: Array<{ el: HTMLElement; base: number }> = [];

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

function renderSessionRow(row: SessionRow): HTMLElement {
  const timeEl = h('span', { className: 'qd-row-time mono' }, [formatDuration(row.sinceMs)]);
  timeTicks.push({ el: timeEl, base: row.sinceMs });

  const el = h(
    'div',
    {
      className: 'qd-row',
      title: row.cwd,
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
        row.inferred ? h('span', { className: 'qd-row-inferred', title: 'Inferred from a cold-start scan' }, ['~']) : null,
        h('span', { className: 'qd-row-title' }, [row.title]),
        row.branch ? h('span', { className: 'qd-row-branch mono' }, [row.branch]) : null,
      ]),
      timeEl,
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
  const onboardingActive = settings ? settings.onboardingDone === false : false;
  const hasRows = snap.sessions.length > 0 || snap.asks.length > 0;

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
  for (const ask of snap.asks) {
    rows.append(renderAskMirrorRow(ask));
  }
  // R-3.4/R-7.3: `snap.sessions` already arrives in the engine's canonical
  // R-7.3 order (`SessionStore::view`); render it as-is, never re-sort here.
  for (const row of snap.sessions) {
    rows.append(renderSessionRow(row));
  }
  elContent.append(rows);
  restoreAskInputs(preservedAsks);
}

function renderFooter(counts: StateSnapshot['counts']): void {
  const text = footerText(counts);
  elFooter.textContent = text.length > 0 ? text : ' ';
}

/** Keys of the boolean settings driven by a `toggleControl`. */
type ToggleKey = 'notifyIdle' | 'notifyAttention' | 'notifyReminder' | 'launchAtLogin';

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
  renderWatchline(latest.counts);
  renderContent(latest);
  renderFooter(latest.counts);
  renderGearIssue(latest.hooksInstalled);
  renderPinState(latest.settings?.popupPinned ?? false);
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
    tick.el.textContent = formatDuration(tick.base + elapsed);
  }
}, 1000);

if (usingMock) {
  installMockScenarioSwitcher();
  // Screenshot convenience only: `?openSettings=1` opens the pane on load.
  if (new URLSearchParams(location.search).get('openSettings') === '1') {
    elGear.click();
  }
}
