//! `agents <session>` — every subagent the session ever launched (a
//! full-file scan; `load`'s "Subagents" section shows only the 10 latest,
//! from a bounded window).

use crate::format::{local_time, truncate};
use crate::handle::format_handle;
use crate::subagents::{self, BackgroundBashTasks, SubagentRecord};
use anyhow::Result;
use colored::Colorize;
use serde::Serialize;
use std::path::Path;

pub fn run(path: &Path, session_dir: &Path, json: bool) -> Result<()> {
    let (subagents, background) = subagents::scan_full(path, session_dir)?;
    if json {
        print_json(&subagents, &background);
    } else {
        print_human(&subagents, &background);
    }
    Ok(())
}

fn print_human(records: &[SubagentRecord], background: &BackgroundBashTasks) {
    if records.is_empty() && background.launched == 0 {
        println!("No subagents or background tasks launched in this session.");
        return;
    }
    for record in records {
        let agent_type = record.agent_type.as_deref().unwrap_or("?");
        let description = record.description.as_deref().unwrap_or("(no description)");
        println!(
            "{} {} [{}] {} \u{2014} launched {}, {}",
            format_handle(record.offset),
            record.short_id().bright_yellow(),
            agent_type,
            description,
            local_time(&record.launched_at).format("%Y-%m-%d %H:%M:%S"),
            record.status.label()
        );
        if let Some(excerpt) = &record.excerpt {
            println!("  {}", truncate(excerpt, 300).dimmed());
        }
    }
    if background.launched > 0 {
        let ids = if background.still_running.is_empty() {
            String::new()
        } else {
            format!(" ({})", background.still_running.join(", "))
        };
        println!(
            "\nBackground Bash tasks: {} launched, {} still running{ids}",
            background.launched,
            background.still_running.len()
        );
    }
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
struct AgentsReportJson {
    subagents: Vec<SubagentJson>,
    background_bash: BackgroundBashJson,
}

fn print_json(records: &[SubagentRecord], background: &BackgroundBashTasks) {
    let report = AgentsReportJson {
        subagents: records
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
        background_bash: BackgroundBashJson { launched: background.launched, still_running: background.still_running.clone() },
    };
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        println!("{json}");
    }
}
