/**
 * Ask window controller (SPEC R-8.3). Always-on-top, small, never steals
 * focus on appear (that's the shell's job — see `src-tauri/src/windows.rs`,
 * T3). This file renders whichever ask is first in the FIFO queue and lets
 * the user answer it via option buttons (keys 1-9), free text, or dismiss.
 */

import { hideCurrentWindow, invoke, onState } from './ipc-client';
import type { AskAnswerKind, AskQuestion, AskRow, PermDecision, PermRow, SessionRow, StateSnapshot } from './ipc-contract';
import { formatCountdown, truncate } from './format';
import { clear, h } from './dom';

const elContent = document.getElementById('qd-ask-content') as HTMLElement;
const elBadge = document.getElementById('qd-ask-badge') as HTMLElement;
const elClose = document.getElementById('qd-ask-close') as HTMLButtonElement;

let latest: StateSnapshot | null = null;
let countdownEl: HTMLElement | null = null;
let countdownTarget: number | null = null;
let freeTextInput: HTMLInputElement | null = null;
/** The permission request currently rendered as the primary item, if any (SPEC
 * §16). Perms take priority over asks in the shared window; while one is primary
 * the keyboard maps A/D/Esc to Allow/Deny/In-terminal instead of the ask keys. */
let primaryPerm: PermRow | null = null;
/** Id of the ask currently rendered, so a re-render of the SAME ask (triggered
 * by an unrelated session's `deck://state` push) can restore the in-progress
 * free-text answer + focus instead of wiping it (R-8). */
let renderedAskId: string | null = null;

/** SPEC §29 (R-29.4): in-progress multi-question form answer, kept module-level
 * so a `deck://state` re-push (from any session) can't wipe the user's
 * selections / typed text mid-fill. Keyed by ask id; reset when a different ask
 * becomes primary. `selected[qi]` holds the chosen option INDICES for question
 * `qi` (radio = at most one, checkbox = any); `texts[qi]` is its free-text. */
interface FormState {
  askId: string;
  selected: Set<number>[];
  texts: string[];
}
let formState: FormState | null = null;
/** Which form free-text field held focus/selection before a rebuild, so it can
 * be restored (the form analog of the single-question `preserved` path). */
let formTextFocus: { askId: string; qi: number; selStart: number | null; selEnd: number | null } | null = null;

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

/** SPEC §29 (R-29.4): render a multi-question / multi-select form. Each block is
 * a header + question + option buttons (radio when `!multiSelect`, checkbox when
 * `multiSelect`) plus an optional per-question free-text field; one Submit
 * validates required single-selects and sends the whole form as a `form`-kind
 * JSON answer `{answers:[{header,question,selected:[...],text?}, ...]}`. Selections
 * + typed text live in module-level `formState` so they survive a re-render. */
