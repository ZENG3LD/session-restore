use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const MAX_HEAD_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_TAIL_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_LINES: usize = 50_000;
pub const DEFAULT_MAX_MESSAGES: usize = 48;
pub const MAX_MESSAGE_CHARS: usize = 4096;
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_SESSION_FILES: usize = 50_000;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOOL_NAMES: usize = 64;
const MAX_FILE_HINTS: usize = 64;
/// How many of the most recent tool calls are kept for the "Recent Tool
/// Operations" section — a compact, ordered trace of what actually ran,
/// separate from the full-session name histogram in `tools.counts`.
const MAX_RECENT_TOOL_OPS: usize = 15;
/// Cap on a single tool operation's key-argument excerpt.
const TOOL_OP_DETAIL_CHARS: usize = 200;

#[derive(Clone, Copy, Debug)]
pub struct RestoreLimits {
    pub max_tail_bytes: usize,
    pub max_lines: usize,
    pub max_messages: usize,
}

impl Default for RestoreLimits {
    fn default() -> Self {
        Self {
            max_tail_bytes: DEFAULT_MAX_TAIL_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            max_messages: DEFAULT_MAX_MESSAGES,
        }
    }
}

impl RestoreLimits {
    pub fn validate(self) -> Result<Self, RestoreError> {
        if !(1024..=64 * 1024 * 1024).contains(&self.max_tail_bytes)
            || !(1..=100_000).contains(&self.max_lines)
            || !(1..=256).contains(&self.max_messages)
        {
            return Err(RestoreError::InvalidArgument(
                "restore limits are outside the supported bounds".to_owned(),
            ));
        }
        Ok(self)
    }
}

#[derive(Debug)]
pub enum RestoreError {
    Io(std::io::Error),
    Json(serde_json::Error),
    HomeUnavailable,
    InvalidArgument(String),
    InvalidTarget,
    UnsafeCandidate,
    AmbiguousPrefix,
    NotFound,
    NoSessionMeta,
    OutputLimit,
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => f.write_str("session storage is unavailable"),
            Self::Json(_) => f.write_str("session metadata is malformed"),
            Self::HomeUnavailable => f.write_str("Codex home is unavailable"),
            Self::InvalidArgument(message) => f.write_str(message),
            Self::InvalidTarget => f.write_str("session selector is invalid"),
            Self::UnsafeCandidate => f.write_str("session candidate is outside the trusted root"),
            Self::AmbiguousPrefix => f.write_str("session ID prefix is ambiguous"),
            Self::NotFound => f.write_str("session was not found"),
            Self::NoSessionMeta => f.write_str("session metadata is missing"),
            Self::OutputLimit => f.write_str("bounded report exceeds the output limit"),
        }
    }
}

impl std::error::Error for RestoreError {}

impl From<std::io::Error> for RestoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for RestoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionCandidate {
    #[serde(skip)]
    pub path: PathBuf,
    pub id: String,
    pub title: Option<String>,
    pub updated_unix_ms: u64,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    /// A tool call's output (`function_call_output` / `custom_tool_call_output`).
    Tool,
    /// A session-level failure (`event_msg.task_complete.error.message`).
    Error,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Message {
    pub role: MessageRole,
    pub text: String,
    pub timestamp: Option<String>,
}

/// A single tool invocation's key argument, captured verbatim (bounded and
/// redacted) so the "last N events" digest shows what actually ran instead
/// of only a name tally. See `ToolInventory::counts` for the full-session
/// histogram, which stays as a secondary, coarser view.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ToolOperation {
    pub name: String,
    pub detail: String,
    pub timestamp: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ToolInventory {
    pub counts: BTreeMap<String, u64>,
    pub changed_files: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GitHints {
    pub recorded_branch: Option<String>,
    pub recorded_commit: Option<String>,
    pub repository_label: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: Option<String>,
    pub started_at: Option<String>,
    pub updated_unix_ms: u64,
    pub workspace_label: Option<String>,
    pub model: Option<String>,
    pub model_provider: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionReport {
    pub schema: &'static str,
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
    pub tool_ops: Vec<ToolOperation>,
    pub tools: ToolInventory,
    pub git: GitHints,
    pub truncated: bool,
    pub malformed_records: u64,
    pub redactions: u64,
}

#[derive(Clone, Debug)]
pub struct SessionSource {
    pub path: PathBuf,
    pub metadata: Metadata,
    pub id: String,
    pub title: Option<String>,
}

#[derive(Clone, Debug)]
struct ParsedMeta {
    id: String,
    timestamp: Option<String>,
    cwd: Option<PathBuf>,
    model_provider: Option<String>,
    git: GitHints,
}

#[derive(Clone, Debug)]
struct TimedMessage {
    ordinal: usize,
    timestamp: Option<String>,
    text: String,
}

pub fn default_codex_home() -> Result<PathBuf, RestoreError> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".codex"))
        .ok_or(RestoreError::HomeUnavailable)
}

