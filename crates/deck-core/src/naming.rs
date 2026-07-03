//! Session title derivation and project naming (SPEC §5). Title precedence
//! (R-5.2), `<project>` = `basename(cwd)`, and Cyrillic/Unicode safety (R-5.3).
//!
//! All string handling is grapheme-cluster-based, never byte- or scalar-based,
//! so multi-byte paths and prompts (Cyrillic, emoji) are never split
//! mid-codepoint AND a compound glyph — a ZWJ emoji sequence (e.g. the family
//! emoji), a flag, or a skin-tone-modified emoji, each several `char`s rendered
//! as ONE cluster — is never severed mid-cluster.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

/// Hard cap on a rendered title (R-5.2: "≤60 chars").
pub const MAX_TITLE_CHARS: usize = 60;

/// Placeholder when no title source is available (R-5.2).
pub const NO_TITLE: &str = "(no title)";

/// Placeholder project name when `cwd` is unknown/empty.
pub const UNKNOWN_PROJECT: &str = "(unknown)";

/// Upper bound on transcript lines scanned for the cold-start fallback, so a
/// pathological transcript can't stall discovery.
const MAX_TRANSCRIPT_LINES_SCAN: usize = 400;

/// Project label for a session = basename of its `cwd`, handling both `/` and
/// `\` separators and trailing slashes. Unicode-safe.
#[must_use]
pub fn project_name(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.map(str::trim).filter(|s| !s.is_empty()) else {
        return UNKNOWN_PROJECT.to_string();
    };
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    if base.is_empty() {
        UNKNOWN_PROJECT.to_string()
    } else {
        base.to_string()
    }
}

/// Unicode bidirectional formatting controls exploited by the "Trojan Source" /
/// RLO spoofing technique: the explicit embedding, override, and isolate
/// characters. Left in agent-supplied display text (a session title from
/// `SessionStart.session_title`/`UserPromptSubmit.prompt`, or an `ask_user`
/// question/option/context) they make the browser's bidi algorithm reorder the
/// rendered glyphs so the text can read as the opposite of its actual code
/// points (e.g. a `.exe` disguised as a `.doc`) — a rendering-fidelity/trust
/// defect on the one surface where a human reads agent text to make a decision
/// (R-8). We strip them before the string ever reaches the DOM.
///
/// Only these invisible directional controls are removed; strongly-typed
/// scripts (Cyrillic, Arabic, Hebrew) are untouched — they derive their
/// direction from their own strong characters, not from these controls — so
/// R-5.3's "Cyrillic/Unicode works end-to-end" is preserved.
const BIDI_CONTROLS: &[char] = &[
    '\u{202A}', // LEFT-TO-RIGHT EMBEDDING
    '\u{202B}', // RIGHT-TO-LEFT EMBEDDING
    '\u{202C}', // POP DIRECTIONAL FORMATTING
    '\u{202D}', // LEFT-TO-RIGHT OVERRIDE
    '\u{202E}', // RIGHT-TO-LEFT OVERRIDE
    '\u{2066}', // LEFT-TO-RIGHT ISOLATE
    '\u{2067}', // RIGHT-TO-LEFT ISOLATE
    '\u{2068}', // FIRST STRONG ISOLATE
    '\u{2069}', // POP DIRECTIONAL ISOLATE
];

/// Remove the [`BIDI_CONTROLS`] from a display string. Cheap fast-path: returns
/// the input unchanged when it contains none (the overwhelmingly common case).
#[must_use]
pub fn strip_bidi_controls(s: &str) -> String {
    if s.contains(|c| BIDI_CONTROLS.contains(&c)) {
        s.chars().filter(|c| !BIDI_CONTROLS.contains(c)).collect()
    } else {
        s.to_string()
    }
}

/// Collapse all whitespace runs to single spaces, trim, strip Unicode bidi
/// override controls ([`strip_bidi_controls`]), and cap at [`MAX_TITLE_CHARS`]
/// grapheme clusters (appending `…` when truncated).
#[must_use]
pub fn normalize_title(s: &str) -> String {
    let cleaned = strip_bidi_controls(s);
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_graphemes(&collapsed, MAX_TITLE_CHARS)
}

