//! `@o<byte-offset>` handles — the stable pointer format every item in
//! `load` and every wave-2 command is addressed by.
//!
//! Stable because transcripts are append-only: the byte offset of a line
//! never moves once written, so a handle printed today seeks straight back
//! to the same record tomorrow (see [`crate::io::read_window_after`]).

/// Render a byte offset as its printable handle, e.g. `@o52331120`.
#[must_use]
pub fn format_handle(offset: u64) -> String {
    format!("@o{offset}")
}

/// Parse a handle back into its byte offset. Accepts the `@o` prefix, and
/// also a bare decimal offset for scripts that already stripped it.
#[must_use]
pub fn parse_handle(text: &str) -> Option<u64> {
    let digits = text.strip_prefix("@o").unwrap_or(text);
    digits.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_handle_prints_the_o_prefixed_offset() {
        assert_eq!(format_handle(52_331_120), "@o52331120");
    }

    #[test]
    fn parse_handle_round_trips_through_format_handle() {
        assert_eq!(parse_handle(&format_handle(0)), Some(0));
        assert_eq!(parse_handle(&format_handle(123_456_789)), Some(123_456_789));
    }

    #[test]
    fn parse_handle_accepts_bare_digits() {
        assert_eq!(parse_handle("42"), Some(42));
    }

    #[test]
    fn parse_handle_rejects_garbage() {
        assert_eq!(parse_handle("not-a-handle"), None);
        assert_eq!(parse_handle("@oxyz"), None);
    }
}
