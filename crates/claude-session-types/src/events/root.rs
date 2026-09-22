//! Root-level event types (Level 1)
//!
//! All events in Claude Code JSONL files parse into one of these 7 root types.
//!
//! # Root Event Types
//!
//! ```text
//! Root Events (.type)
//! ├── progress (82,200)              - Progress updates (MOST FREQUENT)
//! ├── assistant (49,426)             - Assistant responses
//! ├── user (29,913)                  - User messages
//! ├── file-history-snapshot (65)     - File state snapshots
//! ├── queue-operation (58)           - Queue management
//! ├── system (6)                     - System messages
//! └── summary (1)                    - Session summaries
//! ```
//!
//! # Parsing Strategy
//!
//! Use serde's tagged enum to automatically parse based on `type` field:
//!
//! ```rust
//! use claude_session_types::events::SessionEvent;
//!
//! let line = r#"{"type": "user", "uuid": "test-uuid", "sessionId": "session-1", "timestamp": "2024-01-01T00:00:00Z", "isSidechain": false, "message": {"role": "user", "content": "hello"}}"#;
//! let event: SessionEvent = serde_json::from_str(line)?;
//!
//! match event {
//!     SessionEvent::User(_user) => { /* handle user message */ }
//!     SessionEvent::Assistant(_assistant) => { /* handle assistant */ }
//!     SessionEvent::Progress(_progress) => { /* handle progress */ }
//!     _ => {}
//! }
//! # Ok::<(), serde_json::Error>(())
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::fmt;

use super::message::{ContentBlock, MessageContent};
use super::metadata::EventMetadata;
use super::progress::{ProgressData, ProgressEvent};
use super::system::SystemEvent;
use super::tool_result::ToolUseResult;

/// Top-level session event discriminator
///
/// All events in a Claude Code session JSONL file parse into one of these variants.
/// Uses serde's tagged enum feature to automatically select variant based on `type` field.
///
/// # Frequency Distribution (per large session)
///
/// 1. `Progress`: ~82,200 (51%)
/// 2. `Assistant`: ~49,426 (31%)
/// 3. `User`: ~29,913 (19%)
/// 4. `FileSnapshot`: ~65 (<0.1%)
/// 5. `QueueOperation`: ~58 (<0.1%)
/// 6. `System`: ~6 (<0.1%)
/// 7. `Summary`: ~1 (<0.1%)
///
/// # Links and Relationships
///
/// Events form a conversation graph via:
/// - `uuid`: Unique identifier for this event
/// - `parent_uuid`: Links to previous message in conversation chain
/// - `tool_use_id`: Links progress to tool invocation
/// - `source_tool_assistant_uuid`: Links tool result back to assistant
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SessionEvent {
    /// User message event
    ///
    /// Represents messages from:
    /// - Human users (userType = "external")
    /// - Tool results (has tool_use_result field)
    ///
    /// # Links
    ///
    /// - `parent_uuid` → previous message
    /// - `source_tool_assistant_uuid` → assistant that invoked tool
    /// - Contains `.message.content[]` with text/tool_result types
    ///
    /// # Frequency
    ///
    /// ~29,913 events per large session (~19%)
    #[serde(rename = "user")]
    User(UserEvent),

    /// Assistant message event
    ///
    /// Represents responses from Claude, including:
    /// - Text responses
    /// - Tool use invocations
    /// - Token usage statistics
    ///
    /// # Links
    ///
    /// - `parent_uuid` → user message being responded to
    /// - Contains `.message.content[]` with text/tool_use types
    /// - Tool uses link to user tool results via `id`
    ///
    /// # Frequency
    ///
    /// ~49,426 events per large session (~31%)
    #[serde(rename = "assistant")]
    Assistant(AssistantEvent),

    /// Progress event
    ///
    /// Real-time updates during tool execution. Contains full conversation
    /// context in `.data.normalizedMessages[]`.
    ///
    /// # Links
    ///
    /// - `tool_use_id` → tool invocation that triggered this
    /// - `parent_uuid` → parent message
    /// - Contains full conversation history (HUGE!)
    ///
    /// # Frequency
    ///
    /// ~82,200 events per large session (~51% - MOST FREQUENT!)
    #[serde(rename = "progress")]
    Progress(ProgressEvent),

    /// System event
    ///
    /// System-level events:
    /// - Compact boundaries (conversation compaction)
    /// - API errors
    /// - System reminders
    ///
    /// # Links
    ///
    /// - `logical_parent_uuid` → last message before compaction
    ///
    /// # Frequency
    ///
    /// ~6 events per large session (rare but important!)
    #[serde(rename = "system")]
    System(SystemEvent),

    /// File history snapshot
    ///
    /// Tracks file state at message boundaries for undo/redo.
    ///
    /// # Links
    ///
    /// - `message_id` → message where snapshot was taken
    ///
    /// # Frequency
    ///
    /// ~65 events per large session
    #[serde(rename = "file-history-snapshot")]
    FileSnapshot(FileHistorySnapshot),

    /// Queue operation event
    ///
    /// Tracks session queue management (enqueue/dequeue).
    ///
    /// # Frequency
    ///
    /// ~58 events per large session
    #[serde(rename = "queue-operation")]
    QueueOperation(QueueOperation),

    /// Session summary
    ///
    /// Summary of entire session (typically at end).
    ///
    /// # Frequency
    ///
    /// ~1 event per session
    #[serde(rename = "summary")]
    Summary(SessionSummary),

    /// Root-level attachment event
    ///
    /// A hook result, todo reminder, or similar side-channel notice — the
    /// same payload shape as the nested `attachment` field found inside
    /// progress `normalizedMessages`, but emitted directly at the root. The
    /// dominant root event type on real transcripts (v2.1.2xx): dwarfs every
    /// other type combined in raw line count, though it carries no
    /// conversational text relevant to a restore digest.
    #[serde(rename = "attachment")]
    Attachment(RootAttachmentEvent),

    /// Custom session title
    ///
    /// The session title Claude Code's own UI shows, set by the user or the
    /// model. Repeats verbatim through the file as it is re-affirmed; the
    /// **last** occurrence is authoritative. This is the provider's own
    /// title/topic — prefer it over any inferred label.
    #[serde(rename = "custom-title")]
    CustomTitle(CustomTitleEvent),

    /// Model-generated session title
    ///
    /// A model-authored title, distinct from `custom-title` (which reflects
    /// an explicit/user-affirmed title). Used as the topic fallback when no
    /// `custom-title` is present.
    #[serde(rename = "ai-title")]
    AiTitle(AiTitleEvent),

    /// Latest verbatim user prompt
    ///
    /// Tracks the most recent human prompt text as the session progresses;
    /// updates repeatedly. Falls back to this for the topic when neither
    /// title event is present.
    #[serde(rename = "last-prompt")]
    LastPrompt(LastPromptEvent),

    /// Bridge session correlation (cloud sync identity) — not conversational
    /// content, tracked only so it does not fall into the generic unknown
    /// bucket.
    #[serde(rename = "bridge-session")]
    BridgeSession(BridgeSessionEvent),

    /// ATIS latch state — harness-internal signal, not conversational content.
    #[serde(rename = "atis-latch")]
    AtisLatch(AtisLatchEvent),

    /// Conversation mode marker (e.g. "normal") — harness-internal signal.
    #[serde(rename = "mode")]
    Mode(ModeEvent),

    /// Permission-mode marker (e.g. "auto") — harness-internal signal.
    #[serde(rename = "permission-mode")]
    PermissionMode(PermissionModeEvent),

    /// Agent/session display-name marker — harness-internal signal.
    #[serde(rename = "agent-name")]
    AgentName(AgentNameEvent),

    /// Incremental file-history delta (undo/redo tracking), the incremental
    /// counterpart to `file-history-snapshot`.
    #[serde(rename = "file-history-delta")]
    FileHistoryDelta(FileHistoryDeltaEvent),

    /// Unknown event type (forward compatibility)
    ///
    /// Anything not matched above lands here. This is normal on a
    /// continuously-evolving transcript format and must never be treated as a
    /// parse failure — the whole point of tolerant parsing is that one
    /// unrecognized root type never drops the rest of the line's siblings.
    #[serde(other)]
    Unknown,
}

