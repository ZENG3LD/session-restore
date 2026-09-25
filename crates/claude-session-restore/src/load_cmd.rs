//! `load` — the bounded restore digest (plan "Wave 1"): header, first
//! prompts, last compaction summary, last owner messages, last incoming
//! from other sessions, stuck/unanswered diagnostics, open work at end,
//! last agent reports, subagents, commits made this session, files edited,
//! last tool operations, and a pre-filled wave-2 drill-down footer.
//!
//! Every item carries its `@o<offset>` handle (see [`crate::handle`]) so an
//! agent can jump straight to the full record with `show`, or to the id an
//! `agent`/`grep`/`span` command expects.

use crate::commits::{self, CommitRecord};
use crate::digest::{self, AgentReport, FooterDigest};
use crate::format::{
    collapse_blank_line_runs, collapse_paragraphs, display_path, format_size, indent_continuation, last_n, local_time,
    truncate, truncate_chars_reporting,
};
use crate::handle::format_handle;
use crate::human::{human_messages, peer_messages, scan_end_state, stuck_queue_items, HumanMessage, PeerMessage, StuckSummary};
use crate::io::{parse_events, read_head_lines, read_tail_lines, OffsetEvent};
use crate::open_work::{last_open_work_item, OpenWorkBody, OpenWorkItem};
use crate::subagents::{self, BackgroundBashTasks, SubagentRecord};
use crate::topic::{detect_topic, first_human_prompts};
use anyhow::Result;
use chrono::{DateTime, Utc};
use claude_session_restore::transcript::events::{ContentBlock, SessionEvent};
use colored::Colorize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::path::Path;

/// JSON report schema tag for `load --json`.
const SCHEMA_LOAD: &str = "claude-session-restore-load-v3";

/// Tail byte budget — generous, since a full digest is worth reading more
/// context for.
const LOAD_TAIL_BYTES: u64 = 32 * 1024 * 1024;
/// Head byte budget for "First prompts" and title/cwd/branch fallback.
const LOAD_HEAD_BYTES: u64 = 4 * 1024 * 1024;

const FIRST_PROMPTS_COUNT: usize = 3;
const LAST_OWNER_MESSAGES_COUNT: usize = 8;
const OWNER_MESSAGE_CHAR_CAP: usize = 600;
const COMPACTION_SUMMARY_CHAR_CAP: usize = 600;
const LAST_AGENT_REPORTS_COUNT: usize = 5;
const AGENT_REPORT_CHAR_CAP: usize = 700;
const LAST_AGENT_REPORT_CHAR_CAP: usize = 2000;
const LAST_PEER_MESSAGES_COUNT: usize = 3;
const PEER_MESSAGE_CHAR_CAP: usize = 300;
const SUBAGENTS_SHOWN: usize = 10;
const TOOL_OPERATIONS_CAP: usize = 10;
const STUCK_ERRORS_CAP: usize = 5;
const STUCK_ITEMS_CAP: usize = 8;
const INTERRUPTS_CAP: usize = 5;
const FILES_CAP: usize = 20;
const COMMIT_HINTS_CAP: usize = 20;
const COMMITS_SHOWN_CAP: usize = 10;
const SESSION_PREFIX_LEN: usize = 8;

pub fn run(path: &Path, json: bool, debug: bool) -> Result<()> {
    let metadata = std::fs::metadata(path)?;
    let size_bytes = metadata.len();
    let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string();

    let (tail_lines, truncated) = read_tail_lines(path, LOAD_TAIL_BYTES)?;
    let tail_events = parse_events(&tail_lines);
    let head_lines = read_head_lines(path, LOAD_HEAD_BYTES)?;
    let head_events = parse_events(&head_lines);

    if debug {
        let unknown = count_unknown_root_types(&tail_lines);
        if unknown.is_empty() {
            eprintln!("[debug] no unrecognized root event types in the scanned window");
        } else {
            eprintln!("[debug] unrecognized root event types in the scanned window:");
            for (type_name, count) in &unknown {
                eprintln!("[debug]   {type_name}: {count}");
            }
        }
    }

    let window = post_compaction_window(&tail_events);
    let session_dir = path.parent().map(|parent| parent.join(&session_id)).unwrap_or_default();

    let (subagents_full, background) = subagents::scan(&tail_lines, &session_dir);
    let subagents_shown = last_n(&subagents_full, SUBAGENTS_SHOWN);

    let header = build_header(&session_id, path, size_bytes, &head_events, &tail_events, subagents_full.len());
    let first_prompts = first_human_prompts(&head_events, FIRST_PROMPTS_COUNT);
    let compaction_summary = find_compact_summary(&head_events, &tail_events);
    let owner_messages = last_n(&human_messages(window), LAST_OWNER_MESSAGES_COUNT).to_vec();
    let peers = last_n(&peer_messages(window), LAST_PEER_MESSAGES_COUNT).to_vec();
    let stuck = stuck_queue_items(window);
    let end_state = scan_end_state(window);
    let open_work = last_open_work_item(window);
    let agent_reports = digest::last_agent_reports(window, LAST_AGENT_REPORTS_COUNT, false);
    let footer = digest::build_footer(window);
    let commit_hints = digest::commit_hint_scan(window);
    let commits = commits::scan(window, header.cwd.as_deref());

    let report = LoadReport {
        header,
        first_prompts,
        compaction_summary,
        owner_messages,
        peers,
        stuck,
        unanswered: end_state.unanswered,
        interrupt_handles: end_state.interrupts.iter().map(|marker| marker.offset).collect(),
        open_work,
        agent_reports,
        subagents_shown: subagents_shown.to_vec(),
        background,
        commits,
        commit_hints,
        footer,
        truncated,
    };

    if json {
        print_json(&report);
    } else {
        print_human(&report);
    }
    Ok(())
}

