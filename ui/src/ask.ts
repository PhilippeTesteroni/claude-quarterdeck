/**
 * Ask window controller (SPEC R-8.3). Always-on-top, small, never steals
 * focus on appear (that's the shell's job — see `src-tauri/src/windows.rs`,
 * T3). This file renders whichever ask is first in the FIFO queue and lets
 * the user answer it via option buttons (keys 1-9), free text, or dismiss.
 */

import { hideCurrentWindow, invoke, onState } from './ipc-client';
import type { AskAnswerKind, AskRow, SessionRow, StateSnapshot } from './ipc-contract';
import { formatCountdown, truncate } from './format';
import { clear, h } from './dom';

const elContent = document.getElementById('qd-ask-content') as HTMLElement;
const elBadge = document.getElementById('qd-ask-badge') as HTMLElement;
const elClose = document.getElementById('qd-ask-close') as HTMLButtonElement;

let latest: StateSnapshot | null = null;
let countdownEl: HTMLElement | null = null;
let countdownTarget: number | null = null;
let freeTextInput: HTMLInputElement | null = null;
/** Id of the ask currently rendered, so a re-render of the SAME ask (triggered
 * by an unrelated session's `deck://state` push) can restore the in-progress
 * free-text answer + focus instead of wiping it (R-8). */
let renderedAskId: string | null = null;

function findSession(sessions: SessionRow[], id?: string): SessionRow | undefined {
  if (!id) return undefined;
  return sessions.find((s) => s.id === id);
}

/** Ask ids already answered from this window, so a second answer for the SAME
 * ask (a stray double-click on an option, or a click racing a leftover Enter in
 * the free-text field) is dropped. Both answer_ask calls would write the same
 * `<data>/answers/<askId>.json`, the second overwriting the first, and the
 * watcher (debounced) delivers only the last content — silently discarding the
 * user's first answer with no feedback. Single-flight prevents the second send. */
const answered = new Set<string>();

function send(ask: AskRow, answer: string, kind: AskAnswerKind): void {
  if (answered.has(ask.id)) return;
  answered.add(ask.id);
  // Let the user retry only if the answer never reached the backend.
  void invoke('answer_ask', { askId: ask.id, answer, kind }).catch(() => {
    answered.delete(ask.id);
  });
}

function renderIdentity(ask: AskRow, sessions: SessionRow[]): HTMLElement {
  const session = findSession(sessions, ask.sessionId);
  const dot = h('span', { className: 'qd-row-dot', 'data-status': session?.status ?? 'dead' });
  const label = session?.project ?? ask.project ?? (ask.context ? `Unknown agent (${truncate(ask.context, 42)})` : 'Unknown agent');

  const row = h('div', { className: 'qd-ask-identity' }, [
    dot,
    h('span', { className: 'qd-ask-identity-project' }, [label]),
  ]);

  if (ask.timeoutAt !== undefined && !ask.orphaned) {
    const cd = h('span', { className: 'qd-ask-countdown mono' }, ['']);
    row.append(cd);
    countdownEl = cd;
    countdownTarget = ask.timeoutAt;
    updateCountdown();
  } else {
    countdownEl = null;
    countdownTarget = null;
    if (ask.orphaned) {
      row.append(h('span', { className: 'qd-ask-countdown mono' }, ['expired']));
    }
  }

  return row;
}

function updateCountdown(): void {
  if (!countdownEl || countdownTarget === null) return;
  const remaining = countdownTarget - Date.now();
  countdownEl.textContent = `Times out in ${formatCountdown(remaining)}`;
  countdownEl.classList.toggle('urgent', remaining <= 10_000);
}

function renderOptions(ask: AskRow): HTMLElement {
  const options = ask.options ?? [];
  return h(
    'div',
    { className: 'qd-ask-options' },
    options.map((opt, i) =>
      h(
        'button',
        {
          className: 'qd-btn qd-ask-option',
          type: 'button',
          onclick: () => send(ask, opt, 'option'),
        },
        [h('span', { className: 'qd-ask-option-key' }, [String(i + 1)]), h('span', { className: 'qd-ask-option-text' }, [opt])],
      ),
    ),
  );
}

function renderFreeform(ask: AskRow): HTMLElement {
  const input = h('input', {
    type: 'text',
    placeholder: 'Type an answer…',
    onkeydown: (ev: Event) => {
      const kev = ev as KeyboardEvent;
      if (kev.key === 'Enter') {
        const value = (input as HTMLInputElement).value.trim();
        if (value) send(ask, value, 'text');
      }
    },
  }) as HTMLInputElement;
  freeTextInput = input;

  const submit = h(
    'button',
    {
      className: 'qd-btn qd-btn-primary',
      type: 'button',
      onclick: () => {
        const value = input.value.trim();
        if (value) send(ask, value, 'text');
      },
    },
    ['Send answer'],
  );

  return h('div', { className: 'qd-ask-freeform' }, [input, submit]);
}

