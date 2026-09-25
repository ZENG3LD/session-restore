//! Byte-budgeted reads, byte-offset tracking, and event parsing.
//!
//! Every JSONL line this module hands back carries the absolute byte offset
//! of its first byte in the file — the basis of the `@o<offset>` handles
//! `load` and every wave-2 command print. Offsets are stable because
//! transcripts are append-only: a line's byte position never moves once
//! written, so a handle printed today can seek straight back to the same
//! record tomorrow.
//!
//! `list` and `load` read bounded windows from each end of the file (see
//! [`read_tail_lines`] and [`read_head_lines`]) — cost is proportional to the
//! window, not to file size, so a multi-gigabyte transcript loads in well
//! under a second. Wave-2 commands that must search the whole file use
//! [`scan_lines`], a single forward streaming pass that never holds more
//! than one line in memory at a time. [`read_window_before`] and
//! [`read_window_after`] serve `span`'s bounded reads around a handle.

use anyhow::{Context, Result};
use claude_session_restore::transcript::events::SessionEvent;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// A single JSONL line paired with the absolute byte offset of its first
/// byte in the file.
#[derive(Debug, Clone)]
pub struct RawLine {
    pub offset: u64,
    pub text: String,
}

/// A parsed [`SessionEvent`] paired with the absolute byte offset of the
/// JSONL line it came from — the value every `@o<offset>` handle renders.
#[derive(Debug, Clone)]
pub struct OffsetEvent {
    pub offset: u64,
    pub event: SessionEvent,
}

/// Strip a trailing `\r` from a line already split on `\n` (CRLF safety;
/// real transcripts are LF-only, but nothing guarantees that forever).
fn strip_trailing_cr(bytes: &[u8]) -> &[u8] {
    match bytes.split_last() {
        Some((b'\r', rest)) => rest,
        _ => bytes,
    }
}

/// Read the byte range `[start, start + max_bytes)` (clamped to the file's
/// true length) and split it into complete lines with absolute offsets.
///
/// When `drop_leading_partial` is true (the caller is starting mid-file, not
/// at byte 0), a partial leading line — cut off by `start` landing mid-line —
/// is dropped rather than treated as data. A partial trailing line is
/// dropped whenever the read did not reach the file's true end (there could
/// be more of that line still to come); when `start` is itself the exact
/// start of a real line (a handle from an earlier read) and the read is
/// bounded to end exactly at another such offset, no line is ever partial
/// and nothing is dropped.
fn read_range_lines(path: &Path, start: u64, max_bytes: u64, drop_leading_partial: bool) -> Result<Vec<RawLine>> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("Failed to open session file: {}", path.display()))?;
    let file_len = file
        .metadata()
        .with_context(|| format!("Failed to inspect session file: {}", path.display()))?
        .len();
    let start = start.min(file_len);
    let cap = max_bytes.min(file_len - start);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("Failed to seek session file: {}", path.display()))?;

    let mut buffer = vec![0_u8; cap as usize];
    let read = file
        .read(&mut buffer)
        .with_context(|| format!("Failed to read session file: {}", path.display()))?;
    buffer.truncate(read);
    let reached_eof = start + read as u64 >= file_len;

    let mut base_offset = start;
    if drop_leading_partial && start > 0 {
        match buffer.iter().position(|byte| *byte == b'\n') {
            Some(index) => {
                base_offset += index as u64 + 1;
                buffer.drain(..=index);
            }
            None => buffer.clear(),
        }
    }

    if !reached_eof {
        match buffer.iter().rposition(|byte| *byte == b'\n') {
            Some(index) => buffer.truncate(index + 1),
            None => buffer.clear(),
        }
    }

    let mut lines = Vec::new();
    let mut line_start = 0_usize;
    for index in 0..buffer.len() {
        if buffer[index] == b'\n' {
            push_line(&mut lines, &buffer[line_start..index], base_offset + line_start as u64);
            line_start = index + 1;
        }
    }
    if line_start < buffer.len() {
        push_line(&mut lines, &buffer[line_start..], base_offset + line_start as u64);
    }

    Ok(lines)
}

fn push_line(lines: &mut Vec<RawLine>, raw: &[u8], offset: u64) {
    let raw = strip_trailing_cr(raw);
    if raw.is_empty() {
        return;
    }
    lines.push(RawLine { offset, text: String::from_utf8_lossy(raw).into_owned() });
}