// ============================================================================
// Header
// ============================================================================

struct Header {
    session_id: String,
    jsonl_path: String,
    cwd: Option<String>,
    git_branch: Option<String>,
    entrypoint: Option<String>,
    version: Option<String>,
    first_event_time: Option<DateTime<Utc>>,
    last_event_time: Option<DateTime<Utc>>,
    size_bytes: u64,
    topic: String,
    topic_source: String,
    compaction_count: u64,
    last_compaction_time: Option<DateTime<Utc>>,
    subagent_count: usize,
}

fn build_header(
    session_id: &str,
    path: &Path,
    size_bytes: u64,
    head: &[OffsetEvent],
    tail: &[OffsetEvent],
    subagent_count: usize,
) -> Header {
    let (topic, topic_source) = detect_topic(head, tail);
    let cwd = find_metadata_field(tail, head, |m| m.cwd.clone());
    let git_branch = find_metadata_field(tail, head, |m| m.git_branch.clone());
    let entrypoint = find_metadata_field(tail, head, |m| m.entrypoint.clone());
    let version = find_metadata_field(tail, head, |m| m.version.clone());
    let first_event_time = head
        .iter()
        .find(|oe| has_real_timestamp(&oe.event))
        .or_else(|| tail.iter().find(|oe| has_real_timestamp(&oe.event)))
        .map(|oe| oe.event.timestamp());
    let last_event_time = tail
        .iter()
        .rev()
        .find(|oe| has_real_timestamp(&oe.event))
        .or_else(|| head.iter().rev().find(|oe| has_real_timestamp(&oe.event)))
        .map(|oe| oe.event.timestamp());
    let (compaction_count, last_compaction_time) = compaction_stats(head, tail);

    Header {
        session_id: session_id.to_string(),
        jsonl_path: display_path(path),
        cwd,
        git_branch,
        entrypoint,
        version,
        first_event_time,
        last_event_time,
        size_bytes,
        topic,
        topic_source: topic_source.to_string(),
        compaction_count,
        last_compaction_time,
        subagent_count,
    }
}

/// Whether `event` carries a genuine on-disk timestamp. A handful of
/// harness-internal marker events (`custom-title`, `ai-title`, `last-prompt`,
/// `bridge-session`, `atis-latch`, `mode`, `permission-mode`, `agent-name`,
/// and unrecognized types) have no timestamp field at all —
/// [`SessionEvent::timestamp`] falls back to "now" for those, which would
/// otherwise corrupt the header's first/last-event-time display with
/// whatever instant this CLI happened to run at.
fn has_real_timestamp(event: &SessionEvent) -> bool {
    !matches!(
        event,
        SessionEvent::CustomTitle(_)
            | SessionEvent::AiTitle(_)
            | SessionEvent::LastPrompt(_)
            | SessionEvent::BridgeSession(_)
            | SessionEvent::AtisLatch(_)
            | SessionEvent::Mode(_)
            | SessionEvent::PermissionMode(_)
            | SessionEvent::AgentName(_)
            | SessionEvent::Unknown
    )
}

fn find_metadata_field(
    tail: &[OffsetEvent],
    head: &[OffsetEvent],
    extract: impl Fn(&claude_session_restore::transcript::events::EventMetadata) -> Option<String>,
) -> Option<String> {
    tail.iter()
        .rev()
        .find_map(|oe| oe.event.metadata().and_then(|m| extract(&m)))
        .or_else(|| head.iter().rev().find_map(|oe| oe.event.metadata().and_then(|m| extract(&m))))
}

fn compaction_stats(head: &[OffsetEvent], tail: &[OffsetEvent]) -> (u64, Option<DateTime<Utc>>) {
    let mut seen = std::collections::HashSet::new();
    let mut count = 0_u64;
    let mut last: Option<DateTime<Utc>> = None;

    for oe in head.iter().chain(tail.iter()) {
        let SessionEvent::System(sys) = &oe.event else { continue };
        if !sys.is_compact_boundary() {
            continue;
        }
        let key = sys.uuid.clone().unwrap_or_else(|| sys.timestamp.to_rfc3339());
        if seen.insert(key) {
            count += 1;
            last = Some(last.map_or(sys.timestamp, |prev| prev.max(sys.timestamp)));
        }
    }

    (count, last)
}

// ============================================================================
// Compaction windowing: events after the last compact boundary in the tail
// window, when one is present.
// ============================================================================

fn post_compaction_window(tail: &[OffsetEvent]) -> &[OffsetEvent] {
    let boundary =
        tail.iter().rposition(|oe| matches!(&oe.event, SessionEvent::System(sys) if sys.is_compact_boundary()));
    boundary.map_or(tail, |index| &tail[index + 1..])
}

