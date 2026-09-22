//! Fixture-based CLI tests for `grok-session-restore`.
//!
//! Every fixture is authored here with short, benign, synthetic text. None
//! of these tests ever reads a real `~/.grok` session — each spawns the
//! binary with `--home` pointed at a `tempfile` directory built by the test
//! itself, and `GROK_HOME` is explicitly removed from the child environment.

use serde_json::Value;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn executable() -> &'static str {
    env!("CARGO_BIN_EXE_grok-session-restore")
}

/// Build one fake session under `home/sessions/<group>/<session_id>/` with
/// the given `summary.json` body and raw `updates.jsonl` / `chat_history.jsonl`
/// text (already newline-joined JSON lines). Returns the session directory.
fn write_session(
    home: &Path,
    group: &str,
    session_id: &str,
    summary: &Value,
    updates: &str,
    chat: &str,
) -> PathBuf {
    let dir = home.join("sessions").join(group).join(session_id);
    fs::create_dir_all(&dir).expect("create fixture dirs");
    fs::write(dir.join("summary.json"), serde_json::to_string(summary).expect("serialize summary"))
        .expect("write summary.json");
    fs::write(dir.join("updates.jsonl"), updates).expect("write updates.jsonl");
    fs::write(dir.join("chat_history.jsonl"), chat).expect("write chat_history.jsonl");
    dir
}

fn write_compaction(dir: &Path, segment_name: &str, marker: &str) {
    let comp_dir = dir.join("compaction");
    fs::create_dir_all(&comp_dir).expect("create compaction dir");
    fs::write(
        comp_dir.join("INDEX.md"),
        format!(
            "# Compaction segments\n\n| # | file | note |\n|---|------|------|\n| 1 | {segment_name} | first pass |\n"
        ),
    )
    .expect("write INDEX.md");
    fs::write(
        comp_dir.join(segment_name),
        format!("## Summary (curated by compaction step)\n\n{marker} something something\n"),
    )
    .expect("write segment file");
}

fn run(args: &[&str], home: &Path) -> std::process::Output {
    Command::new(executable())
        .env_remove("GROK_HOME")
        .args(args)
        .arg("--home")
        .arg(home)
        .output()
        .expect("spawn grok-session-restore")
}

#[test]
fn help_flag_prints_usage_and_exits_zero() {
    let output = Command::new(executable())
        .env_remove("GROK_HOME")
        .args(["--help"])
        .output()
        .expect("spawn grok-session-restore --help");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("USAGE"));
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());
}

#[test]
fn unknown_command_exits_two_with_usage_on_stderr() {
    let output = Command::new(executable())
        .env_remove("GROK_HOME")
        .args(["bogus"])
        .output()
        .expect("spawn grok-session-restore bogus");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown command"));
    assert!(stderr.contains("USAGE"));
}

#[test]
fn unknown_flags_warn_but_do_not_fail() {
    let home = tempfile::tempdir().expect("tempdir");
    let listed = run(&["list", "--all", "--bogus-flag"], home.path());
    assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stderr).contains("warning: unknown flag: --bogus-flag"));

    let loaded = run(&["load", "some-id", "--another-bogus"], home.path());
    // Session does not exist, so `load` still exits 1 (cannot resolve) — but
    // the unknown-flag warning must appear on stderr before that failure.
    assert!(String::from_utf8_lossy(&loaded.stderr).contains("warning: unknown flag: --another-bogus"));
}

