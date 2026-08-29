//! Message content types (Level 2)
//!
//! Found in `.message.content[]` for both user and assistant events.
//!
//! # Content Block Types
//!
//! ```text
//! Content Blocks (.message.content[].type)
//! ├── text (18,557)          - Text content from user/assistant
//! ├── tool_use (30,782)      - Tool invocations in assistant messages
//! ├── tool_result (29,848)   - Tool results in user messages
//! ├── image (5)              - Image attachments (base64)
//! └── attachment             - File attachments, hooks, reminders
//! ```
//!
//! # Usage
//!
//! Content blocks appear in:
//! - User messages: `text`, `tool_result`, `image`
//! - Assistant messages: `text`, `tool_use`
//! - Progress normalized messages: all types

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Message content wrapper
///
/// Contains role and content blocks for both user and assistant messages.
///
/// # Example
///
/// ```json
/// {
///   "role": "user",
///   "content": [
///     {"type": "text", "text": "Hello"},
///     {"type": "tool_result", "tool_use_id": "...", "content": "..."}
///   ]
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageContent {
    /// Role: "user" or "assistant"
    pub role: String,

    /// Content blocks (text, tool use, tool results, etc.)
    pub content: Vec<ContentBlock>,
}

/// Content block discriminator
///
/// All possible content block types found in message content arrays.
/// Uses serde's tagged enum to automatically parse based on `type` field.
///
/// # Links
///
/// - User messages contain: `Text`, `ToolResult`, `Image`
/// - Assistant messages contain: `Text`, `ToolUse`, `Thinking` (when extended thinking enabled)
/// - Progress normalized messages contain: all types including `Attachment`
///
/// # Frequency (per large session)
///
/// - `ToolUse`: ~30k occurrences
/// - `ToolResult`: ~30k occurrences
/// - `Text`: ~19k occurrences
/// - `Thinking`: Variable (only when extended thinking mode enabled)
/// - `Image`: ~5 occurrences
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Text content block
    ///
    /// Found in: user messages, assistant messages
    ///
    /// Contains plain text or markdown from user input or assistant responses.
    Text(TextBlock),

    /// Tool use invocation
    ///
    /// Found in: assistant messages
    ///
    /// Represents Claude invoking a tool (Read, Write, Edit, Bash, etc.)
    ToolUse(ToolUseBlock),

    /// Tool execution result
    ///
    /// Found in: user messages (as tool result feedback)
    ///
    /// Contains the output from a tool execution, sent back to Claude.
    ToolResult(ToolResultBlock),

    /// Image attachment
    ///
    /// Found in: user messages
    ///
    /// Base64-encoded image data sent by user.
    Image(ImageBlock),

    /// Attachment (files, hooks, reminders)
    ///
    /// Found in: progress normalized messages
    ///
    /// See `attachment` module for detailed attachment types.
    #[serde(rename = "attachment")]
    Attachment(crate::events::attachment::AttachmentBlock),

    /// Thinking block (extended thinking mode)
    ///
    /// Found in: assistant messages (only when user enables extended thinking)
    ///
    /// Contains Claude's reasoning process with signature verification.
    /// This is OPTIONAL - only present when extended thinking mode is enabled.
    Thinking(ThinkingBlock),

    /// Unknown content block type (forward compatibility)
    #[serde(other)]
    Unknown,
}

impl ContentBlock {
    /// Extract text if this is a text block
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(&text.text),
            _ => None,
        }
    }

    /// Extract tool use if this is a tool use block
    #[must_use]
    pub fn as_tool_use(&self) -> Option<(&str, &str, &JsonValue)> {
        match self {
            Self::ToolUse(tool) => Some((&tool.id, &tool.name, &tool.input)),
            _ => None,
        }
    }

    /// Extract thinking if this is a thinking block
    #[must_use]
    pub fn as_thinking(&self) -> Option<&str> {
        match self {
            Self::Thinking(thinking) => Some(&thinking.thinking),
            _ => None,
        }
    }

    /// Check if this is a tool result
    #[must_use]
    pub fn is_tool_result(&self) -> bool {
        matches!(self, Self::ToolResult(_))
    }

    /// Check if this is an attachment
    #[must_use]
    pub fn is_attachment(&self) -> bool {
        matches!(self, Self::Attachment(_))
    }

    /// Check if this is a thinking block
    #[must_use]
    pub fn is_thinking(&self) -> bool {
        matches!(self, Self::Thinking(_))
    }
}