// ============================================================================
// Last compaction summary (plan load section 3)
// ============================================================================

struct CompactionSummary {
    offset: u64,
    timestamp: DateTime<Utc>,
    total_chars: usize,
    excerpt: String,
}

/// The most recent compaction summary — checked across the whole tail
/// window first (titles/summaries repeat and the tail almost always
/// carries the latest one), the head window as a fallback.
fn find_compact_summary(head: &[OffsetEvent], tail: &[OffsetEvent]) -> Option<CompactionSummary> {
    tail.iter()
        .rev()
        .chain(head.iter().rev())
        .find_map(|oe| {
            let SessionEvent::User(user) = &oe.event else { return None };
            if user.is_compact_summary != Some(true) {
                return None;
            }
            let text = user.message.content.iter().find_map(ContentBlock::as_text)?;
            Some((oe.offset, user.timestamp(), text.to_string()))
        })
        .map(|(offset, timestamp, text)| {
            let total_chars = text.chars().count();
            let excerpt: String = text.chars().take(COMPACTION_SUMMARY_CHAR_CAP).collect();
            CompactionSummary { offset, timestamp, total_chars, excerpt }
        })
}

// ============================================================================
// JSON output
// ============================================================================

struct LoadReport {
    header: Header,
    first_prompts: Vec<(u64, String)>,
    compaction_summary: Option<CompactionSummary>,
    owner_messages: Vec<HumanMessage>,
    peers: Vec<PeerMessage>,
    stuck: StuckSummary,
    unanswered: bool,
    interrupt_handles: Vec<u64>,
    open_work: Option<OpenWorkItem>,
    agent_reports: Vec<AgentReport>,
    subagents_shown: Vec<SubagentRecord>,
    background: BackgroundBashTasks,
    commits: Vec<CommitRecord>,
    commit_hints: Vec<String>,
    footer: FooterDigest,
    truncated: bool,
}

#[derive(Serialize)]
struct HeaderJson {
    session_id: String,
    jsonl_path: String,
    cwd: Option<String>,
    git_branch: Option<String>,
    entrypoint: Option<String>,
    version: Option<String>,
    first_event_time: Option<String>,
    last_event_time: Option<String>,
    size_bytes: u64,
    topic: String,
    topic_source: String,
    compaction_count: u64,
    last_compaction_time: Option<String>,
    subagent_count: usize,
}

#[derive(Serialize)]
struct HandleTextJson {
    handle: String,
    text: String,
}

#[derive(Serialize)]
struct CompactionSummaryJson {
    handle: String,
    timestamp: String,
    total_chars: usize,
    excerpt: String,
}

#[derive(Serialize)]
struct HumanMessageJson {
    handle: String,
    text: String,
    timestamp: String,
    delivery: &'static str,
}

#[derive(Serialize)]
struct PeerMessageJson {
    handle: String,
    text: String,
    sender: Option<String>,
    handback: bool,
    timestamp: String,
}

#[derive(Serialize)]
struct StuckItemJson {
    handle: Option<String>,
    kind: &'static str,
    text: String,
    timestamp: Option<String>,
}

#[derive(Serialize)]
struct OpenWorkTodoJson {
    status: String,
    content: String,
}

#[derive(Serialize)]
struct OpenWorkJson {
    handle: String,
    timestamp: String,
    tool_name: String,
    todos: Option<Vec<OpenWorkTodoJson>>,
    todos_omitted: usize,
    raw_input: Option<String>,
    raw_input_truncated_chars: Option<usize>,
}

#[derive(Serialize)]
struct SubagentJson {
    handle: String,
    short_id: String,
    agent_id: Option<String>,
    description: Option<String>,
    agent_type: Option<String>,
    prompt: Option<String>,
    run_in_background: bool,
    launched_at: String,
    status: String,
    excerpt: Option<String>,
}

#[derive(Serialize)]
struct BackgroundBashJson {
    launched: usize,
    still_running: Vec<String>,
}

#[derive(Serialize)]
struct CommitJson {
    handle: String,
    repo: Option<String>,
    branch: String,
    hash: String,
    subject: String,
}

#[derive(Serialize)]
struct FileTouchJson {
    handle: String,
    count: u32,
    path: String,
}

#[derive(Serialize)]
struct LoadReportJson {
    schema: &'static str,
    header: HeaderJson,
    first_prompts: Vec<HandleTextJson>,
    last_compaction_summary: Option<CompactionSummaryJson>,
    last_owner_messages: Vec<HumanMessageJson>,
    last_incoming_from_other_sessions: Vec<PeerMessageJson>,
    stuck: Vec<StuckItemJson>,
    stuck_other_leftover_count: usize,
    open_work: Option<OpenWorkJson>,
    last_agent_reports: Vec<HandleTextJson>,
    subagents: Vec<SubagentJson>,
    background_bash: BackgroundBashJson,
    commits: Vec<CommitJson>,
    commit_hints: Vec<String>,
    tool_operations: Vec<HandleTextJson>,
    files: Vec<FileTouchJson>,
    truncated: bool,
}

