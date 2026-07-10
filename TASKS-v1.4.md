# Quarterdeck v1.4 — round-3 dogfooding (2 tasks)

Sequential build (shared files: engine.rs / naming.rs / lib.rs / popup.ts). Each task: build -> independent spec-check -> fix loop. No git commits by agents (Philipp commits later).
Order: T1 (cut click-to-focus) -> T2 (aiTitle default naming).

---

## T1 — §33 CUT click-to-focus entirely (BUG: it hard-hangs the app)
Reason: click-to-focus spawns a synchronous PowerShell that uses `AttachThreadInput` (added in §26.2) — a classic input-queue deadlock that FREEZES Quarterdeck, and sometimes only surfaces "Couldn't find the terminal window". Philipp decided to remove the whole feature (he'll navigate by matching the row name to the terminal tab, see T2). REMOVE it cleanly across every layer; keep everything else green.

Remove:
- **Delete** `src-tauri/src/focus.rs` and its `mod focus;` declaration in `src-tauri/src/lib.rs`.
- `src-tauri/src/ipc.rs`: delete the `focus_terminal` #[tauri::command] (~:537) and its item in the `generate_handler!` list in lib.rs (~:2906).
- `src-tauri/src/lib.rs`: delete `Shell::focus_terminal` (~:969), `focus_terminal_command` (~:2772), and any `focus::` references / the `NOT_FOUND_MSG` usage.
- `ui/src/popup.ts`: remove the row `onclick` that calls `focusTerminal(row.id)` (~:134-136), delete the `focusTerminal()` function (~:235) and the inline "Couldn't find the terminal window" notice. The row's single click now does nothing; **double-click-to-rename (§27) and the right-click context menu must remain**. Remove any "Focus terminal" context-menu item if present.
- `ui/src/ipc-contract.ts`: remove the `focus_terminal` command from the `Commands` interface.
- `ui/src/tauri-mock.ts`: remove the `focus_terminal` mock handler.
- **Ancestor plumbing** (was only for click-to-focus): remove `Ancestor` capture and storage — `crates/deck-core/src/events.rs` (`Ancestor`, `RawAncestor`, `ancestor_from_raw`, the `ancestor` fields on the SessionStart event/spool structs), `crates/deck-core/src/engine.rs` (`Session.ancestor` field, `ancestor_of`, the `use ... Ancestor`), and the ancestor half of `Get-SessionStartExtra` in `hooks/quarterdeck-hook.ps1` + `hooks/quarterdeck-hook.sh`. **KEEP the `claudePid` half of the ancestor walk — it feeds liveness (R-6) and MUST stay.** If removing Ancestor from the hook is risky, at minimum stop emitting it and drop it app-side; prefer full clean removal.
- **Delete** `e2e/tests/focus.spec.ts`. Remove any focus assertions from other e2e specs. Remove `focus.rs` unit tests (they go with the file). Remove/adjust any deck-core tests referencing `ancestor`/`ancestor_of`.
- `skills/quarterdeck/SKILL.md` / any spec text: drop click-to-focus mentions if present.

Do NOT touch `src-tauri/src/util.rs` (the CREATE_NO_WINDOW helper) or `foreground.rs` — those stay (used by the foreground sampler).

AC: `cargo build --workspace` + `cargo clippy --workspace -- -D warnings` clean (no dead-code/unused warnings from the removal); `cargo test --workspace` green; `npm run ui:build` + Playwright green (with focus.spec.ts gone). Grep proves zero remaining `focus_terminal` / `focusTerminal` / `ancestor_of` references. The app no longer has any row-click terminal-focus path.

## T2 — §34 default row title = Claude's aiTitle (terminal-tab chat name)
Today the default title is `registry_name` = the derived `phily-XX` handle (`nameSource:"derived"`), which is NOT what shows on the terminal tab. The terminal-tab chat name is the transcript's **`aiTitle`** field (e.g. "Работа над поиском работы", "Тестирование приложения Dreambook перед релизом"), found in `~/.claude/projects/<enc-cwd>/<sessionId>.jsonl`. Make that the default so a Quarterdeck row matches its terminal tab (enables manual navigation, replacing click-to-focus). Philipp's Quarterdeck rename (§27) still wins; an explicit Claude-side `/rename` (registry `nameSource:"user"`) also wins over aiTitle.

LOCKED precedence (highest -> lowest):
```
1. override_name                         (Quarterdeck §27 rename)
2. registry_name  IF nameSource == "user"   (explicit `/rename` in Claude)
3. ai_title                              (transcript aiTitle = terminal tab name)
4. registry_name  (nameSource "derived"/absent = phily-XX)
5. session_title
6. latest_prompt
7. transcript_fallback
```

Implement:
- **Registry parse** (`crates/deck-core/src/registry.rs`): parse a new `name_source: Option<String>` from the session json `nameSource` field (defensive, like the other fields). Carry it on `RegistryEntry`.
- **Engine** (`crates/deck-core/src/engine.rs`): add `ai_title: Option<String>` and `registry_name_is_user: bool` (from `nameSource=="user"`) to `Session`. `apply_registry_entry` sets `registry_name_is_user`. Extend the precedence: replace `naming::title_with_override(...)` with a call carrying the 7 candidates above (add a `naming` primitive `title_full(override, user_registry, ai_title, derived_registry, session_title, latest_prompt, transcript_fallback)` or extend the existing one — keep it reusing `normalize_title`/`pick_title`). Add `SessionStore::set_ai_title(id, Option<String>) -> bool` (updates the live row + recomputes title, returns whether display_title changed), mirroring the registry_name update path.
- **aiTitle extraction — pure, in deck-core, unit-testable** (`crates/deck-core/src/naming.rs` or a small `transcript.rs`): `extract_ai_title(bytes: &[u8]) -> Option<String>` that finds the LAST `"aiTitle":"..."` occurrence and JSON-unescapes the value (aiTitle appears on many lines; the last is authoritative; Cyrillic/UTF-8 must survive; ignore null/empty). Runs on a byte slice so the shell can pass it a tail read.
- **Shell I/O** (`src-tauri/src/lib.rs`): on the tick (reuse the same cadence that already stats transcript mtime), for each session whose transcript mtime advanced since the last aiTitle read, read the **tail** of the transcript (last ~128 KB is plenty — aiTitle is written on recent lines; do NOT read whole multi-MB files), call `extract_ai_title`, and `store.set_ai_title(id, ...)` when it changed; push state if a title changed. Cache last-read mtime per session to avoid re-reading unchanged files. Keep the read cheap and failure-tolerant (unreadable/missing transcript -> leave ai_title as-is).
- Sanitize via the existing `normalize_title` (bidi-strip + 60-grapheme cap) as part of `pick_title`.

AC: unit tests — `extract_ai_title` returns the LAST aiTitle, handles Cyrillic, missing/empty -> None, JSON escapes decoded; naming precedence tests proving each rung wins in order (override > user-registry > aiTitle > derived-registry > session_title > prompt). deck-core `cargo test` + clippy green. Shell compiles; add a test for the tail-read/mtime-gated refresh if feasible. Manual note for Philipp: rows should show the terminal-tab chat name by default.

## T3 — integration gate
`cargo build/clippy/test --workspace` + `npm run ui:build` + Playwright, all green from clean. Report honestly; no commits.
