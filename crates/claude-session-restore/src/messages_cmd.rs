//! `messages <session> [--kind …] [--since T] [--until T] [--last N] [--full]`
//! — a full-file, chronological scan of every classified message, one
//! handle per entry.

use crate::io::{parse_line, scan_lines, OffsetEvent};
use crate::render::{classify, render_line, MessageKind, RenderedMessage};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::Path;

const DEFAULT_MAX_CHARS: usize = 600;

pub struct MessagesArgs {
    /// `None` means every kind (`--kind all` or the flag omitted).
    pub kinds: Option<Vec<MessageKind>>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub last: Option<usize>,
    pub full: bool,
    pub json: bool,
}

/// Parse a `--kind owner,peer,agent,notification,all` value into the filter
/// set `run` applies. `"all"` (or an absent flag) means no filtering.
pub fn parse_kinds(raw: Option<&str>) -> Result<Option<Vec<MessageKind>>> {
    let Some(raw) = raw else { return Ok(None) };
    let mut kinds = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.eq_ignore_ascii_case("all") {
            return Ok(None);
        }
        let Some(kind) = MessageKind::parse(part) else {
            anyhow::bail!("Unknown --kind value: {part} (expected owner, peer, agent, notification, or all)");
        };
        kinds.push(kind);
    }
    Ok(Some(kinds))
}

pub fn run(path: &Path, args: &MessagesArgs) -> Result<()> {
    let mut matches: Vec<RenderedMessage> = Vec::new();

    scan_lines(path, |offset, text| {
        let Some(event) = parse_line(text) else { return Ok(true) };
        let oe = OffsetEvent { offset, event };
        for message in classify(&oe) {
            if let Some(kinds) = &args.kinds {
                if !kinds.contains(&message.kind) {
                    continue;
                }
            }
            if let Some(since) = args.since {
                if message.timestamp < since {
                    continue;
                }
            }
            if let Some(until) = args.until {
                if message.timestamp > until {
                    continue;
                }
            }
            matches.push(message);
        }
        Ok(true)
    })?;

    if let Some(last) = args.last {
        if matches.len() > last {
            let drop = matches.len() - last;
            matches.drain(..drop);
        }
    }

    let max_chars = if args.full { usize::MAX } else { DEFAULT_MAX_CHARS };

    if args.json {
        print_json(&matches, args.full);
    } else {
        print_human(&matches, max_chars);
    }
    Ok(())
}

fn print_human(matches: &[RenderedMessage], max_chars: usize) {
    if matches.is_empty() {
        println!("No messages matched.");
        return;
    }
    for message in matches {
        println!("{}", render_line(message, max_chars));
    }
}

#[derive(Serialize)]
struct MessageJson {
    handle: String,
    kind: &'static str,
    timestamp: String,
    sender: Option<String>,
    text: String,
}

fn print_json(matches: &[RenderedMessage], full: bool) {
    let out: Vec<MessageJson> = matches
        .iter()
        .map(|message| MessageJson {
            handle: crate::handle::format_handle(message.offset),
            kind: message.kind.label(),
            timestamp: message.timestamp.to_rfc3339(),
            sender: message.sender.clone(),
            text: if full {
                message.text.clone()
            } else {
                let (text, _) = crate::format::truncate_chars_reporting(&message.text, DEFAULT_MAX_CHARS);
                text
            },
        })
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&out) {
        println!("{json}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kinds_all_means_no_filter() {
        assert_eq!(parse_kinds(Some("all")).unwrap(), None);
        assert_eq!(parse_kinds(None).unwrap(), None);
    }

    #[test]
    fn parse_kinds_splits_and_maps_each_value() {
        let kinds = parse_kinds(Some("owner,peer")).unwrap().unwrap();
        assert_eq!(kinds, vec![MessageKind::Owner, MessageKind::Peer]);
    }

    #[test]
    fn parse_kinds_rejects_unknown_value() {
        assert!(parse_kinds(Some("bogus")).is_err());
    }
}
