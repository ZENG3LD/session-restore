//! Owner/peer message classification shared by `list`, `load`, and every
//! wave-2 command.
//!
//! Delegates the actual "who sent this" decision to
//! [`claude_session_restore::transcript::events::IncomingKind`] (spec:
//! `docs/session-restore/research/2026-09-25-claude-transcript-message-taxonomy.md`),
//! applied uniformly to `user` turns and mid-turn `queued_command`
//! deliveries. This module only assembles the digest-facing views: the
//! merged owner-message timeline, the peer-message timeline, and the
//! text-matched (not FIFO) queue-delivery tracker. Every message carries the
//! byte offset of the line it came from — its `@o<offset>` handle.

use crate::io::OffsetEvent;
use chrono::{DateTime, Utc};
use claude_session_restore::transcript::events::attachment::AttachmentType;
use claude_session_restore::transcript::events::{
    render_owner_command, strip_system_reminder_blocks, unwrap_pasted_content, IncomingKind, SessionEvent,
};

/// A queue-delivery match compares at most this many normalized characters —
/// long messages only need their lead to prove which later record delivered
/// them.
const MATCH_PREFIX_CHARS: usize = 200;

/// How an owner message reached the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// A normal `user` conversation turn.
    Turn,
    /// Sent while a turn was already running, delivered mid-turn as a
    /// `queued_command` attachment rather than becoming its own turn.
    MidTurn,
}

impl DeliveryMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::MidTurn => "mid-turn",
        }
    }
}

/// A single owner-authored message, ready for display.
#[derive(Debug, Clone)]
pub struct HumanMessage {
    pub offset: u64,
    pub text: String,
    pub timestamp: DateTime<Utc>,
    pub delivery: DeliveryMode,
}

/// A message from another session: a cross-session message, a
/// host-injected notice, or a subagent hand-back.
#[derive(Debug, Clone)]
pub struct PeerMessage {
    pub offset: u64,
    pub text: String,
    pub sender: Option<String>,
    pub handback: bool,
    pub timestamp: DateTime<Utc>,
}

