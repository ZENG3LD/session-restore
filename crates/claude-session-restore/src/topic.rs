//! Topic detection (D3: prefer the provider's own title over any inferred
//! label).
//!
//! Priority order: the session's own `custom-title` (Claude Code's UI
//! title), then `ai-title` (model-generated), then `last-prompt` (latest
//! verbatim human prompt), then the first genuine human-typed prompt in the
//! session (skipping harness-injected meta turns, command-palette
//! injections, and tool-result-only turns) — never a generic placeholder
//! unless none of the above exist at all.

use crate::io::{parse_events, read_head_lines, OffsetEvent};
use claude_session_restore::transcript::events::{IncomingKind, SessionEvent};
use std::fmt;
use std::path::Path;

/// Bounded head read used only for a quick disambiguation label — far
/// cheaper than a full topic detection pass, appropriate for listing a
/// handful of ambiguous-prefix candidates.
const QUICK_TOPIC_HEAD_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicSource {
    CustomTitle,
    AiTitle,
    AgentName,
    LastPrompt,
    FirstPrompt,
    None,
}

impl fmt::Display for TopicSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::CustomTitle => "custom_title",
            Self::AiTitle => "ai_title",
            Self::AgentName => "agent_name",
            Self::LastPrompt => "last_prompt",
            Self::FirstPrompt => "first_prompt",
            Self::None => "none",
        };
        f.write_str(label)
    }
}

fn last_custom_title(events: &[OffsetEvent]) -> Option<String> {
    events.iter().rev().find_map(|oe| match &oe.event {
        SessionEvent::CustomTitle(title) => Some(title.custom_title.clone()),
        _ => None,
    })
}

fn last_ai_title(events: &[OffsetEvent]) -> Option<String> {
    events.iter().rev().find_map(|oe| match &oe.event {
        SessionEvent::AiTitle(title) => Some(title.ai_title.clone()),
        _ => None,
    })
}

fn last_last_prompt(events: &[OffsetEvent]) -> Option<String> {
    events.iter().rev().find_map(|oe| match &oe.event {
        SessionEvent::LastPrompt(prompt) => Some(prompt.last_prompt.clone()),
        _ => None,
    })
}

fn last_agent_name(events: &[OffsetEvent]) -> Option<String> {
    events.iter().rev().find_map(|oe| match &oe.event {
        SessionEvent::AgentName(name) => Some(name.agent_name.clone()),
        _ => None,
    })
}

/// First genuine owner turn (a topic fallback) — excludes slash commands,
/// peer/task-notification messages, and harness notifications, per
/// [`IncomingKind`].
fn first_human_prompt(events: &[OffsetEvent]) -> Option<String> {
    events.iter().find_map(|oe| match &oe.event {
        SessionEvent::User(user) => match user.incoming_kind() {
            IncomingKind::Owner(text) => Some(text),
            _ => None,
        },
        _ => None,
    })
}