/// Read up to `max_bytes` from the end of `path`, split into complete lines
/// with absolute offsets.
///
/// A single seek plus one bounded read — cost is proportional to
/// `max_bytes`, not to file size. Returns `(lines, truncated)`, where
/// `truncated` is `true` when the file is larger than `max_bytes` (earlier
/// context exists that this call did not read).
pub fn read_tail_lines(path: &Path, max_bytes: u64) -> Result<(Vec<RawLine>, bool)> {
    let file_len = fs::metadata(path)
        .with_context(|| format!("Failed to inspect session file: {}", path.display()))?
        .len();
    let start = file_len.saturating_sub(max_bytes);
    let lines = read_range_lines(path, start, max_bytes, start > 0)?;
    Ok((lines, start > 0))
}

/// Read up to `max_bytes` from the start of `path`, split into complete
/// lines with absolute offsets.
///
/// Used for title/first-prompt detection and for the `load` "First prompts"
/// section, which must come from before any compaction — i.e. the true start
/// of the file, not the tail window.
pub fn read_head_lines(path: &Path, max_bytes: u64) -> Result<Vec<RawLine>> {
    read_range_lines(path, 0, max_bytes, false)
}

/// A bounded window of complete lines ending exactly at `offset`
/// (exclusive) — used by `span --around` to gather context before a handle.
/// `offset` must be the start of a real line (a handle from an earlier
/// read); the window then always ends on a clean line boundary, so nothing
/// is ever dropped at the end.
pub fn read_window_before(path: &Path, offset: u64, max_bytes: u64) -> Result<Vec<RawLine>> {
    let start = offset.saturating_sub(max_bytes);
    read_range_lines(path, start, offset - start, start > 0)
}

/// A bounded window of complete lines starting exactly at `offset` — used by
/// `span` (forward slices and `--around` context after a handle).
pub fn read_window_after(path: &Path, offset: u64, max_bytes: u64) -> Result<Vec<RawLine>> {
    read_range_lines(path, offset, max_bytes, false)
}

