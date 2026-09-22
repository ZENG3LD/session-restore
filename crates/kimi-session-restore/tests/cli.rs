//! Fixture-based CLI tests for `kimi-session-restore`.
//!
//! Every fixture is authored here with short, benign, synthetic text. None
//! of these tests ever reads a real `~/.kimi-code` session — each spawns the
//! binary with `KIMI_CODE_HOME` (or `--home`) pointed at a `tempfile`
//! directory built by the test itself.

use serde_json::Value;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn executable() -> &'static str {
    env!("CARGO_BIN_EXE_kimi-session-restore")
}

/// Build one fake session under `home/sessions/<wd_key>/<session_id>/` with
/// the given `state.json` body and raw `wire.jsonl` lines (already
/// newline-joined JSON text). Returns the session directory.
fn write_session(home: &Path, wd_key: &str, session_id: &str, state: &Value, wire: &str) -> PathBuf {
    let dir = home.join("sessions").join(wd_key).join(session_id);
    let agent_dir = dir.join("agents").join("main");
    fs::create_dir_all(&agent_dir).expect("create fixture dirs");
    fs::write(dir.join("state.json"), serde_json::to_string(state).expect("serialize state"))
        .expect("write state.json");
    fs::write(agent_dir.join("wire.jsonl"), wire).expect("write wire.jsonl");
    dir
}

fn run(args: &[&str], home: &Path) -> std::process::Output {
    Command::new(executable())
        .env_remove("KIMI_CODE_HOME")
        .args(args)
        .arg("--home")
        .arg(home)
        .output()
        .expect("spawn kimi-session-restore")
}

#[test]
fn help_flag_prints_usage_and_exits_zero() {
    let output = Command::new(executable())
        .env_remove("KIMI_CODE_HOME")
        .args(["--help"])
        .output()
        .expect("spawn kimi-session-restore --help");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("USAGE"));
}

#[test]
fn unknown_command_exits_two_with_usage_on_stderr() {
    let output = Command::new(executable())
        .env_remove("KIMI_CODE_HOME")
        .args(["bogus"])
        .output()
        .expect("spawn kimi-session-restore bogus");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown command"));
    assert!(stderr.contains("USAGE"));
}