pub fn list_sessions(
    home: &Path,
    max_age_hours: Option<u64>,
    limit: usize,
) -> Result<Vec<SessionCandidate>, RestoreError> {
    if !(1..=100).contains(&limit) {
        return Err(RestoreError::InvalidArgument(
            "list limit must be between 1 and 100".to_owned(),
        ));
    }
    let root = trusted_sessions_root(home)?;
    let titles = read_session_index(home)?;
    let now = SystemTime::now();
    let cutoff = max_age_hours
        .map(|hours| Duration::from_secs(hours.saturating_mul(3600)))
        .and_then(|age| now.checked_sub(age));
    let mut candidates = Vec::new();
    for path in discover_session_paths(&root)? {
        let metadata = safe_candidate_metadata(&root, &path)?;
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        if cutoff.is_some_and(|value| modified < value) {
            continue;
        }
        let Some(meta) = read_session_meta(&path, &metadata)? else {
            continue;
        };
        let Some(file_id) = rollout_filename_id(&path) else {
            continue;
        };
        if meta.id != file_id {
            continue;
        }
        // Titles come from session_index.jsonl when present. When the
        // index has none, pay for one bounded head-read of the rollout
        // itself and fall back to the first real human prompt, instead of
        // leaving every untitled session looking identical in `list`.
        let mut title = titles.get(&meta.id).cloned();
        if title.is_none() {
            let mut ignored_redactions = 0;
            title = first_human_prompt(&path)
                .ok()
                .flatten()
                .and_then(|prompt| safe_scalar(&prompt, 256, &mut ignored_redactions));
        }
        candidates.push(SessionCandidate {
            path,
            id: meta.id.clone(),
            title,
            updated_unix_ms: system_time_millis(modified),
            size_bytes: metadata.len(),
        });
    }
    candidates.sort_by(|left, right| {
        right
            .updated_unix_ms
            .cmp(&left.updated_unix_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates.truncate(limit);
    Ok(candidates)
}

pub fn resolve_target(home: &Path, target: &OsStr) -> Result<SessionSource, RestoreError> {
    let root = trusted_sessions_root(home)?;
    let target_path = PathBuf::from(target);
    let path = if target_path.components().count() > 1 || target_path.is_absolute() {
        if !target_path.is_absolute() {
            return Err(RestoreError::InvalidTarget);
        }
        safe_candidate_metadata(&root, &target_path)?;
        let canonical = fs::canonicalize(&target_path).map_err(|_| RestoreError::NotFound)?;
        ensure_descendant(&root, &canonical)?;
        canonical
    } else {
        let selector = target.to_str().ok_or(RestoreError::InvalidTarget)?;
        if selector.len() < 16 || !selector.chars().all(is_id_selector_char) {
            return Err(RestoreError::InvalidTarget);
        }
        let mut matches = Vec::new();
        for candidate_path in discover_session_paths(&root)? {
            let Some(candidate_id) = rollout_filename_id(&candidate_path) else {
                continue;
            };
            if candidate_id == selector || candidate_id.starts_with(selector) {
                let metadata = safe_candidate_metadata(&root, &candidate_path)?;
                let Some(meta) = read_session_meta(&candidate_path, &metadata)? else {
                    continue;
                };
                if meta.id == candidate_id {
                    matches.push(candidate_path);
                }
            }
        }
        if matches.len() > 1 {
            return Err(RestoreError::AmbiguousPrefix);
        }
        matches.pop().ok_or(RestoreError::NotFound)?
    };
    let metadata = safe_candidate_metadata(&root, &path)?;
    let parsed = read_session_meta(&path, &metadata)?.ok_or(RestoreError::NoSessionMeta)?;
    let file_id = rollout_filename_id(&path).ok_or(RestoreError::InvalidTarget)?;
    if parsed.id != file_id {
        return Err(RestoreError::UnsafeCandidate);
    }
    let titles = read_session_index(home)?;
    Ok(SessionSource {
        path,
        metadata,
        id: parsed.id.clone(),
        title: titles.get(&parsed.id).cloned(),
    })
}

pub fn load_session(
    source: &SessionSource,
    limits: RestoreLimits,
) -> Result<SessionReport, RestoreError> {
    let limits = limits.validate()?;
    let (meta_line, records, mut truncated, malformed) = read_bounded_records(source, limits)?;
    let parsed_meta = parse_session_meta(&meta_line)?.ok_or(RestoreError::NoSessionMeta)?;
    if parsed_meta.id != source.id {
        return Err(RestoreError::UnsafeCandidate);
    }
    let mut response_user = Vec::new();
    let mut response_assistant = Vec::new();
    let mut response_tool_output = Vec::new();
    let mut event_user = Vec::new();
    let mut event_assistant = Vec::new();
    let mut event_error = Vec::new();
    let mut tool_ops: Vec<ToolOperation> = Vec::new();
    let mut tool_call_names: BTreeMap<String, String> = BTreeMap::new();
    let mut tools = ToolInventory::default();
    let mut cwd = parsed_meta.cwd.clone();
    let mut model = None;
    let mut redactions = 0_u64;
    let mut malformed_records = malformed;

    for (ordinal, line) in records.into_iter().enumerate() {
        if line.len() > MAX_LINE_BYTES {
            malformed_records += 1;
            truncated = true;
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                malformed_records += 1;
                continue;
            }
        };
        let record_type = value.get("type").and_then(Value::as_str).unwrap_or_default();
        let payload = value.get("payload").unwrap_or(&Value::Null);
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(bounded_scalar);
        match record_type {
            "turn_context" => {
                if let Some(value) = payload.get("cwd").and_then(Value::as_str) {
                    cwd = Some(PathBuf::from(value));
                }
                if let Some(value) = payload.get("model").and_then(Value::as_str) {
                    model = safe_scalar(value, 128, &mut redactions);
                }
            }
            "event_msg" => match payload.get("type").and_then(Value::as_str) {
                Some("user_message") => {
                    if let Some(text) = payload.get("message").and_then(Value::as_str) {
                        push_message(&mut event_user, ordinal, timestamp, text, &mut redactions);
                    }
                }
                Some("agent_message") => {
                    if let Some(text) = payload.get("message").and_then(Value::as_str) {
                        push_message(
                            &mut event_assistant,
                            ordinal,
                            timestamp,
                            text,
                            &mut redactions,
                        );
                    }
                }
                // Forward-compat: current Codex builds wrap turns as
                // `item_completed { item: { type: "user_message"/"agent_message", ... } }`
                // instead of the flat `user_message`/`agent_message` variants above.
                // Every session inspected so far also carries the same content via
                // `response_item`, so this arm is additive, not yet load-bearing.
                Some("item_completed") => {
                    if let Some(item) = payload.get("item") {
                        match item.get("type").and_then(Value::as_str) {
                            Some("user_message") => {
                                if let Some(text) = item_text(item) {
                                    push_message(
                                        &mut event_user,
                                        ordinal,
                                        timestamp,
                                        &text,
                                        &mut redactions,
                                    );
                                }
                            }
                            Some("agent_message") => {
                                if let Some(text) = item_text(item) {
                                    push_message(
                                        &mut event_assistant,
                                        ordinal,
                                        timestamp,
                                        &text,
                                        &mut redactions,
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Some("task_complete") => {
                    if let Some(text) = payload
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                    {
                        push_message(&mut event_error, ordinal, timestamp, text, &mut redactions);
                    }
                }
                Some("patch_apply_end") => record_changed_files(&mut tools, payload, cwd.as_deref()),
                _ => {}
            },
            "response_item" => match payload.get("type").and_then(Value::as_str) {
                Some("message") => {
                    let role = payload.get("role").and_then(Value::as_str);
                    if matches!(role, Some("user") | Some("assistant")) {
                        let text = message_content(payload, role.unwrap());
                        if !text.is_empty() && !looks_injected(&text) {
                            let target = if role == Some("user") {
                                &mut response_user
                            } else {
                                &mut response_assistant
                            };
                            push_message(target, ordinal, timestamp, &text, &mut redactions);
                        }
                    }
                }
                // A bare `agent_message` at the response_item level (e.g. a
                // subagent's own reply) carries no `role` field and uses a
                // `content` array instead of `message`; surface it as
                // assistant text, same as the event_msg fallback above.
                Some("agent_message") => {
                    if let Some(text) = item_text(payload) {
                        if !looks_injected(&text) {
                            push_message(
                                &mut response_assistant,
                                ordinal,
                                timestamp,
                                &text,
                                &mut redactions,
                            );
                        }
                    }
                }
                Some("function_call") | Some("custom_tool_call") => {
                    if let Some(name) = payload.get("name").and_then(Value::as_str) {
                        record_tool_name(&mut tools, name);
                        if let Some(call_id) = payload.get("call_id").and_then(Value::as_str) {
                            tool_call_names.insert(call_id.to_owned(), name.to_owned());
                        }
                        if let Some(detail) = extract_tool_detail(payload) {
                            push_tool_op(&mut tool_ops, timestamp, name, &detail, &mut redactions);
                        }
                    }
                }
                Some("function_call_output") | Some("custom_tool_call_output") => {
                    if let Some(text) = payload.get("output").and_then(extract_text_value) {
                        let name = payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .and_then(|call_id| tool_call_names.get(call_id))
                            .cloned()
                            .unwrap_or_else(|| "tool".to_owned());
                        let combined = format!("{name} -> {text}");
                        push_message(
                            &mut response_tool_output,
                            ordinal,
                            timestamp,
                            &combined,
                            &mut redactions,
                        );
                    }
                }
                _ => {}
            },
            "patch_apply_end" => record_changed_files(&mut tools, payload, cwd.as_deref()),
            _ => {}
        }
    }

    if tool_ops.len() > MAX_RECENT_TOOL_OPS {
        let drop_count = tool_ops.len() - MAX_RECENT_TOOL_OPS;
        tool_ops.drain(..drop_count);
        truncated = true;
    }

    let mut messages = Vec::with_capacity(
        event_user.len()
            + response_user.len()
            + event_assistant.len()
            + response_assistant.len()
            + response_tool_output.len()
            + event_error.len(),
    );
    messages.extend(response_user.into_iter().map(|message| (MessageRole::User, message)));
    messages.extend(event_user.into_iter().map(|message| (MessageRole::User, message)));
    messages.extend(
        response_assistant
            .into_iter()
            .map(|message| (MessageRole::Assistant, message)),
    );
    messages.extend(
        event_assistant
            .into_iter()
            .map(|message| (MessageRole::Assistant, message)),
    );
    messages.extend(
        response_tool_output
            .into_iter()
            .map(|message| (MessageRole::Tool, message)),
    );
    messages.extend(event_error.into_iter().map(|message| (MessageRole::Error, message)));
    messages.sort_by_key(|(_, message)| message.ordinal);
    let mut deduplicated = Vec::with_capacity(messages.len());
    for message in messages {
        // Codex writes the same human/agent turn twice under two encodings
        // (`response_item.message`, whose parts this parser joins with an
        // extra "\n", and the flat `event_msg.*` string) that differ only by
        // incidental whitespace. Compare on collapsed whitespace so that
        // pair merges into one entry, while two messages with different
        // words never collapse into each other.
        let duplicate = deduplicated.last().is_some_and(
            |(last_role, last_message): &(MessageRole, TimedMessage)| {
                *last_role == message.0
                    && normalize_for_dedup(&last_message.text) == normalize_for_dedup(&message.1.text)
            },
        );
        if !duplicate {
            deduplicated.push(message);
        }
    }
    let mut messages = deduplicated;
    if messages.len() > limits.max_messages {
        let drop_count = messages.len() - limits.max_messages;
        messages.drain(..drop_count);
        truncated = true;
    }
    let messages = messages
        .into_iter()
        .map(|(role, message)| Message {
            role,
            text: message.text,
            timestamp: message.timestamp,
        })
        .collect();
    let workspace_label = cwd
        .as_deref()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .and_then(|value| safe_scalar(value, 128, &mut redactions));
    // Provider title first; when the store has none, fall back to the
    // earliest real human prompt (bounded head scan, independent of the
    // tail-bounded record read above so it still works on sessions whose
    // opening turn falls outside that window).
    let title = source
        .title
        .as_deref()
        .and_then(|value| safe_scalar(value, 256, &mut redactions))
        .or_else(|| {
            first_human_prompt(&source.path)
                .ok()
                .flatten()
                .and_then(|prompt| safe_scalar(&prompt, 256, &mut redactions))
        });
    let mut report = SessionReport {
        schema: "codex-session-restore-v1",
        meta: SessionMeta {
            id: source.id.clone(),
            title,
            started_at: parsed_meta.timestamp,
            updated_unix_ms: system_time_millis(
                source.metadata.modified().unwrap_or(UNIX_EPOCH),
            ),
            workspace_label,
            model,
            model_provider: parsed_meta
                .model_provider
                .as_deref()
                .and_then(|value| safe_scalar(value, 128, &mut redactions)),
        },
        messages,
        tool_ops,
        tools,
        git: parsed_meta.git,
        truncated,
        malformed_records,
        redactions,
    };
    enforce_output_bound(&mut report)?;
    Ok(report)
}

pub fn encode_json<T: Serialize>(value: &T) -> Result<String, RestoreError> {
    let encoded = serde_json::to_string_pretty(value)?;
    if encoded.len() > MAX_OUTPUT_BYTES {
        return Err(RestoreError::OutputLimit);
    }
    Ok(encoded)
}

pub fn render_report(report: &SessionReport) -> String {
    let mut output = String::new();
    output.push_str("Codex session restore report\n");
    output.push_str(&format!("session_id: {}\n", report.meta.id));
    if let Some(title) = &report.meta.title {
        output.push_str(&format!("title: {title}\n"));
    }
    if let Some(started_at) = &report.meta.started_at {
        output.push_str(&format!("started_at: {started_at}\n"));
    }
    output.push_str(&format!(
        "updated_unix_ms: {}\n",
        report.meta.updated_unix_ms
    ));
    if let Some(workspace) = &report.meta.workspace_label {
        output.push_str(&format!("workspace_label: {workspace}\n"));
    }
    if let Some(model) = &report.meta.model {
        output.push_str(&format!("model: {model}\n"));
    }
    output.push_str(&format!(
        "truncated: {}\nmalformed_records: {}\nredactions: {}\n",
        report.truncated, report.malformed_records, report.redactions
    ));
    output.push_str("messages:\n");
    for message in &report.messages {
        let role = match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
            MessageRole::Error => "error",
        };
        output.push_str(&format!("- {role}: {}\n", message.text.replace('\n', " ")));
    }
    if !report.tool_ops.is_empty() {
        output.push_str("recent_tool_operations:\n");
        for op in &report.tool_ops {
            output.push_str(&format!("- {}: {}\n", op.name, op.detail.replace('\n', " ")));
        }
    }
    if !report.tools.counts.is_empty() {
        output.push_str("tool_counts:\n");
        for (name, count) in &report.tools.counts {
            output.push_str(&format!("- {name}: {count}\n"));
        }
    }
    if !report.tools.changed_files.is_empty() {
        output.push_str("changed_files:\n");
        for path in &report.tools.changed_files {
            output.push_str(&format!("- {path}\n"));
        }
    }
    if let Some(branch) = &report.git.recorded_branch {
        output.push_str(&format!("recorded_branch: {branch}\n"));
    }
    if let Some(commit) = &report.git.recorded_commit {
        output.push_str(&format!("recorded_commit: {commit}\n"));
    }
    output
}

fn trusted_sessions_root(home: &Path) -> Result<PathBuf, RestoreError> {
    let home_meta = fs::symlink_metadata(home).map_err(|_| RestoreError::HomeUnavailable)?;
    if !home_meta.is_dir() || metadata_is_reparse(&home_meta) {
        return Err(RestoreError::HomeUnavailable);
    }
    let root = home.join("sessions");
    let metadata = fs::symlink_metadata(&root).map_err(|_| RestoreError::HomeUnavailable)?;
    if !metadata.is_dir() || metadata_is_reparse(&metadata) {
        return Err(RestoreError::HomeUnavailable);
    }
    fs::canonicalize(root).map_err(RestoreError::Io)
}

fn discover_session_paths(root: &Path) -> Result<Vec<PathBuf>, RestoreError> {
    let mut paths = Vec::new();
    for year in safe_read_directories(root)? {
        for month in safe_read_directories(&year)? {
            for day in safe_read_directories(&month)? {
                for entry in fs::read_dir(&day)? {
                    let entry = entry?;
                    if paths.len() >= MAX_SESSION_FILES {
                        return Err(RestoreError::InvalidArgument(
                            "session file inventory exceeds the supported bound".to_owned(),
                        ));
                    }
                    let path = entry.path();
                    if path.extension() == Some(OsStr::new("jsonl"))
                        && rollout_filename_id(&path).is_some()
                    {
                        paths.push(path);
                    }
                }
            }
        }
    }
    Ok(paths)
}

fn safe_read_directories(root: &Path) -> Result<Vec<PathBuf>, RestoreError> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() && !file_type.is_symlink() {
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata_is_reparse(&metadata) {
                result.push(entry.path());
            }
        }
    }
    Ok(result)
}

fn safe_candidate_metadata(root: &Path, path: &Path) -> Result<Metadata, RestoreError> {
    let link_meta = fs::symlink_metadata(path).map_err(|_| RestoreError::NotFound)?;
    if !link_meta.is_file() || metadata_is_reparse(&link_meta) {
        return Err(RestoreError::UnsafeCandidate);
    }
    let canonical = fs::canonicalize(path).map_err(|_| RestoreError::UnsafeCandidate)?;
    ensure_descendant(root, &canonical)?;
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_file() {
        return Err(RestoreError::UnsafeCandidate);
    }
    Ok(metadata)
}

fn ensure_descendant(root: &Path, path: &Path) -> Result<(), RestoreError> {
    if path == root || !path.starts_with(root) {
        return Err(RestoreError::UnsafeCandidate);
    }
    Ok(())
}

fn read_session_index(home: &Path) -> Result<BTreeMap<String, String>, RestoreError> {
    let path = home.join("session_index.jsonl");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata_is_reparse(&metadata) || metadata.len() > MAX_INDEX_BYTES {
        return Ok(BTreeMap::new());
    }
    let mut file = open_shared_read(&path)?;
    let snapshot_len = file.metadata()?.len();
    if snapshot_len > MAX_INDEX_BYTES {
        return Ok(BTreeMap::new());
    }
    let mut bytes = Vec::with_capacity(snapshot_len as usize);
    (&mut file).take(snapshot_len).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != snapshot_len {
        return Err(RestoreError::UnsafeCandidate);
    }
    let mut titles = BTreeMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.len() > MAX_LINE_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_str).filter(|id| valid_uuid(id)) else {
            continue;
        };
        let Some(title) = value.get("thread_name").and_then(Value::as_str) else {
            continue;
        };
        let mut redactions = 0;
        if let Some(title) = safe_scalar(title, 256, &mut redactions) {
            titles.insert(id.to_owned(), title);
        }
    }
    Ok(titles)
}

fn read_session_meta(path: &Path, metadata: &Metadata) -> Result<Option<ParsedMeta>, RestoreError> {
    let mut file = open_shared_read(path)?;
    let mut reader = BufReader::new((&mut file).take(MAX_HEAD_BYTES as u64));
    let mut line = Vec::with_capacity((metadata.len() as usize).min(8192));
    if reader.read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    if line.len() > MAX_LINE_BYTES {
        return Ok(None);
    }
    let line = std::str::from_utf8(&line).map_err(|_| RestoreError::NoSessionMeta)?;
    parse_session_meta(line)
}

fn parse_session_meta(line: &str) -> Result<Option<ParsedMeta>, RestoreError> {
    let value: Value = serde_json::from_str(line)?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Ok(None);
    }
    let payload = value.get("payload").ok_or(RestoreError::NoSessionMeta)?;
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| valid_uuid(value))
        .ok_or(RestoreError::NoSessionMeta)?
        .to_owned();
    let git_value = payload.get("git").unwrap_or(&Value::Null);
    let mut ignored_redactions = 0;
    let recorded_branch = git_value
        .get("branch")
        .and_then(Value::as_str)
        .and_then(|value| safe_scalar(value, 256, &mut ignored_redactions));
    let recorded_commit = git_value
        .get("commit_hash")
        .and_then(Value::as_str)
        .filter(|value| (7..=64).contains(&value.len()) && value.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(str::to_owned);
    let repository_label = git_value
        .get("repository_url")
        .and_then(Value::as_str)
        .and_then(repository_label)
        .and_then(|value| safe_scalar(&value, 128, &mut ignored_redactions));
    Ok(Some(ParsedMeta {
        id,
        timestamp: value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(bounded_scalar),
        cwd: payload.get("cwd").and_then(Value::as_str).map(PathBuf::from),
        model_provider: payload
            .get("model_provider")
            .and_then(Value::as_str)
            .map(str::to_owned),
        git: GitHints {
            recorded_branch,
            recorded_commit,
            repository_label,
        },
    }))
}

/// Scan the first `MAX_HEAD_BYTES` of a rollout file for the earliest
/// genuine human prompt, used as a topic fallback when a session has no
/// title in `session_index.jsonl`. This is independent of `load`'s
/// tail-bounded record read, so it still finds the opening prompt on a
/// session whose first turn falls outside that tail window.
fn first_human_prompt(path: &Path) -> Result<Option<String>, RestoreError> {
    let mut file = open_shared_read(path)?;
    let mut head = Vec::new();
    (&mut file).take(MAX_HEAD_BYTES as u64).read_to_end(&mut head)?;
    for raw in head.split(|byte| *byte == b'\n') {
        if raw.is_empty() || raw.len() > MAX_LINE_BYTES {
            continue;
        }
        let Ok(line) = std::str::from_utf8(raw) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let record_type = value.get("type").and_then(Value::as_str).unwrap_or_default();
        let payload = value.get("payload").unwrap_or(&Value::Null);
        let text = match record_type {
            "event_msg" if payload.get("type").and_then(Value::as_str) == Some("user_message") => {
                payload.get("message").and_then(Value::as_str).map(str::to_owned)
            }
            "response_item"
                if payload.get("type").and_then(Value::as_str) == Some("message")
                    && payload.get("role").and_then(Value::as_str) == Some("user") =>
            {
                let text = message_content(payload, "user");
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        };
        let Some(text) = text else {
            continue;
        };
        if looks_injected(&text) {
            continue;
        }
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_owned()));
        }
    }
    Ok(None)
}

