//! Session Summary CLI
//!
//! Restores bounded working context from a Claude Code session transcript:
//! finds sessions on disk (`list`) and prints a digest of verbatim quotes from
//! the tail of a chosen session (`load`). The digest is never an agent-written
//! summary — every line comes straight from the transcript: user prompts,
//! assistant text, tool calls with their key argument, and system errors.
//!
//! # Bounded, not schema-strict
//!
//! The Claude Code transcript format keeps growing new root event types and
//! new field shapes on existing ones. This parser treats that as normal: an
//! unrecognized root type is skipped silently (never a parse failure for the
//! rest of the line's siblings — see [`claude_session_types::events::SessionEvent::Unknown`]),
//! and a handful of known fields (`message.content`, `toolUseResult`) accept
//! more than one wire shape rather than dropping the whole event.
//!
//! # Reads are byte-budgeted, not full-file scans
//!
//! Both `list` and `load` read a bounded window from the end of the file
//! (`read_tail_lines`) with a single seek plus one read — cost is
//! proportional to the window, not to file size, so a multi-gigabyte
//! transcript loads in well under a second. Session titles can lag behind a
//! large tail window on a very bursty session, so title detection falls back
//! to a small bounded read from the *start* of the file (`read_head_lines`)
//! when the tail window doesn't carry one.
//!
//! # Topic selection
//!
//! In priority order: the session's own `custom-title` (Claude Code's UI
//! title), then `ai-title` (model-generated), then `last-prompt` (latest
//! verbatim human prompt), then the first genuine human-typed prompt in the
//! session (skipping harness-injected meta turns, command-palette
//! injections, and tool-result-only turns) — never a generic placeholder
//! unless none of the above exist at all.

#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_precision_loss)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use claude_session_types::events::{ProgressData, SessionEvent};
use colored::Colorize;
use regex::Regex;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// JSON report schema tag for `list --json`.
const SCHEMA_LIST: &str = "claude-session-restore-list-v1";
/// JSON report schema tag for `load --json`.
const SCHEMA_LOAD: &str = "claude-session-restore-load-v1";

/// Tail byte budget for `load` — generous, since a full digest is worth
/// reading more context for.
const LOAD_TAIL_BYTES: u64 = 32 * 1024 * 1024;
/// Tail byte budget per session for `list`'s preview — small, since only a
/// handful of recent items are shown per session and up to `--limit` sessions
/// are scanned per invocation.
const LIST_TAIL_BYTES: u64 = 4 * 1024 * 1024;
/// Head byte budget used by the title/first-prompt fallback scan, for both
/// commands.
const HEAD_FALLBACK_BYTES: u64 = 1024 * 1024;

/// Per-vector item cap for `load`'s digest.
const LOAD_MAX_ITEMS: usize = 10;
/// Per-vector item cap for `list`'s preview digest.
const LIST_MAX_ITEMS: usize = 5;

#[derive(Parser)]
#[command(name = "session-summary")]
#[command(about = "Quick summary of Claude Code session files", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List recent sessions with brief summaries (reads a bounded byte window
    /// from the end of each file, not the whole file)
    List {
        /// Number of recent sessions to show
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Only show projects directory (exclude archive)
        #[arg(long)]
        projects_only: bool,

        /// Maximum age in hours (filter by last modification time)
        #[arg(long, default_value = "12")]
        max_age_hours: u64,

        /// Ignore --max-age-hours and list the newest sessions regardless of age
        #[arg(long)]
        all: bool,

        /// Claude home containing projects/ and archive/ (defaults to ~/.claude)
        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,

        /// Emit machine-readable JSON instead of the human-readable listing
        #[arg(long)]
        json: bool,
    },
    /// Load full context from selected session (last segment + git hints)
    Load {
        /// Session JSONL path, exact UUID, or unique UUID prefix (at least 16 characters)
        session: String,

        /// Claude home containing projects/ and archive/ (defaults to ~/.claude)
        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,

        /// Emit machine-readable JSON instead of the human-readable report
        #[arg(long)]
        json: bool,

        /// Print a count of unrecognized root event types seen in the scanned
        /// window to stderr. Diagnostic only — never changes stdout.
        #[arg(long)]
        debug: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::List { limit, projects_only, max_age_hours, all, home, json } => {
            list_sessions(limit, !projects_only, max_age_hours, all, home, json)?;
        }
        Commands::Load { session, home, json, debug } => {
            let home = match home {
                Some(path) => path,
                None => dirs::home_dir()
                    .context("Failed to get home directory")?
                    .join(".claude"),
            };
            let path = resolve_session_path(&session, &home)?;
            load_session_context(&path, json, debug)?;
        }
    }

    Ok(())
}

// ============================================================================
// Path resolution and filesystem safety
// ============================================================================

#[derive(Debug)]
struct SessionRoot {
    lexical: PathBuf,
    canonical: PathBuf,
}

/// Resolve a load argument without allowing it to escape the selected Claude home.
fn resolve_session_path(session: &str, home: &Path) -> Result<PathBuf> {
    let roots = session_roots(home)?;
    let raw_path = Path::new(session);

    if looks_like_jsonl_path(session, raw_path) {
        return validate_session_path(raw_path, &roots);
    }

    if !is_uuid(session) && !is_uuid_prefix(session) {
        anyhow::bail!(
            "Session must be an exact JSONL path, an exact UUID, or a UUID prefix of at least 16 characters"
        );
    }

    let needle = session.to_ascii_lowercase();
    let exact = is_uuid(session);
    let mut matches = Vec::new();

    for root in &roots {
        collect_session_files(&root.lexical, &mut matches)?;
    }

    matches.retain(|path| {
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            return false;
        };
        if !is_uuid(stem) {
            return false;
        }
        if exact {
            stem.eq_ignore_ascii_case(session)
        } else {
            stem.to_ascii_lowercase().starts_with(&needle)
        }
    });

    match matches.len() {
        0 => anyhow::bail!("No Claude session matches identifier: {session}"),
        1 => validate_session_path(&matches[0], &roots),
        count => anyhow::bail!("Session identifier is ambiguous ({count} matches): {session}"),
    }
}

fn session_roots(home: &Path) -> Result<Vec<SessionRoot>> {
    let home = absolute_lexical(home)?;
    let mut roots = Vec::new();

    for directory in ["projects", "archive"] {
        let lexical = home.join(directory);
        let metadata = match fs::symlink_metadata(&lexical) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| {
                format!("Failed to inspect Claude session root: {}", lexical.display())
            }),
        };

        if is_symlink_or_reparse(&metadata) {
            anyhow::bail!("Claude session root must not be a symlink: {}", lexical.display());
        }
        if !metadata.is_dir() {
            anyhow::bail!("Claude session root is not a directory: {}", lexical.display());
        }

        let canonical = fs::canonicalize(&lexical).with_context(|| {
            format!("Failed to canonicalize Claude session root: {}", lexical.display())
        })?;
        roots.push(SessionRoot { lexical, canonical });
    }

    if roots.is_empty() {
        anyhow::bail!(
            "Claude session roots were not found under: {}",
            home.display()
        );
    }

    Ok(roots)
}

