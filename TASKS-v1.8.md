# Quarterdeck v1.8 — round-7 (1 item, §48)

Single task + gate. No git commits by agents.

## T1 — §48 lamp remembers its own position across expand/collapse
Bug: the compact traffic-light (lamp) jumps after an expand→collapse. Root cause: ONE persisted position (`popupPos`, `settings.rs:208`) is shared by BOTH the list popup and the lamp. Dragging the lamp saves `popupPos=A`; expanding to the list grows 360x460 from A and the §39 clamp / `resize_popup_to_content` MOVE it to A′, which the `Moved` handler (`windows.rs:174`) persists back into the same `popupPos`; collapsing to lamp then lands at A′, not A.

Fix: give each popup MODE its own remembered position so the list's resize/clamp never corrupts the lamp's spot.
- **settings.rs:** add a second persisted position. Keep `popup_pos` (rename its ROLE to the LIST position, or add `lamp_pos: Option<PopupPos>` alongside `popup_pos`). Back-compat: an existing persisted `popupPos` continues to mean the LIST position; `lampPos` is a new optional key (unknown-keys preserved either way). Prefer: keep `popup_pos` = list position, add `lamp_pos: Option<PopupPos>` (serde `lampPos`, default None).
- **windows.rs:**
  - The `Moved` handler (`persist_user_move` / `maybe_persist_popup_pos` ~:162-185) writes to the CURRENT mode's position: `PopupMode::Lamp` -> `lamp_pos`, `PopupMode::List` -> `popup_pos` (list). Keep the R-25.2 debounce.
  - `set_popup_mode` (~switch): when switching TO `Lamp`, restore the saved `lamp_pos` (if any) via `set_position` (marked programmatic with `note_programmatic_move()` so the Moved handler doesn't treat it as a user drag); when switching TO `List`, restore the saved `popup_pos` (list) if any. Then keep the existing `clamp_popup_onto_screen` (§39) so the restored spot is still on-screen. If the target mode has no saved position yet, keep current behavior (grow/anchor as today).
  - `restore_popup_position` at startup (`~:471`): restore the position for the PERSISTED mode (lamp_pos if the app starts in lamp mode, else popup_pos), marking it user-moved so the first content resize grows in place.
- Only the lamp is drag-persisted while pinned (R-25.2) — keep that gating; the point is that lamp and list positions are now independent, so toggling between them is stable.
AC: unit tests for the mode->position selection (Moved writes to the right field; set_popup_mode restores the right field) using the existing settings/persist test seams; a pure test that expanding then collapsing does not change the lamp's stored position. Manual note for Philipp: drag the lamp, expand, collapse — it returns to where it was. cargo build/clippy/test --workspace + npm run ui:build + npx tsc --noEmit + Playwright all green. No dead-code warnings.

## T2 — integration gate
`cargo build/clippy/test --workspace` + `npm run ui:build` + `npx tsc --noEmit` + Playwright, green from clean. Report honestly; no commits.
