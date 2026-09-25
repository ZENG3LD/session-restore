//! `show <session> <handle>...` — the full, verbatim record for one or more
//! handles printed by `load` or any other wave-2 command.

use crate::format::{local_time, truncate_chars_reporting};
use crate::handle::{format_handle, parse_handle};
use crate::io::{self, read_window_after};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use claude_session_restore::transcript::events::attachment::AttachmentType;
use claude_session_restore::transcript::events::root::{AssistantEvent, UserEvent};
use claude_session_restore::transcript::events::{
    render_owner_command, ContentBlock, IncomingKind, QueueOperation, RootAttachmentEvent, SessionEvent, SystemEvent,
};
use colored::Colorize;
use serde::Serialize;
use std::path::Path;

/// Read window used to capture one full JSONL line starting at a handle's
/// offset — generous, since a `progress` line embedding a full conversation
/// replay can be large, but `show` is meant for the message/report/tool-op/
/// error/compaction/notification handles `load` prints, none of which
/// approach this.
const SHOW_LINE_WINDOW_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_CHARS: usize = 20_000;

struct ShownPiece {
    label: String,
    text: String,
    truncated_chars: Option<usize>,
}

struct ShownRecord {
    offset: u64,
    kind: String,
    timestamp: Option<DateTime<Utc>>,
    pieces: Vec<ShownPiece>,
}

pub fn run(path: &Path, handles: &[String], max_chars: usize, json: bool) -> Result<()> {
    let mut records = Vec::new();
    for handle in handles {
        let Some(offset) = parse_handle(handle) else {
            anyhow::bail!("Not a valid handle: {handle} (expected @o<byte-offset>)");
        };
        records.push(load_record(path, offset, max_chars)?);
    }

    if json {
        print_json(&records);
    } else {
        print_human(&records);
    }
    Ok(())
}

fn load_record(path: &Path, offset: u64, max_chars: usize) -> Result<ShownRecord> {
    let window = read_window_after(path, offset, SHOW_LINE_WINDOW_BYTES)
        .with_context(|| format!("Failed to read at handle {}", format_handle(offset)))?;
    let Some(line) = window.first() else {
        anyhow::bail!("No line found at handle {} (past end of file?)", format_handle(offset));
    };
    if line.offset != offset {
        anyhow::bail!(
            "Handle {} does not point to the start of a line in {}",
            format_handle(offset),
            path.display()
        );
    }
    let Some(event) = io::parse_line(&line.text) else {
        anyhow::bail!("Line at handle {} is not a parseable session event", format_handle(offset));
    };
    Ok(render_record(offset, &event, max_chars))
}

fn text_piece(label: &str, text: &str, max_chars: usize) -> ShownPiece {
    let (shown, truncated_chars) = truncate_chars_reporting(text, max_chars);
    ShownPiece { label: label.to_string(), text: shown, truncated_chars }
}

fn render_record(offset: u64, event: &SessionEvent, max_chars: usize) -> ShownRecord {
    match event {
        SessionEvent::User(user) => render_user(offset, user, max_chars),
        SessionEvent::Assistant(assistant) => render_assistant(offset, assistant, max_chars),
        SessionEvent::System(sys) => render_system(offset, sys, max_chars),
        SessionEvent::QueueOperation(op) => render_queue_operation(offset, op, max_chars),
        SessionEvent::Attachment(attachment) => render_attachment(offset, attachment, max_chars),
        other => ShownRecord {
            offset,
            kind: format!("{other}"),
            timestamp: None,
            pieces: vec![text_piece("raw", &format!("{other}"), max_chars)],
        },
    }
}

fn tool_result_content_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|item| item.get("text").and_then(serde_json::Value::as_str))
            .map(str::to_string),
        _ => None,
    }
}

