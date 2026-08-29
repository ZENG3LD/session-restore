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

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const DEFAULT_TAIL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_USER_LIST: usize = 80;
const MAX_FILES: usize = 80;
const SUMMARY_CHARS: usize = 4000;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }
    let home = grok_home();
    match args[1].as_str() {
        "list" => cmd_list(&home, &args[2..]),
        "load" => cmd_load(&home, &args[2..]),
        "help" | "--help" | "-h" => usage(),
        other => {
            eprintln!("unknown command: {other}");
            usage();
            std::process::exit(2);
        }
    }
}

fn usage() {
    eprintln!(
        "grok-session-restore — restore context from Grok CLI sessions\n\
         \n\
         USAGE:\n\
         \x20 grok-session-restore list [--max-age-hours N] [--all] [--include-subagents] [--home PATH]\n\
         \x20 grok-session-restore load <session-dir | session-id-prefix | updates.jsonl> [--full-summary] [--home PATH]\n\
         \n\
         list  — recent parent sessions: id, time, size, cwd, title, last turn\n\
         load  — deep dive: user messages, tools, files, compaction, memory paths"
    );
}

fn grok_home() -> PathBuf {
    if let Ok(h) = std::env::var("GROK_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .expect("no USERPROFILE/HOME");
    PathBuf::from(home).join(".grok")
}

// ---------------------------------------------------------------- list

struct SessionEntry {
    dir: PathBuf,
    id: String,
    updated_ms: i64,
    size: u64,
    title: String,
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
            let title = first_str(&summary, &["generated_title", "session_summary"]);
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

fn cmd_list(home: &Path, args: &[String]) {
    let mut max_age_hours: Option<f64> = Some(12.0);
    let mut include_subagents = false;
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
            _ => {}
        }
        i += 1;
    }
    let home = override_home(home, args);
    let now = now_ms();
    let sessions = scan_sessions(&home);
    let mut shown = 0usize;
    let mut entries = Vec::new();
    for e in &sessions {
        if !include_subagents && e.kind == "subagent" {
            continue;
        }
        if let Some(h) = max_age_hours {
            if now - e.updated_ms > (h * 3_600_000.0) as i64 {
                continue;
            }
        }
        shown += 1;
        let size = if e.size > 0 {
            format!("{:.2} MB", e.size as f64 / 1e6)
        } else {
            "empty".to_string()
        };
        let live = if e.live { " [live]" } else { "" };
        println!(
            "{}. {}{}\n   {} | {} | {} | {}",
            shown,
            e.id,
            live,
            fmt_ms(e.updated_ms),
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
        entries.push(e.dir.display().to_string());
    }
    if shown == 0 {
        println!(
            "No Grok sessions found (window: {:?} hours). Try --all or --include-subagents.",
            max_age_hours
        );
        return;
    }
    println!("To load a session, use:");
    for (i, d) in entries.iter().enumerate() {
        println!("  {}. grok-session-restore load \"{}\"", i + 1, d);
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

fn cmd_load(home: &Path, args: &[String]) {
    let target = match args.first() {
        Some(t) if !t.starts_with("--") => t.clone(),
        _ => {
            eprintln!("load requires a session dir, id prefix, or updates.jsonl path");
            std::process::exit(2);
        }
    };
    let full_summary = args.iter().any(|a| a == "--full-summary");
    let home = override_home(home, args);
    let session_dir = match resolve_target(&home, &target) {
        Some(d) => d,
        None => {
            eprintln!("cannot resolve Grok session: {target}");
            std::process::exit(1);
        }
    };

    let summary = read_json(&session_dir.join("summary.json"));
    let signals = read_json(&session_dir.join("signals.json"));
    let plan = read_json(&session_dir.join("plan.json"));
    let id = session_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let live = live_ids(&home).contains(&id);
    let updates = session_dir.join("updates.jsonl");
    let chat = session_dir.join("chat_history.jsonl");
    let updates_size = file_len(&updates);
    let chat_size = file_len(&chat);

    println!("═══════════════════════════════════════");
    println!("Session: {id}");
    println!("═══════════════════════════════════════");
    println!("Dir: {}", session_dir.display());
    println!(
        "Workdir: {}",
        summary["info"]["cwd"].as_str().unwrap_or("?")
    );
    if let Some(t) = summary["generated_title"].as_str() {
        println!("Title: {t}");
    } else if let Some(t) = summary["session_summary"].as_str() {
        println!("Title: {t}");
    }
    println!(
        "Kind: {}{}",
        summary["session_kind"].as_str().unwrap_or("parent"),
        if live { " [live]" } else { "" }
    );
    println!(
        "State: created {} | updated {}",
        summary["created_at"].as_str().unwrap_or("?"),
        summary["last_active_at"]
            .as_str()
            .or_else(|| summary["updated_at"].as_str())
            .unwrap_or("?")
    );
    println!(
        "Model: {} | agent={} | effort={}",
        summary["current_model_id"].as_str().unwrap_or("?"),
        summary["agent_name"].as_str().unwrap_or("?"),
        summary["reasoning_effort"].as_str().unwrap_or("?")
    );
    println!(
        "Git: branch={} | head={}",
        summary["head_branch"].as_str().unwrap_or("?"),
        truncate(summary["head_commit"].as_str().unwrap_or("?"), 12)
    );
    println!(
        "Wire: updates {:.2} MB | chat {:.2} MB | messages {} / chat {}",
        updates_size as f64 / 1e6,
        chat_size as f64 / 1e6,
        summary["num_messages"].as_u64().unwrap_or(0),
        summary["num_chat_messages"].as_u64().unwrap_or(0)
    );
    if let Some(s) = summary["last_turn_summary"].as_str() {
        if !s.is_empty() {
            println!("Last turn: {}", truncate(&one_line(s), 240));
        }
    }
    println!();

    if !signals.is_null() {
        println!("Signals ⚙");
        println!(
            "  turns={} user={} assistant={} tools={} compactions={}",
            signals["turnCount"].as_u64().unwrap_or(0),
            signals["userMessageCount"].as_u64().unwrap_or(0),
            signals["assistantMessageCount"].as_u64().unwrap_or(0),
            signals["toolCallCount"].as_u64().unwrap_or(0),
            signals["compactionCount"].as_u64().unwrap_or(0)
        );
        if let Some(tools) = signals["toolsUsed"].as_array() {
            let names: Vec<_> = tools.iter().filter_map(|t| t.as_str()).collect();
            if !names.is_empty() {
                println!("  toolsUsed: {}", names.join(", "));
            }
        }
        println!(
            "  context {} / {} tokens",
            signals["contextTokensUsed"].as_u64().unwrap_or(0),
            signals["contextWindowTokens"].as_u64().unwrap_or(0)
        );
        println!();
    }

    let user_msgs = collect_user_messages(&chat);
    println!("User Messages 💬 ({})", user_msgs.len());
    let start = user_msgs.len().saturating_sub(MAX_USER_LIST);
    for (i, (idx, text)) in user_msgs.iter().enumerate().skip(start) {
        println!("  {}. [#{}] {}", i + 1, idx, truncate(&one_line(text), 200));
    }
    if start > 0 {
        println!("  ... ({} earlier omitted)", start);
    }
    println!();

    let (tool_hist, files, tail_truncated) = collect_tools_and_files(&updates);
    println!(
        "Tool Calls 🔧 ({} names{})",
        tool_hist.values().sum::<u64>(),
        if tail_truncated {
            format!(", tail {} MB", DEFAULT_TAIL_BYTES / (1024 * 1024))
        } else {
            String::new()
        }
    );
    let mut hist: Vec<_> = tool_hist.iter().collect();
    hist.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    for (name, n) in hist {
        println!("  {name}: {n}");
    }
    println!();

    if !files.is_empty() {
        println!("Files Touched 📁 ({} unique, from updates tail)", files.len());
        for f in files.iter().take(40) {
            println!("  {f}");
        }
        if files.len() > 40 {
            println!("  ... ({} more)", files.len() - 40);
        }
        println!();
    }

    let todos = plan["todos"].as_object();
    if let Some(todos) = todos {
        if !todos.is_empty() {
            println!("Plan todos 📋 ({})", todos.len());
            for (k, v) in todos {
                println!(
                    "  {} | {} | {}",
                    k,
                    v["status"].as_str().unwrap_or("?"),
                    truncate(v["content"].as_str().unwrap_or(""), 160)
                );
            }
            println!();
        }
    }

    let sub_dir = session_dir.join("subagents");
    if let Ok(rd) = fs::read_dir(&sub_dir) {
        let kids: Vec<_> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        if !kids.is_empty() {
            println!("Subagents 🧩 ({})", kids.len());
            for k in &kids {
                println!("  {k}");
            }
            println!();
        }
    }

    print_compaction(&session_dir, full_summary);
    print_memory_paths(&home, &summary, &id);
}

fn collect_user_messages(chat: &Path) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let Ok(f) = File::open(chat) else {
        return out;
    };
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
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
    out
}

fn collect_tools_and_files(updates: &Path) -> (BTreeMap<String, u64>, Vec<String>, bool) {
    let mut hist = BTreeMap::new();
    let mut files = Vec::new();
    let Ok(meta) = fs::metadata(updates) else {
        return (hist, files, false);
    };
    let truncated = meta.len() > DEFAULT_TAIL_BYTES;
    let Ok(lines) = tail_lines(updates, DEFAULT_TAIL_BYTES) else {
        return (hist, files, truncated);
    };
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let update = &v["params"]["update"];
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        if kind == "tool_call" {
            let name = update["title"].as_str().unwrap_or("?").to_string();
            *hist.entry(name).or_insert(0) += 1;
            push_path(&mut files, update["rawInput"]["target_directory"].as_str());
            push_path(&mut files, update["rawInput"]["target_file"].as_str());
            push_path(&mut files, update["rawInput"]["file_path"].as_str());
            push_path(&mut files, update["rawInput"]["path"].as_str());
        } else if kind == "tool_call_update" {
            if let Some(locs) = update["locations"].as_array() {
                for loc in locs {
                    push_path(&mut files, loc["path"].as_str());
                }
            }
        }
    }
    (hist, files, truncated)
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

fn print_compaction(session_dir: &Path, full: bool) {
    let index = session_dir.join("compaction").join("INDEX.md");
    if !index.exists() {
        println!("Compactions 🗜 (none)");
        return;
    }
    let Ok(idx) = fs::read_to_string(&index) else {
        println!("Compactions 🗜 (unreadable INDEX.md)");
        return;
    };
    let segments: Vec<_> = idx
        .lines()
        .filter(|l| l.starts_with("| ") && l.contains("segment_"))
        .collect();
    println!("Compactions 🗜 ({} segments)", segments.len());
    for line in &segments {
        println!("  {}", line.trim());
    }
    let last = segments.last().and_then(|l| {
        l.split('|')
            .nth(2)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });
    if let Some(name) = last {
        let path = session_dir.join("compaction").join(&name);
        if let Ok(body) = fs::read_to_string(&path) {
            let excerpt = extract_compaction_summary(&body);
            println!("\n── Last compaction summary ({name}) ──");
            if full {
                println!("{excerpt}");
            } else {
                println!("{}", truncate(&excerpt, SUMMARY_CHARS));
                if excerpt.chars().count() > SUMMARY_CHARS {
                    println!("... (truncated, rerun with --full-summary)");
                }
            }
        }
    }
    println!();
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

fn print_memory_paths(home: &Path, summary: &Value, session_id: &str) {
    let mem = home.join("memory");
    if !mem.exists() {
        println!("Memory 🧠 (disabled or empty — not the session transcript)");
        return;
    }
    println!("Memory 🧠 (separate plane, paths only — not a substitute for this session)");
    let global = mem.join("MEMORY.md");
    if global.exists() {
        println!("  global: {}", global.display());
    }
    if let Ok(rd) = fs::read_dir(&mem) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let ws = p.join("MEMORY.md");
            if ws.exists() {
                println!("  workspace: {}", ws.display());
            }
            let sess = p.join("sessions");
            if let Ok(logs) = fs::read_dir(&sess) {
                for log in logs.flatten() {
                    let name = log.file_name().to_string_lossy().into_owned();
                    if name.contains(&session_id[..session_id.len().min(8)]) {
                        println!("  session-log: {}", log.path().display());
                    }
                }
            }
        }
    }
    if let Some(git_root) = summary["git_root_dir"].as_str() {
        println!("  git_root_dir: {git_root}");
    }
}

// ---------------------------------------------------------------- helpers

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

fn truncate(s: &str, n: usize) -> String {
    let mut it = s.chars();
    let taken: String = it.by_ref().take(n).collect();
    if it.next().is_some() {
        format!("{taken}…")
    } else {
        taken
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn iso_to_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis())
}

fn fmt_ms(ms: i64) -> String {
    if ms <= 0 {
        return "?".to_string();
    }
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "?".to_string())
}
