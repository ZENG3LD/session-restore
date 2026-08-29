//! Session Summary CLI
//!
//! Quick summary tool for Claude Code session files.
//! Uses reverse parsing from end of file for efficiency.

#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_precision_loss)]

use anyhow::{Context, Result};
use chrono::DateTime;
use clap::{Parser, Subcommand};
use claude_session_types::events::{ProgressData, SessionEvent};
use colored::Colorize;
use regex::Regex;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

#[derive(Parser)]
#[command(name = "session-summary")]
#[command(about = "Quick summary of Claude Code session files", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List recent sessions with brief summaries (reads from end, stops after 2 key events)
    List {
        /// Number of recent sessions to show
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Only show projects directory (exclude archive)
        #[arg(long)]
        projects_only: bool,

        /// Maximum age in hours (filter by last modification time)
        #[arg(long, default_value = "12")]
        max_age_hours: u64,
    },
    /// Load full context from selected session (last segment + git hints)
    Load {
        /// Session JSONL path, exact UUID, or unique UUID prefix (at least 16 characters)
        session: String,

        /// Claude home containing projects/ and archive/ (defaults to ~/.claude)
        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::List { limit, projects_only, max_age_hours } => {
            list_sessions(limit, !projects_only, max_age_hours)?;
        }
        Commands::Load { session, home } => {
            let home = match home {
                Some(path) => path,
                None => dirs::home_dir()
                    .context("Failed to get home directory")?
                    .join(".claude"),
            };
            let path = resolve_session_path(&session, &home)?;
            load_session_context(&path)?;
        }
    }

    Ok(())
}

#[derive(Debug)]
struct SessionRoot {
    lexical: PathBuf,
    canonical: PathBuf,
}

/// Resolve a load argument without allowing it to escape the selected Claude home.
fn resolve_session_path(session: &str, home: &Path) -> Result<PathBuf> {
    let roots = session_roots(home)?;
    let raw_path = Path::new(session);

    if looks_like_jsonl_path(session, raw_path) {
        return validate_session_path(raw_path, &roots);
    }

    if !is_uuid(session) && !is_uuid_prefix(session) {
        anyhow::bail!(
            "Session must be an exact JSONL path, an exact UUID, or a UUID prefix of at least 16 characters"
        );
    }

    let needle = session.to_ascii_lowercase();
    let exact = is_uuid(session);
    let mut matches = Vec::new();

    for root in &roots {
        collect_session_files(&root.lexical, &mut matches)?;
    }

    matches.retain(|path| {
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            return false;
        };
        if !is_uuid(stem) {
            return false;
        }
        if exact {
            stem.eq_ignore_ascii_case(session)
        } else {
            stem.to_ascii_lowercase().starts_with(&needle)
        }
    });

    match matches.len() {
        0 => anyhow::bail!("No Claude session matches identifier: {session}"),
        1 => validate_session_path(&matches[0], &roots),
        count => anyhow::bail!("Session identifier is ambiguous ({count} matches): {session}"),
    }
}

fn session_roots(home: &Path) -> Result<Vec<SessionRoot>> {
    let home = absolute_lexical(home)?;
    let mut roots = Vec::new();

    for directory in ["projects", "archive"] {
        let lexical = home.join(directory);
        let metadata = match fs::symlink_metadata(&lexical) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| {
                format!("Failed to inspect Claude session root: {}", lexical.display())
            }),
        };

        if is_symlink_or_reparse(&metadata) {
            anyhow::bail!("Claude session root must not be a symlink: {}", lexical.display());
        }
        if !metadata.is_dir() {
            anyhow::bail!("Claude session root is not a directory: {}", lexical.display());
        }

        let canonical = fs::canonicalize(&lexical).with_context(|| {
            format!("Failed to canonicalize Claude session root: {}", lexical.display())
        })?;
        roots.push(SessionRoot { lexical, canonical });
    }

    if roots.is_empty() {
        anyhow::bail!(
            "Claude session roots were not found under: {}",
            home.display()
        );
    }

    Ok(roots)
}

