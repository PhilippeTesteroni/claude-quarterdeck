//! Per-session token telemetry from Claude Code transcripts (SPEC §23).
//!
//! The transcript `*.jsonl` format is explicitly internal and unstable
//! (`docs/hooks-facts.md`), so **everything here is parsed defensively**: a
//! missing/absent `usage` block is tolerated, a partial trailing line (an
//! in-flight append) is left for the next read, a line that isn't valid JSON or
//! isn't an assistant record is skipped, and a genuine format drift (a `usage`
//! field that is present but not an object) degrades this feature for that file
//! with a single WARN — never a crash, and never affecting the rest of the app
//! (R-23.1).
//!
//! ## Incremental reader (R-23.1)
//!
//! [`FileUsage`] holds a byte offset into one transcript and only ever reads the
//! bytes appended since the last read (cap [`MAX_READ_BYTES`] per read). If the
//! file shrank (truncated/rotated) or grew by more than the cap in one interval,
//! it rescans the tail [`TAIL_RESCAN_BYTES`] and marks its totals as a lower
//! bound (`approximate`, rendered "≥"). Only complete newline-terminated lines
//! are consumed, so the reader never trips over a half-written last line.
//!
//! ## Metrics (R-23.2)
//!
//! * **Context fill** — the latest assistant record's
//!   `input_tokens + cache_read_input_tokens + cache_creation_input_tokens` as a
//!   percentage of the model window (`[1m]` model id → 1,000,000, else 200,000).
//! * **Session spend** — the cumulative sum of
//!   `output_tokens + input_tokens + cache_creation_input_tokens` (output plus
//!   the non-cached input) across every assistant record.
//!
//! ## Subagent group (R-23.3)
//!
//! [`SessionUsageGroup`] aggregates the session's own transcript plus every
//! sidechain transcript under `<projects>/<slug>/<session_id>/**/*.jsonl` (the
//! subagent/workflow children), capped at [`MAX_SIDECHAIN_FILES`] newest by
//! mtime, each read with the same incremental discipline.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Maximum bytes read from a transcript in a single incremental read (R-23.1).
pub const MAX_READ_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

/// Tail window rescanned when the byte offset is invalidated (file truncated /
/// rotated) or the appended span overflows [`MAX_READ_BYTES`] (R-23.1).
pub const TAIL_RESCAN_BYTES: u64 = 512 * 1024; // 512 KiB

/// Default model context window when the model id carries no `[1m]` marker
/// (R-23.2a).
pub const DEFAULT_WINDOW: u64 = 200_000;

/// Context window for a `[1m]` (1M-token) model id (R-23.2a).
pub const LARGE_WINDOW: u64 = 1_000_000;

/// Cap on sidechain transcript files tracked per session (R-23.3): newest by
/// mtime; the overflow is logged once and ignored.
pub const MAX_SIDECHAIN_FILES: usize = 64;

/// Bound on the recursion depth of the sidechain (`**/*.jsonl`) walk, so a
/// pathological directory tree can't stall the reader.
const MAX_SIDECHAIN_DEPTH: usize = 8;

/// The model context window inferred from a model id (R-23.2a): a `[1m]` marker
/// anywhere in the id → 1,000,000 tokens, anything else (including an unknown or
/// absent id) → 200,000.
#[must_use]
pub fn model_window(model: Option<&str>) -> u64 {
    match model {
        Some(m) if m.contains("[1m]") => LARGE_WINDOW,
        _ => DEFAULT_WINDOW,
    }
}

/// Format a token count compactly (R-23.2b): `812` → `"812"`, `12_000` →
/// `"12k"`, `1_400_000` → `"1.4M"`. One decimal place, with a trailing `.0`
/// trimmed.
#[must_use]
pub fn format_compact(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        return trim_decimal(n as f64 / 1_000.0, 'k');
    }
    trim_decimal(n as f64 / 1_000_000.0, 'M')
}

fn trim_decimal(v: f64, suffix: char) -> String {
    let s = format!("{v:.1}");
    let s = s.strip_suffix(".0").unwrap_or(&s);
    format!("{s}{suffix}")
}

