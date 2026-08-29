//! kimi-session-restore — list and load Kimi Code CLI sessions for context restoration.
//!
//! Session layout (under ~/.kimi-code/):
//!   session_index.jsonl                      — {sessionId, sessionDir, workDir} per line
//!   sessions/<wd_key>/<session_id>/state.json — title, workDir, createdAt/updatedAt, agents
//!   sessions/<wd_key>/<session_id>/agents/<agent>/wire.jsonl — event stream
//!
//! wire.jsonl event types we use:
//!   turn.prompt / turn.steer   {input:[{type:"text",text}], origin:{kind}, time}
//!   context.append_loop_event  {event:{type: content.part|tool.call|tool.result, ...}}
//!   context.apply_compaction   {summary, compactedCount, tokensBefore, tokensAfter, time}

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }
    let home = kimi_home();
    match args[1].as_str() {
        "list" => cmd_list(&home, &args[2..]),
        "load" => cmd_load(&home, &args[2..]),
        other => {
            eprintln!("unknown command: {other}");
            usage();
            std::process::exit(2);
        }
    }
}

fn usage() {
    eprintln!(
        "kimi-session-restore — restore context from Kimi Code sessions\n\
         \n\
         USAGE:\n\
         \x20 kimi-session-restore list [--max-age-hours N] [--home PATH]\n\
         \x20 kimi-session-restore load <session-dir | session-id-prefix | wire.jsonl> [--full-summary]\n\
         \n\
         list  — recent sessions: id, time, size, workdir, title, last user prompt\n\
         load  — deep dive: user messages, tools, files, compaction summaries"
    );
}

fn kimi_home() -> PathBuf {
    if let Ok(h) = std::env::var("KIMI_CODE_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .expect("no USERPROFILE/HOME");
    PathBuf::from(home).join(".kimi-code")
}

// ---------------------------------------------------------------- list

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
            let id = dir.file_name().unwrap().to_string_lossy().into_owned();
            let state_path = dir.join("state.json");
            let (mut title, mut workdir, mut last_prompt, mut updated_ms) =
                (String::new(), String::new(), String::new(), 0i64);
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
            let (wire_size, mtime_ms) = if wire_path.exists() {
                let md = fs::metadata(&wire_path).unwrap();
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                (md.len(), mtime)
            } else {
                (0, 0)
            };
            out.push(SessionEntry {
                dir,
                id,
                updated_ms: updated_ms.max(mtime_ms),
                wire_size,
                wire_path: wire_path.exists().then_some(wire_path),
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

fn cmd_list(home: &Path, args: &[String]) {
    let mut max_age_hours: Option<f64> = Some(12.0);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max-age-hours" => {
                max_age_hours = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--all" => max_age_hours = None,
            _ => {}
        }
        i += 1;
    }
    let now = now_ms();
    let sessions = scan_sessions(home);
    let mut shown = 0usize;
    let mut entries = Vec::new();
    for e in &sessions {
        if let Some(h) = max_age_hours {
            if now - e.updated_ms > (h * 3_600_000.0) as i64 {
                continue;
            }
        }
        shown += 1;
        let size = if e.wire_size > 0 {
            format!("{:.2} MB", e.wire_size as f64 / 1e6)
        } else {
            "no wire".to_string()
        };
        println!(
            "{}. {}\n   {} | {} | {}",
            shown,
            e.id,
            fmt_ms(e.updated_ms),
            size,
            e.workdir
        );
        if !e.title.is_empty() {
            println!("   📌 {}", truncate(&e.title, 140));
        }
        if !e.last_prompt.is_empty() {
            println!("   💬 last: {}", truncate(&one_line(&e.last_prompt), 140));
        }
        println!();
        entries.push(e.dir.display().to_string());
    }
    if shown == 0 {
        println!("No sessions found (window: {:?} hours). Try --all.", max_age_hours);
        return;
    }
    println!("To load a session, use:");
    for (i, d) in entries.iter().enumerate() {
        println!("  {}. kimi-session-restore load \"{}\"", i + 1, d);
    }
}

// ---------------------------------------------------------------- load

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
    // treat as session-id prefix
    for e in scan_sessions(home) {
        if e.id.starts_with(target) {
            return e.wire_path;
        }
    }
    None
}

