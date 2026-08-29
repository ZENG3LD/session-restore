//! System event types (Level 6)
//!
//! System events are rare but important for understanding session structure.
//! They mark boundaries (compaction), errors, and system-level state changes.
//!
//! # System Event Subtypes
//!
//! ```text
//! System Events (.subtype when .type == 'system')
//! ├── compact_boundary       - Conversation compaction point (~150k tokens)
//! ├── microcompact_boundary  - Smaller compaction point
//! ├── error                  - API errors, overload errors
//! └── ... (more types)
//! ```
//!
//! # Compact Boundaries
//!
//! Compact boundaries are CRITICAL for session segmentation. They mark points
//! where Claude Code compacted conversation history to stay within token limits.
//!
//! These are ideal break points for:
//! - Creating session summaries
//! - Segmenting long sessions
//! - Understanding conversation flow

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use super::metadata::EventMetadata;

/// System event
///
/// Represents system-level events:
/// - Compact boundaries (conversation compaction)
/// - API errors (overload, rate limits)
/// - System reminders
///
/// # Special Fields
///
/// System events don't use flattened EventMetadata because:
/// - `uuid` can be None for some system events
/// - Need special handling for compact boundaries
///
/// # Frequency
///
/// ~6 occurrences per large session (rare but important!)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemEvent {
    /// Event UUID (can be None for some system events)
    pub uuid: Option<String>,

    /// Parent UUID
    #[serde(rename = "parentUuid")]
    pub parent_uuid: Option<String>,

    /// Logical parent UUID (for compact boundaries)
    ///
    /// Compact boundaries have both:
    /// - `parent_uuid` - points to compact boundary system message
    /// - `logical_parent_uuid` - points to last real message before compaction
    ///
    /// This preserves conversation flow across compaction.
    #[serde(rename = "logicalParentUuid")]
    pub logical_parent_uuid: Option<String>,

    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Timestamp
    pub timestamp: DateTime<Utc>,

    /// Is this part of a sidechain
    #[serde(rename = "isSidechain")]
    pub is_sidechain: bool,

    /// User type (typically None for system events)
    #[serde(rename = "userType")]
    pub user_type: Option<String>,

    /// Current working directory
    pub cwd: Option<String>,

    /// Claude Code version
    pub version: Option<String>,

    /// Git branch
    #[serde(rename = "gitBranch")]
    pub git_branch: Option<String>,

    /// Agent slug
    pub slug: Option<String>,

    /// System event subtype
    ///
    /// Common values:
    /// - `"compact_boundary"` - Conversation compaction point
    /// - `"microcompact_boundary"` - Smaller compaction
    /// - `"error"` - System error
    pub subtype: Option<String>,

    /// Content/message
    pub content: Option<String>,

    /// Is this a meta event
    #[serde(rename = "isMeta")]
    pub is_meta: Option<bool>,

    /// Severity level: "info", "warning", "error"
    pub level: Option<String>,

    /// Compact boundary metadata (only for compact_boundary subtype)
    #[serde(rename = "compactMetadata")]
    pub compact_metadata: Option<CompactMetadata>,

    /// Error details (only for error subtype)
    pub error: Option<ErrorDetails>,
}

impl SystemEvent {
    /// Check if this is a compact boundary
    pub fn is_compact_boundary(&self) -> bool {
        self.subtype.as_deref() == Some("compact_boundary")
    }

    /// Check if this is a microcompact boundary
    pub fn is_microcompact_boundary(&self) -> bool {
        self.subtype.as_deref() == Some("microcompact_boundary")
    }

    /// Check if this is an error event
    pub fn is_error(&self) -> bool {
        self.subtype.as_deref() == Some("error")
    }

    /// Get compact metadata if this is a compact boundary
    pub fn compact_metadata(&self) -> Option<&CompactMetadata> {
        self.compact_metadata.as_ref()
    }

    /// Convert to EventMetadata (for consistent interface)
    pub fn metadata(&self) -> EventMetadata {
        EventMetadata {
            uuid: self.uuid.clone().unwrap_or_default(),
            parent_uuid: self.parent_uuid.clone(),
            session_id: self.session_id.clone(),
            timestamp: self.timestamp,
            is_sidechain: self.is_sidechain,
            user_type: self.user_type.clone(),
            cwd: self.cwd.clone(),
            version: self.version.clone(),
            git_branch: self.git_branch.clone(),
            slug: self.slug.clone(),
        }
    }
}

/// Compact boundary metadata
///
/// Marks natural conversation break points where context was compacted.
///
/// # When Compaction Occurs
///
/// Claude Code automatically compacts conversation when:
/// - Token count reaches ~150k-160k tokens
/// - User manually triggers compaction
///
/// # Usage for Segmentation
///
/// Compact boundaries are IDEAL for:
/// - Breaking long sessions into segments
/// - Creating segment summaries
/// - Understanding conversation phases
///
/// # Example
///
/// ```json
/// {
///   "trigger": "auto",
///   "preTokens": 156594
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactMetadata {
    /// Trigger type: "auto" or "manual"
    ///
    /// - `"auto"` - Automatic compaction at ~150k tokens
    /// - `"manual"` - User-triggered compaction
    pub trigger: String,

    /// Token count before compaction
    ///
    /// Typically ~150k-160k for auto-compaction
    #[serde(rename = "preTokens")]
    pub pre_tokens: u64,

    /// Token count after compaction (optional)
    #[serde(rename = "postTokens", skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub post_tokens: Option<u64>,
}