/// Pick the topic per the priority order documented on this module
/// (`custom-title` > `ai-title` > `agent-name` > `last-prompt` > first owner
/// prompt): the tail window is checked first (cheap, and titles repeat
/// through the file so the tail almost always carries the latest one), then
/// the head window as a fallback for sessions whose tail window missed
/// every repeat.
pub fn detect_topic(head: &[OffsetEvent], tail: &[OffsetEvent]) -> (String, TopicSource) {
    if let Some(title) = last_custom_title(tail) {
        return (title, TopicSource::CustomTitle);
    }
    if let Some(title) = last_ai_title(tail) {
        return (title, TopicSource::AiTitle);
    }
    if let Some(name) = last_agent_name(tail) {
        return (name, TopicSource::AgentName);
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
    if let Some(name) = last_agent_name(head) {
        return (name, TopicSource::AgentName);
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

/// The first 3 genuine owner prompts of the session, read from the head of
/// the file (before any compaction rewrote later history), each paired with
/// its handle.
pub fn first_human_prompts(head: &[OffsetEvent], limit: usize) -> Vec<(u64, String)> {
    head.iter()
        .filter_map(|oe| match &oe.event {
            SessionEvent::User(user) => match user.incoming_kind() {
                IncomingKind::Owner(text) => Some((oe.offset, text)),
                _ => None,
            },
            _ => None,
        })
        .take(limit)
        .collect()
}

/// Best-effort topic label for a single candidate path, used only to
/// disambiguate a non-unique UUID prefix. Cheap: head-only, no tail read.
pub fn quick_topic(path: &Path) -> String {
    let Ok(lines) = read_head_lines(path, QUICK_TOPIC_HEAD_BYTES) else {
        return "<unreadable>".to_string();
    };
    let events = parse_events(&lines);
    detect_topic(&events, &[]).0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(offset: u64, json: &str) -> OffsetEvent {
        OffsetEvent { offset, event: serde_json::from_str(json).expect("fixture event must parse") }
    }

    #[test]
    fn detect_topic_prefers_custom_title_over_everything() {
        let tail = vec![
            event(0, r#"{"type":"last-prompt","lastPrompt":"placeholder prompt","sessionId":"s"}"#),
            event(1, r#"{"type":"ai-title","aiTitle":"placeholder ai title","sessionId":"s"}"#),
            event(2, r#"{"type":"custom-title","customTitle":"placeholder custom title","sessionId":"s"}"#),
        ];
        let (topic, source) = detect_topic(&[], &tail);
        assert_eq!(topic, "placeholder custom title");
        assert_eq!(source.to_string(), "custom_title");
    }

    #[test]
    fn detect_topic_falls_back_to_ai_title_then_last_prompt() {
        let ai_only =
            vec![event(0, r#"{"type":"ai-title","aiTitle":"placeholder ai title","sessionId":"s"}"#)];
        assert_eq!(detect_topic(&[], &ai_only).0, "placeholder ai title");

        let last_prompt_only =
            vec![event(0, r#"{"type":"last-prompt","lastPrompt":"placeholder prompt","sessionId":"s"}"#)];
        let (topic, source) = detect_topic(&[], &last_prompt_only);
        assert_eq!(topic, "placeholder prompt");
        assert_eq!(source.to_string(), "last_prompt");
    }

    #[test]
    fn detect_topic_falls_back_to_head_window_then_first_human_prompt() {
        let head =
            vec![event(0, r#"{"type":"custom-title","customTitle":"placeholder head title","sessionId":"s"}"#)];
        let (topic, source) = detect_topic(&head, &[]);
        assert_eq!(topic, "placeholder head title");
        assert_eq!(source.to_string(), "custom_title");

        let head_prompt_only = vec![event(
            0,
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
    fn detect_topic_agent_name_wins_over_last_prompt_but_not_ai_title() {
        let ai_and_agent_name = vec![
            event(0, r#"{"type":"last-prompt","lastPrompt":"placeholder prompt","sessionId":"s"}"#),
            event(1, r#"{"type":"agent-name","agentName":"placeholder agent","sessionId":"s"}"#),
        ];
        let (topic, source) = detect_topic(&[], &ai_and_agent_name);
        assert_eq!(topic, "placeholder agent");
        assert_eq!(source.to_string(), "agent_name");

        let ai_title_still_wins = vec![
            event(0, r#"{"type":"agent-name","agentName":"placeholder agent","sessionId":"s"}"#),
            event(1, r#"{"type":"ai-title","aiTitle":"placeholder ai title","sessionId":"s"}"#),
        ];
        assert_eq!(detect_topic(&[], &ai_title_still_wins).0, "placeholder ai title");
    }

    #[test]
    fn detect_topic_takes_the_last_custom_title_when_it_repeats() {
        let tail = vec![
            event(0, r#"{"type":"custom-title","customTitle":"placeholder first","sessionId":"s"}"#),
            event(1, r#"{"type":"custom-title","customTitle":"placeholder second","sessionId":"s"}"#),
        ];
        assert_eq!(detect_topic(&[], &tail).0, "placeholder second");
    }

    #[test]
    fn first_human_prompts_skips_notifications_caps_at_limit_and_carries_offsets() {
        let head = vec![
            event(10, r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2024-01-01T00:00:00Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"first"}}"#),
            event(20, r#"{"type":"user","uuid":"u2","sessionId":"s","timestamp":"2024-01-01T00:00:01Z","isSidechain":false,"userType":"external","isMeta":true,"cwd":"/work","message":{"role":"user","content":"<local-command-caveat>skip</local-command-caveat>"}}"#),
            event(30, r#"{"type":"user","uuid":"u3","sessionId":"s","timestamp":"2024-01-01T00:00:02Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"second"}}"#),
            event(40, r#"{"type":"user","uuid":"u4","sessionId":"s","timestamp":"2024-01-01T00:00:03Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"third"}}"#),
            event(50, r#"{"type":"user","uuid":"u5","sessionId":"s","timestamp":"2024-01-01T00:00:04Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"fourth"}}"#),
        ];
        assert_eq!(
            first_human_prompts(&head, 3),
            vec![(10, "first".to_string()), (30, "second".to_string()), (40, "third".to_string())]
        );
    }
}
