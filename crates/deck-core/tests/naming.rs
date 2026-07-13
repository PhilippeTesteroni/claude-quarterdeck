//! R-5.2 title precedence, R-5.3 Cyrillic/Unicode safety, project basename.

use std::fs;

use deck_core::naming::{
    derive_title, extract_ai_title, normalize_title, project_name, strip_bidi_controls,
    title_from_sources, title_full, title_with_override, transcript_cwd,
    transcript_first_user_text, NO_TITLE, UNKNOWN_PROJECT,
};
use tempfile::tempdir;

// --- §34 default-title precedence (R-34) -----------------------------------

/// All seven rungs present, in order:
/// override > user-registry > aiTitle > derived-registry > session_title >
/// prompt > transcript_fallback.
fn full(
    ovr: Option<&str>,
    user_reg: Option<&str>,
    ai: Option<&str>,
    derived_reg: Option<&str>,
    sess: Option<&str>,
    prompt: Option<&str>,
    fallback: Option<&str>,
) -> String {
    title_full(ovr, user_reg, ai, derived_reg, sess, prompt, fallback)
}

#[test]
fn s34_each_rung_wins_in_order() {
    // 1. Quarterdeck §27 override beats everything.
    assert_eq!(
        full(
            Some("override"),
            Some("user-rename"),
            Some("aiTitle"),
            Some("phily-42"),
            Some("session"),
            Some("prompt"),
            Some("fallback"),
        ),
        "override"
    );
    // 2. An explicit Claude `/rename` (user registry) beats the aiTitle.
    assert_eq!(
        full(
            None,
            Some("user-rename"),
            Some("aiTitle"),
            Some("phily-42"),
            Some("session"),
            Some("prompt"),
            Some("fallback"),
        ),
        "user-rename"
    );
    // 3. THE §34 CHANGE: the transcript aiTitle is the default — it beats the
    //    derived phily-XX registry handle and everything below it.
    assert_eq!(
        full(
            None,
            None,
            Some("Работа над поиском работы"),
            Some("phily-42"),
            Some("session"),
            Some("prompt"),
            Some("fallback"),
        ),
        "Работа над поиском работы"
    );
    // 4. No aiTitle → the derived registry handle wins.
    assert_eq!(
        full(
            None,
            None,
            None,
            Some("phily-42"),
            Some("session"),
            Some("prompt"),
            Some("fallback"),
        ),
        "phily-42"
    );
    // 5. session_title, 6. prompt, 7. fallback each surface in turn.
    assert_eq!(
        full(
            None,
            None,
            None,
            None,
            Some("session"),
            Some("prompt"),
            Some("fallback")
        ),
        "session"
    );
    assert_eq!(
        full(
            None,
            None,
            None,
            None,
            None,
            Some("prompt"),
            Some("fallback")
        ),
        "prompt"
    );
    assert_eq!(
        full(None, None, None, None, None, None, Some("fallback")),
        "fallback"
    );
    // Nothing at all → placeholder.
    assert_eq!(full(None, None, None, None, None, None, None), NO_TITLE);
}

#[test]
fn s34_blank_rungs_fall_through_and_normalize() {
    // A blank aiTitle is ignored; the derived registry handle wins, and the
    // winner still rides normalize_title (bidi strip + whitespace collapse).
    let t = full(
        None,
        Some("   "),
        Some("  "),
        Some("phily-7  \u{202E}x\u{202C}"),
        None,
        None,
        None,
    );
    assert_eq!(t, "phily-7 x");
    // A Cyrillic aiTitle is capped at 60 grapheme clusters, not bytes.
    let long = "я".repeat(100);
    let capped = full(None, None, Some(&long), Some("phily-1"), None, None, None);
    assert_eq!(capped.chars().count(), 60);
    assert!(capped.ends_with('…'));
}

// --- §34 aiTitle extraction (R-34) -----------------------------------------