fn validate_session_path(path: &Path, roots: &[SessionRoot]) -> Result<PathBuf> {
    if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
        anyhow::bail!("Session path must name a .jsonl file: {}", path.display());
    }

    let lexical = absolute_lexical(path)?;
    let canonical = fs::canonicalize(&lexical)
        .with_context(|| format!("Session file not found: {}", lexical.display()))?;

    let root = roots.iter().find(|root| {
        lexical.starts_with(&root.lexical) && canonical.starts_with(&root.canonical)
    });
    let Some(root) = root else {
        anyhow::bail!(
            "Session path is outside the configured projects/archive roots: {}",
            lexical.display()
        );
    };

    reject_symlink_components(&lexical, &root.lexical)?;

    let metadata = fs::symlink_metadata(&lexical)
        .with_context(|| format!("Failed to inspect session file: {}", lexical.display()))?;
    if is_symlink_or_reparse(&metadata) {
        anyhow::bail!("Session path must not be a symlink: {}", lexical.display());
    }
    if !metadata.is_file() {
        anyhow::bail!("Session path is not a regular file: {}", lexical.display());
    }

    Ok(canonical)
}

fn reject_symlink_components(path: &Path, root: &Path) -> Result<()> {
    let relative = path.strip_prefix(root).context("Session path is outside its root")?;
    let mut current = root.to_path_buf();

    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("Failed to inspect session path: {}", current.display()))?;
        if is_symlink_or_reparse(&metadata) {
            anyhow::bail!("Session path must not contain symlinks: {}", current.display());
        }
    }

    Ok(())
}

fn is_symlink_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        has_windows_reparse_attribute(metadata.file_attributes())
    }

    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
fn has_windows_reparse_attribute(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Directory basenames that never hold selectable sessions: subagent
/// transcripts and their raw tool-result blobs live alongside a session's
/// own `<uuid>.jsonl` under `<uuid>/subagents/` and `<uuid>/tool-results/`.
/// Skipping them here means `load`'s UUID/prefix resolution can never select
/// a subagent transcript (whose filename is `agent-<hex>.jsonl`, not a UUID,
/// so it was already excluded downstream — this also avoids the wasted
/// recursion on sessions with many delegated subagents).
const NON_SESSION_DIRECTORIES: [&str; 2] = ["subagents", "tool-results"];

fn collect_session_files(root: &Path, sessions: &mut Vec<PathBuf>) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];

    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).with_context(|| {
            format!("Failed to read Claude session directory: {}", directory.display())
        })? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).with_context(|| {
                format!("Failed to inspect Claude session entry: {}", path.display())
            })?;

            if is_symlink_or_reparse(&metadata) {
                if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                    sessions.push(path);
                }
                continue;
            }
            if metadata.is_dir() {
                let is_non_session_dir = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| NON_SESSION_DIRECTORIES.contains(&name));
                if !is_non_session_dir {
                    pending.push(path);
                }
            } else if metadata.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            {
                sessions.push(path);
            }
        }
    }

    Ok(())
}

fn looks_like_jsonl_path(session: &str, path: &Path) -> bool {
    path.is_absolute()
        || path.extension().and_then(|value| value.to_str()) == Some("jsonl")
        || session.contains('/')
        || session.contains('\\')
        || session.starts_with('.')
}

fn is_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }

    value.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

fn is_uuid_prefix(value: &str) -> bool {
    if value.len() < 16 || value.len() >= 36 {
        return false;
    }

    value.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("Failed to get current directory")?
            .join(path)
    };
    let mut normalized = PathBuf::new();

    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    anyhow::bail!("Path escapes its filesystem root: {}", path.display());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }

    Ok(normalized)
}

// ============================================================================
// Byte-budgeted reads (no external `tail` process, no full-file scans)
// ============================================================================

/// Read up to `max_bytes` from the end of `path`, split into complete lines.
///
/// A single seek plus one bounded read — cost is proportional to `max_bytes`,
/// not to file size. If the seek lands mid-line, that partial leading line is
/// dropped (it is truncated data anyway; its start lies outside the budget).
/// Returns `(lines, truncated)`, where `truncated` is `true` when the file is
/// larger than `max_bytes` (earlier context exists that this call did not read).
fn read_tail_lines(path: &Path, max_bytes: u64) -> Result<(Vec<String>, bool)> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("Failed to open session file: {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("Failed to inspect session file: {}", path.display()))?
        .len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("Failed to seek session file: {}", path.display()))?;

    let mut buffer = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buffer)
        .with_context(|| format!("Failed to read session file: {}", path.display()))?;

    if start > 0 {
        if let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
            buffer.drain(..=index);
        } else {
            buffer.clear();
        }
    }

    let text = String::from_utf8_lossy(&buffer);
    let lines = text.lines().map(str::to_owned).collect();
    Ok((lines, start > 0))
}

/// Read up to `max_bytes` from the start of `path`, split into complete lines.
///
/// Used only as a fallback for title/first-prompt detection when the tail
/// window (which is read first, since it is far cheaper on a huge session)
/// carries neither.
fn read_head_lines(path: &Path, max_bytes: u64) -> Result<Vec<String>> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("Failed to open session file: {}", path.display()))?;
    let mut buffer = vec![0_u8; max_bytes as usize];
    let read = file
        .read(&mut buffer)
        .with_context(|| format!("Failed to read session file: {}", path.display()))?;
    buffer.truncate(read);

    // If the budget was fully consumed, drop a trailing partial line — its
    // continuation lies outside the budget.
    if read as u64 == max_bytes {
        if let Some(index) = buffer.iter().rposition(|byte| *byte == b'\n') {
            buffer.truncate(index);
        }
    }

    let text = String::from_utf8_lossy(&buffer);
    Ok(text.lines().map(str::to_owned).collect())
}

/// Parse each line as a [`SessionEvent`], silently skipping lines that fail
/// to deserialize (malformed JSON, truncated leading/trailing line, or a
/// partial line from a session still being written to).
fn parse_events(lines: &[String]) -> Vec<SessionEvent> {
    lines
        .iter()
        .filter_map(|line| serde_json::from_str::<SessionEvent>(line).ok())
        .collect()
}

// ============================================================================
// Topic detection (D3: prefer the provider's own title over any inferred label)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopicSource {
    CustomTitle,
    AiTitle,
    LastPrompt,
    FirstPrompt,
    None,
}

impl fmt::Display for TopicSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::CustomTitle => "custom_title",
            Self::AiTitle => "ai_title",
            Self::LastPrompt => "last_prompt",
            Self::FirstPrompt => "first_prompt",
            Self::None => "none",
        };
        f.write_str(label)
    }
}