#[test]
fn verbatim_digest_surfaces_assistant_text_tool_args_and_errors_in_order() {
    let home = tempfile::tempdir().expect("tempdir");
    let summary = serde_json::json!({
        "generated_title": "Recon progress check",
        "info": {"cwd": "C:/proj/recon"},
        "created_at": "2026-09-21T04:35:00Z",
        "last_active_at": "2026-09-21T22:59:03Z",
        "current_model_id": "grok-x",
        "agent_name": "grok",
        "reasoning_effort": "high",
        "head_branch": "main",
        "head_commit": "abcdef123456",
        "session_kind": "parent",
        "last_turn_summary": "STALE PARAPHRASE: should never be the digest"
    });
    let chat = concat!(
        r#"{"type":"user","content":"проверь прогресс","prompt_index":0}"#, "\n",
    );
    let updates = concat!(
        r#"{"params":{"update":{"sessionUpdate":"tool_call","toolCallId":"tc1","title":"run_terminal_command","rawInput":{"command":"Get-CimInstance Win32_Process"}}}}"#, "\n",
        r#"{"params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"tc1","status":"completed"}}}"#, "\n",
        r#"{"params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Идёт. RDAP 8720/14207 (~61%), DNS готов."}}}}"#, "\n",
        r#"{"params":{"update":{"sessionUpdate":"tool_call","toolCallId":"tc2","title":"read_file","rawInput":{"file_path":"C:/data/harvest.log"}}}}"#, "\n",
        r#"{"params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"tc2","status":"failed","content":[{"type":"content","content":{"type":"text","text":"Cannot read binary file: harvest.log"}}]}}}"#, "\n",
    );
    let dir = write_session(home.path(), "proj-recon", "01a0b65a", &summary, updates, chat);

    let human = run(&["load", dir.to_str().expect("utf8 path")], home.path());
    assert!(human.status.success(), "stderr: {}", String::from_utf8_lossy(&human.stderr));
    let stdout = String::from_utf8_lossy(&human.stdout);

    // Verbatim assistant text, in Cyrillic, never paraphrased.
    assert!(stdout.contains("Идёт. RDAP 8720/14207 (~61%), DNS готов."));
    // Verbatim tool call argument (the actual command, not just a name/count).
    assert!(stdout.contains("run_terminal_command: Get-CimInstance Win32_Process"));
    // Verbatim tool failure text.
    assert!(stdout.contains("read_file: FAILED — Cannot read binary file: harvest.log"));
    // The failure is also called out in its own Errors section.
    assert!(stdout.contains("Errors ⚠"));
    // The stale store-provided paraphrase may still be shown for
    // transparency, but only clearly labeled as not a session event, never
    // as if it were the digest itself.
    let note_pos = stdout.find("STALE PARAPHRASE").expect("store note present, labeled");
    let label_pos = stdout.find("Store note (stale, provider-written, NOT a session event)").expect("label present");
    assert!(label_pos < note_pos, "the disclaimer label must precede the quoted stale text");

    // Chronological order preserved: the successful call comes before the
    // later failed one in "Recent Tool Operations".
    let call_pos = stdout.find("run_terminal_command: Get-CimInstance").expect("call present");
    let fail_pos = stdout.find("read_file: FAILED").expect("failure present");
    assert!(call_pos < fail_pos, "tool call must precede its later failure");

    let json_out = run(&["load", dir.to_str().expect("utf8 path"), "--json"], home.path());
    assert!(json_out.status.success(), "stderr: {}", String::from_utf8_lossy(&json_out.stderr));
    let report: Value = serde_json::from_slice(&json_out.stdout).expect("parse load json");
    assert_eq!(report["schema"], "grok-session-restore-load-v1");
    assert_eq!(report["assistant_texts_total"], 1);
    assert!(report["assistant_texts"][0]
        .as_str()
        .expect("assistant text")
        .contains("RDAP 8720/14207"));
    assert_eq!(report["tool_operations_total"], 3); // 2 calls + 1 failed update
    assert_eq!(report["errors_total"], 1);
    assert!(report["errors"][0].as_str().expect("error text").contains("Cannot read binary file"));
    let ops: Vec<&str> = report["tool_operations"].as_array().expect("ops array").iter()
        .map(|v| v.as_str().expect("op string"))
        .collect();
    let call_idx = ops.iter().position(|o| o.starts_with("run_terminal_command:")).expect("call op");
    let fail_idx = ops.iter().position(|o| o.contains("FAILED")).expect("fail op");
    assert!(call_idx < fail_idx);
}

