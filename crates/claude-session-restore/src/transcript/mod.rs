//! Type definitions for Claude Code session events
//!
//! Rust types for the records of a Claude Code session JSONL file.

#![allow(clippy::must_use_candidate)]
#![allow(clippy::doc_markdown)]

pub mod events;

// Re-export main types for convenience
pub use events::attachment::{AttachmentBlock, AttachmentType};
pub use events::message::{ContentBlock, MessageContent};
pub use events::progress::{ProgressData, ProgressEvent};
pub use events::root::{AssistantMessage, FileHistorySnapshot, SessionEvent};
pub use events::system::SystemEvent;