function renderAsk(ask: AskRow, sessions: SessionRow[]): void {
  clear(elContent);
  // R-8.7: an ask recovered after a restart can never be answered — show it as
  // expired with only a Dismiss action ("never answered into the void").
  if (ask.orphaned) {
    freeTextInput = null;
    elContent.append(
      renderIdentity(ask, sessions),
      h('div', { className: 'qd-ask-question' }, [ask.question]),
      h('p', { className: 'qd-ask-empty', style: 'padding:4px 0;text-align:left' }, [
        'This question expired while Quarterdeck was closed. It can no longer be answered.',
      ]),
      h('div', { className: 'qd-ask-actions' }, [
        h(
          'button',
          { className: 'qd-btn qd-btn-primary', type: 'button', onclick: () => send(ask, '', 'dismissed') },
          ['Dismiss'],
        ),
      ]),
    );
    return;
  }
  elContent.append(
    renderIdentity(ask, sessions),
    h('div', { className: 'qd-ask-question' }, [ask.question]),
    renderOptions(ask),
    renderFreeform(ask),
    h('div', { className: 'qd-ask-actions' }, [
      h(
        'button',
        { className: 'qd-btn qd-btn-ghost', type: 'button', onclick: () => send(ask, '', 'dismissed') },
        ['Dismiss'],
      ),
    ]),
  );
}

function renderEmpty(): void {
  clear(elContent);
  countdownEl = null;
  countdownTarget = null;
  elContent.append(h('div', { className: 'qd-ask-empty' }, ['No pending questions.']));
}

function render(snap: StateSnapshot): void {
  const [primary, ...rest] = snap.asks;

  // R-8 data-loss guard: `push_state()` broadcasts to every window on ANY
  // session's state change (a sibling session finishing, a liveness tick, …),
  // not just this ask's. Capture the in-progress free-text answer + focus before
  // rebuilding so an unrelated push can't silently wipe the one interactive
  // surface the ask channel provides.
  const preserved =
    freeTextInput && renderedAskId !== null
      ? {
          askId: renderedAskId,
          value: freeTextInput.value,
          focused: document.activeElement === freeTextInput,
          selStart: freeTextInput.selectionStart,
          selEnd: freeTextInput.selectionEnd,
        }
      : null;

  if (!primary) {
    elBadge.hidden = true;
    renderedAskId = null;
    renderEmpty();
    return;
  }
  if (rest.length > 0) {
    elBadge.hidden = false;
    elBadge.textContent = `${rest.length} more waiting`;
  } else {
    elBadge.hidden = true;
  }
  renderAsk(primary, snap.sessions);
  renderedAskId = primary.id;

  // Only restore when the SAME ask is still on top (its question/options are
  // immutable, so the typed text still applies) and it has a free-text field.
  if (preserved && preserved.askId === primary.id && freeTextInput) {
    freeTextInput.value = preserved.value;
    if (preserved.focused) {
      freeTextInput.focus();
      try {
        freeTextInput.setSelectionRange(preserved.selStart ?? preserved.value.length, preserved.selEnd ?? preserved.value.length);
      } catch {
        /* value already restored; selection is best-effort. */
      }
    }
  }
}

onState((snap) => {
  latest = snap;
  render(snap);
});

// R-18.1: the X (top-right) closes (hides) the WINDOW without dismissing any
// pending ask — they stay queued + mirrored in the popup, badge intact; the
// window re-appears on the next new ask/perm (or via a popup mirror click).
// This is distinct from per-ask "Dismiss", which resolves that ask.
elClose.addEventListener('click', () => hideCurrentWindow());

document.addEventListener('keydown', (ev) => {
  if (ev.key === 'Escape') {
    // R-18.1: Esc is ALWAYS the same as the X button — it hides the window,
    // it never silently dismisses a pending ask, whether one or many are
    // queued.
    hideCurrentWindow();
    return;
  }
  if (!latest || latest.asks.length === 0) return;
  if (document.activeElement === freeTextInput) return;
  const digit = Number(ev.key);
  if (!Number.isInteger(digit) || digit < 1 || digit > 9) return;
  const primary = latest.asks[0];
  const options = primary.options ?? [];
  const opt = options[digit - 1];
  if (opt !== undefined) {
    send(primary, opt, 'option');
  }
});

setInterval(updateCountdown, 1000);