#[test]
fn home_flag_works_without_the_environment_variable() {
    let home = tempfile::tempdir().expect("tempdir");
    let state = serde_json::json!({
        "title": "Fix the build",
        "workDir": "C:/proj/a",
        "createdAt": "2026-09-20T10:00:00Z",
        "updatedAt": "2026-09-20T10:05:00Z",
        "lastPrompt": "please fix the widget",
        "agents": {"main": {}}
    });
    let wire = concat!(
        r#"{"type":"turn.prompt","time":1758362400000,"origin":{"kind":"user"},"input":[{"type":"text","text":"please fix the widget"}]}"#, "\n",
        r#"{"type":"context.append_loop_event","time":1758362401000,"event":{"type":"content.part","part":{"type":"text","text":"Fixed it."}}}"#, "\n",
    );
    let dir = write_session(home.path(), "proj-a", "session-aaa", &state, wire);

    let listed = run(&["list", "--all", "--json"], home.path());
    assert!(listed.status.success(), "stderr: {}", String::from_utf8_lossy(&listed.stderr));
    let report: Value = serde_json::from_slice(&listed.stdout).expect("parse list json");
    let sessions = report["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], "session-aaa");
    assert_eq!(sessions[0]["topic"], "Fix the build");
    assert_eq!(sessions[0]["topic_source"], "title");

    // Id-prefix resolution should also work, not just the exact directory.
    let loaded = run(&["load", "session-aa", "--json"], home.path());
    assert!(loaded.status.success(), "stderr: {}", String::from_utf8_lossy(&loaded.stderr));
    let load_report: Value = serde_json::from_slice(&loaded.stdout).expect("parse load json");
    assert_eq!(load_report["topic"], "Fix the build");
    assert_eq!(load_report["dir"], dir.display().to_string());
}

#[test]
fn title_empty_falls_back_to_first_human_prompt_and_excludes_harness_turns() {
    let home = tempfile::tempdir().expect("tempdir");
    let state = serde_json::json!({
        "title": "",
        "workDir": "C:/proj/b",
        "createdAt": "2026-09-21T09:00:00Z",
        "updatedAt": "2026-09-21T09:10:00Z",
        "lastPrompt": "please help me fix the login bug",
        "agents": {"main": {}}
    });
    let wire = concat!(
        r#"{"type":"turn.prompt","time":1000,"origin":{"kind":"agent"},"input":[{"type":"text","text":"SYSTEM: bootstrap"}]}"#, "\n",
        r#"{"type":"turn.steer","time":1500,"origin":{"kind":"cron"},"input":[{"type":"text","text":"<cron-fire> daily check"}]}"#, "\n",
        r#"{"type":"turn.prompt","time":2000,"origin":{"kind":"user"},"input":[{"type":"text","text":"please help me fix the login bug"}]}"#, "\n",
    );
    let dir = write_session(home.path(), "proj-b", "session-bbb", &state, wire);

    let listed = run(&["list", "--all", "--json"], home.path());
    assert!(listed.status.success());
    let report: Value = serde_json::from_slice(&listed.stdout).expect("parse list json");
    assert_eq!(report["sessions"][0]["topic"], "please help me fix the login bug");
    assert_eq!(report["sessions"][0]["topic_source"], "first_prompt");

    let loaded = run(&["load", dir.to_str().expect("utf8 path"), "--json"], home.path());
    assert!(loaded.status.success(), "stderr: {}", String::from_utf8_lossy(&loaded.stderr));
    let load_report: Value = serde_json::from_slice(&loaded.stdout).expect("parse load json");
    assert_eq!(load_report["topic"], "please help me fix the login bug");
    assert_eq!(load_report["topic_source"], "first_prompt_head");
    assert_eq!(load_report["user_messages_total"], 1);
    assert_eq!(load_report["user_messages"][0]["text"], "please help me fix the login bug");

    let steer_kinds = load_report["steer_kinds"].as_array().expect("steer_kinds array");
    let find_kind = |kind: &str| {
        steer_kinds.iter().find(|k| k["kind"] == kind).map(|k| k["count"].as_u64().unwrap_or(0))
    };
    assert_eq!(find_kind("agent"), Some(1));
    assert_eq!(find_kind("cron"), Some(1));
    assert_eq!(load_report["cron_previews"][0], "<cron-fire> daily check");
}

#[test]
fn load_prints_verbatim_tail_with_truncation_marker_and_recent_tool_ops() {
    let home = tempfile::tempdir().expect("tempdir");
    let state = serde_json::json!({
        "title": "Big session",
        "workDir": "C:/proj/c",
        "createdAt": "2026-09-22T08:00:00Z",
        "updatedAt": "2026-09-22T09:00:00Z",
        "lastPrompt": "",
        "agents": {"main": {}}
    });

    let mut wire = String::new();
    let mut t = 1_000_i64;
    for i in 0..4 {
        wire.push_str(&format!(
            "{{\"type\":\"turn.prompt\",\"time\":{t},\"origin\":{{\"kind\":\"user\"}},\"input\":[{{\"type\":\"text\",\"text\":\"user message number {i}\"}}]}}\n"
        ));
        t += 1;
    }
    let long_message = "x".repeat(5000);
    wire.push_str(&format!(
        "{{\"type\":\"turn.prompt\",\"time\":{t},\"origin\":{{\"kind\":\"user\"}},\"input\":[{{\"type\":\"text\",\"text\":\"{long_message}\"}}]}}\n"
    ));
    t += 1;
    for i in 0..4 {
        wire.push_str(&format!(
            "{{\"type\":\"context.append_loop_event\",\"time\":{t},\"event\":{{\"type\":\"content.part\",\"part\":{{\"type\":\"text\",\"text\":\"assistant reply number {i}\"}}}}}}\n"
        ));
        t += 1;
    }
    // 20 tool.call events: only the last 15 should surface in "recent tool
    // operations", but the histogram/total must still count all 20.
    for i in 0..20 {
        wire.push_str(&format!(
            "{{\"type\":\"context.append_loop_event\",\"time\":{t},\"event\":{{\"type\":\"tool.call\",\"name\":\"Bash\",\"args\":{{\"command\":\"echo step-{i}\"}}}}}}\n"
        ));
        t += 1;
    }

    let dir = write_session(home.path(), "proj-c", "session-ccc", &state, &wire);

    let human = run(&["load", dir.to_str().expect("utf8 path")], home.path());
    assert!(human.status.success(), "stderr: {}", String::from_utf8_lossy(&human.stderr));
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("[truncated 1000 chars]"));
    assert!(stdout.contains("Recent Tool Operations"));
    assert!(stdout.contains("echo step-19"));
    assert!(stdout.contains("(5 earlier, not shown)"));
    assert!(!stdout.contains("echo step-4"));

    let json_out = run(&["load", dir.to_str().expect("utf8 path"), "--json"], home.path());
    assert!(json_out.status.success());
    let report: Value = serde_json::from_slice(&json_out.stdout).expect("parse load json");
    assert_eq!(report["tool_operations_total"], 20);
    assert_eq!(report["tool_operations"].as_array().expect("tool ops array").len(), 15);
    assert_eq!(report["tool_histogram"][0]["name"], "Bash");
    assert_eq!(report["tool_histogram"][0]["count"], 20);
}