/// The incremental usage reader for a single transcript file (R-23.1/R-23.2).
#[derive(Debug, Clone, Default)]
pub struct FileUsage {
    /// Byte offset consumed so far (always at a line boundary for the normal
    /// append path).
    offset: u64,
    /// True once the byte offset was invalidated (truncation) or the appended
    /// span overflowed the read cap, so `spend` is a lower bound (rendered "≥").
    approximate: bool,
    /// Whether any assistant record with a `usage` object has been seen yet
    /// (context fill is `None` until then).
    seen_usage: bool,
    /// Latest assistant record's `input_tokens` (fresh, non-cached input).
    last_input: u64,
    /// Latest assistant record's `cache_read_input_tokens`.
    last_cache_read: u64,
    /// Latest assistant record's `cache_creation_input_tokens`.
    last_cache_creation: u64,
    /// Latest assistant record's `message.model`, for window inference.
    model: Option<String>,
    /// Cumulative session spend (R-23.2b): Σ `output + input + cache_creation`.
    spend: u64,
    /// Latest assistant record's joined text blocks (R-24.1 "the model's last
    /// words"), updated only when a record actually carried non-empty text.
    last_assistant_text: Option<String>,
    /// One-shot guard so a genuine format drift (a non-object `usage`) is logged
    /// at most once per file.
    drift_warned: bool,
}

impl FileUsage {
    /// Incrementally read the bytes appended to `path` since the last call and
    /// fold their assistant `usage` records into the running metrics (R-23.1).
    /// Defensive throughout: an unreadable/missing file is a no-op.
    pub fn ingest_path(&mut self, path: &Path) {
        let Ok(meta) = fs::metadata(path) else {
            return;
        };
        let len = meta.len();
        if len == self.offset {
            return; // nothing new
        }

        let read_from;
        let skip_partial_first;
        if len < self.offset {
            // Truncated / rotated (R-23.1): the accumulated total is for stale
            // content — reset spend, rescan the tail, and mark the total a lower
            // bound of the current file ("≥").
            self.approximate = true;
            self.spend = 0;
            read_from = len.saturating_sub(TAIL_RESCAN_BYTES);
            skip_partial_first = read_from > 0;
        } else if len - self.offset > MAX_READ_BYTES {
            // Overflow (R-23.1): more than the cap appended in one interval —
            // keep the prior spend, jump to the tail (skipping the middle), and
            // mark the total a lower bound.
            self.approximate = true;
            read_from = len.saturating_sub(TAIL_RESCAN_BYTES);
            skip_partial_first = read_from > self.offset;
        } else {
            read_from = self.offset;
            skip_partial_first = false;
        }

        let Ok(mut file) = File::open(path) else {
            return;
        };
        if file.seek(SeekFrom::Start(read_from)).is_err() {
            return;
        }
        let to_read = (len - read_from).min(MAX_READ_BYTES);
        let mut buf = Vec::new();
        if file.take(to_read).read_to_end(&mut buf).is_err() {
            return;
        }

        self.consume_chunk(&buf, read_from, len, skip_partial_first);
    }

    /// Parse complete lines out of a freshly-read chunk, advancing the offset to
    /// just past the last newline (a trailing partial line is left for the next
    /// read). When `skip_partial_first` is set (a tail/overflow seek landed
    /// mid-line), the bytes before the first newline are discarded.
    fn consume_chunk(&mut self, buf: &[u8], read_from: u64, len: u64, skip_partial_first: bool) {
        let mut line_start = 0usize;
        if skip_partial_first {
            match buf.iter().position(|&b| b == b'\n') {
                Some(nl) => line_start = nl + 1,
                None => {
                    // No complete line in the tail — nothing usable this round.
                    self.offset = len;
                    return;
                }
            }
        }
        let mut consumed = line_start;
        let mut idx = line_start;
        while let Some(rel) = buf[idx..].iter().position(|&b| b == b'\n') {
            let nl = idx + rel;
            self.process_line(&buf[line_start..nl]);
            line_start = nl + 1;
            consumed = line_start;
            idx = line_start;
        }
        self.offset = read_from + consumed as u64;
    }