/// Stream every line of `path` from the true start to the true end in a
/// single buffered forward pass, calling `visit(offset, line)` for each
/// non-blank line. Never holds more than one line in memory at a time — the
/// only way to search or classify a multi-gigabyte transcript in full.
/// Returning `Ok(false)` from `visit` stops the scan early.
pub fn scan_lines<F>(path: &Path, mut visit: F) -> Result<()>
where
    F: FnMut(u64, &str) -> Result<bool>,
{
    let file = fs::File::open(path)
        .with_context(|| format!("Failed to open session file: {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut offset: u64 = 0;
    let mut buffer = Vec::new();

    loop {
        buffer.clear();
        let read = reader
            .read_until(b'\n', &mut buffer)
            .with_context(|| format!("Failed to read session file: {}", path.display()))?;
        if read == 0 {
            break;
        }
        let line_offset = offset;
        offset += read as u64;

        let trimmed = match buffer.last() {
            Some(b'\n') => &buffer[..buffer.len() - 1],
            _ => &buffer[..],
        };
        let raw = strip_trailing_cr(trimmed);
        if raw.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(raw);
        if !visit(line_offset, &text)? {
            break;
        }
    }

    Ok(())
}

/// Parse a single JSONL line, discarding it silently on malformed JSON —
/// the same tolerance [`parse_events`] applies to a whole window.
pub fn parse_line(text: &str) -> Option<SessionEvent> {
    serde_json::from_str(text).ok()
}

/// Parse each line as a [`SessionEvent`], silently skipping lines that fail
/// to deserialize (malformed JSON, truncated leading/trailing line, or a
/// partial line from a session still being written to).
pub fn parse_events(lines: &[RawLine]) -> Vec<OffsetEvent> {
    lines
        .iter()
        .filter_map(|line| parse_line(&line.text).map(|event| OffsetEvent { offset: line.offset, event }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    struct TempFile {
        path: std::path::PathBuf,
    }

    impl TempFile {
        fn new(content: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("claude-session-restore-io-tests-{}-{unique}.jsonl", std::process::id()));
            fs::write(&path, content).expect("write fixture");
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn texts(lines: &[RawLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn read_tail_lines_returns_whole_file_when_under_budget() {
        let file = TempFile::new("line-one\nline-two\nline-three\n");
        let (lines, truncated) = read_tail_lines(&file.path, 4096).expect("read tail");
        assert_eq!(texts(&lines), vec!["line-one", "line-two", "line-three"]);
        assert!(!truncated);
        assert_eq!(lines[0].offset, 0);
        assert_eq!(lines[1].offset, 9);
        assert_eq!(lines[2].offset, 18);
    }

    #[test]
    fn read_tail_lines_drops_partial_leading_line_and_reports_truncated() {
        let mut content = String::new();
        for index in 0..50 {
            content.push_str(&format!("line{index:03}\n"));
        }
        let file = TempFile::new(&content);
        let (lines, truncated) = read_tail_lines(&file.path, 25).expect("read tail");
        assert!(truncated);
        for line in &lines {
            assert!(content.lines().any(|full| full == line.text), "unexpected partial line: {}", line.text);
        }
        assert_eq!(lines.last().map(|line| line.text.as_str()), Some("line049"));
    }

    #[test]
    fn read_head_lines_drops_partial_trailing_line() {
        let file = TempFile::new("line-one\nline-two\nline-three\n");
        let lines = read_head_lines(&file.path, 12).expect("read head");
        assert_eq!(texts(&lines), vec!["line-one"]);
        assert_eq!(lines[0].offset, 0);
    }

    #[test]
    fn offsets_survive_a_seek_and_reread_round_trip() {
        // The whole point of a handle: seeking back to its offset and
        // reading forward from there must reproduce the exact same line.
        let file = TempFile::new("aaa\nbbbb\nccccc\ndddddd\n");
        let head = read_head_lines(&file.path, 4096).expect("read head");
        assert_eq!(texts(&head), vec!["aaa", "bbbb", "ccccc", "dddddd"]);

        let target = &head[2]; // "ccccc"
        let reread = read_window_after(&file.path, target.offset, 4096).expect("read window after");
        assert_eq!(reread[0].text, "ccccc");
        assert_eq!(reread[0].offset, target.offset);
    }

    #[test]
    fn read_window_before_ends_exactly_at_the_given_offset() {
        let file = TempFile::new("aaa\nbbbb\nccccc\ndddddd\n");
        let head = read_head_lines(&file.path, 4096).expect("read head");
        let target_offset = head[2].offset; // start of "ccccc"

        let before = read_window_before(&file.path, target_offset, 4096).expect("read window before");
        assert_eq!(texts(&before), vec!["aaa", "bbbb"]);
    }

    #[test]
    fn read_window_before_drops_a_partial_leading_line_under_a_tight_budget() {
        let file = TempFile::new("aaaaaaaaaa\nbbbb\ncccc\n");
        let head = read_head_lines(&file.path, 4096).expect("read head");
        let target_offset = head[2].offset; // start of "cccc"

        // Budget only covers the tail of "aaaaaaaaaa\n" plus all of "bbbb\n".
        let before = read_window_before(&file.path, target_offset, 8).expect("read window before");
        assert_eq!(texts(&before), vec!["bbbb"]);
    }

    #[test]
    fn scan_lines_visits_every_line_with_its_offset_and_can_stop_early() {
        let file = TempFile::new("one\ntwo\nthree\nfour\n");
        let mut seen = Vec::new();
        scan_lines(&file.path, |offset, text| {
            seen.push((offset, text.to_string()));
            Ok(text != "two")
        })
        .expect("scan lines");
        assert_eq!(seen, vec![(0, "one".to_string()), (4, "two".to_string())]);
    }

    #[test]
    fn scan_lines_skips_blank_lines() {
        let file = TempFile::new("one\n\ntwo\n");
        let mut seen = Vec::new();
        scan_lines(&file.path, |offset, text| {
            seen.push((offset, text.to_string()));
            Ok(true)
        })
        .expect("scan lines");
        assert_eq!(seen, vec![(0, "one".to_string()), (5, "two".to_string())]);
    }

    #[test]
    fn parse_events_carries_the_line_offset_onto_each_parsed_event() {
        let lines = vec![
            RawLine { offset: 0, text: r#"{"type":"custom-title","customTitle":"t","sessionId":"s"}"#.to_string() },
            RawLine { offset: 100, text: "not json".to_string() },
        ];
        let events = parse_events(&lines);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].offset, 0);
    }
}