fn validate_session_path(path: &Path, roots: &[SessionRoot]) -> Result<PathBuf> {
    if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
        anyhow::bail!("Session path must name a .jsonl file: {}", path.display());
    }

    let lexical = absolute_lexical(path)?;
    let canonical = fs::canonicalize(&lexical)
        .with_context(|| format!("Session file not found: {}", lexical.display()))?;

    let root = roots.iter().find(|root| {
        lexical.starts_with(&root.lexical) && canonical.starts_with(&root.canonical)
    });
    let Some(root) = root else {
        anyhow::bail!(
            "Session path is outside the configured projects/archive roots: {}",
            lexical.display()
        );
    };

    reject_symlink_components(&lexical, &root.lexical)?;

    let metadata = fs::symlink_metadata(&lexical)
        .with_context(|| format!("Failed to inspect session file: {}", lexical.display()))?;
    if is_symlink_or_reparse(&metadata) {
        anyhow::bail!("Session path must not be a symlink: {}", lexical.display());
    }
    if !metadata.is_file() {
        anyhow::bail!("Session path is not a regular file: {}", lexical.display());
    }

    Ok(canonical)
}

fn reject_symlink_components(path: &Path, root: &Path) -> Result<()> {
    let relative = path.strip_prefix(root).context("Session path is outside its root")?;
    let mut current = root.to_path_buf();

    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("Failed to inspect session path: {}", current.display()))?;
        if is_symlink_or_reparse(&metadata) {
            anyhow::bail!("Session path must not contain symlinks: {}", current.display());
        }
    }

    Ok(())
}

fn is_symlink_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return has_windows_reparse_attribute(metadata.file_attributes());
    }

    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
fn has_windows_reparse_attribute(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn collect_session_files(root: &Path, sessions: &mut Vec<PathBuf>) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];

    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).with_context(|| {
            format!("Failed to read Claude session directory: {}", directory.display())
        })? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).with_context(|| {
                format!("Failed to inspect Claude session entry: {}", path.display())
            })?;

            if is_symlink_or_reparse(&metadata) {
                if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                    sessions.push(path);
                }
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            {
                sessions.push(path);
            }
        }
    }

    Ok(())
}

fn looks_like_jsonl_path(session: &str, path: &Path) -> bool {
    path.is_absolute()
        || path.extension().and_then(|value| value.to_str()) == Some("jsonl")
        || session.contains('/')
        || session.contains('\\')
        || session.starts_with('.')
}

fn is_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }

    value.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

fn is_uuid_prefix(value: &str) -> bool {
    if value.len() < 16 || value.len() >= 36 {
        return false;
    }

    value.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("Failed to get current directory")?
            .join(path)
    };
    let mut normalized = PathBuf::new();

    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    anyhow::bail!("Path escapes its filesystem root: {}", path.display());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }

    Ok(normalized)
}

