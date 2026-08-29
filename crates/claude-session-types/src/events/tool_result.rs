//! Tool result types (Level 5)
//!
//! Found in `.toolUseResult` field of user messages that contain tool results.
//!
//! # Tool Result Types
//!
//! ```text
//! Tool Results (.toolUseResult.type)
//! ├── text (28,313)    - Text output from tools (Bash, Read, Grep)
//! ├── create (1,895)   - File creation result (Write)
//! ├── update (155)     - File update result (Edit)
//! ├── delete           - File deletion result
//! ├── read             - File read result (structured)
//! └── error            - Tool execution error
//! ```
//!
//! # Usage
//!
//! Tool results appear in user messages after tool execution:
//!
//! ```json
//! {
//!   "type": "user",
//!   "toolUseResult": {
//!     "type": "create",
//!     "filePath": "/path/to/new_file.rs",
//!     "content": "// File contents",
//!     "structuredPatch": [],
//!     "originalFile": null
//!   }
//! }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Tool use result discriminator
///
/// Represents the structured result of a tool execution.
///
/// # Links
///
/// - Present in `UserEvent.tool_use_result`
/// - Also in `ToolResultBlock.tool_use_result`
/// - Links to `ToolUseBlock.id` via `tool_use_id`
///
/// # Frequency (per large session)
///
/// - `Text`: ~28k (most common - Bash, Read, etc.)
/// - `Create`: ~1.9k (Write tool)
/// - `Update`: ~155 (Edit tool)
/// - Others: rare
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolUseResult {
    /// Text result (from Bash, Read, Grep, etc.)
    ///
    /// Most common tool result type. Contains plain text output.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "text",
    ///   "content": "cargo build succeeded"
    /// }
    /// ```
    Text(TextResult),

    /// File creation result (from Write tool)
    ///
    /// Contains full file contents and metadata.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "create",
    ///   "filePath": "/path/to/new_file.rs",
    ///   "content": "pub fn main() {}",
    ///   "structuredPatch": [],
    ///   "originalFile": null
    /// }
    /// ```
    Create(CreateResult),

    /// File update result (from Edit tool)
    ///
    /// Contains updated file contents and diff information.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "update",
    ///   "filePath": "/path/to/file.rs",
    ///   "content": "pub fn main() { println!(\"Hello\"); }",
    ///   "structuredPatch": [
    ///     {"oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1}
    ///   ],
    ///   "originalFile": "pub fn main() {}"
    /// }
    /// ```
    Update(UpdateResult),

    /// File deletion result
    ///
    /// Confirms file was deleted.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "delete",
    ///   "filePath": "/path/to/deleted_file.rs"
    /// }
    /// ```
    Delete(DeleteResult),

    /// File read result (structured)
    ///
    /// Structured result from Read tool with metadata.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "read",
    ///   "filePath": "/path/to/file.rs",
    ///   "content": "pub fn main() {}",
    ///   "lineCount": 1
    /// }
    /// ```
    Read(ReadResult),

    /// Tool execution error
    ///
    /// Contains error details when tool execution fails.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "error",
    ///   "error": "File not found: /path/to/missing.rs",
    ///   "toolName": "Read"
    /// }
    /// ```
    Error(ErrorResult),

    /// Image result
    ///
    /// Contains image data from tool execution.
    ///
    /// # Example
    ///
    /// ```json
    /// {
    ///   "type": "image",
    ///   "source": {
    ///     "type": "base64",
    ///     "media_type": "image/png",
    ///     "data": "iVBORw0KGgo..."
    ///   }
    /// }
    /// ```
    ///
    /// # Frequency
    ///
    /// ~77 occurrences per large session
    Image(ImageResult),

    /// Unknown tool result type (forward compatibility)
    #[serde(other)]
    Unknown,
}

impl ToolUseResult {
    /// Extract file path if this is a file operation
    pub fn file_path(&self) -> Option<&str> {
        match self {
            Self::Create(r) => Some(&r.file_path),
            Self::Update(r) => Some(&r.file_path),
            Self::Delete(r) => Some(&r.file_path),
            Self::Read(r) => Some(&r.file_path),
            Self::Image(r) => r.file_path.as_deref(),
            _ => None,
        }
    }

    /// Check if this is a file creation
    pub fn is_create(&self) -> bool {
        matches!(self, Self::Create(_))
    }

    /// Check if this is a file update
    pub fn is_update(&self) -> bool {
        matches!(self, Self::Update(_))
    }

