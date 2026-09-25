//! Shared digest primitives: last main-chain assistant reports, and the
//! tool-operations/errors/files-touched footer. Used by both `load` (the
//! main session, plus its commit-hint scan on top) and `agent` (a
//! subagent's own transcript) — the two places a bounded window of
//! [`OffsetEvent`]s gets turned into a "what happened, what changed, what
//! broke" digest.

use crate::format::truncate;
use crate::io::OffsetEvent;
use claude_session_restore::transcript::events::{IncomingKind, SessionEvent};
use regex::Regex;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::OnceLock;

/// One main-chain assistant text block, with its handle.
#[derive(Debug, Clone)]
pub struct AgentReport {
    pub offset: u64,
    pub text: String,
}

/// The last `count` assistant text blocks in `window`, oldest first.
///
/// `include_sidechain` decides whether `isSidechain: true` turns count:
/// - `false` (the main session, via `load`) — skip them; they are a
///   delegated subagent's activity replayed inline in the parent file, not
///   the parent's own report.
/// - `true` (a subagent's own transcript file, via `agent`) — include them;
///   every assistant turn in a subagent's own file is marked
///   `isSidechain: true` from the parent's point of view, but it is that
///   subagent's *main* chain from its own. Skipping it there would silently
///   drop the subagent's entire final report.
#[must_use]
pub fn last_agent_reports(window: &[OffsetEvent], count: usize, include_sidechain: bool) -> Vec<AgentReport> {
    let mut reports = Vec::new();
    for oe in window {
        let SessionEvent::Assistant(assistant) = &oe.event else { continue };
        if assistant.metadata.is_sidechain && !include_sidechain {
            continue;
        }
        for block in &assistant.message.content {
            if let Some(text) = block.as_text() {
                reports.push(AgentReport { offset: oe.offset, text: text.to_string() });
            }
        }
    }
    if reports.len() > count {
        let drop = reports.len() - count;
        reports.drain(..drop);
    }
    reports
}

/// One tool invocation, one line, with its handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOpRecord {
    pub offset: u64,
    pub text: String,
}

/// One system error, with its handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorRecord {
    pub offset: u64,
    pub text: String,
}

/// One distinct file edited by `Write`/`Edit`/`MultiEdit`/`NotebookEdit`
/// (reads do not count), deduped with an edit count and the handle of its most
/// recent edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTouch {
    pub path: String,
    pub count: u32,
    pub last_offset: u64,
}

#[derive(Debug, Clone, Default)]
pub struct FooterDigest {
    pub tool_operations: Vec<ToolOpRecord>,
    pub errors: Vec<ErrorRecord>,
    /// Sorted most-recently-touched first.
    pub files: Vec<FileTouch>,
}

/// Build the tool-operations/errors/files-touched footer over `window`. The
/// caller truncates each list to its own display cap.
#[must_use]
pub fn build_footer(window: &[OffsetEvent]) -> FooterDigest {
    let mut tool_operations = Vec::new();
    let mut errors = Vec::new();
    let mut touches: Vec<(String, u64)> = Vec::new();

    for oe in window {
        match &oe.event {
            SessionEvent::Assistant(assistant) => {
                for block in &assistant.message.content {
                    if let Some((_, name, input)) = block.as_tool_use() {
                        tool_operations.push(ToolOpRecord { offset: oe.offset, text: describe_tool_use(name, input) });
                        if let Some(path) = file_touch_path(name, input) {
                            touches.push((path, oe.offset));
                        }
                    }
                }
            }
            SessionEvent::System(sys) if sys.is_error() => {
                let message = sys
                    .error
                    .as_ref()
                    .map(|error| format!("{}: {}", error.error_type, error.message))
                    .or_else(|| sys.content.clone())
                    .unwrap_or_else(|| "unspecified system error".to_string());
                errors.push(ErrorRecord { offset: oe.offset, text: message });
            }
            _ => {}
        }
    }

    FooterDigest { tool_operations, errors, files: dedupe_files(touches) }
}

fn dedupe_files(touches: Vec<(String, u64)>) -> Vec<FileTouch> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut last_offset: HashMap<String, u64> = HashMap::new();
    for (path, offset) in &touches {
        *counts.entry(path.clone()).or_insert(0) += 1;
        last_offset.insert(path.clone(), *offset);
    }
    let mut files: Vec<FileTouch> = counts
        .into_iter()
        .map(|(path, count)| {
            let last = last_offset[&path];
            FileTouch { path, count, last_offset: last }
        })
        .collect();
    files.sort_by_key(|file| std::cmp::Reverse(file.last_offset));
    files
}

fn file_touch_path(name: &str, input: &JsonValue) -> Option<String> {
    if !matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit") {
        return None;
    }
    let path = input
        .get("file_path")
        .or_else(|| input.get("filePath"))
        .or_else(|| input.get("notebook_path"))
        .and_then(JsonValue::as_str)?;
    Some(path.replace('\\', "/"))
}

/// One-line description of a tool invocation including its most relevant
/// argument. `pub(crate)` — `span` reuses this for its "tool calls one
/// line" rendering.
pub(crate) fn describe_tool_use(name: &str, input: &JsonValue) -> String {
    let key_arg = match name {
        "Bash" => input.get("command").and_then(JsonValue::as_str),
        "Read" | "Write" | "Edit" | "NotebookEdit" => {
            input.get("file_path").or_else(|| input.get("filePath")).and_then(JsonValue::as_str)
        }
        "Grep" | "Glob" => input.get("pattern").and_then(JsonValue::as_str),
        "WebSearch" => input.get("query").and_then(JsonValue::as_str),
        "WebFetch" => input.get("url").and_then(JsonValue::as_str),
        "Task" | "Agent" => {
            input.get("description").and_then(JsonValue::as_str).or_else(|| input.get("subagent_type").and_then(JsonValue::as_str))
        }
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

fn commit_hint_patterns() -> &'static [Regex; 5] {
    static PATTERNS: OnceLock<[Regex; 5]> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            Regex::new(r"feat\(([^)]+)\)").expect("static regex"),
            Regex::new(r"fix\(([^)]+)\)").expect("static regex"),
            Regex::new(r"refactor\(([^)]+)\)").expect("static regex"),
            Regex::new(r"chore\(([^)]+)\)").expect("static regex"),
            Regex::new(r"test\(([^)]+)\)").expect("static regex"),
        ]
    })
}

