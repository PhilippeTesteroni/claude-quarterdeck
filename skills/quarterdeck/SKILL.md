---
name: quarterdeck
description: >-
  Reach the human through Quarterdeck when you are running autonomously and hit a
  decision only they can make. Use the `ask_user` MCP tool to ask a blocking
  question (with options and a timeout) and the `notify_user` tool to send a
  fire-and-forget heads-up. Trigger this when you are blocked on a human choice,
  need approval before a risky or irreversible action, must resolve an ambiguity
  you cannot decide yourself, or want to flag that a long task finished or needs
  attention — and the user is NOT actively watching the terminal.
---

# Quarterdeck — asking the human while you work

Quarterdeck is a tray app that watches every Claude Code session on this machine
and shows the user who needs them. It also runs a small local MCP server that
lets you **reach the user directly** through an always-on-top popup, even when
they are in another window. Two tools are available (already configured — no
setup needed from you):

- `ask_user` — ask a question and **block** until the user answers, the timeout
  elapses, or they dismiss/cancel it.
- `update_ask` / `cancel_ask` — revise or cancel a still-pending question from a
  parallel tool call.
- `notify_user` — send a one-line notification and continue immediately.

## When to use `ask_user`

Use it during long, autonomous runs at the moments a human would want to be
consulted:

- You are **blocked on a decision only the user can make** (which of two designs,
  which account/environment, whether their intent was A or B).
- You are about to do something **risky or irreversible** and want a go/no-go
  (delete data, force-push, spend money, touch production).
- You hit an **ambiguity** you genuinely cannot resolve from the task, the repo,
  or context — and guessing wrong would waste real work.

Do **not** use it for things you can and should decide yourself. If you can make
a reasonable call and keep moving, do that. `ask_user` is for the human's call,
not to offload your judgment.

### How to call it well

```
ask_user(
  question: "Deploy build 41 to production now, or wait for the nightly?",
  options: ["Deploy now", "Wait for nightly"],   // offer options whenever the answer is a choice
  detail: "Build 41 passed CI 10 min ago; the nightly runs in ~6h and includes the pricing migration.",
  context: "<your current working directory>",    // REQUIRED — see below
  timeout_seconds: 300                             // optional; omit to wait indefinitely
)
```

- **Always pass `context` = your current working directory (cwd).** Quarterdeck
  uses it to attribute the question to the right session and to label the popup
  with the project name. Without it the ask shows as "Unknown agent".
- **Offer `options` when the answer is a choice.** The user gets one-tap buttons
  (and can still type free text). Keep options short and mutually exclusive.
- **Keep `question` short; put the reasoning in `detail`.** `question` is one
  short, specific decision. `detail` (optional) is the longer body/rationale the
  user needs to decide — it renders muted under the question, so move context
  out of `question` and into `detail` rather than cramming it all into one line.
- **`timeout_seconds` is optional.** Set it (max 3600) when the work can only
  wait so long. **Omit it (or pass 0) to make the ask persistent** — it waits
  indefinitely until the user answers, dismisses, or you `cancel_ask` it, and
  shows no countdown. Prefer persistent for genuine blockers you can't proceed
  past; use an explicit timeout when you have a sensible fallback.

### Asking several questions at once (a form)

For a small batch of related decisions, send a `questions` array instead of a
single `question` — the user gets one form with radio buttons (single-select)
or checkboxes (multi-select) per question, plus optional free text:

```
ask_user(
  questions: [
    { header: "Environment", question: "Which environment?", options: ["prod", "staging"] },
    { header: "Flags", question: "Extra flags?", multiSelect: true, options: ["--fast", "--safe", "--verbose"] },
  ],
  context: "<your current working directory>",
  timeout_seconds: 300
)
```

- Provide **EITHER** `question` (+ optional `options`) **OR** `questions[]`, not
  both — when `questions` is present, `question`/`options` are ignored.
- Each item is `{header?, question, multiSelect?, options[]}`. `multiSelect:true`
  → checkboxes (any number, including none); omitted/false → radio (exactly one).