fn print_json(report: &LoadReport) {
    let mut stuck_json: Vec<StuckItemJson> = report
        .stuck
        .owner_items
        .iter()
        .map(|item| StuckItemJson {
            handle: Some(format_handle(item.offset)),
            kind: "queued_never_delivered",
            text: item.content.clone(),
            timestamp: Some(item.enqueued_at.to_rfc3339()),
        })
        .collect();
    if report.unanswered {
        stuck_json.push(StuckItemJson { handle: None, kind: "unanswered", text: String::new(), timestamp: None });
    }
    for offset in &report.interrupt_handles {
        stuck_json.push(StuckItemJson { handle: Some(format_handle(*offset)), kind: "interrupted", text: String::new(), timestamp: None });
    }
    for error in last_n(&report.footer.errors, STUCK_ERRORS_CAP) {
        stuck_json.push(StuckItemJson { handle: Some(format_handle(error.offset)), kind: "error", text: error.text.clone(), timestamp: None });
    }

    let open_work = report.open_work.as_ref().map(|item| match &item.body {
        OpenWorkBody::Todos { items, omitted } => OpenWorkJson {
            handle: format_handle(item.offset),
            timestamp: item.timestamp.to_rfc3339(),
            tool_name: item.tool_name.clone(),
            todos: Some(items.iter().map(|todo| OpenWorkTodoJson { status: todo.status.clone(), content: todo.content.clone() }).collect()),
            todos_omitted: *omitted,
            raw_input: None,
            raw_input_truncated_chars: None,
        },
        OpenWorkBody::RawInput { text, truncated_chars } => OpenWorkJson {
            handle: format_handle(item.offset),
            timestamp: item.timestamp.to_rfc3339(),
            tool_name: item.tool_name.clone(),
            todos: None,
            todos_omitted: 0,
            raw_input: Some(text.clone()),
            raw_input_truncated_chars: *truncated_chars,
        },
    });

    let json_report = LoadReportJson {
        schema: SCHEMA_LOAD,
        header: HeaderJson {
            session_id: report.header.session_id.clone(),
            jsonl_path: report.header.jsonl_path.clone(),
            cwd: report.header.cwd.clone(),
            git_branch: report.header.git_branch.clone(),
            entrypoint: report.header.entrypoint.clone(),
            version: report.header.version.clone(),
            first_event_time: report.header.first_event_time.map(|value| value.to_rfc3339()),
            last_event_time: report.header.last_event_time.map(|value| value.to_rfc3339()),
            size_bytes: report.header.size_bytes,
            topic: report.header.topic.clone(),
            topic_source: report.header.topic_source.clone(),
            compaction_count: report.header.compaction_count,
            last_compaction_time: report.header.last_compaction_time.map(|value| value.to_rfc3339()),
            subagent_count: report.header.subagent_count,
        },
        first_prompts: report.first_prompts.iter().map(|(offset, text)| HandleTextJson { handle: format_handle(*offset), text: text.clone() }).collect(),
        last_compaction_summary: report.compaction_summary.as_ref().map(|summary| CompactionSummaryJson {
            handle: format_handle(summary.offset),
            timestamp: summary.timestamp.to_rfc3339(),
            total_chars: summary.total_chars,
            excerpt: summary.excerpt.clone(),
        }),
        last_owner_messages: report
            .owner_messages
            .iter()
            .map(|message| HumanMessageJson {
                handle: format_handle(message.offset),
                text: message.text.clone(),
                timestamp: message.timestamp.to_rfc3339(),
                delivery: message.delivery.label(),
            })
            .collect(),
        last_incoming_from_other_sessions: report
            .peers
            .iter()
            .map(|peer| PeerMessageJson {
                handle: format_handle(peer.offset),
                text: peer.text.clone(),
                sender: peer.sender.clone(),
                handback: peer.handback,
                timestamp: peer.timestamp.to_rfc3339(),
            })
            .collect(),
        stuck: stuck_json,
        stuck_other_leftover_count: report.stuck.other_leftover_count,
        open_work,
        last_agent_reports: report.agent_reports.iter().map(|report| HandleTextJson { handle: format_handle(report.offset), text: report.text.clone() }).collect(),
        subagents: report
            .subagents_shown
            .iter()
            .map(|record| SubagentJson {
                handle: format_handle(record.offset),
                short_id: record.short_id(),
                agent_id: record.agent_id.clone(),
                description: record.description.clone(),
                agent_type: record.agent_type.clone(),
                prompt: record.prompt.clone(),
                run_in_background: record.run_in_background,
                launched_at: record.launched_at.to_rfc3339(),
                status: record.status.label(),
                excerpt: record.excerpt.clone(),
            })
            .collect(),
        background_bash: BackgroundBashJson { launched: report.background.launched, still_running: report.background.still_running.clone() },
        commits: report
            .commits
            .iter()
            .map(|commit| CommitJson {
                handle: format_handle(commit.offset),
                repo: commit.repo.clone(),
                branch: commit.branch.clone(),
                hash: commit.hash.clone(),
                subject: commit.subject.clone(),
            })
            .collect(),
        commit_hints: report.commit_hints.clone(),
        tool_operations: last_n(&report.footer.tool_operations, TOOL_OPERATIONS_CAP)
            .iter()
            .map(|op| HandleTextJson { handle: format_handle(op.offset), text: op.text.clone() })
            .collect(),
        files: report
            .footer
            .files
            .iter()
            .take(FILES_CAP)
            .map(|file| FileTouchJson { handle: format_handle(file.last_offset), count: file.count, path: file.path.clone() })
            .collect(),
        truncated: report.truncated,
    };

    if let Ok(json) = serde_json::to_string_pretty(&json_report) {
        println!("{json}");
    }
}