fn read_bounded_records(
    source: &SessionSource,
    limits: RestoreLimits,
) -> Result<(String, Vec<String>, bool, u64), RestoreError> {
    let mut file = open_shared_read(&source.path)?;
    let snapshot_len = file.metadata()?.len();
    let mut head = Vec::with_capacity((snapshot_len as usize).min(MAX_HEAD_BYTES));
    (&mut file).take(MAX_HEAD_BYTES as u64).read_to_end(&mut head)?;
    let meta_line = head
        .split(|byte| *byte == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .ok_or(RestoreError::NoSessionMeta)?
        .to_owned();
    let mut truncated = snapshot_len as usize > limits.max_tail_bytes;
    let start = snapshot_len.saturating_sub(limits.max_tail_bytes as u64);
    file.seek(SeekFrom::Start(start))?;
    let snapshot_tail_len = snapshot_len - start;
    let mut tail = Vec::with_capacity(snapshot_tail_len as usize);
    (&mut file)
        .take(snapshot_tail_len)
        .read_to_end(&mut tail)?;
    if tail.len() as u64 != snapshot_tail_len {
        return Err(RestoreError::UnsafeCandidate);
    }
    if start > 0 {
        if let Some(index) = tail.iter().position(|byte| *byte == b'\n') {
            tail.drain(..=index);
        } else {
            tail.clear();
        }
    }
    let mut malformed = 0_u64;
    let mut lines = Vec::new();
    for raw in tail.split(|byte| *byte == b'\n') {
        if raw.is_empty() {
            continue;
        }
        if raw.len() > MAX_LINE_BYTES {
            malformed += 1;
            truncated = true;
            continue;
        }
        match std::str::from_utf8(raw) {
            Ok(line) => lines.push(line.to_owned()),
            Err(_) => malformed += 1,
        }
    }
    if lines.len() > limits.max_lines {
        let drop_count = lines.len() - limits.max_lines;
        lines.drain(..drop_count);
        truncated = true;
    }
    Ok((meta_line, lines, truncated, malformed))
}

#[cfg(windows)]
fn open_shared_read(path: &Path) -> Result<File, RestoreError> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x00000001;
    const FILE_SHARE_WRITE: u32 = 0x00000002;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x00200000;
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if metadata_is_reparse(&file.metadata()?) {
        return Err(RestoreError::UnsafeCandidate);
    }
    Ok(file)
}

