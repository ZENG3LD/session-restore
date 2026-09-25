//! `span <session> <from-handle> [<to-handle>] [--around <handle> -n N]` —
//! a chronological slice: owner/peer/assistant text verbatim, tool calls
//! one line, tool results truncated. This is how to read what happened
//! around one point.

use crate::digest::describe_tool_use;
use crate::format::{collapse_paragraphs, local_time, truncate};
use crate::handle::{format_handle, parse_handle};
use crate::io::{read_window_after, read_window_before, OffsetEvent};
use crate::render::classify;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use claude_session_restore::transcript::events::{ContentBlock, SessionEvent};
use colored::Colorize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::path::Path;

/// Bounded window read on each side of a `--around` handle — generous
/// enough to contain far more than a typical `-n` worth of classified
/// entries, without ever scanning the whole file.
const AROUND_WINDOW_BYTES: u64 = 8 * 1024 * 1024;
/// Forward window used for `<from-handle>` with no `<to-handle>`.
const DEFAULT_FORWARD_BYTES: u64 = 4 * 1024 * 1024;
const ENTRY_TEXT_CHAR_CAP: usize = 800;
const TOOL_RESULT_CHAR_CAP: usize = 400;

pub enum SpanMode {
    Around { handle: String, n: usize },
    Range { from: String, to: Option<String> },
}

pub struct SpanArgs {
    pub mode: SpanMode,
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Owner,
    Peer,
    Agent,
    Notification,
    ToolCall,
    ToolResult,
}

impl EntryKind {
    fn label(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Peer => "peer",
            Self::Agent => "agent",
            Self::Notification => "notification",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
        }
    }
}

struct Entry {
    offset: u64,
    kind: EntryKind,
    timestamp: DateTime<Utc>,
    text: String,
}

fn tool_result_text(content: &JsonValue) -> Option<String> {
    match content {
        JsonValue::String(text) => Some(text.clone()),
        JsonValue::Array(items) => {
            items.iter().find_map(|item| item.get("text").and_then(JsonValue::as_str)).map(str::to_string)
        }
        _ => None,
    }
}

/// Every renderable entry a single line produces: classified messages
/// (owner/peer/agent-text/notification), main-chain tool calls (one line
/// each), and tool results (truncated).
fn span_entries(oe: &OffsetEvent) -> Vec<Entry> {
    let mut entries: Vec<Entry> = classify(oe)
        .into_iter()
        .map(|message| {
            let kind = match message.kind {
                crate::render::MessageKind::Owner => EntryKind::Owner,
                crate::render::MessageKind::Peer => EntryKind::Peer,
                crate::render::MessageKind::Agent => EntryKind::Agent,
                crate::render::MessageKind::Notification => EntryKind::Notification,
            };
            Entry { offset: message.offset, kind, timestamp: message.timestamp, text: message.text }
        })
        .collect();

    match &oe.event {
        SessionEvent::Assistant(assistant) if !assistant.metadata.is_sidechain => {
            for block in &assistant.message.content {
                if let Some((_, name, input)) = block.as_tool_use() {
                    entries.push(Entry {
                        offset: oe.offset,
                        kind: EntryKind::ToolCall,
                        timestamp: assistant.timestamp(),
                        text: describe_tool_use(name, input),
                    });
                }
            }
        }
        SessionEvent::User(user) => {
            for block in &user.message.content {
                if let ContentBlock::ToolResult(result) = block {
                    if let Some(text) = tool_result_text(&result.content) {
                        entries.push(Entry { offset: oe.offset, kind: EntryKind::ToolResult, timestamp: user.timestamp(), text });
                    }
                }
            }
        }
        _ => {}
    }

    entries
}

fn lines_to_entries(lines: &[crate::io::RawLine]) -> Vec<Entry> {
    lines
        .iter()
        .filter_map(|line| crate::io::parse_line(&line.text).map(|event| OffsetEvent { offset: line.offset, event }))
        .flat_map(|oe| span_entries(&oe))
        .collect()
}