/// Collect every owner message found in `events`, in file (chronological)
/// order — the merge of normal `user` turns and mid-turn `queued_command`
/// deliveries.
pub fn human_messages(events: &[OffsetEvent]) -> Vec<HumanMessage> {
    let mut messages = Vec::new();

    for oe in events {
        match &oe.event {
            SessionEvent::User(user) => match user.incoming_kind() {
                IncomingKind::Owner(text) => {
                    messages.push(HumanMessage {
                        offset: oe.offset,
                        text,
                        timestamp: user.timestamp(),
                        delivery: DeliveryMode::Turn,
                    });
                }
                IncomingKind::OwnerCommand { name, args } => {
                    messages.push(HumanMessage {
                        offset: oe.offset,
                        text: render_owner_command(&name, &args),
                        timestamp: user.timestamp(),
                        delivery: DeliveryMode::Turn,
                    });
                }
                _ => {}
            },
            SessionEvent::Attachment(attachment) => {
                if let AttachmentType::QueuedCommand(queued) = &attachment.attachment {
                    if let IncomingKind::OwnerMidTurn(text) = queued.incoming_kind() {
                        messages.push(HumanMessage {
                            offset: oe.offset,
                            text,
                            timestamp: queued.timestamp.unwrap_or(attachment.metadata.timestamp),
                            delivery: DeliveryMode::MidTurn,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    messages
}

/// Collect every peer message found in `events` — another session's
/// cross-session message, a host-injected notice, or a subagent hand-back —
/// in file order.
pub fn peer_messages(events: &[OffsetEvent]) -> Vec<PeerMessage> {
    let mut messages = Vec::new();

    for oe in events {
        match &oe.event {
            SessionEvent::User(user) => {
                if let IncomingKind::Peer { text, sender, handback } = user.incoming_kind() {
                    messages.push(PeerMessage { offset: oe.offset, text, sender, handback, timestamp: user.timestamp() });
                }
            }
            SessionEvent::Attachment(attachment) => {
                if let AttachmentType::QueuedCommand(queued) = &attachment.attachment {
                    if let IncomingKind::Peer { text, sender, handback } = queued.incoming_kind() {
                        messages.push(PeerMessage {
                            offset: oe.offset,
                            text,
                            sender,
                            handback,
                            timestamp: queued.timestamp.unwrap_or(attachment.metadata.timestamp),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    messages
}

/// Normalize text for queue-delivery matching: strip any `<system-reminder>`
/// wrapper and `<pasted_content>` wrapper tags exactly as the classifier
/// does, collapse whitespace runs to single spaces, and cap the result —
/// applied identically to a queue item's own content and to every candidate
/// delivery, so a raw (unclassified) `remove`/enqueue string and an
/// already-classified `Owner`/`OwnerMidTurn` string compare equal.
fn normalize_for_match(text: &str) -> String {
    let stripped = strip_system_reminder_blocks(text);
    let unwrapped = unwrap_pasted_content(&stripped);
    unwrapped.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MATCH_PREFIX_CHARS).collect()
}

/// Raw comparable text carried by a later record that can deliver a queued
/// item: a `user` turn's own text block, a `queued_command` attachment's
/// prompt, or a `queue-operation remove`'s content. `dequeue` never carries
/// content and is not a delivery signal.
fn candidate_text(event: &SessionEvent) -> Option<&str> {
    match event {
        SessionEvent::User(user) => user.message.content.iter().find_map(|block| block.as_text()),
        SessionEvent::Attachment(attachment) => match &attachment.attachment {
            AttachmentType::QueuedCommand(queued) => Some(queued.prompt.as_str()),
            _ => None,
        },
        SessionEvent::QueueOperation(op) if op.operation == "remove" => op.content.as_deref(),
        _ => None,
    }
}

/// A queued owner message that was enqueued but never delivered.
#[derive(Debug, Clone)]
pub struct StuckQueueItem {
    pub offset: u64,
    pub content: String,
    pub enqueued_at: DateTime<Utc>,
}

/// Queue-delivery tracking result (spec section C): owner-kind items that
/// never matched a later delivery, plus a count of non-owner (peer/
/// task-notification) items that likewise never matched — reported as one
/// summary line rather than individually.
#[derive(Debug, Clone, Default)]
pub struct StuckSummary {
    pub owner_items: Vec<StuckQueueItem>,
    pub other_leftover_count: usize,
}

/// Track `queue-operation enqueue` items against every later record that
/// could deliver them, matched by normalized text rather than FIFO
/// operation pairing — a bare `dequeue` never carries content, so it can
/// never be the delivery signal (spec section C).
pub fn stuck_queue_items(events: &[OffsetEvent]) -> StuckSummary {
    let mut summary = StuckSummary::default();

    for (index, oe) in events.iter().enumerate() {
        let SessionEvent::QueueOperation(op) = &oe.event else { continue };
        if op.operation != "enqueue" {
            continue;
        }
        let Some(content) = &op.content else { continue };
        let Some(kind) = op.incoming_kind() else { continue };

        let needle = normalize_for_match(content);
        let delivered = events[index + 1..]
            .iter()
            .filter_map(|later| candidate_text(&later.event))
            .any(|candidate| normalize_for_match(candidate) == needle);
        if delivered {
            continue;
        }

        match kind {
            IncomingKind::Owner(_) | IncomingKind::OwnerCommand { .. } => {
                summary.owner_items.push(StuckQueueItem { offset: oe.offset, content: content.clone(), enqueued_at: op.timestamp });
            }
            IncomingKind::Peer { .. } | IncomingKind::TaskNotification(_) => {
                summary.other_leftover_count += 1;
            }
            _ => {}
        }
    }

    summary
}

/// One `[Request interrupted by user…]` marker, with its handle.
#[derive(Debug, Clone, Copy)]
pub struct InterruptMarker {
    pub offset: u64,
}

/// End-of-window state used for the "Stuck / not answered" section:
/// whether the last owner message went unanswered, and every interrupt
/// marker's handle.
#[derive(Debug, Clone, Default)]
pub struct EndState {
    pub unanswered: bool,
    pub interrupts: Vec<InterruptMarker>,
}

/// Scan `events` once to determine whether the last owner message in the
/// window was followed by any assistant text, and collect interruption
/// markers along the way.
pub fn scan_end_state(events: &[OffsetEvent]) -> EndState {
    let mut last_human_seen = false;
    let mut answered_since_last_human = true;
    let mut interrupts = Vec::new();

    for oe in events {
        match &oe.event {
            SessionEvent::User(user) => match user.incoming_kind() {
                IncomingKind::Owner(_) | IncomingKind::OwnerCommand { .. } => {
                    last_human_seen = true;
                    answered_since_last_human = false;
                }
                IncomingKind::Interrupt(_) => {
                    interrupts.push(InterruptMarker { offset: oe.offset });
                    last_human_seen = true;
                    answered_since_last_human = false;
                }
                _ => {}
            },
            SessionEvent::Attachment(attachment) => {
                if let AttachmentType::QueuedCommand(queued) = &attachment.attachment {
                    if matches!(queued.incoming_kind(), IncomingKind::OwnerMidTurn(_)) {
                        last_human_seen = true;
                        answered_since_last_human = false;
                    }
                }
            }
            SessionEvent::Assistant(assistant)
                if assistant.message.content.iter().any(|block| block.as_text().is_some()) =>
            {
                answered_since_last_human = true;
            }
            _ => {}
        }
    }

    EndState { unanswered: last_human_seen && !answered_since_last_human, interrupts }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(offset: u64, json: &str) -> OffsetEvent {
        OffsetEvent { offset, event: serde_json::from_str(json).expect("fixture event must parse") }
    }

    #[test]
    fn human_messages_merges_turn_and_mid_turn_in_file_order_with_offsets() {
        let events = vec![
            event(0, r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-09-24T23:00:00Z","isSidechain":false,"userType":"external","origin":{"kind":"human"},"cwd":"/work","message":{"role":"user","content":"placeholder normal prompt"}}"#),
            event(200, r#"{"parentUuid":null,"isSidechain":false,"attachment":{"type":"queued_command","prompt":"placeholder mid-turn message","source_uuid":"su","commandMode":"prompt","origin":{"kind":"human"},"timestamp":"2026-01-01T00:00:10.816Z","humanTurn":true},"type":"attachment","uuid":"att1","timestamp":"2026-01-01T00:00:10.816Z","userType":"external","cwd":"/work","sessionId":"s"}"#),
            event(400, r#"{"parentUuid":null,"isSidechain":false,"attachment":{"type":"queued_command","prompt":"<task-notification>\n<task-id>t</task-id>\n</task-notification>","source_uuid":"su2","commandMode":"task-notification","timestamp":"2026-09-24T23:10:00Z"},"type":"attachment","uuid":"att2","timestamp":"2026-09-24T23:10:00Z","userType":"external","cwd":"/work","sessionId":"s"}"#),
        ];

        let messages = human_messages(&events);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "placeholder normal prompt");
        assert_eq!(messages[0].offset, 0);
        assert_eq!(messages[0].delivery, DeliveryMode::Turn);
        assert_eq!(messages[1].text, "placeholder mid-turn message");
        assert_eq!(messages[1].offset, 200);
        assert_eq!(messages[1].delivery, DeliveryMode::MidTurn);
    }

    #[test]
    fn peer_messages_collects_cross_session_and_handback() {
        let events = vec![
            event(0, r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-09-24T23:00:00Z","isSidechain":false,"userType":"external","isMeta":true,"origin":{"kind":"peer","from":"f1","name":"nemo-76","body":"placeholder peer text"},"cwd":"/work","message":{"role":"user","content":"Another Claude session sent a message:\n<cross-session-message from=\"f1\" name=\"nemo-76\">placeholder peer text</cross-session-message>"}}"#),
            event(500, r#"{"type":"user","uuid":"u2","sessionId":"s","timestamp":"2026-09-24T23:01:00Z","isSidechain":false,"userType":"external","isMeta":true,"origin":{"kind":"peer","from":"a1","senderTaskId":"a1","body":"[Subagent hand-back] placeholder report","handback":true},"cwd":"/work","message":{"role":"user","content":"Another Claude session sent a message:\n<agent-message from=\"a1\">[Subagent hand-back] placeholder report</agent-message>"}}"#),
        ];

        let messages = peer_messages(&events);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "placeholder peer text");
        assert_eq!(messages[0].offset, 0);
        assert_eq!(messages[0].sender.as_deref(), Some("nemo-76"));
        assert!(!messages[0].handback);
        assert_eq!(messages[1].text, "[Subagent hand-back] placeholder report");
        assert_eq!(messages[1].offset, 500);
        assert!(messages[1].handback);
    }

    // ------------------------------------------------------------------
    // Stuck detection: matched by normalized text against ANY later
    // record, never FIFO (spec section C).
    // ------------------------------------------------------------------

    #[test]
    fn delivered_via_content_less_dequeue_then_user_record() {
        // Real lifecycle (survey rows 0/1/3): enqueue(text) →
        // dequeue(no content, ignored) → user record with the same text.
        let events = vec![
            event(0, r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-09-24T20:08:29.825Z","sessionId":"s","content":"placeholder owner text"}"#),
            event(100, r#"{"type":"queue-operation","operation":"dequeue","timestamp":"2026-09-24T20:08:30.672Z","sessionId":"s"}"#),
            event(200, r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-09-24T20:08:31.823Z","isSidechain":false,"userType":"external","origin":{"kind":"human"},"cwd":"/work","message":{"role":"user","content":"placeholder owner text"}}"#),
        ];

        let summary = stuck_queue_items(&events);
        assert!(summary.owner_items.is_empty(), "must be delivered, not stuck: {summary:?}");
    }

    #[test]
    fn delivered_via_queued_command_then_remove() {
        // Real lifecycle (survey rows ~100-109): enqueue(text) →
        // queued_command attachment(same text) → remove(same text,
        // absorbed_mid_turn).
        let events = vec![
            event(0, r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-09-24T20:23:00.152Z","sessionId":"s","content":"placeholder mid-turn text"}"#),
            event(100, r#"{"parentUuid":null,"isSidechain":false,"attachment":{"type":"queued_command","prompt":"placeholder mid-turn text","source_uuid":"su","commandMode":"prompt","origin":{"kind":"human"},"timestamp":"2026-09-24T20:23:40Z","humanTurn":true},"type":"attachment","uuid":"att1","timestamp":"2026-09-24T20:23:40Z","userType":"external","cwd":"/work","sessionId":"s"}"#),
            event(200, r#"{"type":"queue-operation","operation":"remove","timestamp":"2026-09-24T20:23:53.808Z","sessionId":"s","content":"placeholder mid-turn text","reason":"absorbed_mid_turn"}"#),
        ];

        let summary = stuck_queue_items(&events);
        assert!(summary.owner_items.is_empty(), "must be delivered, not stuck: {summary:?}");
    }

    #[test]
    fn enqueue_with_nothing_after_is_stuck() {
        let events = vec![event(
            42,
            r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-09-24T20:08:29.825Z","sessionId":"s","content":"placeholder never delivered"}"#,
        )];

        let summary = stuck_queue_items(&events);
        assert_eq!(summary.owner_items.len(), 1);
        assert_eq!(summary.owner_items[0].content, "placeholder never delivered");
        assert_eq!(summary.owner_items[0].offset, 42);
    }

    #[test]
    fn bare_dequeues_without_a_matching_enqueue_are_ignored() {
        // Common per the field survey — must not crash or spuriously match.
        let events = vec![
            event(0, r#"{"type":"queue-operation","operation":"dequeue","timestamp":"2026-09-24T20:08:30.672Z","sessionId":"s"}"#),
            event(100, r#"{"type":"queue-operation","operation":"dequeue","timestamp":"2026-09-24T20:09:30.672Z","sessionId":"s"}"#),
        ];
        let summary = stuck_queue_items(&events);
        assert!(summary.owner_items.is_empty());
        assert_eq!(summary.other_leftover_count, 0);
    }

    #[test]
    fn owner_text_with_leading_reminder_matches_the_stripped_user_delivery() {
        // Real lifecycle (survey row 695 → 696): the enqueue's raw content
        // still carries the reminder wrapper; the delivering `user` record's
        // classified text has it stripped. Matching must normalize both.
        let events = vec![
            event(0, r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-09-24T21:34:40.769Z","sessionId":"s","content":"<system-reminder>\nThe user started your suggested background task.\n</system-reminder>\n\nplaceholder owner question"}"#),
            event(300, r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-09-24T21:34:40.791Z","isSidechain":false,"userType":"external","origin":{"kind":"human"},"cwd":"/work","message":{"role":"user","content":"<system-reminder>\nThe user started your suggested background task.\n</system-reminder>\n\nplaceholder owner question"}}"#),
        ];

        let summary = stuck_queue_items(&events);
        assert!(summary.owner_items.is_empty(), "must be delivered, not stuck: {summary:?}");
    }

    #[test]
    fn peer_and_task_notification_leftovers_count_but_are_not_owner_items() {
        let events = vec![
            event(0, r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-09-24T20:18:32.894Z","sessionId":"s","content":"<task-notification>\n<task-id>t</task-id>\n</task-notification>"}"#),
            event(100, r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-09-24T20:19:32.894Z","sessionId":"s","content":"<cross-session-message from=\"x\">placeholder</cross-session-message>"}"#),
        ];
        let summary = stuck_queue_items(&events);
        assert!(summary.owner_items.is_empty());
        assert_eq!(summary.other_leftover_count, 2);
    }

    #[test]
    fn scan_end_state_flags_unanswered_last_human_message() {
        let events = vec![event(
            0,
            r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-09-24T23:00:00Z","isSidechain":false,"userType":"external","origin":{"kind":"human"},"cwd":"/work","message":{"role":"user","content":"placeholder final question"}}"#,
        )];
        let state = scan_end_state(&events);
        assert!(state.unanswered);
    }

    #[test]
    fn scan_end_state_clears_unanswered_after_assistant_text() {
        let events = vec![
            event(0, r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-09-24T23:00:00Z","isSidechain":false,"userType":"external","origin":{"kind":"human"},"cwd":"/work","message":{"role":"user","content":"placeholder question"}}"#),
            event(300, r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","timestamp":"2026-09-24T23:00:01Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"placeholder answer"}]}}"#),
        ];
        let state = scan_end_state(&events);
        assert!(!state.unanswered);
    }

    #[test]
    fn scan_end_state_collects_interrupt_handles() {
        let events = vec![event(
            777,
            r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-09-24T23:00:00Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
        )];
        let state = scan_end_state(&events);
        assert_eq!(state.interrupts.len(), 1);
        assert_eq!(state.interrupts[0].offset, 777);
    }
}