impl SessionEvent {
    /// Extract common metadata present in most events
    pub fn metadata(&self) -> Option<EventMetadata> {
        match self {
            Self::User(e) => Some(e.metadata.clone()),
            Self::Assistant(e) => Some(e.metadata.clone()),
            Self::Progress(e) => Some(e.metadata.clone()),
            Self::System(e) => Some(e.metadata()),
            Self::Attachment(e) => Some(e.metadata.clone()),
            _ => None,
        }
    }

    /// Get UUID of this event
    pub fn uuid(&self) -> Option<&str> {
        match self {
            Self::User(e) => Some(&e.metadata.uuid),
            Self::Assistant(e) => Some(&e.metadata.uuid),
            Self::Progress(e) => Some(&e.metadata.uuid),
            Self::System(e) => e.uuid.as_deref(),
            Self::FileSnapshot(e) => Some(&e.message_id),
            Self::QueueOperation(e) => Some(&e.session_id),
            Self::Summary(e) => Some(&e.session_id),
            Self::Attachment(e) => Some(&e.metadata.uuid),
            Self::CustomTitle(e) => Some(&e.session_id),
            Self::AiTitle(e) => Some(&e.session_id),
            Self::LastPrompt(e) => Some(&e.session_id),
            Self::BridgeSession(e) => Some(&e.session_id),
            Self::AtisLatch(e) => Some(&e.session_id),
            Self::Mode(e) => Some(&e.session_id),
            Self::PermissionMode(e) => Some(&e.session_id),
            Self::AgentName(e) => Some(&e.session_id),
            Self::FileHistoryDelta(e) => Some(&e.message_id),
            Self::Unknown => None,
        }
    }

    /// Get parent UUID for conversation graph traversal
    pub fn parent_uuid(&self) -> Option<&str> {
        match self {
            Self::User(e) => e.metadata.parent_uuid.as_deref(),
            Self::Assistant(e) => e.metadata.parent_uuid.as_deref(),
            Self::Progress(e) => e.metadata.parent_uuid.as_deref(),
            Self::System(e) => e.parent_uuid.as_deref(),
            Self::Attachment(e) => e.metadata.parent_uuid.as_deref(),
            _ => None,
        }
    }

    /// Get event timestamp
    ///
    /// Several harness-internal marker events (`custom-title`, `ai-title`,
    /// `last-prompt`, `bridge-session`, `atis-latch`, `mode`,
    /// `permission-mode`, `agent-name`) carry no timestamp field on disk;
    /// these fall back to the current time, matching the existing `Unknown`
    /// fallback, since ordering by them is never meaningful.
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            Self::User(e) => e.metadata.timestamp,
            Self::Assistant(e) => e.metadata.timestamp,
            Self::Progress(e) => e.metadata.timestamp,
            Self::System(e) => e.timestamp,
            Self::FileSnapshot(e) => e.timestamp,
            Self::QueueOperation(e) => e.timestamp,
            Self::Summary(e) => e.timestamp,
            Self::Attachment(e) => e.metadata.timestamp,
            Self::FileHistoryDelta(e) => e.timestamp,
            Self::CustomTitle(_)
            | Self::AiTitle(_)
            | Self::LastPrompt(_)
            | Self::BridgeSession(_)
            | Self::AtisLatch(_)
            | Self::Mode(_)
            | Self::PermissionMode(_)
            | Self::AgentName(_)
            | Self::Unknown => Utc::now(),
        }
    }

    /// Extract all text content from this event (for FTS indexing)
    pub fn extract_text_content(&self) -> Option<String> {
        match self {
            Self::User(e) => e.extract_text_content(),
            Self::Assistant(e) => e.extract_text_content(),
            Self::Progress(e) => match &e.data {
                ProgressData::AgentProgress(agent) => Some(agent.prompt.clone()),
                ProgressData::BashProgress(bash) => Some(bash.full_output.clone()),
                _ => None,
            },
            Self::System(e) => e.content.clone(),
            _ => None,
        }
    }

    /// Extract file paths mentioned in this event
    pub fn extract_file_paths(&self) -> Vec<String> {
        match self {
            Self::User(e) => e.extract_file_paths(),
            Self::Assistant(e) => e.extract_file_paths(),
            Self::FileSnapshot(e) => e.snapshot.tracked_file_backups.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }

    /// Extract tool names used in this event
    pub fn extract_tool_names(&self) -> Vec<String> {
        match self {
            Self::User(e) => e.extract_tool_names(),
            Self::Assistant(e) => e.extract_tool_names(),
            _ => Vec::new(),
        }
    }
}

