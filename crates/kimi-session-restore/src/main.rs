//! kimi-session-restore — list and load Kimi Code CLI sessions for context restoration.
//!
//! Session layout (under `~/.kimi-code/`, overridable with `--home` or
//! `$KIMI_CODE_HOME`):
//!   sessions/<wd_key>/<session_id>/state.json — title, workDir, createdAt/updatedAt, agents, lastPrompt
//!   sessions/<wd_key>/<session_id>/agents/<agent>/wire.jsonl — event stream
//!
//! wire.jsonl event types read here:
//!   turn.prompt / turn.steer   {input:[{type:"text",text}], origin:{kind}, time}
//!   context.append_loop_event  {event:{type: content.part|tool.call, ...}}
//!   context.apply_compaction   {summary, tokensBefore, tokensAfter, keptUserMessageCount, time}
//!
//! # Topic selection
//!
//! `state.json`'s own `title` wins when non-empty. Otherwise the topic falls
//! back to the first genuine human prompt — the first `turn.prompt` event
//! whose `origin.kind == "user"` — never a `turn.steer` (cron fires, task
//! notifications) and never a `turn.prompt` from a non-user origin (agent
//! bootstrap turns, etc.). Those stay in the Steers/Notifications counter.
//!
//! # Digest is verbatim, not a summary
//!
//! `load`'s report never paraphrases: the last human prompts, the last
//! assistant texts, and the last tool operations are printed as direct
//! quotes from the transcript, in chronological order, newest last.
//!
//! # Reads are byte-budgeted, not full-file scans
//!
//! Both the digest scan (`read_tail_lines`) and the title/first-prompt
//! fallback scan (`read_head_lines`) read a bounded window with a single
//! seek plus one read, so a multi-gigabyte `wire.jsonl` still loads in well
//! under a second. When the tail window doesn't cover the whole file, the
//! report says so instead of silently pretending the digest is complete.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// JSON report schema tag for `list --json`.
const SCHEMA_LIST: &str = "kimi-session-restore-list-v1";
/// JSON report schema tag for `load --json`.
const SCHEMA_LOAD: &str = "kimi-session-restore-load-v1";

/// Tail byte budget for `load`'s digest scan — a single seek plus one bounded
/// read, so cost is proportional to this constant, not to file size.
const LOAD_TAIL_BYTES: u64 = 32 * 1024 * 1024;
/// Head byte budget for the title/first-prompt fallback scan (`list` and
/// `load` alike) — the earliest human prompt lives near the start of the
/// file, so this window is small.
const HEAD_FALLBACK_BYTES: u64 = 1024 * 1024;

/// How many of the most recent human prompts / assistant texts are kept for
/// the verbatim tail digest.
const LOAD_MAX_ITEMS: usize = 10;
/// How many of the most recent tool.call events are kept for the "Recent
/// Tool Operations" section.
const TOOL_OPS_MAX: usize = 15;
/// Cap on unique files listed in the "Files Touched" inventory.
const FILES_MAX: usize = 100;
/// Cap on cron-fire previews shown under Steers/Notifications.
const CRON_PREVIEWS_MAX: usize = 3;