// ============================================================================
// Human-readable output
// ============================================================================

fn print_human(report: &LoadReport) {
    print_header(&report.header);
    print_first_prompts(&report.first_prompts);
    print_compaction_summary(&report.compaction_summary);
    print_owner_messages(&report.owner_messages);
    print_peers(&report.peers);
    print_stuck(&report.stuck, report.unanswered, &report.interrupt_handles, report.owner_messages.last(), &report.footer.errors);
    print_open_work(&report.open_work, &report.subagents_shown, &report.background);
    print_agent_reports(&report.agent_reports);
    print_subagents(&report.subagents_shown, &report.background);
    print_commits(&report.commits, &report.commit_hints);
    print_files(&report.footer.files);
    print_tool_operations(&report.footer.tool_operations);
    print_footer_commands(report);

    if report.truncated {
        println!("\n{}", "(session is larger than the read window — earlier context was not scanned)".dimmed());
    }
    println!();
}

fn print_header(header: &Header) {
    println!("{}", "═══════════════════════════════════════".bright_cyan());
    println!("{} {}", "Session:".bold(), header.session_id.bright_yellow());
    println!("{}", "═══════════════════════════════════════".bright_cyan());
    println!("{} {}", "Path:".bold(), header.jsonl_path);
    if let Some(cwd) = &header.cwd {
        println!("{} {}", "Cwd:".bold(), cwd);
    }
    if let Some(branch) = &header.git_branch {
        println!("{} {}", "Git branch:".bold(), branch.bright_cyan());
    }
    if let Some(entrypoint) = &header.entrypoint {
        println!("{} {}", "Entrypoint:".bold(), entrypoint);
    }
    if let Some(version) = &header.version {
        println!("{} {}", "Version:".bold(), version);
    }
    if let Some(first) = header.first_event_time {
        println!("{} {}", "First event:".bold(), local_time(&first).format("%Y-%m-%d %H:%M:%S %z"));
    }
    if let Some(last) = header.last_event_time {
        println!("{} {}", "Last event:".bold(), local_time(&last).format("%Y-%m-%d %H:%M:%S %z"));
    }
    println!("{} {}", "Size:".bold(), format_size(header.size_bytes));
    println!("{} {} ({})", "Topic:".bold(), header.topic.bright_green(), header.topic_source.dimmed());
    if header.compaction_count > 0 {
        let last = header
            .last_compaction_time
            .map_or_else(String::new, |value| format!(", last {}", local_time(&value).format("%Y-%m-%d %H:%M:%S")));
        println!("{} {}{}", "Compactions:".bold(), header.compaction_count, last);
    }
    println!("{} {}", "Subagents launched:".bold(), header.subagent_count);
}

fn print_first_prompts(first_prompts: &[(u64, String)]) {
    if first_prompts.is_empty() {
        return;
    }
    println!("\n{}", "First prompts".bold());
    for (index, (offset, text)) in first_prompts.iter().enumerate() {
        println!(
            "  {}. {} «{}»",
            index + 1,
            format_handle(*offset).dimmed(),
            truncate(&collapse_paragraphs(text), 400).bright_white()
        );
    }
}

fn print_compaction_summary(summary: &Option<CompactionSummary>) {
    let Some(summary) = summary else { return };
    println!("\n{}", "Last compaction summary".bold());
    println!(
        "  {} [{}] {} chars total",
        format_handle(summary.offset).dimmed(),
        local_time(&summary.timestamp).format("%Y-%m-%d %H:%M:%S"),
        summary.total_chars
    );
    println!("  «{}»", summary.excerpt.bright_white());
    if summary.total_chars > summary.excerpt.chars().count() {
        println!("  {}", "(show this handle for the full summary)".dimmed());
    }
}

fn print_owner_messages(messages: &[HumanMessage]) {
    if messages.is_empty() {
        return;
    }
    println!("\n{}", "Last owner messages".bold());
    for (index, message) in messages.iter().enumerate() {
        let collapsed = collapse_paragraphs(&message.text);
        let (text, cut) = truncate_chars_reporting(&collapsed, OWNER_MESSAGE_CHAR_CAP);
        println!(
            "  {}. {} [{}] ({}) «{}»",
            index + 1,
            format_handle(message.offset).dimmed(),
            local_time(&message.timestamp).format("%H:%M:%S"),
            message.delivery.label(),
            text.bright_white()
        );
        if let Some(cut) = cut {
            println!("     {}", format!("[truncated {cut} chars]").dimmed());
        }
    }
}

