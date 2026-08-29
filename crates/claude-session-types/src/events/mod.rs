//! Complete event type hierarchy for Claude Code sessions
//!
//! This module provides a comprehensive type map for ALL event types found in
//! Claude Code JSONL session files, based on detailed analysis in
//! `research/TYPE_HIERARCHY.md`.
//!
//! # Design Philosophy
//!
//! These types serve as a **complete data map** - not for immediate parsing of
//! everything, but to know where data lives and have typed access when needed.
//!
//! # Module Structure
//!
//! - `root` - Top-level event types (7 root types)
//! - `message` - Message content types (5 content types)
//! - `progress` - Progress event types (5+ progress types)
//! - `attachment` - Attachment types (10+ types)
//! - `tool_result` - Tool result types (5+ types)
//! - `system` - System event subtypes (2+ types)
//! - `metadata` - Common metadata structures
//!
//! # Type Hierarchy Overview
//!
//! ```text
//! Level 1: Root Events (.type)
//! ├── progress (82,200)      → progress::ProgressEvent
//! ├── assistant (49,426)     → root::AssistantEvent
//! ├── user (29,913)          → root::UserEvent
//! ├── system (6)             → system::SystemEvent
//! ├── file-history-snapshot  → root::FileHistorySnapshot
//! ├── queue-operation        → root::QueueOperation
//! └── summary                → root::SessionSummary
//!
//! Level 2: Message Content (.message.content[].type)
//! ├── text (18,557)          → message::TextBlock
//! ├── tool_use (30,782)      → message::ToolUseBlock
//! ├── tool_result (29,848)   → message::ToolResultBlock
//! ├── image (5)              → message::ImageBlock
//! └── attachment             → attachment::AttachmentBlock
//!
//! Level 3: Progress Data (.data.type when root .type == 'progress')
//! ├── bash_progress (49,591)  → progress::BashProgressData
//! ├── hook_progress (31,668)  → progress::HookProgressData
//! ├── agent_progress (1,371)  → progress::AgentProgressData
//! ├── query_update            → progress::QueryUpdateData
//! └── search_results_received → progress::SearchResultsData
//!
//! Level 4: Attachment Types (.attachment.type)
//! ├── hook_success (31,648)            → attachment::HookSuccess
//! ├── todo_reminder (2,914)            → attachment::TodoReminder
//! ├── critical_system_reminder (1,096) → attachment::CriticalReminder
//! ├── edited_text_file (302)           → attachment::EditedTextFile
//! ├── edited_notebook_cell             → attachment::EditedNotebookCell
//! ├── file_snapshot                    → attachment::FileSnapshot
//! └── ... (more types)
//!
//! Level 5: Tool Result Types (.toolUseResult.type)
//! ├── text (28,313)  → tool_result::TextResult
//! ├── create (1,895) → tool_result::CreateResult
//! ├── update (155)   → tool_result::UpdateResult
//! ├── delete         → tool_result::DeleteResult
//! ├── read           → tool_result::ReadResult
//! └── error          → tool_result::ErrorResult
//!
//! Level 6: System Subtypes (.subtype when .type == 'system')
//! ├── compact_boundary       → system::CompactBoundary
//! ├── microcompact_boundary  → system::MicrocompactBoundary
//! └── ... (more types)
//! ```
//!
//! # Usage Examples
//!
//! ## Parsing Root Events
//!
//! ```rust
//! use zengeld_memory_core::sources::claude::SessionEvent;
//!
//! let line = r#"{"type": "user", ...}"#;
//! let event: SessionEvent = serde_json::from_str(line)?;
//!
//! match event {
//!     SessionEvent::User(user) => {
//!         // Access user message content
//!         for block in &user.message.content {
//!             // Process content blocks
//!         }
//!     }
//!     SessionEvent::Progress(progress) => {
//!         // Access progress data
//!         match &progress.data {
//!             ProgressData::BashProgress(bash) => {
//!                 println!("Output: {}", bash.full_output);
//!             }
//!             ProgressData::AgentProgress(agent) => {
//!                 println!("Agent: {}", agent.agent_id);
//!             }
//!             _ => {}
//!         }
//!     }
//!     _ => {}
//! }
//! # Ok::<(), serde_json::Error>(())
//! ```
//!
//! ## Accessing Nested Content
//!
//! ```rust
//! use zengeld_memory_core::sources::claude::{SessionEvent, ContentBlock};
//!
//! # let event: SessionEvent = serde_json::from_str(r#"{"type": "user", "uuid": "test", "timestamp": "2024-01-01T00:00:00Z", "sessionId": "test", "parentUuid": null, "isSidechain": false, "cwd": "/test", "message": {"role": "user", "content": []}}"#)?;
//! if let SessionEvent::User(user) = event {
//!     for block in &user.message.content {
//!         match block {
//!             ContentBlock::Text(text) => {
//!                 println!("Text: {}", text.text);
//!             }
//!             ContentBlock::ToolResult(result) => {
//!                 println!("Tool result ID: {}", result.tool_use_id);
//!
//!                 // Access nested tool use result
//!                 if let Some(tool_result) = &result.tool_use_result {
//!                     match tool_result {
//!                         ToolUseResult::Create(create) => {
//!                             println!("Created: {}", create.file_path);
//!                         }
//!                         _ => {}
//!                     }
//!                 }
//!             }
//!             _ => {}
//!         }
//!     }
//! }
//! # Ok::<(), serde_json::Error>(())
//! ```
//!
//! ## Working with Progress Events
//!
//! Progress events contain the full conversation context in
//! `.data.normalized_messages[]`, which recursively contains all event types:
//!
//! ```rust
//! use zengeld_memory_core::sources::claude::{SessionEvent, ProgressData, NormalizedMessage};
//!
//! # let event: SessionEvent = serde_json::from_str(r#"{"type": "progress", "uuid": "test", "timestamp": "2024-01-01T00:00:00Z", "sessionId": "test", "parentUuid": null, "isSidechain": false, "cwd": "/test", "data": {"type": "bash_progress", "output": "", "fullOutput": "", "elapsedTimeSeconds": 0, "totalLines": 0, "normalizedMessages": []}}"#)?;
//! if let SessionEvent::Progress(progress) = event {
//!     // Access normalized messages (conversation replay)
//!     if let Some(normalized) = progress.data.normalized_messages() {
//!         for msg in normalized {
//!             match msg {
//!                 NormalizedMessage::User(user) => {
//!                     println!("User turn");
//!                 }
//!                 NormalizedMessage::Assistant(assistant) => {
//!                     println!("Assistant turn");
//!                 }
//!                 NormalizedMessage::Attachment(attachment) => {
//!                     println!("Attachment type: {:?}", attachment.attachment_type);
//!                 }
//!                 _ => {}
//!             }
//!         }
//!     }
//! }
//! # Ok::<(), serde_json::Error>(())
//! ```