const USAGE: &str = "kimi-session-restore — restore context from Kimi Code sessions\n\
\n\
USAGE:\n\
\x20 kimi-session-restore list [--max-age-hours N] [--all] [--home PATH] [--json]\n\
\x20 kimi-session-restore load <session-dir | session-id-prefix | wire.jsonl> [--full-summary] [--home PATH] [--json]\n\
\x20 kimi-session-restore --help | -h\n\
\n\
list  — recent sessions: id, time, size, workdir, topic, last user prompt\n\
load  — deep dive: verbatim tail of user/assistant messages, recent tool\n\
\x20       operations, tool histogram, files touched, compaction summaries\n\
\n\
FLAGS:\n\
\x20 --home PATH        Kimi home containing sessions/ (default: $KIMI_CODE_HOME or ~/.kimi-code)\n\
\x20 --max-age-hours N  list: only sessions updated within the last N hours (default: 12)\n\
\x20 --all              list: ignore --max-age-hours, show every session\n\
\x20 --full-summary     load: print the last compaction summary in full\n\
\x20 --json             emit machine-readable JSON instead of the human report";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage_error();
        std::process::exit(2);
    }
    if args[0] == "--help" || args[0] == "-h" {
        print_help();
        return;
    }
    match args[0].as_str() {
        "list" => cmd_list(&args[1..]),
        "load" => cmd_load(&args[1..]),
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

fn kimi_home() -> PathBuf {
    if let Ok(h) = std::env::var("KIMI_CODE_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    PathBuf::from(home).join(".kimi-code")
}

// ---------------------------------------------------------------- scanning sessions

struct SessionEntry {
    dir: PathBuf,
    id: String,
    updated_ms: i64,
    wire_size: u64,
    wire_path: Option<PathBuf>,
    title: String,
    workdir: String,
    last_prompt: String,
}

fn scan_sessions(home: &Path) -> Vec<SessionEntry> {
    let mut out = Vec::new();
    let root = home.join("sessions");
    let Ok(wds) = fs::read_dir(&root) else { return out };
    for wd in wds.flatten() {
        let Ok(sessions) = fs::read_dir(wd.path()) else { continue };
        for s in sessions.flatten() {
            let dir = s.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(id) = dir.file_name().map(|name| name.to_string_lossy().into_owned()) else {
                continue;
            };
            let state_path = dir.join("state.json");
            let (mut title, mut workdir, mut last_prompt, mut updated_ms) =
                (String::new(), String::new(), String::new(), 0_i64);
            if let Ok(txt) = fs::read_to_string(&state_path) {
                if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                    title = v["title"].as_str().unwrap_or("").to_string();
                    workdir = v["workDir"].as_str().unwrap_or("").to_string();
                    last_prompt = v["lastPrompt"].as_str().unwrap_or("").to_string();
                    updated_ms = v["updatedAt"].as_str().and_then(iso_to_ms).unwrap_or(0);
                }
            }

            // main agent wire.jsonl (fallback: any agent wire)
            let mut wire_path = dir.join("agents").join("main").join("wire.jsonl");
            if !wire_path.exists() {
                if let Some(p) = find_any_wire(&dir) {
                    wire_path = p;
                }
            }
            let wire_exists = wire_path.exists();
            let (wire_size, mtime_ms) = if wire_exists {
                fs::metadata(&wire_path)
                    .map(|md| {
                        let mtime = md
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0);
                        (md.len(), mtime)
                    })
                    .unwrap_or((0, 0))
            } else {
                (0, 0)
            };

            out.push(SessionEntry {
                dir,
                id,
                updated_ms: updated_ms.max(mtime_ms),
                wire_size,
                wire_path: wire_exists.then_some(wire_path),
                title,
                workdir,
                last_prompt,
            });
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.updated_ms));
    out
}