#[cfg(not(windows))]
fn open_shared_read(path: &Path) -> Result<File, RestoreError> {
    Ok(OpenOptions::new().read(true).open(path)?)
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x00000400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn message_content(payload: &Value, role: &str) -> String {
    let expected = if role == "user" { "input_text" } else { "output_text" };
    payload
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some(expected))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract plain text from a JSON value that may be a bare string, an
/// object carrying `content`/`text`, or an array of such parts. Codex uses
/// all three shapes across `function_call_output.output`,
/// `custom_tool_call_output.output`, and bare `response_item.agent_message`
/// / `item_completed` item payloads.
fn extract_text_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(_) => value
            .get("content")
            .and_then(extract_text_value)
            .or_else(|| value.get("text").and_then(Value::as_str).map(str::to_owned)),
        Value::Array(items) => {
            let joined = items
                .iter()
                .filter_map(extract_text_value)
                .collect::<Vec<_>>()
                .join("\n");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

/// Text of a `user_message`/`agent_message` item under either shape Codex
/// has used: a flat `message` string (what `event_msg`'s direct variants
/// carry) or a `content` array / `text` field (what a bare
/// `response_item.agent_message` and the newer `item_completed` wrapper
/// carry instead).
fn item_text(item: &Value) -> Option<String> {
    item.get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| item.get("content").and_then(extract_text_value))
        .or_else(|| item.get("text").and_then(Value::as_str).map(str::to_owned))
}

/// The key argument of a tool call — the shell command, URL, query, or path
/// a reviewer actually needs to see, not just the tool's name.
/// `custom_tool_call.input` and `function_call.arguments` are read as
/// either a JSON object (pull a well-known field) or, when that fails
/// (`custom_tool_call.input` is often raw script text, not JSON), the raw
/// string itself, bounded and redacted the same as any other excerpt.
fn extract_tool_detail(payload: &Value) -> Option<String> {
    let raw = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .and_then(Value::as_str)?;
    let detail = serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|parsed| salient_arg_field(&parsed))
        .unwrap_or_else(|| raw.to_owned());
    Some(detail)
}