/// List recent sessions from ~/.claude/projects/
fn list_sessions(limit: usize, include_archived: bool, max_age_hours: u64) -> Result<()> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    let projects_dir = home.join(".claude").join("projects");

    if !projects_dir.exists() {
        anyhow::bail!("Claude projects directory not found: {}", projects_dir.display());
    }

    // Calculate cutoff time (now - max_age_hours)
    let now = SystemTime::now();
    let max_age = std::time::Duration::from_secs(max_age_hours * 3600);
    let cutoff_time = now.checked_sub(max_age).unwrap_or(SystemTime::UNIX_EPOCH);

    // Find all session files (path, size, modified, source)
    let mut sessions: Vec<(PathBuf, u64, Option<DateTime<chrono::Utc>>, &str)> = Vec::new();

    for entry in fs::read_dir(&projects_dir)? {
        let entry = entry?;
        let project_path = entry.path();

        if !project_path.is_dir() {
            continue;
        }

        for session_entry in fs::read_dir(&project_path)? {
            let session_entry = session_entry?;
            let path = session_entry.path();

            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            if path.to_str().unwrap_or("").contains("subagents") {
                continue;
            }

            if let Ok(metadata) = fs::metadata(&path) {
                // Filter by modification time
                if let Ok(modified_time) = metadata.modified() {
                    if modified_time < cutoff_time {
                        continue; // Skip sessions older than max_age_hours
                    }
                }

                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|st| {
                        st.duration_since(SystemTime::UNIX_EPOCH)
                            .ok()
                            .and_then(|d| DateTime::from_timestamp(d.as_secs() as i64, 0))
                    });

                sessions.push((path, metadata.len(), modified, "projects"));
            }
        }
    }

    // Check archive if requested
    if include_archived {
        let archive_dir = home.join(".claude").join("archive");
        if archive_dir.exists() {
            for entry in fs::read_dir(&archive_dir)? {
                let entry = entry?;
                let path = entry.path();

                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }

                if let Ok(metadata) = fs::metadata(&path) {
                    // Filter by modification time
                    if let Ok(modified_time) = metadata.modified() {
                        if modified_time < cutoff_time {
                            continue; // Skip sessions older than max_age_hours
                        }
                    }

                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|st| {
                            st.duration_since(SystemTime::UNIX_EPOCH)
                                .ok()
                                .and_then(|d| DateTime::from_timestamp(d.as_secs() as i64, 0))
                        });

                    sessions.push((path, metadata.len(), modified, "archive"));
                }
            }
        }
    }

    // Sort by modification time (newest first)
    sessions.sort_by(|a, b| b.2.cmp(&a.2));
    sessions.truncate(limit);

    println!("{}", "Recent Sessions:".bold().bright_cyan());
    println!();

    for (i, (path, size, modified, source)) in sessions.iter().enumerate() {
        // Extract brief context (last 2 events from end)
        let brief = extract_brief_context(path).unwrap_or_default();

        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        println!("{}", format!("{}. {}", i + 1, session_id).bright_yellow());

        if let Some(mod_time) = modified {
            print!("   {} | ", mod_time.format("%b %d %H:%M"));
        }
        print!("{} | ", format_size(*size));
        print!("[{}] | ", source.dimmed());
        println!("{}", brief.topic.bright_green());

        // Show all available context
        if !brief.last_tasks.is_empty() {
            let tasks_preview: Vec<String> = brief.last_tasks
                .iter()
                .map(|t| truncate(t, 60))
                .collect();
            println!("   📋 Tasks: {}", tasks_preview.join(" → ").dimmed());
        }

        if !brief.user_messages.is_empty() {
            let msg_preview: Vec<String> = brief.user_messages
                .iter()
                .take(2)
                .map(|m| truncate(m, 50))
                .collect();
            println!("   💬 User: {}", msg_preview.join(" → ").dimmed());
        }

        if !brief.tool_operations.is_empty() {
            println!("   🔧 Tools: {}", brief.tool_operations.join(", ").dimmed());
        }

        if !brief.bash_activities.is_empty() {
            println!("   ⚙️  Bash: {}", brief.bash_activities.join("; ").dimmed());
        }

        if !brief.web_queries.is_empty() {
            println!("   🔍 Search: {}", brief.web_queries.join(", ").dimmed());
        }

        println!();
    }

    // Print load commands for easy copy-paste
    println!();
    println!("{}", "To load a session, use:".bold());
    for (i, (path, _, _, _)) in sessions.iter().enumerate() {
        println!("  {}. session-summary.exe load \"{}\"", i + 1, path.display());
    }

    Ok(())
}