    fn process_line(&mut self, line: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return; // partial/garbage line — skip (never fatal, R-23.1)
        };
        if !is_assistant_record(&value) {
            return;
        }
        let Some(msg) = value.get("message") else {
            return;
        };
        if let Some(model) = msg.get("model").and_then(Value::as_str) {
            let model = model.trim();
            if !model.is_empty() {
                self.model = Some(model.to_string());
            }
        }
        if let Some(usage) = msg.get("usage") {
            match usage {
                Value::Object(_) => {
                    let input = u64_field(usage, "input_tokens");
                    let cache_creation = u64_field(usage, "cache_creation_input_tokens");
                    let cache_read = u64_field(usage, "cache_read_input_tokens");
                    let output = u64_field(usage, "output_tokens");
                    self.seen_usage = true;
                    self.last_input = input;
                    self.last_cache_read = cache_read;
                    self.last_cache_creation = cache_creation;
                    self.spend = self
                        .spend
                        .saturating_add(output)
                        .saturating_add(input)
                        .saturating_add(cache_creation);
                }
                _ => {
                    if !self.drift_warned {
                        self.drift_warned = true;
                        tracing::warn!(
                            "transcript `message.usage` is present but not an object; token stats degraded for this file (R-23.1)"
                        );
                    }
                }
            }
        }
        if let Some(text) = assistant_text(msg) {
            self.last_assistant_text = Some(text);
        }
    }

    /// Latest-record context tokens (input + cache_read + cache_creation), or
    /// `None` until a usage record has been seen (R-23.2a).
    #[must_use]
    pub fn context_tokens(&self) -> Option<u64> {
        if self.seen_usage {
            Some(
                self.last_input
                    .saturating_add(self.last_cache_read)
                    .saturating_add(self.last_cache_creation),
            )
        } else {
            None
        }
    }

    /// Context fill as a whole-number percentage of the model window (R-23.2a),
    /// or `None` until a usage record has been seen.
    #[must_use]
    pub fn context_percent(&self) -> Option<u32> {
        let tokens = self.context_tokens()?;
        let window = model_window(self.model.as_deref());
        let pct = (tokens as f64 / window as f64 * 100.0).round();
        Some(pct.max(0.0) as u32)
    }

    /// Cumulative session spend so far (R-23.2b).
    #[must_use]
    pub fn spend(&self) -> u64 {
        self.spend
    }

    /// Whether the totals are a lower bound after a truncation/overflow rescan
    /// (R-23.1) — the UI renders spend as "≥".
    #[must_use]
    pub fn approximate(&self) -> bool {
        self.approximate
    }

    /// Latest assistant record's model id, if seen.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The model's last words (R-24.1): the latest assistant record's joined
    /// text blocks, or `None` if no assistant text has been seen.
    #[must_use]
    pub fn last_assistant_text(&self) -> Option<&str> {
        self.last_assistant_text.as_deref()
    }
}

fn u64_field(obj: &Value, key: &str) -> u64 {
    obj.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Whether a transcript record is an assistant message: `type == "assistant"`
/// at the top level, or `message.role == "assistant"` (R-23.1 defensive shape).
fn is_assistant_record(v: &Value) -> bool {
    if v.get("type").and_then(Value::as_str) == Some("assistant") {
        return true;
    }
    v.get("message")
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        == Some("assistant")
}

/// Join the text blocks of an assistant `message.content` (R-24.1). A string
/// content is taken verbatim; an array yields the concatenation of every
/// `{"type":"text","text":…}` block. `None` when there is no text (e.g. a
/// tool-use-only record), so the caller keeps the previous last-words.
fn assistant_text(msg: &Value) -> Option<String> {
    let content = msg.get("content")?;
    match content {
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        Value::Array(items) => {
            let mut parts: Vec<String> = Vec::new();
            for item in items {
                if item.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    let text = text.trim();
                    if !text.is_empty() {
                        parts.push(text.to_string());
                    }
                }
            }
            (!parts.is_empty()).then(|| parts.join(" "))
        }
        _ => None,
    }
}

/// The whole telemetry group for one session (R-23.2 + R-23.3): the session's
/// own transcript plus its subagent/workflow sidechains.
#[derive(Debug, Default)]
pub struct SessionUsageGroup {
    main: FileUsage,
    sidechains: BTreeMap<PathBuf, FileUsage>,
    /// One-shot guard so the "sidechain count exceeds cap" WARN fires once.
    capped_warned: bool,
}