#[test]
fn huge_wire_is_read_within_a_byte_budget_and_reports_truncated() {
    let home = tempfile::tempdir().expect("tempdir");
    let state = serde_json::json!({
        "title": "Huge session",
        "workDir": "C:/proj/d",
        "createdAt": "2026-09-01T00:00:00Z",
        "updatedAt": "2026-09-23T00:00:00Z",
        "lastPrompt": "",
        "agents": {"main": {}}
    });

    let dir = home.path().join("sessions").join("proj-d").join("session-ddd");
    let agent_dir = dir.join("agents").join("main");
    fs::create_dir_all(&agent_dir).expect("create fixture dirs");
    fs::write(dir.join("state.json"), serde_json::to_string(&state).expect("serialize state"))
        .expect("write state.json");

    let wire_path = agent_dir.join("wire.jsonl");
    let mut f = File::create(&wire_path).expect("create wire.jsonl");

    // Marker at the very start of the file — must land outside the 32 MiB
    // tail window on a file well over that size.
    writeln!(
        f,
        r#"{{"type":"turn.prompt","time":1,"origin":{{"kind":"user"}},"input":[{{"type":"text","text":"VERY_FIRST_MARKER_MUST_NOT_APPEAR"}}]}}"#
    )
    .expect("write first marker");

    // Padding: filler assistant text lines until the file exceeds the tail
    // budget by a comfortable margin.
    let filler_line = format!(
        "{{\"type\":\"context.append_loop_event\",\"time\":2,\"event\":{{\"type\":\"content.part\",\"part\":{{\"type\":\"text\",\"text\":\"{}\"}}}}}}\n",
        "filler-payload-".repeat(8)
    );
    let target_bytes: u64 = 40 * 1024 * 1024;
    let mut written: u64 = 0;
    while written < target_bytes {
        f.write_all(filler_line.as_bytes()).expect("write filler");
        written += filler_line.len() as u64;
    }

    // Marker at the very end — must land inside the tail window.
    writeln!(
        f,
        r#"{{"type":"turn.prompt","time":999999,"origin":{{"kind":"user"}},"input":[{{"type":"text","text":"FINAL_MARKER_SHOULD_APPEAR"}}]}}"#
    )
    .expect("write final marker");
    drop(f);

    let json_out = run(&["load", dir.to_str().expect("utf8 path"), "--json"], home.path());
    assert!(json_out.status.success(), "stderr: {}", String::from_utf8_lossy(&json_out.stderr));
    let report: Value = serde_json::from_slice(&json_out.stdout).expect("parse load json");
    assert_eq!(report["truncated"], true);
    let combined = String::from_utf8_lossy(&json_out.stdout);
    assert!(combined.contains("FINAL_MARKER_SHOULD_APPEAR"));
    assert!(!combined.contains("VERY_FIRST_MARKER_MUST_NOT_APPEAR"));

    let human = run(&["load", dir.to_str().expect("utf8 path")], home.path());
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("larger than the read window"));
    assert!(stdout.contains("FINAL_MARKER_SHOULD_APPEAR"));
    assert!(!stdout.contains("VERY_FIRST_MARKER_MUST_NOT_APPEAR"));
}
