//! Attachment types (Level 4)
//!
//! Found in `.data.normalizedMessages[].attachment` within progress events.
//!
//! # Attachment Types
//!
//! ```text
//! Attachments (.attachment.type)
//! ├── hook_success (31,648)            - Successful hook execution
//! ├── todo_reminder (2,914)            - Todo list reminders
//! ├── critical_system_reminder (1,096) - Critical warnings
//! ├── edited_text_file (302)           - File edit summaries
//! ├── edited_notebook_cell             - Jupyter cell edits
//! ├── file_snapshot                    - File state snapshots
//! ├── hook_failure                     - Failed hook execution
//! ├── hook_progress                    - Hook execution progress
//! ├── agent_spawn                      - Agent delegation info
//! └── ... (more types as discovered)
//! ```
//!
//! # Usage
//!
//! Attachments appear in progress events as part of normalized messages:
//!
//! ```json
//! {
//!   "type": "progress",
//!   "data": {
//!     "normalizedMessages": [
//!       {
//!         "type": "attachment",
//!         "attachment": {
//!           "type": "hook_success",
//!           "hookName": "pre-commit",
//!           "output": "✓ All checks passed"
//!         }
//!       }
//!     ]
//!   }
//! }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Attachment block wrapper
///
/// Contains an attachment of a specific type.
///
/// Found in: `progress.data.normalizedMessages[].attachment`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentBlock {
    /// Attachment type and data
    #[serde(flatten)]
    pub attachment: AttachmentType,
}

/// Attachment type discriminator
///
/// All possible attachment types found in progress normalized messages.
///
/// # Frequency (per large session)
///
/// - `HookSuccess`: ~31k (most common)
/// - `TodoReminder`: ~2.9k
/// - `CriticalSystemReminder`: ~1.1k
/// - `EditedTextFile`: ~302
/// - Others: rare
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentType {
    /// Hook execution success
    ///
    /// Emitted when a pre/post hook completes successfully.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "hook_success",
    ///   "hookName": "pre-commit",
    ///   "hookEvent": "pre-tool-use",
    ///   "output": "✓ All checks passed"
    /// }
    /// ```
    ///
    /// # Frequency
    ///
    /// ~31,648 occurrences per large session (most common attachment)
    HookSuccess(HookSuccess),

    /// Hook execution failure
    ///
    /// Emitted when a pre/post hook fails.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "hook_failure",
    ///   "hookName": "pre-commit",
    ///   "hookEvent": "pre-tool-use",
    ///   "error": "Tests failed",
    ///   "exitCode": 1
    /// }
    /// ```
    HookFailure(HookFailure),

    /// Hook execution progress
    ///
    /// Real-time progress updates during hook execution.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "hook_progress",
    ///   "hookName": "pre-commit",
    ///   "hookEvent": "pre-tool-use",
    ///   "output": "Running tests..."
    /// }
    /// ```
    HookProgress(HookProgress),

    /// Todo list reminder
    ///
    /// Reminds Claude of current todo items.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "todo_reminder",
    ///   "todos": [
    ///     {"content": "Fix bug in parser", "status": "in_progress", "activeForm": "Fixing bug"},
    ///     {"content": "Write tests", "status": "pending", "activeForm": "Writing tests"}
    ///   ]
    /// }
    /// ```
    ///
    /// # Frequency
    ///
    /// ~2,914 occurrences per large session
    TodoReminder(TodoReminder),

    /// Critical system reminder
    ///
    /// Important system warnings or reminders.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "critical_system_reminder",
    ///   "message": "Budget warning: 80% of tokens used",
    ///   "level": "warning"
    /// }
    /// ```
    ///
    /// # Frequency
    ///
    /// ~1,096 occurrences per large session
    CriticalSystemReminder(CriticalSystemReminder),

    /// Edited text file summary
    ///
    /// Summary of file edits with line-numbered snippet.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "edited_text_file",
    ///   "filename": "/path/to/file.rs",
    ///   "snippet": "42→pub mod kucoin;\n43→pub mod binance;...",
    ///   "description": "Added new exchange modules"
    /// }
    /// ```
    ///
    /// # Frequency
    ///
    /// ~302 occurrences per large session
    EditedTextFile(EditedTextFile),

    /// Edited notebook cell
    ///
    /// Summary of Jupyter notebook cell edits.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "edited_notebook_cell",
    ///   "filename": "/path/to/notebook.ipynb",
    ///   "cellIndex": 5,
    ///   "cellType": "code",
    ///   "snippet": "import pandas as pd..."
    /// }
    /// ```
    EditedNotebookCell(EditedNotebookCell),

    /// File snapshot
    ///
    /// Snapshot of file state at a point in time.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "file_snapshot",
    ///   "filePath": "/path/to/file.rs",
    ///   "content": "pub fn main() {}",
    ///   "timestamp": "2024-01-01T00:00:00Z"
    /// }
    /// ```
    FileSnapshot(FileSnapshot),

    /// Agent spawn notification
    ///
    /// Notifies that an agent was spawned for a task.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "agent_spawn",
    ///   "agentId": "abc123",
    ///   "agentSlug": "rust-implementer",
    ///   "prompt": "Implement feature X"
    /// }
    /// ```
    AgentSpawn(AgentSpawn),

    /// Unknown attachment type (forward compatibility)
    #[serde(other)]
    Unknown,
}