impl SessionUsageGroup {
    /// Re-read the session's own transcript and every tracked sidechain,
    /// discovering (and pruning) sidechain files under
    /// `<transcript-without-ext>/**/*.jsonl`, newest-64 by mtime (R-23.3).
    pub fn update(&mut self, transcript_path: &Path) {
        self.main.ingest_path(transcript_path);

        let dir = sidechain_dir(transcript_path);
        let files = newest_sidechain_files(&dir, MAX_SIDECHAIN_FILES, &mut self.capped_warned);
        let keep: HashSet<&PathBuf> = files.iter().collect();
        self.sidechains.retain(|k, _| keep.contains(k));
        for file in &files {
            self.sidechains
                .entry(file.clone())
                .or_default()
                .ingest_path(file);
        }
    }

    /// Context fill percent of the session's own transcript (R-23.2a).
    #[must_use]
    pub fn context_percent(&self) -> Option<u32> {
        self.main.context_percent()
    }

    /// Session spend of the session's own transcript (R-23.2b).
    #[must_use]
    pub fn spend(&self) -> u64 {
        self.main.spend()
    }

    /// Whether the session spend is a lower bound (R-23.1).
    #[must_use]
    pub fn spend_approx(&self) -> bool {
        self.main.approximate()
    }

    /// The model's last words from the session's own transcript (R-24.1).
    #[must_use]
    pub fn last_assistant_text(&self) -> Option<&str> {
        self.main.last_assistant_text()
    }

    /// Combined spend across every sidechain transcript (R-23.3) — the group
    /// spend shown beside the `⛭ N` subagent badge. Excludes the main transcript
    /// (that is the row's own session spend).
    #[must_use]
    pub fn group_spend(&self) -> u64 {
        self.sidechains.values().map(FileUsage::spend).sum()
    }

    /// Whether any sidechain's spend is a lower bound (R-23.1).
    #[must_use]
    pub fn group_approx(&self) -> bool {
        self.sidechains.values().any(FileUsage::approximate)
    }
}

/// The sidechain directory for a session's transcript (R-23.3): the transcript
/// path with its `.jsonl` extension dropped — `<slug>/<session-id>.jsonl` →
/// `<slug>/<session-id>/`.
fn sidechain_dir(transcript_path: &Path) -> PathBuf {
    transcript_path.with_extension("")
}

/// Collect the newest [`MAX_SIDECHAIN_FILES`] `*.jsonl` files (by mtime) under
/// `dir`, recursively. Logs once via `capped_warned` when the total exceeds the
/// cap (R-23.3).
fn newest_sidechain_files(dir: &Path, cap: usize, capped_warned: &mut bool) -> Vec<PathBuf> {
    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    collect_jsonl(dir, &mut files, 0);
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if files.len() > cap && !*capped_warned {
        *capped_warned = true;
        tracing::warn!(
            found = files.len(),
            cap,
            "session sidechain transcript count exceeds cap; tracking the newest only (R-23.3)"
        );
    }
    files.into_iter().take(cap).map(|(p, _)| p).collect()
}

