# Quarterdeck v1.6 — round-5 dogfooding (9 items, §36–§44)

Sequential build (heavy shared files: engine.rs / popup.ts / styles.css / windows.rs / lib.rs). Each task: build -> independent spec-check -> fix loop. No git commits by agents.
Engine/status changes first (they add a new Status color the UI needs), then UI, then the isolated fixes.
Order: T1 timer -> T2 blue status + badge-counter-fix -> T3 ESC demote -> T4 kill -> T5 window-clamp -> T6 cursor -> T7 simplify multi-agent badge -> T8 roomier rows -> T9 per-agent pie lamp.

---

## T1 — §36 working-time timer (replace confusing time-in-status)
Today the row time = `now - entered_at_ms` (`engine.rs:583`), reset on EVERY effective-status change (incl. the §30 reverse-gear idle<->working flips) — so it resets unpredictably. New semantics:
- While 🟡 **working**: show a live counter of time spent working on the current task (turn), anchored at the real work start (UserPromptSubmit), NOT reset by §30 reverse-gear flips or busy-override toggles.
- When it stops (🟢 idle after a Stop): FREEZE and show "took <duration>" (the total working time of that turn) instead of a running idle timer.
- Reset on the next UserPromptSubmit.
Impl: add `work_started_ms: Option<u64>` (set when a session enters working from a real UserPromptSubmit; preserved across recovery flips) and `last_work_ms: Option<u64>` (set on Stop = stop_ts - work_started). Expose both on `SessionView`; `popup.ts` renders "Xm Ys" live while working, "took Xm" when idle-with-last_work, and nothing/started-ago for other states. Keep the existing 1s UI tick.
AC: engine unit tests — work timer survives a reverse-gear idle->working->idle flip without resetting; freezes correctly on Stop; resets on new prompt. Playwright — working row shows a running timer, idle row shows "took …". cargo + npm green.

## T2 — §43 "waiting for workflow" status (blue) + fix vanishing multi-agent indicator
Bug: the multi-agent indicator disappears the moment the parent session hits Stop, because §21 `settle_subagents` zeroes `active_subagents` on any non-working settle — even while background subagents/workflows are still running.
- Fix `settle_subagents`: do NOT zero the counter merely because the parent went idle/Stop while subagents are still open. Keep the Start/Stop balance; only clear on a genuine leak (e.g. liveness `dead`, or a bounded staleness), never on a clean Stop that still has open subagents.
- New status **`WaitingWorkflow` (blue, e.g. `#58a6ff`)**: when `active_subagents > 0` and the hook status is `idle` (parent turn ended but background multi-agent work continues), the row + tray + lamp show blue instead of green. Slots into the `Status` enum, `effective()`, sort priority (between working and idle), tray icon set (add a blue variant — reuse an existing asset tint or add one), and the row-dot / lamp color maps. A pending ask/perm (attention) still outranks it.
AC: engine tests — subagents active + hook idle => WaitingWorkflow(blue); counter is NOT zeroed by a Stop while a subagent is still open; a real SubagentStop balance returning to 0 clears it; dead still clears. UI shows blue. cargo + npm green.