#[test]
fn extract_ai_title_takes_the_last_occurrence() {
    // aiTitle appears on many lines as the conversation evolves; the LAST wins.
    let body = concat!(
        "{\"type\":\"summary\",\"aiTitle\":\"Early guess\"}\n",
        "{\"type\":\"user\",\"content\":\"hi\"}\n",
        "{\"type\":\"summary\",\"aiTitle\":\"Final authoritative name\"}\n"
    );
    assert_eq!(
        extract_ai_title(body.as_bytes()).as_deref(),
        Some("Final authoritative name")
    );
}

#[test]
fn extract_ai_title_survives_cyrillic_and_json_escapes() {
    // Cyrillic UTF-8 literal bytes pass through; \" and \uXXXX decode.
    let body = "{\"aiTitle\":\"Тестирование приложения \\\"Dreambook\\\" перед \\u0440\\u0435\\u043b\\u0438\\u0437\\u043e\\u043c\"}";
    assert_eq!(
        extract_ai_title(body.as_bytes()).as_deref(),
        Some("Тестирование приложения \"Dreambook\" перед релизом")
    );
}

#[test]
fn extract_ai_title_missing_null_and_empty_yield_none() {
    // No aiTitle key at all.
    assert_eq!(
        extract_ai_title(b"{\"type\":\"user\",\"content\":\"hi\"}"),
        None
    );
    // Explicit null.
    assert_eq!(extract_ai_title(b"{\"aiTitle\":null}"), None);
    // Empty / whitespace-only value.
    assert_eq!(extract_ai_title(b"{\"aiTitle\":\"\"}"), None);
    assert_eq!(extract_ai_title(b"{\"aiTitle\":\"   \"}"), None);
    // A later null does NOT clobber an earlier real title (scan skips it).
    let body = "{\"aiTitle\":\"Real name\"}\n{\"aiTitle\":null}";
    assert_eq!(
        extract_ai_title(body.as_bytes()).as_deref(),
        Some("Real name")
    );
}

#[test]
fn extract_ai_title_tolerates_tail_truncated_mid_value() {
    // A tail read can begin mid-line and end mid-value. An unterminated LAST
    // aiTitle is skipped; an earlier complete one still resolves.
    let body = "{\"aiTitle\":\"Complete one\"}\n{\"garbage\":1,\"aiTitle\":\"truncated na";
    assert_eq!(
        extract_ai_title(body.as_bytes()).as_deref(),
        Some("Complete one")
    );
    // Leading partial line (mid-UTF-8 is fine — we only decode the key's value).
    let lead = "рвано{\"aiTitle\":\"Хвост\"}";
    assert_eq!(extract_ai_title(lead.as_bytes()).as_deref(), Some("Хвост"));
}

#[test]
fn title_precedence_session_title_wins() {
    let t = title_from_sources(Some("Ship the release"), Some("some prompt"), Some("txt"));
    assert_eq!(t, "Ship the release");
}

#[test]
fn title_override_wins_over_registry_and_every_other_source() {
    // R-27.1: the user override is the new highest-precedence layer — it beats
    // even the registry `name` (the former head of the chain).
    let t = title_with_override(
        Some("My renamed session"),
        Some("registry name"),
        Some("session title"),
        Some("latest prompt"),
        Some("transcript fallback"),
    );
    assert_eq!(t, "My renamed session");
}

#[test]
fn title_override_blank_falls_through_to_the_normal_chain() {
    // R-27.4 "empty name clears": a blank/whitespace override is ignored, so the
    // registry name (next in the chain) wins.
    let t = title_with_override(
        Some("   "),
        Some("registry name"),
        Some("session title"),
        None,
        None,
    );
    assert_eq!(t, "registry name");
    // No override at all → same fall-through.
    let t2 = title_with_override(None, None, Some("session title"), None, None);
    assert_eq!(t2, "session title");
}

#[test]
fn title_override_is_bidi_stripped_and_capped_like_every_other_source() {
    // R-27.7: the override rides the same `normalize_title` pipeline — bidi
    // controls stripped, whitespace collapsed, capped at 60 grapheme clusters.
    let spoof = title_with_override(
        Some("run \u{202E}cod.exe\u{202C}  now"),
        Some("registry"),
        None,
        None,
        None,
    );
    assert_eq!(spoof, "run cod.exe now");
    let long = "я".repeat(100);
    let capped = title_with_override(Some(&long), None, None, None, None);
    assert_eq!(capped.chars().count(), 60);
    assert!(capped.ends_with('…'));
}

