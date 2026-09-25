//! Small display-formatting helpers shared by `list` and `load`.

use chrono::{DateTime, Local, Utc};
use std::path::Path;

/// Windows' extended-length ("verbatim") path prefix — real to the
/// filesystem, meaningless to a reader. Stripped everywhere a path is
/// printed (human output and JSON alike).
const WINDOWS_VERBATIM_PREFIX: &str = r"\\?\";

/// `path.display()`, with the Windows verbatim prefix stripped when present.
pub fn display_path(path: &Path) -> String {
    let rendered = path.display().to_string();
    rendered.strip_prefix(WINDOWS_VERBATIM_PREFIX).map(str::to_string).unwrap_or(rendered)
}

/// Slice out the last `n` elements of `items`, preserving order (oldest of
/// the kept set first, newest last).
pub fn last_n<T>(items: &[T], n: usize) -> &[T] {
    let start = items.len().saturating_sub(n);
    &items[start..]
}

/// Convert a stored UTC timestamp to local time for human display. `--json`
/// output keeps the UTC `DateTime` untouched (via `to_rfc3339`) — this
/// conversion is display-only.
pub fn local_time(utc: &DateTime<Utc>) -> DateTime<Local> {
    utc.with_timezone(&Local)
}

/// Truncate string to max length (UTF-8 safe).
pub fn truncate(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        let mut boundary = max_len;
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!("{}...", &s[..boundary])
    } else {
        s.to_string()
    }
}

/// Truncate `text` to at most `max_chars` *characters* (not bytes — a
/// byte cap silently halves the visible length of Cyrillic text). Returns
/// the truncated text and, if truncation happened, how many characters were
/// cut.
pub fn truncate_chars_reporting(text: &str, max_chars: usize) -> (String, Option<usize>) {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return (text.to_string(), None);
    }
    let truncated: String = text.chars().take(max_chars).collect();
    (truncated, Some(total_chars - max_chars))
}

/// Collapse every whitespace run in `text` onto a single display line:
/// a run containing a newline becomes `" ⏎ "` (keeps the paragraph break
/// visible without breaking the line), any other run becomes a single
/// space. Used for owner messages, first prompts, and stuck lines, which
/// are each rendered as one numbered line.
pub fn collapse_paragraphs(text: &str) -> String {
    // Trim first: a leading/trailing whitespace run has no paragraph on one
    // side of it, so it must vanish rather than become a dangling `⏎`.
    let trimmed = text.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            let mut has_newline = c == '\n';
            while let Some(&next) = chars.peek() {
                if !next.is_whitespace() {
                    break;
                }
                has_newline |= next == '\n';
                chars.next();
            }
            out.push_str(if has_newline { " \u{23ce} " } else { " " });
        } else {
            out.push(c);
        }
    }
    out
}

/// Collapse every whitespace run in `text` to a single space, producing one
/// display line with no paragraph markers at all — used for the compact
/// `list` preview (`first:`/`last:`), which has no room for a `⏎`.
pub fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Keep single newlines as-is; collapse a run of 2 or more *consecutive*
/// newlines (i.e. two or more blank lines) down to exactly one blank line.
/// Used for `Last agent reports` excerpts, which are markdown-shaped and
/// should keep their real line structure — only the excessive gaps.
pub fn collapse_blank_line_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' {
            let mut newline_count = 1_usize;
            while let Some(&next) = chars.peek() {
                if next == '\n' {
                    newline_count += 1;
                    chars.next();
                } else if next == ' ' || next == '\t' || next == '\r' {
                    // Trailing horizontal whitespace on an otherwise-blank
                    // line — consumed without counting as its own break.
                    chars.next();
                } else {
                    break;
                }
            }
            let emitted = newline_count.min(2);
            for _ in 0..emitted {
                out.push('\n');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Indent every line after the first by `indent`, so a multi-line item stays
/// visibly inside its list entry instead of spilling to column 0. Blank lines
/// stay empty (no trailing spaces).
pub fn indent_continuation(text: &str, indent: &str) -> String {
    let mut out = String::with_capacity(text.len() + indent.len() * 4);
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
            if !line.trim().is_empty() {
                out.push_str(indent);
            }
        }
        out.push_str(line.trim_end_matches('\r'));
    }
    out
}

/// Format file size in human-readable form.
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indent_continuation_indents_every_nonblank_line_after_the_first() {
        let text = "Итог такой:\n- **Первый пункт**\n\nДальше проверка\r\nMX";
        assert_eq!(
            indent_continuation(text, "     "),
            "Итог такой:\n     - **Первый пункт**\n\n     Дальше проверка\n     MX"
        );
        assert_eq!(indent_continuation("one line", "  "), "one line");
    }

    #[test]
    fn last_n_returns_the_tail_slice() {
        let items: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| (*s).to_string()).collect();
        assert_eq!(last_n(&items, 2), ["c".to_string(), "d".to_string()]);
        assert_eq!(last_n(&items, 10), items.as_slice());
        assert_eq!(last_n(&items, 0), Vec::<String>::new().as_slice());
    }


    #[test]
    fn local_time_preserves_the_instant() {
        let utc = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("valid timestamp");
        let local = local_time(&utc);
        assert_eq!(local.timestamp(), utc.timestamp());
    }

    #[test]
    fn truncate_chars_reporting_counts_characters_not_bytes() {
        // Each Cyrillic char is 2 bytes in UTF-8 — a byte-based cap would
        // silently halve the visible length here.
        let text = "привет мир";
        let (truncated, cut) = truncate_chars_reporting(text, 6);
        assert_eq!(truncated, "привет");
        assert_eq!(cut, Some(4)); // " мир" = 4 chars
    }

    #[test]
    fn display_path_strips_windows_verbatim_prefix() {
        let path = Path::new(r"\\?\C:\Users\owner\session.jsonl");
        assert_eq!(display_path(path), r"C:\Users\owner\session.jsonl");

        let plain = Path::new(r"C:\Users\owner\session.jsonl");
        assert_eq!(display_path(plain), r"C:\Users\owner\session.jsonl");
    }

    #[test]
    fn collapse_paragraphs_marks_newline_runs_and_single_spaces_the_rest() {
        let text = "первая строка\n\n\nвторая  строка";
        assert_eq!(collapse_paragraphs(text), "первая строка \u{23ce} вторая строка");
    }

    #[test]
    fn collapse_paragraphs_trims_leading_and_trailing_whitespace() {
        assert_eq!(collapse_paragraphs("\n\nhello\n\n"), "hello");
    }

    #[test]
    fn single_line_collapses_every_whitespace_run() {
        assert_eq!(single_line("hello\n\n\nworld   again"), "hello world again");
    }

    #[test]
    fn collapse_blank_line_runs_keeps_single_newlines() {
        assert_eq!(collapse_blank_line_runs("line one\nline two"), "line one\nline two");
    }

    #[test]
    fn collapse_blank_line_runs_collapses_two_or_more_blank_lines_to_one() {
        assert_eq!(collapse_blank_line_runs("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(collapse_blank_line_runs("a\n\nb"), "a\n\nb");
    }
}