/// Origin of a `user` turn.
///
/// Real `kind` values seen on disk: `"human"` (a genuine human-typed
/// prompt), `"task-notification"` (a delegated agent's completion notice
/// routed back as a `user` turn), and `"peer"` (a message from another
/// Claude session, e.g. a cross-session hand-back). Other fields vary by
/// `kind` and are not modeled — only `kind` is needed to exclude
/// harness-generated turns from a restore digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OriginInfo {
    /// Origin kind, e.g. `"human"`, `"task-notification"`, `"peer"`
    pub kind: String,
}

/// User message event
///
/// Represents messages from:
/// - Human users (userType = "external")
/// - Tool results (has tool_use_result field)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserEvent {
    /// Common event metadata
    #[serde(flatten)]
    pub metadata: EventMetadata,

    /// Message content
    pub message: MessageContent,

    /// Permission mode (for file operations)
    #[serde(rename = "permissionMode")]
    pub permission_mode: Option<String>,

    /// Marks injected/synthetic turns: local-command caveats and similar
    /// harness-generated notices rather than something the human actually typed.
    #[serde(rename = "isMeta")]
    pub is_meta: Option<bool>,

    /// Marks a synthesized conversation-compaction summary turn (the text
    /// Claude Code writes back as a `user` turn to replay a compacted
    /// conversation) rather than something the human actually typed.
    #[serde(rename = "isCompactSummary")]
    pub is_compact_summary: Option<bool>,

    /// Where this turn came from. Present on newer transcripts; absent on
    /// older ones and on several harness-injected shapes that predate this
    /// field, so its absence does not by itself mean "human" — only specific
    /// present values (`"task-notification"`, `"peer"`) are used, as an
    /// exclusion signal.
    pub origin: Option<OriginInfo>,

    /// Tool result (if this is a tool result message)
    ///
    /// Deserialized leniently — see
    /// [`crate::events::tool_result::deserialize_tool_use_result_lenient`].
    #[serde(
        rename = "toolUseResult",
        deserialize_with = "super::tool_result::deserialize_tool_use_result_lenient",
        default
    )]
    pub tool_use_result: Option<ToolUseResult>,

    /// Links result back to assistant message that invoked tool
    #[serde(rename = "sourceToolAssistantUUID")]
    pub source_tool_assistant_uuid: Option<String>,
}

impl UserEvent {
    /// Get UUID
    pub fn uuid(&self) -> &str {
        &self.metadata.uuid
    }

    /// Get parent UUID
    pub fn parent_uuid(&self) -> Option<&str> {
        self.metadata.parent_uuid.as_deref()
    }

    /// Get timestamp
    pub fn timestamp(&self) -> DateTime<Utc> {
        self.metadata.timestamp
    }

