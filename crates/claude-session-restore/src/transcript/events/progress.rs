//! Progress event types (Level 3)
//!
//! Progress events are the MOST FREQUENT event type in Claude Code sessions
//! (~82k occurrences per large session). They contain real-time updates during
//! tool execution and include the full conversation context in normalized messages.
//!
//! # Progress Data Types
//!
//! ```text
//! Progress Data (.data.type when root .type == 'progress')
//! ├── bash_progress (49,591)            - Bash command execution
//! ├── hook_progress (31,668)            - Hook execution
//! ├── agent_progress (1,371)            - Agent spawning/execution
//! ├── query_update (1,183)              - Web search queries
//! ├── search_results_received (1,181)   - Web search results
//! └── ... (more types)
//! ```
//!
//! # Special Feature: Normalized Messages
//!
//! Progress events contain `.data.normalizedMessages[]` which is a COMPLETE
//! CONVERSATION REPLAY. This explains why progress events dominate file size!
//!
//! Each progress event embeds:
//! - The current operation (bash, agent, hook)
//! - The triggering message (`.data.message`)
//! - **Entire conversation history** (`.data.normalizedMessages[]`)
//!
//! This creates a recursive structure where progress events contain OTHER events!

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use super::metadata::EventMetadata;

/// Progress event wrapper
///
/// Contains real-time progress updates during tool execution.
///
/// # Links
///
/// - `tool_use_id`: Links to `ToolUseBlock.id` that triggered this progress
/// - `parent_tool_use_id`: For nested tool invocations (agents spawning agents)
/// - `metadata.parent_uuid`: Links to parent message in conversation
///
/// # Frequency
///
/// ~82,200 occurrences per large session (MOST FREQUENT event type!)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEvent {
    /// Common event metadata
    #[serde(flatten)]
    pub metadata: EventMetadata,

    /// Tool use ID that triggered this progress
    ///
    /// Links to `ToolUseBlock.id` in assistant message
    #[serde(rename = "toolUseID")]
    pub tool_use_id: Option<String>,

    /// Parent tool use ID (for nested tools)
    ///
    /// Used when agents spawn sub-agents or tools invoke other tools
    #[serde(rename = "parentToolUseID")]
    pub parent_tool_use_id: Option<String>,

    /// Progress data (bash, agent, hook, etc.)
    pub data: ProgressData,
}

impl ProgressEvent {
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
}

/// Progress data discriminator
///
/// All possible progress data types.
///
/// # Special Note: Normalized Messages
///
/// Most progress data variants contain `.normalizedMessages[]` which is a
/// COMPLETE conversation history replay. This makes progress events HUGE.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgressData {
    /// Bash command execution progress
    ///
    /// Real-time updates during bash command execution.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "bash_progress",
    ///   "output": "Compiling...",
    ///   "fullOutput": "Building...\nCompiling...",
    ///   "elapsedTimeSeconds": 5,
    ///   "totalLines": 2,
    ///   "message": {...},
    ///   "normalizedMessages": [...]
    /// }
    /// ```
    ///
    /// # Frequency
    ///
    /// ~49,591 occurrences per large session (most common progress type)
    BashProgress(BashProgressData),

    /// Hook execution progress
    ///
    /// Updates during pre/post hook execution.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "hook_progress",
    ///   "hookEvent": "pre-tool-use",
    ///   "hookName": "pre-commit",
    ///   "command": "./hooks/pre-commit.sh",
    ///   "message": {...},
    ///   "normalizedMessages": [...]
    /// }
    /// ```
    ///
    /// # Frequency
    ///
    /// ~31,668 occurrences per large session
    HookProgress(HookProgressData),

    /// Agent spawning and execution
    ///
    /// Updates when delegating to agents.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "agent_progress",
    ///   "agentId": "abc123",
    ///   "prompt": "Implement feature X",
    ///   "message": {...},
    ///   "normalizedMessages": [...]
    /// }
    /// ```
    ///
    /// # Frequency
    ///
    /// ~1,371 occurrences per large session
    AgentProgress(AgentProgressData),

    /// Web search query update
    ///
    /// Shows query being sent to search engine.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "query_update",
    ///   "query": "rust async tokio best practices"
    /// }
    /// ```
    ///
    /// # Frequency
    ///
    /// ~1,183 occurrences per large session (when web search enabled)
    QueryUpdate(QueryUpdateData),

    /// Web search results received
    ///
    /// Search results from web search tool.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "search_results_received",
    ///   "results": [
    ///     {"title": "...", "url": "...", "snippet": "..."}
    ///   ]
    /// }
    /// ```
    ///
    /// # Frequency
    ///
    /// ~1,181 occurrences per large session
    SearchResultsReceived(SearchResultsData),

    /// Waiting for task completion
    ///
    /// Shows that execution is waiting for a task to complete.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "waiting_for_task",
    ///   "taskDescription": "Write Bitstamp tests Phase 3",
    ///   "taskType": "local_agent"
    /// }
    /// ```
    ///
    /// # Frequency
    ///
    /// ~8 occurrences per large session (rare, used with background tasks)
    WaitingForTask(WaitingForTaskData),

    /// Unknown progress type (forward compatibility)
    #[serde(other)]
    Unknown,
}