fn salient_arg_field(value: &Value) -> Option<String> {
    const KEYS: [&str; 8] = [
        "command", "cmd", "url", "query", "path", "file_path", "pattern", "script",
    ];
    let object = value.as_object()?;
    for key in KEYS {
        if let Some(text) = object.get(key).and_then(Value::as_str) {
            return Some(text.to_owned());
        }
    }
    None
}

fn push_message(
    target: &mut Vec<TimedMessage>,
    ordinal: usize,
    timestamp: Option<String>,
    text: &str,
    redactions: &mut u64,
) {
    if looks_injected(text) {
        return;
    }
    let text = redact_text(text, redactions);
    let text = truncate_message_tail(text.trim(), MAX_MESSAGE_CHARS);
    if !text.is_empty() {
        target.push(TimedMessage {
            ordinal,
            timestamp,
            text,
        });
    }
}

/// Push a tool operation onto `target`. Callers append in ordinal order
/// during a single forward pass over the record stream, so `target` is
/// already chronological without a separate sort step.
fn push_tool_op(
    target: &mut Vec<ToolOperation>,
    timestamp: Option<String>,
    name: &str,
    detail: &str,
    redactions: &mut u64,
) {
    if looks_injected(detail) {
        return;
    }
    let redacted = redact_text(detail, redactions);
    let first_line = redacted.lines().next().unwrap_or(&redacted);
    let detail = truncate_chars(first_line.trim(), TOOL_OP_DETAIL_CHARS);
    if !detail.is_empty() {
        target.push(ToolOperation {
            name: name.to_owned(),
            detail,
            timestamp,
        });
    }
}

fn looks_injected(value: &str) -> bool {
    let lowered = value.to_ascii_lowercase();
    [
        "<environment_context>",
        "<permissions instructions>",
        "<collaboration_mode>",
        "<skills_instructions>",
        "<app-context>",
        "# agents.md instructions",
        "========= memory_summary begins =========",
    ]
    .iter()
    .any(|marker| lowered.contains(marker))
}

fn record_tool_name(tools: &mut ToolInventory, name: &str) {
    if tools.counts.len() >= MAX_TOOL_NAMES && !tools.counts.contains_key(name) {
        return;
    }
    if !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':'))
    {
        *tools.counts.entry(name.to_owned()).or_default() += 1;
    }
}

fn record_changed_files(tools: &mut ToolInventory, payload: &Value, cwd: Option<&Path>) {
    let Some(cwd) = cwd else {
        return;
    };
    let Some(changes) = payload.get("changes").and_then(Value::as_object) else {
        return;
    };
    for path in changes.keys() {
        if tools.changed_files.len() >= MAX_FILE_HINTS {
            break;
        }
        let path = Path::new(path);
        let Ok(relative) = path.strip_prefix(cwd) else {
            continue;
        };
        if !safe_relative_path(relative) {
            continue;
        }
        let text = relative.to_string_lossy().replace('\\', "/");
        if text.len() <= 512 && !contains_credential_marker(&text) {
            tools.changed_files.insert(text);
        }
    }
}

fn safe_relative_path(path: &Path) -> bool {
    path.components().next().is_some()
        && path.components().all(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .is_some_and(|text| !text.is_empty() && !text.chars().any(char::is_control)),
            _ => false,
        })
}

fn enforce_output_bound(report: &mut SessionReport) -> Result<(), RestoreError> {
    loop {
        let encoded = serde_json::to_vec(report)?;
        if encoded.len() <= MAX_OUTPUT_BYTES {
            return Ok(());
        }
        if report.messages.is_empty() {
            return Err(RestoreError::OutputLimit);
        }
        report.messages.remove(0);
        report.truncated = true;
    }
}