    /// Extract text content for FTS indexing
    pub fn extract_text_content(&self) -> Option<String> {
        let texts: Vec<String> = self
            .message
            .content
            .iter()
            .filter_map(|block| block.as_text().map(std::string::ToString::to_string))
            .collect();

        if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        }
    }

    /// Extract file paths mentioned
    pub fn extract_file_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();

        // Check tool use result
        if let Some(result) = &self.tool_use_result {
            if let Some(path) = result.file_path() {
                paths.push(path.to_string());
            }
        }

        // Check content blocks
        for block in &self.message.content {
            if let ContentBlock::ToolResult(result) = block {
                if let Some(path) = extract_path_from_json(&result.content) {
                    paths.push(path);
                }
            }
        }

        paths
    }

    /// Extract tool names
    pub fn extract_tool_names(&self) -> Vec<String> {
        self.message
            .content
            .iter()
            .filter_map(|block| {
                if let ContentBlock::ToolResult(result) = block {
                    Some(format!("tool_result:{}", result.tool_use_id))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Classify this turn's content for a restore digest.
    ///
    /// Excludes, as [`UserTurnKind::HarnessNotification`]:
    /// - non-external turns (tool results routed back as `user` events with
    ///   `userType` unset or `"internal"`)
    /// - `isMeta` turns (harness-injected notices)
    /// - `isCompactSummary` turns (the synthesized replay of a compacted
    ///   conversation, written back as a `user` turn — not human-authored)
    /// - turns whose `origin.kind` is `"task-notification"` (a delegated
    ///   agent's completion notice) or `"peer"` (a cross-session message)
    /// - turns whose content is only tool results (no text block at all —
    ///   handled implicitly, since [`ContentBlock::as_text`] only matches
    ///   text blocks)
    /// - local-command echoes (`<local-command-caveat>`,
    ///   `<local-command-stdout>`, `<local-command-stderr>`), stray
    ///   `<task-notification>`/`<system-reminder>` text blocks not caught by
    ///   the structural checks above
    ///
    /// Recognizes, as [`UserTurnKind::SlashCommand`]: a `<command-name>`
    /// invocation — a real user action, not free text, but not a harness
    /// notification either. The caller decides how to render it.
    ///
    /// Everything else is [`UserTurnKind::HumanPrompt`] — including
    /// system-generated notices of a genuine user action, e.g.
    /// `[Request interrupted by user]`, which reflect something the human
    /// actually did even though the string wasn't typed character-by-character.
    #[must_use]
    pub fn classify_turn(&self) -> UserTurnKind<'_> {
        if self.metadata.user_type.as_deref() != Some("external") {
            return UserTurnKind::HarnessNotification;
        }
        if self.is_meta == Some(true) {
            return UserTurnKind::HarnessNotification;
        }
        if self.is_compact_summary == Some(true) {
            return UserTurnKind::HarnessNotification;
        }
        if matches!(
            self.origin.as_ref().map(|origin| origin.kind.as_str()),
            Some("task-notification" | "peer")
        ) {
            return UserTurnKind::HarnessNotification;
        }

        let Some(text) = self.message.content.iter().find_map(ContentBlock::as_text) else {
            return UserTurnKind::HarnessNotification;
        };
        let trimmed = text.trim_start();

        if trimmed.starts_with("<command-name>") {
            if let (Some(name), Some(args)) =
                (extract_tag(trimmed, "command-name"), extract_tag(trimmed, "command-args"))
            {
                return UserTurnKind::SlashCommand { name, args };
            }
        }

        const NOTIFICATION_PREFIXES: [&str; 5] = [
            "<local-command-caveat>",
            "<local-command-stdout>",
            "<local-command-stderr>",
            "<task-notification>",
            "<system-reminder>",
        ];
        if NOTIFICATION_PREFIXES.iter().any(|prefix| trimmed.starts_with(prefix)) {
            return UserTurnKind::HarnessNotification;
        }

        UserTurnKind::HumanPrompt(text)
    }
}

/// Classification of a `user` event's content for a restore digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserTurnKind<'a> {
    /// Genuine human-authored prompt text, or a system-generated notice of a
    /// real user action (e.g. an interruption marker).
    HumanPrompt(&'a str),
    /// A slash-command invocation, e.g. `/model` with argument
    /// `claude-fable-5`, or `/compact` with no arguments. A real user
    /// action, but not free text — the caller decides how to render it
    /// (e.g. `"/model claude-fable-5"`).
    SlashCommand {
        /// Command name, including its leading slash (e.g. `"/model"`)
        name: &'a str,
        /// Raw argument text, possibly empty
        args: &'a str,
    },
    /// A harness-generated notice with no human-authored content: a
    /// non-external/tool-result turn, an `isMeta` or `isCompactSummary`
    /// turn, a task-notification or peer/cross-session message, or a
    /// local-command echo.
    HarnessNotification,
}

/// Extract the text between `<tag>` and `</tag>` in `text`, if both are present.
fn extract_tag<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&close)?;
    Some(&text[start..end])
}

/// Assistant message event
///
/// Represents responses from Claude, including text and tool invocations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantEvent {
    /// Common event metadata
    #[serde(flatten)]
    pub metadata: EventMetadata,

    /// Assistant message with model info and usage
    pub message: AssistantMessage,

    /// Request ID for API correlation
    #[serde(rename = "requestId")]
    pub request_id: Option<String>,
}

impl AssistantEvent {
    /// Get UUID
    pub fn uuid(&self) -> &str {
        &self.metadata.uuid
    }

    /// Get parent UUID
    pub fn parent_uuid(&self) -> Option<&str> {
        self.metadata.parent_uuid.as_deref()
    }

    /// Get timestamp
    pub fn timestamp(&self) -> DateTime<Utc> {
        self.metadata.timestamp
    }