/// Text content block
///
/// Contains plain text or markdown from user input or assistant responses.
///
/// # Example
///
/// ```json
/// {
///   "type": "text",
///   "text": "I'll help you implement that feature."
/// }
/// ```
///
/// # Usage
///
/// - User prompts
/// - Assistant explanations
/// - Agent task descriptions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBlock {
    /// Text content (plain text or markdown)
    pub text: String,
}

/// Tool use invocation block
///
/// Represents Claude invoking a tool (Read, Write, Edit, Bash, etc.)
///
/// # Example
///
/// ```json
/// {
///   "type": "tool_use",
///   "id": "toolu_abc123",
///   "name": "Read",
///   "input": {
///     "file_path": "/path/to/file.rs"
///   }
/// }
/// ```
///
/// # Links
///
/// - `id` links to `ToolResultBlock.tool_use_id` in user messages
/// - `id` links to `ProgressEvent.tool_use_id` for progress updates
///
/// # Common Tool Names
///
/// - `Read` - Read file contents
/// - `Write` - Write new file
/// - `Edit` - Edit existing file
/// - `Bash` - Execute bash command
/// - `Glob` - Search for files by pattern
/// - `Grep` - Search file contents
/// - `TodoWrite` - Update todo list
/// - `Agent` - Spawn sub-agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseBlock {
    /// Unique tool use ID
    ///
    /// Links to tool result in user message
    /// Format: `"toolu_{random}"` or `"{tool_name}-{uuid}"`
    pub id: String,

    /// Tool name
    ///
    /// Common tools: Read, Write, Edit, Bash, Glob, Grep, Agent
    pub name: String,

    /// Tool input parameters (JSON object)
    ///
    /// Structure depends on tool type:
    /// - Read: `{"file_path": "/path"}`
    /// - Write: `{"file_path": "/path", "content": "..."}`
    /// - Edit: `{"file_path": "/path", "old_string": "...", "new_string": "..."}`
    /// - Bash: `{"command": "cargo build", "description": "..."}`
    pub input: JsonValue,
}

/// Tool execution result block
///
/// Contains the output from a tool execution, sent back to Claude.
///
/// # Example
///
/// ```json
/// {
///   "type": "tool_result",
///   "tool_use_id": "toolu_abc123",
///   "content": {
///     "type": "text",
///     "content": "File contents here..."
///   }
/// }
/// ```
///
/// # Links
///
/// - `tool_use_id` links back to `ToolUseBlock.id` in assistant message
/// - Nested `content` can contain `ToolUseResult` for file operations
///
/// # Content Types
///
/// Content can be:
/// - String: simple text output
/// - Object with `type` field: structured result (see `tool_result` module)
/// - Array: multiple result items
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultBlock {
    /// Links back to tool use that generated this result
    #[serde(rename = "tool_use_id")]
    pub tool_use_id: String,

    /// Tool execution result (JSON value)
    ///
    /// Can be:
    /// - String: simple output (`"cargo build completed"`)
    /// - Object: structured result with `type` field (see `ToolUseResult`)
    /// - Array: multiple result items
    pub content: JsonValue,

    /// Structured tool use result (for file operations)
    ///
    /// Present when tool modifies files (Write, Edit)
    /// See `tool_result` module for detailed types
    #[serde(rename = "toolUseResult")]
    pub tool_use_result: Option<crate::events::tool_result::ToolUseResult>,
}