fn print_peers(peers: &[PeerMessage]) {
    if peers.is_empty() {
        return;
    }
    println!("\n{}", "Last incoming from other sessions".bold());
    for (index, peer) in peers.iter().enumerate() {
        let sender = peer.sender.as_deref().unwrap_or("unknown sender");
        let handback = if peer.handback { " [subagent hand-back]".yellow().to_string() } else { String::new() };
        let collapsed = collapse_paragraphs(&peer.text);
        let (text, cut) = truncate_chars_reporting(&collapsed, PEER_MESSAGE_CHAR_CAP);
        println!(
            "  {}. {} [{}] {}{} «{}»",
            index + 1,
            format_handle(peer.offset).dimmed(),
            local_time(&peer.timestamp).format("%H:%M:%S"),
            sender.bright_cyan(),
            handback,
            text.bright_white()
        );
        if let Some(cut) = cut {
            println!("     {}", format!("[truncated {cut} chars]").dimmed());
        }
    }
}

fn print_stuck(
    stuck: &StuckSummary,
    unanswered: bool,
    interrupt_handles: &[u64],
    last_owner_message: Option<&HumanMessage>,
    errors: &[crate::digest::ErrorRecord],
) {
    let visible_errors = last_n(errors, STUCK_ERRORS_CAP);
    let visible_interrupts = last_n(interrupt_handles, INTERRUPTS_CAP);
    let visible_stuck = last_n(&stuck.owner_items, STUCK_ITEMS_CAP);
    if visible_stuck.is_empty() && stuck.other_leftover_count == 0 && !unanswered && visible_interrupts.is_empty() && visible_errors.is_empty() {
        return;
    }

    println!("\n{}", "Stuck / not answered".bold());
    if stuck.owner_items.len() > visible_stuck.len() {
        println!("  ({} queued-never-delivered item(s), showing last {})", stuck.owner_items.len(), visible_stuck.len());
    }
    for item in visible_stuck {
        println!(
            "  - {} queued, never delivered: «{}» (enqueued {})",
            format_handle(item.offset).dimmed(),
            truncate(&collapse_paragraphs(&item.content), 100).bright_white(),
            local_time(&item.enqueued_at).format("%H:%M:%S")
        );
    }
    if stuck.other_leftover_count > 0 {
        println!("  - {} other queued item(s) (peer/task-notification) never delivered", stuck.other_leftover_count);
    }
    if unanswered {
        match last_owner_message {
            Some(message) => println!(
                "  - {} unanswered at session end: «{}»",
                format_handle(message.offset).dimmed(),
                truncate(&collapse_paragraphs(&message.text), 100).bright_white()
            ),
            None => println!("  - unanswered at session end"),
        }
    }
    for offset in visible_interrupts {
        println!("  - {} interruption marker ([Request interrupted by user])", format_handle(*offset).dimmed());
    }
    for error in visible_errors {
        println!("  - {} error: {}", format_handle(error.offset).dimmed(), truncate(&error.text, 200).bright_white());
    }
}

fn print_open_work(open_work: &Option<OpenWorkItem>, subagents: &[SubagentRecord], background: &BackgroundBashTasks) {
    let running_subagents: Vec<&SubagentRecord> =
        subagents.iter().filter(|record| record.status == crate::subagents::SubagentStatus::Running).collect();
    if open_work.is_none() && running_subagents.is_empty() && background.still_running.is_empty() {
        return;
    }

    println!("\n{}", "Open work at end".bold());
    if let Some(item) = open_work {
        println!(
            "  Last {} {} [{}]:",
            item.tool_name,
            format_handle(item.offset).dimmed(),
            local_time(&item.timestamp).format("%H:%M:%S")
        );
        match &item.body {
            OpenWorkBody::Todos { items, omitted } => {
                for todo in items {
                    println!("    - [{}] {}", todo.status, todo.content);
                }
                if *omitted > 0 {
                    println!("    ({omitted} more todo(s) not shown)");
                }
            }
            OpenWorkBody::RawInput { text, truncated_chars } => {
                println!("    {text}");
                if let Some(cut) = truncated_chars {
                    println!("    {}", format!("[truncated {cut} chars]").dimmed());
                }
            }
        }
    }
    if !running_subagents.is_empty() {
        let ids: Vec<String> = running_subagents.iter().map(|record| record.short_id()).collect();
        println!("  Running subagents: {}", ids.join(", "));
    }
    if !background.still_running.is_empty() {
        println!("  Background Bash tasks still running: {}", background.still_running.join(", "));
    }
}

fn print_agent_reports(reports: &[AgentReport]) {
    if reports.is_empty() {
        return;
    }
    println!("\n{}", "Last agent reports".bold());
    let last_index = reports.len() - 1;
    for (index, report) in reports.iter().enumerate() {
        let cap = if index == last_index { LAST_AGENT_REPORT_CHAR_CAP } else { AGENT_REPORT_CHAR_CAP };
        let collapsed = collapse_blank_line_runs(&report.text);
        let (text, cut) = truncate_chars_reporting(&collapsed, cap);
        let text = indent_continuation(&text, "     ");
        println!("  {}. {} {}", index + 1, format_handle(report.offset).dimmed(), text.bright_white());
        if let Some(cut) = cut {
            println!("     {}", format!("[truncated {cut} chars]").dimmed());
        }
    }
}

