//! Commits made in this session (plan load section 10): parsed from the
//! output of `git commit` Bash tool calls, never guessed from free text.
//! `git commit`'s own confirmation line (`[branch abc1234] subject`, or
//! `[branch (root-commit) abc1234] subject` for a repo's first commit) is
//! the ground truth; a commit-flavoured word seen in owner/agent prose is
//! kept only as the pre-existing free-text fallback (`extract_commit_hints`
//! in `load_cmd`), never promoted into this list.

use crate::io::OffsetEvent;
use claude_session_restore::transcript::events::{ContentBlock, SessionEvent};
use regex::Regex;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::OnceLock;

/// One real commit made during the session.
#[derive(Debug, Clone)]
pub struct CommitRecord {
    /// Handle of the `Bash` tool-use line that ran `git commit`.
    pub offset: u64,
    pub repo: Option<String>,
    pub branch: String,
    pub hash: String,
    pub subject: String,
}

fn commit_line_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)^\[(.+?)\s+([0-9a-f]{7,40})\]\s+(.+)$").expect("static commit-line regex must compile")
    })
}

/// Pull the path after `git -C <path>` (quoted or bare) out of a Bash
/// command string, when present.
fn repo_from_command(command: &str) -> Option<String> {
    let index = command.find("-C ")?;
    let rest = command[index + 3..].trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        return Some(stripped[..end].to_string());
    }
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let path = &rest[..end];
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

struct PendingCommit {
    offset: u64,
    repo: Option<String>,
}

fn tool_result_text(content: &JsonValue) -> Option<String> {
    match content {
        JsonValue::String(text) => Some(text.clone()),
        JsonValue::Array(items) => {
            items.iter().find_map(|item| item.get("text").and_then(JsonValue::as_str)).map(str::to_string)
        }
        _ => None,
    }
}

/// Scan `window` for `git commit` Bash calls and their results, in file
/// order. `fallback_cwd` (the session's own cwd) is used as the repo when a
/// command carries no `git -C <path>`.
#[must_use]
pub fn scan(window: &[OffsetEvent], fallback_cwd: Option<&str>) -> Vec<CommitRecord> {
    let mut pending: HashMap<String, PendingCommit> = HashMap::new();
    let mut commits = Vec::new();

    for oe in window {
        match &oe.event {
            SessionEvent::Assistant(assistant) => {
                for block in &assistant.message.content {
                    let Some((id, name, input)) = block.as_tool_use() else { continue };
                    if name != "Bash" {
                        continue;
                    }
                    let Some(command) = input.get("command").and_then(JsonValue::as_str) else { continue };
                    // `git -C <path> commit …` puts other tokens between
                    // `git` and `commit`, so a plain substring check on
                    // `"git commit"` misses it — match on tokens instead.
                    let is_git_commit =
                        command.contains("git") && command.split_whitespace().any(|word| word == "commit");
                    if !is_git_commit {
                        continue;
                    }
                    let repo = repo_from_command(command).or_else(|| fallback_cwd.map(str::to_string));
                    pending.insert(id.to_string(), PendingCommit { offset: oe.offset, repo });
                }
            }
            SessionEvent::User(user) => {
                for block in &user.message.content {
                    let ContentBlock::ToolResult(result) = block else { continue };
                    let Some(pending_commit) = pending.remove(&result.tool_use_id) else { continue };
                    let Some(text) = tool_result_text(&result.content) else { continue };
                    for captures in commit_line_regex().captures_iter(&text) {
                        commits.push(CommitRecord {
                            offset: pending_commit.offset,
                            repo: pending_commit.repo.clone(),
                            branch: captures[1].trim().to_string(),
                            hash: captures[2].to_string(),
                            subject: captures[3].trim().to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    commits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(offset: u64, json: &str) -> OffsetEvent {
        OffsetEvent { offset, event: serde_json::from_str(json).expect("fixture event must parse") }
    }

    #[test]
    fn finds_a_commit_confirmed_by_git_commits_own_output() {
        let events = vec![
            event(0, r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"m1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"git -C /work/session-restore commit -m 'fix: repair parser'","description":"commit"}}]}}"#),
            event(200, r#"{"type":"user","uuid":"u1","parentUuid":"a1","sessionId":"s","timestamp":"2026-01-01T00:00:01Z","isSidechain":false,"userType":"internal","cwd":"/work","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"[main abc1234] fix: repair parser\n 2 files changed, 10 insertions(+)"}]}}"#),
        ];

        let commits = scan(&events, Some("/work"));
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].offset, 0);
        assert_eq!(commits[0].repo.as_deref(), Some("/work/session-restore"));
        assert_eq!(commits[0].branch, "main");
        assert_eq!(commits[0].hash, "abc1234");
        assert_eq!(commits[0].subject, "fix: repair parser");
    }

    #[test]
    fn falls_back_to_session_cwd_when_no_dash_c_flag() {
        let events = vec![
            event(0, r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"m1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"git commit -m 'chore: tidy'","description":"commit"}}]}}"#),
            event(200, r#"{"type":"user","uuid":"u1","parentUuid":"a1","sessionId":"s","timestamp":"2026-01-01T00:00:01Z","isSidechain":false,"userType":"internal","cwd":"/work","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"[main (root-commit) 0000abc] chore: tidy"}]}}"#),
        ];

        let commits = scan(&events, Some("/work/session-restore"));
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].repo.as_deref(), Some("/work/session-restore"));
        assert_eq!(commits[0].branch, "main (root-commit)");
        assert_eq!(commits[0].hash, "0000abc");
        assert_eq!(commits[0].subject, "chore: tidy");
    }

    #[test]
    fn ignores_bash_calls_that_are_not_git_commit() {
        let events = vec![
            event(0, r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"m1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"git status","description":"status"}}]}}"#),
            event(200, r#"{"type":"user","uuid":"u1","parentUuid":"a1","sessionId":"s","timestamp":"2026-01-01T00:00:01Z","isSidechain":false,"userType":"internal","cwd":"/work","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"nothing to commit, working tree clean"}]}}"#),
        ];

        let commits = scan(&events, Some("/work"));
        assert!(commits.is_empty());
    }
}