pub mod attachment;
pub mod message;
pub mod metadata;
pub mod progress;
pub mod root;
pub mod system;
pub mod tool_result;

// Re-export main types for convenience
pub use attachment::{AttachmentBlock, AttachmentType};
pub use message::{ContentBlock, MessageContent};
pub use metadata::EventMetadata;
pub use progress::{ProgressData, ProgressEvent};
pub use root::{
    AssistantMessage, CacheCreation, FileHistorySnapshot, QueueOperation, SessionEvent,
    SessionSummary, Snapshot, TokenUsage,
};
pub use system::{CompactMetadata, SystemEvent};
pub use tool_result::ToolUseResult;

// Re-export for backward compatibility with existing code
pub use root::QueueOperation as QueueOperationEvent;
pub use root::{AssistantEvent as AssistantMessageEvent, UserEvent as UserMessageEvent};

// ============================================================================
// Legacy Implementations for Backward Compatibility
// ============================================================================

impl SessionEvent {
    /// Extract tags for indexing (legacy method)
    ///
    /// Tags enable fast filtering of events without parsing full content.
    pub fn extract_tags(&self) -> Vec<String> {
        match self {
            Self::User(e) => {
                let mut tags = vec!["user".to_string()];

                if e.metadata.user_type.as_deref() == Some("external") {
                    tags.push("prompt".to_string());
                }

                if e.tool_use_result.is_some() {
                    tags.push("tool_result".to_string());
                }

                tags
            }

            Self::Assistant(e) => {
                let mut tags = vec!["assistant".to_string()];
                tags.push(e.message.model.clone());

                for block in &e.message.content {
                    match block {
                        ContentBlock::Text(_) => {
                            if !tags.contains(&"text".to_string()) {
                                tags.push("text".to_string());
                            }
                        }
                        ContentBlock::ToolUse(tool) => {
                            tags.push("tool_use".to_string());
                            tags.push(tool.name.clone());
                        }
                        _ => {}
                    }
                }

                tags
            }

            Self::Progress(e) => {
                let mut tags = vec!["progress".to_string()];

                match &e.data {
                    ProgressData::BashProgress(_) => {
                        tags.push("bash_progress".to_string());
                    }
                    ProgressData::AgentProgress(agent) => {
                        tags.push("agent_progress".to_string());
                        tags.push(agent.agent_id.clone());
                        if let Some(slug) = &e.metadata.slug {
                            tags.push(slug.clone());
                        }
                    }
                    ProgressData::HookProgress(hook) => {
                        tags.push("hook_progress".to_string());
                        tags.push(hook.hook_name.clone());
                    }
                    ProgressData::QueryUpdate(_) => {
                        tags.push("query_update".to_string());
                    }
                    ProgressData::SearchResultsReceived(_) => {
                        tags.push("search_results".to_string());
                    }
                    ProgressData::WaitingForTask(task) => {
                        tags.push("waiting_for_task".to_string());
                        tags.push(task.task_type.clone());
                    }
                    ProgressData::Unknown => {
                        tags.push("unknown_progress".to_string());
                    }
                }

                tags
            }

            Self::System(e) => {
                let mut tags = vec!["system".to_string()];

                if let Some(subtype) = &e.subtype {
                    tags.push(subtype.clone());

                    if e.is_compact_boundary() {
                        if let Some(meta) = &e.compact_metadata {
                            tags.push(meta.trigger.clone());
                        }
                    }
                }

                tags
            }

            Self::FileSnapshot(_) => vec!["file_snapshot".to_string()],

            Self::QueueOperation(e) => {
                vec!["queue_operation".to_string(), e.operation.clone()]
            }

            Self::Summary(_) => vec!["summary".to_string()],

            Self::Unknown => vec!["unknown".to_string()],
        }
    }

