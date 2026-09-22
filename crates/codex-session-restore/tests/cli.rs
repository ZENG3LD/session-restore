use serde_json::Value;
use std::fs::{self, File};
use std::io::Write;
use std::process::Command;

#[test]
fn isolated_cli_lists_and_loads_exact_id_without_leaking_fixture_secret() {
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions/2026/08/10");
    fs::create_dir_all(&sessions).unwrap();
    let id = "12345678-1234-4234-8234-123456789abc";
    let path = sessions.join(format!(
        "rollout-2026-08-10T10-00-00-{id}.jsonl"
    ));
    let mut file = File::create(path).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-10T10:00:00Z",
            "type": "session_meta",
            "payload": {"id": id, "cwd": "C:\\private\\project", "model_provider": "openai"}
        })
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-10T10:01:00Z",
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "token=FIXTURE_SECRET_MUST_NOT_ESCAPE"}
        })
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": "2026-08-10T10:02:00Z",
            "type": "event_msg",
            "payload": {"type": "agent_message", "message": "Use the bounded design"}
        })
    )
    .unwrap();

    let executable = env!("CARGO_BIN_EXE_codex-session-restore");
    let listed = Command::new(executable)
        .env("CODEX_HOME", home.path())
        .args(["list", "--all", "--json"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let list: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], id);
    assert!(!String::from_utf8_lossy(&listed.stdout).contains("private"));

    let loaded = Command::new(executable)
        .env("CODEX_HOME", home.path())
        .args(["load", id, "--json"])
        .output()
        .unwrap();
    assert!(loaded.status.success());
    let report: Value = serde_json::from_slice(&loaded.stdout).unwrap();
    assert_eq!(report["meta"]["id"], id);
    assert_eq!(report["messages"][1]["text"], "Use the bounded design");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&loaded.stdout),
        String::from_utf8_lossy(&loaded.stderr)
    );
    assert!(!combined.contains("FIXTURE_SECRET_MUST_NOT_ESCAPE"));
    assert!(!combined.contains("C:\\private"));
}

#[test]
fn empty_window_prints_a_widen_hint() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join("sessions")).unwrap();

    let executable = env!("CARGO_BIN_EXE_codex-session-restore");
    let output = Command::new(executable)
        .env("CODEX_HOME", home.path())
        .args(["list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Try --all or --max-age-hours"),
        "expected a widen hint, got: {stdout}"
    );

    let widened = Command::new(executable)
        .env("CODEX_HOME", home.path())
        .args(["list", "--all"])
        .output()
        .unwrap();
    assert!(widened.status.success());
    let widened_stdout = String::from_utf8_lossy(&widened.stdout);
    assert_eq!(widened_stdout.trim(), "No matching Codex sessions.");
}

#[test]
fn human_report_surfaces_tool_operations_and_errors() {
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions/2026/09/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "89abcdef-1234-4234-8234-abcdef012345";
    let path = sessions.join(format!("rollout-2026-09-01T00-00-00-{id}.jsonl"));
    let mut file = File::create(path).unwrap();
    for record in [
        serde_json::json!({
            "timestamp": "2026-09-01T00:00:00Z",
            "type": "session_meta",
            "payload": {"id": id, "cwd": "C:\\work\\demo"}
        }),
        serde_json::json!({
            "timestamp": "2026-09-01T00:00:01Z",
            "type": "response_item",
            "payload": {"type": "function_call", "name": "shell", "call_id": "call_1", "arguments": "{\"command\":\"rg TODO\"}"}
        }),
        serde_json::json!({
            "timestamp": "2026-09-01T00:00:02Z",
            "type": "event_msg",
            "payload": {"type": "task_complete", "error": {"message": "usage limit hit"}}
        }),
    ] {
        writeln!(file, "{}", serde_json::to_string(&record).unwrap()).unwrap();
    }

    let executable = env!("CARGO_BIN_EXE_codex-session-restore");
    let output = Command::new(executable)
        .env("CODEX_HOME", home.path())
        .args(["load", id])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("recent_tool_operations:"), "stdout: {stdout}");
    assert!(stdout.contains("shell: rg TODO"), "stdout: {stdout}");
    assert!(stdout.contains("- error: usage limit hit"), "stdout: {stdout}");
}