/// Error details for error system events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorDetails {
    /// Error type
    ///
    /// Common values:
    /// - `"error"` - Generic error
    /// - `"overloaded_error"` - API overload
    /// - `"rate_limit_error"` - Rate limit exceeded
    #[serde(rename = "type")]
    pub error_type: String,

    /// Error message
    pub message: String,

    /// Additional error context
    #[serde(flatten)]
    pub extra: JsonValue,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_compact_boundary() {
        let json = r#"{
            "type": "system",
            "subtype": "compact_boundary",
            "uuid": "boundary-uuid",
            "parentUuid": null,
            "logicalParentUuid": "last-message-uuid",
            "sessionId": "session-123",
            "timestamp": "2024-01-01T00:00:00Z",
            "isSidechain": false,
            "cwd": "/test",
            "version": "2.1.19",
            "gitBranch": "main",
            "content": "Conversation compacted",
            "level": "info",
            "compactMetadata": {
                "trigger": "auto",
                "preTokens": 156594,
                "postTokens": 50000
            }
        }"#;

        let event: SystemEvent = serde_json::from_str(json).unwrap();
        assert!(event.is_compact_boundary());
        assert!(!event.is_microcompact_boundary());
        assert!(!event.is_error());

        let metadata = event.compact_metadata().unwrap();
        assert_eq!(metadata.trigger, "auto");
        assert_eq!(metadata.pre_tokens, 156_594);
        assert_eq!(metadata.post_tokens, Some(50_000));

        assert_eq!(
            event.logical_parent_uuid,
            Some("last-message-uuid".to_string())
        );
    }

    #[test]
    fn test_parse_microcompact_boundary() {
        let json = r#"{
            "type": "system",
            "subtype": "microcompact_boundary",
            "uuid": "micro-uuid",
            "parentUuid": null,
            "sessionId": "session-123",
            "timestamp": "2024-01-01T00:00:00Z",
            "isSidechain": false,
            "cwd": "/test"
        }"#;

        let event: SystemEvent = serde_json::from_str(json).unwrap();
        assert!(event.is_microcompact_boundary());
        assert!(!event.is_compact_boundary());
    }

    #[test]
    fn test_parse_error_event() {
        let json = r#"{
            "type": "system",
            "subtype": "error",
            "uuid": "error-uuid",
            "parentUuid": null,
            "sessionId": "session-123",
            "timestamp": "2024-01-01T00:00:00Z",
            "isSidechain": false,
            "cwd": "/test",
            "level": "error",
            "content": "API overloaded",
            "error": {
                "type": "overloaded_error",
                "message": "API is currently overloaded, please try again"
            }
        }"#;

        let event: SystemEvent = serde_json::from_str(json).unwrap();
        assert!(event.is_error());
        assert_eq!(event.level, Some("error".to_string()));
        assert_eq!(event.content, Some("API overloaded".to_string()));

        if let Some(error) = &event.error {
            assert_eq!(error.error_type, "overloaded_error");
            assert_eq!(
                error.message,
                "API is currently overloaded, please try again"
            );
        }
    }

    #[test]
    fn test_system_event_metadata() {
        let event = SystemEvent {
            uuid: Some("test-uuid".to_string()),
            parent_uuid: Some("parent-uuid".to_string()),
            logical_parent_uuid: None,
            session_id: "session-123".to_string(),
            timestamp: Utc::now(),
            is_sidechain: false,
            user_type: None,
            cwd: Some("/test".to_string()),
            version: Some("2.1.19".to_string()),
            git_branch: Some("main".to_string()),
            slug: None,
            subtype: Some("compact_boundary".to_string()),
            content: None,
            is_meta: None,
            level: None,
            compact_metadata: None,
            error: None,
        };

        let metadata = event.metadata();
        assert_eq!(metadata.uuid, "test-uuid");
        assert_eq!(metadata.parent_uuid, Some("parent-uuid".to_string()));
        assert_eq!(metadata.session_id, "session-123");
    }

    #[test]
    fn test_system_event_without_uuid() {
        let json = r#"{
            "type": "system",
            "subtype": "info",
            "parentUuid": null,
            "sessionId": "session-123",
            "timestamp": "2024-01-01T00:00:00Z",
            "isSidechain": false,
            "cwd": "/test",
            "content": "System notification"
        }"#;

        let event: SystemEvent = serde_json::from_str(json).unwrap();
        assert!(event.uuid.is_none());

        // metadata() should handle None uuid gracefully
        let metadata = event.metadata();
        assert_eq!(metadata.uuid, "");
    }
}