#[test]
fn title_precedence_prompt_when_no_session_title() {
    let t = title_from_sources(None, Some("Investigate the flaky test"), Some("txt"));
    assert_eq!(t, "Investigate the flaky test");
    // Empty session title falls through to prompt.
    let t2 = title_from_sources(Some("   "), Some("prompt wins"), None);
    assert_eq!(t2, "prompt wins");
}

#[test]
fn title_precedence_transcript_fallback_then_placeholder() {
    let t = title_from_sources(None, None, Some("first user line"));
    assert_eq!(t, "first user line");
    let t2 = title_from_sources(None, None, None);
    assert_eq!(t2, NO_TITLE);
    let t3 = title_from_sources(Some(""), Some("  "), Some(""));
    assert_eq!(t3, NO_TITLE);
}

#[test]
fn normalize_collapses_whitespace_and_trims() {
    assert_eq!(
        normalize_title("  hello\n\t  world   again  "),
        "hello world again"
    );
}

#[test]
fn normalize_truncates_to_60_chars_char_safe_for_cyrillic() {
    // 100 Cyrillic chars → must be capped at 60 CHARS (not bytes) and not panic.
    let long = "я".repeat(100);
    let out = normalize_title(&long);
    assert_eq!(out.chars().count(), 60);
    assert!(out.ends_with('…'));
    // Must be valid UTF-8 (no split codepoint) — guaranteed by String, assert byte length is even-ish.
    assert!(out.is_char_boundary(out.len()));
}

#[test]
fn normalize_leaves_short_unicode_untouched() {
    let s = "Почини баг 🐛";
    assert_eq!(normalize_title(s), s);
}

#[test]
fn normalize_never_severs_a_zwj_emoji_sequence_at_the_cut() {
    // R-5.3 Unicode safety: the 60-cluster cap must not slice a compound emoji
    // (here the family ZWJ sequence 👨‍👩‍👧‍👦 = U+1F468 U+200D U+1F469 U+200D
    // U+1F467 U+200D U+1F466, seven scalars rendered as ONE grapheme cluster).
    // Engineer the title so a scalar-based cut would land INSIDE the sequence.
    let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
    let title = format!("{}{family} tail", "x".repeat(58));
    let out = normalize_title(&title);

    assert!(out.ends_with('…'), "truncated title ends with ellipsis");
    // Grapheme truncation is always cluster-aligned, so the cut can never leave a
    // dangling ZWJ joiner immediately before the ellipsis (a whole family emoji
    // legitimately contains internal joiners, so we can't assert `!contains ZWJ`).
    assert!(
        !out.trim_end_matches('…').ends_with('\u{200D}'),
        "no dangling ZWJ joiner left at the cut: {out:?}"
    );
    // The compound emoji must appear WHOLE or not at all — never a lone prefix
    // (a bare 👨 with the rest of the sequence severed off).
    let has_whole_family = out.contains(family);
    let has_no_family_scalar = !out.contains('\u{1F468}')
        && !out.contains('\u{1F469}')
        && !out.contains('\u{1F467}')
        && !out.contains('\u{1F466}');
    assert!(
        has_whole_family || has_no_family_scalar,
        "family emoji kept whole or dropped whole, never severed: {out:?}"
    );
}

#[test]
fn normalize_strips_bidi_override_controls_no_spoofing() {
    // Trojan-Source / RLO spoof: U+202E (RLO) + U+202C (PDF) make the browser's
    // bidi algorithm render "cod.exe" reversed as "exe.doc". Stripping the
    // controls leaves the real code points in their real visual order (R-5.3/R-8).
    let spoof = "OK to run \u{202E}cod.exe\u{202C} named safe_report_final_";
    let out = normalize_title(spoof);
    assert!(
        !out.contains('\u{202E}') && !out.contains('\u{202C}'),
        "bidi controls must be stripped: {out:?}"
    );
    assert_eq!(out, "OK to run cod.exe named safe_report_final_");
}