/// Thinking block (extended thinking mode)
///
/// Contains Claude's reasoning process when extended thinking mode is enabled.
/// This block is OPTIONAL and only appears when the user explicitly enables
/// extended thinking in their Claude settings.
///
/// # Example
///
/// ```json
/// {
///   "type": "thinking",
///   "thinking": "The user is asking about...",
///   "signature": "ErQYCkYICxgCKkA1FuCoAqSF..."
/// }
/// ```
///
/// # Usage
///
/// - Available when extended thinking mode is enabled
/// - Contains full reasoning process
/// - Includes cryptographic signature for verification
/// - Makes Claude competitive with Gemini for reasoning extraction
///
/// # Frequency
///
/// Variable - only present when extended thinking is enabled by user
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingBlock {
    /// Full reasoning text
    ///
    /// Contains Claude's complete thought process, including:
    /// - Analysis of the request
    /// - Consideration of alternatives
    /// - Decision-making rationale
    /// - Context evaluation
    pub thinking: String,

    /// Cryptographic signature for verification
    ///
    /// Used to verify the authenticity of the thinking content.
    /// Optional field that may not be present in all thinking blocks.
    pub signature: Option<String>,
}

/// Image attachment block
///
/// Base64-encoded image data sent by user.
///
/// # Example
///
/// ```json
/// {
///   "type": "image",
///   "source": {
///     "type": "base64",
///     "media_type": "image/png",
///     "data": "iVBORw0KGgoAAAANSUhEUgAA..."
///   }
/// }
/// ```
///
/// # Usage
///
/// - User sends screenshot
/// - User sends diagram for analysis
/// - User sends error message screenshot
///
/// # Frequency
///
/// Very rare in typical sessions (~5 per large session)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageBlock {
    /// Image source (base64 data)
    pub source: ImageSource,
}

