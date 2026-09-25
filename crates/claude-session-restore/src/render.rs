//! Shared classification-to-display glue for every wave-2 command
//! (`messages`, `grep`, `span`): one classifier
//! ([`claude_session_restore::transcript::events::IncomingKind`]), one renderer.
//!
//! `load` keeps its own richer, section-specific formatting (numbered
//! lists, delivery-mode tags, char-count truncation notes) built directly
//! on [`crate::human`]/[`crate::subagents`] — this module is for the
//! flatter, chronological "one line per record" views the wave-2 commands
//! share.

use crate::format::{collapse_paragraphs, truncate};
use crate::handle::format_handle;
use crate::io::OffsetEvent;
use chrono::{DateTime, Utc};
use claude_session_restore::transcript::events::attachment::AttachmentType;
use claude_session_restore::transcript::events::{IncomingKind, SessionEvent};

/// A digest-facing message kind — the label shown next to every handle in
/// `messages`/`grep`/`span`, and the `--kind` filter vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// The owner, as a normal turn, mid-turn delivery, or slash command.
    Owner,
    /// Another session, subagent hand-back, or host-injected message.
    Peer,
    /// Main-chain assistant text (never a subagent's own sidechain text).
    Agent,
    /// A background agent/Bash task completion notice.
    Notification,
}

impl MessageKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Peer => "peer",
            Self::Agent => "agent",
            Self::Notification => "notification",
        }
    }

    /// Parse a `--kind` filter value. `"all"` is handled by the caller
    /// (an empty/absent filter set), not by this parser.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "owner" => Some(Self::Owner),
            "peer" => Some(Self::Peer),
            "agent" => Some(Self::Agent),
            "notification" => Some(Self::Notification),
            _ => None,
        }
    }
}

/// One classified, renderable record: a handle, a kind, a time, and its
/// verbatim text.
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    pub offset: u64,
    pub kind: MessageKind,
    pub timestamp: DateTime<Utc>,
    pub text: String,
    /// Peer sender name/id, when known. `None` for every other kind.
    pub sender: Option<String>,
}

/// Extract every renderable message out of a single parsed line: an owner
/// turn or mid-turn delivery, a peer message, a background-task
/// notification, or a main-chain assistant text block. Sidechain assistant
/// turns (subagent transcripts embedded in the main file) are skipped —
/// `agent <session> <id>` reads a subagent's *own* transcript file instead.
#[must_use]
pub fn classify(oe: &OffsetEvent) -> Vec<RenderedMessage> {
    match &oe.event {
        SessionEvent::User(user) => {
            let timestamp = user.timestamp();
            match user.incoming_kind() {
                IncomingKind::Owner(text) => vec![render(oe.offset, MessageKind::Owner, timestamp, text, None)],
                IncomingKind::OwnerCommand { name, args } => vec![render(
                    oe.offset,
                    MessageKind::Owner,
                    timestamp,
                    claude_session_restore::transcript::events::render_owner_command(&name, &args),
                    None,
                )],
                IncomingKind::Peer { text, sender, .. } => {
                    vec![render(oe.offset, MessageKind::Peer, timestamp, text, sender)]
                }
                IncomingKind::TaskNotification(text) => {
                    vec![render(oe.offset, MessageKind::Notification, timestamp, text, None)]
                }
                _ => Vec::new(),
            }
        }
        SessionEvent::Attachment(attachment) => {
            let AttachmentType::QueuedCommand(queued) = &attachment.attachment else { return Vec::new() };
            let timestamp = queued.timestamp.unwrap_or(attachment.metadata.timestamp);
            match queued.incoming_kind() {
                IncomingKind::OwnerMidTurn(text) => vec![render(oe.offset, MessageKind::Owner, timestamp, text, None)],
                IncomingKind::Peer { text, sender, .. } => {
                    vec![render(oe.offset, MessageKind::Peer, timestamp, text, sender)]
                }
                IncomingKind::TaskNotification(text) => {
                    vec![render(oe.offset, MessageKind::Notification, timestamp, text, None)]
                }
                _ => Vec::new(),
            }
        }
        SessionEvent::Assistant(assistant) => {
            if assistant.metadata.is_sidechain {
                return Vec::new();
            }
            let timestamp = assistant.timestamp();
            assistant
                .message
                .content
                .iter()
                .filter_map(|block| block.as_text())
                .map(|text| render(oe.offset, MessageKind::Agent, timestamp, text.to_string(), None))
                .collect()
        }
        _ => Vec::new(),
    }
}

fn render(offset: u64, kind: MessageKind, timestamp: DateTime<Utc>, text: String, sender: Option<String>) -> RenderedMessage {
    RenderedMessage { offset, kind, timestamp, text, sender }
}

/// One display line: `@o<offset> [kind] time (sender) «text»`, capped at
/// `max_chars` and paragraph-collapsed for readability. Shared by
/// `messages`, `grep`, and `span`.
#[must_use]
pub fn render_line(message: &RenderedMessage, max_chars: usize) -> String {
    let handle = format_handle(message.offset);
    let time = crate::format::local_time(&message.timestamp).format("%Y-%m-%d %H:%M:%S");
    let sender = message.sender.as_deref().map(|name| format!(" ({name})")).unwrap_or_default();
    let text = truncate(&collapse_paragraphs(&message.text), max_chars);
    format!("{handle} [{}]{sender} {time} «{text}»", message.kind.label())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(offset: u64, json: &str) -> OffsetEvent {
        OffsetEvent { offset, event: serde_json::from_str(json).expect("fixture event must parse") }
    }

    #[test]
    fn classify_extracts_owner_text() {
        let oe = event(
            10,
            r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-09-24T23:00:00Z","isSidechain":false,"userType":"external","origin":{"kind":"human"},"cwd":"/work","message":{"role":"user","content":"placeholder"}}"#,
        );
        let rendered = classify(&oe);
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].kind, MessageKind::Owner);
        assert_eq!(rendered[0].offset, 10);
        assert_eq!(rendered[0].text, "placeholder");
    }

    #[test]
    fn classify_skips_sidechain_assistant_text() {
        let oe = event(
            10,
            r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":true,"cwd":"/work","message":{"model":"claude-test","id":"m1","type":"message","role":"assistant","content":[{"type":"text","text":"sidechain text"}]}}"#,
        );
        assert!(classify(&oe).is_empty());
    }

    #[test]
    fn classify_extracts_main_chain_assistant_text_as_agent_kind() {
        let oe = event(
            10,
            r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"m1","type":"message","role":"assistant","content":[{"type":"text","text":"main chain report"}]}}"#,
        );
        let rendered = classify(&oe);
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].kind, MessageKind::Agent);
        assert_eq!(rendered[0].text, "main chain report");
    }

    #[test]
    fn message_kind_parse_round_trips_labels() {
        for kind in [MessageKind::Owner, MessageKind::Peer, MessageKind::Agent, MessageKind::Notification] {
            assert_eq!(MessageKind::parse(kind.label()), Some(kind));
        }
        assert_eq!(MessageKind::parse("bogus"), None);
    }

    #[test]
    fn render_line_includes_handle_kind_time_and_text() {
        let message = RenderedMessage {
            offset: 42,
            kind: MessageKind::Owner,
            timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().with_timezone(&Utc),
            text: "hello world".to_string(),
            sender: None,
        };
        let line = render_line(&message, 400);
        assert!(line.starts_with("@o42 [owner]"));
        assert!(line.contains("hello world"));
    }
}
