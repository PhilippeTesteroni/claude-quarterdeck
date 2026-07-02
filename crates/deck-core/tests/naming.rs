//! R-5.2 title precedence, R-5.3 Cyrillic/Unicode safety, project basename.

use std::fs;

use deck_core::naming::{
    derive_title, normalize_title, project_name, title_from_sources, transcript_cwd,
    transcript_first_user_text, NO_TITLE, UNKNOWN_PROJECT,
};
use tempfile::tempdir;

#[test]
fn title_precedence_session_title_wins() {
    let t = title_from_sources(Some("Ship the release"), Some("some prompt"), Some("txt"));
    assert_eq!(t, "Ship the release");
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