/// Truncate `s` to at most `max` grapheme clusters, appending `…` when it was
/// shortened. Grapheme-aware (not merely scalar-value-aware), so a compound
/// glyph — a ZWJ emoji sequence (e.g. 👨‍👩‍👧‍👦, seven scalars rendered as one
/// cluster), a regional-indicator flag, or a skin-tone-modified emoji — is never
/// severed mid-cluster into a lone prefix or a dangling ZWJ (R-5.3 Unicode
/// safety). Any whitespace exposed at the cut is trimmed before the ellipsis.
/// Shared with the toast-body truncation in `src-tauri/notify.rs`.
#[must_use]
pub fn truncate_graphemes(s: &str, max: usize) -> String {
    let clusters: Vec<&str> = s.graphemes(true).collect();
    if clusters.len() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = clusters[..keep].concat();
    let trimmed_len = out.trim_end().len();
    out.truncate(trimmed_len);
    out.push('…');
    out
}

/// Apply the R-5.2 precedence to already-resolved sources.
///
/// `session_title` → latest `UserPromptSubmit.prompt` → cold-start transcript
/// fallback → `(no title)`. Each candidate is trimmed; empty candidates fall
/// through to the next.
#[must_use]
pub fn title_from_sources(
    session_title: Option<&str>,
    latest_prompt: Option<&str>,
    transcript_fallback: Option<&str>,
) -> String {
    for candidate in [session_title, latest_prompt, transcript_fallback] {
        if let Some(text) = candidate.map(str::trim).filter(|s| !s.is_empty()) {
            return normalize_title(text);
        }
    }
    NO_TITLE.to_string()
}

/// Full precedence including the guarded transcript read (R-5.2). Used where a
/// caller has a `transcript_path` but no cached fallback yet.
#[must_use]
pub fn derive_title(
    session_title: Option<&str>,
    latest_prompt: Option<&str>,
    transcript_path: Option<&Path>,
) -> String {
    // Only touch the filesystem when the cheaper sources are absent.
    let fallback = if session_title.map(str::trim).is_none_or(str::is_empty)
        && latest_prompt.map(str::trim).is_none_or(str::is_empty)
    {
        transcript_path.and_then(transcript_first_user_text)
    } else {
        None
    };
    title_from_sources(session_title, latest_prompt, fallback.as_deref())
}

/// Guarded best-effort read of the first user-authored text line from a Claude
/// Code transcript (`*.jsonl`). The transcript format is explicitly internal and
/// unstable (`docs/hooks-facts.md`), so every line is parsed defensively and any
/// failure is skipped — this never panics and never propagates an error.
#[must_use]
pub fn transcript_first_user_text(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(MAX_TRANSCRIPT_LINES_SCAN) {
        let Ok(line) = line else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(text) = extract_user_text(&value) {
            return Some(text);
        }
    }
    None
}

/// Best-effort read of the `cwd` recorded in a transcript (used by cold-start
/// discovery to reconstruct the project when there was no live `SessionStart`).
#[must_use]
pub fn transcript_cwd(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(MAX_TRANSCRIPT_LINES_SCAN) {
        let Ok(line) = line else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
            let cwd = cwd.trim();
            if !cwd.is_empty() {
                return Some(cwd.to_string());
            }
        }
    }
    None
}

fn extract_user_text(v: &Value) -> Option<String> {
    let role = v
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| v.get("type").and_then(Value::as_str))
        .or_else(|| {
            v.get("message")
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
        })?;
    if !role.eq_ignore_ascii_case("user") {
        return None;
    }
    let content = v
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| v.get("content"))?;
    content_to_text(content)
}

fn content_to_text(c: &Value) -> Option<String> {
    match c {
        Value::String(s) => non_empty(s),
        Value::Array(items) => {
            for item in items {
                // Skip tool results / non-text blocks; we want the human's words.
                if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                    continue;
                }
                if let Some(s) = item.get("text").and_then(Value::as_str) {
                    if let Some(t) = non_empty(s) {
                        return Some(t);
                    }
                }
                if let Value::String(s) = item {
                    if let Some(t) = non_empty(s) {
                        return Some(t);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}
