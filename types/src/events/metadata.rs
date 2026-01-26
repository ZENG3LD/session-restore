//! Common metadata structures shared across event types
//!
//! All events in Claude Code sessions share common metadata fields that provide
//! execution context, session identity, and graph traversal capabilities.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Common metadata present in most events
///
/// This structure captures the execution context for an event:
/// - **Session identity**: `session_id` links events to sessions
/// - **Graph structure**: `parent_uuid` forms conversation tree
/// - **Execution context**: `cwd`, `git_branch` track environment
/// - **Timing**: `timestamp` for chronological ordering
///
/// # Links and Relationships
///
/// - `uuid`: Unique identifier for this event
/// - `parent_uuid`: Links to previous message (conversation chain)
/// - `session_id`: Groups events into sessions
/// - `logical_parent_uuid`: Used for compact boundaries to preserve logical flow
///
/// # Frequency in Sessions
///
/// Present in ~95% of events (all except some system events)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMetadata {
    /// Unique identifier for this event
    ///
    /// UUIDs are used to build conversation graphs and link related events.
    pub uuid: String,

    /// Links to parent message in conversation chain
    ///
    /// Forms a tree structure where:
    /// - `None` = root message (session start)
    /// - `Some(uuid)` = response to/result of another event
    #[serde(rename = "parentUuid")]
    pub parent_uuid: Option<String>,

    /// Session identifier (groups related events)
    ///
    /// All events in a session file share the same `session_id`.
    /// Format: `{uuid}` (e.g., "7b1f4d79-4ab1-4d12-913d-e367cb3a5387")
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Event timestamp (UTC)
    ///
    /// Used for chronological ordering and session duration calculation.
    pub timestamp: DateTime<Utc>,

    /// Whether this event is part of a sidechain (branching conversation)
    ///
    /// Sidechains occur when users:
    /// - Undo/redo operations
    /// - Branch from earlier conversation points
    #[serde(rename = "isSidechain")]
    pub is_sidechain: bool,

    /// User type discriminator
    ///
    /// - `"external"` = Human user input
    /// - `"internal"` = Tool result or system message
    /// - `None` = Not applicable (system events)
    #[serde(rename = "userType")]
    pub user_type: Option<String>,

    /// Current working directory when event occurred
    ///
    /// Examples:
    /// - Windows: `"C:\\Users\\user\\project"`
    /// - Unix: `"/home/user/project"`
    pub cwd: Option<String>,

    /// Claude Code version
    ///
    /// Format: `"2.1.19"` or similar semver
    pub version: Option<String>,

    /// Git branch when event occurred
    ///
    /// Useful for correlating sessions with code branches.
    /// Example: `"main"`, `"feature-xyz"`
    #[serde(rename = "gitBranch")]
    pub git_branch: Option<String>,

    /// Agent slug for delegated operations
    ///
    /// Present in progress events when using agents.
    /// Examples: `"rust-implementer"`, `"research-agent"`
    pub slug: Option<String>,
}

/// Logical parent UUID (for compact boundaries)
///
/// Compact boundaries have both:
/// - `parent_uuid` - points to the compact boundary system message
/// - `logical_parent_uuid` - points to the last real message before compaction
///
/// This preserves conversation flow while marking compaction points.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogicalParentMetadata {
    /// Logical parent UUID (preserves conversation flow across compaction)
    #[serde(rename = "logicalParentUuid")]
    pub logical_parent_uuid: Option<String>,
}

/// Tool use linking metadata
///
/// Present in progress events to link progress updates back to the
/// tool invocation that triggered them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseMetadata {
    /// Tool use ID that triggered this progress
    ///
    /// Links back to `ContentBlock::ToolUse.id` in assistant message
    #[serde(rename = "toolUseID")]
    pub tool_use_id: Option<String>,

    /// Parent tool use ID (for nested tool invocations)
    ///
    /// Used when agents spawn sub-agents or tools invoke other tools
    #[serde(rename = "parentToolUseID")]
    pub parent_tool_use_id: Option<String>,
}

/// Source tool assistant linking
///
/// Present in user messages that are tool results, linking back to the
/// assistant message that invoked the tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceToolMetadata {
    /// UUID of assistant message that invoked this tool
    ///
    /// Links `user` (tool result) → `assistant` (tool invocation)
    #[serde(rename = "sourceToolAssistantUUID")]
    pub source_tool_assistant_uuid: Option<String>,
}

impl EventMetadata {
    /// Check if this is a human user prompt (vs tool result)
    pub fn is_user_prompt(&self) -> bool {
        self.user_type.as_deref() == Some("external")
    }

    /// Check if this is an internal message (tool result)
    pub fn is_internal(&self) -> bool {
        self.user_type.as_deref() == Some("internal")
    }

    /// Check if this is part of a sidechain
    pub fn is_sidechain(&self) -> bool {
        self.is_sidechain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_user_type_checks() {
        let metadata = EventMetadata {
            uuid: "test-uuid".to_string(),
            parent_uuid: None,
            session_id: "session-123".to_string(),
            timestamp: Utc::now(),
            is_sidechain: false,
            user_type: Some("external".to_string()),
            cwd: Some("/test".to_string()),
            version: Some("2.1.19".to_string()),
            git_branch: Some("main".to_string()),
            slug: None,
        };

        assert!(metadata.is_user_prompt());
        assert!(!metadata.is_internal());
        assert!(!metadata.is_sidechain());
    }

    #[test]
    fn test_metadata_internal_type() {
        let metadata = EventMetadata {
            uuid: "test-uuid".to_string(),
            parent_uuid: Some("parent-uuid".to_string()),
            session_id: "session-123".to_string(),
            timestamp: Utc::now(),
            is_sidechain: false,
            user_type: Some("internal".to_string()),
            cwd: Some("/test".to_string()),
            version: Some("2.1.19".to_string()),
            git_branch: Some("main".to_string()),
            slug: None,
        };

        assert!(!metadata.is_user_prompt());
        assert!(metadata.is_internal());
    }

    #[test]
    fn test_metadata_sidechain() {
        let metadata = EventMetadata {
            uuid: "test-uuid".to_string(),
            parent_uuid: Some("parent-uuid".to_string()),
            session_id: "session-123".to_string(),
            timestamp: Utc::now(),
            is_sidechain: true,
            user_type: Some("external".to_string()),
            cwd: Some("/test".to_string()),
            version: Some("2.1.19".to_string()),
            git_branch: Some("main".to_string()),
            slug: None,
        };

        assert!(metadata.is_sidechain());
    }
}