    /// Check if this is an error
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }

    /// Check if this is an image
    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image(_))
    }

    /// Extract text content if available
    pub fn text_content(&self) -> Option<&str> {
        match self {
            Self::Text(r) => Some(&r.content),
            Self::Create(r) => Some(&r.content),
            Self::Update(r) => Some(&r.content),
            Self::Read(r) => Some(&r.content),
            _ => None,
        }
    }

    /// Get create result if this is create
    pub fn as_create(&self) -> Option<&CreateResult> {
        match self {
            Self::Create(r) => Some(r),
            _ => None,
        }
    }

    /// Get update result if this is update
    pub fn as_update(&self) -> Option<&UpdateResult> {
        match self {
            Self::Update(r) => Some(r),
            _ => None,
        }
    }
}

/// Text result (most common)
///
/// Plain text output from tools like Bash, Grep, Glob, etc.
///
/// # Frequency
///
/// ~28k occurrences per large session (most common tool result)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextResult {
    /// Text content
    pub content: String,
}

/// File creation result
///
/// Result from Write tool creating a new file.
///
/// # Frequency
///
/// ~1,895 occurrences per large session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateResult {
    /// Path to created file
    #[serde(rename = "filePath")]
    pub file_path: String,

    /// New file contents
    pub content: String,

    /// Structured patch (typically empty for new files)
    #[serde(rename = "structuredPatch")]
    pub structured_patch: Vec<PatchHunk>,

    /// Original file contents (null for new files)
    #[serde(rename = "originalFile")]
    pub original_file: Option<String>,
}

/// File update result
///
/// Result from Edit tool modifying an existing file.
///
/// # Frequency
///
/// ~155 occurrences per large session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateResult {
    /// Path to updated file
    #[serde(rename = "filePath")]
    pub file_path: String,

    /// Updated file contents
    pub content: String,

    /// Structured patch (diff information)
    #[serde(rename = "structuredPatch")]
    pub structured_patch: Vec<PatchHunk>,

    /// Original file contents (before edit)
    #[serde(rename = "originalFile")]
    pub original_file: Option<String>,
}

/// File deletion result
///
/// Result from tool deleting a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteResult {
    /// Path to deleted file
    #[serde(rename = "filePath")]
    pub file_path: String,
}

/// File read result (structured)
///
/// Structured result from Read tool with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResult {
    /// Path to read file
    #[serde(rename = "filePath")]
    pub file_path: String,

    /// File contents
    pub content: String,

    /// Number of lines in file
    #[serde(rename = "lineCount")]
    pub line_count: Option<u64>,
}

/// Tool execution error
///
/// Error details when tool execution fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResult {
    /// Error message
    pub error: String,

    /// Tool name that failed
    #[serde(rename = "toolName")]
    pub tool_name: Option<String>,

    /// Additional error context
    #[serde(flatten)]
    pub extra: JsonValue,
}

/// Image result
///
/// Contains image data from tool execution.
///
/// # Frequency
///
/// ~77 occurrences per large session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageResult {
    /// Image source
    pub source: ImageSource,

    /// Optional file path if image was saved
    #[serde(rename = "filePath")]
    pub file_path: Option<String>,
}

/// Image source for ImageResult
///
/// Contains base64-encoded image data and media type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    /// Source type (typically "base64")
    #[serde(rename = "type")]
    pub source_type: String,

    /// Media type (MIME type)
    ///
    /// Examples: `"image/png"`, `"image/jpeg"`, `"image/webp"`
    #[serde(rename = "media_type")]
    pub media_type: String,

    /// Base64-encoded image data
    pub data: String,
}

