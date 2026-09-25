//! Open work at end (plan load section 7): the input of the last
//! `TodoWrite` call, or the last `TaskCreate`/`TaskUpdate` call — whichever
//! task-tracking tool the session used most recently — verbatim.

use crate::io::OffsetEvent;
use chrono::{DateTime, Utc};
use claude_session_restore::transcript::events::SessionEvent;
use serde_json::Value as JsonValue;

const OPEN_WORK_TOOLS: [&str; 3] = ["TodoWrite", "TaskCreate", "TaskUpdate"];
/// Bound on how many `TodoWrite` items are individually rendered — a cap on
/// count, never a rewrite of a shown item's own text.
const MAX_TODOS_SHOWN: usize = 20;
/// Char cap for a non-`TodoWrite` (`TaskCreate`/`TaskUpdate`) input, which
/// has no fixed shape to render item-by-item.
const RAW_INPUT_CHAR_CAP: usize = 800;

/// One todo item, verbatim from `TodoWrite`'s own `input.todos[]`.
#[derive(Debug, Clone)]
pub struct TodoItem {
    pub status: String,
    pub content: String,
}

/// The rendered form of the session's last task-tracking call.
#[derive(Debug, Clone)]
pub enum OpenWorkBody {
    Todos { items: Vec<TodoItem>, omitted: usize },
    RawInput { text: String, truncated_chars: Option<usize> },
}

/// The last task-tracking tool call in a window, with its handle.
#[derive(Debug, Clone)]
pub struct OpenWorkItem {
    pub offset: u64,
    pub timestamp: DateTime<Utc>,
    pub tool_name: String,
    pub body: OpenWorkBody,
}

fn render_todos(input: &JsonValue) -> Option<OpenWorkBody> {
    let todos = input.get("todos")?.as_array()?;
    let mut items = Vec::new();
    for todo in todos.iter().take(MAX_TODOS_SHOWN) {
        let status = todo.get("status").and_then(JsonValue::as_str).unwrap_or("?").to_string();
        let content = todo.get("content").and_then(JsonValue::as_str).unwrap_or("").to_string();
        items.push(TodoItem { status, content });
    }
    let omitted = todos.len().saturating_sub(items.len());
    Some(OpenWorkBody::Todos { items, omitted })
}

fn render_raw(input: &JsonValue) -> OpenWorkBody {
    let compact = serde_json::to_string(input).unwrap_or_else(|_| input.to_string());
    let total_chars = compact.chars().count();
    if total_chars <= RAW_INPUT_CHAR_CAP {
        OpenWorkBody::RawInput { text: compact, truncated_chars: None }
    } else {
        let text: String = compact.chars().take(RAW_INPUT_CHAR_CAP).collect();
        OpenWorkBody::RawInput { text, truncated_chars: Some(total_chars - RAW_INPUT_CHAR_CAP) }
    }
}

/// The last task-tracking tool call (`TodoWrite`, `TaskCreate`, or
/// `TaskUpdate`, whichever is most recent) in `window`, verbatim.
#[must_use]
pub fn last_open_work_item(window: &[OffsetEvent]) -> Option<OpenWorkItem> {
    window.iter().rev().find_map(|oe| {
        let SessionEvent::Assistant(assistant) = &oe.event else { return None };
        assistant.message.content.iter().rev().find_map(|block| {
            let (_, name, input) = block.as_tool_use()?;
            if !OPEN_WORK_TOOLS.contains(&name) {
                return None;
            }
            let body = if name == "TodoWrite" {
                render_todos(input).unwrap_or_else(|| render_raw(input))
            } else {
                render_raw(input)
            };
            Some(OpenWorkItem { offset: oe.offset, timestamp: oe.event.timestamp(), tool_name: name.to_string(), body })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(offset: u64, json: &str) -> OffsetEvent {
        OffsetEvent { offset, event: serde_json::from_str(json).expect("fixture event must parse") }
    }

    #[test]
    fn picks_the_last_todo_write_and_renders_items_verbatim() {
        let events = vec![
            event(0, r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"m1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"TodoWrite","input":{"todos":[{"content":"first task","status":"completed","activeForm":"Doing first"}]}}]}}"#),
            event(200, r#"{"type":"assistant","uuid":"a2","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:01:00Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"m2","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"TodoWrite","input":{"todos":[{"content":"second task","status":"in_progress","activeForm":"Doing second"},{"content":"third task","status":"pending","activeForm":"Doing third"}]}}]}}"#),
        ];

        let item = last_open_work_item(&events).expect("open work item");
        assert_eq!(item.offset, 200);
        assert_eq!(item.tool_name, "TodoWrite");
        let OpenWorkBody::Todos { items, omitted } = item.body else { panic!("expected Todos body") };
        assert_eq!(omitted, 0);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].content, "second task");
        assert_eq!(items[0].status, "in_progress");
        assert_eq!(items[1].content, "third task");
    }

    #[test]
    fn task_update_wins_when_it_is_more_recent_than_todo_write() {
        let events = vec![
            event(0, r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"m1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"TodoWrite","input":{"todos":[{"content":"first task","status":"completed","activeForm":"Doing first"}]}}]}}"#),
            event(200, r#"{"type":"assistant","uuid":"a2","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:01:00Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"m2","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"TaskUpdate","input":{"id":"task-7","status":"in_progress"}}]}}"#),
        ];

        let item = last_open_work_item(&events).expect("open work item");
        assert_eq!(item.tool_name, "TaskUpdate");
        let OpenWorkBody::RawInput { text, .. } = item.body else { panic!("expected RawInput body") };
        assert!(text.contains("task-7"));
    }

    #[test]
    fn none_when_no_task_tracking_call_in_window() {
        let events = vec![event(
            0,
            r#"{"type":"assistant","uuid":"a1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"cwd":"/work","message":{"model":"claude-test","id":"m1","type":"message","role":"assistant","content":[{"type":"text","text":"just talking"}]}}"#,
        )];
        assert!(last_open_work_item(&events).is_none());
    }
}
