# Quarterdeck v1.7 — round-6 (3 items, §45–§47)

Sequential build. Order: T1 flap-fix (engine) -> T2 dual-answer (ask layers + skill) -> T3 kill reminder (engine/ui). No git commits by agents.

---

## T1 — §45 fix status flapping: gate the §44 registry-demote on transcript quiescence
Bug (regression in §44): a session flaps 🟡→🟢→🟡. `maybe_registry_demote` (`crates/deck-core/src/engine.rs` ~:428) demotes hook-working→idle when the registry status is quiescent (`idle`/`waiting`), fresh, and `updated > last_activity`. But Claude Code writes `waiting` MID-TURN (waiting on a tool/permission), not only when the turn actually ends — so while the agent is actively working (transcript advancing), a transient `waiting` triggers a false demote to idle ("finished"), then §30 `poll_recovery` re-promotes on the next transcript advance → yellow again. That flap also fires a spurious second "finished" toast.
Fix: the §44 demote must ALSO require the transcript to be QUIESCENT (agent not actively writing). A genuinely-working agent writes its transcript continuously, so it must never be demoted; an ESC-interrupt stops the transcript, so it still demotes correctly.
- Thread the transcript mtime into the demote decision. `maybe_registry_demote` is called on the tick where `mtime_of` is available (see `tick`/`poll_*`); pass the current transcript mtime (or a `transcript_recently_advanced: bool`) in. Demote only when, in addition to the existing guards, the transcript mtime has NOT advanced within `RECOVERY_MIN_ADVANCE_MS` (~2s) of `now` (reuse the §30 constant + the last-seen mtime bookkeeping).
- Keep all existing §44 guards (genuine hook `working`, not `recovery_promoted`, registry quiescent + fresh + `updated > last_activity`).
AC: engine tests — (a) a working session whose registry momentarily reports `waiting` while the transcript keeps advancing is NOT demoted (no flap); (b) an ESC-interrupt (registry idle/waiting + transcript quiescent) still demotes to idle; (c) interplay with §30 reverse-gear + busy-override unchanged. cargo build/clippy/test green.

## T2 — §46 dual-answer: answer a question in the deck OR in the terminal
Give the user both paths for one question, without denying the native tool. Default: questions come via our `ask_user` MCP tool and render an answerable form in the always-on-top deck popup (answerable from anywhere on screen). Add a one-click escape to the terminal.
- **New answer kind `terminal`.** Add `AskAnswerKind::Terminal` (Rust `src-tauri/src/ipc.rs`, `ui/src/ipc-contract.ts`, and an `AskAnswer::terminal()` + wiring in `src-tauri/src/mcp_server.rs` so `ask_user` returns `{answer:"", kind:"terminal", ask_id}` to the caller). Serialize as lowercase `"terminal"`. All new-kind additions must stay backward-compatible (serde).
- **"In terminal" button in the ask FORM** (`ui/src/ask.ts` — `renderAsk`/`renderOptions`/`renderFreeform`/`renderForm`): a grey/secondary button that sends `answer_ask(askId, "", 'terminal')`. Distinct from the existing Dismiss. Present on both the single-question and multi-question (§29) forms; also mirror the option in the popup ask-mirror row where applicable. Keep it out of the way (secondary styling).
- **Skill (`skills/quarterdeck/SKILL.md`):** document that `ask_user` may return `kind:"terminal"` — when it does, the user chose to answer in the terminal, so the agent should RE-ASK the same question using the native `AskUserQuestion` tool (which renders the terminal picker). Update the `ask_user` tool description in `mcp_server.rs` to list `terminal` in the returned `kind`s and explain the re-ask contract. Do NOT deny/disable the native `AskUserQuestion` — both paths stay alive.
AC: unit — `ask_user` returns `kind:"terminal"` when the deck sends that kind; existing option/text/form/dismiss/timeout/cancel paths unchanged; old callers/persisted files still deserialize. Playwright — the ask form shows an "In terminal" button that resolves the ask with kind terminal. SKILL.md reads coherently. cargo + npm green.

## T3 — §47 mute the redundant "still waiting" reminder
The `idle_prompt` Notification fires right after `Stop`, so the Reminder toast ("still waiting for instructions") always duplicates the just-shown "finished" toast — useless. Mute it.
- Stop emitting the `ToastKind::Reminder` decision on `idle_prompt` (`crates/deck-core/src/engine.rs` ~:940-953, the `NotifClass::IdlePrompt` arm) — `idle_prompt` now changes nothing and fires no toast (keep the debug log). 
- Remove the now-dead "reminder" toggle from the settings pane UI (`ui/src/popup.ts` settings render) so it isn't a no-op control; KEEP the `notifyReminder` field in `settings.rs`/the contract for backward-compat (unknown-keys preserved), just no longer surfaced/used. Update `fire_toasts` so the `ToastKind::Reminder` branch is unreachable/removed cleanly (no clippy dead-code).
- Update SPEC.md R-2.3 / R-9.5 to note the reminder is retired (may return later as a delayed nudge).
AC: engine tests — an `idle_prompt` notification produces NO toast decision (update/replace the reminder test); no other toast behavior changes; a `Stop` still fires exactly one "finished" toast. cargo + npm green; no dead-code warnings.

## T4 — integration gate
`cargo build/clippy/test --workspace` + `npm run ui:build` + `npx tsc --noEmit` + Playwright, all green from clean. Report honestly; no commits.