/// Patch hunk (unified diff format)
///
/// Represents a single hunk in a diff, describing changes to a file.
///
/// # Example
///
/// ```json
/// {
///   "oldStart": 10,
///   "oldLines": 5,
///   "newStart": 10,
///   "newLines": 7
/// }
/// ```
///
/// This represents:
/// - Lines 10-14 in old file (5 lines)
/// - Replaced with lines 10-16 in new file (7 lines)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchHunk {
    /// Starting line in old file
    #[serde(rename = "oldStart")]
    pub old_start: u64,

    /// Number of lines in old file
    #[serde(rename = "oldLines")]
    pub old_lines: u64,

    /// Starting line in new file
    #[serde(rename = "newStart")]
    pub new_start: u64,

    /// Number of lines in new file
    #[serde(rename = "newLines")]
    pub new_lines: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_text_result() {
        let json = r#"{
            "type": "text",
            "content": "cargo build succeeded"
        }"#;

        let result: ToolUseResult = serde_json::from_str(json).unwrap();
        assert!(matches!(result, ToolUseResult::Text(_)));

        if let ToolUseResult::Text(text) = result {
            assert_eq!(text.content, "cargo build succeeded");
        }
    }

    #[test]
    fn test_parse_create_result() {
        let json = r#"{
            "type": "create",
            "filePath": "/test/new_file.rs",
            "content": "pub fn main() {}",
            "structuredPatch": [],
            "originalFile": null
        }"#;

        let result: ToolUseResult = serde_json::from_str(json).unwrap();
        assert!(result.is_create());
        assert_eq!(result.file_path(), Some("/test/new_file.rs"));

        if let ToolUseResult::Create(create) = result {
            assert_eq!(create.file_path, "/test/new_file.rs");
            assert_eq!(create.content, "pub fn main() {}");
            assert!(create.structured_patch.is_empty());
            assert!(create.original_file.is_none());
        }
    }

    #[test]
    fn test_parse_update_result() {
        let json = r#"{
            "type": "update",
            "filePath": "/test/file.rs",
            "content": "pub fn main() { println!(\"Hello\"); }",
            "structuredPatch": [
                {
                    "oldStart": 1,
                    "oldLines": 1,
                    "newStart": 1,
                    "newLines": 1
                }
            ],
            "originalFile": "pub fn main() {}"
        }"#;

        let result: ToolUseResult = serde_json::from_str(json).unwrap();
        assert!(result.is_update());
        assert_eq!(result.file_path(), Some("/test/file.rs"));

        if let ToolUseResult::Update(update) = result {
            assert_eq!(update.file_path, "/test/file.rs");
            assert_eq!(update.content, "pub fn main() { println!(\"Hello\"); }");
            assert_eq!(update.structured_patch.len(), 1);
            assert_eq!(update.original_file, Some("pub fn main() {}".to_string()));

            let patch = &update.structured_patch[0];
            assert_eq!(patch.old_start, 1);
            assert_eq!(patch.old_lines, 1);
            assert_eq!(patch.new_start, 1);
            assert_eq!(patch.new_lines, 1);
        }
    }

    #[test]
    fn test_parse_delete_result() {
        let json = r#"{
            "type": "delete",
            "filePath": "/test/deleted.rs"
        }"#;

        let result: ToolUseResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.file_path(), Some("/test/deleted.rs"));
    }

    #[test]
    fn test_parse_error_result() {
        let json = r#"{
            "type": "error",
            "error": "File not found",
            "toolName": "Read"
        }"#;

        let result: ToolUseResult = serde_json::from_str(json).unwrap();
        assert!(result.is_error());

        if let ToolUseResult::Error(error) = result {
            assert_eq!(error.error, "File not found");
            assert_eq!(error.tool_name, Some("Read".to_string()));
        }
    }

    #[test]
    fn test_parse_image_result() {
        let json = r#"{
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": "iVBORw0KGgo="
            },
            "filePath": "/tmp/screenshot.png"
        }"#;

        let result: ToolUseResult = serde_json::from_str(json).unwrap();
        assert!(result.is_image());
        assert_eq!(result.file_path(), Some("/tmp/screenshot.png"));

        if let ToolUseResult::Image(image) = result {
            assert_eq!(image.source.source_type, "base64");
            assert_eq!(image.source.media_type, "image/png");
            assert_eq!(image.source.data, "iVBORw0KGgo=");
            assert_eq!(image.file_path, Some("/tmp/screenshot.png".to_string()));
        }
    }

    #[test]
    fn test_text_content_extraction() {
        let text_result = ToolUseResult::Text(TextResult {
            content: "Output".to_string(),
        });
        assert_eq!(text_result.text_content(), Some("Output"));

        let create_result = ToolUseResult::Create(CreateResult {
            file_path: "/test.rs".to_string(),
            content: "Code".to_string(),
            structured_patch: vec![],
            original_file: None,
        });
        assert_eq!(create_result.text_content(), Some("Code"));

        let delete_result = ToolUseResult::Delete(DeleteResult {
            file_path: "/test.rs".to_string(),
        });
        assert_eq!(delete_result.text_content(), None);
    }
}