fn print_subagents(records: &[SubagentRecord], background: &BackgroundBashTasks) {
    if records.is_empty() && background.launched == 0 {
        return;
    }
    println!("\n{}", "Subagents".bold());
    for record in records {
        let agent_type = record.agent_type.as_deref().unwrap_or("?");
        let description = record.description.as_deref().unwrap_or("(no description)");
        println!(
            "  - {} {} [{}] {} — launched {}, {}",
            format_handle(record.offset).dimmed(),
            record.short_id().bright_yellow(),
            agent_type,
            description,
            local_time(&record.launched_at).format("%H:%M:%S"),
            record.status.label()
        );
        if let Some(excerpt) = &record.excerpt {
            let (text, _) = truncate_chars_reporting(&collapse_blank_line_runs(excerpt.trim()), 300);
            println!("    {}", indent_continuation(&text, "    ").dimmed());
        }
    }
    if background.launched > 0 {
        let ids = if background.still_running.is_empty() {
            String::new()
        } else {
            format!(" ({})", background.still_running.join(", "))
        };
        println!("  Background Bash tasks: {} launched, {} still running{ids}", background.launched, background.still_running.len());
    }
}

fn print_commits(commits: &[CommitRecord], commit_hints: &[String]) {
    if !commits.is_empty() {
        println!("\n{}", "Commits made in this session".bold());
        for commit in commits.iter().take(COMMITS_SHOWN_CAP) {
            let repo = commit.repo.as_deref().unwrap_or("?");
            println!(
                "  - {} [{repo}] {} {} {}",
                format_handle(commit.offset).dimmed(),
                commit.branch.bright_cyan(),
                commit.hash.bright_yellow(),
                commit.subject
            );
        }
        if commits.len() > COMMITS_SHOWN_CAP {
            println!("  ({} more commit(s) not shown)", commits.len() - COMMITS_SHOWN_CAP);
        }
    }
    if !commit_hints.is_empty() {
        println!("\n{}", "Commit hints (unconfirmed, from free text)".bold());
        for hint in commit_hints.iter().take(COMMIT_HINTS_CAP) {
            println!("  - {}", hint.dimmed());
        }
    }
}

fn print_files(files: &[crate::digest::FileTouch]) {
    if files.is_empty() {
        return;
    }
    println!("\n{} {}", "Files edited".bold(), format!("({}, most recent first)", files.len()).dimmed());
    for file in files.iter().take(FILES_CAP) {
        println!("  - {} (x{}) {}", format_handle(file.last_offset).dimmed(), file.count, file.path.bright_white());
    }
}

fn print_tool_operations(tool_operations: &[crate::digest::ToolOpRecord]) {
    if tool_operations.is_empty() {
        return;
    }
    let shown = last_n(tool_operations, TOOL_OPERATIONS_CAP);
    println!("\n{} {}", "Last tool operations".bold(), format!("(last {})", shown.len()).dimmed());
    for (index, op) in shown.iter().enumerate() {
        println!("  {}. {} {}", index + 1, format_handle(op.offset).dimmed(), truncate(&op.text, 150).bright_white());
    }
}

fn print_footer_commands(report: &LoadReport) {
    let prefix = &report.header.session_id[..report.header.session_id.len().min(SESSION_PREFIX_LEN)];
    let anchor_handle = report
        .owner_messages
        .last()
        .map(|m| m.offset)
        .or_else(|| report.compaction_summary.as_ref().map(|s| s.offset))
        .or_else(|| report.footer.tool_operations.last().map(|op| op.offset));
    let keyword = footer_grep_keyword(&report.header.topic);

    println!("\n{}", "Drill-down".bold());
    if let Some(offset) = anchor_handle {
        println!("  claude-session-restore show {prefix} {}", format_handle(offset));
    }
    if let Some(record) = report.subagents_shown.first() {
        println!("  claude-session-restore agent {prefix} {}", record.short_id());
    } else if let Some(task_id) = report.background.still_running.first() {
        println!("  claude-session-restore agent {prefix} {task_id}");
    }
    println!("  claude-session-restore grep {prefix} {keyword}");
    if let Some(offset) = anchor_handle {
        println!("  claude-session-restore span {prefix} --around {} -n 10", format_handle(offset));
    }
    println!("  claude-session-restore messages {prefix} --kind owner --last 20");
}

/// A short, always-matchable literal word pulled from the session's own
/// topic — printed footer commands must run exactly as shown, so `grep`
/// needs a real word rather than a placeholder.
fn footer_grep_keyword(topic: &str) -> String {
    topic
        .split(|c: char| !c.is_alphanumeric())
        .find(|word| word.chars().count() >= 3)
        .map(str::to_string)
        .unwrap_or_else(|| "session".to_string())
}

/// Root event `type` tags modeled by [`SessionEvent`]. Used only by the
/// `--debug` histogram.
const KNOWN_ROOT_TYPES: &[&str] = &[
    "user",
    "assistant",
    "progress",
    "system",
    "file-history-snapshot",
    "queue-operation",
    "summary",
    "attachment",
    "custom-title",
    "ai-title",
    "last-prompt",
    "bridge-session",
    "atis-latch",
    "mode",
    "permission-mode",
    "agent-name",
    "file-history-delta",
];