/// Image source (base64 encoded)
///
/// Contains base64-encoded image data and media type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    /// Source type (always "base64")
    #[serde(rename = "type")]
    pub source_type: String,

    /// Media type (MIME type)
    ///
    /// Examples: `"image/png"`, `"image/jpeg"`, `"image/webp"`
    pub media_type: String,

    /// Base64-encoded image data
    pub data: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_text_block() {
        let json = r#"{
            "type": "text",
            "text": "Hello world"
        }"#;

        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert!(matches!(block, ContentBlock::Text(_)));

        if let ContentBlock::Text(text) = block {
            assert_eq!(text.text, "Hello world");
        }
    }

    #[test]
    fn test_parse_tool_use_block() {
        let json = r#"{
            "type": "tool_use",
            "id": "toolu_abc123",
            "name": "Read",
            "input": {
                "file_path": "/test/file.rs"
            }
        }"#;

        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert!(matches!(block, ContentBlock::ToolUse(_)));

        if let ContentBlock::ToolUse(tool) = block {
            assert_eq!(tool.id, "toolu_abc123");
            assert_eq!(tool.name, "Read");
            assert_eq!(tool.input["file_path"], "/test/file.rs");
        }
    }

    #[test]
    fn test_parse_tool_result_block() {
        let json = r#"{
            "type": "tool_result",
            "tool_use_id": "toolu_abc123",
            "content": "File contents here"
        }"#;

        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert!(matches!(block, ContentBlock::ToolResult(_)));
        assert!(block.is_tool_result());

        if let ContentBlock::ToolResult(result) = block {
            assert_eq!(result.tool_use_id, "toolu_abc123");
            assert_eq!(result.content, "File contents here");
        }
    }

    #[test]
    fn test_parse_image_block() {
        let json = r#"{
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": "iVBORw0KGgo="
            }
        }"#;

        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert!(matches!(block, ContentBlock::Image(_)));

        if let ContentBlock::Image(image) = block {
            assert_eq!(image.source.source_type, "base64");
            assert_eq!(image.source.media_type, "image/png");
            assert_eq!(image.source.data, "iVBORw0KGgo=");
        }
    }

    #[test]
    fn test_content_block_helpers() {
        let text_block = ContentBlock::Text(TextBlock {
            text: "Test".to_string(),
        });
        assert_eq!(text_block.as_text(), Some("Test"));
        assert!(!text_block.is_tool_result());

        let tool_block = ContentBlock::ToolUse(ToolUseBlock {
            id: "tool-1".to_string(),
            name: "Read".to_string(),
            input: serde_json::json!({}),
        });
        let tool_use = tool_block.as_tool_use();
        assert!(tool_use.is_some());
        let (id, name, _) = tool_use.unwrap();
        assert_eq!(id, "tool-1");
        assert_eq!(name, "Read");
    }

    #[test]
    fn test_parse_thinking_block() {
        let json = r#"{
            "type": "thinking",
            "thinking": "Let me analyze this request carefully...",
            "signature": "ErQYCkYICxgCKkA1FuCoAqSF..."
        }"#;

        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert!(matches!(block, ContentBlock::Thinking(_)));
        assert!(block.is_thinking());

        if let ContentBlock::Thinking(thinking) = block {
            assert_eq!(thinking.thinking, "Let me analyze this request carefully...");
            assert!(thinking.signature.is_some());
            assert_eq!(thinking.signature.unwrap(), "ErQYCkYICxgCKkA1FuCoAqSF...");
        }
    }

    #[test]
    fn test_parse_thinking_block_without_signature() {
        let json = r#"{
            "type": "thinking",
            "thinking": "Analyzing the problem..."
        }"#;

        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert!(matches!(block, ContentBlock::Thinking(_)));

        if let ContentBlock::Thinking(thinking) = block {
            assert_eq!(thinking.thinking, "Analyzing the problem...");
            assert!(thinking.signature.is_none());
        }
    }

    #[test]
    fn test_content_block_as_thinking() {
        let thinking_block = ContentBlock::Thinking(ThinkingBlock {
            thinking: "Test reasoning".to_string(),
            signature: Some("sig123".to_string()),
        });

        assert_eq!(thinking_block.as_thinking(), Some("Test reasoning"));
        assert!(thinking_block.is_thinking());

        let text_block = ContentBlock::Text(TextBlock {
            text: "Test".to_string(),
        });
        assert_eq!(text_block.as_thinking(), None);
        assert!(!text_block.is_thinking());
    }

    #[test]
    fn test_message_content() {
        let json = r#"{
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "Let me read the file"
                },
                {
                    "type": "tool_use",
                    "id": "tool-123",
                    "name": "Read",
                    "input": {"file_path": "/test.rs"}
                }
            ]
        }"#;

        let message: MessageContent = serde_json::from_str(json).unwrap();
        assert_eq!(message.role, "assistant");
        assert_eq!(message.content.len(), 2);

        assert!(matches!(message.content[0], ContentBlock::Text(_)));
        assert!(matches!(message.content[1], ContentBlock::ToolUse(_)));
    }

    #[test]
    fn test_message_with_thinking() {
        let json = r#"{
            "role": "assistant",
            "content": [
                {
                    "type": "thinking",
                    "thinking": "I need to analyze this carefully..."
                },
                {
                    "type": "text",
                    "text": "Based on my analysis..."
                }
            ]
        }"#;

        let message: MessageContent = serde_json::from_str(json).unwrap();
        assert_eq!(message.role, "assistant");
        assert_eq!(message.content.len(), 2);

        assert!(matches!(message.content[0], ContentBlock::Thinking(_)));
        assert!(matches!(message.content[1], ContentBlock::Text(_)));

        // Verify we can extract thinking
        assert!(message.content[0].is_thinking());
        assert_eq!(
            message.content[0].as_thinking(),
            Some("I need to analyze this carefully...")
        );
    }
}