/// Load full context from selected session
fn load_session_context(path: &Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("Session file not found: {}", path.display());
    }

    let metadata = fs::metadata(path)?;
    let size_bytes = metadata.len();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|st| {
            st.duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .and_then(|d| DateTime::from_timestamp(d.as_secs() as i64, 0))
        });

    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // Extract last segment (expanded context)
    let context = extract_last_segment_context(path)?;

    println!("{}", "═══════════════════════════════════════".bright_cyan());
    println!("{} {}", "Session:".bold(), session_id.bright_yellow());
    println!("{}", "═══════════════════════════════════════".bright_cyan());

    if let Some(mod_time) = modified {
        println!("{} {}", "Date:".bold(), mod_time.format("%Y-%m-%d %H:%M:%S"));
    }

    println!("{} {}", "Size:".bold(), format_size(size_bytes));
    println!("{} {}", "Topic:".bold(), context.topic.bright_green());

    if !context.agent_tasks.is_empty() {
        println!("\n{} 📋 {}", "Agent Tasks".bold(), format!("({} tasks)", context.agent_tasks.len()).dimmed());
        for (i, task) in context.agent_tasks.iter().take(10).enumerate() {
            println!("  {}. {}", i + 1, truncate(task, 200).bright_white());
        }
        if context.agent_tasks.len() > 10 {
            println!("  {} ({} more)", "...".dimmed(), context.agent_tasks.len() - 10);
        }
    }

    if !context.user_messages.is_empty() {
        println!("\n{} 💬 {}", "User Messages".bold(), format!("({} messages)", context.user_messages.len()).dimmed());
        for (i, msg) in context.user_messages.iter().take(10).enumerate() {
            println!("  {}. {}", i + 1, truncate(msg, 150).bright_white());
        }
        if context.user_messages.len() > 10 {
            println!("  {} ({} more)", "...".dimmed(), context.user_messages.len() - 10);
        }
    }

    if !context.tool_operations.is_empty() {
        println!("\n{} 🔧 {}", "Tool Operations".bold(), format!("({} operations)", context.tool_operations.len()).dimmed());
        for (i, tool) in context.tool_operations.iter().take(15).enumerate() {
            println!("  {}. {}", i + 1, tool.bright_white());
        }
        if context.tool_operations.len() > 15 {
            println!("  {} ({} more)", "...".dimmed(), context.tool_operations.len() - 15);
        }
    }

    if !context.bash_activities.is_empty() {
        println!("\n{} ⚙️  {}", "Bash Activities".bold(), format!("({} commands)", context.bash_activities.len()).dimmed());
        for (i, cmd) in context.bash_activities.iter().take(5).enumerate() {
            let first_line = cmd.lines().next().unwrap_or("");
            println!("  {}. {}", i + 1, truncate(first_line, 150).bright_white());
        }
        if context.bash_activities.len() > 5 {
            println!("  {} ({} more)", "...".dimmed(), context.bash_activities.len() - 5);
        }
    }

    if !context.web_queries.is_empty() {
        println!("\n{} 🔍 {}", "Web Searches".bold(), format!("({} queries)", context.web_queries.len()).dimmed());
        for (i, query) in context.web_queries.iter().take(10).enumerate() {
            println!("  {}. {}", i + 1, query.bright_white());
        }
        if context.web_queries.len() > 10 {
            println!("  {} ({} more)", "...".dimmed(), context.web_queries.len() - 10);
        }
    }

    if !context.files.is_empty() {
        println!("\n{} 📁 {}", "Files Modified".bold(), format!("({} files)", context.files.len()).dimmed());
        for (i, file) in context.files.iter().take(10).enumerate() {
            println!("  {}. {}", i + 1, shorten_path(file).bright_white());
        }
        if context.files.len() > 10 {
            println!("  {} ({} more files)", "...".dimmed(), context.files.len() - 10);
        }
    }

    if let Some(ref branch) = context.git_branch {
        println!("\n{} {}", "Git Branch:".bold(), branch.bright_cyan());
    }

    if !context.commit_hints.is_empty() {
        println!("\n{} 🔎", "Git Commit Hints (for git log search):".bold());
        for hint in &context.commit_hints {
            println!("  - {}", hint.dimmed());
        }
    }

    println!();

    Ok(())
}

// ============================================================================
// Context Extraction
// ============================================================================