function renderForm(ask: AskRow, questions: AskQuestion[]): HTMLElement {
  // Reset the working state whenever a different ask becomes primary; otherwise
  // keep the user's in-progress selections/text across a re-render (R-29.4).
  if (!formState || formState.askId !== ask.id) {
    formState = {
      askId: ask.id,
      selected: questions.map(() => new Set<number>()),
      texts: questions.map(() => ''),
    };
  }
  const state = formState;
  freeTextInput = null;

  const errorEl = h('div', { className: 'qd-ask-form-error', hidden: true }, ['Please answer every required question.']);

  const blocks = questions.map((q, qi) => {
    const optionButtons: HTMLButtonElement[] = [];
    const syncSelected = (): void => {
      optionButtons.forEach((btn, oi) => btn.classList.toggle('selected', state.selected[qi].has(oi)));
    };
    (q.options ?? []).forEach((opt, oi) => {
      const btn = h(
        'button',
        {
          className: 'qd-btn qd-ask-option qd-ask-form-option',
          type: 'button',
          role: q.multiSelect ? 'checkbox' : 'radio',
          onclick: () => {
            const sel = state.selected[qi];
            if (q.multiSelect) {
              if (sel.has(oi)) sel.delete(oi);
              else sel.add(oi);
            } else {
              // Radio: exactly one — replace any prior choice with this one.
              sel.clear();
              sel.add(oi);
            }
            errorEl.hidden = true;
            syncSelected();
          },
        },
        [
          h('span', { className: `qd-ask-form-mark${q.multiSelect ? ' checkbox' : ''}` }, []),
          h('span', { className: 'qd-ask-option-text' }, [opt]),
        ],
      ) as HTMLButtonElement;
      optionButtons.push(btn);
    });
    syncSelected();

    const textInput = h('input', {
      className: 'qd-ask-form-text',
      type: 'text',
      'data-qi': String(qi),
      placeholder: q.options && q.options.length > 0 ? 'Or type an answer…' : 'Type an answer…',
      value: state.texts[qi],
      oninput: (ev: Event) => {
        state.texts[qi] = (ev.target as HTMLInputElement).value;
      },
    }) as HTMLInputElement;

    return h('div', { className: 'qd-ask-form-q' }, [
      q.header ? h('div', { className: 'qd-ask-q-header' }, [q.header]) : null,
      h('div', { className: 'qd-ask-question qd-ask-form-question' }, [q.question]),
      optionButtons.length > 0 ? h('div', { className: 'qd-ask-options' }, optionButtons) : null,
      textInput,
    ]);
  });

  const submit = h(
    'button',
    {
      className: 'qd-btn qd-btn-primary',
      type: 'button',
      onclick: () => {
        // R-29.4: a single-select question that offers options requires a choice.
        const missing = questions.some(
          (q, qi) => !q.multiSelect && (q.options?.length ?? 0) > 0 && state.selected[qi].size === 0,
        );
        if (missing) {
          errorEl.hidden = false;
          return;
        }
        const answers = questions.map((q, qi) => {
          const selected = [...state.selected[qi]].sort((a, b) => a - b).map((i) => q.options[i]);
          const text = state.texts[qi].trim();
          const entry: { header?: string; question: string; selected: string[]; text?: string } = {
            question: q.question,
            selected,
          };
          if (q.header) entry.header = q.header;
          if (text) entry.text = text;
          return entry;
        });
        send(ask, JSON.stringify({ answers }), 'form');
      },
    },
    ['Submit'],
  );

  return h('div', { className: 'qd-ask-form' }, [
    ...blocks,
    errorEl,
    h('div', { className: 'qd-ask-actions' }, [
      h('button', { className: 'qd-btn qd-btn-ghost', type: 'button', onclick: () => send(ask, '', 'dismissed') }, ['Dismiss']),
      submit,
    ]),
  ]);
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
  // SPEC §29 (R-29.4): a multi-question / multi-select ask renders as a form
  // (radio/checkbox blocks + Submit) instead of the single-question options +
  // free-text. The form carries its own Dismiss, so no separate actions row.
  if (ask.questions && ask.questions.length > 0) {
    elContent.append(
      renderIdentity(ask, sessions),
      // The synthesized headline duplicates the first block, so skip it here and
      // let the form's own per-question text carry the prompt.
      ...(ask.detail ? [h('div', { className: 'qd-ask-detail' }, [ask.detail])] : []),
      renderForm(ask, ask.questions),
    );
    return;
  }
  formState = null;
  elContent.append(
    renderIdentity(ask, sessions),
    h('div', { className: 'qd-ask-question' }, [ask.question]),
    // R-19.1: optional long-form rationale, muted + smaller, scrollable if long.
    ...(ask.detail ? [h('div', { className: 'qd-ask-detail' }, [ask.detail])] : []),
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

function sendPerm(perm: PermRow, decision: PermDecision): void {
  if (answered.has(perm.id)) return;
  answered.add(perm.id);
  void invoke('answer_perm', { permId: perm.id, decision }).catch(() => {
    answered.delete(perm.id);
  });
}

/** SPEC §16 (R-16.2): the permission modal — amber accent, "<project> requests
 * permission", tool name + compact pretty-printed input (already sanitized +
 * capped by the shell, R-16.5), and Allow / Deny / In terminal actions. */
function renderPerm(perm: PermRow, sessions: SessionRow[]): void {
  clear(elContent);
  freeTextInput = null;
  countdownEl = null;
  countdownTarget = null;

  const session = findSession(sessions, perm.sessionId);
  const dot = h('span', { className: 'qd-row-dot', 'data-status': session?.status ?? 'attention' });
  const label =
    session?.project ??
    perm.project ??
    (perm.context ? `Unknown agent (${truncate(perm.context, 42)})` : 'Unknown agent');

  // R-32.1: past the deadline the perm's hook has already given up (a deck
  // decision can no longer reach it), so Allow/Deny are disabled until the tick
  // sweep removes the row. "In terminal" stays live so the user can still clear
  // it locally. An identity tag flags the expired state.
  const expired = perm.expiresAt !== undefined && Date.now() >= perm.expiresAt;

  const identity = h('div', { className: 'qd-ask-identity qd-perm-identity' }, [
    dot,
    h('span', { className: 'qd-ask-identity-project' }, [label]),
    h('span', { className: 'qd-perm-tag mono' }, [expired ? 'expired' : 'requests permission']),
  ]);

  const allow = h(
    'button',
    { className: 'qd-btn qd-btn-primary qd-perm-allow', type: 'button', onclick: () => sendPerm(perm, 'allow') },
    ['Allow'],
  ) as HTMLButtonElement;
  const deny = h(
    'button',
    { className: 'qd-btn qd-perm-deny', type: 'button', onclick: () => sendPerm(perm, 'deny') },
    ['Deny'],
  ) as HTMLButtonElement;
  if (expired) {
    allow.disabled = true;
    deny.disabled = true;
  }

  elContent.append(
    h('div', { className: 'qd-perm' }, [
      identity,
      h('div', { className: 'qd-ask-question qd-perm-tool' }, [`Run ${perm.toolName}?`]),
      h('pre', { className: 'qd-perm-input mono' }, [perm.toolInput || '(no input)']),
      h('div', { className: 'qd-ask-actions qd-perm-actions' }, [
        allow,
        deny,
        h(
          'button',
          { className: 'qd-btn qd-btn-ghost qd-perm-defer', type: 'button', onclick: () => sendPerm(perm, 'defer') },
          ['In terminal'],
        ),
      ]),
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
  const perms = snap.perms ?? [];
  const asks = snap.asks;
  const total = perms.length + asks.length;

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

  // R-29.4: the form analog — remember which per-question free-text field was
  // focused (its value already lives in `formState`) so an unrelated re-push
  // doesn't drop the caret mid-answer.
  const active = document.activeElement;
  formTextFocus =
    renderedAskId !== null && active instanceof HTMLInputElement && active.classList.contains('qd-ask-form-text')
      ? {
          askId: renderedAskId,
          qi: Number(active.getAttribute('data-qi')),
          selStart: active.selectionStart,
          selEnd: active.selectionEnd,
        }
      : null;

  if (total === 0) {
    elBadge.hidden = true;
    renderedAskId = null;
    primaryPerm = null;
    renderEmpty();
    return;
  }
  // "N more waiting" counts every OTHER pending item (asks + perms), R-16.2.
  if (total > 1) {
    elBadge.hidden = false;
    elBadge.textContent = `${total - 1} more waiting`;
  } else {
    elBadge.hidden = true;
  }

  // R-16.2: perms and asks share ONE FIFO queue by arrival — the primary slot
  // goes to whichever of the front perm / front ask arrived first (smaller
  // `queuedAt`), NOT to perms unconditionally. Both `perms` and `asks` are
  // already arrival-ordered within themselves (the backend pushes at the back),
  // so comparing the two fronts is enough. A perm wins ties (a blocked terminal
  // is the more latency-sensitive case when arrivals are simultaneous).
  const frontPerm = perms[0];
  const frontAsk = asks[0];
  const permIsPrimary = frontPerm !== undefined && (frontAsk === undefined || frontPerm.queuedAt <= frontAsk.queuedAt);
  if (permIsPrimary) {
    renderedAskId = null;
    primaryPerm = frontPerm;
    renderPerm(frontPerm, snap.sessions);
    return;
  }

  primaryPerm = null;
  const primary = frontAsk;
  renderAsk(primary, snap.sessions);
  renderedAskId = primary.id;

  // R-29.4: restore form free-text focus when the SAME form ask is still primary.
  if (formTextFocus && formTextFocus.askId === primary.id) {
    const field = elContent.querySelector<HTMLInputElement>(`.qd-ask-form-text[data-qi="${formTextFocus.qi}"]`);
    if (field) {
      field.focus();
      try {
        field.setSelectionRange(formTextFocus.selStart ?? field.value.length, formTextFocus.selEnd ?? field.value.length);
      } catch {
        /* value already restored from formState; selection is best-effort. */
      }
    }
  }

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
  // SPEC §16 (R-16.2): while a permission request is primary, the keyboard maps
  // A → Allow, D → Deny, Esc → In terminal (defer). This overrides the ask
  // window's Esc-hides-the-window behavior for that item specifically.
  if (primaryPerm) {
    const key = ev.key.toLowerCase();
    // R-32.1: past the deadline Allow/Deny are disabled (the hook has given up),
    // so their A/D shortcuts are inert too; Esc = In terminal still clears it.
    const permExpired = primaryPerm.expiresAt !== undefined && Date.now() >= primaryPerm.expiresAt;
    if (key === 'a' && !permExpired) {
      sendPerm(primaryPerm, 'allow');
      return;
    }
    if (key === 'd' && !permExpired) {
      sendPerm(primaryPerm, 'deny');
      return;
    }
    if (ev.key === 'Escape') {
      // R-16.2: Esc = In terminal (answers "no decision" → the terminal dialog
      // appears immediately), NOT hide-the-window.
      sendPerm(primaryPerm, 'defer');
      return;
    }
    return;
  }
  if (ev.key === 'Escape') {
    // R-18.1: Esc is ALWAYS the same as the X button — it hides the window,
    // it never silently dismisses a pending ask, whether one or many are
    // queued.
    hideCurrentWindow();
    return;
  }
  if (!latest || latest.asks.length === 0) return;
  if (document.activeElement === freeTextInput) return;
  const primary = latest.asks[0];
  // R-29.4: a multi-question form is answered via its buttons/Submit, never the
  // 1-9 option shortcut (which maps to the single-question options).
  if (primary.questions && primary.questions.length > 0) return;
  const digit = Number(ev.key);
  if (!Number.isInteger(digit) || digit < 1 || digit > 9) return;
  const options = primary.options ?? [];
  const opt = options[digit - 1];
  if (opt !== undefined) {
    send(primary, opt, 'option');
  }
});

setInterval(updateCountdown, 1000);