fn last_custom_title(events: &[SessionEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| match event {
        SessionEvent::CustomTitle(title) => Some(title.custom_title.clone()),
        _ => None,
    })
}

fn last_ai_title(events: &[SessionEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| match event {
        SessionEvent::AiTitle(title) => Some(title.ai_title.clone()),
        _ => None,
    })
}

fn last_last_prompt(events: &[SessionEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| match event {
        SessionEvent::LastPrompt(prompt) => Some(prompt.last_prompt.clone()),
        _ => None,
    })
}

fn first_human_prompt(events: &[SessionEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        SessionEvent::User(user) => user.human_prompt_text().map(str::to_owned),
        _ => None,
    })
}

/// Pick the topic per the priority order documented on this module: the
/// tail window is checked first (cheap, and titles repeat through the file
/// so the tail almost always carries the latest one), then the head window
/// as a fallback for sessions whose tail window missed every repeat.
fn detect_topic(head: &[SessionEvent], tail: &[SessionEvent]) -> (String, TopicSource) {
    if let Some(title) = last_custom_title(tail) {
        return (title, TopicSource::CustomTitle);
    }
    if let Some(title) = last_ai_title(tail) {
        return (title, TopicSource::AiTitle);
    }
    if let Some(prompt) = last_last_prompt(tail) {
        return (prompt, TopicSource::LastPrompt);
    }
    if let Some(title) = last_custom_title(head) {
        return (title, TopicSource::CustomTitle);
    }
    if let Some(title) = last_ai_title(head) {
        return (title, TopicSource::AiTitle);
    }
    if let Some(prompt) = last_last_prompt(head) {
        return (prompt, TopicSource::LastPrompt);
    }
    if let Some(prompt) = first_human_prompt(head) {
        return (prompt, TopicSource::FirstPrompt);
    }
    if let Some(prompt) = first_human_prompt(tail) {
        return (prompt, TopicSource::FirstPrompt);
    }
    ("Empty session".to_string(), TopicSource::None)
}

// ============================================================================
// Digest extraction
// ============================================================================

#[derive(Debug, Clone, Default)]
struct SessionDigest {
    topic: String,
    topic_source: String,
    agent_tasks: Vec<String>,
    user_messages: Vec<String>,
    assistant_texts: Vec<String>,
    tool_operations: Vec<String>,
    bash_activities: Vec<String>,
    web_queries: Vec<String>,
    errors: Vec<String>,
    files: Vec<String>,
    git_branch: Option<String>,
    commit_hints: Vec<String>,
    truncated: bool,
    unknown_type_counts: Vec<(String, u64)>,
}

struct DigestLimits {
    tail_bytes: u64,
    head_bytes: u64,
    max_items: usize,
    count_unknown_types: bool,
}

const LIST_LIMITS: DigestLimits = DigestLimits {
    tail_bytes: LIST_TAIL_BYTES,
    head_bytes: HEAD_FALLBACK_BYTES,
    max_items: LIST_MAX_ITEMS,
    count_unknown_types: false,
};

fn load_limits(debug: bool) -> DigestLimits {
    DigestLimits {
        tail_bytes: LOAD_TAIL_BYTES,
        head_bytes: HEAD_FALLBACK_BYTES,
        max_items: LOAD_MAX_ITEMS,
        count_unknown_types: debug,
    }
}

fn push_capped(items: &mut Vec<String>, value: String, cap: usize) {
    if items.len() < cap {
        items.push(value);
    }
}

/// Build a verbatim-quote digest of `path`'s tail window.
fn build_digest(path: &Path, limits: &DigestLimits) -> Result<SessionDigest> {
    let (tail_lines, truncated) = read_tail_lines(path, limits.tail_bytes)?;
    let tail_events = parse_events(&tail_lines);
    let head_lines = read_head_lines(path, limits.head_bytes)?;
    let head_events = parse_events(&head_lines);

    let (topic, topic_source) = detect_topic(&head_events, &tail_events);

    let unknown_type_counts = if limits.count_unknown_types {
        count_unknown_root_types(&tail_lines)
    } else {
        Vec::new()
    };

    // "Last events" means after the last compaction boundary, when one
    // exists inside the scanned window.
    let boundary = tail_events.iter().rposition(|event| {
        matches!(event, SessionEvent::System(sys) if sys.is_compact_boundary())
    });
    let window = boundary.map_or(tail_events.as_slice(), |index| &tail_events[index + 1..]);

    let mut digest = SessionDigest {
        topic,
        topic_source: topic_source.to_string(),
        truncated,
        unknown_type_counts,
        ..SessionDigest::default()
    };
    let mut commit_hints = HashSet::new();
    let mut files = HashSet::new();

    for event in window {
        match event {
            SessionEvent::User(user) => {
                if let Some(branch) = &user.metadata.git_branch {
                    digest.git_branch = Some(branch.clone());
                }
                if let Some(text) = user.human_prompt_text() {
                    extract_commit_hints(text, &mut commit_hints);
                    push_capped(&mut digest.user_messages, text.to_string(), limits.max_items);
                }
            }
            SessionEvent::Assistant(assistant) => {
                for block in &assistant.message.content {
                    if let Some(text) = block.as_text() {
                        extract_commit_hints(text, &mut commit_hints);
                        push_capped(&mut digest.assistant_texts, text.to_string(), limits.max_items);
                    }
                    if let Some((_, name, input)) = block.as_tool_use() {
                        push_capped(
                            &mut digest.tool_operations,
                            describe_tool_use(name, input),
                            limits.max_items * 3,
                        );
                        record_tool_side_effects(name, input, limits, &mut digest, &mut files);
                    }
                }
            }
            SessionEvent::Progress(progress) => match &progress.data {
                ProgressData::AgentProgress(agent) => {
                    extract_commit_hints(&agent.prompt, &mut commit_hints);
                    push_capped(&mut digest.agent_tasks, agent.prompt.clone(), limits.max_items);
                }
                ProgressData::QueryUpdate(query) => {
                    push_capped(&mut digest.web_queries, query.query.clone(), limits.max_items);
                }
                _ => {}
            },
            SessionEvent::FileSnapshot(snapshot) => {
                for file_path in snapshot.snapshot.tracked_file_backups.keys() {
                    files.insert(normalize_path_separators(file_path));
                }
            }
            SessionEvent::System(sys) if sys.is_error() => {
                let message = sys
                    .error
                    .as_ref()
                    .map(|error| format!("{}: {}", error.error_type, error.message))
                    .or_else(|| sys.content.clone())
                    .unwrap_or_else(|| "unspecified system error".to_string());
                push_capped(&mut digest.errors, message, limits.max_items);
            }
            _ => {}
        }
    }

    digest.files = files.into_iter().collect();
    digest.files.sort();
    digest.commit_hints = commit_hints.into_iter().collect();
    digest.commit_hints.sort();

    Ok(digest)
}

