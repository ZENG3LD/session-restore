//! `agent <session> <agentId|tool-use-id|task-id>` — one subagent's full
//! brief, status, and its own transcript digested with the same classifier
//! as `load`; or, for a background Bash task id, its command and the tail
//! of its captured output file.

use crate::digest::{self, AgentReport};
use crate::format::{local_time, truncate};
use crate::handle::format_handle;
use crate::io::{self, parse_events};
use crate::subagents::{self, BashTaskRecord, SubagentRecord};
use anyhow::{Context, Result};
use colored::Colorize;
use serde::Serialize;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Bound on the subagent's own transcript read — generous, since a
/// subagent's transcript is normally far smaller than the parent session's.
const AGENT_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;
const LAST_REPORTS_COUNT: usize = 5;
const FINAL_REPORT_CHAR_CAP: usize = 8_000;
const REPORT_CHAR_CAP: usize = 700;
const TOOL_OPERATIONS_CAP: usize = 15;
const ERRORS_CAP: usize = 10;
const FILES_CAP: usize = 20;
const TASK_OUTPUT_TAIL_BYTES: u64 = 256 * 1024;

pub fn run(path: &Path, id: &str, json: bool) -> Result<()> {
    let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string();
    let session_dir = path.parent().map(|parent| parent.join(&session_id)).unwrap_or_default();

    let (subagents, background) = subagents::scan_full(path, &session_dir)?;

    if let Some(record) = subagents.iter().find(|record| record.matches_id(id)) {
        return show_subagent(record, &session_dir, json);
    }
    if let Some(bash) = background.records.iter().find(|record| record.matches_id(id)) {
        return show_bash_task(path, bash, json);
    }

    anyhow::bail!(
        "No subagent or background task matches id: {id} (checked {} subagent(s) and {} background task(s) across the whole file)",
        subagents.len(),
        background.records.len()
    );
}

struct AgentDigest {
    reports: Vec<AgentReport>,
    footer: digest::FooterDigest,
    transcript_found: bool,
    transcript_path: PathBuf,
}

fn digest_transcript(session_dir: &Path, agent_id: &str) -> Result<AgentDigest> {
    let transcript_path = subagents::transcript_path(session_dir, agent_id);
    if !transcript_path.exists() {
        return Ok(AgentDigest { reports: Vec::new(), footer: digest::FooterDigest::default(), transcript_found: false, transcript_path });
    }
    let (lines, _truncated) = io::read_tail_lines(&transcript_path, AGENT_TRANSCRIPT_BYTES)
        .with_context(|| format!("Failed to read subagent transcript: {}", transcript_path.display()))?;
    let events = parse_events(&lines);
    // `true`: every assistant turn in a subagent's own transcript file is
    // marked `isSidechain: true` from the parent session's point of view,
    // but it is this subagent's own main chain.
    let reports = digest::last_agent_reports(&events, LAST_REPORTS_COUNT, true);
    let footer = digest::build_footer(&events);
    Ok(AgentDigest { reports, footer, transcript_found: true, transcript_path })
}

fn show_subagent(record: &SubagentRecord, session_dir: &Path, json: bool) -> Result<()> {
    let digest = match &record.agent_id {
        Some(agent_id) => digest_transcript(session_dir, agent_id)?,
        None => AgentDigest {
            reports: Vec::new(),
            footer: digest::FooterDigest::default(),
            transcript_found: false,
            transcript_path: PathBuf::new(),
        },
    };

    if json {
        print_subagent_json(record, &digest);
        return Ok(());
    }

    println!("{}", "═══════════════════════════════════════".bright_cyan());
    println!("{} {}", "Subagent:".bold(), record.short_id().bright_yellow());
    println!("{} {}", "Launch handle:".bold(), format_handle(record.offset));
    if let Some(agent_id) = &record.agent_id {
        println!("{} {}", "Agent id:".bold(), agent_id);
    }
    println!("{} {}", "Type:".bold(), record.agent_type.as_deref().unwrap_or("?"));
    println!("{} {}", "Launched:".bold(), local_time(&record.launched_at).format("%Y-%m-%d %H:%M:%S %z"));
    println!("{} {}", "Status:".bold(), record.status.label());
    println!("{} {}", "Background:".bold(), record.run_in_background);
    if let Some(description) = &record.description {
        println!("{} {}", "Description:".bold(), description);
    }

    println!("\n{}", "Brief".bold());
    match &record.prompt {
        Some(prompt) => println!("{prompt}"),
        None => println!("(no prompt recorded on the launch tool call)"),
    }

    if let Some(excerpt) = &record.excerpt {
        println!("\n{}", "Notification excerpt".bold());
        println!("{excerpt}");
    }

    if !digest.transcript_found {
        println!(
            "\n{}",
            format!("(subagent transcript not found: {})", digest.transcript_path.display()).dimmed()
        );
        return Ok(());
    }

    if let Some(final_report) = digest.reports.last() {
        println!("\n{} {}", "Final report".bold(), format_handle(final_report.offset).dimmed());
        let (text, cut) = crate::format::truncate_chars_reporting(
            &crate::format::collapse_blank_line_runs(&final_report.text),
            FINAL_REPORT_CHAR_CAP,
        );
        println!("{text}");
        if let Some(cut) = cut {
            println!("{}", format!("[truncated {cut} chars]").dimmed());
        }
    }

    if digest.reports.len() > 1 {
        println!("\n{}", "Earlier reports".bold());
        for report in &digest.reports[..digest.reports.len() - 1] {
            let text = truncate(&crate::format::collapse_paragraphs(&report.text), REPORT_CHAR_CAP);
            println!("  {} {}", format_handle(report.offset), text);
        }
    }

    print_footer(&digest.footer);
    Ok(())
}

