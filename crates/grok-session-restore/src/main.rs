//! grok-session-restore — list and load Grok CLI sessions for context restoration.
//!
//! Session layout (under $GROK_HOME or ~/.grok):
//!   sessions/<url-encoded-cwd>/<session-id>/summary.json   — index
//!   sessions/<url-encoded-cwd>/<session-id>/updates.jsonl  — ACP conversation log
//!   sessions/<url-encoded-cwd>/<session-id>/chat_history.jsonl
//!   sessions/<url-encoded-cwd>/<session-id>/signals.json
//!   sessions/<url-encoded-cwd>/<session-id>/plan.json
//!   sessions/<url-encoded-cwd>/<session-id>/compaction/    — segment summaries
//!   sessions/<url-encoded-cwd>/<session-id>/subagents/     — child session ids
//!   active_sessions.json                                  — live pids
//!
//! Memory is a separate plane under ~/.grok/memory/ and is only path-listed.
//!
//! # Topic selection
//!
//! `summary.json`'s own `generated_title`/`session_summary` wins when
//! non-empty. Otherwise the topic falls back to the first non-synthetic human
//! prompt found in `chat_history.jsonl` (a bounded head scan, then the tail
//! digest as a last resort) — never left blank when real content exists.
//!
//! # Digest is verbatim, not a summary
//!
//! `load`'s report never paraphrases the end of a session: the last human
//! prompts, the last assistant texts, and the last tool operations (calls
//! with their key argument, plus outputs and failures) are printed as direct
//! quotes from `updates.jsonl`/`chat_history.jsonl`, in chronological order,
//! newest last. `summary.json`'s own `last_turn_summary` and any compaction
//! segment body are the *store's own* LLM-written paraphrases — they are
//! shown separately, clearly labeled, and never substituted for the verbatim
//! digest. The compaction body itself is hidden by default; `--full-summary`
//! opts in.
//!
//! # Reads are byte-budgeted, not full-file scans
//!
//! Both `updates.jsonl` and `chat_history.jsonl` are read via a bounded tail
//! window (`DEFAULT_TAIL_BYTES`), and the topic fallback uses a bounded head
//! window (`HEAD_FALLBACK_BYTES`), so even a multi-hundred-megabyte session
//! loads in a fraction of a second.

use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Tail byte budget shared by `updates.jsonl` and `chat_history.jsonl` — a
/// single seek plus one bounded read, so cost is proportional to this
/// constant, not to file size.
const DEFAULT_TAIL_BYTES: u64 = 8 * 1024 * 1024;
/// Head byte budget for the title/first-prompt fallback scan.
const HEAD_FALLBACK_BYTES: u64 = 1024 * 1024;
/// Cap on displayed user messages (from the tail-bounded scan).
const MAX_USER_LIST: usize = 80;
/// How many of the most recent assistant texts are kept verbatim.
const ASSISTANT_MAX_ITEMS: usize = 10;
/// How many of the most recent tool operations (calls, outputs, failures)
/// are kept for the "Recent Tool Operations" section.
const TOOL_OPS_MAX: usize = 15;
/// How many of the most recent tool failures are kept for the "Errors"
/// section (in addition to appearing inline in Recent Tool Operations).
const ERRORS_MAX: usize = 10;
const MAX_FILES: usize = 80;
const SUMMARY_CHARS: usize = 4000;

/// JSON report schema tag for `list --json`.
const SCHEMA_LIST: &str = "grok-session-restore-list-v1";
/// JSON report schema tag for `load --json`.
const SCHEMA_LOAD: &str = "grok-session-restore-load-v1";