fn render_user(offset: u64, user: &UserEvent, max_chars: usize) -> ShownRecord {
    let timestamp = Some(user.timestamp());

    if user.is_compact_summary == Some(true) {
        let text = user.message.content.iter().find_map(ContentBlock::as_text).unwrap_or_default();
        // The compaction summary is always shown in full, never capped — it
        // is the densest pre-compaction recap and the whole reason `show`
        // exists for it.
        return ShownRecord {
            offset,
            kind: "compact-summary".to_string(),
            timestamp,
            pieces: vec![ShownPiece { label: "text".to_string(), text: text.to_string(), truncated_chars: None }],
        };
    }

    let mut pieces = Vec::new();
    let kind = match user.incoming_kind() {
        IncomingKind::Owner(text) => {
            pieces.push(text_piece("text", &text, max_chars));
            "owner".to_string()
        }
        IncomingKind::OwnerCommand { name, args } => {
            pieces.push(text_piece("command", &render_owner_command(&name, &args), max_chars));
            "owner-command".to_string()
        }
        IncomingKind::Peer { text, sender, handback } => {
            if let Some(sender) = sender {
                pieces.push(ShownPiece { label: "sender".to_string(), text: sender, truncated_chars: None });
            }
            pieces.push(text_piece("text", &text, max_chars));
            if handback { "peer-handback".to_string() } else { "peer".to_string() }
        }
        IncomingKind::TaskNotification(text) => {
            pieces.push(text_piece("text", &text, max_chars));
            "task-notification".to_string()
        }
        IncomingKind::Interrupt(text) => {
            pieces.push(text_piece("text", &text, max_chars));
            "interrupt".to_string()
        }
        IncomingKind::OwnerMidTurn(_) | IncomingKind::CompactSummary => {
            // A `user` event's classifier never returns these — mid-turn is
            // only ever produced from a `queued_command` attachment, and the
            // compaction case is handled above.
            "harness".to_string()
        }
        IncomingKind::Harness => {
            if let Some(text) = user.message.content.iter().find_map(ContentBlock::as_text) {
                pieces.push(text_piece("text", text, max_chars));
            }
            "harness".to_string()
        }
    };

    for block in &user.message.content {
        if let ContentBlock::ToolResult(result) = block {
            if let Some(text) = tool_result_content_text(&result.content) {
                pieces.push(text_piece("tool_result", &text, max_chars));
            }
        }
    }

    ShownRecord { offset, kind, timestamp, pieces }
}

fn render_assistant(offset: u64, assistant: &AssistantEvent, max_chars: usize) -> ShownRecord {
    let mut pieces = Vec::new();
    for block in &assistant.message.content {
        match block {
            ContentBlock::Text(text_block) => pieces.push(text_piece("text", &text_block.text, max_chars)),
            ContentBlock::ToolUse(tool) => {
                let input = serde_json::to_string_pretty(&tool.input).unwrap_or_else(|_| tool.input.to_string());
                pieces.push(text_piece(&format!("tool_use:{}", tool.name), &input, max_chars));
            }
            ContentBlock::Thinking(thinking) => pieces.push(text_piece("thinking", &thinking.thinking, max_chars)),
            _ => {}
        }
    }
    ShownRecord { offset, kind: "assistant".to_string(), timestamp: Some(assistant.timestamp()), pieces }
}

fn render_system(offset: u64, sys: &SystemEvent, max_chars: usize) -> ShownRecord {
    let mut pieces = Vec::new();
    let kind = sys.subtype.clone().unwrap_or_else(|| "system".to_string());
    if let Some(error) = &sys.error {
        pieces.push(text_piece(&format!("error:{}", error.error_type), &error.message, max_chars));
    }
    if let Some(content) = &sys.content {
        pieces.push(text_piece("content", content, max_chars));
    }
    if let Some(meta) = &sys.compact_metadata {
        pieces.push(ShownPiece {
            label: "compact_boundary".to_string(),
            text: format!("trigger={} preTokens={} postTokens={:?}", meta.trigger, meta.pre_tokens, meta.post_tokens),
            truncated_chars: None,
        });
    }
    ShownRecord { offset, kind, timestamp: Some(sys.timestamp), pieces }
}