    /// Extract text content for FTS indexing
    pub fn extract_text_content(&self) -> Option<String> {
        let texts: Vec<String> = self
            .message
            .content
            .iter()
            .filter_map(|block| block.as_text().map(std::string::ToString::to_string))
            .collect();

        if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        }
    }

    /// Extract file paths from tool use inputs
    pub fn extract_file_paths(&self) -> Vec<String> {
        self.message
            .content
            .iter()
            .filter_map(|block| {
                if let Some((_, name, input)) = block.as_tool_use() {
                    if matches!(name, "Read" | "Write" | "Edit") {
                        extract_path_from_json(input)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    /// Extract tool names
    pub fn extract_tool_names(&self) -> Vec<String> {
        self.message
            .content
            .iter()
            .filter_map(|block| {
                if let Some((_, name, _)) = block.as_tool_use() {
                    Some(name.to_string())
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Assistant message with model info and usage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// Model name (e.g., "claude-sonnet-4-5-20250929")
    pub model: String,

    /// Message ID (API-level identifier)
    pub id: String,

    /// Message type (always "message")
    #[serde(rename = "type")]
    pub message_type: String,

    /// Role (always "assistant")
    pub role: String,

    /// Message content blocks
    pub content: Vec<ContentBlock>,

    /// Stop reason: "end_turn", "tool_use", "max_tokens"
    #[serde(rename = "stop_reason")]
    pub stop_reason: Option<String>,

    /// Stop sequence (if stopped by sequence)
    #[serde(rename = "stop_sequence")]
    pub stop_sequence: Option<String>,

    /// Token usage statistics
    pub usage: Option<TokenUsage>,

    /// Context management (reserved for future use)
    #[serde(rename = "context_management")]
    pub context_management: Option<JsonValue>,
}

/// Token usage with granular cache tracking
///
/// Captures detailed token usage for cost analysis.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input tokens (new prompt tokens)
    #[serde(default)]
    pub input_tokens: u64,

    /// Output tokens (response tokens)
    #[serde(default)]
    pub output_tokens: u64,

    /// Cache creation tokens (tokens added to cache, 25% more expensive)
    #[serde(default)]
    pub cache_creation_input_tokens: u64,

    /// Cache read tokens (tokens read from cache, 90% discount)
    #[serde(default)]
    pub cache_read_input_tokens: u64,

    /// Cache creation details
    #[serde(default)]
    pub cache_creation: Option<CacheCreation>,

    /// Service tier (for pricing)
    #[serde(default)]
    pub service_tier: Option<String>,
}

/// Cache creation details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheCreation {
    /// Ephemeral 5-minute cache tokens
    #[serde(default)]
    pub ephemeral_5m_input_tokens: u64,

    /// Ephemeral 1-hour cache tokens
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u64,
}

/// File history snapshot
///
/// Tracks file state at message boundaries for undo/redo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHistorySnapshot {
    /// Message ID where snapshot was taken
    #[serde(rename = "messageId")]
    pub message_id: String,

    /// File snapshot data
    pub snapshot: Snapshot,

    /// Is this an update to existing snapshot
    #[serde(rename = "isSnapshotUpdate")]
    pub is_snapshot_update: bool,

    /// Snapshot timestamp
    pub timestamp: DateTime<Utc>,
}

/// Snapshot data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Message ID
    #[serde(rename = "messageId")]
    pub message_id: String,

    /// Map of file path → backup content
    #[serde(rename = "trackedFileBackups")]
    pub tracked_file_backups: HashMap<String, String>,

    /// Snapshot timestamp
    pub timestamp: DateTime<Utc>,
}

/// Queue operation event
///
/// Tracks session queue management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueOperation {
    /// Operation type: "enqueue" or "dequeue"
    pub operation: String,

    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Timestamp
    pub timestamp: DateTime<Utc>,
}

/// Session summary
///
/// Summary of entire session (typically at end).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Summary text
    pub summary: Option<String>,

    /// Session statistics
    pub stats: Option<JsonValue>,

    /// Timestamp
    pub timestamp: DateTime<Utc>,
}

/// Root-level attachment event
///
/// Same payload as the nested attachment field in progress
/// `normalizedMessages` (see [`crate::events::attachment::AttachmentType`]),
/// emitted directly at the root. Real example:
///
/// ```json
/// {
///   "type": "attachment",
///   "uuid": "...", "parentUuid": null, "sessionId": "...",
///   "timestamp": "...", "isSidechain": false, "userType": "external",
///   "cwd": "...", "version": "2.1.258", "gitBranch": "main",
///   "attachment": {"type": "hook_success", "hookName": "SessionStart:startup", ...}
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootAttachmentEvent {
    /// Common event metadata
    #[serde(flatten)]
    pub metadata: EventMetadata,

    /// Attachment payload
    pub attachment: crate::events::attachment::AttachmentType,
}

/// `custom-title` event — the session title Claude Code's own UI shows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomTitleEvent {
    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Title text, verbatim
    #[serde(rename = "customTitle")]
    pub custom_title: String,
}

/// `ai-title` event — a model-generated session title.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiTitleEvent {
    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Title text, verbatim
    #[serde(rename = "aiTitle")]
    pub ai_title: String,
}

/// `last-prompt` event — the latest verbatim human prompt seen so far.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastPromptEvent {
    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Prompt text, verbatim
    #[serde(rename = "lastPrompt")]
    pub last_prompt: String,

    /// UUID of the conversation leaf this prompt was taken from
    #[serde(rename = "leafUuid")]
    pub leaf_uuid: Option<String>,
}

/// `bridge-session` event — cloud sync correlation, not conversational content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeSessionEvent {
    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

/// `atis-latch` event — harness-internal latch state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtisLatchEvent {
    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Latch value (frequently empty)
    pub atis: Option<String>,
}

/// `mode` event — conversation mode marker (e.g. `"normal"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeEvent {
    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Mode value
    pub mode: String,
}

/// `permission-mode` event — permission mode marker (e.g. `"auto"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionModeEvent {
    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Permission mode value
    #[serde(rename = "permissionMode")]
    pub permission_mode: String,
}

/// `agent-name` event — agent/session display-name marker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNameEvent {
    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Display name
    #[serde(rename = "agentName")]
    pub agent_name: String,
}

/// `file-history-delta` event — incremental undo/redo tracking, the
/// incremental counterpart to [`FileHistorySnapshot`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHistoryDeltaEvent {
    /// Message ID this delta is attached to
    #[serde(rename = "messageId")]
    pub message_id: String,

    /// Message ID of the snapshot this delta is relative to
    #[serde(rename = "snapshotMessageId")]
    pub snapshot_message_id: Option<String>,

    /// File path being tracked
    #[serde(rename = "trackingPath")]
    pub tracking_path: String,

    /// Timestamp
    pub timestamp: DateTime<Utc>,
}

/// Helper function to extract file path from JSON value
fn extract_path_from_json(value: &JsonValue) -> Option<String> {
    value
        .get("file_path")
        .or_else(|| value.get("filePath"))
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
}