#[test]
fn strip_bidi_controls_removes_all_directional_formatting_but_keeps_text() {
    // Every embedding/override/isolate control is removed…
    for c in [
        '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}',
        '\u{2068}', '\u{2069}',
    ] {
        let s = format!("a{c}b");
        assert_eq!(strip_bidi_controls(&s), "ab", "control {c:?} not stripped");
    }
    // …while strongly-typed scripts (Cyrillic, Arabic) pass through untouched,
    // since their direction comes from their own strong characters (R-5.3).
    let cyr = "Почини баг 🐛";
    assert_eq!(strip_bidi_controls(cyr), cyr);
    let ar = "مرحبا world";
    assert_eq!(strip_bidi_controls(ar), ar);
}

#[test]
fn project_basename_handles_both_separators_and_unicode() {
    assert_eq!(project_name(Some("/home/user/my-proj")), "my-proj");
    assert_eq!(
        project_name(Some("C:\\Users\\phily\\projects\\quarterdeck")),
        "quarterdeck"
    );
    assert_eq!(project_name(Some("C:/Проекты/мой-агент")), "мой-агент");
    assert_eq!(project_name(Some("/tmp/🚀-rocket")), "🚀-rocket");
    // Trailing slashes trimmed.
    assert_eq!(project_name(Some("/home/user/proj/")), "proj");
    assert_eq!(project_name(Some("C:\\a\\b\\")), "b");
}

#[test]
fn project_unknown_when_empty_or_missing() {
    assert_eq!(project_name(None), UNKNOWN_PROJECT);
    assert_eq!(project_name(Some("   ")), UNKNOWN_PROJECT);
    assert_eq!(project_name(Some("/")), UNKNOWN_PROJECT);
}

#[test]
fn transcript_fallback_reads_first_user_text() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("проект-транскрипт.jsonl");
    // Mixed transcript: a system/meta line, then a user line with array content.
    let body = concat!(
        "{\"type\":\"summary\",\"summary\":\"ignore me\"}\n",
        "not even json\n",
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Проверь деплой\"}]}}\n",
        "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"later\"}}\n"
    );
    fs::write(&path, body).unwrap();

    let text = transcript_first_user_text(&path).unwrap();
    assert_eq!(text, "Проверь деплой");

    // Full precedence with a real transcript path and no cheaper source.
    let title = derive_title(None, None, Some(&path));
    assert_eq!(title, "Проверь деплой");
}

#[test]
fn transcript_fallback_string_content_and_tool_result_skipped() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let body = concat!(
        "{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"content\":\"x\"}]}\n",
        "{\"role\":\"user\",\"content\":\"plain string prompt\"}\n"
    );
    fs::write(&path, body).unwrap();
    assert_eq!(
        transcript_first_user_text(&path).as_deref(),
        Some("plain string prompt")
    );
}

#[test]
fn transcript_fallback_missing_or_no_user_returns_none() {
    // Missing file → None, never panics.
    assert_eq!(
        transcript_first_user_text(std::path::Path::new("/no/such/file.jsonl")),
        None
    );

    let dir = tempdir().unwrap();
    let path = dir.path().join("assistant-only.jsonl");
    fs::write(&path, "{\"role\":\"assistant\",\"content\":\"hi\"}\n").unwrap();
    assert_eq!(transcript_first_user_text(&path), None);
    assert_eq!(derive_title(None, None, Some(&path)), NO_TITLE);
}

#[test]
fn transcript_cwd_extracted_best_effort() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    fs::write(
        &path,
        "{\"type\":\"user\",\"cwd\":\"C:/Проекты/агент\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();
    assert_eq!(transcript_cwd(&path).as_deref(), Some("C:/Проекты/агент"));
}

#[test]
fn derive_title_does_not_read_transcript_when_cheaper_source_present() {
    // Even with a bogus transcript path, session_title short-circuits the read.
    let title = derive_title(
        Some("known title"),
        None,
        Some(std::path::Path::new("/definitely/missing.jsonl")),
    );
    assert_eq!(title, "known title");
}