fn count_unknown_root_types(lines: &[crate::io::RawLine]) -> Vec<(String, u64)> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for line in lines {
        let Ok(value) = serde_json::from_str::<JsonValue>(&line.text) else { continue };
        let Some(type_tag) = value.get("type").and_then(JsonValue::as_str) else { continue };
        if !KNOWN_ROOT_TYPES.contains(&type_tag) {
            *counts.entry(type_tag.to_string()).or_default() += 1;
        }
    }
    let mut counts: Vec<(String, u64)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(offset: u64, json: &str) -> OffsetEvent {
        OffsetEvent { offset, event: serde_json::from_str(json).expect("fixture event must parse") }
    }

    #[test]
    fn header_first_event_time_skips_marker_events_with_no_real_timestamp() {
        // Real transcripts often open with `custom-title`/`mode`/etc, none of
        // which carry an on-disk timestamp — `SessionEvent::timestamp()`
        // falls back to "now" for those, which must not leak into the
        // header's first/last-event-time display.
        let head = vec![
            event(0, r#"{"type":"custom-title","customTitle":"placeholder title","sessionId":"s"}"#),
            event(1, r#"{"type":"mode","mode":"normal","sessionId":"s"}"#),
            event(2, r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"real first event"}}"#),
        ];
        let tail = vec![event(
            3,
            r#"{"type":"user","uuid":"u2","sessionId":"s","timestamp":"2026-01-01T00:05:00Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"real last event"}}"#,
        )];

        let header = build_header("session-id", Path::new("session-id.jsonl"), 0, &head, &tail, 0);
        assert_eq!(
            header.first_event_time,
            Some(DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().with_timezone(&Utc))
        );
        assert_eq!(
            header.last_event_time,
            Some(DateTime::parse_from_rfc3339("2026-01-01T00:05:00Z").unwrap().with_timezone(&Utc))
        );
    }

    #[test]
    fn header_picks_up_entrypoint_and_version() {
        let tail = vec![event(
            0,
            r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"userType":"external","entrypoint":"sdk-cli","version":"2.1.280","cwd":"/work","message":{"role":"user","content":"placeholder"}}"#,
        )];
        let header = build_header("session-id", Path::new("session-id.jsonl"), 0, &[], &tail, 0);
        assert_eq!(header.entrypoint.as_deref(), Some("sdk-cli"));
        assert_eq!(header.version.as_deref(), Some("2.1.280"));
    }

    #[test]
    fn header_strips_windows_verbatim_path_prefix() {
        let header = build_header("session-id", Path::new(r"\\?\C:\Users\owner\session-id.jsonl"), 0, &[], &[], 0);
        assert_eq!(header.jsonl_path, r"C:\Users\owner\session-id.jsonl");
    }

    #[test]
    fn post_compaction_window_starts_after_the_last_boundary() {
        let events = vec![
            event(0, r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"before"}}"#),
            event(100, r#"{"type":"system","subtype":"compact_boundary","uuid":"boundary1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:01Z","isSidechain":false,"cwd":"/work","compactMetadata":{"trigger":"auto","preTokens":1000}}"#),
            event(200, r#"{"type":"user","uuid":"u2","sessionId":"s","timestamp":"2026-01-01T00:00:02Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"after"}}"#),
        ];
        let window = post_compaction_window(&events);
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].offset, 200);
    }

    #[test]
    fn compaction_stats_deduplicates_by_uuid_across_head_and_tail() {
        let boundary = event(0, r#"{"type":"system","subtype":"compact_boundary","uuid":"boundary1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:01Z","isSidechain":false,"cwd":"/work","compactMetadata":{"trigger":"auto","preTokens":1000}}"#);
        let head = vec![boundary.clone()];
        let tail = vec![boundary];
        let (count, last) = compaction_stats(&head, &tail);
        assert_eq!(count, 1);
        assert!(last.is_some());
    }

    #[test]
    fn find_compact_summary_reads_the_full_text_and_caps_the_excerpt() {
        let long_text = "x".repeat(COMPACTION_SUMMARY_CHAR_CAP + 50);
        let tail = vec![event(
            42,
            &format!(
                r#"{{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"userType":"external","isCompactSummary":true,"cwd":"/work","message":{{"role":"user","content":"{long_text}"}}}}"#
            ),
        )];
        let summary = find_compact_summary(&[], &tail).expect("compaction summary");
        assert_eq!(summary.offset, 42);
        assert_eq!(summary.total_chars, COMPACTION_SUMMARY_CHAR_CAP + 50);
        assert_eq!(summary.excerpt.chars().count(), COMPACTION_SUMMARY_CHAR_CAP);
    }

    #[test]
    fn find_compact_summary_none_when_absent() {
        let tail = vec![event(
            0,
            r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isSidechain":false,"userType":"external","cwd":"/work","message":{"role":"user","content":"just a normal prompt"}}"#,
        )];
        assert!(find_compact_summary(&[], &tail).is_none());
    }

    #[test]
    fn footer_grep_keyword_picks_a_real_word_from_the_topic() {
        assert_eq!(footer_grep_keyword("payments merchant onboarding"), "payments");
        assert_eq!(footer_grep_keyword("Empty session"), "Empty");
        assert_eq!(footer_grep_keyword(""), "session");
    }
}