fn record_tool_side_effects(
    name: &str,
    input: &JsonValue,
    limits: &DigestLimits,
    digest: &mut SessionDigest,
    files: &mut HashSet<String>,
) {
    match name {
        "Read" | "Write" | "Edit" | "NotebookEdit" => {
            if let Some(path) = input
                .get("file_path")
                .or_else(|| input.get("filePath"))
                .and_then(JsonValue::as_str)
            {
                files.insert(normalize_path_separators(path));
            }
        }
        "Bash" => {
            if let Some(command) = input.get("command").and_then(JsonValue::as_str) {
                if is_interesting_bash(command) {
                    push_capped(&mut digest.bash_activities, command.to_string(), limits.max_items);
                }
            }
        }
        "WebSearch" => {
            if let Some(query) = input.get("query").and_then(JsonValue::as_str) {
                push_capped(&mut digest.web_queries, query.to_string(), limits.max_items);
            }
        }
        _ => {}
    }
}

fn normalize_path_separators(path: &str) -> String {
    path.replace('\\', "/")
}

fn is_interesting_bash(command: &str) -> bool {
    let lowered = command.to_lowercase();
    [
        "cargo build",
        "cargo check",
        "cargo test",
        "npm install",
        "git commit",
        "pytest",
        "compiling",
    ]
    .iter()
    .any(|marker| lowered.contains(marker))
}

/// One-line description of a tool invocation including its most relevant
/// argument — the "tool calls with key args" the digest spec asks for.
fn describe_tool_use(name: &str, input: &JsonValue) -> String {
    let key_arg = match name {
        "Bash" => input.get("command").and_then(JsonValue::as_str),
        "Read" | "Write" | "Edit" | "NotebookEdit" => {
            input.get("file_path").or_else(|| input.get("filePath")).and_then(JsonValue::as_str)
        }
        "Grep" | "Glob" => input.get("pattern").and_then(JsonValue::as_str),
        "WebSearch" => input.get("query").and_then(JsonValue::as_str),
        "WebFetch" => input.get("url").and_then(JsonValue::as_str),
        "Task" | "Agent" => input
            .get("description")
            .and_then(JsonValue::as_str)
            .or_else(|| input.get("subagent_type").and_then(JsonValue::as_str)),
        _ => None,
    };

    match key_arg {
        Some(arg) => {
            let first_line = arg.lines().next().unwrap_or(arg);
            format!("{name}: {}", truncate(first_line, 100))
        }
        None => name.to_string(),
    }
}

/// Extract git commit hints (`feat(scope)`-style prefixes and path mentions)
/// from verbatim quoted text — human prompts and assistant text alike.
fn extract_commit_hints(text: &str, hints: &mut HashSet<String>) {
    let patterns = [
        r"feat\(([^)]+)\)",
        r"fix\(([^)]+)\)",
        r"refactor\(([^)]+)\)",
        r"chore\(([^)]+)\)",
        r"test\(([^)]+)\)",
    ];

    for pattern in &patterns {
        if let Ok(re) = Regex::new(pattern) {
            for cap in re.captures_iter(text) {
                if let Some(scope) = cap.get(1) {
                    hints.insert(scope.as_str().to_string());
                }
            }
        }
    }

    for word in text.split_whitespace() {
        if word.contains("v5/") || word.contains("connectors/") || word.contains("ui/") {
            hints.insert(word.to_string());
        }
    }
}

/// Root event `type` tags modeled by [`SessionEvent`]. Used only by the
/// `--debug` histogram to name types that fall into `SessionEvent::Unknown`
/// without re-deriving the tag list from serde internals.
const KNOWN_ROOT_TYPES: &[&str] = &[
    "user",
    "assistant",
    "progress",
    "system",
    "file-history-snapshot",
    "queue-operation",
    "summary",
    "attachment",
    "custom-title",
    "ai-title",
    "last-prompt",
    "bridge-session",
    "atis-latch",
    "mode",
    "permission-mode",
    "agent-name",
    "file-history-delta",
];

/// Count root `type` tags that do not match any [`SessionEvent`] variant,
/// for the `--debug` diagnostic. This never affects `load`'s stdout output.
fn count_unknown_root_types(lines: &[String]) -> Vec<(String, u64)> {
    let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for line in lines {
        let Ok(value) = serde_json::from_str::<JsonValue>(line) else {
            continue;
        };
        let Some(type_tag) = value.get("type").and_then(JsonValue::as_str) else {
            continue;
        };
        if !KNOWN_ROOT_TYPES.contains(&type_tag) {
            *counts.entry(type_tag.to_string()).or_default() += 1;
        }
    }
    let mut counts: Vec<(String, u64)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
}

// ============================================================================
// `list`
// ============================================================================

#[derive(Serialize)]
struct SessionListEntry {
    id: String,
    modified: Option<String>,
    size_bytes: u64,
    source: &'static str,
    topic: String,
    topic_source: String,
    tasks: Vec<String>,
    user_messages: Vec<String>,
    tool_operations: Vec<String>,
    bash_activities: Vec<String>,
    web_queries: Vec<String>,
}

#[derive(Serialize)]
struct SessionListReport {
    schema: &'static str,
    sessions: Vec<SessionListEntry>,
}