fn print_footer(footer: &digest::FooterDigest) {
    if !footer.tool_operations.is_empty() {
        let shown = crate::format::last_n(&footer.tool_operations, TOOL_OPERATIONS_CAP);
        println!("\n{} {}", "Tool operations".bold(), format!("(last {})", shown.len()).dimmed());
        for op in shown {
            println!("  {} {}", format_handle(op.offset), truncate(&op.text, 150));
        }
    }
    if !footer.errors.is_empty() {
        let shown = crate::format::last_n(&footer.errors, ERRORS_CAP);
        println!("\n{} {}", "Errors".bold(), format!("(last {})", shown.len()).dimmed());
        for error in shown {
            println!("  {} {}", format_handle(error.offset), truncate(&error.text, 300));
        }
    }
    if !footer.files.is_empty() {
        let shown = &footer.files[..footer.files.len().min(FILES_CAP)];
        println!("\n{} {}", "Files touched".bold(), format!("({})", footer.files.len()).dimmed());
        for file in shown {
            println!("  {} (x{}) {}", format_handle(file.last_offset), file.count, file.path);
        }
    }
}

fn show_bash_task(session_path: &Path, bash: &BashTaskRecord, json: bool) -> Result<()> {
    let output = task_output(session_path, bash)?;

    if json {
        print_bash_task_json(bash, output.as_deref());
        return Ok(());
    }

    println!("{}", "═══════════════════════════════════════".bright_cyan());
    println!("{} {}", "Background Bash task:".bold(), bash.task_id.as_deref().unwrap_or(&bash.tool_use_id));
    println!("{} {}", "Launch handle:".bold(), format_handle(bash.offset));
    println!("{} {}", "Launched:".bold(), local_time(&bash.launched_at).format("%Y-%m-%d %H:%M:%S %z"));
    println!("{} {}", "Status:".bold(), bash.status.label());
    if let Some(description) = &bash.description {
        println!("{} {}", "Description:".bold(), description);
    }
    println!("\n{}", "Command".bold());
    println!("{}", bash.command.as_deref().unwrap_or("(command not recorded)"));
    if let Some(excerpt) = &bash.excerpt {
        println!("\n{}", "Notification excerpt".bold());
        println!("{excerpt}");
    }

    println!("\n{}", "Output".bold());
    match output {
        Some(text) => println!("{text}"),
        None => println!("{}", task_output_missing_note(session_path, bash).dimmed()),
    }
    Ok(())
}

fn task_output_path(session_path: &Path, task_id: &str) -> Option<PathBuf> {
    let project_slug = session_path.parent()?.file_name()?.to_str()?;
    let session_id = session_path.file_stem()?.to_str()?;
    Some(std::env::temp_dir().join("claude").join(project_slug).join(session_id).join("tasks").join(format!("{task_id}.output")))
}

fn task_output_missing_note(session_path: &Path, bash: &BashTaskRecord) -> String {
    match &bash.task_id {
        Some(task_id) => match task_output_path(session_path, task_id) {
            Some(path) => format!("task output file not found: {}", path.display()),
            None => "task output file not found: could not determine its path".to_string(),
        },
        None => "no background task id was recorded for this launch — its output file cannot be located".to_string(),
    }
}

fn task_output(session_path: &Path, bash: &BashTaskRecord) -> Result<Option<String>> {
    let Some(task_id) = &bash.task_id else { return Ok(None) };
    let Some(path) = task_output_path(session_path, task_id) else { return Ok(None) };
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(read_tail_text(&path, TASK_OUTPUT_TAIL_BYTES)?))
}

fn read_tail_text(path: &Path, max_bytes: u64) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("Failed to open task output file: {}", path.display()))?;
    let len = file.metadata().with_context(|| format!("Failed to inspect task output file: {}", path.display()))?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).with_context(|| format!("Failed to seek task output file: {}", path.display()))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).with_context(|| format!("Failed to read task output file: {}", path.display()))?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

#[derive(Serialize)]
struct AgentReportJson {
    handle: String,
    text: String,
}