fn find_any_wire(dir: &Path) -> Option<PathBuf> {
    let agents = dir.join("agents");
    let entries = fs::read_dir(agents).ok()?;
    for a in entries.flatten() {
        let p = a.path().join("wire.jsonl");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Resolve a `load` argument to a `wire.jsonl` path: an explicit file, a
/// session directory, or a session-id prefix looked up under `home`.
fn resolve_target(home: &Path, target: &str) -> Option<PathBuf> {
    let p = PathBuf::from(target);
    if p.is_file() {
        return Some(p);
    }
    if p.is_dir() {
        let w = p.join("agents").join("main").join("wire.jsonl");
        if w.exists() {
            return Some(w);
        }
        return find_any_wire(&p);
    }
    for e in scan_sessions(home) {
        if e.id.starts_with(target) {
            return e.wire_path;
        }
    }
    None
}

// ---------------------------------------------------------------- topic fallback

/// First genuine human prompt found in a bounded head-window scan of
/// `wire` — the first `turn.prompt` whose `origin.kind == "user"`. Never a
/// `turn.steer`, and never a `turn.prompt` from any other origin.
fn first_human_prompt_from_wire(wire: Option<&Path>, head_bytes: u64) -> Option<String> {
    let wire = wire?;
    let lines = read_head_lines(wire, head_bytes).ok()?;
    for line in &lines {
        if line.len() < 10 {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let kind = v["origin"]["kind"].as_str().unwrap_or("");
        if v["type"] == "turn.prompt" && kind == "user" {
            let text = one_line(&input_text(&v));
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

// ---------------------------------------------------------------- byte-budgeted reads

/// Read up to `max_bytes` from the end of `path`, split into complete lines.
///
/// A single seek plus one bounded read — cost is proportional to
/// `max_bytes`, not to file size. If the seek lands mid-line, that partial
/// leading line is dropped. Returns `(lines, truncated)`, where `truncated`
/// is `true` when the file is larger than `max_bytes` (earlier context
/// exists that this call did not read).
fn read_tail_lines(path: &Path, max_bytes: u64) -> io::Result<(Vec<String>, bool)> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;

    let mut buffer = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buffer)?;

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

/// Read up to `max_bytes` from the start of `path`, split into complete
/// lines. Used only for the title/first-prompt fallback scan.
fn read_head_lines(path: &Path, max_bytes: u64) -> io::Result<Vec<String>> {
    let mut file = fs::File::open(path)?;
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

// ---------------------------------------------------------------- wire digest

#[derive(Debug, Clone)]
struct CompactionInfo {
    tokens_before: i64,
    tokens_after: i64,
    kept_user_messages: i64,
    time_ms: i64,
    summary: String,
}

#[derive(Default)]
struct WireStats {
    events: u64,
    window_first_ms: i64,
    window_last_ms: i64,
    /// Chronological, capped to the last [`LOAD_MAX_ITEMS`].
    user_prompts: Vec<(i64, String)>,
    /// Count of matching human prompts before capping.
    user_prompts_total: u64,
    steer_kinds: BTreeMap<String, u64>,
    cron_previews: Vec<String>,
    tool_hist: BTreeMap<String, u64>,
    /// Chronological, capped to the last [`TOOL_OPS_MAX`].
    tool_ops: Vec<String>,
    files: Vec<String>,
    /// Chronological, capped to the last [`LOAD_MAX_ITEMS`].
    assistant_texts: Vec<(i64, String)>,
    /// Count of assistant texts before capping.
    assistant_count: u64,
    compactions: Vec<CompactionInfo>,
    /// `true` when the tail window did not cover the whole file.
    truncated: bool,
}

/// Build a verbatim digest of the last `tail_bytes` of `path`.
fn scan_wire(path: &Path, tail_bytes: u64) -> io::Result<WireStats> {
    let (lines, truncated) = read_tail_lines(path, tail_bytes)?;
    let mut st = WireStats { truncated, ..WireStats::default() };

    for line in &lines {
        if line.len() < 10 {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        st.events += 1;
        let t = v["time"].as_i64().unwrap_or(0);
        if t > 0 {
            if st.window_first_ms == 0 {
                st.window_first_ms = t;
            }
            st.window_last_ms = t;
        }

        match v["type"].as_str().unwrap_or("") {
            "turn.prompt" | "turn.steer" => {
                let kind = v["origin"]["kind"].as_str().unwrap_or("?").to_string();
                let text = one_line(&input_text(&v));
                let is_human_prompt = v["type"] == "turn.prompt" && kind == "user";
                if is_human_prompt {
                    if !text.trim().is_empty() {
                        st.user_prompts_total += 1;
                        st.user_prompts.push((t, text));
                    }
                } else {
                    *st.steer_kinds.entry(kind.clone()).or_insert(0) += 1;
                    if (kind == "cron" || text.contains("<cron-fire"))
                        && st.cron_previews.len() < CRON_PREVIEWS_MAX
                    {
                        st.cron_previews.push(truncate(&text, 200));
                    }
                }
            }
            "context.append_loop_event" => {
                let ev = &v["event"];
                match ev["type"].as_str().unwrap_or("") {
                    "content.part" if ev["part"]["type"] == "text" => {
                        let txt = ev["part"]["text"].as_str().unwrap_or("");
                        if !txt.trim().is_empty() {
                            st.assistant_count += 1;
                            st.assistant_texts.push((t, txt.to_string()));
                        }
                    }
                    "tool.call" => {
                        let name = ev["name"].as_str().unwrap_or("?").to_string();
                        *st.tool_hist.entry(name.clone()).or_insert(0) += 1;
                        st.tool_ops.push(describe_tool_call(&name, &ev["args"]));
                        if matches!(
                            name.as_str(),
                            "Read" | "Write" | "Edit" | "NotebookEdit" | "Glob" | "Grep"
                        ) {
                            let args = &ev["args"];
                            let file =
                                args["file_path"].as_str().or_else(|| args["path"].as_str());
                            if let Some(file) = file {
                                let file = file.to_string();
                                if !st.files.contains(&file) && st.files.len() < FILES_MAX {
                                    st.files.push(file);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            "context.apply_compaction" => {
                st.compactions.push(CompactionInfo {
                    tokens_before: v["tokensBefore"].as_i64().unwrap_or(0),
                    tokens_after: v["tokensAfter"].as_i64().unwrap_or(0),
                    kept_user_messages: v["keptUserMessageCount"].as_i64().unwrap_or(0),
                    time_ms: t,
                    summary: v["summary"].as_str().unwrap_or("").to_string(),
                });
            }
            _ => {}
        }
    }

    truncate_to_last(&mut st.user_prompts, LOAD_MAX_ITEMS);
    truncate_to_last(&mut st.assistant_texts, LOAD_MAX_ITEMS);
    truncate_to_last(&mut st.tool_ops, TOOL_OPS_MAX);

    Ok(st)
}

/// One-line description of a tool invocation including its key argument —
/// verbatim, not a paraphrase.
fn describe_tool_call(name: &str, args: &Value) -> String {
    let key_arg = match name {
        "Bash" => args["command"].as_str(),
        "Read" | "Write" | "Edit" | "NotebookEdit" => {
            args["file_path"].as_str().or_else(|| args["path"].as_str())
        }
        "Glob" | "Grep" => args["pattern"].as_str().or_else(|| args["path"].as_str()),
        "WebFetch" => args["url"].as_str(),
        "WebSearch" => args["query"].as_str(),
        "Task" | "Agent" => {
            args["description"].as_str().or_else(|| args["subagent_type"].as_str())
        }
        _ => None,
    };

    match key_arg {
        Some(arg) => {
            let first_line = arg.lines().next().unwrap_or(arg);
            format!("{name}: {}", truncate(first_line, 150))
        }
        None => name.to_string(),
    }
}

// ---------------------------------------------------------------- `list`

#[derive(Serialize)]
struct SessionListEntryJson {
    id: String,
    dir: String,
    updated_at: Option<String>,
    wire_size_bytes: u64,
    workdir: String,
    topic: String,
    topic_source: &'static str,
    last_prompt: String,
}

#[derive(Serialize)]
struct SessionListReportJson {
    schema: &'static str,
    sessions: Vec<SessionListEntryJson>,
}

struct ListView {
    id: String,
    dir: PathBuf,
    updated_ms: i64,
    wire_size: u64,
    workdir: String,
    topic: String,
    topic_source: &'static str,
    last_prompt: String,
}

fn build_list_view(e: &SessionEntry, head_bytes: u64) -> ListView {
    let (topic, topic_source) = if !e.title.is_empty() {
        (e.title.clone(), "title")
    } else if let Some(p) = first_human_prompt_from_wire(e.wire_path.as_deref(), head_bytes) {
        (p, "first_prompt")
    } else {
        (String::new(), "none")
    };
    ListView {
        id: e.id.clone(),
        dir: e.dir.clone(),
        updated_ms: e.updated_ms,
        wire_size: e.wire_size,
        workdir: e.workdir.clone(),
        topic,
        topic_source,
        last_prompt: e.last_prompt.clone(),
    }
}

fn cmd_list(args: &[String]) {
    if has_help_flag(args) {
        print_help();
        return;
    }

    let mut max_age_hours: Option<f64> = Some(12.0);
    let mut home_override: Option<PathBuf> = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max-age-hours" => {
                max_age_hours = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--all" => max_age_hours = None,
            "--home" => {
                if let Some(p) = args.get(i + 1) {
                    home_override = Some(PathBuf::from(p));
                }
                i += 1;
            }
            "--json" => json = true,
            _ => {}
        }
        i += 1;
    }

    let home = home_override.unwrap_or_else(kimi_home);
    let now = now_ms();
    let sessions = scan_sessions(&home);
    let mut views = Vec::new();
    for e in &sessions {
        if let Some(h) = max_age_hours {
            if now - e.updated_ms > (h * 3_600_000.0) as i64 {
                continue;
            }
        }
        views.push(build_list_view(e, HEAD_FALLBACK_BYTES));
    }

    if json {
        let entries = views
            .iter()
            .map(|v| SessionListEntryJson {
                id: v.id.clone(),
                dir: v.dir.display().to_string(),
                updated_at: iso_utc_ms(v.updated_ms),
                wire_size_bytes: v.wire_size,
                workdir: v.workdir.clone(),
                topic: v.topic.clone(),
                topic_source: v.topic_source,
                last_prompt: v.last_prompt.clone(),
            })
            .collect();
        let report = SessionListReportJson { schema: SCHEMA_LIST, sessions: entries };
        match serde_json::to_string_pretty(&report) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to encode JSON: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if views.is_empty() {
        println!(
            "No sessions found (window: {max_age_hours:?} hours). Try --all or --max-age-hours <N>."
        );
        return;
    }

    for (i, v) in views.iter().enumerate() {
        let size = if v.wire_size > 0 { format_size(v.wire_size) } else { "no wire".to_string() };
        println!("{}. {}\n   {} | {} | {}", i + 1, v.id, fmt_local_ms(v.updated_ms), size, v.workdir);
        if !v.topic.is_empty() {
            println!("   📌 {}", truncate(&v.topic, 140));
        }
        if !v.last_prompt.is_empty() {
            println!("   💬 last: {}", truncate(&one_line(&v.last_prompt), 140));
        }
        println!();
    }
    println!("To load a session, use:");
    for (i, v) in views.iter().enumerate() {
        println!("  {}. kimi-session-restore load \"{}\"", i + 1, v.dir.display());
    }
}

// ---------------------------------------------------------------- `load`

struct LoadView<'a> {
    session_dir: &'a Path,
    session_id: &'a str,
    state: &'a Value,
    stats: &'a WireStats,
    size_bytes: u64,
    topic: &'a str,
    topic_source: &'static str,
    agents: &'a [String],
    tool_ops_total: u64,
}

#[derive(Serialize)]
struct TimedTextJson {
    time: Option<String>,
    text: String,
}

#[derive(Serialize)]
struct ToolCountJson {
    name: String,
    count: u64,
}

#[derive(Serialize)]
struct KindCountJson {
    kind: String,
    count: u64,
}

#[derive(Serialize)]
struct CompactionJson {
    tokens_before: i64,
    tokens_after: i64,
    kept_user_message_count: i64,
    time: Option<String>,
    summary: String,
}

#[derive(Serialize)]
struct SessionLoadReportJson {
    schema: &'static str,
    session_id: String,
    dir: String,
    workdir: String,
    title: String,
    topic: String,
    topic_source: &'static str,
    created_at: Option<String>,
    updated_at: Option<String>,
    wire_size_bytes: u64,
    events_scanned: u64,
    window_first: Option<String>,
    window_last: Option<String>,
    truncated: bool,
    agents: Vec<String>,
    user_messages: Vec<TimedTextJson>,
    user_messages_total: u64,
    assistant_texts: Vec<TimedTextJson>,
    assistant_texts_total: u64,
    tool_operations: Vec<String>,
    tool_operations_total: u64,
    tool_histogram: Vec<ToolCountJson>,
    files: Vec<String>,
    steer_kinds: Vec<KindCountJson>,
    cron_previews: Vec<String>,
    compactions: Vec<CompactionJson>,
    last_state_prompt: Option<String>,
}

fn cmd_load(args: &[String]) {
    if has_help_flag(args) {
        print_help();
        return;
    }

    let mut target: Option<String> = None;
    let mut full_summary = false;
    let mut home_override: Option<PathBuf> = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--full-summary" => full_summary = true,
            "--home" => {
                if let Some(p) = args.get(i + 1) {
                    home_override = Some(PathBuf::from(p));
                }
                i += 1;
            }
            "--json" => json = true,
            other if target.is_none() && !other.starts_with("--") => {
                target = Some(other.to_string());
            }
            _ => {}
        }
        i += 1;
    }

    let Some(target) = target else {
        eprintln!("load requires a session dir, id prefix, or wire.jsonl path");
        std::process::exit(2);
    };
    let home = home_override.unwrap_or_else(kimi_home);
    let Some(wire) = resolve_target(&home, &target) else {
        eprintln!("cannot resolve session: {target}");
        std::process::exit(1);
    };

    // wire = <session_dir>/agents/<agent>/wire.jsonl → three levels up.
    let session_dir = wire
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default();

    let state_path = session_dir.join("state.json");
    let state: Value = fs::read_to_string(&state_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);

    let stats = match scan_wire(&wire, LOAD_TAIL_BYTES) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read {}: {e}", wire.display());
            std::process::exit(1);
        }
    };

    let title = state["title"].as_str().unwrap_or("").to_string();
    let (topic, topic_source) = if !title.is_empty() {
        (title, "title")
    } else if let Some(p) = first_human_prompt_from_wire(Some(&wire), HEAD_FALLBACK_BYTES) {
        (p, "first_prompt_head")
    } else if let Some((_, p)) = stats.user_prompts.first() {
        (p.clone(), "first_prompt_tail")
    } else {
        (String::new(), "none")
    };

    let size = fs::metadata(&wire).map(|m| m.len()).unwrap_or(0);
    let session_id =
        session_dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let agents: Vec<String> =
        state["agents"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
    let tool_ops_total: u64 = stats.tool_hist.values().sum();

    let view = LoadView {
        session_dir: &session_dir,
        session_id: &session_id,
        state: &state,
        stats: &stats,
        size_bytes: size,
        topic: &topic,
        topic_source,
        agents: &agents,
        tool_ops_total,
    };

    if json {
        print_load_json(&view);
    } else {
        print_load_human(&view, full_summary);
    }
}

fn build_load_json(view: &LoadView) -> SessionLoadReportJson {
    let user_messages = view
        .stats
        .user_prompts
        .iter()
        .map(|(t, text)| TimedTextJson { time: iso_utc_ms(*t), text: text.clone() })
        .collect();
    let assistant_texts = view
        .stats
        .assistant_texts
        .iter()
        .map(|(t, text)| TimedTextJson { time: iso_utc_ms(*t), text: text.clone() })
        .collect();
    let tool_histogram = view
        .stats
        .tool_hist
        .iter()
        .map(|(name, count)| ToolCountJson { name: name.clone(), count: *count })
        .collect();
    let steer_kinds = view
        .stats
        .steer_kinds
        .iter()
        .map(|(kind, count)| KindCountJson { kind: kind.clone(), count: *count })
        .collect();
    let compactions = view
        .stats
        .compactions
        .iter()
        .map(|c| CompactionJson {
            tokens_before: c.tokens_before,
            tokens_after: c.tokens_after,
            kept_user_message_count: c.kept_user_messages,
            time: iso_utc_ms(c.time_ms),
            summary: c.summary.clone(),
        })
        .collect();
    let last_state_prompt =
        view.state["lastPrompt"].as_str().filter(|s| !s.is_empty()).map(str::to_string);

    SessionLoadReportJson {
        schema: SCHEMA_LOAD,
        session_id: view.session_id.to_string(),
        dir: view.session_dir.display().to_string(),
        workdir: view.state["workDir"].as_str().unwrap_or("").to_string(),
        title: view.state["title"].as_str().unwrap_or("").to_string(),
        topic: view.topic.to_string(),
        topic_source: view.topic_source,
        created_at: view.state["createdAt"].as_str().map(str::to_string),
        updated_at: view.state["updatedAt"].as_str().map(str::to_string),
        wire_size_bytes: view.size_bytes,
        events_scanned: view.stats.events,
        window_first: iso_utc_ms(view.stats.window_first_ms),
        window_last: iso_utc_ms(view.stats.window_last_ms),
        truncated: view.stats.truncated,
        agents: view.agents.to_vec(),
        user_messages,
        user_messages_total: view.stats.user_prompts_total,
        assistant_texts,
        assistant_texts_total: view.stats.assistant_count,
        tool_operations: view.stats.tool_ops.clone(),
        tool_operations_total: view.tool_ops_total,
        tool_histogram,
        files: view.stats.files.clone(),
        steer_kinds,
        cron_previews: view.stats.cron_previews.clone(),
        compactions,
        last_state_prompt,
    }
}

fn print_load_json(view: &LoadView) {
    let report = build_load_json(view);
    match serde_json::to_string_pretty(&report) {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("failed to encode JSON: {e}");
            std::process::exit(1);
        }
    }
}

fn print_load_human(view: &LoadView, full_summary: bool) {
    println!("═══════════════════════════════════════");
    println!("Session: {}", view.session_id);
    println!("═══════════════════════════════════════");
    println!("Dir: {}", view.session_dir.display());
    println!("Workdir: {}", view.state["workDir"].as_str().unwrap_or("?"));
    let topic_display = if view.topic.is_empty() { "(empty session)" } else { view.topic };
    println!("Topic ({}): {topic_display}", view.topic_source);
    println!(
        "State: created {} | updated {}",
        view.state["createdAt"].as_str().unwrap_or("?"),
        view.state["updatedAt"].as_str().unwrap_or("?")
    );
    println!(
        "Wire: {} | {} events scanned | window {} → {}",
        format_size(view.size_bytes),
        view.stats.events,
        fmt_local_ms(view.stats.window_first_ms),
        fmt_local_ms(view.stats.window_last_ms)
    );
    if !view.agents.is_empty() {
        println!("Agents: {}", view.agents.join(", "));
    }
    if view.stats.truncated {
        println!(
            "(session is larger than the read window — only the last {} were scanned; earlier context was not read)",
            format_size(LOAD_TAIL_BYTES)
        );
    }
    println!();

    println!("User Messages 💬 ({} total)", view.stats.user_prompts_total);
    if view.stats.user_prompts.is_empty() {
        println!("  (none in the scanned window)");
    } else {
        print_numbered_tail_verbatim(&view.stats.user_prompts, view.stats.user_prompts_total, 150, 3, 4000);
    }
    println!();

    if !view.stats.steer_kinds.is_empty() {
        let kinds: Vec<String> =
            view.stats.steer_kinds.iter().map(|(k, n)| format!("{k}:{n}")).collect();
        println!("Steers/Notifications ⚙ ({})", kinds.join(", "));
        for c in &view.stats.cron_previews {
            println!("  cron> {c}");
        }
        println!();
    }

    println!("Recent Tool Operations 🔧 ({} total)", view.tool_ops_total);
    if view.stats.tool_ops.is_empty() {
        println!("  (none in the scanned window)");
    } else {
        print_recent_tool_ops(&view.stats.tool_ops, view.tool_ops_total);
    }
    println!();

    if !view.stats.tool_hist.is_empty() {
        println!("Tool Histogram 📊");
        let mut hist: Vec<_> = view.stats.tool_hist.iter().collect();
        hist.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        for (name, n) in hist {
            println!("  {name}: {n}");
        }
        println!();
    }

    if !view.stats.files.is_empty() {
        println!("Files Touched 📁 ({} unique)", view.stats.files.len());
        for f in view.stats.files.iter().take(30) {
            println!("  {f}");
        }
        if view.stats.files.len() > 30 {
            println!("  ... ({} more)", view.stats.files.len() - 30);
        }
        println!();
    }

    println!("Assistant Texts 🤖 ({} total)", view.stats.assistant_count);
    if view.stats.assistant_texts.is_empty() {
        println!("  (none in the scanned window)");
    } else {
        print_numbered_tail_verbatim(&view.stats.assistant_texts, view.stats.assistant_count, 200, 3, 4000);
    }
    println!();

    println!("Compactions 🗜 ({})", view.stats.compactions.len());
    for (i, c) in view.stats.compactions.iter().enumerate() {
        println!(
            "  {}. tokens {} → {} | kept user msgs {} | {}",
            i + 1,
            c.tokens_before,
            c.tokens_after,
            c.kept_user_messages,
            fmt_local_ms(c.time_ms)
        );
    }
    if let Some(last) = view.stats.compactions.last() {
        println!("\n── Last compaction summary ──");
        if full_summary {
            println!("{}", last.summary);
        } else {
            println!("{}", truncate(&last.summary, 4000));
            if last.summary.chars().count() > 4000 {
                println!("... (truncated, rerun with --full-summary)");
            }
        }
    }

    if let Some(lp) = view.state["lastPrompt"].as_str() {
        if !lp.is_empty() {
            println!("\n── Last user prompt (state.json) ──\n{}", truncate(lp, 1500));
        }
    }
    println!();
}

/// Print the most recent `items` (already capped to the digest's item
/// budget by the caller), with the last `full_tail` entries shown in full
/// (capped at `full_max_chars` with an explicit `[truncated N chars]`
/// marker) and earlier ones collapsed to a single line. `total` is the true
/// count before capping, so the "N earlier, not shown" note stays accurate
/// even though `items` itself no longer carries the dropped entries.
fn print_numbered_tail_verbatim(
    items: &[(i64, String)],
    total: u64,
    short_max_chars: usize,
    full_tail: usize,
    full_max_chars: usize,
) {
    let hidden = total.saturating_sub(items.len() as u64);
    let full_start = items.len().saturating_sub(full_tail);

    for (i, (t, item)) in items.iter().enumerate() {
        let index = hidden + i as u64 + 1;
        if i >= full_start {
            let (text, cut) = truncate_reporting(item, full_max_chars);
            println!("  {index}. [{}] {text}", fmt_local_ms(*t));
            if let Some(cut) = cut {
                println!("     [truncated {cut} chars]");
            }
        } else {
            let first_line = item.lines().next().unwrap_or(item);
            println!("  {index}. [{}] {}", fmt_local_ms(*t), truncate(first_line, short_max_chars));
        }
    }
    if hidden > 0 {
        println!("  ... ({hidden} earlier, not shown)");
    }
}

fn print_recent_tool_ops(ops: &[String], total: u64) {
    let hidden = total.saturating_sub(ops.len() as u64);
    for (i, op) in ops.iter().enumerate() {
        println!("  {}. {op}", hidden + i as u64 + 1);
    }
    if hidden > 0 {
        println!("  ... ({hidden} earlier, not shown)");
    }
}

// ---------------------------------------------------------------- helpers

fn input_text(v: &Value) -> String {
    match &v["input"] {
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
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

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn iso_to_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

/// UTC, ISO 8601 — used for every timestamp in `--json` output.
fn iso_utc_ms(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339())
}

/// Local time with a numeric UTC offset — used for every timestamp in
/// human-readable output. `--json` keeps UTC (see [`iso_utc_ms`]).
fn fmt_local_ms(ms: i64) -> String {
    if ms <= 0 {
        return "?".to_string();
    }
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %z").to_string())
        .unwrap_or_else(|| "?".to_string())
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
    fn input_text_joins_text_parts() {
        let v = serde_json::json!({
            "input": [
                {"type": "text", "text": "fix the"},
                {"type": "text", "text": "widget"},
                {"type": "image", "data": "ignored"}
            ]
        });
        assert_eq!(input_text(&v), "fix the widget");
    }

    #[test]
    fn describe_tool_call_picks_the_key_argument() {
        let bash = serde_json::json!({"command": "cargo build"});
        assert_eq!(describe_tool_call("Bash", &bash), "Bash: cargo build");

        let read = serde_json::json!({"file_path": "src/main.rs"});
        assert_eq!(describe_tool_call("Read", &read), "Read: src/main.rs");

        let grep = serde_json::json!({"pattern": "fn main", "path": "src/"});
        assert_eq!(describe_tool_call("Grep", &grep), "Grep: fn main");

        let unknown = serde_json::json!({});
        assert_eq!(describe_tool_call("Mystery", &unknown), "Mystery");
    }

    #[test]
    fn first_human_prompt_from_wire_skips_non_user_origins() {
        let dir = std::env::temp_dir().join(format!(
            "kimi-session-restore-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        let wire = dir.join("wire.jsonl");
        fs::write(
            &wire,
            concat!(
                r#"{"type":"turn.prompt","time":1,"origin":{"kind":"agent"},"input":[{"type":"text","text":"bootstrap"}]}"#, "\n",
                r#"{"type":"turn.prompt","time":2,"origin":{"kind":"user"},"input":[{"type":"text","text":"real question"}]}"#, "\n",
            ),
        )
        .expect("write fixture");

        let prompt = first_human_prompt_from_wire(Some(&wire), HEAD_FALLBACK_BYTES);
        assert_eq!(prompt.as_deref(), Some("real question"));

        let _ = fs::remove_dir_all(&dir);
    }
}