#[derive(Debug, Clone, Default)]
struct BriefContext {
    topic: String,
    last_tasks: Vec<String>,
    user_messages: Vec<String>,
    tool_operations: Vec<String>,
    bash_activities: Vec<String>,
    web_queries: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct FullContext {
    topic: String,
    agent_tasks: Vec<String>,
    user_messages: Vec<String>,
    tool_operations: Vec<String>,
    bash_activities: Vec<String>,
    web_queries: Vec<String>,
    files: HashSet<String>,
    git_branch: Option<String>,
    commit_hints: Vec<String>,
}

/// Extract brief context by reading last N lines and finding 2 key events
fn extract_brief_context(path: &Path) -> Result<BriefContext> {
    // Use tail to read last 50k lines (faster than reading whole file)
    let output = Command::new("tail")
        .arg("-n")
        .arg("50000")
        .arg(path)
        .output()?;

    let lines = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = lines.lines().collect();

    let mut last_tasks = Vec::new();
    let mut user_messages = Vec::new();
    let mut tool_uses: Vec<(String, Vec<String>)> = Vec::new();
    let mut bash_commands = Vec::new();
    let mut web_queries = Vec::new();
    let mut edited_files: Vec<String> = Vec::new();

    // Parse from end to beginning
    for line in lines.iter().rev() {
        if let Ok(event) = serde_json::from_str::<SessionEvent>(line) {
            match event {
                SessionEvent::Progress(progress) => {
                    match &progress.data {
                        ProgressData::AgentProgress(agent) => {
                            if last_tasks.len() < 5 {
                                last_tasks.push(agent.prompt.clone());
                            }
                        }
                        ProgressData::BashProgress(bash) => {
                            if bash_commands.len() < 3 && !bash.full_output.is_empty() {
                                let output_lower = bash.full_output.to_lowercase();
                                if output_lower.contains("cargo build")
                                    || output_lower.contains("cargo check")
                                    || output_lower.contains("npm install")
                                    || output_lower.contains("git commit")
                                    || output_lower.contains("pytest")
                                    || output_lower.contains("compiling")
                                {
                                    bash_commands.push(bash.full_output.clone());
                                }
                            }
                        }
                        ProgressData::QueryUpdate(query) => {
                            if web_queries.len() < 3 {
                                web_queries.push(query.query.clone());
                            }
                        }
                        _ => {}
                    }
                }
                SessionEvent::User(user) => {
                    if user_messages.len() < 5 {
                        if let Some(text) = user.extract_text_content() {
                            user_messages.push(text);
                        }
                    }
                }
                SessionEvent::Assistant(assistant) => {
                    if tool_uses.len() < 5 {
                        let tools = assistant.extract_tool_names();
                        let paths = assistant.extract_file_paths();
                        if !tools.is_empty() {
                            for tool in &tools {
                                tool_uses.push((tool.clone(), paths.clone()));
                            }
                        }
                    }
                }
                SessionEvent::FileSnapshot(snapshot) => {
                    if edited_files.is_empty() {
                        edited_files = snapshot.snapshot.tracked_file_backups.keys().cloned().collect();
                    }
                }
                _ => {}
            }

            // Stop only when we collected enough from all vectors
            // Focus on tasks (slowest vector) - need at least 3 to get 2 different ones
            // Also collect from other vectors in parallel
            if last_tasks.len() >= 3
                && user_messages.len() >= 3
                && tool_uses.len() >= 3
            {
                break;
            }
        }
    }

    // Reverse to get chronological order
    last_tasks.reverse();
    user_messages.reverse();
    tool_uses.reverse();
    bash_commands.reverse();
    web_queries.reverse();

    // Format tool operations for display
    let tool_operations: Vec<String> = tool_uses
        .iter()
        .map(|(tool, paths)| {
            if paths.is_empty() {
                tool.clone()
            } else {
                format!("{} ({})", tool, paths.join(", "))
            }
        })
        .collect();

    // Format bash activities
    let bash_activities: Vec<String> = bash_commands
        .iter()
        .map(|cmd| {
            let first_line = cmd.lines().next().unwrap_or("").trim();
            truncate(first_line, 100)
        })
        .collect();

    // Simple topic (just to show something in the list)
    let topic = if !last_tasks.is_empty() {
        "Agent tasks"
    } else if !user_messages.is_empty() {
        "User session"
    } else if !tool_operations.is_empty() {
        "Tool usage"
    } else {
        "Empty session"
    }
    .to_string();

    Ok(BriefContext {
        topic,
        last_tasks,
        user_messages: user_messages.into_iter().take(5).collect(),
        tool_operations: tool_operations.into_iter().take(5).collect(),
        bash_activities: bash_activities.into_iter().take(3).collect(),
        web_queries: web_queries.into_iter().take(3).collect(),
    })
}

/// Extract last segment context (compact boundary to end)
fn extract_last_segment_context(path: &Path) -> Result<FullContext> {
    // Use tail to read last 100k lines
    let output = Command::new("tail")
        .arg("-n")
        .arg("100000")
        .arg(path)
        .output()?;

    let lines = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = lines.lines().collect();

    let mut agent_tasks = Vec::new();
    let mut user_messages = Vec::new();
    let mut tool_operations: Vec<String> = Vec::new();
    let mut bash_activities = Vec::new();
    let mut web_queries = Vec::new();
    let mut files = HashSet::new();
    let mut git_branch = None;
    let mut commit_hints = HashSet::new();

    // Find last compact boundary
    let mut last_boundary_idx = None;
    for (i, line) in lines.iter().enumerate().rev() {
        if let Ok(SessionEvent::System(sys)) = serde_json::from_str::<SessionEvent>(line) {
            if sys.is_compact_boundary() {
                last_boundary_idx = Some(i);
                break;
            }
        }
    }

    // Parse from last boundary (or beginning if no boundary found) to end
    let start_idx = last_boundary_idx.unwrap_or(0);

    for line in &lines[start_idx..] {
        if let Ok(event) = serde_json::from_str::<SessionEvent>(line) {
            match event {
                SessionEvent::Progress(progress) => {
                    match &progress.data {
                        ProgressData::AgentProgress(agent) => {
                            agent_tasks.push(agent.prompt.clone());
                            extract_commit_hints(&agent.prompt, &mut commit_hints);
                        }
                        ProgressData::BashProgress(bash) => {
                            if !bash.full_output.is_empty() {
                                let output_lower = bash.full_output.to_lowercase();
                                if output_lower.contains("cargo build")
                                    || output_lower.contains("cargo check")
                                    || output_lower.contains("npm install")
                                    || output_lower.contains("git commit")
                                    || output_lower.contains("pytest")
                                    || output_lower.contains("compiling")
                                {
                                    bash_activities.push(bash.full_output.clone());
                                }
                            }
                        }
                        ProgressData::QueryUpdate(query) => {
                            web_queries.push(query.query.clone());
                        }
                        _ => {}
                    }
                }

                SessionEvent::FileSnapshot(snapshot) => {
                    for path in snapshot.snapshot.tracked_file_backups.keys() {
                        let cleaned = path.replace("\\\\", "/");
                        files.insert(cleaned);
                    }
                }

                SessionEvent::User(user) => {
                    if let Some(ref branch) = user.metadata.git_branch {
                        git_branch = Some(branch.clone());
                    }
                    if let Some(text) = user.extract_text_content() {
                        user_messages.push(text);
                    }
                }

                SessionEvent::Assistant(assistant) => {
                    let tools = assistant.extract_tool_names();
                    let paths = assistant.extract_file_paths();
                    for tool in &tools {
                        if paths.is_empty() {
                            tool_operations.push(tool.clone());
                        } else {
                            tool_operations.push(format!("{} ({})", tool, paths.join(", ")));
                        }
                    }
                }

                _ => {}
            }
        }
    }

    // Simple topic label
    let topic = if !agent_tasks.is_empty() {
        "Agent tasks"
    } else if !user_messages.is_empty() {
        "User session"
    } else {
        "Session data"
    }
    .to_string();

    Ok(FullContext {
        topic,
        agent_tasks,
        user_messages,
        tool_operations,
        bash_activities,
        web_queries,
        files,
        git_branch,
        commit_hints: commit_hints.into_iter().collect(),
    })
}

/// Extract git commit hints from agent prompts
fn extract_commit_hints(prompt: &str, hints: &mut HashSet<String>) {
    // Look for common commit message patterns
    let patterns = [
        r"feat\(([^)]+)\)",      // feat(core)
        r"fix\(([^)]+)\)",       // fix(connectors)
        r"refactor\(([^)]+)\)",  // refactor(ui)
        r"chore\(([^)]+)\)",     // chore(deps)
        r"test\(([^)]+)\)",      // test(api)
    ];

    for pattern in &patterns {
        if let Ok(re) = Regex::new(pattern) {
            for cap in re.captures_iter(prompt) {
                if let Some(scope) = cap.get(1) {
                    hints.insert(scope.as_str().to_string());
                }
            }
        }
    }

    // Also look for direct mentions of modules
    for word in prompt.split_whitespace() {
        if word.contains("v5/") || word.contains("connectors/") || word.contains("ui/") {
            hints.insert(word.to_string());
        }
    }
}

// NOTE: Topic inference functions removed - we now collect raw data and let Claude analyze it

/// Shorten path for display
fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > 2 {
        format!(".../{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        path.to_string()
    }
}

/// Truncate string to max length (UTF-8 safe)
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        let mut boundary = max_len;
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!("{}...", &s[..boundary])
    } else {
        s.to_string()
    }
}