impl ProgressData {
    /// Extract agent ID if this is agent progress
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::AgentProgress(data) => Some(&data.agent_id),
            _ => None,
        }
    }

    /// Extract agent prompt if this is agent progress
    pub fn agent_prompt(&self) -> Option<&str> {
        match self {
            Self::AgentProgress(data) => Some(&data.prompt),
            _ => None,
        }
    }

    /// Get normalized messages if present
    pub fn normalized_messages(&self) -> Option<&[NormalizedMessage]> {
        match self {
            Self::BashProgress(data) => Some(&data.normalized_messages),
            Self::HookProgress(data) => Some(&data.normalized_messages),
            Self::AgentProgress(data) => Some(&data.normalized_messages),
            _ => None,
        }
    }

    /// Get bash progress data if this is bash progress
    pub fn as_bash_progress(&self) -> Option<&BashProgressData> {
        match self {
            Self::BashProgress(data) => Some(data),
            _ => None,
        }
    }

    /// Get agent progress data if this is agent progress
    pub fn as_agent_progress(&self) -> Option<&AgentProgressData> {
        match self {
            Self::AgentProgress(data) => Some(data),
            _ => None,
        }
    }

    /// Get hook progress data if this is hook progress
    pub fn as_hook_progress(&self) -> Option<&HookProgressData> {
        match self {
            Self::HookProgress(data) => Some(data),
            _ => None,
        }
    }
}

/// Bash command execution progress
///
/// Most common progress type (~49k per session).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashProgressData {
    /// Incremental output (latest chunk)
    pub output: String,

    /// Full accumulated output (all chunks)
    #[serde(rename = "fullOutput")]
    pub full_output: String,

    /// Elapsed time (seconds)
    #[serde(rename = "elapsedTimeSeconds")]
    pub elapsed_time_seconds: u64,

    /// Total output lines
    #[serde(rename = "totalLines")]
    pub total_lines: u64,

    /// Triggering message
    pub message: JsonValue,

    /// Complete conversation history (HUGE!)
    #[serde(rename = "normalizedMessages")]
    pub normalized_messages: Vec<NormalizedMessage>,
}

/// Hook execution progress
///
/// ~31,668 occurrences per large session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookProgressData {
    /// Hook event type: "pre-tool-use", "post-tool-use"
    #[serde(rename = "hookEvent")]
    pub hook_event: String,

    /// Hook name (e.g., "pre-commit")
    #[serde(rename = "hookName")]
    pub hook_name: String,

    /// Hook command being executed
    pub command: String,

    /// Triggering message
    pub message: JsonValue,

    /// Complete conversation history
    #[serde(rename = "normalizedMessages")]
    pub normalized_messages: Vec<NormalizedMessage>,
}

/// Agent spawning and execution
///
/// ~1,371 occurrences per large session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProgressData {
    /// Agent ID (unique identifier)
    #[serde(rename = "agentId")]
    pub agent_id: String,

    /// Agent task/prompt
    pub prompt: String,

    /// Triggering message
    pub message: JsonValue,

    /// Complete conversation history
    #[serde(rename = "normalizedMessages")]
    pub normalized_messages: Vec<NormalizedMessage>,
}

/// Web search query update
///
/// ~1,183 occurrences per large session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryUpdateData {
    /// Search query string
    pub query: String,
}

/// Web search results received
///
/// ~1,181 occurrences per large session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResultsData {
    /// Search results (unstructured JSON)
    #[serde(flatten)]
    pub results: JsonValue,
}

/// Waiting for task completion
///
/// ~8 occurrences per large session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitingForTaskData {
    /// Task description
    #[serde(rename = "taskDescription")]
    pub task_description: String,

    /// Task type (e.g., "local_agent")
    #[serde(rename = "taskType")]
    pub task_type: String,
}

/// Normalized message in progress events
///
/// Progress events contain `.data.normalizedMessages[]` which is a COMPLETE
/// conversation replay. This is a recursive structure that can contain ANY
/// event type including more progress events!
///
/// # Why This Exists
///
/// Progress events need full conversation context to:
/// - Resume interrupted operations
/// - Handle retries with context
/// - Display conversation state during long operations
///
/// # Structure
///
/// ```text
/// NormalizedMessage (enum)
/// ├── User          - User turns
/// ├── Assistant     - Assistant turns
/// ├── Progress      - Nested progress events
/// ├── Attachment    - Attachments (hooks, todos, etc.)
/// └── System        - System messages
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum NormalizedMessage {
    /// User message in conversation replay
    #[serde(rename = "user")]
    User(NormalizedUserMessage),

    /// Assistant message in conversation replay
    #[serde(rename = "assistant")]
    Assistant(NormalizedAssistantMessage),

    /// Nested progress event (yes, progress events can contain progress events!)
    #[serde(rename = "progress")]
    Progress(JsonValue),

    /// Attachment (hook, todo, etc.)
    #[serde(rename = "attachment")]
    Attachment(NormalizedAttachment),

    /// System message
    #[serde(rename = "system")]
    System(JsonValue),

    /// Unknown normalized message type
    #[serde(other)]
    Unknown,
}