- Caps enforced server-side: ≤8 questions, ≤12 options each. Keep forms short —
  this is for a handful of related choices, not a survey.

### The result and how to react

`ask_user` returns `{answer, kind, ask_id}` where `kind` is one of:

- `option` — the user picked one of your options; `answer` is that option.
- `text` — the user typed a free-text reply; `answer` is their text.
- `timeout` — no answer within `timeout_seconds` (only for non-persistent asks).
- `dismissed` — the user dismissed the question without answering.
- `cancelled` — a parallel `cancel_ask` withdrew the question (see below).
- `form` — the user submitted a `questions[]` form; `answer` is a JSON string
  `{"answers":[{header,question,selected:[...],text?}, ...]}` in the same order
  you sent the questions (`selected` is the chosen option strings for that
  question; `text` is present only if they also typed one). Parse it to read
  each answer.

Keep `ask_id` if a parallel task might need to revise or withdraw the question.

**Degrade gracefully.** On `timeout`, `dismissed`, or `cancelled`, do **not**
stall or ask again in a loop. Proceed on your best judgment, choose the
safe/reversible path, and clearly note in your final summary that you continued
without an answer and what you assumed — so the user can correct course.

### Revising or withdrawing a pending question (parallel calls)

`ask_user` **blocks**, so the call that is waiting cannot revise or cancel
itself. Use `update_ask` / `cancel_ask` from a **parallel tool call** (or a
different session) when the situation changes while a question is on screen:

```
update_ask(ask_id: "<from ask_user>", question: "...", options: [...], detail: "...")  // replace any field
cancel_ask(ask_id: "<from ask_user>")   // the blocked ask_user returns kind:"cancelled"
```

- Both act only on a **pending** ask; an unknown or already-settled `ask_id`
  returns an error result (not a crash) — treat it as "already resolved".
- **A blocked `ask_user` can't cancel itself.** If you fire `ask_user` and then
  want to withdraw it, the `cancel_ask` must come from another concurrent tool
  call in the same turn (parallel tool calls) or another session — not from
  after the (still-blocked) `ask_user` returns.

### Very long / persistent asks stay alive

While an `ask_user` call is blocked, Quarterdeck keeps the MCP call alive
automatically (it streams a progress heartbeat every 30s), so a persistent ask
survives long idle waits. For extreme autonomy you may also raise the per-server
`timeout` in your MCP config, but the default setup needs no extra flags.

### Do not spam

- **Batch decisions.** If you have several related questions, ask them together
  (one question listing the choices, or a single question whose options cover the
  branches) rather than firing many popups.
- **One ask per genuine blocker.** Don't re-ask the same thing; don't ask for
  confirmation of trivial steps.
- **Prefer the built-in `AskUserQuestion` when the user is actively interactive**
  (they're watching this conversation and replying). `ask_user` — including its
  `questions[]` form, which mirrors `AskUserQuestion`'s multi-question shape — is
  for when you are running autonomously and the user is elsewhere: it interrupts
  them with a system popup, so reserve it for when that interruption is
  warranted. When the user is right here, ask in-conversation instead.

## When to use `notify_user`

Fire-and-forget FYIs that need no answer:

```
notify_user(
  message: "Migration finished: 12,304 rows moved, 0 errors.",
  context: "<your current working directory>"
)
```

Good for: a long task completed, a milestone reached, or a non-blocking warning
you want the user to see without stopping your work. It returns `{delivered, id}`
immediately — never use it when you actually need an answer (use `ask_user`).

## Etiquette summary

- Ask only when a human's call is genuinely needed; otherwise decide and move on.
- Always send your cwd as `context`.
- Prefer options; keep the `question` tight and push rationale into `detail`.
- Omit `timeout_seconds` for a true blocker (persistent); set one when you have a
  fallback.
- On timeout/dismiss/cancel, proceed on best judgment and disclose the assumption.
- Revise/withdraw a live question with `update_ask`/`cancel_ask` from a parallel
  call — the blocked `ask_user` can't do it itself.
- Batch related questions; don't loop; prefer interactive `AskUserQuestion` when
  the user is right here with you.