    /// Check if this event is important for context extraction
    pub fn is_context_relevant(&self) -> bool {
        match self {
            Self::Progress(e) => matches!(e.data, ProgressData::AgentProgress(_)),
            Self::User(e) => e.metadata.user_type.as_deref() == Some("external"),
            Self::Assistant(e) => e
                .message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text(_))),
            Self::System(e) => e.is_compact_boundary(),
            _ => false,
        }
    }
}

impl root::TokenUsage {
    /// Total tokens (input + output)
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Total input including cache operations
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }

    /// Effective input tokens with cache discount (90% for reads)
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn effective_input(&self) -> f64 {
        (self.input_tokens + self.cache_creation_input_tokens) as f64
            + (self.cache_read_input_tokens as f64 * 0.1)
    }

    /// Calculate cost in USD based on model pricing
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn calculate_cost(&self, model: &str) -> Option<f64> {
        let (input_cost, output_cost) = match normalize_model_name(model) {
            "haiku" => (1.0 / 1_000_000.0, 5.0 / 1_000_000.0),
            "sonnet" => (3.0 / 1_000_000.0, 15.0 / 1_000_000.0),
            "opus" => (15.0 / 1_000_000.0, 75.0 / 1_000_000.0),
            _ => return None,
        };

        let cache_write_cost = input_cost * 1.25;
        let cache_read_cost = input_cost * 0.1;

        Some(
            (self.input_tokens as f64 * input_cost)
                + (self.output_tokens as f64 * output_cost)
                + (self.cache_creation_input_tokens as f64 * cache_write_cost)
                + (self.cache_read_input_tokens as f64 * cache_read_cost),
        )
    }

    /// Format cost as human-readable string
    #[must_use]
    pub fn format_cost(&self, model: &str) -> String {
        match self.calculate_cost(model) {
            Some(cost) => {
                if cost < 0.01 {
                    format!("${cost:.4}")
                } else {
                    format!("${cost:.2}")
                }
            }
            None => "Unknown model".to_string(),
        }
    }

    /// Calculate approximate cost (deprecated)
    #[deprecated(since = "0.1.0", note = "Use calculate_cost(model) instead")]
    pub fn estimated_cost(&self) -> f64 {
        self.calculate_cost("sonnet").unwrap_or(0.0)
    }
}

/// Normalize model name to "haiku", "sonnet", or "opus"
fn normalize_model_name(model: &str) -> &str {
    let lower = model.to_lowercase();
    if lower.contains("haiku") {
        "haiku"
    } else if lower.contains("sonnet") {
        "sonnet"
    } else if lower.contains("opus") {
        "opus"
    } else {
        "unknown"
    }
}