fn collect_jsonl(dir: &Path, out: &mut Vec<(PathBuf, SystemTime)>, depth: usize) {
    if depth > MAX_SIDECHAIN_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out, depth + 1);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            out.push((path, mtime));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "quarterdeck-usage-test-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A realistic assistant record line (R-23.1 shape) with the given usage +
    /// optional model + optional text.
    fn assistant_line(
        model: Option<&str>,
        input: u64,
        cache_creation: u64,
        cache_read: u64,
        output: u64,
        text: Option<&str>,
    ) -> String {
        let content = match text {
            Some(t) => format!(r#"[{{"type":"text","text":{}}}]"#, json_str(t)),
            None => r#"[{"type":"tool_use","id":"tu","name":"Bash","input":{}}]"#.to_string(),
        };
        let model_field = match model {
            Some(m) => format!(r#""model":{},"#, json_str(m)),
            None => String::new(),
        };
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant",{model_field}"content":{content},"usage":{{"input_tokens":{input},"cache_creation_input_tokens":{cache_creation},"cache_read_input_tokens":{cache_read},"output_tokens":{output}}}}}}}"#
        ) + "\n"
    }

    fn json_str(s: &str) -> String {
        serde_json::to_string(s).unwrap()
    }

    #[test]
    fn model_window_inference() {
        assert_eq!(model_window(Some("claude-opus-4-8[1m]")), LARGE_WINDOW);
        assert_eq!(model_window(Some("claude-sonnet-4-5")), DEFAULT_WINDOW);
        assert_eq!(model_window(None), DEFAULT_WINDOW);
        assert_eq!(model_window(Some("something[1m]weird")), LARGE_WINDOW);
    }

    #[test]
    fn compact_formatting() {
        assert_eq!(format_compact(0), "0");
        assert_eq!(format_compact(812), "812");
        assert_eq!(format_compact(12_000), "12k");
        assert_eq!(format_compact(1_500), "1.5k");
        assert_eq!(format_compact(1_400_000), "1.4M");
        assert_eq!(format_compact(2_000_000), "2M");
    }

    #[test]
    fn reads_usage_and_computes_metrics() {
        let dir = tmp_dir("basic");
        let path = dir.join("sess.jsonl");
        let mut f = File::create(&path).unwrap();
        // Two assistant turns.
        f.write_all(
            assistant_line(
                Some("claude-sonnet-4-5"),
                100,
                2_000,
                40_000,
                500,
                Some("First"),
            )
            .as_bytes(),
        )
        .unwrap();
        f.write_all(
            assistant_line(
                Some("claude-sonnet-4-5"),
                200,
                0,
                60_000,
                800,
                Some("Second reply"),
            )
            .as_bytes(),
        )
        .unwrap();
        drop(f);

        let mut usage = FileUsage::default();
        usage.ingest_path(&path);

        // Context fill uses the LATEST record: 200 + 60_000 + 0 = 60_200 of 200k.
        assert_eq!(usage.context_tokens(), Some(60_200));
        assert_eq!(usage.context_percent(), Some(30)); // 60_200/200_000 = 30.1% → 30
                                                       // Spend accumulates both: (500+100+2000) + (800+200+0) = 2600 + 1000 = 3600.
        assert_eq!(usage.spend(), 3_600);
        assert!(!usage.approximate());
        assert_eq!(usage.last_assistant_text(), Some("Second reply"));
        assert_eq!(usage.model(), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn incremental_append_only_reads_new_bytes() {
        let dir = tmp_dir("append");
        let path = dir.join("sess.jsonl");
        let mut f = File::create(&path).unwrap();
        f.write_all(assistant_line(None, 100, 0, 1_000, 200, Some("A")).as_bytes())
            .unwrap();
        f.flush().unwrap();

        let mut usage = FileUsage::default();
        usage.ingest_path(&path);
        assert_eq!(usage.spend(), 300); // 200+100
        let offset_after_first = usage.offset;

        // Append a second record; a re-ingest must fold ONLY the new bytes.
        f.write_all(assistant_line(None, 50, 0, 2_000, 400, Some("B")).as_bytes())
            .unwrap();
        f.flush().unwrap();
        usage.ingest_path(&path);
        assert_eq!(usage.spend(), 750); // +450
        assert!(usage.offset > offset_after_first);
        assert_eq!(usage.last_assistant_text(), Some("B"));

        // A no-op ingest (no new bytes) changes nothing.
        usage.ingest_path(&path);
        assert_eq!(usage.spend(), 750);
    }

    #[test]
    fn partial_trailing_line_is_not_consumed_until_complete() {
        let dir = tmp_dir("partial");
        let path = dir.join("sess.jsonl");
        let mut f = File::create(&path).unwrap();
        let line = assistant_line(None, 10, 0, 100, 20, Some("done"));
        // Write everything EXCEPT the trailing newline (an in-flight append).
        let without_nl = &line[..line.len() - 1];
        f.write_all(without_nl.as_bytes()).unwrap();
        f.flush().unwrap();

        let mut usage = FileUsage::default();
        usage.ingest_path(&path);
        // The record isn't complete (no newline) → not counted yet.
        assert_eq!(usage.spend(), 0);
        assert_eq!(usage.context_tokens(), None);

        // Now the newline lands; the record is folded in.
        f.write_all(b"\n").unwrap();
        f.flush().unwrap();
        usage.ingest_path(&path);
        assert_eq!(usage.spend(), 30);
    }

    #[test]
    fn truncation_marks_approximate_and_rescans() {
        let dir = tmp_dir("truncate");
        let path = dir.join("sess.jsonl");
        {
            let mut f = File::create(&path).unwrap();
            for _ in 0..3 {
                f.write_all(assistant_line(None, 100, 0, 1_000, 100, Some("x")).as_bytes())
                    .unwrap();
            }
        }
        let mut usage = FileUsage::default();
        usage.ingest_path(&path);
        assert_eq!(usage.spend(), 600);
        assert!(!usage.approximate());

        // Rotate: replace with a shorter file (len < offset).
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(assistant_line(None, 50, 0, 500, 50, Some("fresh")).as_bytes())
                .unwrap();
        }
        usage.ingest_path(&path);
        assert!(
            usage.approximate(),
            "truncation marks totals approximate (R-23.1)"
        );
        // Spend was reset then re-read from the (short) tail.
        assert_eq!(usage.spend(), 100);
        assert_eq!(usage.last_assistant_text(), Some("fresh"));
    }

    #[test]
    fn defensive_against_garbage_and_non_assistant_lines() {
        let dir = tmp_dir("garbage");
        let path = dir.join("sess.jsonl");
        let mut f = File::create(&path).unwrap();
        f.write_all(b"not json at all\n").unwrap();
        f.write_all(br#"{"type":"user","message":{"role":"user","content":"hi"}}"#)
            .unwrap();
        f.write_all(b"\n").unwrap();
        f.write_all(
            br#"{"type":"assistant","message":{"role":"assistant","content":[],"usage":"broken"}}"#,
        )
        .unwrap();
        f.write_all(b"\n").unwrap();
        f.write_all(assistant_line(None, 10, 0, 0, 5, Some("ok")).as_bytes())
            .unwrap();
        drop(f);

        let mut usage = FileUsage::default();
        usage.ingest_path(&path); // must not panic
                                  // Only the last (valid) record counts.
        assert_eq!(usage.spend(), 15);
        assert_eq!(usage.last_assistant_text(), Some("ok"));
    }

    #[test]
    fn group_aggregates_sidechain_spend() {
        let dir = tmp_dir("group");
        // Main transcript: <dir>/sess.jsonl ; sidechains under <dir>/sess/.
        let main = dir.join("sess.jsonl");
        let mut f = File::create(&main).unwrap();
        f.write_all(assistant_line(None, 100, 0, 1_000, 200, Some("main")).as_bytes())
            .unwrap();
        drop(f);

        let side_dir = dir.join("sess");
        fs::create_dir_all(side_dir.join("nested")).unwrap();
        let mut s1 = File::create(side_dir.join("a.jsonl")).unwrap();
        s1.write_all(assistant_line(None, 10, 0, 0, 100, Some("sub a")).as_bytes())
            .unwrap();
        drop(s1);
        let mut s2 = File::create(side_dir.join("nested").join("b.jsonl")).unwrap();
        s2.write_all(assistant_line(None, 20, 0, 0, 300, Some("sub b")).as_bytes())
            .unwrap();
        drop(s2);

        let mut group = SessionUsageGroup::default();
        group.update(&main);
        assert_eq!(group.spend(), 300, "main session spend (200+100)");
        // Group spend = sidechains only: (100+10) + (300+20) = 110 + 320 = 430.
        assert_eq!(group.group_spend(), 430);
    }

    #[test]
    fn group_caps_sidechain_files() {
        let dir = tmp_dir("cap");
        let main = dir.join("sess.jsonl");
        File::create(&main).unwrap();
        let side_dir = dir.join("sess");
        fs::create_dir_all(&side_dir).unwrap();
        // Create more than the cap; each with one small record.
        for i in 0..(MAX_SIDECHAIN_FILES + 10) {
            let mut f = File::create(side_dir.join(format!("s{i:03}.jsonl"))).unwrap();
            f.write_all(assistant_line(None, 1, 0, 0, 1, Some("s")).as_bytes())
                .unwrap();
        }
        let mut group = SessionUsageGroup::default();
        group.update(&main);
        assert_eq!(
            group.sidechains.len(),
            MAX_SIDECHAIN_FILES,
            "only the newest {MAX_SIDECHAIN_FILES} sidechains are tracked (R-23.3)"
        );
    }
}