/// Hook execution success
///
/// Most common attachment type (~31k per session).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSuccess {
    /// Hook name (e.g., "pre-commit")
    #[serde(rename = "hookName")]
    pub hook_name: String,

    /// Hook event type (e.g., "pre-tool-use", "post-tool-use")
    #[serde(rename = "hookEvent")]
    pub hook_event: String,

    /// Hook output
    pub output: Option<String>,

    /// Execution time (milliseconds)
    #[serde(rename = "executionTimeMs")]
    pub execution_time_ms: Option<u64>,
}

/// Hook execution failure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookFailure {
    /// Hook name
    #[serde(rename = "hookName")]
    pub hook_name: String,

    /// Hook event type
    #[serde(rename = "hookEvent")]
    pub hook_event: String,

    /// Error message
    pub error: String,

    /// Exit code
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,

    /// Error output (stderr)
    pub stderr: Option<String>,
}

/// Hook execution progress
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookProgress {
    /// Hook name
    #[serde(rename = "hookName")]
    pub hook_name: String,

    /// Hook event type
    #[serde(rename = "hookEvent")]
    pub hook_event: String,

    /// Progress output
    pub output: String,
}

/// Todo list reminder
///
/// ~2,914 occurrences per large session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoReminder {
    /// Todo items
    pub todos: Vec<TodoItem>,
}

/// Todo item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    /// Task description (imperative form)
    pub content: String,

    /// Active form (present continuous for display)
    #[serde(rename = "activeForm")]
    pub active_form: String,

    /// Task status: "pending", "`in_progress`", "completed"
    pub status: String,
}

/// Critical system reminder
///
/// ~1,096 occurrences per large session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriticalSystemReminder {
    /// Reminder message
    pub message: String,

    /// Severity level: "info", "warning", "error"
    pub level: Option<String>,

    /// Additional context
    #[serde(flatten)]
    pub extra: JsonValue,
}

/// Edited text file summary
///
/// ~302 occurrences per large session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditedTextFile {
    /// File path
    pub filename: String,

    /// Line-numbered snippet showing changes
    ///
    /// Format: "42→pub mod kucoin;\n43→pub mod binance;..."
    pub snippet: String,

    /// Description of changes
    pub description: Option<String>,
}

/// Edited notebook cell
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditedNotebookCell {
    /// Notebook file path
    pub filename: String,

    /// Cell index (0-based)
    #[serde(rename = "cellIndex")]
    pub cell_index: u64,

    /// Cell type: "code", "markdown", "raw"
    #[serde(rename = "cellType")]
    pub cell_type: String,

    /// Cell content snippet
    pub snippet: String,

    /// Description of changes
    pub description: Option<String>,
}

/// File snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshot {
    /// File path
    #[serde(rename = "filePath")]
    pub file_path: String,

    /// File contents at snapshot time
    pub content: String,

    /// Snapshot timestamp
    pub timestamp: Option<String>,
}