struct LoadStats {
    events: u64,
    first_ms: i64,
    last_ms: i64,
    user_msgs: Vec<(i64, String)>,
    steer_kinds: BTreeMap<String, u64>,
    cron_previews: Vec<String>,
    tool_hist: BTreeMap<String, u64>,
    files: Vec<String>,
    assistant_count: u64,
    assistant_tail: Vec<String>,
    compactions: Vec<Value>,
}

fn cmd_load(home: &Path, args: &[String]) {
    let target = match args.first() {
        Some(t) => t.clone(),
        None => {
            eprintln!("load requires a session dir, id prefix, or wire.jsonl path");
            std::process::exit(2);
        }
    };
    let full_summary = args.iter().any(|a| a == "--full-summary");
    let wire = match resolve_target(home, &target) {
        Some(w) => w,
        None => {
            eprintln!("cannot resolve session: {target}");
            std::process::exit(1);
        }
    };
    // wire = <session_dir>/agents/<agent>/wire.jsonl → 3 levels up
    let session_dir = wire
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_default();

    // state.json
    let state_path = session_dir.join("state.json");
    let state: Value = fs::read_to_string(&state_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);

    let mut st = LoadStats {
        events: 0,
        first_ms: 0,
        last_ms: 0,
        user_msgs: Vec::new(),
        steer_kinds: BTreeMap::new(),
        cron_previews: Vec::new(),
        tool_hist: BTreeMap::new(),
        files: Vec::new(),
        assistant_count: 0,
        assistant_tail: Vec::new(),
        compactions: Vec::new(),
    };

    let f = fs::File::open(&wire).expect("open wire.jsonl");
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
        if line.len() < 10 {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        st.events += 1;
        let t = v["time"].as_i64().unwrap_or(0);
        if t > 0 {
            if st.first_ms == 0 {
                st.first_ms = t;
            }
            st.last_ms = t;
        }
        match v["type"].as_str().unwrap_or("") {
            "turn.prompt" | "turn.steer" => {
                let kind = v["origin"]["kind"].as_str().unwrap_or("?").to_string();
                let text = input_text(&v);
                if v["type"] == "turn.prompt" && kind == "user" {
                    if !text.trim().is_empty() {
                        st.user_msgs.push((t, text));
                    }
                } else {
                    *st.steer_kinds.entry(kind.clone()).or_insert(0) += 1;
                    if (kind == "cron" || text.contains("<cron-fire"))
                        && st.cron_previews.len() < 3
                    {
                        st.cron_previews.push(truncate(&one_line(&text), 200));
                    }
                }
            }
            "context.append_loop_event" => {
                let ev = &v["event"];
                match ev["type"].as_str().unwrap_or("") {
                    "content.part" => {
                        let part = &ev["part"];
                        if part["type"] == "text" {
                            let txt = part["text"].as_str().unwrap_or("");
                            if !txt.trim().is_empty() {
                                st.assistant_count += 1;
                                st.assistant_tail.push(txt.to_string());
                                if st.assistant_tail.len() > 3 {
                                    st.assistant_tail.remove(0);
                                }
                            }
                        }
                    }
                    "tool.call" => {
                        let name = ev["name"].as_str().unwrap_or("?").to_string();
                        *st.tool_hist.entry(name.clone()).or_insert(0) += 1;
                        if matches!(name.as_str(), "Read" | "Write" | "Edit" | "Glob" | "Grep") {
                            let args = &ev["args"];
                            let p = args["path"].as_str().or_else(|| args["file_path"].as_str());
                            if let Some(p) = p {
                                let p = p.to_string();
                                if !st.files.contains(&p) && st.files.len() < 100 {
                                    st.files.push(p);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            "context.apply_compaction" => st.compactions.push(v),
            _ => {}
        }
    }

    // ---- print report
    let size = fs::metadata(&wire).map(|m| m.len()).unwrap_or(0);
    println!("═══════════════════════════════════════");
    println!("Session: {}", session_dir.file_name().map(|s| s.to_string_lossy()).unwrap_or_default());
    println!("═══════════════════════════════════════");
    println!("Dir: {}", session_dir.display());
    println!("Workdir: {}", state["workDir"].as_str().unwrap_or("?"));
    if let Some(t) = state["title"].as_str() {
        println!("Title: {t}");
    }
    println!(
        "State: created {} | updated {}",
        state["createdAt"].as_str().unwrap_or("?"),
        state["updatedAt"].as_str().unwrap_or("?")
    );
    println!(
        "Wire: {:.2} MB | {} events | {} → {}",
        size as f64 / 1e6,
        st.events,
        fmt_ms(st.first_ms),
        fmt_ms(st.last_ms)
    );
    if let Some(agents) = state["agents"].as_object() {
        let names: Vec<_> = agents.keys().map(|k| k.as_str()).collect();
        println!("Agents: {}", names.join(", "));
    }
    println!();

    println!("User Messages 💬 ({})", st.user_msgs.len());
    for (i, (t, m)) in st.user_msgs.iter().enumerate() {
        println!("  {}. [{}] {}", i + 1, fmt_ms(*t), truncate(&one_line(m), 200));
    }
    println!();

    if !st.steer_kinds.is_empty() {
        let kinds: Vec<_> = st.steer_kinds.iter().map(|(k, n)| format!("{k}:{n}")).collect();
        println!("Steers/Notifications ⚙ ({})", kinds.join(", "));
        for c in &st.cron_previews {
            println!("  cron> {c}");
        }
        println!();
    }

    println!("Tool Calls 🔧 ({} total)", st.tool_hist.values().sum::<u64>());
    let mut hist: Vec<_> = st.tool_hist.iter().collect();
    hist.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    for (name, n) in hist {
        println!("  {name}: {n}");
    }
    println!();

    if !st.files.is_empty() {
        println!("Files Touched 📁 ({} unique)", st.files.len());
        for f in st.files.iter().take(30) {
            println!("  {f}");
        }
        if st.files.len() > 30 {
            println!("  ... ({} more)", st.files.len() - 30);
        }
        println!();
    }

    println!("Assistant Texts 🤖 ({})", st.assistant_count);
    for t in &st.assistant_tail {
        println!("  --- {}", truncate(&one_line(t), 400));
    }
    println!();

    println!("Compactions 🗜 ({})", st.compactions.len());
    for (i, c) in st.compactions.iter().enumerate() {
        println!(
            "  {}. tokens {} → {} | kept user msgs {} | {}",
            i + 1,
            c["tokensBefore"].as_i64().unwrap_or(0),
            c["tokensAfter"].as_i64().unwrap_or(0),
            c["keptUserMessageCount"].as_i64().unwrap_or(0),
            fmt_ms(c["time"].as_i64().unwrap_or(0))
        );
    }
    if let Some(last) = st.compactions.last() {
        println!("\n── Last compaction summary ──");
        let s = last["summary"].as_str().unwrap_or("");
        if full_summary {
            println!("{s}");
        } else {
            println!("{}", truncate(s, 4000));
            if s.chars().count() > 4000 {
                println!("... (truncated, rerun with --full-summary)");
            }
        }
    }
    if let Some(lp) = state["lastPrompt"].as_str() {
        if !lp.is_empty() {
            println!("\n── Last user prompt (state.json) ──\n{}", truncate(lp, 1500));
        }
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