const USAGE: &str = "grok-session-restore — restore context from Grok CLI sessions\n\
\n\
USAGE:\n\
\x20 grok-session-restore list [--max-age-hours N] [--all] [--include-subagents] [--home PATH] [--json]\n\
\x20 grok-session-restore load <session-dir | session-id-prefix | updates.jsonl> [--full-summary] [--home PATH] [--json]\n\
\x20 grok-session-restore --help | -h\n\
\n\
list  — recent parent sessions: id, time, size, cwd, title, last turn\n\
load  — deep dive: verbatim user/assistant/tool digest, tool histogram,\n\
\x20       files touched, plan todos, subagents, compaction, memory paths\n\
\n\
FLAGS:\n\
\x20 --home PATH           Grok home containing sessions/ (default: $GROK_HOME or ~/.grok)\n\
\x20 --max-age-hours N     list: only sessions updated within the last N hours (default: 12)\n\
\x20 --all                 list: ignore --max-age-hours, show every session\n\
\x20 --include-subagents   list: also show session_kind=subagent rows\n\
\x20 --full-summary        load: print the last compaction summary in full (LLM-written, hidden by default)\n\
\x20 --json                emit machine-readable JSON instead of the human report";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage_error();
        std::process::exit(2);
    }
    if args[0] == "help" || args[0] == "--help" || args[0] == "-h" {
        print_help();
        return;
    }
    let home = grok_home();
    match args[0].as_str() {
        "list" => cmd_list(&home, &args[1..]),
        "load" => cmd_load(&home, &args[1..]),
        other => {
            eprintln!("unknown command: {other}");
            print_usage_error();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!("{USAGE}");
}

fn print_usage_error() {
    eprintln!("{USAGE}");
}

fn has_help_flag(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

fn grok_home() -> PathBuf {
    if let Ok(h) = std::env::var("GROK_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".grok")
}

// ---------------------------------------------------------------- list

struct SessionEntry {
    dir: PathBuf,
    id: String,
    updated_ms: i64,
    size: u64,
    title: String,
    topic_source: &'static str,
    cwd: String,
    kind: String,
    last_turn: String,
    model: String,
    agent: String,
    live: bool,
}

fn live_ids(home: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let p = home.join("active_sessions.json");
    let Ok(txt) = fs::read_to_string(&p) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return out;
    };
    if let Some(arr) = v.as_array() {
        for row in arr {
            if let Some(id) = row["session_id"].as_str() {
                out.insert(id.to_string());
            }
        }
    }
    out
}

fn scan_sessions(home: &Path) -> Vec<SessionEntry> {
    let live = live_ids(home);
    let mut out = Vec::new();
    let root = home.join("sessions");
    let Ok(groups) = fs::read_dir(&root) else {
        return out;
    };
    for group in groups.flatten() {
        let gpath = group.path();
        if !gpath.is_dir() {
            continue;
        }
        let group_cwd = read_group_cwd(&gpath);
        let Ok(sessions) = fs::read_dir(&gpath) else {
            continue;
        };
        for s in sessions.flatten() {
            let dir = s.path();
            if !dir.is_dir() {
                continue;
            }
            let summary_path = dir.join("summary.json");
            if !summary_path.exists() {
                continue;
            }
            let id = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let summary = read_json(&summary_path);
            let kind = summary["session_kind"]
                .as_str()
                .unwrap_or("parent")
                .to_string();
            let mut title = first_str(&summary, &["generated_title", "session_summary"]);
            let mut topic_source: &'static str = if title.is_empty() { "none" } else { "title" };
            if title.is_empty() {
                if let Some(p) =
                    first_human_prompt_from_chat(&dir.join("chat_history.jsonl"), HEAD_FALLBACK_BYTES)
                {
                    title = p;
                    topic_source = "first_prompt";
                }
            }
            let cwd = summary["info"]["cwd"]
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| group_cwd.clone())
                .unwrap_or_default();
            let updated_ms = iso_to_ms(
                summary["last_active_at"]
                    .as_str()
                    .or_else(|| summary["updated_at"].as_str())
                    .unwrap_or(""),
            )
            .unwrap_or_else(|| mtime_ms(&summary_path));
            let size = file_len(&dir.join("updates.jsonl")) + file_len(&dir.join("chat_history.jsonl"));
            out.push(SessionEntry {
                dir,
                live: live.contains(&id),
                id,
                updated_ms,
                size,
                title,
                topic_source,
                cwd,
                kind,
                last_turn: summary["last_turn_summary"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                model: summary["current_model_id"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                agent: summary["agent_name"].as_str().unwrap_or("").to_string(),
            });
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.updated_ms));
    out
}

fn read_group_cwd(group: &Path) -> Option<String> {
    let cwd_file = group.join(".cwd");
    fs::read_to_string(cwd_file)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[derive(Serialize)]
struct SessionListEntryJson {
    id: String,
    dir: String,
    updated_at: Option<String>,
    live: bool,
    size_bytes: u64,
    kind: String,
    cwd: String,
    title: String,
    topic_source: &'static str,
    last_turn: String,
    model: String,
    agent: String,
}

#[derive(Serialize)]
struct SessionListReportJson {
    schema: &'static str,
    window_hours: Option<f64>,
    sessions: Vec<SessionListEntryJson>,
}

fn cmd_list(home: &Path, args: &[String]) {
    if has_help_flag(args) {
        print_help();
        return;
    }
    let mut max_age_hours: Option<f64> = Some(12.0);
    let mut include_subagents = false;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max-age-hours" => {
                max_age_hours = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--all" => max_age_hours = None,
            "--include-subagents" => include_subagents = true,
            "--home" => {
                i += 1;
            }
            "--json" => json = true,
            other if other.starts_with("--") => eprintln!("warning: unknown flag: {other}"),
            _ => {}
        }
        i += 1;
    }
    let home = override_home(home, args);
    let now = now_ms();
    let sessions = scan_sessions(&home);
    let mut shown: Vec<&SessionEntry> = Vec::new();
    for e in &sessions {
        if !include_subagents && e.kind == "subagent" {
            continue;
        }
        if let Some(h) = max_age_hours {
            if now - e.updated_ms > (h * 3_600_000.0) as i64 {
                continue;
            }
        }
        shown.push(e);
    }

    if json {
        let entries = shown
            .iter()
            .map(|e| SessionListEntryJson {
                id: e.id.clone(),
                dir: e.dir.display().to_string(),
                updated_at: iso_utc_ms(e.updated_ms),
                live: e.live,
                size_bytes: e.size,
                kind: e.kind.clone(),
                cwd: e.cwd.clone(),
                title: e.title.clone(),
                topic_source: e.topic_source,
                last_turn: e.last_turn.clone(),
                model: e.model.clone(),
                agent: e.agent.clone(),
            })
            .collect();
        let report = SessionListReportJson {
            schema: SCHEMA_LIST,
            window_hours: max_age_hours,
            sessions: entries,
        };
        match serde_json::to_string_pretty(&report) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to encode JSON: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if shown.is_empty() {
        println!(
            "No Grok sessions found (window: {max_age_hours:?} hours). Try --all or --include-subagents."
        );
        return;
    }
    for (i, e) in shown.iter().enumerate() {
        let size = if e.size > 0 {
            format!("{:.2} MB", e.size as f64 / 1e6)
        } else {
            "empty".to_string()
        };
        let live = if e.live { " [live]" } else { "" };
        println!(
            "{}. {}{}\n   {} | {} | {} | {}",
            i + 1,
            e.id,
            live,
            fmt_local_ms(e.updated_ms),
            size,
            e.kind,
            e.cwd
        );
        if !e.title.is_empty() {
            println!("   📌 {}", truncate(&e.title, 140));
        }
        if !e.last_turn.is_empty() {
            println!("   💬 last: {}", truncate(&one_line(&e.last_turn), 140));
        }
        if !e.model.is_empty() || !e.agent.is_empty() {
            println!("   ⚙ {} | agent={}", e.model, e.agent);
        }
        println!();
    }
    println!("To load a session, use:");
    for (i, e) in shown.iter().enumerate() {
        println!("  {}. grok-session-restore load \"{}\"", i + 1, e.dir.display());
    }
}

fn override_home(default: &Path, args: &[String]) -> PathBuf {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--home" {
            if let Some(p) = args.get(i + 1) {
                return PathBuf::from(p);
            }
        }
        i += 1;
    }
    default.to_path_buf()
}

// ---------------------------------------------------------------- load

fn resolve_target(home: &Path, target: &str) -> Option<PathBuf> {
    let p = PathBuf::from(target);
    if p.is_file() {
        return p.parent().map(|d| d.to_path_buf());
    }
    if p.is_dir() && p.join("summary.json").exists() {
        return Some(p);
    }
    let mut matches = Vec::new();
    for e in scan_sessions(home) {
        if e.id == target || e.id.starts_with(target) {
            matches.push(e.dir);
        }
    }
    if matches.len() == 1 {
        return matches.pop();
    }
    None
}

#[derive(Serialize)]
struct ToolCountJson {
    name: String,
    count: u64,
}

#[derive(Serialize)]
struct TodoJson {
    id: String,
    status: String,
    content: String,
}

#[derive(Serialize)]
struct SessionLoadReportJson {
    schema: &'static str,
    session_id: String,
    dir: String,
    workdir: String,
    kind: String,
    live: bool,
    title: String,
    topic_source: &'static str,
    created_at: String,
    updated_at: String,
    model: String,
    agent: String,
    effort: String,
    git_branch: String,
    git_head: String,
    updates_size_bytes: u64,
    chat_size_bytes: u64,
    num_messages: u64,
    num_chat_messages: u64,
    /// The store's own paraphrase of the last turn — NOT a session event.
    /// Kept for transparency only; never treated as the digest.
    store_last_turn_note: String,
    signals_turns: u64,
    signals_user: u64,
    signals_assistant: u64,
    signals_tools: u64,
    signals_compactions: u64,
    signals_tools_used: Vec<String>,
    signals_context_used: u64,
    signals_context_window: u64,
    user_messages: Vec<String>,
    user_messages_total: u64,
    chat_tail_truncated: bool,
    assistant_texts: Vec<String>,
    assistant_texts_total: u64,
    updates_tail_truncated: bool,
    tool_operations: Vec<String>,
    tool_operations_total: u64,
    tool_histogram: Vec<ToolCountJson>,
    errors: Vec<String>,
    errors_total: u64,
    files: Vec<String>,
    plan_todos: Vec<TodoJson>,
    subagents: Vec<String>,
    compaction_segments: Vec<String>,
    last_compaction_segment: Option<String>,
    /// The store's own compaction summary body — LLM-written, not
    /// reconstructed session events. Human output hides this unless
    /// `--full-summary` is passed; JSON always carries it, clearly named.
    compaction_summary_llm_written: Option<String>,
    memory_paths: Vec<String>,
}

fn cmd_load(home: &Path, args: &[String]) {
    if has_help_flag(args) {
        print_help();
        return;
    }
    let mut target: Option<String> = None;
    let mut full_summary = false;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--full-summary" => full_summary = true,
            "--home" => {
                i += 1;
            }
            "--json" => json = true,
            other if target.is_none() && !other.starts_with("--") => {
                target = Some(other.to_string());
            }
            other if other.starts_with("--") => eprintln!("warning: unknown flag: {other}"),
            _ => {}
        }
        i += 1;
    }
    let Some(target) = target else {
        eprintln!("load requires a session dir, id prefix, or updates.jsonl path");
        std::process::exit(2);
    };
    let home = override_home(home, args);
    let session_dir = match resolve_target(&home, &target) {
        Some(d) => d,
        None => {
            eprintln!("cannot resolve Grok session: {target}");
            std::process::exit(1);
        }
    };

    let report = build_load_report(&home, &session_dir);
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to encode JSON: {e}");
                std::process::exit(1);
            }
        }
    } else {
        render_load_human(&report, full_summary);
    }
}

