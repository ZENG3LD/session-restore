//! `grep <session> <regex> [--kind …] [-C N]` — a full-file scan; hits with
//! handle, kind, time, and a snippet around the match.
//!
//! A byte substring pre-filter runs before JSON parsing whenever `pattern`
//! has no regex metacharacters (the common case — a plain keyword like
//! `QuickNode`): a cheap ASCII-case-insensitive `contains` check rejects the
//! overwhelming majority of lines before they ever reach `serde_json`. A
//! pattern that does use regex syntax skips the pre-filter (soundness over
//! speed) and every line is parsed and matched directly.

use crate::format::{format_size, local_time};
use crate::handle::format_handle;
use crate::io::{parse_line, scan_lines, OffsetEvent};
use crate::render::{classify, MessageKind};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use regex::RegexBuilder;
use serde::Serialize;
use std::path::Path;
use std::time::{Duration, Instant};

pub const DEFAULT_CONTEXT_CHARS: usize = 60;

pub struct GrepArgs {
    pub pattern: String,
    /// `None` means every kind.
    pub kinds: Option<Vec<MessageKind>>,
    pub context_chars: usize,
    pub json: bool,
}

struct Hit {
    offset: u64,
    kind: MessageKind,
    timestamp: DateTime<Utc>,
    snippet: String,
}

const REGEX_METACHARS: [char; 14] = ['.', '^', '$', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '\\'];

/// A cheap ASCII-lowercase byte pre-filter, applied before JSON parsing —
/// only sound when `pattern` has no regex metacharacters (a plain literal).
/// `None` means every line must be parsed and matched directly.
fn literal_prefilter(pattern: &str) -> Option<Vec<u8>> {
    if pattern.chars().any(|c| REGEX_METACHARS.contains(&c)) {
        return None;
    }
    Some(pattern.to_ascii_lowercase().into_bytes())
}

fn contains_ascii_ci(haystack: &str, needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let haystack = haystack.as_bytes();
    if haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

/// A `context_chars`-wide window of `text` around the byte range
/// `[match_start, match_end)`, UTF-8 safe (built on char boundaries, never
/// byte offsets directly).
fn snippet_around(text: &str, match_start: usize, match_end: usize, context_chars: usize) -> String {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let start_char = chars.iter().position(|(byte_idx, _)| *byte_idx >= match_start).unwrap_or(chars.len());
    let end_char = chars.iter().position(|(byte_idx, _)| *byte_idx >= match_end).unwrap_or(chars.len());
    let window_start = start_char.saturating_sub(context_chars);
    let window_end = (end_char + context_chars).min(chars.len());
    let snippet: String = chars[window_start..window_end].iter().map(|(_, c)| *c).collect();
    let prefix = if window_start > 0 { "\u{2026}" } else { "" };
    let suffix = if window_end < chars.len() { "\u{2026}" } else { "" };
    format!("{prefix}{snippet}{suffix}")
}

pub fn run(path: &Path, args: &GrepArgs) -> Result<()> {
    let regex = RegexBuilder::new(&args.pattern)
        .case_insensitive(true)
        .build()
        .with_context(|| format!("Invalid grep pattern: {}", args.pattern))?;
    let prefilter = literal_prefilter(&args.pattern);

    let started = Instant::now();
    let mut hits = Vec::new();
    let mut lines_scanned: u64 = 0;
    let mut bytes_scanned: u64 = 0;

    scan_lines(path, |offset, text| {
        lines_scanned += 1;
        bytes_scanned += text.len() as u64;
        if let Some(needle) = &prefilter {
            if !contains_ascii_ci(text, needle) {
                return Ok(true);
            }
        }
        let Some(event) = parse_line(text) else { return Ok(true) };
        let oe = OffsetEvent { offset, event };
        for message in classify(&oe) {
            if let Some(kinds) = &args.kinds {
                if !kinds.contains(&message.kind) {
                    continue;
                }
            }
            if let Some(m) = regex.find(&message.text) {
                let snippet = snippet_around(&message.text, m.start(), m.end(), args.context_chars);
                hits.push(Hit { offset: message.offset, kind: message.kind, timestamp: message.timestamp, snippet });
            }
        }
        Ok(true)
    })?;

    let elapsed = started.elapsed();

    if args.json {
        print_json(&hits, lines_scanned, bytes_scanned, elapsed);
    } else {
        print_human(&hits, lines_scanned, bytes_scanned, elapsed);
    }
    Ok(())
}

fn print_human(hits: &[Hit], lines_scanned: u64, bytes_scanned: u64, elapsed: Duration) {
    if hits.is_empty() {
        println!("No hits.");
    }
    for hit in hits {
        println!(
            "{} [{}] {} \u{ab}{}\u{bb}",
            format_handle(hit.offset),
            hit.kind.label(),
            local_time(&hit.timestamp).format("%Y-%m-%d %H:%M:%S"),
            crate::format::collapse_paragraphs(&hit.snippet)
        );
    }
    println!(
        "\n{} hit(s) \u{2014} {} line(s) scanned ({}) in {:.2?}",
        hits.len(),
        lines_scanned,
        format_size(bytes_scanned),
        elapsed
    );
}

#[derive(Serialize)]
struct HitJson {
    handle: String,
    kind: &'static str,
    timestamp: String,
    snippet: String,
}

#[derive(Serialize)]
struct GrepReportJson {
    hits: Vec<HitJson>,
    lines_scanned: u64,
    bytes_scanned: u64,
    elapsed_ms: u128,
}

fn print_json(hits: &[Hit], lines_scanned: u64, bytes_scanned: u64, elapsed: Duration) {
    let report = GrepReportJson {
        hits: hits
            .iter()
            .map(|hit| HitJson {
                handle: format_handle(hit.offset),
                kind: hit.kind.label(),
                timestamp: hit.timestamp.to_rfc3339(),
                snippet: hit.snippet.clone(),
            })
            .collect(),
        lines_scanned,
        bytes_scanned,
        elapsed_ms: elapsed.as_millis(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        println!("{json}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_prefilter_accepts_a_plain_word() {
        assert_eq!(literal_prefilter("QuickNode"), Some(b"quicknode".to_vec()));
    }

    #[test]
    fn literal_prefilter_rejects_a_pattern_with_regex_metacharacters() {
        assert_eq!(literal_prefilter("foo.*bar"), None);
        assert_eq!(literal_prefilter("a+b"), None);
    }

    #[test]
    fn contains_ascii_ci_matches_case_insensitively() {
        assert!(contains_ascii_ci("Found a QUICKNODE mention", b"quicknode"));
        assert!(!contains_ascii_ci("nothing here", b"quicknode"));
    }

    #[test]
    fn snippet_around_windows_on_char_boundaries_around_a_multibyte_match() {
        let text = "привет QuickNode мир";
        let start = text.find("QuickNode").expect("match present");
        let end = start + "QuickNode".len();
        let snippet = snippet_around(text, start, end, 3);
        assert!(snippet.contains("QuickNode"));
    }
}