fn redact_text(value: &str, redactions: &mut u64) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = String::new();
    let mut in_pem = false;
    for line in normalized.lines() {
        let lowered = line.to_ascii_lowercase();
        if lowered.contains("-----begin ") && lowered.contains("private key-----") {
            in_pem = true;
            *redactions += 1;
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("[REDACTED]");
            continue;
        }
        if in_pem {
            if lowered.contains("-----end ") && lowered.contains("private key-----") {
                in_pem = false;
            }
            continue;
        }
        let mut clean: String = line.chars().filter(|ch| !ch.is_control() || *ch == '\t').collect();
        if credential_regex().is_match(&clean) || auth_header_regex().is_match(&clean) {
            *redactions += 1;
            clean = "[REDACTED]".to_owned();
        }
        clean = jwt_regex()
            .replace_all(&clean, |_: &regex::Captures<'_>| {
                *redactions += 1;
                "[REDACTED]"
            })
            .into_owned();
        clean = token_regex()
            .replace_all(&clean, |_: &regex::Captures<'_>| {
                *redactions += 1;
                "[REDACTED]"
            })
            .into_owned();
        clean = uri_userinfo_regex()
            .replace_all(&clean, |captures: &regex::Captures<'_>| {
                *redactions += 1;
                format!("{}[REDACTED]@", &captures[1])
            })
            .into_owned();
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&clean);
    }
    output
}

fn credential_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| {
        Regex::new(r#"(?i)(?:^|[^A-Za-z0-9_])["']?(?:password|passwd|secret|token|api[_-]?key|authorization|[A-Za-z0-9_]+_(?:password|passwd|secret|token|api[_-]?key|authorization))["']?\s*[:=]"#)
            .unwrap()
    })
}

fn auth_header_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r#"(?i)\b(?:bearer|basic)\s+[^\s,;"']+"#).unwrap())
}

fn jwt_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b").unwrap())
}

fn token_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| {
        Regex::new(r"\b(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[A-Z0-9]{16})\b").unwrap()
    })
}

fn uri_userinfo_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    VALUE.get_or_init(|| Regex::new(r"([A-Za-z][A-Za-z0-9+.-]*://)[^/@\s]+:[^/@\s]+@").unwrap())
}

fn contains_credential_marker(value: &str) -> bool {
    credential_regex().is_match(value)
        || auth_header_regex().is_match(value)
        || jwt_regex().is_match(value)
        || token_regex().is_match(value)
        || uri_userinfo_regex().is_match(value)
}

fn safe_scalar(value: &str, max_chars: usize, redactions: &mut u64) -> Option<String> {
    let value = redact_text(value, redactions);
    let value = truncate_chars(value.trim(), max_chars);
    (!value.is_empty()).then_some(value)
}

fn bounded_scalar(value: &str) -> String {
    truncate_chars(value.trim(), 128)
}

/// Truncate a short scalar (title, model, workspace label, …) to at most
/// `max_chars`, keeping the HEAD. These are identifiers, not narrative
/// text — the meaningful part is at the front. Message bodies use
/// [`truncate_message_tail`] instead.
fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut result: String = value.chars().take(max_chars).collect();
    if value.chars().count() > max_chars {
        result.push('…');
    }
    result
}

/// Truncate a message BODY to at most `max_chars`, keeping the TAIL. This
/// crate's design keeps "the last N" throughout — the last bytes of a
/// growing file, the last lines, the last messages — and per-message
/// character truncation used to be the one place that kept the *first* N
/// characters instead, cutting off a long tool-approval or error message
/// right before its actionable ending. The dropped prefix is called out
/// with an explicit `[truncated N chars]` marker instead of disappearing
/// silently.
fn truncate_message_tail(text: &str, max_chars: usize) -> String {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text.to_owned();
    }
    let cut = total_chars - max_chars;
    let tail: String = text.chars().skip(cut).collect();
    format!("[truncated {cut} chars]…{tail}")
}

/// Collapse whitespace runs (including newlines) before a dedup
/// comparison. Codex writes the same human/agent turn twice under two
/// encodings — `response_item.message` (whose parts this parser joins with
/// an extra `"\n"`) and `event_msg.*` (a single flat string) — that differ
/// only by incidental whitespace, not content. Comparing on collapsed
/// whitespace merges exactly that pair while leaving two messages with
/// different words distinct.
fn normalize_for_dedup(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn repository_label(value: &str) -> Option<String> {
    let without_query = value.split(['?', '#']).next().unwrap_or_default();
    let tail = without_query
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\', ':'])
        .next()
        .unwrap_or_default()
        .trim_end_matches(".git");
    (!tail.is_empty()).then_some(tail.to_owned())
}

fn rollout_filename_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let id = stem.rsplit('-').take(5).collect::<Vec<_>>();
    if id.len() != 5 {
        return None;
    }
    let candidate = format!("{}-{}-{}-{}-{}", id[4], id[3], id[2], id[1], id[0]);
    valid_uuid(&candidate).then_some(candidate)
}

fn valid_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.chars().enumerate().all(|(index, ch)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            ch == '-'
        } else {
            ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()
        }
    })
}

fn is_id_selector_char(ch: char) -> bool {
    ch == '-' || (ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase())
}

