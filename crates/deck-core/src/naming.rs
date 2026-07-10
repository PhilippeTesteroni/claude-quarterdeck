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

/// Pick the first non-empty candidate (trimmed), normalize it, or `(no title)`.
/// The single primitive behind both [`title_from_sources`] (R-5.2) and
/// [`title_from_registry`] (R-15.2): callers pass their precedence chain in
/// order and the first candidate with content wins.
#[must_use]
pub fn pick_title<'a>(candidates: impl IntoIterator<Item = Option<&'a str>>) -> String {
    for candidate in candidates {
        if let Some(text) = candidate.map(str::trim).filter(|s| !s.is_empty()) {
            return normalize_title(text);
        }
    }
    NO_TITLE.to_string()
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
    pick_title([session_title, latest_prompt, transcript_fallback])
}

/// Apply the R-15.2 precedence (which replaces R-5.2's chain head): the live
/// registry `name` (matched by sessionId) → `session_title` → latest
/// `UserPromptSubmit.prompt` → cold-start transcript fallback → `(no title)`.
/// The registry name refreshes on every poll, so a `/rename` mid-session shows
/// up within one poll.
#[must_use]
pub fn title_from_registry(
    registry_name: Option<&str>,
    session_title: Option<&str>,
    latest_prompt: Option<&str>,
    transcript_fallback: Option<&str>,
) -> String {
    pick_title([
        registry_name,
        session_title,
        latest_prompt,
        transcript_fallback,
    ])
}

/// Apply the R-27.1 precedence: a user-set title override wins over every other
/// source (registry `name` → `session_title` → latest `UserPromptSubmit.prompt`
/// → cold-start transcript fallback → `(no title)`). The override is fed through
/// the same [`pick_title`] pipeline as every other candidate, so it inherits the
/// whitespace-collapse + [`strip_bidi_controls`] + [`truncate_graphemes`] cap of
/// [`normalize_title`] for free (R-27.7). A blank/whitespace override falls
/// through to the normal chain (R-27.4 "empty name clears").
#[must_use]
pub fn title_with_override(
    override_name: Option<&str>,
    registry_name: Option<&str>,
    session_title: Option<&str>,
    latest_prompt: Option<&str>,
    transcript_fallback: Option<&str>,
) -> String {
    pick_title([
        override_name,
        registry_name,
        session_title,
        latest_prompt,
        transcript_fallback,
    ])
}

/// Apply the LOCKED §34 title precedence (highest → lowest, R-34):
///
/// 1. `override_name` — Quarterdeck §27 rename (R-27.1)
/// 2. `user_registry_name` — an explicit Claude-side `/rename` (registry
///    `nameSource == "user"`)
/// 3. `ai_title` — the transcript `aiTitle`, i.e. the terminal-tab chat name
/// 4. `derived_registry_name` — the auto-generated `phily-XX` handle (registry
///    `nameSource` "derived"/absent)
/// 5. `session_title`
/// 6. `latest_prompt`
/// 7. `transcript_fallback`
///
/// The two registry rungs (2 and 4) are the SAME registry `name` routed to
/// exactly one slot by the caller's `nameSource` classification — a user-set
/// name outranks the `aiTitle`, a derived one loses to it. Every candidate
/// rides the same [`pick_title`] pipeline, so each inherits the whitespace
/// collapse + [`strip_bidi_controls`] + [`truncate_graphemes`] cap of
/// [`normalize_title`] (R-27.7); a blank/whitespace candidate falls through.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn title_full(
    override_name: Option<&str>,
    user_registry_name: Option<&str>,
    ai_title: Option<&str>,
    derived_registry_name: Option<&str>,
    session_title: Option<&str>,
    latest_prompt: Option<&str>,
    transcript_fallback: Option<&str>,
) -> String {
    pick_title([
        override_name,
        user_registry_name,
        ai_title,
        derived_registry_name,
        session_title,
        latest_prompt,
        transcript_fallback,
    ])
}

/// The `"aiTitle"` JSON key marker scanned for in a transcript byte slice.
const AI_TITLE_KEY: &[u8] = b"\"aiTitle\"";

/// Extract the LAST non-empty `"aiTitle":"…"` value from a Claude Code
/// transcript byte slice — or a *tail read* of one (the shell passes the last
/// ~128 KB so a multi-MB transcript is never read whole) — for the §34 default
/// title (R-34): the terminal-tab chat name.
///
/// `aiTitle` is (re)written on many transcript lines as the conversation
/// evolves; the LAST occurrence is authoritative, so the scan runs backwards.
/// The value is JSON-unescaped via `serde_json` so escapes (`\"`, `\n`,
/// `\uXXXX`) decode and Cyrillic/UTF-8 survives intact. A `null`, empty, or
/// unterminated (tail-truncated mid-value) occurrence is skipped and the scan
/// continues to the previous one; `None` when no usable `aiTitle` is present.
/// Pure and defensive — never panics on malformed/partial input.
#[must_use]
pub fn extract_ai_title(bytes: &[u8]) -> Option<String> {
    let mut end = bytes.len();
    while let Some(pos) = rfind_bytes(&bytes[..end], AI_TITLE_KEY) {
        if let Some(value) = json_string_value_after(&bytes[pos + AI_TITLE_KEY.len()..]) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        // null / empty / unterminated → look further back for an earlier title.
        end = pos;
    }
    None
}

/// Last index at which `needle` occurs in `haystack`, or `None`.
fn rfind_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .rev()
        .find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Given the bytes immediately after an `"aiTitle"` key, parse the JSON string
/// value that follows (`: "…"`). Skips insignificant whitespace, requires the
/// `:` and a string (a `null`/other value yields `None`), finds the closing
/// unescaped quote, and JSON-decodes the `"…"` slice via `serde_json` so all
/// escapes and multi-byte UTF-8 are handled correctly. `None` on any deviation
/// (no colon, non-string value, unterminated string).
fn json_string_value_after(bytes: &[u8]) -> Option<String> {
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b':' {
        return None;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'"' {
        return None; // null, number, or missing value — no title here.
    }
    let start = i; // the opening quote
    i += 1;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if b == b'"' {
            // Round-trip the `"…"` slice through serde to unescape safely.
            return serde_json::from_slice::<String>(&bytes[start..=i]).ok();
        }
        i += 1;
    }
    None // unterminated string (tail truncated mid-value)
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
