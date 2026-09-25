//! `list` — compact recent-session listing with an optional `--grep` filter
//! (spec B).

use crate::format::{format_size, local_time, single_line, truncate};
use crate::handle::format_handle;
use crate::human::{human_messages, scan_end_state, stuck_queue_items, StuckSummary};
use crate::io::{parse_events, read_head_lines, read_tail_lines, OffsetEvent};
use crate::topic::{detect_topic, first_human_prompts};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use claude_session_restore::transcript::events::SessionEvent;
use colored::Colorize;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// JSON report schema tag for `list --json`.
const SCHEMA_LIST: &str = "claude-session-restore-list-v2";

/// Tail byte budget per session for `list`'s preview — small, since only a
/// handful of recent items are shown per session and many sessions may be
/// scanned per invocation.
const LIST_TAIL_BYTES: u64 = 4 * 1024 * 1024;
/// Head byte budget used by the title/first-prompt/`--grep` scan.
const LIST_HEAD_BYTES: u64 = 1024 * 1024;
/// Default number of sessions shown.
pub const DEFAULT_LIMIT: usize = 30;
/// Default age window when no `--grep` filter narrows the search.
const DEFAULT_MAX_AGE_HOURS: u64 = 12;
/// Preview length for the first/last message lines.
const PREVIEW_CHARS: usize = 80;

pub struct ListArgs {
    pub limit: usize,
    pub include_archived: bool,
    pub max_age_hours: Option<u64>,
    pub all: bool,
    pub home: Option<PathBuf>,
    pub json: bool,
    pub grep: Option<String>,
}

struct Candidate {
    path: PathBuf,
    size: u64,
    modified: Option<DateTime<Utc>>,
    source: &'static str,
}

struct ListEntry {
    id: String,
    modified: Option<DateTime<Utc>>,
    size_bytes: u64,
    source: &'static str,
    topic: String,
    topic_source: String,
    first_prompt: Option<String>,
    last_message: Option<String>,
    last_message_delivery: Option<&'static str>,
    last_message_handle: Option<String>,
    warning: Option<String>,
}

#[derive(Serialize)]
struct ListEntryJson {
    id: String,
    modified: Option<String>,
    size_bytes: u64,
    source: &'static str,
    topic: String,
    topic_source: String,
    first_prompt: Option<String>,
    last_message: Option<String>,
    last_message_delivery: Option<&'static str>,
    last_message_handle: Option<String>,
    warning: Option<String>,
}

#[derive(Serialize)]
struct ListReportJson {
    schema: &'static str,
    sessions: Vec<ListEntryJson>,
}

pub fn run(args: ListArgs) -> Result<()> {
    let home = match &args.home {
        Some(path) => path.clone(),
        None => dirs::home_dir().context("Failed to get home directory")?.join(".claude"),
    };
    let projects_dir = home.join("projects");

    if !projects_dir.exists() {
        anyhow::bail!("Claude projects directory not found: {}", projects_dir.display());
    }

    let now = SystemTime::now();
    let cutoff_time = resolve_cutoff(now, args.all, args.max_age_hours, args.grep.is_some());

    let mut candidates: Vec<Candidate> = Vec::new();
    for entry in fs::read_dir(&projects_dir).with_context(|| {
        format!("Failed to read Claude projects directory: {}", projects_dir.display())
    })? {
        let entry = entry?;
        let project_path = entry.path();
        if !project_path.is_dir() {
            continue;
        }
        collect_top_level_sessions(&project_path, cutoff_time, "projects", &mut candidates)?;
    }
    if args.include_archived {
        let archive_dir = home.join("archive");
        if archive_dir.exists() {
            collect_top_level_sessions(&archive_dir, cutoff_time, "archive", &mut candidates)?;
        }
    }

    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.modified));

    let needle = args.grep.as_ref().map(|value| value.to_lowercase());
    let entries: Vec<ListEntry> = if let Some(needle) = &needle {
        let mut matched = Vec::new();
        for candidate in &candidates {
            let (entry, haystack) = build_entry(candidate)?;
            if haystack.to_lowercase().contains(needle.as_str()) {
                matched.push(entry);
            }
        }
        matched.truncate(args.limit);
        matched
    } else {
        candidates.truncate(args.limit);
        let mut built = Vec::with_capacity(candidates.len());
        for candidate in &candidates {
            built.push(build_entry(candidate)?.0);
        }
        built
    };

    if entries.is_empty() {
        print_empty(&args, &projects_dir);
        return Ok(());
    }

    if args.json {
        print_json(&entries);
        return Ok(());
    }

    print_human(&entries);
    Ok(())
}