/// Free-text commit-flavoured hints (`feat(scope)`-style prefixes and path
/// mentions) — the fallback line kept alongside the real, confirmed commits
/// in [`crate::commits`]. Main-session only; never guessed to be a real
/// commit.
#[must_use]
pub fn commit_hint_scan(window: &[OffsetEvent]) -> Vec<String> {
    let mut hints: std::collections::HashSet<String> = std::collections::HashSet::new();
    for oe in window {
        match &oe.event {
            SessionEvent::Assistant(assistant) => {
                for block in &assistant.message.content {
                    if let Some(text) = block.as_text() {
                        scan_text_for_hints(text, &mut hints);
                    }
                }
            }
            SessionEvent::User(user) => {
                if let IncomingKind::Owner(text) = user.incoming_kind() {
                    scan_text_for_hints(&text, &mut hints);
                }
            }
            _ => {}
        }
    }
    let mut hints: Vec<String> = hints.into_iter().collect();
    hints.sort();
    hints
}

fn scan_text_for_hints(text: &str, hints: &mut std::collections::HashSet<String>) {
    for pattern in commit_hint_patterns() {
        for cap in pattern.captures_iter(text) {
            if let Some(scope) = cap.get(1) {
                hints.insert(scope.as_str().to_string());
            }
        }
    }
    for word in text.split_whitespace() {
        if word.contains("v5/") || word.contains("connectors/") || word.contains("ui/") {
            hints.insert(word.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(offset: u64, json: &str) -> OffsetEvent {
        OffsetEvent { offset, event: serde_json::from_str(json).expect("fixture event must parse") }
    }

    #[test]
    fn last_agent_reports_skips_sidechain_and_caps_the_tail() {
        let mut events = Vec::new();
        for i in 0..7 {
            events.push(event(
                i * 10,
                &format!(
                    r#"{{"type":"assistant","uuid":"a{i}","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:0{i}Z","isSidechain":false,"cwd":"/work","message":{{"model":"claude-test","id":"msg_{i}","type":"message","role":"assistant","content":[{{"type":"text","text":"report {i}"}}]}}}}"#
                ),
            ));
        }
        events.push(event(
            999,
            r#"{"type":"assistant","uuid":"side","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:09Z","isSidechain":true,"cwd":"/work","message":{"model":"claude-test","id":"msg_side","type":"message","role":"assistant","content":[{"type":"text","text":"sidechain report"}]}}"#,
        ));

        let reports = last_agent_reports(&events, 5, false);
        assert_eq!(reports.len(), 5);
        assert_eq!(reports.last().map(|r| r.text.as_str()), Some("report 6"));
        assert!(reports.iter().all(|r| r.text != "sidechain report"));

        // A subagent's own transcript file marks every one of its own
        // assistant turns `isSidechain: true` — `include_sidechain: true`
        // must keep them, or a subagent's final report vanishes.
        let with_sidechain = last_agent_reports(&events, 8, true);
        assert!(with_sidechain.iter().any(|r| r.text == "sidechain report"));
        assert_eq!(with_sidechain.last().map(|r| r.text.as_str()), Some("sidechain report"));
    }

    #[test]
    fn build_footer_collects_tool_operations_errors_and_deduped_files() {
        let events = vec![
            event(0, r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"msg_1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"/work/src/lib.rs"}}]}}"#),
            event(200, r#"{"type":"assistant","uuid":"a2","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:02Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"msg_2","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tool-2","name":"Edit","input":{"file_path":"/work/src/lib.rs"}}]}}"#),
            event(300, r#"{"type":"system","subtype":"error","uuid":"sys1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:01Z","isSidechain":false,"cwd":"/work","error":{"type":"overloaded_error","message":"overloaded"}}"#),
        ];
        let footer = build_footer(&events);
        assert_eq!(footer.tool_operations.len(), 2);
        assert_eq!(footer.tool_operations[0].offset, 0);
        assert_eq!(footer.errors, vec![ErrorRecord { offset: 300, text: "overloaded_error: overloaded".to_string() }]);
        // The Read at offset 0 is not an edit; only the Edit counts.
        assert_eq!(footer.files.len(), 1);
        assert_eq!(footer.files[0].path, "/work/src/lib.rs");
        assert_eq!(footer.files[0].count, 1);
        assert_eq!(footer.files[0].last_offset, 200);
    }

    #[test]
    fn build_footer_files_ignore_reads() {
        let events = vec![event(
            0,
            r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"msg_1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"/tmp/task.output"}}]}}"#,
        )];
        let footer = build_footer(&events);
        assert_eq!(footer.tool_operations.len(), 1);
        assert!(footer.files.is_empty());
    }

    #[test]
    fn commit_hint_scan_picks_up_owner_text_via_no_origin_fallback() {
        let events = vec![event(
            0,
            r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"chore(build): tidy up v5/ connectors/"}}"#,
        )];
        let hints = commit_hint_scan(&events);
        assert!(hints.contains(&"build".to_string()));
    }
}