/// Format file size in human-readable format
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHome {
        path: PathBuf,
    }

    impl TestHome {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "session-summary-tests-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(path.join("projects").join("project-a"))
                .expect("create projects root");
            fs::create_dir_all(path.join("archive")).expect("create archive root");
            Self { path }
        }

        fn session(&self, relative: &str, id: &str) -> PathBuf {
            let directory = self.path.join(relative);
            fs::create_dir_all(&directory).expect("create session directory");
            let path = directory.join(format!("{id}.jsonl"));
            fs::write(&path, "{}\n").expect("write session fixture");
            path
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn parses_home_after_load_identifier() {
        let cli = Cli::try_parse_from([
            "session-summary",
            "load",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "--home",
            "C:\\fixture\\.claude",
        ])
        .expect("parse load command");

        let Commands::Load { session, home } = cli.command else {
            panic!("expected load command");
        };
        assert_eq!(session, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        assert_eq!(home, Some(PathBuf::from("C:\\fixture\\.claude")));
    }

    #[cfg(windows)]
    #[test]
    fn recognizes_windows_reparse_attribute() {
        assert!(has_windows_reparse_attribute(0x400));
        assert!(has_windows_reparse_attribute(0x420));
        assert!(!has_windows_reparse_attribute(0x20));
    }

    #[test]
    fn resolves_existing_path_exact_uuid_and_unique_prefix() {
        let home = TestHome::new();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let path = home.session("projects/project-a", id);
        let expected = fs::canonicalize(&path).expect("canonical fixture path");

        assert_eq!(
            resolve_session_path(path.to_str().expect("UTF-8 path"), &home.path)
                .expect("resolve exact path"),
            expected
        );
        assert_eq!(
            resolve_session_path(id, &home.path).expect("resolve exact UUID"),
            expected
        );
        assert_eq!(
            resolve_session_path("aaaaaaaa-aaaa-4aaa-8aaa-a", &home.path)
                .expect("resolve unique prefix"),
            expected
        );
    }

    #[test]
    fn rejects_short_and_ambiguous_prefixes() {
        let home = TestHome::new();
        home.session(
            "projects/project-a",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        );
        home.session("archive", "aaaaaaaa-aaaa-4aaa-8aaa-bbbbbbbbbbbb");

        let short = resolve_session_path("aaaaaaaa-aaaa-4", &home.path)
            .expect_err("short prefix must fail")
            .to_string();
        assert!(short.contains("at least 16"), "unexpected error: {short}");

        let ambiguous = resolve_session_path("aaaaaaaa-aaaa-4a", &home.path)
            .expect_err("ambiguous prefix must fail")
            .to_string();
        assert!(ambiguous.contains("ambiguous"), "unexpected error: {ambiguous}");
    }

    #[test]
    fn rejects_outside_and_nonregular_paths() {
        let home = TestHome::new();
        let outside = home.path.with_extension("outside.jsonl");
        fs::write(&outside, "{}\n").expect("write outside fixture");
        let nonregular = home.path.join("projects").join("directory.jsonl");
        fs::create_dir(&nonregular).expect("create nonregular fixture");

        let outside_error = resolve_session_path(
            outside.to_str().expect("UTF-8 outside path"),
            &home.path,
        )
        .expect_err("outside path must fail")
        .to_string();
        assert!(
            outside_error.contains("outside"),
            "unexpected error: {outside_error}"
        );

        let nonregular_error = resolve_session_path(
            nonregular.to_str().expect("UTF-8 nonregular path"),
            &home.path,
        )
        .expect_err("nonregular path must fail")
        .to_string();
        assert!(
            nonregular_error.contains("not a regular file"),
            "unexpected error: {nonregular_error}"
        );

        fs::remove_file(outside).expect("remove outside fixture");
    }

    #[test]
    fn rejects_symlink_session_path() {
        let home = TestHome::new();
        let target = home.session(
            "projects/project-a",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        );
        let link = home
            .path
            .join("projects")
            .join("project-a")
            .join("cccccccc-cccc-4ccc-8ccc-cccccccccccc.jsonl");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).expect("create session symlink");

        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_file(&target, &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(1314)
            {
                return;
            }
            panic!("create session symlink: {error}");
        }

        let error = resolve_session_path(link.to_str().expect("UTF-8 link path"), &home.path)
            .expect_err("symlink must fail")
            .to_string();
        assert!(
            error.contains("symlink"),
            "unexpected symlink error: {error}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_junction_component() {
        let home = TestHome::new();
        let id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        let target = home.path.join("projects").join("real-project");
        fs::create_dir_all(&target).expect("create junction target");
        fs::write(target.join(format!("{id}.jsonl")), "{}\n")
            .expect("write junction session fixture");
        let junction = home.path.join("projects").join("linked-project");
        let output = Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .expect("invoke mklink");
        assert!(
            output.status.success(),
            "mklink failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let linked_session = junction.join(format!("{id}.jsonl"));
        let error = resolve_session_path(
            linked_session.to_str().expect("UTF-8 junction path"),
            &home.path,
        )
        .expect_err("junction component must fail")
        .to_string();
        assert!(
            error.contains("symlink"),
            "unexpected junction error: {error}"
        );

        fs::remove_dir(&junction).expect("remove junction fixture");
    }
}