// Display implementations
impl fmt::Display for SessionEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(e) => write!(
                f,
                "User[{}] at {}",
                e.metadata.uuid,
                e.metadata.timestamp.format("%Y-%m-%d %H:%M:%S")
            ),
            Self::Assistant(e) => write!(
                f,
                "Assistant[{}] {} at {}",
                e.metadata.uuid,
                e.message.model,
                e.metadata.timestamp.format("%Y-%m-%d %H:%M:%S")
            ),
            Self::Progress(e) => write!(
                f,
                "Progress[{}] {:?} at {}",
                e.metadata.uuid,
                e.data,
                e.metadata.timestamp.format("%Y-%m-%d %H:%M:%S")
            ),
            Self::System(e) => {
                if let Some(uuid) = &e.uuid {
                    write!(
                        f,
                        "System[{}] {:?} at {}",
                        uuid,
                        e.subtype,
                        e.timestamp.format("%Y-%m-%d %H:%M:%S")
                    )
                } else {
                    write!(
                        f,
                        "System {:?} at {}",
                        e.subtype,
                        e.timestamp.format("%Y-%m-%d %H:%M:%S")
                    )
                }
            }
            Self::FileSnapshot(e) => {
                write!(
                    f,
                    "FileSnapshot[{}] {} files",
                    e.message_id,
                    e.snapshot.tracked_file_backups.len()
                )
            }
            Self::QueueOperation(e) => write!(
                f,
                "QueueOp[{}] {} at {}",
                e.session_id,
                e.operation,
                e.timestamp.format("%Y-%m-%d %H:%M:%S")
            ),
            Self::Summary(e) => write!(f, "Summary[{}]", e.session_id),
            Self::Attachment(e) => write!(
                f,
                "Attachment[{}] at {}",
                e.metadata.uuid,
                e.metadata.timestamp.format("%Y-%m-%d %H:%M:%S")
            ),
            Self::CustomTitle(e) => write!(f, "CustomTitle[{}] {:?}", e.session_id, e.custom_title),
            Self::AiTitle(e) => write!(f, "AiTitle[{}] {:?}", e.session_id, e.ai_title),
            Self::LastPrompt(e) => write!(f, "LastPrompt[{}] {:?}", e.session_id, e.last_prompt),
            Self::BridgeSession(e) => write!(f, "BridgeSession[{}]", e.session_id),
            Self::AtisLatch(e) => write!(f, "AtisLatch[{}]", e.session_id),
            Self::Mode(e) => write!(f, "Mode[{}] {}", e.session_id, e.mode),
            Self::PermissionMode(e) => {
                write!(f, "PermissionMode[{}] {}", e.session_id, e.permission_mode)
            }
            Self::AgentName(e) => write!(f, "AgentName[{}] {:?}", e.session_id, e.agent_name),
            Self::FileHistoryDelta(e) => {
                write!(f, "FileHistoryDelta[{}] {}", e.message_id, e.tracking_path)
            }
            Self::Unknown => write!(f, "Unknown event"),
        }
    }
}

