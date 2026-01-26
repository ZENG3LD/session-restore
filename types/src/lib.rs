//! Type definitions for Claude Code session events
//!
//! This library provides Rust types for parsing Claude Code session JSONL files.

#![allow(clippy::must_use_candidate)]
#![allow(clippy::doc_markdown)]

pub mod events;

// Re-export main types for convenience
pub use events::attachment::{AttachmentBlock, AttachmentType};
pub use events::message::{ContentBlock, MessageContent};
pub use events::progress::{ProgressData, ProgressEvent};
pub use events::root::{AssistantMessage, FileHistorySnapshot, SessionEvent};
pub use events::system::SystemEvent;
