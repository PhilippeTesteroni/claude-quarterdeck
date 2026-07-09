//! §27 rename-by-double-click: the user title override layer in the engine
//! (R-27.1 highest precedence, R-27.2 live re-derive, R-27.4 empty clears,
//! R-27.6 prune on SessionEnd, R-27.3 persisted-map seeding on (re)create).

mod common;

use std::collections::HashMap;

use common::*;

const T0: u64 = 1_751_000_000_000;

#[test]
fn override_wins_over_registry_name_and_survives_a_state_rebuild() {
    // R-27.1/R-27.2: a user override beats even the registry `name`, and because
    // it lives on the session (mirrored from the store map) it survives every
    // subsequent snapshot/`recompute_title`.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full(
        "a",
        "/home/dev/quarterdeck",
        None,
        None,
        Some("Original session title"),
        T0,
    ));
    assert_eq!(s.title_of("a").as_deref(), Some("Original session title"));

    let changed = s.set_override_name("a", Some("My renamed session".to_string()));
    assert!(changed, "renaming changed the display title");
    assert_eq!(s.title_of("a").as_deref(), Some("My renamed session"));

    // A later prompt (which recomputes the title from sources) must NOT dislodge
    // the override — it is the highest layer.
    s.on_event(&prompt("a", "keep working on the thing", T0 + 5_000));
    assert_eq!(s.title_of("a").as_deref(), Some("My renamed session"));
}

#[test]
fn empty_override_clears_and_falls_back_to_the_normal_chain() {
    // R-27.4: an empty/whitespace name clears the override, restoring the
    // session-title-derived name.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full(
        "a",
        "/p",
        None,
        None,
        Some("Chain title"),
        T0,
    ));
    s.set_override_name("a", Some("Renamed".to_string()));
    assert_eq!(s.title_of("a").as_deref(), Some("Renamed"));

    let changed = s.set_override_name("a", Some("   ".to_string()));
    assert!(changed, "clearing the override changed the display title");
    assert_eq!(s.title_of("a").as_deref(), Some("Chain title"));
    assert_eq!(s.override_name_of("a"), None);
}

#[test]
fn override_persists_in_the_store_map_and_reseeds_a_recreated_row() {
    // R-27.3/R-27.6: the override lives in the store's persisted map; a reused id
    // (SessionEnd then a later SessionStart) inherits it via the map ONLY if the
    // override outlived the end — but R-27.6 prunes on SessionEnd, so a genuinely
    // ended session's override is dropped. Seeding a NEW (never-ended) id from the
    // loaded map, however, must apply on first materialization.
    let (mut s, _c) = store_at(T0);
    let mut loaded = HashMap::new();
    loaded.insert("a".to_string(), "Loaded name".to_string());
    s.set_overrides(loaded);
    // The map is authoritative but sets no dirty flag (it IS the on-disk state).
    assert!(!s.take_overrides_dirty());

    // First event for `a` materializes the row, which inherits the loaded name.
    s.on_event(&session_start_full(
        "a",
        "/p",
        None,
        None,
        Some("Hook title"),
        T0,
    ));
    assert_eq!(s.title_of("a").as_deref(), Some("Loaded name"));
}

#[test]
fn session_end_prunes_the_override_and_marks_the_map_dirty() {
    // R-27.6: a reused id never inherits a stale name — SessionEnd drops it from
    // the map (and flags a re-persist).
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full("a", "/p", None, None, Some("Title"), T0));
    s.set_override_name("a", Some("Renamed".to_string()));
    assert_eq!(s.override_name_of("a").as_deref(), Some("Renamed"));
    let _ = s.take_overrides_dirty();

    s.on_event(&session_end("a", "clear", T0 + 10_000));
    assert_eq!(s.override_name_of("a"), None, "override pruned on end");
    assert!(s.take_overrides_dirty(), "prune flags a re-persist");

    // A brand-new incarnation of the id (a later start) starts clean.
    s.on_event(&session_start_full(
        "a",
        "/p",
        None,
        None,
        Some("Fresh title"),
        T0 + 20_000,
    ));
    assert_eq!(s.title_of("a").as_deref(), Some("Fresh title"));
}

#[test]
fn set_override_name_flags_the_map_dirty_for_persistence() {
    // R-27.3: every rename marks the map dirty so the shell re-persists.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    let _ = s.take_overrides_dirty();
    s.set_override_name("a", Some("X".to_string()));
    assert!(s.take_overrides_dirty());
    // And the snapshot exposes it for the on-disk write.
    s.set_override_name("a", Some("Y".to_string()));
    assert_eq!(s.overrides_snapshot().get("a").map(String::as_str), Some("Y"));
}
