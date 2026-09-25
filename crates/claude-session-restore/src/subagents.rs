//! Subagent and background-Bash-task tracking (spec C.6, plan sections 7/9).
//!
//! Walks the raw JSON of each scanned line (not the typed [`SessionEvent`]
//! model) because the fields this section needs — a launch's synchronous
//! `toolUseResult` (`agentId`, `status`), and `<task-notification>` tags —
//! do not fit the typed model: the async-launch `toolUseResult` carries no
//! `type` tag at all (unlike every modeled [`claude_session_restore::transcript::events::ToolUseResult`]
//! variant), and a task notification's tags are free text, not a JSON shape.
//!
//! [`scan`] walks a bounded window (`load`, addressed by [`crate::io::RawLine`]);
//! [`scan_full`] streams the whole file (`agents`/`agent`, spec wave-2) —
//! both share the same per-line ingestion so a subagent launched outside
//! `load`'s tail window is still found by the full-file commands.

use crate::format::truncate;
use crate::io::{self, RawLine};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const EXCERPT_CHARS: usize = 300;

/// Final status of a tracked subagent or background task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentStatus {
    /// No completion notification seen in the scanned window.
    Running,
    Completed,
    Failed,
    Killed,
    /// Any other `<status>` value a notification carried, verbatim.
    Other(String),
}

impl SubagentStatus {
    fn from_notification(raw: &str) -> Self {
        match raw {
            "completed" => Self::Completed,
            "failed" | "error" => Self::Failed,
            "killed" => Self::Killed,
            other => Self::Other(other.to_string()),
        }
    }

    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Running => "running".to_string(),
            Self::Completed => "completed".to_string(),
            Self::Failed => "failed".to_string(),
            Self::Killed => "killed".to_string(),
            Self::Other(raw) => raw.clone(),
        }
    }
}

/// One delegated subagent (`Agent`/`Task` tool use), ready for display.
#[derive(Debug, Clone)]
pub struct SubagentRecord {
    /// Byte offset of the launching `tool_use` line — the handle for
    /// `show`, independent of the `agentId`/`tool-use-id` address used by
    /// `agent`.
    pub offset: u64,
    pub tool_use_id: String,
    pub agent_id: Option<String>,
    pub description: Option<String>,
    pub agent_type: Option<String>,
    /// The task brief passed to the agent (`input.prompt`) — shown in full
    /// by the `agent` wave-2 command.
    pub prompt: Option<String>,
    pub run_in_background: bool,
    pub launched_at: DateTime<Utc>,
    pub status: SubagentStatus,
    pub excerpt: Option<String>,
}

impl SubagentRecord {
    /// A short display id: the agent id when known (already short), else a
    /// truncated tool-use id.
    #[must_use]
    pub fn short_id(&self) -> String {
        match &self.agent_id {
            Some(id) => id.clone(),
            None => {
                let id = self.tool_use_id.trim_start_matches("toolu_");
                truncate(id, 12)
            }
        }
    }

    /// Whether `needle` (an agent-id prefix, a full tool-use id, or a short
    /// display id) addresses this record — the lookup `agent` uses.
    #[must_use]
    pub fn matches_id(&self, needle: &str) -> bool {
        if let Some(agent_id) = &self.agent_id {
            if agent_id == needle || agent_id.starts_with(needle) {
                return true;
            }
        }
        self.tool_use_id == needle || self.short_id() == needle
    }
}

/// One background `Bash` launch (`run_in_background: true`), ready for the
/// `agent <session> <task-id>` lookup.
#[derive(Debug, Clone)]
pub struct BashTaskRecord {
    /// Byte offset of the launching `tool_use` line.
    pub offset: u64,
    pub tool_use_id: String,
    /// The background task id from the launch's own tool result
    /// (`background with ID: <id>`), when the launch reached that point.
    pub task_id: Option<String>,
    pub command: Option<String>,
    pub description: Option<String>,
    pub launched_at: DateTime<Utc>,
    pub status: SubagentStatus,
    pub excerpt: Option<String>,
}

impl BashTaskRecord {
    /// Whether `needle` (a task id, a tool-use id, or a short id prefix)
    /// addresses this record.
    #[must_use]
    pub fn matches_id(&self, needle: &str) -> bool {
        if let Some(task_id) = &self.task_id {
            if task_id == needle || task_id.starts_with(needle) {
                return true;
            }
        }
        self.tool_use_id == needle || truncate(self.tool_use_id.trim_start_matches("toolu_"), 12) == needle
    }
}