pub fn run(path: &Path, args: &SpanArgs) -> Result<()> {
    let entries = match &args.mode {
        SpanMode::Around { handle, n } => build_around(path, handle, *n)?,
        SpanMode::Range { from, to } => build_range(path, from, to.as_deref())?,
    };

    if args.json {
        print_json(&entries);
    } else {
        print_human(&entries);
    }
    Ok(())
}

fn build_around(path: &Path, handle: &str, n: usize) -> Result<Vec<Entry>> {
    let Some(offset) = parse_handle(handle) else {
        anyhow::bail!("Not a valid handle: {handle} (expected @o<byte-offset>)");
    };

    let before_lines = read_window_before(path, offset, AROUND_WINDOW_BYTES)
        .with_context(|| format!("Failed to read the window before handle {}", format_handle(offset)))?;
    let after_lines = read_window_after(path, offset, AROUND_WINDOW_BYTES)
        .with_context(|| format!("Failed to read the window after handle {}", format_handle(offset)))?;

    let mut before = lines_to_entries(&before_lines);
    if before.len() > n {
        let drop = before.len() - n;
        before.drain(..drop);
    }

    let mut after = lines_to_entries(&after_lines);
    // `after` starts at `offset` itself — keep the target line's own
    // entries plus up to `n` more that follow it.
    let target_entry_count = after.iter().take_while(|entry| entry.offset == offset).count();
    let keep = (target_entry_count + n).min(after.len());
    after.truncate(keep);

    before.extend(after);
    Ok(before)
}

fn build_range(path: &Path, from: &str, to: Option<&str>) -> Result<Vec<Entry>> {
    let Some(from_offset) = parse_handle(from) else {
        anyhow::bail!("Not a valid handle: {from} (expected @o<byte-offset>)");
    };

    let lines = match to {
        Some(to) => {
            let Some(to_offset) = parse_handle(to) else {
                anyhow::bail!("Not a valid handle: {to} (expected @o<byte-offset>)");
            };
            if to_offset <= from_offset {
                anyhow::bail!("<to-handle> must be after <from-handle>: {from} .. {to}");
            }
            read_window_after(path, from_offset, to_offset - from_offset)
                .with_context(|| format!("Failed to read the span {from} .. {to}"))?
        }
        None => read_window_after(path, from_offset, DEFAULT_FORWARD_BYTES)
            .with_context(|| format!("Failed to read forward from handle {from}"))?,
    };

    Ok(lines_to_entries(&lines))
}

fn render_text(kind: EntryKind, text: &str) -> String {
    match kind {
        EntryKind::ToolResult => truncate(text, TOOL_RESULT_CHAR_CAP),
        EntryKind::ToolCall => text.to_string(),
        _ => truncate(&collapse_paragraphs(text), ENTRY_TEXT_CHAR_CAP),
    }
}

fn print_human(entries: &[Entry]) {
    if entries.is_empty() {
        println!("No entries in this span.");
        return;
    }
    for entry in entries {
        // One line per entry; `show <handle>` has the verbatim layout.
        let rendered = crate::format::collapse_paragraphs(&render_text(entry.kind, &entry.text));
        println!(
            "{} [{}] {} \u{ab}{}\u{bb}",
            format_handle(entry.offset).bright_yellow(),
            entry.kind.label(),
            local_time(&entry.timestamp).format("%Y-%m-%d %H:%M:%S"),
            rendered
        );
    }
}

#[derive(Serialize)]
struct EntryJson {
    handle: String,
    kind: &'static str,
    timestamp: String,
    text: String,
}

fn print_json(entries: &[Entry]) {
    let out: Vec<EntryJson> = entries
        .iter()
        .map(|entry| EntryJson {
            handle: format_handle(entry.offset),
            kind: entry.kind.label(),
            timestamp: entry.timestamp.to_rfc3339(),
            text: render_text(entry.kind, &entry.text),
        })
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&out) {
        println!("{json}");
    }
}