impl fmt::Display for ProgressData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BashProgress(data) => write!(f, "Bash({}s)", data.elapsed_time_seconds),
            Self::HookProgress(data) => write!(f, "Hook({})", data.hook_name),
            Self::AgentProgress(data) => write!(f, "Agent({})", data.agent_id),
            Self::QueryUpdate(data) => write!(f, "Query({})", data.query),
            Self::SearchResultsReceived(_) => write!(f, "SearchResults"),
            Self::WaitingForTask(data) => write!(f, "WaitingForTask({})", data.task_type),
            Self::Unknown => write!(f, "UnknownProgress"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_user_event() {
        let json = r#"{
            "type": "user",
            "uuid": "user-uuid",
            "parentUuid": null,
            "sessionId": "session-123",
            "timestamp": "2024-01-01T00:00:00Z",
            "isSidechain": false,
            "userType": "external",
            "cwd": "/test",
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "Hello"}
                ]
            }
        }"#;

        let event: SessionEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, SessionEvent::User(_)));
        assert_eq!(event.uuid(), Some("user-uuid"));
    }

    #[test]
    fn test_parse_assistant_event() {
        let json = r#"{
            "type": "assistant",
            "uuid": "assistant-uuid",
            "parentUuid": "user-uuid",
            "sessionId": "session-123",
            "timestamp": "2024-01-01T00:00:00Z",
            "isSidechain": false,
            "cwd": "/test",
            "message": {
                "model": "claude-sonnet-4-5",
                "id": "msg_123",
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Hello!"}
                ],
                "stop_reason": "end_turn"
            }
        }"#;

        let event: SessionEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, SessionEvent::Assistant(_)));
    }

    #[test]
    fn test_parse_progress_event() {
        let json = r#"{
            "type": "progress",
            "uuid": "progress-uuid",
            "parentUuid": null,
            "sessionId": "session-123",
            "timestamp": "2024-01-01T00:00:00Z",
            "isSidechain": false,
            "cwd": "/test",
            "toolUseID": "tool-123",
            "data": {
                "type": "bash_progress",
                "output": "test",
                "fullOutput": "test",
                "elapsedTimeSeconds": 1,
                "totalLines": 1,
                "message": {},
                "normalizedMessages": []
            }
        }"#;

        let event: SessionEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, SessionEvent::Progress(_)));
    }

    #[test]
    fn test_extract_text_content() {
        let json = r#"{
            "type": "user",
            "uuid": "user-uuid",
            "parentUuid": null,
            "sessionId": "session-123",
            "timestamp": "2024-01-01T00:00:00Z",
            "isSidechain": false,
            "userType": "external",
            "cwd": "/test",
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "First line"},
                    {"type": "text", "text": "Second line"}
                ]
            }
        }"#;

        let event: SessionEvent = serde_json::from_str(json).unwrap();
        let text = event.extract_text_content();
        assert_eq!(text, Some("First line\nSecond line".to_string()));
    }

    // ------------------------------------------------------------------
    // Fixtures below are built from real event shapes observed on-disk in
    // Claude Code v2.1.2xx transcripts (content redacted to placeholders).
    // See docs/session-restore/audits/2026-09-23-restorers-live-test.md.
    // ------------------------------------------------------------------

    #[test]
    fn test_parse_user_event_with_bare_string_content() {
        // Real shape: plain single-turn human prompts carry `content` as a
        // bare string, not an array of blocks (D1).
        let json = r#"{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{"role":"user","content":"placeholder prompt text"},"uuid":"user-uuid","timestamp":"2026-09-05T23:53:25.920Z","permissionMode":"auto","userType":"external","entrypoint":"claude-desktop","cwd":"C:\\work","sessionId":"session-123","version":"2.1.258","gitBranch":"main"}"#;

        let event: SessionEvent = serde_json::from_str(json).expect("bare string content must parse");
        let SessionEvent::User(user) = event else {
            panic!("expected user event");
        };
        assert_eq!(user.classify_turn(), UserTurnKind::HumanPrompt("placeholder prompt text"));
    }

    #[test]
    fn test_classify_turn_skips_meta_turns() {
        let json = r#"{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{"role":"user","content":"<local-command-caveat>Caveat: placeholder</local-command-caveat>"},"isMeta":true,"uuid":"user-uuid","timestamp":"2026-09-06T00:14:48.155Z","userType":"external","cwd":"C:\\work","sessionId":"session-123","version":"2.1.258","gitBranch":"main"}"#;

        let SessionEvent::User(user) = serde_json::from_str::<SessionEvent>(json).unwrap() else {
            panic!("expected user event");
        };
        assert_eq!(user.classify_turn(), UserTurnKind::HarnessNotification);
    }

    #[test]
    fn test_classify_turn_recognizes_slash_command_with_args() {
        // Real shape: `/model claude-fable-5`.
        let json = r#"{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{"role":"user","content":"<command-name>/model</command-name>\n            <command-message>model</command-message>\n            <command-args>placeholder-model</command-args>"},"uuid":"user-uuid","timestamp":"2026-08-28T17:02:33.492Z","userType":"external","cwd":"C:\\work","sessionId":"session-123","version":"2.1.246","gitBranch":"main"}"#;

        let SessionEvent::User(user) = serde_json::from_str::<SessionEvent>(json).unwrap() else {
            panic!("expected user event");
        };
        assert_eq!(
            user.classify_turn(),
            UserTurnKind::SlashCommand { name: "/model", args: "placeholder-model" }
        );
    }

    #[test]
    fn test_classify_turn_recognizes_slash_command_without_args() {
        // Real shape: `/compact` with an empty `<command-args>` tag.
        let json = r#"{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{"role":"user","content":"<command-name>/compact</command-name>\n            <command-message>compact</command-message>\n            <command-args></command-args>"},"uuid":"user-uuid","timestamp":"2026-08-28T17:02:33.492Z","userType":"external","cwd":"C:\\work","sessionId":"session-123","version":"2.1.246","gitBranch":"main"}"#;

        let SessionEvent::User(user) = serde_json::from_str::<SessionEvent>(json).unwrap() else {
            panic!("expected user event");
        };
        assert_eq!(user.classify_turn(), UserTurnKind::SlashCommand { name: "/compact", args: "" });
    }

    #[test]
    fn test_classify_turn_skips_local_command_stdout_and_stderr() {
        for content in [
            "<local-command-stdout>Set model to placeholder</local-command-stdout>",
            "<local-command-stderr>placeholder error</local-command-stderr>",
        ] {
            let json = format!(
                r#"{{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{{"role":"user","content":"{content}"}},"uuid":"user-uuid","timestamp":"2026-08-28T17:02:33.492Z","userType":"external","cwd":"C:\\work","sessionId":"session-123","version":"2.1.246","gitBranch":"main"}}"#
            );
            let SessionEvent::User(user) = serde_json::from_str::<SessionEvent>(&json).unwrap() else {
                panic!("expected user event");
            };
            assert_eq!(
                user.classify_turn(),
                UserTurnKind::HarnessNotification,
                "must skip: {content}"
            );
        }
    }

    #[test]
    fn test_classify_turn_skips_task_notification_by_content_and_by_origin() {
        // By content prefix alone (no origin field — defense in depth).
        let json = r#"{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>placeholder</task-id>\n<status>completed</status>\n</task-notification>"},"uuid":"user-uuid","timestamp":"2026-09-22T00:00:00.000Z","userType":"external","cwd":"C:\\work","sessionId":"session-123","version":"2.1.258","gitBranch":"main"}"#;
        let SessionEvent::User(user) = serde_json::from_str::<SessionEvent>(json).unwrap() else {
            panic!("expected user event");
        };
        assert_eq!(user.classify_turn(), UserTurnKind::HarnessNotification);

        // Real shape: `origin: {"kind": "task-notification"}`.
        let json = r#"{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{"role":"user","content":"placeholder result text, not a real prompt"},"uuid":"user-uuid","timestamp":"2026-09-22T00:00:00.000Z","userType":"external","origin":{"kind":"task-notification"},"cwd":"C:\\work","sessionId":"session-123","version":"2.1.258","gitBranch":"main"}"#;
        let SessionEvent::User(user) = serde_json::from_str::<SessionEvent>(json).unwrap() else {
            panic!("expected user event");
        };
        assert_eq!(user.classify_turn(), UserTurnKind::HarnessNotification);
    }

    #[test]
    fn test_classify_turn_skips_peer_cross_session_message() {
        // Real shape: `origin: {"kind": "peer", ...}`, isMeta true.
        let json = r#"{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{"role":"user","content":"Another Claude session sent a message:\n<cross-session-message from=\"placeholder\">placeholder body</cross-session-message>"},"isMeta":true,"origin":{"kind":"peer"},"uuid":"user-uuid","timestamp":"2026-08-28T17:02:33.492Z","userType":"external","cwd":"C:\\work","sessionId":"session-123","version":"2.1.246","gitBranch":"main"}"#;
        let SessionEvent::User(user) = serde_json::from_str::<SessionEvent>(json).unwrap() else {
            panic!("expected user event");
        };
        assert_eq!(user.classify_turn(), UserTurnKind::HarnessNotification);
    }

    #[test]
    fn test_classify_turn_skips_compact_summary() {
        // Real shape: `isCompactSummary: true` — the synthesized replay text
        // Claude Code writes back as a `user` turn after compaction. userType
        // is "external" and isMeta is absent, so only the dedicated field
        // catches this.
        let json = r#"{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{"role":"user","content":"This session is being continued from a previous conversation that ran out of context. Summary: placeholder"},"isCompactSummary":true,"isVisibleInTranscriptOnly":true,"uuid":"user-uuid","timestamp":"2026-09-10T00:00:00.000Z","userType":"external","cwd":"C:\\work","sessionId":"session-123","version":"2.1.258","gitBranch":"main"}"#;
        let SessionEvent::User(user) = serde_json::from_str::<SessionEvent>(json).unwrap() else {
            panic!("expected user event");
        };
        assert_eq!(user.classify_turn(), UserTurnKind::HarnessNotification);
    }

    #[test]
    fn test_classify_turn_keeps_interruption_marker_as_human_prompt() {
        // Real shape: an array-content turn holding only a text block that
        // is a system-generated notice of a genuine user action (hitting
        // Escape), not free text — still counts as human.
        let json = r#"{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"uuid":"user-uuid","timestamp":"2026-08-28T17:02:27.169Z","userType":"external","cwd":"C:\\work","sessionId":"session-123","version":"2.1.246","gitBranch":"main"}"#;
        let SessionEvent::User(user) = serde_json::from_str::<SessionEvent>(json).unwrap() else {
            panic!("expected user event");
        };
        assert_eq!(user.classify_turn(), UserTurnKind::HumanPrompt("[Request interrupted by user]"));
    }

    #[test]
    fn test_classify_turn_skips_tool_result_only_turn() {
        // Real shape: a user turn whose content array holds only a
        // tool_result block (a system-reminder response), no text block.
        let json = r#"{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{"role":"user","content":[{"tool_use_id":"tool-use-id","type":"tool_result","content":"<system-reminder>placeholder warning</system-reminder>"}]},"uuid":"user-uuid","timestamp":"2026-08-30T23:51:17.045Z","toolUseResult":{"type":"text","file":{"filePath":"C:\\work\\out.txt","content":"","numLines":1,"startLine":1,"totalLines":1}},"sourceToolAssistantUUID":"assistant-uuid","userType":"external","cwd":"C:\\work","sessionId":"session-123","version":"2.1.246","gitBranch":"main"}"#;

        let event: SessionEvent =
            serde_json::from_str(json).expect("mismatched toolUseResult shape must not fail the event");
        let SessionEvent::User(user) = event else {
            panic!("expected user event");
        };
        assert_eq!(user.classify_turn(), UserTurnKind::HarnessNotification);
        assert!(user.tool_use_result.is_none());
    }

    #[test]
    fn test_classify_turn_skips_internal_user_type() {
        let json = r#"{"parentUuid":"parent-uuid","isSidechain":false,"promptId":"prompt-id","type":"user","message":{"role":"user","content":"placeholder"},"uuid":"user-uuid","timestamp":"2026-08-28T17:02:33.492Z","userType":"internal","cwd":"C:\\work","sessionId":"session-123","version":"2.1.246","gitBranch":"main"}"#;

        let SessionEvent::User(user) = serde_json::from_str::<SessionEvent>(json).unwrap() else {
            panic!("expected user event");
        };
        assert_eq!(user.classify_turn(), UserTurnKind::HarnessNotification);
    }

    use crate::events::attachment::AttachmentType;

    #[test]
    fn test_parse_root_attachment_event() {
        let json = r#"{"parentUuid":null,"isSidechain":false,"attachment":{"type":"hook_success","hookName":"SessionStart:startup","hookEvent":"SessionStart","output":"placeholder"},"type":"attachment","uuid":"attachment-uuid","timestamp":"2026-09-05T23:53:25.318Z","userType":"external","cwd":"C:\\work","sessionId":"session-123","version":"2.1.258","gitBranch":"main"}"#;

        let event: SessionEvent = serde_json::from_str(json).expect("root attachment event must parse");
        let SessionEvent::Attachment(attachment) = event else {
            panic!("expected attachment event");
        };
        assert_eq!(attachment.metadata.uuid, "attachment-uuid");
        assert!(matches!(attachment.attachment, AttachmentType::HookSuccess(_)));
    }

    #[test]
    fn test_parse_custom_title_last_prompt_ai_title() {
        let custom_title: SessionEvent =
            serde_json::from_str(r#"{"type":"custom-title","customTitle":"placeholder title","sessionId":"session-123"}"#)
                .unwrap();
        assert!(matches!(custom_title, SessionEvent::CustomTitle(ref e) if e.custom_title == "placeholder title"));

        let ai_title: SessionEvent = serde_json::from_str(
            r#"{"type":"ai-title","aiTitle":"placeholder ai title","sessionId":"session-123"}"#,
        )
        .unwrap();
        assert!(matches!(ai_title, SessionEvent::AiTitle(ref e) if e.ai_title == "placeholder ai title"));

        let last_prompt: SessionEvent = serde_json::from_str(
            r#"{"type":"last-prompt","lastPrompt":"placeholder last prompt","leafUuid":"leaf-uuid","sessionId":"session-123"}"#,
        )
        .unwrap();
        assert!(
            matches!(last_prompt, SessionEvent::LastPrompt(ref e) if e.last_prompt == "placeholder last prompt")
        );
    }

    #[test]
    fn test_parse_harness_internal_markers_never_fail() {
        // bridge-session, atis-latch, mode, permission-mode, agent-name,
        // file-history-delta: none carry conversational content, but a new
        // root type must never fail the whole line.
        let lines = [
            r#"{"type":"bridge-session","sessionId":"session-123","bridgeSessionId":"cse_placeholder","lastSequenceNum":0}"#,
            r#"{"type":"atis-latch","atis":"","sessionId":"session-123"}"#,
            r#"{"type":"mode","mode":"normal","sessionId":"session-123"}"#,
            r#"{"type":"permission-mode","permissionMode":"auto","sessionId":"session-123"}"#,
            r#"{"type":"agent-name","agentName":"placeholder agent","sessionId":"session-123"}"#,
            r#"{"type":"file-history-delta","messageId":"message-id","snapshotMessageId":"snapshot-id","trackingPath":"src/lib.rs","backup":{"backupFileName":"placeholder","version":1},"timestamp":"2026-09-16T16:23:51.749Z"}"#,
        ];
        for line in lines {
            let event: SessionEvent =
                serde_json::from_str(line).unwrap_or_else(|error| panic!("must parse {line}: {error}"));
            assert!(
                !matches!(event, SessionEvent::Unknown),
                "must not fall back to Unknown: {line}"
            );
        }
    }

    #[test]
    fn test_unrecognized_root_type_falls_back_to_unknown_without_failing() {
        let json = r#"{"type":"some-future-event-type","sessionId":"session-123","payload":{"nested":true}}"#;
        let event: SessionEvent = serde_json::from_str(json).expect("unknown root type must not fail parsing");
        assert!(matches!(event, SessionEvent::Unknown));
    }
}