fn build_load_report(home: &Path, session_dir: &Path) -> SessionLoadReportJson {
    let summary = read_json(&session_dir.join("summary.json"));
    let signals = read_json(&session_dir.join("signals.json"));
    let plan = read_json(&session_dir.join("plan.json"));
    let id = session_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let live = live_ids(home).contains(&id);
    let updates = session_dir.join("updates.jsonl");
    let chat = session_dir.join("chat_history.jsonl");
    let updates_size = file_len(&updates);
    let chat_size = file_len(&chat);

    let mut title = first_str(&summary, &["generated_title", "session_summary"]);
    let mut topic_source: &'static str = if title.is_empty() { "none" } else { "title" };

    let (user_msgs, chat_tail_truncated) = collect_user_messages(&chat, DEFAULT_TAIL_BYTES);

    if title.is_empty() {
        if let Some(p) = first_human_prompt_from_chat(&chat, HEAD_FALLBACK_BYTES) {
            title = p;
            topic_source = "first_prompt";
        } else if let Some((_, p)) = user_msgs.first() {
            title = p.clone();
            topic_source = "first_prompt_tail";
        }
    }

    let start = user_msgs.len().saturating_sub(MAX_USER_LIST);
    let user_messages: Vec<String> = user_msgs[start..].iter().map(|(_, t)| t.clone()).collect();
    let user_messages_total = user_msgs.len() as u64;

    let digest = scan_updates(&updates, DEFAULT_TAIL_BYTES);
    let compaction = build_compaction(session_dir);
    let subagents = list_subagents(session_dir);
    let memory_paths = collect_memory_lines(home, &summary, &id);

    let tool_histogram: Vec<ToolCountJson> = {
        let mut hist: Vec<_> = digest.tool_hist.iter().collect();
        hist.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        hist.into_iter()
            .map(|(name, count)| ToolCountJson {
                name: name.clone(),
                count: *count,
            })
            .collect()
    };

    let plan_todos: Vec<TodoJson> = plan["todos"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| TodoJson {
                    id: k.clone(),
                    status: v["status"].as_str().unwrap_or("?").to_string(),
                    content: v["content"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    SessionLoadReportJson {
        schema: SCHEMA_LOAD,
        session_id: id,
        dir: session_dir.display().to_string(),
        workdir: summary["info"]["cwd"].as_str().unwrap_or("?").to_string(),
        kind: summary["session_kind"].as_str().unwrap_or("parent").to_string(),
        live,
        title,
        topic_source,
        created_at: summary["created_at"].as_str().unwrap_or("?").to_string(),
        updated_at: summary["last_active_at"]
            .as_str()
            .or_else(|| summary["updated_at"].as_str())
            .unwrap_or("?")
            .to_string(),
        model: summary["current_model_id"].as_str().unwrap_or("?").to_string(),
        agent: summary["agent_name"].as_str().unwrap_or("?").to_string(),
        effort: summary["reasoning_effort"].as_str().unwrap_or("?").to_string(),
        git_branch: summary["head_branch"].as_str().unwrap_or("?").to_string(),
        git_head: summary["head_commit"].as_str().unwrap_or("?").to_string(),
        updates_size_bytes: updates_size,
        chat_size_bytes: chat_size,
        num_messages: summary["num_messages"].as_u64().unwrap_or(0),
        num_chat_messages: summary["num_chat_messages"].as_u64().unwrap_or(0),
        store_last_turn_note: summary["last_turn_summary"].as_str().unwrap_or("").to_string(),
        signals_turns: signals["turnCount"].as_u64().unwrap_or(0),
        signals_user: signals["userMessageCount"].as_u64().unwrap_or(0),
        signals_assistant: signals["assistantMessageCount"].as_u64().unwrap_or(0),
        signals_tools: signals["toolCallCount"].as_u64().unwrap_or(0),
        signals_compactions: signals["compactionCount"].as_u64().unwrap_or(0),
        signals_tools_used: signals["toolsUsed"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        signals_context_used: signals["contextTokensUsed"].as_u64().unwrap_or(0),
        signals_context_window: signals["contextWindowTokens"].as_u64().unwrap_or(0),
        user_messages,
        user_messages_total,
        chat_tail_truncated,
        assistant_texts: digest.assistant_texts,
        assistant_texts_total: digest.assistant_total,
        updates_tail_truncated: digest.tail_truncated,
        tool_operations: digest.tool_ops,
        tool_operations_total: digest.tool_ops_total,
        tool_histogram,
        errors: digest.errors,
        errors_total: digest.errors_total,
        files: digest.files,
        plan_todos,
        subagents,
        compaction_segments: compaction.segments,
        last_compaction_segment: compaction.last_name,
        compaction_summary_llm_written: compaction.summary,
        memory_paths,
    }
}

fn render_load_human(r: &SessionLoadReportJson, full_summary: bool) {
    println!("═══════════════════════════════════════");
    println!("Session: {}", r.session_id);
    println!("═══════════════════════════════════════");
    println!("Dir: {}", r.dir);
    println!("Workdir: {}", r.workdir);
    let topic_display = if r.title.is_empty() {
        "(empty session)"
    } else {
        r.title.as_str()
    };
    println!("Topic ({}): {topic_display}", r.topic_source);
    println!("Kind: {}{}", r.kind, if r.live { " [live]" } else { "" });
    println!(
        "State: created {} | updated {}",
        fmt_iso_local(&r.created_at),
        fmt_iso_local(&r.updated_at)
    );
    println!(
        "Model: {} | agent={} | effort={}",
        r.model, r.agent, r.effort
    );
    println!("Git: branch={} | head={}", r.git_branch, truncate(&r.git_head, 12));
    println!(
        "Wire: updates {:.2} MB{} | chat {:.2} MB{} | messages {} / chat {}",
        r.updates_size_bytes as f64 / 1e6,
        if r.updates_tail_truncated { " (tail-capped)" } else { "" },
        r.chat_size_bytes as f64 / 1e6,
        if r.chat_tail_truncated { " (tail-capped)" } else { "" },
        r.num_messages,
        r.num_chat_messages
    );
    if !r.store_last_turn_note.is_empty() {
        println!(
            "Store note (stale, provider-written, NOT a session event): {}",
            truncate(&one_line(&r.store_last_turn_note), 240)
        );
    }
    println!();

    println!("Signals ⚙");
    println!(
        "  turns={} user={} assistant={} tools={} compactions={}",
        r.signals_turns, r.signals_user, r.signals_assistant, r.signals_tools, r.signals_compactions
    );
    if !r.signals_tools_used.is_empty() {
        println!("  toolsUsed: {}", r.signals_tools_used.join(", "));
    }
    println!(
        "  context {} / {} tokens",
        r.signals_context_used, r.signals_context_window
    );
    println!();

    println!("User Messages 💬 ({} total)", r.user_messages_total);
    if r.user_messages.is_empty() {
        println!("  (none in the scanned window)");
    } else {
        print_numbered_tail_verbatim(&r.user_messages, r.user_messages_total, 200, 5, 4000);
    }
    println!();

    println!("Assistant Texts 🤖 ({} total)", r.assistant_texts_total);
    if r.assistant_texts.is_empty() {
        println!("  (none in the scanned window)");
    } else {
        print_numbered_tail_verbatim(&r.assistant_texts, r.assistant_texts_total, 200, 5, 4000);
    }
    println!();

    println!("Recent Tool Operations 🔧 ({} total)", r.tool_operations_total);
    if r.tool_operations.is_empty() {
        println!("  (none in the scanned window)");
    } else {
        print_recent_ops(&r.tool_operations, r.tool_operations_total);
    }
    println!();

    if !r.tool_histogram.is_empty() {
        println!("Tool Histogram 📊 (secondary, coarse tally over the same tail window)");
        for t in &r.tool_histogram {
            println!("  {}: {}", t.name, t.count);
        }
        println!();
    }

    if r.errors_total > 0 {
        println!("Errors ⚠ ({} total)", r.errors_total);
        print_recent_ops(&r.errors, r.errors_total);
        println!();
    }

    if !r.files.is_empty() {
        println!("Files Touched 📁 ({} unique, from updates tail)", r.files.len());
        for f in r.files.iter().take(40) {
            println!("  {f}");
        }
        if r.files.len() > 40 {
            println!("  ... ({} more)", r.files.len() - 40);
        }
        println!();
    }

    if !r.plan_todos.is_empty() {
        println!("Plan todos 📋 ({})", r.plan_todos.len());
        for t in &r.plan_todos {
            println!("  {} | {} | {}", t.id, t.status, truncate(&t.content, 160));
        }
        println!();
    }

    if !r.subagents.is_empty() {
        println!("Subagents 🧩 ({})", r.subagents.len());
        for s in &r.subagents {
            println!("  {s}");
        }
        println!();
    }

    println!("Compactions 🗜 ({} segments)", r.compaction_segments.len());
    for line in &r.compaction_segments {
        println!("  {line}");
    }
    if let Some(name) = &r.last_compaction_segment {
        match (&r.compaction_summary_llm_written, full_summary) {
            (Some(summary), true) => {
                println!("\n── Compaction summary (LLM-written, not session events; segment {name}) ──");
                println!("{summary}");
            }
            (Some(_), false) => {
                println!(
                    "\n(compaction summary available for segment {name} — LLM-written, not session events; rerun with --full-summary to view)"
                );
            }
            (None, _) => {}
        }
    }
    println!();

    println!("Memory 🧠");
    for l in &r.memory_paths {
        println!("  {l}");
    }
}

// ---------------------------------------------------------------- chat_history.jsonl (user prompts)

fn collect_user_messages(chat: &Path, tail_bytes: u64) -> (Vec<(u64, String)>, bool) {
    let mut out = Vec::new();
    let Ok(meta) = fs::metadata(chat) else {
        return (out, false);
    };
    let truncated = meta.len() > tail_bytes;
    let Ok(lines) = tail_lines(chat, tail_bytes) else {
        return (out, truncated);
    };
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v["type"].as_str() != Some("user") {
            continue;
        }
        if v.get("synthetic_reason").and_then(|s| s.as_str()).is_some() {
            continue;
        }
        let text = content_text(&v["content"]);
        if text.trim().is_empty() {
            continue;
        }
        let idx = v["prompt_index"].as_u64().unwrap_or(out.len() as u64);
        out.push((idx, text));
    }
    (out, truncated)
}

/// First genuine human prompt found in a bounded head-window scan of
/// `chat_history.jsonl` — the first `type == "user"` line without a
/// `synthetic_reason` (harness-injected reminders are not human turns).
fn first_human_prompt_from_chat(chat: &Path, head_bytes: u64) -> Option<String> {
    let lines = read_head_lines(chat, head_bytes).ok()?;
    for line in &lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["type"].as_str() != Some("user") {
            continue;
        }
        if v.get("synthetic_reason").and_then(|s| s.as_str()).is_some() {
            continue;
        }
        let text = one_line(&content_text(&v["content"]));
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

// ---------------------------------------------------------------- updates.jsonl (verbatim digest)

#[derive(Default)]
struct UpdatesDigest {
    tool_hist: BTreeMap<String, u64>,
    files: Vec<String>,
    tail_truncated: bool,
    /// Chronological, capped to the last [`ASSISTANT_MAX_ITEMS`].
    assistant_texts: Vec<String>,
    assistant_total: u64,
    /// Chronological, capped to the last [`TOOL_OPS_MAX`] — tool calls
    /// (with their salient argument), tool outputs, and failures, in the
    /// order they appear in the tailed window.
    tool_ops: Vec<String>,
    tool_ops_total: u64,
    /// Chronological, capped to the last [`ERRORS_MAX`] — the same failure
    /// text that also appears inline in `tool_ops`, surfaced on its own so
    /// it is never missed.
    errors: Vec<String>,
    errors_total: u64,
}

/// Build a verbatim digest of the last `tail_bytes` of `updates.jsonl`.
///
/// Reads `agent_message_chunk` for assistant text, `tool_call.rawInput` for
/// the call's key argument, and `tool_call_update` for outputs/failures —
/// the three event kinds a prior audit found unread (`agent_message_chunk`,
/// `rawInput`, failed `tool_call_update`).
fn scan_updates(updates: &Path, tail_bytes: u64) -> UpdatesDigest {
    let mut digest = UpdatesDigest::default();
    let Ok(meta) = fs::metadata(updates) else {
        return digest;
    };
    digest.tail_truncated = meta.len() > tail_bytes;
    let Ok(lines) = tail_lines(updates, tail_bytes) else {
        return digest;
    };

    let mut call_names: BTreeMap<String, String> = BTreeMap::new();
    for line in &lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let update = &v["params"]["update"];
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        match kind {
            "tool_call" => {
                let name = update["title"].as_str().unwrap_or("?").to_string();
                *digest.tool_hist.entry(name.clone()).or_insert(0) += 1;
                if let Some(id) = update["toolCallId"].as_str() {
                    call_names.insert(id.to_string(), name.clone());
                }
                let op = match salient_tool_arg(&update["rawInput"]) {
                    Some(arg) => {
                        let first_line = arg.lines().next().unwrap_or(&arg);
                        format!("{name}: {}", truncate(first_line, 150))
                    }
                    None => name.clone(),
                };
                digest.tool_ops_total += 1;
                digest.tool_ops.push(op);
                push_path(&mut digest.files, update["rawInput"]["target_directory"].as_str());
                push_path(&mut digest.files, update["rawInput"]["target_file"].as_str());
                push_path(&mut digest.files, update["rawInput"]["file_path"].as_str());
                push_path(&mut digest.files, update["rawInput"]["path"].as_str());
            }
            "tool_call_update" => {
                let id = update["toolCallId"].as_str().unwrap_or("");
                let name = call_names.get(id).cloned().unwrap_or_else(|| "tool".to_string());
                let status = update["status"].as_str().unwrap_or("");
                if status == "failed" {
                    let err = extract_tool_update_text(update);
                    let err = if err.trim().is_empty() {
                        "(no error detail)".to_string()
                    } else {
                        one_line(&err)
                    };
                    let line = format!("{name}: FAILED — {}", truncate(&err, 300));
                    digest.tool_ops_total += 1;
                    digest.tool_ops.push(line.clone());
                    digest.errors_total += 1;
                    digest.errors.push(line);
                } else {
                    let out = extract_tool_update_text(update);
                    if !out.trim().is_empty() {
                        let line = format!("{name}: {}", truncate(&one_line(&out), 300));
                        digest.tool_ops_total += 1;
                        digest.tool_ops.push(line);
                    }
                }
                if let Some(locs) = update["locations"].as_array() {
                    for loc in locs {
                        push_path(&mut digest.files, loc["path"].as_str());
                    }
                }
            }
            "agent_message_chunk" => {
                let text = content_text(&update["content"]);
                if !text.trim().is_empty() {
                    digest.assistant_total += 1;
                    digest.assistant_texts.push(text);
                }
            }
            _ => {}
        }
    }

    truncate_to_last(&mut digest.assistant_texts, ASSISTANT_MAX_ITEMS);
    truncate_to_last(&mut digest.tool_ops, TOOL_OPS_MAX);
    truncate_to_last(&mut digest.errors, ERRORS_MAX);
    digest
}

/// Pick a tool call's most informative argument out of `rawInput`, verbatim
/// (e.g. the full shell command, not just a path). Falls back to a compact
/// JSON rendering of the whole object when no known key matches, so an
/// unrecognized tool still surfaces something rather than nothing.
fn salient_tool_arg(raw_input: &Value) -> Option<String> {
    const KEYS: &[&str] = &[
        "command",
        "target_file",
        "target_directory",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
    ];
    for k in KEYS {
        if let Some(s) = raw_input[k].as_str() {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    if raw_input.as_object().is_some_and(|o| !o.is_empty()) {
        return serde_json::to_string(raw_input).ok();
    }
    None
}

/// Extract verbatim text from a `tool_call_update`: first the ACP
/// `content: [{ "content": ContentBlock }, ...]` shape, then a
/// best-effort scan of `rawOutput` (e.g. `rawOutput.FileReadError`) for the
/// first non-empty string value.
fn extract_tool_update_text(update: &Value) -> String {
    if let Some(arr) = update["content"].as_array() {
        let joined: Vec<String> = arr
            .iter()
            .map(|item| content_text(&item["content"]))
            .filter(|s| !s.trim().is_empty())
            .collect();
        if !joined.is_empty() {
            return joined.join(" ");
        }
    }
    if let Some(obj) = update["rawOutput"].as_object() {
        for v in obj.values() {
            if let Some(s) = v.as_str() {
                if !s.trim().is_empty() {
                    return s.to_string();
                }
            }
            if let Some(inner) = v.as_object() {
                for iv in inner.values() {
                    if let Some(s) = iv.as_str() {
                        if !s.trim().is_empty() {
                            return s.to_string();
                        }
                    }
                }
            }
        }
    }
    String::new()
}

fn push_path(files: &mut Vec<String>, p: Option<&str>) {
    let Some(p) = p else { return };
    if p.is_empty() {
        return;
    }
    if !files.iter().any(|x| x == p) && files.len() < MAX_FILES {
        files.push(p.to_string());
    }
}

// ---------------------------------------------------------------- compaction / subagents / memory

struct CompactionData {
    segments: Vec<String>,
    last_name: Option<String>,
    summary: Option<String>,
}

fn build_compaction(session_dir: &Path) -> CompactionData {
    let index = session_dir.join("compaction").join("INDEX.md");
    let Ok(idx) = fs::read_to_string(&index) else {
        return CompactionData {
            segments: Vec::new(),
            last_name: None,
            summary: None,
        };
    };
    let segments: Vec<String> = idx
        .lines()
        .filter(|l| l.starts_with("| ") && l.contains("segment_"))
        .map(|l| l.trim().to_string())
        .collect();
    let last_name = segments.last().and_then(|l| {
        l.split('|')
            .nth(2)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });
    let summary = last_name.as_ref().and_then(|name| {
        fs::read_to_string(session_dir.join("compaction").join(name))
            .ok()
            .map(|body| extract_compaction_summary(&body))
    });
    CompactionData {
        segments,
        last_name,
        summary,
    }
}

fn extract_compaction_summary(body: &str) -> String {
    const MARKERS: &[&str] = &[
        "## Summary (curated by compaction step)",
        "## Summary",
        "Summary:",
    ];
    for marker in MARKERS {
        if let Some(idx) = body.find(marker) {
            return body[idx..].to_string();
        }
    }
    body.chars().take(SUMMARY_CHARS).collect()
}

fn list_subagents(session_dir: &Path) -> Vec<String> {
    let sub_dir = session_dir.join("subagents");
    let Ok(rd) = fs::read_dir(sub_dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn collect_memory_lines(home: &Path, summary: &Value, session_id: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mem = home.join("memory");
    if !mem.exists() {
        lines.push("(disabled or empty — not the session transcript)".to_string());
        return lines;
    }
    lines.push("(separate plane, paths only — not a substitute for this session)".to_string());
    let global = mem.join("MEMORY.md");
    if global.exists() {
        lines.push(format!("global: {}", global.display()));
    }
    if let Ok(rd) = fs::read_dir(&mem) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let ws = p.join("MEMORY.md");
            if ws.exists() {
                lines.push(format!("workspace: {}", ws.display()));
            }
            let sess = p.join("sessions");
            if let Ok(logs) = fs::read_dir(sess) {
                for log in logs.flatten() {
                    let name = log.file_name().to_string_lossy().into_owned();
                    if name.contains(&session_id[..session_id.len().min(8)]) {
                        lines.push(format!("session-log: {}", log.path().display()));
                    }
                }
            }
        }
    }
    if let Some(git_root) = summary["git_root_dir"].as_str() {
        lines.push(format!("git_root_dir: {git_root}"));
    }
    lines
}

// ---------------------------------------------------------------- rendering helpers

/// Print the most recent `items` (already capped to the digest's item
/// budget by the caller) verbatim: the last `full_tail` entries in full
/// (bounded by `full_max_chars`, with an explicit "[truncated N chars]"
/// marker), earlier ones collapsed to a single short line. `total` is the
/// true count before capping, so the "N earlier, not shown" note stays
/// accurate even though `items` no longer carries the dropped entries.
fn print_numbered_tail_verbatim(
    items: &[String],
    total: u64,
    short_max_chars: usize,
    full_tail: usize,
    full_max_chars: usize,
) {
    let hidden = total.saturating_sub(items.len() as u64);
    let full_start = items.len().saturating_sub(full_tail);
    for (i, item) in items.iter().enumerate() {
        let index = hidden + i as u64 + 1;
        if i >= full_start {
            let (text, cut) = truncate_reporting(item, full_max_chars);
            println!("  {index}. {text}");
            if let Some(cut) = cut {
                println!("     [truncated {cut} chars]");
            }
        } else {
            let first_line = item.lines().next().unwrap_or(item);
            println!("  {index}. {}", truncate(first_line, short_max_chars));
        }
    }
    if hidden > 0 {
        println!("  ... ({hidden} earlier, not shown)");
    }
}

fn print_recent_ops(ops: &[String], total: u64) {
    let hidden = total.saturating_sub(ops.len() as u64);
    for (i, op) in ops.iter().enumerate() {
        println!("  {}. {op}", hidden + i as u64 + 1);
    }
    if hidden > 0 {
        println!("  ... ({hidden} earlier, not shown)");
    }
}

// ---------------------------------------------------------------- generic helpers

/// Read up to `max_bytes` from the end of `path`, split into complete lines.
/// A single seek plus one bounded read — cost is proportional to
/// `max_bytes`, not to file size. If the seek lands mid-line, that partial
/// leading line is dropped.
fn tail_lines(path: &Path, max_bytes: u64) -> std::io::Result<Vec<String>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    if len > max_bytes {
        f.seek(SeekFrom::Start(len - max_bytes))?;
    }
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;
    let mut lines: Vec<String> = buf.lines().map(|s| s.to_string()).collect();
    if len > max_bytes && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(lines)
}

/// Read up to `max_bytes` from the start of `path`, split into complete
/// lines. Used only for the title/first-prompt fallback scan.
fn read_head_lines(path: &Path, max_bytes: u64) -> std::io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0_u8; max_bytes as usize];
    let read = file.read(&mut buffer)?;
    buffer.truncate(read);
    if read as u64 == max_bytes {
        if let Some(index) = buffer.iter().rposition(|byte| *byte == b'\n') {
            buffer.truncate(index);
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    Ok(text.lines().map(str::to_owned).collect())
}

fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text" || p.get("text").is_some())
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        Value::Object(obj) => obj
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn read_json(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn first_str(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = v[k].as_str() {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate `s` to at most `n` characters, appending an ellipsis when cut.
fn truncate(s: &str, n: usize) -> String {
    let mut it = s.chars();
    let taken: String = it.by_ref().take(n).collect();
    if it.next().is_some() {
        format!("{taken}…")
    } else {
        taken
    }
}

/// Truncate `text` to at most `max_chars` characters, reporting how many
/// characters were cut instead of silently dropping them.
fn truncate_reporting(text: &str, max_chars: usize) -> (String, Option<usize>) {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return (text.to_string(), None);
    }
    let taken: String = text.chars().take(max_chars).collect();
    (taken, Some(total_chars - max_chars))
}

/// Keep only the last `cap` elements of `items`, preserving order.
fn truncate_to_last<T>(items: &mut Vec<T>, cap: usize) {
    if items.len() > cap {
        let drop_count = items.len() - cap;
        items.drain(..drop_count);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn iso_to_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// UTC, ISO 8601 — used for every computed timestamp in `--json` output.
fn iso_utc_ms(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339())
}

/// Local time with a numeric UTC offset — used for every computed
/// timestamp in human-readable output. `--json` keeps UTC (see
/// [`iso_utc_ms`]).
fn fmt_local_ms(ms: i64) -> String {
    if ms <= 0 {
        return "?".to_string();
    }
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %z").to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Render a raw ISO 8601 string from `summary.json` in local time for the
/// human report; falls back to the raw string when it doesn't parse.
fn fmt_iso_local(raw: &str) -> String {
    match iso_to_ms(raw) {
        Some(ms) => fmt_local_ms(ms),
        None => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_appends_ellipsis_only_when_cut() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello…");
    }

    #[test]
    fn truncate_reporting_marks_cut_length() {
        let (text, cut) = truncate_reporting("hello world", 5);
        assert_eq!(text, "hello");
        assert_eq!(cut, Some(6));

        let (text, cut) = truncate_reporting("hi", 5);
        assert_eq!(text, "hi");
        assert_eq!(cut, None);
    }

    #[test]
    fn truncate_to_last_keeps_the_most_recent() {
        let mut items = vec![1, 2, 3, 4, 5];
        truncate_to_last(&mut items, 2);
        assert_eq!(items, vec![4, 5]);

        let mut short = vec![1, 2];
        truncate_to_last(&mut short, 5);
        assert_eq!(short, vec![1, 2]);
    }

    #[test]
    fn one_line_collapses_whitespace() {
        assert_eq!(one_line("hello\n  world\t!"), "hello world !");
    }

    #[test]
    fn content_text_handles_string_object_and_array() {
        assert_eq!(content_text(&serde_json::json!("plain")), "plain");
        assert_eq!(content_text(&serde_json::json!({"type": "text", "text": "hi"})), "hi");
        let arr = serde_json::json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]);
        assert_eq!(content_text(&arr), "a b");
    }

    #[test]
    fn salient_tool_arg_picks_known_keys_and_falls_back_to_json() {
        let cmd = serde_json::json!({"command": "echo hi"});
        assert_eq!(salient_tool_arg(&cmd).as_deref(), Some("echo hi"));

        let path = serde_json::json!({"file_path": "src/main.rs"});
        assert_eq!(salient_tool_arg(&path).as_deref(), Some("src/main.rs"));

        let unknown = serde_json::json!({"weird_key": "value"});
        assert!(salient_tool_arg(&unknown).unwrap().contains("weird_key"));

        assert_eq!(salient_tool_arg(&serde_json::json!({})), None);
    }

    #[test]
    fn extract_tool_update_text_reads_content_array_then_raw_output() {
        let with_content = serde_json::json!({
            "content": [{"type": "content", "content": {"type": "text", "text": "diff applied"}}]
        });
        assert_eq!(extract_tool_update_text(&with_content), "diff applied");

        let with_raw_output = serde_json::json!({
            "rawOutput": {"FileReadError": "Cannot read binary file: harvest.log"}
        });
        assert_eq!(
            extract_tool_update_text(&with_raw_output),
            "Cannot read binary file: harvest.log"
        );

        assert_eq!(extract_tool_update_text(&serde_json::json!({})), "");
    }

    #[test]
    fn first_human_prompt_from_chat_skips_synthetic_lines() {
        let dir = std::env::temp_dir().join(format!(
            "grok-session-restore-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        let chat = dir.join("chat_history.jsonl");
        fs::write(
            &chat,
            concat!(
                r#"{"type":"system"}"#, "\n",
                r#"{"type":"user","synthetic_reason":"system_reminder","content":"bootstrap"}"#, "\n",
                r#"{"type":"user","content":"real question"}"#, "\n",
            ),
        )
        .expect("write fixture");

        let prompt = first_human_prompt_from_chat(&chat, HEAD_FALLBACK_BYTES);
        assert_eq!(prompt.as_deref(), Some("real question"));

        let _ = fs::remove_dir_all(&dir);
    }
}