fn resolve_cutoff(now: SystemTime, all: bool, max_age_hours: Option<u64>, has_grep: bool) -> SystemTime {
    if all {
        return SystemTime::UNIX_EPOCH;
    }
    let hours = match max_age_hours {
        Some(hours) => hours,
        None if has_grep => return SystemTime::UNIX_EPOCH,
        None => DEFAULT_MAX_AGE_HOURS,
    };
    let max_age = std::time::Duration::from_secs(hours * 3600);
    now.checked_sub(max_age).unwrap_or(SystemTime::UNIX_EPOCH)
}

fn collect_top_level_sessions(
    dir: &Path,
    cutoff_time: SystemTime,
    source: &'static str,
    sessions: &mut Vec<Candidate>,
) -> Result<()> {
    for entry in fs::read_dir(dir)
        .with_context(|| format!("Failed to read Claude session directory: {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(metadata) = fs::metadata(&path) else { continue };
        let Ok(modified_time) = metadata.modified() else { continue };
        if modified_time < cutoff_time {
            continue;
        }
        let modified = modified_time
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .and_then(|duration| DateTime::from_timestamp(duration.as_secs() as i64, 0));
        sessions.push(Candidate { path, size: metadata.len(), modified, source });
    }
    Ok(())
}

/// Build a display entry plus the lowercase-joined text `--grep` matches
/// against: title sources (`custom-title`/`ai-title`/`agent-name`),
/// `last-prompt`, and every human prompt found in the read bytes.
fn build_entry(candidate: &Candidate) -> Result<(ListEntry, String)> {
    let (tail_lines, _truncated) = read_tail_lines(&candidate.path, LIST_TAIL_BYTES)?;
    let tail_events = parse_events(&tail_lines);
    let head_lines = read_head_lines(&candidate.path, LIST_HEAD_BYTES)?;
    let head_events = parse_events(&head_lines);

    let (topic, topic_source) = detect_topic(&head_events, &tail_events);
    let first_prompt = first_human_prompts(&head_events, 1)
        .into_iter()
        .next()
        .or_else(|| first_human_prompts(&tail_events, 1).into_iter().next())
        .map(|(_, text)| text);

    let mut messages = human_messages(&tail_events);
    if messages.is_empty() {
        messages = human_messages(&head_events);
    }
    let last = messages.last();
    let last_message = last.map(|message| message.text.clone());
    let last_message_delivery = last.map(|message| message.delivery.label());
    let last_message_handle = last.map(|message| format_handle(message.offset));

    let stuck = stuck_queue_items(&tail_events);
    let end_state = scan_end_state(&tail_events);
    let warning = build_warning(&stuck, end_state.unanswered);

    let id = candidate.path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string();

    let mut grep_texts = vec![topic.clone()];
    grep_texts.extend(title_and_last_prompt_texts(&head_events));
    grep_texts.extend(title_and_last_prompt_texts(&tail_events));
    grep_texts.extend(human_messages(&head_events).into_iter().map(|m| m.text));
    grep_texts.extend(human_messages(&tail_events).into_iter().map(|m| m.text));
    let haystack = grep_texts.join("\n");

    let entry = ListEntry {
        id,
        modified: candidate.modified,
        size_bytes: candidate.size,
        source: candidate.source,
        topic,
        topic_source: topic_source.to_string(),
        first_prompt,
        last_message,
        last_message_delivery,
        last_message_handle,
        warning,
    };
    Ok((entry, haystack))
}

/// `custom-title`/`ai-title`/`agent-name`/`last-prompt` texts within
/// `events`, for `--grep` matching only (independent of the topic priority
/// chain, which stops at the first source it finds).
fn title_and_last_prompt_texts(events: &[OffsetEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|oe| match &oe.event {
            SessionEvent::CustomTitle(title) => Some(title.custom_title.clone()),
            SessionEvent::AiTitle(title) => Some(title.ai_title.clone()),
            SessionEvent::AgentName(name) => Some(name.agent_name.clone()),
            SessionEvent::LastPrompt(prompt) => Some(prompt.last_prompt.clone()),
            _ => None,
        })
        .collect()
}

fn build_warning(stuck: &StuckSummary, unanswered: bool) -> Option<String> {
    let mut parts = Vec::new();
    if !stuck.owner_items.is_empty() {
        parts.push(format!("{} queued message(s) never delivered", stuck.owner_items.len()));
    }
    if stuck.other_leftover_count > 0 {
        parts.push(format!("{} other queued item(s) (peer/task) never delivered", stuck.other_leftover_count));
    }
    if unanswered {
        parts.push("last message unanswered".to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn print_empty(args: &ListArgs, projects_dir: &Path) {
    println!("{}", "Recent Sessions:".bold().bright_cyan());
    println!();
    let window = if args.all || (args.grep.is_some() && args.max_age_hours.is_none()) {
        "any age".to_string()
    } else {
        let hours = args.max_age_hours.unwrap_or(DEFAULT_MAX_AGE_HOURS);
        format!("the last {hours}h")
    };
    if let Some(grep) = &args.grep {
        println!(
            "No Claude sessions found in {window} under {} matching --grep {grep:?}.",
            projects_dir.display()
        );
    } else {
        println!(
            "No Claude sessions found in {window} under {}. Try --max-age-hours <N> or --all.",
            projects_dir.display()
        );
    }
}

fn print_json(entries: &[ListEntry]) {
    let sessions = entries
        .iter()
        .map(|entry| ListEntryJson {
            id: entry.id.clone(),
            modified: entry.modified.map(|value| value.to_rfc3339()),
            size_bytes: entry.size_bytes,
            source: entry.source,
            topic: entry.topic.clone(),
            topic_source: entry.topic_source.clone(),
            first_prompt: entry.first_prompt.clone(),
            last_message: entry.last_message.clone(),
            last_message_delivery: entry.last_message_delivery,
            last_message_handle: entry.last_message_handle.clone(),
            warning: entry.warning.clone(),
        })
        .collect();
    let report = ListReportJson { schema: SCHEMA_LIST, sessions };
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        println!("{json}");
    }
}

fn print_human(entries: &[ListEntry]) {
    println!("{}", "Recent Sessions:".bold().bright_cyan());
    println!();

    for (index, entry) in entries.iter().enumerate() {
        println!("{}", format!("{}. {}", index + 1, entry.id).bright_yellow());

        let date = entry.modified.map_or_else(|| "?".to_string(), |value| local_time(&value).format("%b %d %H:%M").to_string());
        println!(
            "   {} | {} | [{}] | {}",
            date,
            format_size(entry.size_bytes),
            entry.source.dimmed(),
            truncate(&entry.topic, 80).bright_green()
        );

        if let Some(first) = &entry.first_prompt {
            println!("   first: «{}»", truncate(&single_line(first), PREVIEW_CHARS).dimmed());
        }
        if let Some(last) = &entry.last_message {
            let handle = entry.last_message_handle.as_deref().map(|h| format!("{h} ")).unwrap_or_default();
            println!("   last: {handle}«{}»", truncate(&single_line(last), PREVIEW_CHARS).dimmed());
        }
        if let Some(warning) = &entry.warning {
            println!("   {} {}", "\u{26a0}".yellow(), warning.yellow());
        }

        println!();
    }

    println!();
    println!("{}", "To load a session, use:".bold());
    for (index, entry) in entries.iter().enumerate() {
        println!("  {}. claude-session-restore load {}", index + 1, entry.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cutoff_defaults_to_12h_without_grep() {
        let now = SystemTime::now();
        let cutoff = resolve_cutoff(now, false, None, false);
        let expected = now.checked_sub(std::time::Duration::from_secs(12 * 3600)).unwrap();
        let delta = expected.duration_since(cutoff).unwrap_or_default();
        assert!(delta.as_secs() < 2, "cutoff should be ~12h back, delta={delta:?}");
    }

    #[test]
    fn resolve_cutoff_has_no_filter_with_grep_and_no_explicit_max_age() {
        let now = SystemTime::now();
        let cutoff = resolve_cutoff(now, false, None, true);
        assert_eq!(cutoff, SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn resolve_cutoff_honours_explicit_max_age_even_with_grep() {
        let now = SystemTime::now();
        let cutoff = resolve_cutoff(now, false, Some(3), true);
        let expected = now.checked_sub(std::time::Duration::from_secs(3 * 3600)).unwrap();
        let delta = expected.duration_since(cutoff).unwrap_or_default();
        assert!(delta.as_secs() < 2);
    }

    #[test]
    fn resolve_cutoff_all_ignores_everything() {
        let now = SystemTime::now();
        assert_eq!(resolve_cutoff(now, true, Some(1), false), SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn build_warning_combines_stuck_and_unanswered() {
        assert_eq!(build_warning(&StuckSummary::default(), false), None);
        let stuck = StuckSummary {
            owner_items: vec![crate::human::StuckQueueItem {
                offset: 0,
                content: "placeholder".to_string(),
                enqueued_at: Utc::now(),
            }],
            other_leftover_count: 2,
        };
        assert_eq!(
            build_warning(&stuck, true).as_deref(),
            Some("1 queued message(s) never delivered; 2 other queued item(s) (peer/task) never delivered; last message unanswered")
        );
    }
}
