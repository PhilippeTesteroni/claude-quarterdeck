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
  elapses, or they dismiss it.
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
  context: "<your current working directory>",    // REQUIRED — see below
  timeout_seconds: 300                             // pick a sensible bound, max 600
)
```

- **Always pass `context` = your current working directory (cwd).** Quarterdeck
  uses it to attribute the question to the right session and to label the popup
  with the project name. Without it the ask shows as "Unknown agent".
- **Offer `options` when the answer is a choice.** The user gets one-tap buttons
  (and can still type free text). Keep options short and mutually exclusive.
- **Set a sensible `timeout_seconds`** for how long the work can wait (max 600).
  If omitted it defaults to 600.
- **Keep the question concise and specific** — one decision, phrased so a quick
  answer is possible. Put supporting detail the user needs to decide in the
  question text itself.

### The result and how to react

`ask_user` returns `{answer, kind}` where `kind` is one of:

- `option` — the user picked one of your options; `answer` is that option.
- `text` — the user typed a free-text reply; `answer` is their text.
- `timeout` — no answer within `timeout_seconds`.
- `dismissed` — the user dismissed the question without answering.

**Degrade gracefully.** On `timeout` or `dismissed`, do **not** stall or ask
again in a loop. Proceed on your best judgment, choose the safe/reversible path,
and clearly note in your final summary that you continued without an answer and
what you assumed — so the user can correct course.

### Do not spam

- **Batch decisions.** If you have several related questions, ask them together
  (one question listing the choices, or a single question whose options cover the
  branches) rather than firing many popups.
- **One ask per genuine blocker.** Don't re-ask the same thing; don't ask for
  confirmation of trivial steps.
- **Prefer the built-in `AskUserQuestion` when the user is actively interactive**
  (they're watching this conversation and replying). `ask_user` is for when you
  are running autonomously and the user is elsewhere — it interrupts them with a
  system popup, so reserve it for when that interruption is warranted.

## When to use `notify_user`

Fire-and-forget FYIs that need no answer:

```
notify_user(
  message: "Migration finished: 12,304 rows moved, 0 errors.",
  context: "<your current working directory>"
)
```

Good for: a long task completed, a milestone reached, or a non-blocking warning
you want the user to see without stopping your work. It returns immediately —
never use it when you actually need an answer (use `ask_user` for that).

## Etiquette summary

- Ask only when a human's call is genuinely needed; otherwise decide and move on.
- Always send your cwd as `context`.
- Prefer options; keep questions tight; set a sane timeout.
- On timeout/dismiss, proceed on best judgment and disclose the assumption.
- Batch related questions; don't loop; prefer interactive `AskUserQuestion` when
  the user is right here with you.