## T3 — §44 ESC-interrupt updates status (registry-driven demote)
Bug: interrupting Claude with ESC leaves the deck stuck 🟡 — ESC fires no Stop hook, so `hook_status` stays Working, and Quarterdeck only uses the registry to OVERRIDE idle->working, never to demote. Claude writes the registry status to idle/`waiting` on interrupt.
- Let the registry DEMOTE: when the registry reports the session `idle`/`waiting` (fresh `updatedAt`) but `hook_status == Working` with no fresher hook activity, drop the effective status to idle. Implement as a registry-driven demote in `apply_registry_entry` / the tick (complement to §30's transcript-quiescence demote; do not fight a genuinely-busy registry). Handle the `waiting` status string too (map appropriately).
AC: engine tests — registry flips busy->idle/waiting with hook stuck Working + stale hook activity => effective demotes to idle; a busy registry does NOT demote; interplay with §30 reverse-gear and busy-override stays correct. cargo green.

## T4 — §38 kill-agent-process button
`claude_pid` exists in the engine but is not exposed to the UI. Plumb it onto `SessionRow` (`pid?: number`), add a `kill_session(sessionId)` #[tauri::command] that resolves the session's claude pid and force-terminates it (Windows `taskkill /PID <pid> /F`; mac `kill`), then removes the row (reuse the remove-row path). Add a kill affordance to the row's right-click context menu ("Kill process") — guarded/confirm-free but clearly labelled; only shown when a pid is known.
- Also investigate the lingering-row cause: stale `~/.claude/sessions/*.json` files that Claude Code didn't delete keep ghost rows alive; tighten liveness so a registry-backed row whose pid is gone (or whose registry file is stale beyond a window) goes `dead` promptly. Document what you change.
AC: a `kill_session` on a fake/injected pid path is unit-testable (pure resolve + the taskkill command shape); context-menu item appears only with a pid; removing works. cargo + npm green.

## T5 — §39 keep the window on-screen on restore/expand
Collapsing to the lamp (~56x56) then expanding can restore the popup partly/fully off-screen. Clamp the popup's top-left into the current monitor work area on every mode restore / `set_size` / `set_popup_mode` transition (reuse the horizontal/vertical clamp logic from `compute_anchor_position`). Ensure a lamp positioned near a screen edge expands back fully within the work area.
AC: a pure clamp unit test (window rect + work area -> on-screen rect); manual note for Philipp. cargo green.

## T6 — §40 cursor: text-caret only on the editable row
Hovering any text in the deck shows the I-beam text cursor. Set `cursor: default` + `user-select: none` on the deck chrome (rows, header, lamp, settings labels), and allow `cursor: text` + text selection ONLY on the §27 rename `<input>` (and any genuinely editable/inputs). Keep copy-session-id affordances working (they use a click, not selection). CSS-only in `styles.css`.
AC: Playwright/asserted — a row title has `cursor: default`; the rename input has `cursor: text`. npm green.

## T7 — §37 simplify the multi-agent indicator to a plain icon
The `⛭ N` badge with aggregated subagent spend (§23.3) confuses (the token figure is cumulative, not per-flow). Replace it with a **simple multi-agent glyph** (an icon meaning "multi-agent activity", no counts, no token/spend numbers). Keep the main per-session `ctx% · spend` line as-is (do NOT change that). Remove only the subagent spend/detail from the badge; the glyph shows whenever `active_subagents > 0` (i.e. the WaitingWorkflow/working-with-subagents case). `popup.ts` (the badge render ~:222) + `styles.css`.
AC: Playwright — the multi-agent glyph shows when subagents active, carries no number/token text; the row usage line is unchanged. npm green.

## T8 — §42 roomier agent rows (a couple of lines, larger)
Give each agent row more space and lay it out over ~2 lines with breathing room. Increase `.qd-row` vertical padding + type sizes (`styles.css:499` etc.), restructure `renderSessionRow` (`popup.ts:167`) into a clean two-line layout: line 1 = status dot + name (aiTitle) + right-aligned time (§36); line 2 = project/branch + `ctx% · spend` + the multi-agent glyph (§37). Keep it dense-but-calm (Mission Control). Verify the §31 fixed 5-row settings height still fits under the 560 cap after rows grow (it measures a real row, so it should scale — confirm it doesn't exceed the band).
AC: Playwright — a row renders two lines with the new spacing; light+dark ok; reduced-motion ok; §31 settings height still valid. npm green.

## T9 — §41 compact lamp = per-agent pie
Replace the single worst-of dot in lamp mode (`renderLamp` `popup.ts:699`, ~56x56) with an **SVG pie: one segment per agent**, each segment colored by that agent's current status (red/yellow/green/blue/gray), equal-sized slices (N agents -> N equal wedges). Zero agents -> neutral ring. Keep it crisp at 56px; keep the pinned lamp draggable (§25) and the click-to-expand. Reuse the status color tokens (incl. the §43 blue).
AC: Playwright/asserted — N agents render N wedges with per-status fills; 0 agents -> neutral; light+dark ok. npm green.

## T10 — integration gate
`cargo build/clippy/test --workspace` + `npm run ui:build` + `npx tsc --noEmit` + Playwright, all green from clean. Report honestly; no commits.
