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

function renderSessionRow(row: SessionRow): HTMLElement {
  const timeEl = h('span', { className: 'qd-row-time mono' }, [formatDuration(row.sinceMs)]);
  timeTicks.push({ el: timeEl, base: row.sinceMs });

  const el = h(
    'div',
    {
      className: 'qd-row',
      title: row.cwd,
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

function renderAskMirrorRow(ask: AskRow): HTMLElement {
  const send = (answer: string, kind: AskAnswerKind): void => {
    void invoke('answer_ask', { askId: ask.id, answer, kind });
  };

  // R-8.7: an ask recovered after a restart can never be answered — show it as
  // expired with only a Dismiss action, "never answered into the void".
  if (ask.orphaned) {
    return h('div', { className: 'qd-ask-row qd-ask-row-expired' }, [
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

  return h('div', { className: 'qd-ask-row' }, [
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

function renderContent(snap: StateSnapshot): void {
  clear(elContent);
  timeTicks = [];
  const settings = snap.settings;
  const onboardingActive = settings ? settings.onboardingDone === false : false;

  if (onboardingActive) {
    elContent.append(renderOnboarding(settings as SettingsState, snap.hooksInstalled));
  } else if (!snap.hooksInstalled) {
    elContent.append(renderHooksBanner());
  }

  if (installError) {
    elContent.append(h('div', { className: 'qd-banner-error' }, [installError]));
  }

  const hasRows = snap.sessions.length > 0 || snap.asks.length > 0;

  if (!hasRows && !onboardingActive) {
    elContent.append(renderEmptyState(snap.hooksInstalled));
    elFooter.style.display = 'none';
    return;
  }

  elFooter.style.display = hasRows ? '' : 'none';

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
}

function renderFooter(counts: StateSnapshot['counts']): void {
  const text = footerText(counts);
  elFooter.textContent = text.length > 0 ? text : ' ';
}

function toggleControl(label: string, checked: boolean, onToggle: (next: boolean) => void): HTMLElement {
  return h('div', { className: 'qd-toggle-row' }, [
    h('span', { className: 'qd-toggle-label' }, [label]),
    h('button', {
      className: 'qd-toggle',
      type: 'button',
      role: 'switch',
      'aria-checked': String(checked),
      onclick: () => onToggle(!checked),
    }),
  ]);
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
        toggleControl('Notify when a session finishes', settings.notifyIdle, (v) => set('notifyIdle', v)),
        toggleControl('Notify when a session needs you', settings.notifyAttention, (v) => set('notifyAttention', v)),
        toggleControl('Remind me if a session is still waiting', settings.notifyReminder, (v) => set('notifyReminder', v)),
      ]),
      h('div', { className: 'qd-settings-section' }, [
        h('p', { className: 'qd-settings-section-title' }, ['General']),
        toggleControl('Launch Quarterdeck at login', settings.launchAtLogin, (v) => set('launchAtLogin', v)),
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
  if (settingsOpen) renderSettings(latest);
  syncPopupHeight();
}

/** R-7.1 grow-then-scroll: report the intrinsic content height so the shell can
 * size the window (clamped to 460..=560 in Rust). `.qd-content` scrolls
 * internally, so its `scrollHeight` is the full natural content height even
 * while the window constrains it. No-op in mock/browser mode and while the
 * settings overlay is open (it has its own scroll). */
function syncPopupHeight(): void {
  if (usingMock || settingsOpen) return;
  const header = document.querySelector('.qd-header') as HTMLElement | null;
  const headerH = header?.offsetHeight ?? 0;
  const contentH = elContent.scrollHeight;
  const footerH = elFooter.style.display === 'none' ? 0 : elFooter.offsetHeight;
  const total = headerH + contentH + footerH;
  void invoke('resize_popup', { contentHeight: total }).catch(() => undefined);
}

elGear.addEventListener('click', () => {
  settingsOpen = !settingsOpen;
  if (settingsOpen && latest) renderSettings(latest);
  elSettings.classList.toggle('open', settingsOpen);
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