/// Background `Bash` calls (`run_in_background: true`), summarized as one
/// line rather than individual records for `load`, plus the full per-task
/// list `agent` searches by id.
#[derive(Debug, Clone, Default)]
pub struct BackgroundBashTasks {
    pub launched: usize,
    pub still_running: Vec<String>,
    pub records: Vec<BashTaskRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchKind {
    Subagent,
    Bash,
}

#[derive(Debug, Clone)]
struct Launch {
    offset: u64,
    kind: LaunchKind,
    timestamp: DateTime<Utc>,
    description: Option<String>,
    subagent_type: Option<String>,
    prompt: Option<String>,
    command: Option<String>,
    run_in_background: bool,
    agent_id: Option<String>,
    sync_excerpt: Option<String>,
    background_task_id: Option<String>,
    notification_status: Option<String>,
    notification_excerpt: Option<String>,
}

impl Launch {
    fn new(offset: u64, kind: LaunchKind, timestamp: DateTime<Utc>, input: &JsonValue) -> Self {
        Self {
            offset,
            kind,
            timestamp,
            description: input.get("description").and_then(JsonValue::as_str).map(str::to_string),
            subagent_type: input.get("subagent_type").and_then(JsonValue::as_str).map(str::to_string),
            prompt: input.get("prompt").and_then(JsonValue::as_str).map(str::to_string),
            command: input.get("command").and_then(JsonValue::as_str).map(str::to_string),
            run_in_background: input.get("run_in_background").and_then(JsonValue::as_bool).unwrap_or(false),
            agent_id: None,
            sync_excerpt: None,
            background_task_id: None,
            notification_status: None,
            notification_excerpt: None,
        }
    }
}

fn parse_timestamp(value: &JsonValue) -> Option<DateTime<Utc>> {
    value.get("timestamp").and_then(JsonValue::as_str).and_then(|raw| {
        DateTime::parse_from_rfc3339(raw).ok().map(|dt| dt.with_timezone(&Utc))
    })
}

/// Extract the text between `<tag>` and `</tag>`, trimmed. Mirrors
/// `UserEvent`'s private slash-command tag extractor, but over the
/// dash-named task-notification tags instead.
fn extract_tag<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&close)?;
    Some(text[start..end].trim())
}

