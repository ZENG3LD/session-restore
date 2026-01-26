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
//! use zengeld_memory_core::sources::claude::SessionEvent;
//!
//! let line = r#"{"type": "user", "uuid": "...", ...}"#;
//! let event: SessionEvent = serde_json::from_str(line)?;
//!
//! match event {
//!     SessionEvent::User(user) => { /* handle user message */ }
//!     SessionEvent::Assistant(assistant) => { /* handle assistant */ }
//!     SessionEvent::Progress(progress) => { /* handle progress */ }
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

    /// Unknown event type (forward compatibility)
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
            _ => None,
        }
    }

    /// Get event timestamp
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            Self::User(e) => e.metadata.timestamp,
            Self::Assistant(e) => e.metadata.timestamp,
            Self::Progress(e) => e.metadata.timestamp,
            Self::System(e) => e.timestamp,
            Self::FileSnapshot(e) => e.timestamp,
            Self::QueueOperation(e) => e.timestamp,
            Self::Summary(e) => e.timestamp,
            Self::Unknown => Utc::now(),
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

    /// Tool result (if this is a tool result message)
    #[serde(rename = "toolUseResult")]
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
}