#[test]
fn compaction_summary_is_hidden_by_default_and_labeled_under_full_summary() {
    let home = tempfile::tempdir().expect("tempdir");
    let summary = serde_json::json!({
        "generated_title": "Long running task",
        "info": {"cwd": "C:/proj/b"},
        "created_at": "2026-09-01T00:00:00Z",
        "last_active_at": "2026-09-02T00:00:00Z",
        "session_kind": "parent"
    });
    let dir = write_session(home.path(), "proj-b", "01a0065c", &summary, "", "");
    write_compaction(&dir, "segment_009.md", "UNIQUE_MARKER_TEXT_12345");

    let default_run = run(&["load", dir.to_str().expect("utf8 path")], home.path());
    assert!(default_run.status.success());
    let default_stdout = String::from_utf8_lossy(&default_run.stdout);
    assert!(!default_stdout.contains("UNIQUE_MARKER_TEXT_12345"));
    assert!(default_stdout.contains("LLM-written, not session events"));
    assert!(default_stdout.contains("rerun with --full-summary"));

    let full_run = run(&["load", dir.to_str().expect("utf8 path"), "--full-summary"], home.path());
    assert!(full_run.status.success());
    let full_stdout = String::from_utf8_lossy(&full_run.stdout);
    assert!(full_stdout.contains("UNIQUE_MARKER_TEXT_12345"));
    assert!(full_stdout.contains("LLM-written, not session events; segment segment_009.md"));

    let json_out = run(&["load", dir.to_str().expect("utf8 path"), "--json"], home.path());
    let report: Value = serde_json::from_slice(&json_out.stdout).expect("parse load json");
    assert!(report["compaction_summary_llm_written"]
        .as_str()
        .expect("summary field")
        .contains("UNIQUE_MARKER_TEXT_12345"));
    assert_eq!(report["last_compaction_segment"], "segment_009.md");
}

#[test]
fn untitled_session_falls_back_to_first_human_prompt() {
    let home = tempfile::tempdir().expect("tempdir");
    let summary = serde_json::json!({
        "generated_title": "",
        "info": {"cwd": "C:/proj/c"},
        "created_at": "2026-09-21T09:00:00Z",
        "last_active_at": "2026-09-21T09:10:00Z",
        "session_kind": "parent"
    });
    let chat = concat!(
        r#"{"type":"system"}"#, "\n",
        r#"{"type":"user","synthetic_reason":"system_reminder","content":"bootstrap reminder"}"#, "\n",
        r#"{"type":"user","content":"please help me fix the login bug","prompt_index":0}"#, "\n",
    );
    let dir = write_session(home.path(), "proj-c", "01a08311", &summary, "", chat);

    let listed = run(&["list", "--all", "--json"], home.path());
    assert!(listed.status.success(), "stderr: {}", String::from_utf8_lossy(&listed.stderr));
    let report: Value = serde_json::from_slice(&listed.stdout).expect("parse list json");
    let sessions = report["sessions"].as_array().expect("sessions array");
    let entry = sessions.iter().find(|s| s["id"] == "01a08311").expect("session present");
    assert_eq!(entry["title"], "please help me fix the login bug");
    assert_eq!(entry["topic_source"], "first_prompt");

    let loaded = run(&["load", dir.to_str().expect("utf8 path"), "--json"], home.path());
    assert!(loaded.status.success());
    let load_report: Value = serde_json::from_slice(&loaded.stdout).expect("parse load json");
    assert_eq!(load_report["title"], "please help me fix the login bug");
    assert_eq!(load_report["topic_source"], "first_prompt");
}