fn render_queue_operation(offset: u64, op: &QueueOperation, max_chars: usize) -> ShownRecord {
    let mut pieces = vec![ShownPiece { label: "operation".to_string(), text: op.operation.clone(), truncated_chars: None }];
    if let Some(content) = &op.content {
        pieces.push(text_piece("content", content, max_chars));
    }
    if let Some(reason) = &op.reason {
        pieces.push(ShownPiece { label: "reason".to_string(), text: reason.clone(), truncated_chars: None });
    }
    ShownRecord { offset, kind: "queue-operation".to_string(), timestamp: Some(op.timestamp), pieces }
}

fn render_attachment(offset: u64, attachment: &RootAttachmentEvent, max_chars: usize) -> ShownRecord {
    if let AttachmentType::QueuedCommand(queued) = &attachment.attachment {
        let timestamp = Some(queued.timestamp.unwrap_or(attachment.metadata.timestamp));
        let mut pieces = Vec::new();
        let kind = match queued.incoming_kind() {
            IncomingKind::OwnerMidTurn(text) => {
                pieces.push(text_piece("text", &text, max_chars));
                "owner-mid-turn".to_string()
            }
            IncomingKind::Peer { text, sender, handback } => {
                if let Some(sender) = sender {
                    pieces.push(ShownPiece { label: "sender".to_string(), text: sender, truncated_chars: None });
                }
                pieces.push(text_piece("text", &text, max_chars));
                if handback { "peer-handback".to_string() } else { "peer".to_string() }
            }
            IncomingKind::TaskNotification(text) => {
                pieces.push(text_piece("text", &text, max_chars));
                "task-notification".to_string()
            }
            _ => {
                pieces.push(text_piece("prompt", &queued.prompt, max_chars));
                "harness".to_string()
            }
        };
        return ShownRecord { offset, kind, timestamp, pieces };
    }

    let raw = serde_json::to_string_pretty(&attachment.attachment).unwrap_or_default();
    ShownRecord {
        offset,
        kind: format!("attachment:{}", attachment.attachment.type_name()),
        timestamp: Some(attachment.metadata.timestamp),
        pieces: vec![text_piece("raw", &raw, max_chars)],
    }
}

fn print_human(records: &[ShownRecord]) {
    for (index, record) in records.iter().enumerate() {
        if index > 0 {
            println!();
        }
        println!("{}", "═══════════════════════════════════════".bright_cyan());
        println!("{} {}", "Handle:".bold(), format_handle(record.offset).bright_yellow());
        println!("{} {}", "Kind:".bold(), record.kind);
        if let Some(timestamp) = record.timestamp {
            println!("{} {}", "Time:".bold(), local_time(&timestamp).format("%Y-%m-%d %H:%M:%S %z"));
        }
        for piece in &record.pieces {
            println!("\n{}", format!("[{}]", piece.label).bold());
            println!("{}", piece.text);
            if let Some(cut) = piece.truncated_chars {
                println!("{}", format!("[truncated {cut} chars — raise --max-chars to see more]").dimmed());
            }
        }
    }
}

#[derive(Serialize)]
struct ShownPieceJson {
    label: String,
    text: String,
    truncated_chars: Option<usize>,
}

#[derive(Serialize)]
struct ShownRecordJson {
    handle: String,
    kind: String,
    timestamp: Option<String>,
    pieces: Vec<ShownPieceJson>,
}

fn print_json(records: &[ShownRecord]) {
    let out: Vec<ShownRecordJson> = records
        .iter()
        .map(|record| ShownRecordJson {
            handle: format_handle(record.offset),
            kind: record.kind.clone(),
            timestamp: record.timestamp.map(|value| value.to_rfc3339()),
            pieces: record
                .pieces
                .iter()
                .map(|piece| ShownPieceJson {
                    label: piece.label.clone(),
                    text: piece.text.clone(),
                    truncated_chars: piece.truncated_chars,
                })
                .collect(),
        })
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&out) {
        println!("{json}");
    }
}