fn background_task_id_from_bash_result(text: &str) -> Option<String> {
    let marker = "background with ID: ";
    let start = text.find(marker)? + marker.len();
    let rest = &text[start..];
    let end = rest.find(['.', '\n', ' ']).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Ingest one already-parsed JSON line into `launches`/`order` — the shared
/// core of [`scan`] (bounded window) and [`scan_full`] (whole-file stream).
fn ingest_line(offset: u64, value: &JsonValue, launches: &mut HashMap<String, Launch>, order: &mut Vec<String>) {
    let Some(root_type) = value.get("type").and_then(JsonValue::as_str) else { return };

    match root_type {
        "assistant" => {
            if value.get("isSidechain").and_then(JsonValue::as_bool).unwrap_or(false) {
                return;
            }
            let Some(timestamp) = parse_timestamp(value) else { return };
            let Some(content) = value.pointer("/message/content").and_then(JsonValue::as_array) else { return };
            for block in content {
                if block.get("type").and_then(JsonValue::as_str) != Some("tool_use") {
                    continue;
                }
                let Some(id) = block.get("id").and_then(JsonValue::as_str) else { continue };
                let name = block.get("name").and_then(JsonValue::as_str).unwrap_or("");
                let input = block.get("input").unwrap_or(&JsonValue::Null);
                let kind = if matches!(name, "Agent" | "Task") {
                    Some(LaunchKind::Subagent)
                } else if name == "Bash" && input.get("run_in_background").and_then(JsonValue::as_bool) == Some(true) {
                    Some(LaunchKind::Bash)
                } else {
                    None
                };
                if let Some(kind) = kind {
                    // A retried/duplicated line can repeat the exact same
                    // tool-use id; keep only the first sighting so a later
                    // duplicate can't reset fields a correlating
                    // tool_result/notification already filled in, and so it
                    // isn't double-counted below.
                    if let std::collections::hash_map::Entry::Vacant(entry) = launches.entry(id.to_string()) {
                        entry.insert(Launch::new(offset, kind, timestamp, input));
                        order.push(id.to_string());
                    }
                }
            }
        }
        "user" => {
            let content = value.pointer("/message/content");
            if let Some(text) = content.and_then(JsonValue::as_str) {
                handle_task_notification(text, launches);
            } else if let Some(blocks) = content.and_then(JsonValue::as_array) {
                handle_tool_result(blocks, value, launches);
            }
        }
        _ => {}
    }
}

/// Scan a bounded window of already-read lines for `Agent`/`Task`
/// delegations and background `Bash` calls, correlating launches with their
/// synchronous tool results and any later `<task-notification>`. Reads
/// `<session_dir>/subagents/agent-<id>.meta.json` for the returned subagent
/// records only (bounded I/O).
#[must_use]
pub fn scan(lines: &[RawLine], session_dir: &Path) -> (Vec<SubagentRecord>, BackgroundBashTasks) {
    let mut launches: HashMap<String, Launch> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for line in lines {
        if let Ok(value) = serde_json::from_str::<JsonValue>(&line.text) {
            ingest_line(line.offset, &value, &mut launches, &mut order);
        }
    }

    finalize(launches, order, session_dir)
}

/// Stream the whole file (never holding more than one line in memory) for
/// the same delegations and background calls — the full-file counterpart
/// used by the `agents`/`agent` wave-2 commands, which must see every
/// subagent a session ever launched, not just the ones inside `load`'s tail
/// window.
pub fn scan_full(path: &Path, session_dir: &Path) -> Result<(Vec<SubagentRecord>, BackgroundBashTasks)> {
    let mut launches: HashMap<String, Launch> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    io::scan_lines(path, |offset, text| {
        if let Ok(value) = serde_json::from_str::<JsonValue>(text) {
            ingest_line(offset, &value, &mut launches, &mut order);
        }
        Ok(true)
    })?;

    Ok(finalize(launches, order, session_dir))
}

fn finalize(
    launches: HashMap<String, Launch>,
    order: Vec<String>,
    session_dir: &Path,
) -> (Vec<SubagentRecord>, BackgroundBashTasks) {
    let mut background = BackgroundBashTasks::default();
    let mut subagents: Vec<SubagentRecord> = Vec::new();

    for id in order {
        let Some(launch) = launches.get(&id) else { continue };
        match launch.kind {
            LaunchKind::Bash => {
                background.launched += 1;
                if launch.notification_status.is_none() {
                    background.still_running.push(
                        launch.background_task_id.clone().unwrap_or_else(|| truncate(&id, 12)),
                    );
                }
                background.records.push(build_bash_record(&id, launch));
            }
            LaunchKind::Subagent => {
                subagents.push(build_record(&id, launch, session_dir));
            }
        }
    }

    (subagents, background)
}

fn build_bash_record(tool_use_id: &str, launch: &Launch) -> BashTaskRecord {
    let (status, excerpt) = if let Some(raw) = &launch.notification_status {
        (SubagentStatus::from_notification(raw), launch.notification_excerpt.clone())
    } else if launch.background_task_id.is_some() {
        (SubagentStatus::Running, None)
    } else {
        (SubagentStatus::Other("launched".to_string()), launch.sync_excerpt.clone())
    };

    BashTaskRecord {
        offset: launch.offset,
        tool_use_id: tool_use_id.to_string(),
        task_id: launch.background_task_id.clone(),
        command: launch.command.clone(),
        description: launch.description.clone(),
        launched_at: launch.timestamp,
        status,
        excerpt,
    }
}

fn handle_tool_result(blocks: &[JsonValue], event: &JsonValue, launches: &mut HashMap<String, Launch>) {
    for block in blocks {
        if block.get("type").and_then(JsonValue::as_str) != Some("tool_result") {
            continue;
        }
        let Some(tool_use_id) = block.get("tool_use_id").and_then(JsonValue::as_str) else { continue };
        let Some(launch) = launches.get_mut(tool_use_id) else { continue };

        match launch.kind {
            LaunchKind::Subagent => {
                if let Some(result) = event.get("toolUseResult") {
                    if let Some(agent_id) = result.get("agentId").and_then(JsonValue::as_str) {
                        launch.agent_id = Some(agent_id.to_string());
                    }
                }
                if launch.agent_id.is_none() {
                    if let Some(text) = tool_result_text(block) {
                        launch.sync_excerpt = Some(truncate(&text, EXCERPT_CHARS));
                    }
                }
            }
            LaunchKind::Bash => {
                if let Some(text) = tool_result_text(block) {
                    launch.background_task_id = background_task_id_from_bash_result(&text);
                }
            }
        }
    }
}

fn tool_result_text(block: &JsonValue) -> Option<String> {
    match block.get("content") {
        Some(JsonValue::String(text)) => Some(text.clone()),
        Some(JsonValue::Array(items)) => items
            .iter()
            .find_map(|item| item.get("text").and_then(JsonValue::as_str))
            .map(str::to_string),
        _ => None,
    }
}

fn handle_task_notification(text: &str, launches: &mut HashMap<String, Launch>) {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("<task-notification>") {
        return;
    }
    let Some(tool_use_id) = extract_tag(trimmed, "tool-use-id") else { return };
    let Some(launch) = launches.get_mut(tool_use_id) else { return };

    if let Some(status) = extract_tag(trimmed, "status") {
        launch.notification_status = Some(status.to_string());
    }
    let result = extract_tag(trimmed, "result").filter(|value| !value.is_empty());
    let summary = extract_tag(trimmed, "summary").filter(|value| !value.is_empty());
    if let Some(excerpt) = result.or(summary) {
        launch.notification_excerpt = Some(truncate(excerpt, EXCERPT_CHARS));
    }
}

fn build_record(tool_use_id: &str, launch: &Launch, session_dir: &Path) -> SubagentRecord {
    let mut description = launch.description.clone();
    let mut agent_type = launch.subagent_type.clone();

    if let Some(agent_id) = &launch.agent_id {
        if let Some(meta) = read_meta(session_dir, agent_id) {
            if let Some(value) = meta.description {
                description = Some(value);
            }
            if let Some(value) = meta.agent_type {
                agent_type = Some(value);
            }
        }
    }

    let (status, excerpt) = if let Some(raw) = &launch.notification_status {
        (SubagentStatus::from_notification(raw), launch.notification_excerpt.clone())
    } else if launch.sync_excerpt.is_some() {
        (SubagentStatus::Completed, launch.sync_excerpt.clone())
    } else {
        (SubagentStatus::Running, None)
    };

    SubagentRecord {
        offset: launch.offset,
        tool_use_id: tool_use_id.to_string(),
        agent_id: launch.agent_id.clone(),
        description,
        agent_type,
        prompt: launch.prompt.clone(),
        run_in_background: launch.run_in_background,
        launched_at: launch.timestamp,
        status,
        excerpt,
    }
}

struct MetaFile {
    agent_type: Option<String>,
    description: Option<String>,
}

fn read_meta(session_dir: &Path, agent_id: &str) -> Option<MetaFile> {
    let path = session_dir.join("subagents").join(format!("agent-{agent_id}.meta.json"));
    let raw = fs::read_to_string(path).ok()?;
    let value: JsonValue = serde_json::from_str(&raw).ok()?;
    Some(MetaFile {
        agent_type: value.get("agentType").and_then(JsonValue::as_str).map(str::to_string),
        description: value.get("description").and_then(JsonValue::as_str).map(str::to_string),
    })
}

/// Path to a subagent's own transcript: `<session_dir>/subagents/agent-<id>.jsonl`.
#[must_use]
pub fn transcript_path(session_dir: &Path, agent_id: &str) -> std::path::PathBuf {
    session_dir.join("subagents").join(format!("agent-{agent_id}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("claude-session-restore-subagents-tests-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn raw(offset: u64, text: &str) -> RawLine {
        RawLine { offset, text: text.to_string() }
    }

    #[test]
    fn async_launch_with_completion_notification_is_completed() {
        let dir = TempDir::new();
        let lines = vec![
            raw(0, r#"{"type":"assistant","uuid":"a1","isSidechain":false,"timestamp":"2026-09-24T17:17:25.000Z","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Agent","input":{"subagent_type":"implementer","description":"placeholder task","prompt":"placeholder brief","run_in_background":true}}]}}"#),
            raw(500, r#"{"type":"user","isSidechain":false,"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"agent123"},"message":{"content":[{"tool_use_id":"toolu_1","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]}}"#),
            raw(900, r#"{"type":"user","isSidechain":false,"message":{"content":"<task-notification>\n<task-id>t1</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n<status>completed</status>\n<summary>placeholder summary</summary>\n<result>placeholder result</result>\n</task-notification>"}}"#),
        ];

        let (subagents, background) = scan(&lines, &dir.path);
        assert_eq!(background.launched, 0);
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents[0].offset, 0);
        assert_eq!(subagents[0].agent_id.as_deref(), Some("agent123"));
        assert_eq!(subagents[0].prompt.as_deref(), Some("placeholder brief"));
        assert_eq!(subagents[0].status, SubagentStatus::Completed);
        assert_eq!(subagents[0].excerpt.as_deref(), Some("placeholder result"));
    }

    #[test]
    fn async_launch_without_notification_is_running() {
        let dir = TempDir::new();
        let lines = vec![
            raw(0, r#"{"type":"assistant","uuid":"a1","isSidechain":false,"timestamp":"2026-09-24T17:17:25.000Z","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"Task","input":{"subagent_type":"research-agent","description":"placeholder research","run_in_background":true}}]}}"#),
            raw(400, r#"{"type":"user","isSidechain":false,"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"agent456"},"message":{"content":[{"tool_use_id":"toolu_2","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]}}"#),
        ];

        let (subagents, _background) = scan(&lines, &dir.path);
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents[0].status, SubagentStatus::Running);
        assert_eq!(subagents[0].agent_id.as_deref(), Some("agent456"));
    }

    #[test]
    fn meta_json_supplies_description_and_agent_type_when_present() {
        let dir = TempDir::new();
        let subagents_dir = dir.path.join("subagents");
        fs::create_dir_all(&subagents_dir).expect("create subagents dir");
        fs::write(
            subagents_dir.join("agent-agent789.meta.json"),
            r#"{"agentType":"implementer","description":"meta description","toolUseId":"toolu_3"}"#,
        )
        .expect("write meta fixture");

        let lines = vec![
            raw(0, r#"{"type":"assistant","uuid":"a1","isSidechain":false,"timestamp":"2026-09-24T17:17:25.000Z","message":{"content":[{"type":"tool_use","id":"toolu_3","name":"Agent","input":{"subagent_type":"launch-type","description":"launch description","run_in_background":true}}]}}"#),
            raw(300, r#"{"type":"user","isSidechain":false,"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"agent789"},"message":{"content":[{"tool_use_id":"toolu_3","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]}}"#),
        ];

        let (subagents, _background) = scan(&lines, &dir.path);
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents[0].description.as_deref(), Some("meta description"));
        assert_eq!(subagents[0].agent_type.as_deref(), Some("implementer"));
    }

    #[test]
    fn background_bash_task_tracks_launched_and_still_running() {
        let dir = TempDir::new();
        let lines = vec![
            raw(0, r#"{"type":"assistant","uuid":"a1","isSidechain":false,"timestamp":"2026-09-24T23:18:24.000Z","message":{"content":[{"type":"tool_use","id":"toolu_bash1","name":"Bash","input":{"command":"cargo build --release","description":"placeholder build","run_in_background":true}}]}}"#),
            raw(200, r#"{"type":"user","isSidechain":false,"message":{"content":[{"tool_use_id":"toolu_bash1","type":"tool_result","content":"Command running in background with ID: bxyz1234. Output is being written to: /tmp/out"}]}}"#),
        ];

        let (subagents, background) = scan(&lines, &dir.path);
        assert!(subagents.is_empty());
        assert_eq!(background.launched, 1);
        assert_eq!(background.still_running, vec!["bxyz1234".to_string()]);
    }

    #[test]
    fn background_bash_task_completed_by_notification_is_not_still_running() {
        let dir = TempDir::new();
        let lines = vec![
            raw(0, r#"{"type":"assistant","uuid":"a1","isSidechain":false,"timestamp":"2026-09-24T23:18:24.000Z","message":{"content":[{"type":"tool_use","id":"toolu_bash2","name":"Bash","input":{"command":"cargo test","description":"placeholder test","run_in_background":true}}]}}"#),
            raw(200, r#"{"type":"user","isSidechain":false,"message":{"content":[{"tool_use_id":"toolu_bash2","type":"tool_result","content":"Command running in background with ID: bdone999. Output is being written to: /tmp/out"}]}}"#),
            raw(400, r#"{"type":"user","isSidechain":false,"message":{"content":"<task-notification>\n<task-id>bdone999</task-id>\n<tool-use-id>toolu_bash2</tool-use-id>\n<status>completed</status>\n<summary>placeholder done</summary>\n</task-notification>"}}"#),
        ];

        let (subagents, background) = scan(&lines, &dir.path);
        assert!(subagents.is_empty());
        assert_eq!(background.launched, 1);
        assert!(background.still_running.is_empty());
    }

    #[test]
    fn sidechain_assistant_events_are_ignored() {
        let dir = TempDir::new();
        let lines = vec![raw(
            0,
            r#"{"type":"assistant","uuid":"a1","isSidechain":true,"timestamp":"2026-09-24T17:17:25.000Z","message":{"content":[{"type":"tool_use","id":"toolu_side","name":"Agent","input":{"subagent_type":"implementer","description":"sidechain launch","run_in_background":true}}]}}"#,
        )];

        let (subagents, background) = scan(&lines, &dir.path);
        assert!(subagents.is_empty());
        assert_eq!(background.launched, 0);
    }

    #[test]
    fn a_repeated_launch_line_is_not_double_counted() {
        // A retried/duplicated JSONL line can repeat the exact same
        // tool-use id; it must be counted (and its still-running id listed)
        // only once.
        let dir = TempDir::new();
        let lines = vec![
            raw(0, r#"{"type":"assistant","uuid":"a1","isSidechain":false,"timestamp":"2026-09-24T23:18:24.000Z","message":{"content":[{"type":"tool_use","id":"toolu_dup","name":"Bash","input":{"command":"cargo build","description":"placeholder build","run_in_background":true}}]}}"#),
            raw(200, r#"{"type":"assistant","uuid":"a1","isSidechain":false,"timestamp":"2026-09-24T23:18:24.000Z","message":{"content":[{"type":"tool_use","id":"toolu_dup","name":"Bash","input":{"command":"cargo build","description":"placeholder build","run_in_background":true}}]}}"#),
            raw(400, r#"{"type":"user","isSidechain":false,"message":{"content":[{"tool_use_id":"toolu_dup","type":"tool_result","content":"Command running in background with ID: bdupe111. Output is being written to: /tmp/out"}]}}"#),
        ];

        let (subagents, background) = scan(&lines, &dir.path);
        assert!(subagents.is_empty());
        assert_eq!(background.launched, 1);
        assert_eq!(background.still_running, vec!["bdupe111".to_string()]);
    }

    #[test]
    fn scan_full_streams_the_whole_file_and_finds_the_same_launch() {
        let dir = TempDir::new();
        let unique = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("clock").as_nanos();
        let session_path = std::env::temp_dir()
            .join(format!("claude-session-restore-subagents-scan-full-{}-{unique}.jsonl", std::process::id()));
        fs::write(
            &session_path,
            "{\"type\":\"assistant\",\"uuid\":\"a1\",\"isSidechain\":false,\"timestamp\":\"2026-09-24T17:17:25.000Z\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_full\",\"name\":\"Agent\",\"input\":{\"subagent_type\":\"implementer\",\"description\":\"placeholder\",\"prompt\":\"placeholder brief\",\"run_in_background\":true}}]}}\n\
             {\"type\":\"user\",\"isSidechain\":false,\"toolUseResult\":{\"isAsync\":true,\"status\":\"async_launched\",\"agentId\":\"agentfull\"},\"message\":{\"content\":[{\"tool_use_id\":\"toolu_full\",\"type\":\"tool_result\",\"content\":[{\"type\":\"text\",\"text\":\"Async agent launched successfully.\"}]}]}}\n",
        )
        .expect("write session fixture");

        let (subagents, _background) = scan_full(&session_path, &dir.path).expect("scan_full");
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents[0].agent_id.as_deref(), Some("agentfull"));
        assert_eq!(subagents[0].prompt.as_deref(), Some("placeholder brief"));

        let _ = fs::remove_file(&session_path);
    }

    #[test]
    fn matches_id_accepts_agent_id_prefix_and_tool_use_id() {
        let record = SubagentRecord {
            offset: 0,
            tool_use_id: "toolu_abcdefgh12345".to_string(),
            agent_id: Some("a1b2c3d4e5f607182".to_string()),
            description: None,
            agent_type: None,
            prompt: None,
            run_in_background: true,
            launched_at: Utc::now(),
            status: SubagentStatus::Running,
            excerpt: None,
        };
        assert!(record.matches_id("a1b2c3d4"));
        assert!(record.matches_id("a1b2c3d4e5f607182"));
        assert!(record.matches_id("toolu_abcdefgh12345"));
        assert!(!record.matches_id("nope"));
    }
}