#[test]
fn chat_history_is_read_within_a_byte_budget_and_reports_truncated() {
    let home = tempfile::tempdir().expect("tempdir");
    let summary = serde_json::json!({
        "generated_title": "Huge chat session",
        "info": {"cwd": "C:/proj/d"},
        "created_at": "2026-09-01T00:00:00Z",
        "last_active_at": "2026-09-23T00:00:00Z",
        "session_kind": "parent"
    });
    let dir = home.path().join("sessions").join("proj-d").join("01a0b51a");
    fs::create_dir_all(&dir).expect("create fixture dirs");
    fs::write(dir.join("summary.json"), serde_json::to_string(&summary).expect("serialize summary"))
        .expect("write summary.json");
    fs::write(dir.join("updates.jsonl"), "").expect("write updates.jsonl");

    let chat_path = dir.join("chat_history.jsonl");
    let mut f = File::create(&chat_path).expect("create chat_history.jsonl");

    // Marker at the very start — must land outside the 8 MiB tail window.
    writeln!(f, r#"{{"type":"user","content":"VERY_FIRST_MARKER_MUST_NOT_APPEAR","prompt_index":0}}"#)
        .expect("write first marker");

    let filler_line = format!(
        "{{\"type\":\"user\",\"synthetic_reason\":\"filler\",\"content\":\"{}\"}}\n",
        "filler-payload-".repeat(8)
    );
    let target_bytes: u64 = 9 * 1024 * 1024;
    let mut written: u64 = 0;
    while written < target_bytes {
        f.write_all(filler_line.as_bytes()).expect("write filler");
        written += filler_line.len() as u64;
    }

    // Marker at the very end — must land inside the tail window.
    writeln!(f, r#"{{"type":"user","content":"FINAL_MARKER_SHOULD_APPEAR","prompt_index":999999}}"#)
        .expect("write final marker");
    drop(f);

    let json_out = run(&["load", dir.to_str().expect("utf8 path"), "--json"], home.path());
    assert!(json_out.status.success(), "stderr: {}", String::from_utf8_lossy(&json_out.stderr));
    let report: Value = serde_json::from_slice(&json_out.stdout).expect("parse load json");
    assert_eq!(report["chat_tail_truncated"], true);
    let combined = String::from_utf8_lossy(&json_out.stdout);
    assert!(combined.contains("FINAL_MARKER_SHOULD_APPEAR"));
    assert!(!combined.contains("VERY_FIRST_MARKER_MUST_NOT_APPEAR"));

    let human = run(&["load", dir.to_str().expect("utf8 path")], home.path());
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("(tail-capped)"));
    assert!(stdout.contains("FINAL_MARKER_SHOULD_APPEAR"));
    assert!(!stdout.contains("VERY_FIRST_MARKER_MUST_NOT_APPEAR"));
}

#[test]
fn json_flag_shape_on_list_and_load() {
    let home = tempfile::tempdir().expect("tempdir");
    let summary = serde_json::json!({
        "generated_title": "Fix the build",
        "info": {"cwd": "C:/proj/e"},
        "created_at": "2026-09-20T10:00:00Z",
        "last_active_at": "2026-09-20T10:05:00Z",
        "current_model_id": "grok-x",
        "agent_name": "grok",
        "session_kind": "parent"
    });
    let chat = r#"{"type":"user","content":"please fix the widget","prompt_index":0}"#;
    let dir = write_session(home.path(), "proj-e", "session-aaa", &summary, "", chat);

    let listed = run(&["list", "--all", "--json"], home.path());
    assert!(listed.status.success(), "stderr: {}", String::from_utf8_lossy(&listed.stderr));
    let report: Value = serde_json::from_slice(&listed.stdout).expect("parse list json");
    assert_eq!(report["schema"], "grok-session-restore-list-v1");
    let sessions = report["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], "session-aaa");
    assert_eq!(sessions[0]["title"], "Fix the build");
    assert_eq!(sessions[0]["topic_source"], "title");

    let loaded = run(&["load", "session-aa", "--json"], home.path());
    assert!(loaded.status.success(), "stderr: {}", String::from_utf8_lossy(&loaded.stderr));
    let load_report: Value = serde_json::from_slice(&loaded.stdout).expect("parse load json");
    assert_eq!(load_report["schema"], "grok-session-restore-load-v1");
    assert_eq!(load_report["title"], "Fix the build");
    assert_eq!(load_report["dir"], dir.display().to_string());
    assert_eq!(load_report["user_messages"][0], "please fix the widget");
}