fn list_sessions(
    limit: usize,
    include_archived: bool,
    max_age_hours: u64,
    all: bool,
    home: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let home = match home {
        Some(path) => path,
        None => dirs::home_dir().context("Failed to get home directory")?.join(".claude"),
    };
    let projects_dir = home.join("projects");

    if !projects_dir.exists() {
        anyhow::bail!("Claude projects directory not found: {}", projects_dir.display());
    }

    let now = SystemTime::now();
    let cutoff_time = if all {
        SystemTime::UNIX_EPOCH
    } else {
        let max_age = std::time::Duration::from_secs(max_age_hours * 3600);
        now.checked_sub(max_age).unwrap_or(SystemTime::UNIX_EPOCH)
    };

    let mut sessions: Vec<(PathBuf, u64, Option<DateTime<Utc>>, &'static str)> = Vec::new();

    for entry in fs::read_dir(&projects_dir).with_context(|| {
        format!("Failed to read Claude projects directory: {}", projects_dir.display())
    })? {
        let entry = entry?;
        let project_path = entry.path();
        if !project_path.is_dir() {
            continue;
        }
        collect_top_level_sessions(&project_path, cutoff_time, "projects", &mut sessions)?;
    }

    if include_archived {
        let archive_dir = home.join("archive");
        if archive_dir.exists() {
            collect_top_level_sessions(&archive_dir, cutoff_time, "archive", &mut sessions)?;
        }
    }

    sessions.sort_by_key(|(_, _, modified, _)| std::cmp::Reverse(*modified));
    sessions.truncate(limit);

    if sessions.is_empty() {
        if json {
            let report = SessionListReport { schema: SCHEMA_LIST, sessions: Vec::new() };
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("{}", "Recent Sessions:".bold().bright_cyan());
            println!();
            let window = if all {
                "any age".to_string()
            } else {
                format!("the last {max_age_hours}h")
            };
            println!(
                "No Claude sessions found in {window} under {}. Try --max-age-hours <N> or --all.",
                projects_dir.display()
            );
        }
        return Ok(());
    }

    if json {
        let mut entries = Vec::with_capacity(sessions.len());
        for (path, size, modified, source) in &sessions {
            entries.push(build_list_entry(path, *size, *modified, source)?);
        }
        let report = SessionListReport { schema: SCHEMA_LIST, sessions: entries };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("{}", "Recent Sessions:".bold().bright_cyan());
    println!();

    for (i, (path, size, modified, source)) in sessions.iter().enumerate() {
        let digest = build_digest(path, &LIST_LIMITS).unwrap_or_default();

        let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");

        println!("{}", format!("{}. {}", i + 1, session_id).bright_yellow());

        if let Some(mod_time) = modified {
            print!("   {} | ", mod_time.format("%b %d %H:%M"));
        }
        print!("{} | ", format_size(*size));
        print!("[{}] | ", source.dimmed());
        println!("{}", truncate(&digest.topic, 80).bright_green());

        if !digest.agent_tasks.is_empty() {
            let tasks_preview: Vec<String> =
                digest.agent_tasks.iter().map(|t| truncate(t, 60)).collect();
            println!("   📋 Tasks: {}", tasks_preview.join(" → ").dimmed());
        }

        if !digest.user_messages.is_empty() {
            let msg_preview: Vec<String> =
                digest.user_messages.iter().take(2).map(|m| truncate(m, 50)).collect();
            println!("   💬 User: {}", msg_preview.join(" → ").dimmed());
        }

        if !digest.tool_operations.is_empty() {
            let tools_preview: Vec<String> =
                digest.tool_operations.iter().take(5).map(|t| truncate(t, 80)).collect();
            println!("   🔧 Tools: {}", tools_preview.join(", ").dimmed());
        }

        if !digest.bash_activities.is_empty() {
            let bash_preview: Vec<String> = digest
                .bash_activities
                .iter()
                .map(|cmd| truncate(cmd.lines().next().unwrap_or(cmd).trim(), 100))
                .collect();
            println!("   ⚙️  Bash: {}", bash_preview.join("; ").dimmed());
        }

        if !digest.web_queries.is_empty() {
            println!("   🔍 Search: {}", digest.web_queries.join(", ").dimmed());
        }

        println!();
    }

    println!();
    println!("{}", "To load a session, use:".bold());
    for (i, (path, _, _, _)) in sessions.iter().enumerate() {
        println!("  {}. session-summary.exe load \"{}\"", i + 1, path.display());
    }

    Ok(())
}

fn collect_top_level_sessions(
    dir: &Path,
    cutoff_time: SystemTime,
    source: &'static str,
    sessions: &mut Vec<(PathBuf, u64, Option<DateTime<Utc>>, &'static str)>,
) -> Result<()> {
    for entry in fs::read_dir(dir)
        .with_context(|| format!("Failed to read Claude session directory: {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }

        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        let Ok(modified_time) = metadata.modified() else {
            continue;
        };
        if modified_time < cutoff_time {
            continue;
        }

        let modified = modified_time
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .and_then(|duration| DateTime::from_timestamp(duration.as_secs() as i64, 0));

        sessions.push((path, metadata.len(), modified, source));
    }

    Ok(())
}

fn build_list_entry(
    path: &Path,
    size: u64,
    modified: Option<DateTime<Utc>>,
    source: &'static str,
) -> Result<SessionListEntry> {
    let digest = build_digest(path, &LIST_LIMITS)?;
    let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string();

    Ok(SessionListEntry {
        id,
        modified: modified.map(|value| value.to_rfc3339()),
        size_bytes: size,
        source,
        topic: digest.topic,
        topic_source: digest.topic_source,
        tasks: digest.agent_tasks,
        user_messages: digest.user_messages,
        tool_operations: digest.tool_operations,
        bash_activities: digest.bash_activities,
        web_queries: digest.web_queries,
    })
}

// ============================================================================
// `load`
// ============================================================================

#[derive(Serialize)]
struct SessionLoadReport {
    schema: &'static str,
    session_id: String,
    date: Option<String>,
    size_bytes: u64,
    topic: String,
    topic_source: String,
    agent_tasks: Vec<String>,
    user_messages: Vec<String>,
    assistant_texts: Vec<String>,
    tool_operations: Vec<String>,
    bash_activities: Vec<String>,
    web_queries: Vec<String>,
    errors: Vec<String>,
    files: Vec<String>,
    git_branch: Option<String>,
    commit_hints: Vec<String>,
    truncated: bool,
}

fn load_session_context(path: &Path, json: bool, debug: bool) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("Session file not found: {}", path.display());
    }

    let metadata = fs::metadata(path)?;
    let size_bytes = metadata.len();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|st| st.duration_since(SystemTime::UNIX_EPOCH).ok())
        .and_then(|d| DateTime::from_timestamp(d.as_secs() as i64, 0));

    let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    let digest = build_digest(path, &load_limits(debug))?;

    if debug {
        if digest.unknown_type_counts.is_empty() {
            eprintln!("[debug] no unrecognized root event types in the scanned window");
        } else {
            eprintln!("[debug] unrecognized root event types in the scanned window:");
            for (type_name, count) in &digest.unknown_type_counts {
                eprintln!("[debug]   {type_name}: {count}");
            }
        }
    }

    if json {
        let report = SessionLoadReport {
            schema: SCHEMA_LOAD,
            session_id: session_id.to_string(),
            date: modified.map(|value| value.to_rfc3339()),
            size_bytes,
            topic: digest.topic,
            topic_source: digest.topic_source,
            agent_tasks: digest.agent_tasks,
            user_messages: digest.user_messages,
            assistant_texts: digest.assistant_texts,
            tool_operations: digest.tool_operations,
            bash_activities: digest.bash_activities,
            web_queries: digest.web_queries,
            errors: digest.errors,
            files: digest.files,
            git_branch: digest.git_branch,
            commit_hints: digest.commit_hints,
            truncated: digest.truncated,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("{}", "═══════════════════════════════════════".bright_cyan());
    println!("{} {}", "Session:".bold(), session_id.bright_yellow());
    println!("{}", "═══════════════════════════════════════".bright_cyan());

    if let Some(mod_time) = modified {
        println!("{} {}", "Date:".bold(), mod_time.format("%Y-%m-%d %H:%M:%S"));
    }

    println!("{} {}", "Size:".bold(), format_size(size_bytes));
    println!("{} {}", "Topic:".bold(), digest.topic.bright_green());

    if !digest.agent_tasks.is_empty() {
        println!(
            "\n{} 📋 {}",
            "Agent Tasks".bold(),
            format!("({} tasks)", digest.agent_tasks.len()).dimmed()
        );
        print_numbered(&digest.agent_tasks, 10, 200);
    }

    if !digest.user_messages.is_empty() {
        println!(
            "\n{} 💬 {}",
            "User Messages".bold(),
            format!("({} messages)", digest.user_messages.len()).dimmed()
        );
        print_numbered(&digest.user_messages, 10, 150);
    }

    if !digest.assistant_texts.is_empty() {
        println!(
            "\n{} 🤖 {}",
            "Assistant Texts".bold(),
            format!("({} texts)", digest.assistant_texts.len()).dimmed()
        );
        print_numbered(&digest.assistant_texts, 10, 200);
    }

    if !digest.tool_operations.is_empty() {
        println!(
            "\n{} 🔧 {}",
            "Tool Operations".bold(),
            format!("({} operations)", digest.tool_operations.len()).dimmed()
        );
        print_numbered(&digest.tool_operations, 15, 200);
    }

    if !digest.bash_activities.is_empty() {
        println!(
            "\n{} ⚙️  {}",
            "Bash Activities".bold(),
            format!("({} commands)", digest.bash_activities.len()).dimmed()
        );
        for (i, cmd) in digest.bash_activities.iter().take(5).enumerate() {
            let first_line = cmd.lines().next().unwrap_or("");
            println!("  {}. {}", i + 1, truncate(first_line, 150).bright_white());
        }
        if digest.bash_activities.len() > 5 {
            println!("  {} ({} more)", "...".dimmed(), digest.bash_activities.len() - 5);
        }
    }

    if !digest.web_queries.is_empty() {
        println!(
            "\n{} 🔍 {}",
            "Web Searches".bold(),
            format!("({} queries)", digest.web_queries.len()).dimmed()
        );
        print_numbered(&digest.web_queries, 10, 200);
    }

    if !digest.errors.is_empty() {
        println!(
            "\n{} 🚨 {}",
            "Errors".bold(),
            format!("({} errors)", digest.errors.len()).dimmed()
        );
        print_numbered(&digest.errors, 10, 300);
    }

    if !digest.files.is_empty() {
        println!(
            "\n{} 📁 {}",
            "Files Touched".bold(),
            format!("({} files)", digest.files.len()).dimmed()
        );
        for (i, file) in digest.files.iter().take(10).enumerate() {
            println!("  {}. {}", i + 1, shorten_path(file).bright_white());
        }
        if digest.files.len() > 10 {
            println!("  {} ({} more files)", "...".dimmed(), digest.files.len() - 10);
        }
    }

    if let Some(ref branch) = digest.git_branch {
        println!("\n{} {}", "Git Branch:".bold(), branch.bright_cyan());
    }

    if !digest.commit_hints.is_empty() {
        println!("\n{} 🔎", "Git Commit Hints (for git log search):".bold());
        for hint in &digest.commit_hints {
            println!("  - {}", hint.dimmed());
        }
    }

    if digest.truncated {
        println!(
            "\n{}",
            "(session is larger than the read window — earlier context was not scanned)".dimmed()
        );
    }

    println!();

    Ok(())
}

fn print_numbered(items: &[String], limit: usize, max_chars: usize) {
    for (i, item) in items.iter().take(limit).enumerate() {
        println!("  {}. {}", i + 1, truncate(item, max_chars).bright_white());
    }
    if items.len() > limit {
        println!("  {} ({} more)", "...".dimmed(), items.len() - limit);
    }
}

// ============================================================================
// Formatting helpers
// ============================================================================

/// Shorten path for display
fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > 2 {
        format!(".../{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        path.to_string()
    }
}

/// Truncate string to max length (UTF-8 safe)
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        let mut boundary = max_len;
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!("{}...", &s[..boundary])
    } else {
        s.to_string()
    }
}

/// Format file size in human-readable format
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHome {
        path: PathBuf,
    }

    impl TestHome {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "session-summary-tests-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(path.join("projects").join("project-a"))
                .expect("create projects root");
            fs::create_dir_all(path.join("archive")).expect("create archive root");
            Self { path }
        }

        fn session(&self, relative: &str, id: &str) -> PathBuf {
            let directory = self.path.join(relative);
            fs::create_dir_all(&directory).expect("create session directory");
            let path = directory.join(format!("{id}.jsonl"));
            fs::write(&path, "{}\n").expect("write session fixture");
            path
        }

        fn session_with_content(&self, relative: &str, id: &str, content: &str) -> PathBuf {
            let directory = self.path.join(relative);
            fs::create_dir_all(&directory).expect("create session directory");
            let path = directory.join(format!("{id}.jsonl"));
            fs::write(&path, content).expect("write session fixture");
            path
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn parses_home_after_load_identifier() {
        let cli = Cli::try_parse_from([
            "session-summary",
            "load",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "--home",
            "C:\\fixture\\.claude",
        ])
        .expect("parse load command");

        let Commands::Load { session, home, json, debug } = cli.command else {
            panic!("expected load command");
        };
        assert_eq!(session, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        assert_eq!(home, Some(PathBuf::from("C:\\fixture\\.claude")));
        assert!(!json);
        assert!(!debug);
    }

    #[test]
    fn parses_list_with_home_all_and_json() {
        let cli = Cli::try_parse_from([
            "session-summary",
            "list",
            "--home",
            "C:\\fixture\\.claude",
            "--all",
            "--json",
        ])
        .expect("parse list command");

        let Commands::List { home, all, json, .. } = cli.command else {
            panic!("expected list command");
        };
        assert_eq!(home, Some(PathBuf::from("C:\\fixture\\.claude")));
        assert!(all);
        assert!(json);
    }

    #[cfg(windows)]
    #[test]
    fn recognizes_windows_reparse_attribute() {
        assert!(has_windows_reparse_attribute(0x400));
        assert!(has_windows_reparse_attribute(0x420));
        assert!(!has_windows_reparse_attribute(0x20));
    }

    #[test]
    fn resolves_existing_path_exact_uuid_and_unique_prefix() {
        let home = TestHome::new();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let path = home.session("projects/project-a", id);
        let expected = fs::canonicalize(&path).expect("canonical fixture path");

        assert_eq!(
            resolve_session_path(path.to_str().expect("UTF-8 path"), &home.path)
                .expect("resolve exact path"),
            expected
        );
        assert_eq!(
            resolve_session_path(id, &home.path).expect("resolve exact UUID"),
            expected
        );
        assert_eq!(
            resolve_session_path("aaaaaaaa-aaaa-4aaa-8aaa-a", &home.path)
                .expect("resolve unique prefix"),
            expected
        );
    }

    #[test]
    fn rejects_short_and_ambiguous_prefixes() {
        let home = TestHome::new();
        home.session(
            "projects/project-a",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        );
        home.session("archive", "aaaaaaaa-aaaa-4aaa-8aaa-bbbbbbbbbbbb");

        let short = resolve_session_path("aaaaaaaa-aaaa-4", &home.path)
            .expect_err("short prefix must fail")
            .to_string();
        assert!(short.contains("at least 16"), "unexpected error: {short}");

        let ambiguous = resolve_session_path("aaaaaaaa-aaaa-4a", &home.path)
            .expect_err("ambiguous prefix must fail")
            .to_string();
        assert!(ambiguous.contains("ambiguous"), "unexpected error: {ambiguous}");
    }

    #[test]
    fn rejects_outside_and_nonregular_paths() {
        let home = TestHome::new();
        let outside = home.path.with_extension("outside.jsonl");
        fs::write(&outside, "{}\n").expect("write outside fixture");
        let nonregular = home.path.join("projects").join("directory.jsonl");
        fs::create_dir(&nonregular).expect("create nonregular fixture");

        let outside_error = resolve_session_path(
            outside.to_str().expect("UTF-8 outside path"),
            &home.path,
        )
        .expect_err("outside path must fail")
        .to_string();
        assert!(
            outside_error.contains("outside"),
            "unexpected error: {outside_error}"
        );

        let nonregular_error = resolve_session_path(
            nonregular.to_str().expect("UTF-8 nonregular path"),
            &home.path,
        )
        .expect_err("nonregular path must fail")
        .to_string();
        assert!(
            nonregular_error.contains("not a regular file"),
            "unexpected error: {nonregular_error}"
        );

        fs::remove_file(outside).expect("remove outside fixture");
    }

    #[test]
    fn rejects_symlink_session_path() {
        let home = TestHome::new();
        let target = home.session(
            "projects/project-a",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        );
        let link = home
            .path
            .join("projects")
            .join("project-a")
            .join("cccccccc-cccc-4ccc-8ccc-cccccccccccc.jsonl");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).expect("create session symlink");

        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_file(&target, &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(1314)
            {
                return;
            }
            panic!("create session symlink: {error}");
        }

        let error = resolve_session_path(link.to_str().expect("UTF-8 link path"), &home.path)
            .expect_err("symlink must fail")
            .to_string();
        assert!(
            error.contains("symlink"),
            "unexpected symlink error: {error}"
        );
    }

    #[test]
    fn subagent_transcripts_are_never_selected_as_load_targets() {
        let home = TestHome::new();
        let session_id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        home.session("projects/project-a", session_id);
        // A subagent transcript directory sibling to the session file, named
        // after the session UUID, holding a non-UUID-stemmed jsonl.
        let subagents_dir = home
            .path
            .join("projects")
            .join("project-a")
            .join(session_id)
            .join("subagents");
        fs::create_dir_all(&subagents_dir).expect("create subagents dir");
        fs::write(subagents_dir.join("agent-deadbeef.jsonl"), "{}\n")
            .expect("write subagent fixture");

        let mut matches = Vec::new();
        let roots = session_roots(&home.path).expect("session roots");
        for root in &roots {
            collect_session_files(&root.lexical, &mut matches).expect("collect session files");
        }
        assert!(
            matches.iter().all(|path| path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(is_uuid)),
            "collect_session_files must never surface a non-UUID-stemmed subagent transcript: {matches:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_junction_component() {
        let home = TestHome::new();
        let id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        let target = home.path.join("projects").join("real-project");
        fs::create_dir_all(&target).expect("create junction target");
        fs::write(target.join(format!("{id}.jsonl")), "{}\n")
            .expect("write junction session fixture");
        let junction = home.path.join("projects").join("linked-project");
        let output = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .expect("invoke mklink");
        assert!(
            output.status.success(),
            "mklink failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let linked_session = junction.join(format!("{id}.jsonl"));
        let error = resolve_session_path(
            linked_session.to_str().expect("UTF-8 junction path"),
            &home.path,
        )
        .expect_err("junction component must fail")
        .to_string();
        assert!(
            error.contains("symlink"),
            "unexpected junction error: {error}"
        );

        fs::remove_dir(&junction).expect("remove junction fixture");
    }

    // ------------------------------------------------------------------
    // Byte-budgeted reads
    // ------------------------------------------------------------------

    #[test]
    fn read_tail_lines_returns_whole_file_when_under_budget() {
        let home = TestHome::new();
        let path = home.session_with_content(
            "projects/project-a",
            "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee",
            "line-one\nline-two\nline-three\n",
        );
        let (lines, truncated) = read_tail_lines(&path, 4096).expect("read tail");
        assert_eq!(lines, vec!["line-one", "line-two", "line-three"]);
        assert!(!truncated);
    }

    #[test]
    fn read_tail_lines_drops_partial_leading_line_and_reports_truncated() {
        let home = TestHome::new();
        // Each line is 10 bytes ("lineNNN\n"). A budget smaller than the
        // whole file must land the seek mid-line at least once.
        let mut content = String::new();
        for index in 0..50 {
            content.push_str(&format!("line{index:03}\n"));
        }
        let path = home.session_with_content(
            "projects/project-a",
            "ffffffff-ffff-4fff-8fff-ffffffffffff",
            &content,
        );
        let (lines, truncated) = read_tail_lines(&path, 25).expect("read tail");
        assert!(truncated);
        // No partial ("torn") line survives, and every surviving line is a
        // real, complete, unbroken record from the original content.
        for line in &lines {
            assert!(content.lines().any(|full| full == line), "unexpected partial line: {line}");
        }
        assert_eq!(lines.last().map(String::as_str), Some("line049"));
    }

    #[test]
    fn read_head_lines_drops_partial_trailing_line() {
        let home = TestHome::new();
        let path = home.session_with_content(
            "projects/project-a",
            "11111111-1111-4111-8111-111111111111",
            "line-one\nline-two\nline-three\n",
        );
        // Budget lands inside "line-two".
        let lines = read_head_lines(&path, 12).expect("read head");
        assert_eq!(lines, vec!["line-one"]);
    }

    // ------------------------------------------------------------------
    // Topic detection (D3)
    // ------------------------------------------------------------------

    fn event(json: &str) -> SessionEvent {
        serde_json::from_str(json).expect("fixture event must parse")
    }

    #[test]
    fn detect_topic_prefers_custom_title_over_everything() {
        let tail = vec![
            event(r#"{"type":"last-prompt","lastPrompt":"placeholder prompt","sessionId":"s"}"#),
            event(r#"{"type":"ai-title","aiTitle":"placeholder ai title","sessionId":"s"}"#),
            event(r#"{"type":"custom-title","customTitle":"placeholder custom title","sessionId":"s"}"#),
        ];
        let (topic, source) = detect_topic(&[], &tail);
        assert_eq!(topic, "placeholder custom title");
        assert_eq!(source.to_string(), "custom_title");
    }

    #[test]
    fn detect_topic_falls_back_to_ai_title_then_last_prompt() {
        let ai_only = vec![event(
            r#"{"type":"ai-title","aiTitle":"placeholder ai title","sessionId":"s"}"#,
        )];
        assert_eq!(detect_topic(&[], &ai_only).0, "placeholder ai title");

        let last_prompt_only = vec![event(
            r#"{"type":"last-prompt","lastPrompt":"placeholder prompt","sessionId":"s"}"#,
        )];
        let (topic, source) = detect_topic(&[], &last_prompt_only);
        assert_eq!(topic, "placeholder prompt");
        assert_eq!(source.to_string(), "last_prompt");
    }

    #[test]
    fn detect_topic_falls_back_to_head_window_then_first_human_prompt() {
        let head = vec![event(
            r#"{"type":"custom-title","customTitle":"placeholder head title","sessionId":"s"}"#,
        )];
        let (topic, source) = detect_topic(&head, &[]);
        assert_eq!(topic, "placeholder head title");
        assert_eq!(source.to_string(), "custom_title");

        let head_prompt_only = vec![event(
            r#"{"type":"user","uuid":"u","sessionId":"s","timestamp":"2024-01-01T00:00:00Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"placeholder first prompt"}}"#,
        )];
        let (topic, source) = detect_topic(&head_prompt_only, &[]);
        assert_eq!(topic, "placeholder first prompt");
        assert_eq!(source.to_string(), "first_prompt");
    }

    #[test]
    fn detect_topic_none_when_nothing_present() {
        let (topic, source) = detect_topic(&[], &[]);
        assert_eq!(topic, "Empty session");
        assert_eq!(source.to_string(), "none");
    }

    #[test]
    fn detect_topic_takes_the_last_custom_title_when_it_repeats() {
        let tail = vec![
            event(r#"{"type":"custom-title","customTitle":"placeholder first","sessionId":"s"}"#),
            event(r#"{"type":"custom-title","customTitle":"placeholder second","sessionId":"s"}"#),
        ];
        assert_eq!(detect_topic(&[], &tail).0, "placeholder second");
    }

    // ------------------------------------------------------------------
    // Tool-call key-argument descriptions
    // ------------------------------------------------------------------

    #[test]
    fn describe_tool_use_extracts_key_arguments() {
        assert_eq!(
            describe_tool_use("Bash", &serde_json::json!({"command": "cargo build --release"})),
            "Bash: cargo build --release"
        );
        assert_eq!(
            describe_tool_use("Read", &serde_json::json!({"file_path": "/work/src/lib.rs"})),
            "Read: /work/src/lib.rs"
        );
        assert_eq!(
            describe_tool_use("Grep", &serde_json::json!({"pattern": "fn main"})),
            "Grep: fn main"
        );
        assert_eq!(describe_tool_use("TodoWrite", &serde_json::json!({})), "TodoWrite");
    }

    // ------------------------------------------------------------------
    // Digest extraction end to end (fixture built from real event shapes,
    // content redacted to placeholders)
    // ------------------------------------------------------------------

    #[test]
    fn build_digest_produces_verbatim_quotes_and_skips_injections() {
        let home = TestHome::new();
        let lines = [
            r#"{"type":"custom-title","customTitle":"placeholder session title","sessionId":"s"}"#.to_string(),
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","timestamp":"2024-01-01T00:00:00Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"placeholder human prompt"}}"#.to_string(),
            r#"{"type":"user","uuid":"u2","parentUuid":"u1","sessionId":"s","timestamp":"2024-01-01T00:00:01Z","isSidechain":false,"userType":"external","isMeta":true,"cwd":"/work","message":{"role":"user","content":"<local-command-caveat>placeholder caveat</local-command-caveat>"}}"#.to_string(),
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"s","timestamp":"2024-01-01T00:00:02Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"placeholder assistant reply"},{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"cargo build --release"}}]}}"#.to_string(),
            r#"{"type":"system","subtype":"error","uuid":"sys1","parentUuid":"a1","sessionId":"s","timestamp":"2024-01-01T00:00:03Z","isSidechain":false,"cwd":"/work","level":"error","content":"placeholder error text","error":{"type":"overloaded_error","message":"placeholder overload"}}"#.to_string(),
        ];
        let path = home.session_with_content(
            "projects/project-a",
            "22222222-2222-4222-8222-222222222222",
            &lines.join("\n"),
        );

        let digest = build_digest(&path, &load_limits(false)).expect("build digest");
        assert_eq!(digest.topic, "placeholder session title");
        assert_eq!(digest.topic_source, "custom_title");
        assert_eq!(digest.user_messages, vec!["placeholder human prompt".to_string()]);
        assert_eq!(digest.assistant_texts, vec!["placeholder assistant reply".to_string()]);
        assert_eq!(digest.tool_operations, vec!["Bash: cargo build --release".to_string()]);
        assert_eq!(digest.errors, vec!["overloaded_error: placeholder overload".to_string()]);
    }

    #[test]
    fn build_digest_windows_to_events_after_the_last_compact_boundary() {
        let home = TestHome::new();
        let lines = [
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","timestamp":"2024-01-01T00:00:00Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"placeholder before boundary"}}"#.to_string(),
            r#"{"type":"system","subtype":"compact_boundary","uuid":"boundary1","parentUuid":null,"sessionId":"s","timestamp":"2024-01-01T00:00:01Z","isSidechain":false,"cwd":"/work","compactMetadata":{"trigger":"auto","preTokens":1000}}"#.to_string(),
            r#"{"type":"user","uuid":"u2","parentUuid":"boundary1","sessionId":"s","timestamp":"2024-01-01T00:00:02Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"placeholder after boundary"}}"#.to_string(),
        ];
        let path = home.session_with_content(
            "projects/project-a",
            "33333333-3333-4333-8333-333333333333",
            &lines.join("\n"),
        );

        let digest = build_digest(&path, &load_limits(false)).expect("build digest");
        assert_eq!(digest.user_messages, vec!["placeholder after boundary".to_string()]);
    }

    #[test]
    fn count_unknown_root_types_ignores_modeled_types() {
        let lines = vec![
            r#"{"type":"user","message":{"role":"user","content":"x"}}"#.to_string(),
            r#"{"type":"some-future-event"}"#.to_string(),
            r#"{"type":"some-future-event"}"#.to_string(),
            r#"{"type":"another-future-event"}"#.to_string(),
        ];
        let counts = count_unknown_root_types(&lines);
        assert_eq!(
            counts,
            vec![
                ("some-future-event".to_string(), 2),
                ("another-future-event".to_string(), 1),
            ]
        );
    }
}
