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
/** SPEC §35.2: the last content height reported to the shell via `resize_ask`,
 * so a re-render that measures the same height doesn't re-invoke the resize
 * (guards churn from the `deck://state` re-pushes that fire on every session's
 * change, not just this ask's). */
let lastAskHeight: number | null = null;
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

/** §46 dual-answer: the secondary "In terminal" escape. Resolves the ask with
 * kind `terminal` (empty answer), the signal for the agent to re-ask the same
 * question via the native `AskUserQuestion` terminal picker. Grey/ghost styling
 * so it stays out of the way beside Dismiss; present on the answerable single-
 * question + form paths (never on an orphaned ask, which can't be answered). */
function renderTerminalEscape(ask: AskRow): HTMLButtonElement {
  return h(
    'button',
    {
      className: 'qd-btn qd-btn-ghost qd-ask-terminal',
      type: 'button',
      title: 'Answer this in the terminal instead',
      onclick: () => send(ask, '', 'terminal'),
    },
    ['In terminal'],
  ) as HTMLButtonElement;
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
      // §46: "In terminal" + Dismiss grouped on the left, Submit on the right.
      h('div', { className: 'qd-ask-actions-group' }, [
        renderTerminalEscape(ask),
        h('button', { className: 'qd-btn qd-btn-ghost', type: 'button', onclick: () => send(ask, '', 'dismissed') }, ['Dismiss']),
      ]),
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
    // §46: "In terminal" escape sits beside Dismiss, both secondary.
    h('div', { className: 'qd-ask-actions' }, [
      h('div', { className: 'qd-ask-actions-group' }, [
        renderTerminalEscape(ask),
        h(
          'button',
          { className: 'qd-btn qd-btn-ghost', type: 'button', onclick: () => send(ask, '', 'dismissed') },
          ['Dismiss'],
        ),
      ]),
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

/** SPEC §35.1 truncation budgets for the structured permission render. The
 * shell already caps `toolInput` overall (R-16.5); these keep any single long
 * field (a file's whole content, an edit's old/new string, a stray value) from
 * dominating the modal before the window's own scroll takes over. */
const PERM_PREVIEW_MAX = 2000;
const PERM_EDIT_MAX = 600;
const PERM_VALUE_MAX = 400;

/** Human labels for the path/pattern fields of the read-only tools (R-35.1). */
const PERM_PATH_LABELS: Record<string, string> = {
  file_path: 'Path',
  path: 'Path',
  pattern: 'Pattern',
  glob: 'Glob',
};

/** Coerce a parsed JSON field to a display string: strings verbatim, everything
 * else (numbers, booleans, nested objects/arrays) via a compact JSON dump so a
 * `{"timeout":120000}`-style value still reads. Absent → empty. */
function permValueStr(v: unknown): string {
  if (typeof v === 'string') return v;
  if (v === undefined || v === null) return '';
  return JSON.stringify(v);
}

/** A plain (non-array) object, or null — so a malformed `edits[i]` / options
 * entry can't throw while we index into it. */
function asRecord(v: unknown): Record<string, unknown> | null {
  return typeof v === 'object' && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
}

/** A label + single-line mono value (a file path, pattern, or key/value cell). */
function permLineField(label: string, value: string): HTMLElement {
  return h('div', { className: 'qd-perm-field' }, [
    h('div', { className: 'qd-perm-field-label' }, [label]),
    h('div', { className: 'qd-perm-field-path mono' }, [value || '(empty)']),
  ]);
}

/** A label + a newline-preserving mono code box (reusing the `.qd-perm-input`
 * `<pre>` that also serves as the parse-failure fallback, so both look alike). */
function permCodeField(label: string, code: string): HTMLElement {
  return h('div', { className: 'qd-perm-field' }, [
    h('div', { className: 'qd-perm-field-label' }, [label]),
    h('pre', { className: 'qd-perm-input mono' }, [code || '(empty)']),
  ]);
}

/** Bash: the `command` in a mono box, plus its `description` if present. */
function renderBashInput(input: Record<string, unknown>): HTMLElement {
  const description = typeof input.description === 'string' ? input.description : '';
  return h('div', { className: 'qd-perm-body' }, [
    permCodeField('Command', permValueStr(input.command)),
    description ? h('div', { className: 'qd-perm-desc' }, [description]) : null,
  ]);
}

/** Write: the target `file_path` + a truncated preview of `content`. */
function renderWriteInput(input: Record<string, unknown>): HTMLElement {
  return h('div', { className: 'qd-perm-body' }, [
    permLineField('File', permValueStr(input.file_path)),
    permCodeField('Content', truncate(permValueStr(input.content), PERM_PREVIEW_MAX)),
  ]);
}

/** One old→new replacement pair (an Edit, or one entry of a MultiEdit `edits`). */
function renderEditPair(rec: Record<string, unknown> | null, label: string | null): HTMLElement {
  return h('div', { className: 'qd-perm-edit' }, [
    label !== null ? h('div', { className: 'qd-perm-field-label' }, [label]) : null,
    permCodeField('Replace', truncate(permValueStr(rec?.old_string), PERM_EDIT_MAX)),
    permCodeField('With', truncate(permValueStr(rec?.new_string), PERM_EDIT_MAX)),
  ]);
}

/** Edit / MultiEdit: the `file_path` + one replacement pair, or the `edits` list. */
function renderEditInput(input: Record<string, unknown>): HTMLElement {
  const children: (HTMLElement | null)[] = [permLineField('File', permValueStr(input.file_path))];
  const edits = Array.isArray(input.edits) ? input.edits : null;
  if (edits) {
    edits.forEach((e, i) => children.push(renderEditPair(asRecord(e), edits.length > 1 ? `Edit ${i + 1}` : null)));
  } else {
    children.push(renderEditPair(input, null));
  }
  return h('div', { className: 'qd-perm-body' }, children);
}

/** Read / Glob / Grep / LS: the present path/pattern fields, one line each. */
function renderPathInput(input: Record<string, unknown>): HTMLElement {
  const fields = Object.keys(PERM_PATH_LABELS)
    .filter((k) => input[k] !== undefined)
    .map((k) => permLineField(PERM_PATH_LABELS[k], truncate(permValueStr(input[k]), PERM_VALUE_MAX)));
  // No known path/pattern field present → fall back to generic key/value rows.
  if (fields.length === 0) return renderKeyValueInput(input);
  return h('div', { className: 'qd-perm-body' }, fields);
}

/** One READ-ONLY AskUserQuestion block: header (if any) + question + its options
 * as a numbered list. This is the permission *view* — it is never answerable
 * here (the hook can't carry a choice back), so no inputs/buttons (R-35.1). */
function renderQuestionBlock(q: Record<string, unknown>): HTMLElement {
  const header = typeof q.header === 'string' ? q.header : '';
  const options = Array.isArray(q.options) ? q.options : [];
  const optEls = options.map((opt, i) => {
    const rec = asRecord(opt);
    // Native options are `{label, description}`; tolerate a plain string too.
    const label = rec ? permValueStr(rec.label ?? rec.option) : permValueStr(opt);
    return h('li', { className: 'qd-perm-q-option' }, [
      h('span', { className: 'qd-ask-option-key' }, [String(i + 1)]),
      h('span', { className: 'qd-ask-option-text' }, [truncate(label, PERM_VALUE_MAX)]),
    ]);
  });
  return h('div', { className: 'qd-perm-q' }, [
    header ? h('div', { className: 'qd-ask-q-header' }, [header]) : null,
    h('div', { className: 'qd-ask-question qd-perm-q-question' }, [truncate(permValueStr(q.question), PERM_VALUE_MAX)]),
    optEls.length > 0 ? h('ol', { className: 'qd-perm-q-options' }, optEls) : null,
  ]);
}

/** AskUserQuestion: every question block, read-only (R-35.1). */
function renderQuestionInput(input: Record<string, unknown>): HTMLElement {
  const questions = Array.isArray(input.questions) ? input.questions : null;
  if (questions && questions.length > 0) {
    return h('div', { className: 'qd-perm-body' }, questions.map((q) => renderQuestionBlock(asRecord(q) ?? {})));
  }
  // A single top-level `question` (no `questions` array) is still renderable.
  if (typeof input.question === 'string') {
    return h('div', { className: 'qd-perm-body' }, [renderQuestionBlock(input)]);
  }
  return renderKeyValueInput(input);
}

/** Unknown/other tool: the parsed object as `key: value` rows, mono values — a
 * readable render, never a raw JSON blob (R-35.1). */
function renderKeyValueInput(input: Record<string, unknown>): HTMLElement {
  const entries = Object.entries(input);
  if (entries.length === 0) return h('pre', { className: 'qd-perm-input mono' }, ['(no input)']);
  return h(
    'div',
    { className: 'qd-perm-body qd-perm-kv' },
    entries.map(([k, v]) => permLineField(k, truncate(permValueStr(v), PERM_VALUE_MAX))),
  );
}

/** SPEC §35.1: render a permission request's `toolInput` (a JSON string, already
 * shell-sanitized + §28-decoded) as a structured, tool-aware block keyed on
 * `perm.toolName`, replacing the raw `<pre>` dump. On a `JSON.parse` failure
 * (a truncated/oversized input, R-16.5) — or a JSON scalar/array with nothing
 * to key on — it keeps the verbatim `<pre>` so a fragment still shows. */
function renderPermInput(perm: PermRow): HTMLElement {
  const raw = perm.toolInput || '';
  let parsed: Record<string, unknown>;
  try {
    const value: unknown = JSON.parse(raw);
    const rec = asRecord(value);
    if (!rec) return h('pre', { className: 'qd-perm-input mono' }, [raw || '(no input)']);
    parsed = rec;
  } catch {
    return h('pre', { className: 'qd-perm-input mono' }, [raw || '(no input)']);
  }

  switch (perm.toolName) {
    case 'Bash':
      return renderBashInput(parsed);
    case 'Write':
      return renderWriteInput(parsed);
    case 'Edit':
    case 'MultiEdit':
      return renderEditInput(parsed);
    case 'Read':
    case 'Glob':
    case 'Grep':
    case 'LS':
      return renderPathInput(parsed);
    case 'AskUserQuestion':
      return renderQuestionInput(parsed);
    default:
      return renderKeyValueInput(parsed);
  }
}

/** SPEC §16 (R-16.2) / §35.1: the permission modal — amber accent, "<project>
 * requests permission", tool name + a structured, tool-aware render of the input
 * (R-35.1), and type-aware actions. A normal tool keeps Allow / Deny / In
 * terminal (+ the §32 expired disabling); an `AskUserQuestion` — which the
 * permission channel can't actually answer — drops Allow, leaving "In terminal"
 * (defer, the real path) + Deny plus a one-line hint. */
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

  // R-35.1 / §49: an AskUserQuestion arriving through the permission channel is a
  // question, not a permission-to-run. It can't be answered here (the hook only
  // returns allow/deny/defer, never the user's choice) — so it shows NO Allow,
  // only "In terminal" (defer) + Deny + a hint, and its identity tag reads
  // "asking you" rather than the generic "requests permission".
  const isQuestion = perm.toolName === 'AskUserQuestion';

  const identity = h('div', { className: 'qd-ask-identity qd-perm-identity' }, [
    dot,
    h('span', { className: 'qd-ask-identity-project' }, [
      label,
    ]),
    h('span', { className: 'qd-perm-tag mono' }, [
      expired ? 'expired' : isQuestion ? 'asking you' : 'requests permission',
    ]),
  ]);

  const deny = h(
    'button',
    { className: 'qd-btn qd-perm-deny', type: 'button', onclick: () => sendPerm(perm, 'deny') },
    ['Deny'],
  ) as HTMLButtonElement;
  const defer = h(
    'button',
    { className: 'qd-btn qd-btn-ghost qd-perm-defer', type: 'button', onclick: () => sendPerm(perm, 'defer') },
    ['In terminal'],
  ) as HTMLButtonElement;
  const allow = isQuestion
    ? null
    : (h(
        'button',
        { className: 'qd-btn qd-btn-primary qd-perm-allow', type: 'button', onclick: () => sendPerm(perm, 'allow') },
        ['Allow'],
      ) as HTMLButtonElement);
  if (expired) {
    if (allow) allow.disabled = true;
    deny.disabled = true;
  }

  // Question: "In terminal" (the real path) leads, then Deny. Normal tool: the
  // familiar Allow / Deny / In terminal.
  const actions = isQuestion ? [defer, deny] : [allow as HTMLButtonElement, deny, defer];

  elContent.append(
    h('div', { className: 'qd-perm' }, [
      identity,
      h('div', { className: 'qd-ask-question qd-perm-tool' }, [
        isQuestion ? 'Claude is asking a question' : `Run ${perm.toolName}?`,
      ]),
      renderPermInput(perm),
      isQuestion
        ? h('p', { className: 'qd-perm-hint' }, ['Answer in the terminal — or have Claude ask via the ask_user tool.'])
        : null,
      h('div', { className: 'qd-ask-actions qd-perm-actions' }, actions),
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

/** SPEC §35.2 auto-size: the ask window's intrinsic content height, `header +
 * true content height`. Mirrors `popup.ts`'s `measureAutoHeight` — `#app` is
 * normally stretched to the OS window height (`height: 100%`), so its flex-grow
 * `.qd-ask-content` child re-stretches to fill it and would mask SHRINKING (a
 * long perm collapsing to a short one would never report a smaller height).
 * Momentarily letting `#app` size to its own content removes that ambient
 * stretch so `elContent.scrollHeight` reflects the real intrinsic height;
 * restoring right after is a same-tick style read+write that paints no
 * intermediate frame. */
function measureAskHeight(): number {
  const appEl = document.getElementById('app') as HTMLElement | null;
  const header = document.querySelector('.qd-ask .qd-header') as HTMLElement | null;
  const prevHeight = appEl?.style.height ?? '';
  const prevMaxHeight = appEl?.style.maxHeight ?? '';
  if (appEl) {
    appEl.style.height = 'auto';
    appEl.style.maxHeight = 'none';
  }
  const headerH = header?.offsetHeight ?? 0;
  const contentH = elContent.scrollHeight;
  if (appEl) {
    appEl.style.height = prevHeight;
    appEl.style.maxHeight = prevMaxHeight;
  }
  return headerH + contentH;
}

/** SPEC §35.2: report the measured content height so the shell can size the ask
 * window (clamped to 140..=640 in Rust). Snaps directly — the window resize
 * isn't animated, so there's nothing for reduced-motion to disable — and is
 * skipped when the height hasn't meaningfully changed, so an unrelated
 * `deck://state` re-push can't thrash the window. Reported even in mock/browser
 * mode (no window to size there, but the number itself is test-observable). */
function syncAskHeight(): void {
  const total = measureAskHeight();
  if (lastAskHeight !== null && Math.abs(total - lastAskHeight) < 1) return;
  lastAskHeight = total;
  void invoke('resize_ask', { contentHeight: total }).catch(() => undefined);
}

onState((snap) => {
  latest = snap;
  render(snap);
  syncAskHeight();
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
    // R-35.1: an AskUserQuestion perm has no Allow, so its A shortcut is inert;
    // Esc still = In terminal (defer), and D still = Deny.
    const isQuestion = primaryPerm.toolName === 'AskUserQuestion';
    if (key === 'a' && !permExpired && !isQuestion) {
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