/// Agent spawn notification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpawn {
    /// Agent ID
    #[serde(rename = "agentId")]
    pub agent_id: String,

    /// Agent slug (e.g., "rust-implementer")
    #[serde(rename = "agentSlug")]
    pub agent_slug: String,

    /// Agent prompt/task
    pub prompt: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hook_success() {
        let json = r#"{
            "type": "hook_success",
            "hookName": "pre-commit",
            "hookEvent": "pre-tool-use",
            "output": "✓ All checks passed",
            "executionTimeMs": 150
        }"#;

        let attachment: AttachmentType = serde_json::from_str(json).unwrap();
        assert!(matches!(attachment, AttachmentType::HookSuccess(_)));

        if let AttachmentType::HookSuccess(hook) = attachment {
            assert_eq!(hook.hook_name, "pre-commit");
            assert_eq!(hook.hook_event, "pre-tool-use");
            assert_eq!(hook.output, Some("✓ All checks passed".to_string()));
            assert_eq!(hook.execution_time_ms, Some(150));
        }
    }

    #[test]
    fn test_parse_todo_reminder() {
        let json = r#"{
            "type": "todo_reminder",
            "todos": [
                {
                    "content": "Fix bug",
                    "activeForm": "Fixing bug",
                    "status": "in_progress"
                },
                {
                    "content": "Write tests",
                    "activeForm": "Writing tests",
                    "status": "pending"
                }
            ]
        }"#;

        let attachment: AttachmentType = serde_json::from_str(json).unwrap();
        assert!(matches!(attachment, AttachmentType::TodoReminder(_)));

        if let AttachmentType::TodoReminder(reminder) = attachment {
            assert_eq!(reminder.todos.len(), 2);
            assert_eq!(reminder.todos[0].content, "Fix bug");
            assert_eq!(reminder.todos[0].status, "in_progress");
            assert_eq!(reminder.todos[1].content, "Write tests");
            assert_eq!(reminder.todos[1].status, "pending");
        }
    }

    #[test]
    fn test_parse_edited_text_file() {
        let json = r#"{
            "type": "edited_text_file",
            "filename": "/path/to/file.rs",
            "snippet": "42→pub mod kucoin;\n43→pub mod binance;",
            "description": "Added exchange modules"
        }"#;

        let attachment: AttachmentType = serde_json::from_str(json).unwrap();
        assert!(matches!(attachment, AttachmentType::EditedTextFile(_)));

        if let AttachmentType::EditedTextFile(edited) = attachment {
            assert_eq!(edited.filename, "/path/to/file.rs");
            assert!(edited.snippet.contains("pub mod kucoin"));
            assert_eq!(
                edited.description,
                Some("Added exchange modules".to_string())
            );
        }
    }

    #[test]
    fn test_parse_critical_reminder() {
        let json = r#"{
            "type": "critical_system_reminder",
            "message": "Budget warning: 80% used",
            "level": "warning"
        }"#;

        let attachment: AttachmentType = serde_json::from_str(json).unwrap();
        assert!(matches!(
            attachment,
            AttachmentType::CriticalSystemReminder(_)
        ));

        if let AttachmentType::CriticalSystemReminder(reminder) = attachment {
            assert_eq!(reminder.message, "Budget warning: 80% used");
            assert_eq!(reminder.level, Some("warning".to_string()));
        }
    }

    #[test]
    fn test_parse_agent_spawn() {
        let json = r#"{
            "type": "agent_spawn",
            "agentId": "abc123",
            "agentSlug": "rust-implementer",
            "prompt": "Implement feature X"
        }"#;

        let attachment: AttachmentType = serde_json::from_str(json).unwrap();
        assert!(matches!(attachment, AttachmentType::AgentSpawn(_)));

        if let AttachmentType::AgentSpawn(spawn) = attachment {
            assert_eq!(spawn.agent_id, "abc123");
            assert_eq!(spawn.agent_slug, "rust-implementer");
            assert_eq!(spawn.prompt, "Implement feature X");
        }
    }
}