/// User message in normalized messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedUserMessage {
    /// Message UUID
    pub uuid: String,

    /// Parent UUID
    #[serde(rename = "parentUuid")]
    pub parent_uuid: Option<String>,

    /// Timestamp
    pub timestamp: DateTime<Utc>,

    /// Message content
    pub message: super::message::MessageContent,

    /// User type: "external" or "internal"
    #[serde(rename = "userType")]
    pub user_type: Option<String>,
}

/// Assistant message in normalized messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedAssistantMessage {
    /// Message UUID
    pub uuid: String,

    /// Parent UUID
    #[serde(rename = "parentUuid")]
    pub parent_uuid: Option<String>,

    /// Timestamp
    pub timestamp: DateTime<Utc>,

    /// Message content
    pub message: JsonValue,

    /// Model name
    pub model: Option<String>,
}

/// Attachment in normalized messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedAttachment {
    /// Attachment type and data
    #[serde(flatten)]
    pub attachment_type: super::attachment::AttachmentType,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bash_progress() {
        let json = r#"{
            "type": "bash_progress",
            "output": "Compiling...",
            "fullOutput": "Building...\nCompiling...",
            "elapsedTimeSeconds": 5,
            "totalLines": 2,
            "message": {},
            "normalizedMessages": []
        }"#;

        let data: ProgressData = serde_json::from_str(json).unwrap();
        assert!(matches!(data, ProgressData::BashProgress(_)));

        if let ProgressData::BashProgress(bash) = data {
            assert_eq!(bash.output, "Compiling...");
            assert_eq!(bash.full_output, "Building...\nCompiling...");
            assert_eq!(bash.elapsed_time_seconds, 5);
            assert_eq!(bash.total_lines, 2);
        }
    }

    #[test]
    fn test_parse_agent_progress() {
        let json = r#"{
            "type": "agent_progress",
            "agentId": "abc123",
            "prompt": "Implement feature X",
            "message": {},
            "normalizedMessages": []
        }"#;

        let data: ProgressData = serde_json::from_str(json).unwrap();
        assert!(matches!(data, ProgressData::AgentProgress(_)));

        assert_eq!(data.agent_id(), Some("abc123"));
        assert_eq!(data.agent_prompt(), Some("Implement feature X"));
    }

    #[test]
    fn test_parse_hook_progress() {
        let json = r#"{
            "type": "hook_progress",
            "hookEvent": "pre-tool-use",
            "hookName": "pre-commit",
            "command": "./hooks/pre-commit.sh",
            "message": {},
            "normalizedMessages": []
        }"#;

        let data: ProgressData = serde_json::from_str(json).unwrap();
        assert!(matches!(data, ProgressData::HookProgress(_)));

        if let ProgressData::HookProgress(hook) = data {
            assert_eq!(hook.hook_event, "pre-tool-use");
            assert_eq!(hook.hook_name, "pre-commit");
            assert_eq!(hook.command, "./hooks/pre-commit.sh");
        }
    }

    #[test]
    fn test_parse_query_update() {
        let json = r#"{
            "type": "query_update",
            "query": "rust async best practices"
        }"#;

        let data: ProgressData = serde_json::from_str(json).unwrap();
        assert!(matches!(data, ProgressData::QueryUpdate(_)));

        if let ProgressData::QueryUpdate(query) = data {
            assert_eq!(query.query, "rust async best practices");
        }
    }

    #[test]
    fn test_parse_waiting_for_task() {
        let json = r#"{
            "type": "waiting_for_task",
            "taskDescription": "Write Bitstamp tests Phase 3",
            "taskType": "local_agent"
        }"#;

        let data: ProgressData = serde_json::from_str(json).unwrap();
        assert!(matches!(data, ProgressData::WaitingForTask(_)));

        if let ProgressData::WaitingForTask(task) = data {
            assert_eq!(task.task_description, "Write Bitstamp tests Phase 3");
            assert_eq!(task.task_type, "local_agent");
        }
    }

    #[test]
    fn test_normalized_messages_access() {
        let json = r#"{
            "type": "bash_progress",
            "output": "test",
            "fullOutput": "test",
            "elapsedTimeSeconds": 1,
            "totalLines": 1,
            "message": {},
            "normalizedMessages": []
        }"#;

        let data: ProgressData = serde_json::from_str(json).unwrap();
        assert!(data.normalized_messages().is_some());
        assert_eq!(data.normalized_messages().unwrap().len(), 0);
    }
}