#[derive(Serialize)]
struct ToolOpJson {
    handle: String,
    text: String,
}

#[derive(Serialize)]
struct ErrorJson {
    handle: String,
    text: String,
}

#[derive(Serialize)]
struct FileTouchJson {
    handle: String,
    count: u32,
    path: String,
}

#[derive(Serialize)]
struct SubagentDetailJson {
    launch_handle: String,
    agent_id: Option<String>,
    agent_type: Option<String>,
    description: Option<String>,
    prompt: Option<String>,
    launched_at: String,
    status: String,
    notification_excerpt: Option<String>,
    transcript_found: bool,
    final_report: Option<AgentReportJson>,
    earlier_reports: Vec<AgentReportJson>,
    tool_operations: Vec<ToolOpJson>,
    errors: Vec<ErrorJson>,
    files: Vec<FileTouchJson>,
}

fn print_subagent_json(record: &SubagentRecord, digest: &AgentDigest) {
    let mut reports: Vec<AgentReportJson> =
        digest.reports.iter().map(|report| AgentReportJson { handle: format_handle(report.offset), text: report.text.clone() }).collect();
    let final_report = reports.pop();

    let detail = SubagentDetailJson {
        launch_handle: format_handle(record.offset),
        agent_id: record.agent_id.clone(),
        agent_type: record.agent_type.clone(),
        description: record.description.clone(),
        prompt: record.prompt.clone(),
        launched_at: record.launched_at.to_rfc3339(),
        status: record.status.label(),
        notification_excerpt: record.excerpt.clone(),
        transcript_found: digest.transcript_found,
        final_report,
        earlier_reports: reports,
        tool_operations: digest
            .footer
            .tool_operations
            .iter()
            .map(|op| ToolOpJson { handle: format_handle(op.offset), text: op.text.clone() })
            .collect(),
        errors: digest.footer.errors.iter().map(|error| ErrorJson { handle: format_handle(error.offset), text: error.text.clone() }).collect(),
        files: digest
            .footer
            .files
            .iter()
            .map(|file| FileTouchJson { handle: format_handle(file.last_offset), count: file.count, path: file.path.clone() })
            .collect(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&detail) {
        println!("{json}");
    }
}

#[derive(Serialize)]
struct BashTaskJson {
    launch_handle: String,
    task_id: Option<String>,
    description: Option<String>,
    command: Option<String>,
    launched_at: String,
    status: String,
    notification_excerpt: Option<String>,
    output_found: bool,
    output: Option<String>,
}

fn print_bash_task_json(bash: &BashTaskRecord, output: Option<&str>) {
    let detail = BashTaskJson {
        launch_handle: format_handle(bash.offset),
        task_id: bash.task_id.clone(),
        description: bash.description.clone(),
        command: bash.command.clone(),
        launched_at: bash.launched_at.to_rfc3339(),
        status: bash.status.label(),
        notification_excerpt: bash.excerpt.clone(),
        output_found: output.is_some(),
        output: output.map(str::to_string),
    };
    if let Ok(json) = serde_json::to_string_pretty(&detail) {
        println!("{json}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagents::SubagentStatus;

    #[test]
    fn task_output_path_builds_the_expected_layout() {
        let session_path = Path::new("/home/user/.claude/projects/my-project/deadbeef-0000-4000-8000-000000000000.jsonl");
        let path = task_output_path(session_path, "task_abc123").expect("path");
        let rendered = path.to_string_lossy().replace('\\', "/");
        assert!(rendered.ends_with("claude/my-project/deadbeef-0000-4000-8000-000000000000/tasks/task_abc123.output"));
    }

    #[test]
    fn task_output_missing_note_names_the_expected_path() {
        let session_path = Path::new("/home/user/.claude/projects/my-project/deadbeef-0000-4000-8000-000000000000.jsonl");
        let bash = BashTaskRecord {
            offset: 0,
            tool_use_id: "toolu_1".to_string(),
            task_id: Some("task_abc123".to_string()),
            command: Some("cargo build".to_string()),
            description: None,
            launched_at: chrono::Utc::now(),
            status: SubagentStatus::Running,
            excerpt: None,
        };
        let note = task_output_missing_note(session_path, &bash);
        assert!(note.contains("task_abc123.output"), "unexpected note: {note}");
    }

    #[test]
    fn task_output_missing_note_without_a_task_id_says_so() {
        let session_path = Path::new("/home/user/.claude/projects/my-project/deadbeef-0000-4000-8000-000000000000.jsonl");
        let bash = BashTaskRecord {
            offset: 0,
            tool_use_id: "toolu_1".to_string(),
            task_id: None,
            command: None,
            description: None,
            launched_at: chrono::Utc::now(),
            status: SubagentStatus::Running,
            excerpt: None,
        };
        let note = task_output_missing_note(session_path, &bash);
        assert!(note.contains("no background task id"), "unexpected note: {note}");
    }
}