fn system_time_millis(value: SystemTime) -> u64 {
    value
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;

    fn fixture_home() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("sessions/2026/08/10")).unwrap();
        temp
    }

    fn write_session(home: &Path, id: &str, records: &[Value]) -> PathBuf {
        let path = home
            .join("sessions/2026/08/10")
            .join(format!("rollout-2026-08-10T10-00-00-{id}.jsonl"));
        let mut file = File::create(&path).unwrap();
        let meta = serde_json::json!({
            "timestamp": "2026-08-10T10:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "cwd": "C:\\work\\demo",
                "model_provider": "openai",
                "git": {"branch":"feature/restore","commit_hash":"0123456789abcdef","repository_url":"https://user:pass@example.invalid/acme/demo.git"}
            }
        });
        writeln!(file, "{}", serde_json::to_string(&meta).unwrap()).unwrap();
        for record in records {
            writeln!(file, "{}", serde_json::to_string(record).unwrap()).unwrap();
        }
        path
    }

    fn source(home: &Path, id: &str) -> SessionSource {
        resolve_target(home, OsStr::new(id)).unwrap()
    }

    #[test]
    fn parser_surfaces_only_user_and_assistant_and_excludes_hidden_records() {
        let temp = fixture_home();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        write_session(
            temp.path(),
            id,
            &[
                serde_json::json!({"timestamp":"1","type":"response_item","payload":{"type":"reasoning","summary":["HIDDEN_REASONING"]}}),
                serde_json::json!({"timestamp":"2","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"HIDDEN_DEVELOPER"}]}}),
                serde_json::json!({"timestamp":"3","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fallback user"}]}}),
                serde_json::json!({"timestamp":"4","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fallback assistant"}]}}),
                serde_json::json!({"timestamp":"5","type":"event_msg","payload":{"type":"user_message","message":"Choose checked_add"}}),
                serde_json::json!({"timestamp":"6","type":"event_msg","payload":{"type":"agent_message","message":"Preserve the public API"}}),
                serde_json::json!({"timestamp":"7","type":"response_item","payload":{"type":"function_call","name":"shell_command","call_id":"call_1","arguments":"VISIBLE_COMMAND_ARG"}}),
                serde_json::json!({"timestamp":"8","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"VISIBLE_TOOL_OUTPUT"}}),
                serde_json::json!({"timestamp":"9","type":"compacted","payload":{"replacement_history":"HIDDEN_COMPACTION"}}),
                serde_json::json!({"timestamp":"10","type":"event_msg","payload":{"type":"patch_apply_end","changes":{"C:\\work\\demo\\src\\lib.rs":{"kind":"update"}},"stdout":"HIDDEN_PATCH_OUTPUT"}}),
            ],
        );
        let report = load_session(&source(temp.path(), id), RestoreLimits::default()).unwrap();
        assert_eq!(
            report
                .messages
                .iter()
                .filter(|message| matches!(message.role, MessageRole::User | MessageRole::Assistant))
                .count(),
            4
        );
        assert!(report.messages.iter().any(|message| message.text == "fallback user"));
        assert!(report.messages.iter().any(|message| message.text == "fallback assistant"));
        assert!(report.messages.iter().any(|message| message.text == "Choose checked_add"));
        assert!(report.messages.iter().any(|message| message.text == "Preserve the public API"));
        // D1: tool call arguments now surface in their own section.
        assert_eq!(report.tool_ops.len(), 1);
        assert_eq!(report.tool_ops[0].name, "shell_command");
        assert_eq!(report.tool_ops[0].detail, "VISIBLE_COMMAND_ARG");
        // D2: tool outputs now surface as their own message role.
        assert!(report
            .messages
            .iter()
            .any(|message| message.role == MessageRole::Tool && message.text.contains("VISIBLE_TOOL_OUTPUT")));
        let encoded = encode_json(&report).unwrap();
        for hidden in ["HIDDEN_REASONING", "HIDDEN_DEVELOPER", "HIDDEN_COMPACTION"] {
            assert!(!encoded.contains(hidden));
        }
        assert!(encoded.contains("VISIBLE_COMMAND_ARG"));
        assert!(encoded.contains("VISIBLE_TOOL_OUTPUT"));
        assert_eq!(report.tools.counts.get("shell_command"), Some(&1));
        assert!(report.tools.changed_files.contains("src/lib.rs"));
        assert!(!encoded.contains("HIDDEN_PATCH_OUTPUT"));
    }

    #[test]
    fn event_msg_is_fallback_without_duplicates() {
        let temp = fixture_home();
        let id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        write_session(
            temp.path(),
            id,
            &[
                serde_json::json!({"timestamp":"1","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"canonical user"}]}}),
                serde_json::json!({"timestamp":"2","type":"event_msg","payload":{"type":"user_message","message":"canonical user"}}),
                serde_json::json!({"timestamp":"3","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"assistant fallback"}]}}),
            ],
        );
        let report = load_session(&source(temp.path(), id), RestoreLimits::default()).unwrap();
        assert_eq!(report.messages.iter().filter(|m| m.role == MessageRole::User).count(), 1);
        assert!(report.messages.iter().any(|m| m.text == "canonical user"));
        assert!(report.messages.iter().any(|m| m.text == "assistant fallback"));
    }

    #[test]
    fn dedup_collapses_dual_encoding_whitespace_variant_but_keeps_distinct_messages() {
        let temp = fixture_home();
        let id = "66666666-6666-4666-8666-666666666666";
        write_session(
            temp.path(),
            id,
            &[
                // response_item joins parts with "\n"; the trailing "\n" on
                // the first part plus the join's own "\n" produces a
                // double newline, unlike the flat event_msg string below.
                serde_json::json!({"timestamp":"1","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"line one\n"},{"type":"input_text","text":"line two"}]}}),
                serde_json::json!({"timestamp":"2","type":"event_msg","payload":{"type":"user_message","message":"line one\nline two"}}),
                serde_json::json!({"timestamp":"3","type":"event_msg","payload":{"type":"user_message","message":"a genuinely different follow-up"}}),
            ],
        );
        let report = load_session(&source(temp.path(), id), RestoreLimits::default()).unwrap();
        let user_messages: Vec<_> =
            report.messages.iter().filter(|m| m.role == MessageRole::User).collect();
        assert_eq!(
            user_messages.len(),
            2,
            "dual-encoding pair must collapse but the distinct follow-up must survive: {user_messages:?}"
        );
        assert!(user_messages[0].text.contains("line one"));
        assert!(user_messages[1].text.contains("a genuinely different follow-up"));
    }

    #[test]
    fn tool_call_arguments_are_captured_as_recent_tool_operations() {
        let temp = fixture_home();
        let id = "22222222-2222-4222-8222-222222222222";
        write_session(
            temp.path(),
            id,
            &[
                serde_json::json!({"timestamp":"1","type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"call_1","arguments":"{\"command\":\"rg -n TODO\"}"}}),
                serde_json::json!({"timestamp":"2","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_2","input":"tools.exec_command({cmd:\"ls -la\"})"}}),
            ],
        );
        let report = load_session(&source(temp.path(), id), RestoreLimits::default()).unwrap();
        assert_eq!(report.tool_ops.len(), 2);
        assert_eq!(report.tool_ops[0].name, "shell");
        assert_eq!(report.tool_ops[0].detail, "rg -n TODO");
        assert_eq!(report.tool_ops[1].name, "exec");
        assert!(report.tool_ops[1].detail.contains("tools.exec_command"));
    }

    #[test]
    fn tool_output_and_task_complete_error_are_surfaced() {
        let temp = fixture_home();
        let id = "33333333-3333-4333-8333-333333333333";
        write_session(
            temp.path(),
            id,
            &[
                serde_json::json!({"timestamp":"1","type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"call_9","arguments":"{\"command\":\"ls\"}"}}),
                serde_json::json!({"timestamp":"2","type":"response_item","payload":{"type":"function_call_output","call_id":"call_9","output":"total 0\nfile.txt"}}),
                serde_json::json!({"timestamp":"3","type":"response_item","payload":{"type":"agent_message","content":[{"type":"text","text":"Agent errored: usage limit reached"}]}}),
                serde_json::json!({"timestamp":"4","type":"event_msg","payload":{"type":"task_complete","error":{"message":"You have hit your usage limit"}}}),
            ],
        );
        let report = load_session(&source(temp.path(), id), RestoreLimits::default()).unwrap();
        let tool_message = report
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("tool output message present");
        assert!(tool_message.text.contains("shell -> total 0"));
        let error_message = report
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Error)
            .expect("error message present");
        assert_eq!(error_message.text, "You have hit your usage limit");
        assert!(report
            .messages
            .iter()
            .any(|m| m.role == MessageRole::Assistant && m.text.contains("Agent errored")));
    }

    #[test]
    fn item_completed_event_is_handled_for_forward_compatibility() {
        let temp = fixture_home();
        let id = "77777777-7777-4777-8777-777777777777";
        write_session(
            temp.path(),
            id,
            &[
                serde_json::json!({"timestamp":"1","type":"event_msg","payload":{"type":"item_completed","item":{"type":"user_message","message":"future schema user turn"}}}),
                serde_json::json!({"timestamp":"2","type":"event_msg","payload":{"type":"item_completed","item":{"type":"agent_message","message":"future schema assistant turn"}}}),
            ],
        );
        let report = load_session(&source(temp.path(), id), RestoreLimits::default()).unwrap();
        assert!(report
            .messages
            .iter()
            .any(|m| m.role == MessageRole::User && m.text == "future schema user turn"));
        assert!(report
            .messages
            .iter()
            .any(|m| m.role == MessageRole::Assistant && m.text == "future schema assistant turn"));
    }

    #[test]
    fn title_falls_back_to_first_human_prompt_when_untitled() {
        let temp = fixture_home();
        let id = "44444444-4444-4444-8444-444444444444";
        write_session(
            temp.path(),
            id,
            &[
                serde_json::json!({"timestamp":"1","type":"event_msg","payload":{"type":"user_message","message":"Investigate the failing build"}}),
                serde_json::json!({"timestamp":"2","type":"event_msg","payload":{"type":"agent_message","message":"Looking into it"}}),
            ],
        );
        let report = load_session(&source(temp.path(), id), RestoreLimits::default()).unwrap();
        assert_eq!(report.meta.title.as_deref(), Some("Investigate the failing build"));

        let candidates = list_sessions(temp.path(), None, 10).unwrap();
        let candidate = candidates.iter().find(|c| c.id == id).expect("candidate present");
        assert_eq!(candidate.title.as_deref(), Some("Investigate the failing build"));
    }

    #[test]
    fn long_message_bodies_are_tail_truncated_with_marker() {
        let temp = fixture_home();
        let id = "55555555-5555-4555-8555-555555555555";
        let filler = "A".repeat(5000);
        let actionable_tail = "APPROVAL REQUEST: run rm -rf /tmp/example";
        let long_message = format!("{filler}{actionable_tail}");
        write_session(
            temp.path(),
            id,
            &[serde_json::json!({"timestamp":"1","type":"event_msg","payload":{"type":"user_message","message": long_message}})],
        );
        let report = load_session(&source(temp.path(), id), RestoreLimits::default()).unwrap();
        let message = &report.messages[0];
        assert!(message.text.starts_with("[truncated "));
        assert!(
            message.text.ends_with(actionable_tail),
            "tail must keep the actionable ending, got: {}",
            message.text
        );
    }

    #[test]
    fn redactor_removes_credentials_from_all_report_fields() {
        let temp = fixture_home();
        let id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        write_session(
            temp.path(),
            id,
            &[
                serde_json::json!({"timestamp":"1","type":"event_msg","payload":{"type":"user_message","message":"\"api_key\": \"JSON_SECRET_MUST_NOT_ESCAPE\"\nOPENAI_API_KEY=ENV_SECRET_MUST_NOT_ESCAPE\nuse sk-ABCDEFGHIJKLMNOPQRSTUV"}}),
                serde_json::json!({"timestamp":"2","type":"event_msg","payload":{"type":"agent_message","message":"Bearer dotted.secret.value"}}),
            ],
        );
        let report = load_session(&source(temp.path(), id), RestoreLimits::default()).unwrap();
        let encoded = encode_json(&report).unwrap();
        assert!(!encoded.contains("JSON_SECRET_MUST_NOT_ESCAPE"));
        assert!(!encoded.contains("ENV_SECRET_MUST_NOT_ESCAPE"));
        assert!(!encoded.contains("sk-ABCDEFGHIJKLMNOPQRSTUV"));
        assert!(!encoded.contains("dotted.secret.value"));
        assert!(report.redactions >= 4);
        assert_eq!(report.git.repository_label.as_deref(), Some("demo"));
    }

    #[test]
    fn redactor_preserves_noncredential_policy_and_budget_fields() {
        let mut redactions = 0;
        let input = "token_budget=4096\ntoken_count: 20\nauthorization_policy=deny";
        let output = redact_text(input, &mut redactions);
        assert_eq!(output, input);
        assert_eq!(redactions, 0);
    }

    #[test]
    fn resolve_unique_prefix_and_reject_ambiguous() {
        let temp = fixture_home();
        let first = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        let second = "dddddddd-dddd-4ddd-8ddd-ddddddddddde";
        write_session(temp.path(), first, &[]);
        write_session(temp.path(), second, &[]);
        assert!(matches!(
            resolve_target(temp.path(), OsStr::new("dddddddd-dddd-4ddd")),
            Err(RestoreError::AmbiguousPrefix)
        ));
        let resolved = resolve_target(temp.path(), OsStr::new(first)).unwrap();
        assert_eq!(resolved.id, first);
    }

    #[test]
    fn exact_id_resolution_is_not_limited_to_the_newest_hundred_sessions() {
        let temp = fixture_home();
        let target = "00000000-0000-4000-8000-000000000000";
        write_session(temp.path(), target, &[]);
        for index in 1..=120_u64 {
            let id = format!(
                "{index:08x}-0000-4000-8000-{index:012x}"
            );
            write_session(temp.path(), &id, &[]);
        }
        let resolved = resolve_target(temp.path(), OsStr::new(target)).unwrap();
        assert_eq!(resolved.id, target);
    }

    #[test]
    fn bounded_reader_never_reads_more_than_tail_and_keeps_latest_messages() {
        let temp = fixture_home();
        let id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
        let records = (0..200)
            .map(|index| serde_json::json!({"timestamp":index.to_string(),"type":"event_msg","payload":{"type":"user_message","message":format!("message-{index:03}")}}))
            .collect::<Vec<_>>();
        write_session(temp.path(), id, &records);
        let report = load_session(
            &source(temp.path(), id),
            RestoreLimits {
                max_tail_bytes: 4096,
                max_lines: 20,
                max_messages: 3,
            },
        )
        .unwrap();
        assert!(report.truncated);
        assert_eq!(report.messages.len(), 3);
        assert!(report.messages.last().unwrap().text.contains("199"));
    }

    #[test]
    fn active_rollout_growth_cannot_extend_the_opened_snapshot_read() {
        let temp = fixture_home();
        let id = "11111111-1111-4111-8111-111111111111";
        let path = write_session(
            temp.path(),
            id,
            &[serde_json::json!({"timestamp":"1","type":"event_msg","payload":{"type":"user_message","message":"bounded active session"}})],
        );
        let source = source(temp.path(), id);
        let keep_writing = Arc::new(AtomicBool::new(true));
        let writer_flag = Arc::clone(&keep_writing);
        let writer = thread::spawn(move || {
            let mut file = OpenOptions::new().append(true).open(path).unwrap();
            let record = serde_json::json!({"timestamp":"2","type":"event_msg","payload":{"type":"agent_message","message":"active append"}}).to_string();
            while writer_flag.load(Ordering::Acquire) {
                writeln!(file, "{record}").unwrap();
                file.flush().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });
        thread::sleep(Duration::from_millis(20));
        let (send, receive) = mpsc::channel();
        let reader = thread::spawn(move || {
            let result = load_session(
                &source,
                RestoreLimits {
                    max_tail_bytes: 4096,
                    max_lines: 64,
                    max_messages: 8,
                },
            );
            let _ = send.send(result.map(|report| report.messages.len()));
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        keep_writing.store(false, Ordering::Release);
        writer.join().unwrap();
        reader.join().unwrap();
        assert!(result.unwrap().is_ok());
    }

    #[test]
    fn injected_context_is_not_surfaced() {
        let temp = fixture_home();
        let id = "ffffffff-ffff-4fff-8fff-ffffffffffff";
        write_session(
            temp.path(),
            id,
            &[
                serde_json::json!({"timestamp":"1","type":"event_msg","payload":{"type":"user_message","message":"<environment_context>HIDDEN_ENV</environment_context>"}}),
                serde_json::json!({"timestamp":"2","type":"event_msg","payload":{"type":"user_message","message":"Real user decision"}}),
            ],
        );
        let report = load_session(&source(temp.path(), id), RestoreLimits::default()).unwrap();
        assert_eq!(report.messages.len(), 1);
        assert_eq!(report.messages[0].text, "Real user decision");
    }
}
