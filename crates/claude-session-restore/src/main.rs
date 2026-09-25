//! Claude Session Restore CLI
//!
//! Restores bounded working context from a Claude Code session transcript in
//! two waves (see `docs/session-restore/plans/2026-09-25-claude-restore-two-wave-recon.md`):
//!
//! - **Wave 1** — `list` finds sessions on disk, `load` prints a bounded
//!   (~180-line) digest of verbatim quotes from a chosen session, with every
//!   item carrying an `@o<byte-offset>` handle (see [`handle`]).
//! - **Wave 2** — `show`, `messages`, `agents`, `agent`, `grep`, and `span`
//!   take a session plus handles/ids from wave 1 and print full verbatim
//!   material, streaming the whole file when that's what the question needs.
//!
//! Nothing here is an agent-written summary — every line is a verbatim
//! quote, a count, or a pointer.
//!
//! # Bounded, not schema-strict
//!
//! The Claude Code transcript format keeps growing new root event types and
//! new field shapes on existing ones. This parser treats that as normal: an
//! unrecognized root type is skipped silently (never a parse failure for the
//! rest of the line's siblings — see [`claude_session_restore::transcript::events::SessionEvent::Unknown`]),
//! and a handful of known fields (`message.content`, `toolUseResult`) accept
//! more than one wire shape rather than dropping the whole event.
//!
//! # Reads are byte-budgeted, not full-file scans — except where the
//! question demands one
//!
//! `list` and `load` read a bounded window from each end of the file (see
//! [`io::read_tail_lines`] and [`io::read_head_lines`]) — cost is
//! proportional to the window, not to file size, so a multi-gigabyte
//! transcript loads in well under a second. Wave-2 commands that must search
//! the whole file (`messages`, `grep`, `agents`) stream it forward in a
//! single buffered pass (see [`io::scan_lines`]) rather than holding it in
//! memory; `show` and `span` seek straight to a handle's byte offset instead.

mod agent_cmd;
mod agents_cmd;
mod commits;
mod digest;
mod format;
mod grep_cmd;
mod handle;
mod human;
mod io;
mod list_cmd;
mod load_cmd;
mod messages_cmd;
mod open_work;
mod paths;
mod render;
mod show_cmd;
mod span_cmd;
mod subagents;
mod topic;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "claude-session-restore", version)]
#[command(about = "Restore bounded context from Claude Code session transcripts", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List recent sessions with a compact preview (reads a bounded byte
    /// window from each file, not the whole file)
    List {
        /// Number of sessions to show, after filtering
        #[arg(short, long, default_value_t = list_cmd::DEFAULT_LIMIT)]
        limit: usize,

        /// Only show projects directory (exclude archive)
        #[arg(long)]
        projects_only: bool,

        /// Maximum age in hours. Defaults to 12h; when `--grep` is given
        /// with no explicit value here, the search covers every age.
        #[arg(long)]
        max_age_hours: Option<u64>,

        /// Ignore `--max-age-hours` and list the newest sessions regardless of age
        #[arg(long)]
        all: bool,

        /// Claude home containing projects/ and archive/ (defaults to ~/.claude)
        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,

        /// Emit machine-readable JSON instead of the human-readable listing
        #[arg(long)]
        json: bool,

        /// Case-insensitive substring match against the session title
        /// (custom-title/ai-title/agent-name), last-prompt, and every human
        /// prompt found in the bytes already read (head + tail windows)
        #[arg(long)]
        grep: Option<String>,
    },
    /// Load the full restore digest for a selected session (wave 1)
    Load {
        /// Session JSONL path, exact UUID, or unique UUID prefix (at least 8 characters)
        session: String,

        /// Claude home containing projects/ and archive/ (defaults to ~/.claude)
        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,

        /// Emit machine-readable JSON instead of the human-readable report
        #[arg(long)]
        json: bool,

        /// Print a count of unrecognized root event types seen in the scanned
        /// window to stderr. Diagnostic only — never changes stdout.
        #[arg(long)]
        debug: bool,
    },
    /// Print the full, verbatim record for one or more handles (wave 2)
    Show {
        session: String,
        /// One or more `@o<byte-offset>` handles, from `load` or any other command
        #[arg(required = true, num_args = 1..)]
        handles: Vec<String>,

        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,

        /// Cap on characters shown per text piece (a compaction summary is
        /// always shown in full regardless of this cap)
        #[arg(long, default_value_t = show_cmd::DEFAULT_MAX_CHARS)]
        max_chars: usize,

        #[arg(long)]
        json: bool,
    },
    /// Full-file, chronological scan of every classified message (wave 2)
    Messages {
        session: String,

        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,

        /// Comma-separated: owner, peer, agent, notification, all (default: all)
        #[arg(long)]
        kind: Option<String>,

        /// RFC-3339 timestamp lower bound
        #[arg(long)]
        since: Option<String>,

        /// RFC-3339 timestamp upper bound
        #[arg(long)]
        until: Option<String>,

        /// Keep only the last N matches
        #[arg(long)]
        last: Option<usize>,

        /// Print untruncated text
        #[arg(long)]
        full: bool,

        #[arg(long)]
        json: bool,
    },
    /// Every subagent the session ever launched (wave 2, full-file scan)
    Agents {
        session: String,

        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,

        #[arg(long)]
        json: bool,
    },
    /// One subagent's brief, status, and its own transcript digest — or a
    /// background Bash task's command and captured output (wave 2)
    Agent {
        session: String,
        /// An `agentId` (8+ char prefix accepted), a launching `tool-use-id`, or a Bash task id
        id: String,

        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,

        #[arg(long)]
        json: bool,
    },
    /// Full-file regex search; hits with handle, kind, time, and a snippet (wave 2)
    Grep {
        session: String,
        pattern: String,

        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,

        /// Comma-separated: owner, peer, agent, notification, all (default: all)
        #[arg(long)]
        kind: Option<String>,

        /// Characters of context shown on each side of a match
        #[arg(short = 'C', long = "context", default_value_t = grep_cmd::DEFAULT_CONTEXT_CHARS)]
        context: usize,

        #[arg(long)]
        json: bool,
    },
    /// A chronological slice around a point, or between two handles (wave 2)
    Span {
        session: String,
        /// Start handle for a `<from> [<to>]` range (omit when using `--around`)
        from: Option<String>,
        /// End handle for a `<from> <to>` range
        to: Option<String>,

        /// Center handle for a window of `n` entries on each side
        #[arg(long)]
        around: Option<String>,
        #[arg(short = 'n', long, default_value_t = 10)]
        n: usize,

        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,

        #[arg(long)]
        json: bool,
    },
}

fn resolve_home(home: Option<PathBuf>) -> Result<PathBuf> {
    match home {
        Some(path) => Ok(path),
        None => Ok(dirs::home_dir().context("Failed to get home directory")?.join(".claude")),
    }
}

fn session_dir_for(path: &std::path::Path) -> std::path::PathBuf {
    let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    path.parent().map(|parent| parent.join(session_id)).unwrap_or_default()
}

fn parse_kind_filter(raw: Option<&str>) -> Result<Option<Vec<render::MessageKind>>> {
    messages_cmd::parse_kinds(raw)
}

fn parse_timestamp_arg(raw: Option<&str>, flag: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    match raw {
        None => Ok(None),
        Some(raw) => {
            let parsed = chrono::DateTime::parse_from_rfc3339(raw)
                .with_context(|| format!("Invalid {flag} timestamp (expected RFC-3339): {raw}"))?;
            Ok(Some(parsed.with_timezone(&chrono::Utc)))
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::List { limit, projects_only, max_age_hours, all, home, json, grep } => {
            list_cmd::run(list_cmd::ListArgs {
                limit,
                include_archived: !projects_only,
                max_age_hours,
                all,
                home,
                json,
                grep,
            })?;
        }
        Commands::Load { session, home, json, debug } => {
            let home = resolve_home(home)?;
            let path = paths::resolve_session_path(&session, &home)?;
            load_cmd::run(&path, json, debug)?;
        }
        Commands::Show { session, handles, home, max_chars, json } => {
            let home = resolve_home(home)?;
            let path = paths::resolve_session_path(&session, &home)?;
            show_cmd::run(&path, &handles, max_chars, json)?;
        }
        Commands::Messages { session, home, kind, since, until, last, full, json } => {
            let home = resolve_home(home)?;
            let path = paths::resolve_session_path(&session, &home)?;
            let args = messages_cmd::MessagesArgs {
                kinds: parse_kind_filter(kind.as_deref())?,
                since: parse_timestamp_arg(since.as_deref(), "--since")?,
                until: parse_timestamp_arg(until.as_deref(), "--until")?,
                last,
                full,
                json,
            };
            messages_cmd::run(&path, &args)?;
        }
        Commands::Agents { session, home, json } => {
            let home = resolve_home(home)?;
            let path = paths::resolve_session_path(&session, &home)?;
            let session_dir = session_dir_for(&path);
            agents_cmd::run(&path, &session_dir, json)?;
        }
        Commands::Agent { session, id, home, json } => {
            let home = resolve_home(home)?;
            let path = paths::resolve_session_path(&session, &home)?;
            agent_cmd::run(&path, &id, json)?;
        }
        Commands::Grep { session, pattern, home, kind, context, json } => {
            let home = resolve_home(home)?;
            let path = paths::resolve_session_path(&session, &home)?;
            let args = grep_cmd::GrepArgs { pattern, kinds: parse_kind_filter(kind.as_deref())?, context_chars: context, json };
            grep_cmd::run(&path, &args)?;
        }
        Commands::Span { session, from, to, around, n, home, json } => {
            let home = resolve_home(home)?;
            let path = paths::resolve_session_path(&session, &home)?;
            let mode = match around {
                Some(handle) => span_cmd::SpanMode::Around { handle, n },
                None => {
                    let Some(from) = from else {
                        anyhow::bail!("span requires either <from-handle> or --around <handle>");
                    };
                    span_cmd::SpanMode::Range { from, to }
                }
            };
            span_cmd::run(&path, &span_cmd::SpanArgs { mode, json })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_home_after_load_identifier() {
        let cli = Cli::try_parse_from([
            "claude-session-restore",
            "load",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "--home",
            "C:\\fixture\\.claude",
        ])
        .expect("parse load command");

        let Commands::Load { session, home, json, debug } = cli.command else {
            panic!("expected load command");
        };
        assert_eq!(session, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        assert_eq!(home, Some(PathBuf::from("C:\\fixture\\.claude")));
        assert!(!json);
        assert!(!debug);
    }

    #[test]
    fn parses_list_with_home_all_and_json() {
        let cli = Cli::try_parse_from([
            "claude-session-restore",
            "list",
            "--home",
            "C:\\fixture\\.claude",
            "--all",
            "--json",
        ])
        .expect("parse list command");

        let Commands::List { home, all, json, limit, grep, max_age_hours, .. } = cli.command else {
            panic!("expected list command");
        };
        assert_eq!(home, Some(PathBuf::from("C:\\fixture\\.claude")));
        assert!(all);
        assert!(json);
        assert_eq!(limit, list_cmd::DEFAULT_LIMIT);
        assert_eq!(grep, None);
        assert_eq!(max_age_hours, None);
    }

    #[test]
    fn parses_list_with_grep_and_explicit_max_age() {
        let cli = Cli::try_parse_from([
            "claude-session-restore",
            "list",
            "--grep",
            "payments",
            "--max-age-hours",
            "5",
        ])
        .expect("parse list command");

        let Commands::List { grep, max_age_hours, .. } = cli.command else {
            panic!("expected list command");
        };
        assert_eq!(grep, Some("payments".to_string()));
        assert_eq!(max_age_hours, Some(5));
    }

    #[test]
    fn parses_show_with_multiple_handles() {
        let cli = Cli::try_parse_from(["claude-session-restore", "show", "0123abcd", "@o100", "@o200"])
            .expect("parse show command");
        let Commands::Show { session, handles, .. } = cli.command else { panic!("expected show command") };
        assert_eq!(session, "0123abcd");
        assert_eq!(handles, vec!["@o100".to_string(), "@o200".to_string()]);
    }

    #[test]
    fn parses_grep_with_context_flag() {
        let cli = Cli::try_parse_from(["claude-session-restore", "grep", "4567cdef", "QuickNode", "-C", "80"])
            .expect("parse grep command");
        let Commands::Grep { session, pattern, context, .. } = cli.command else { panic!("expected grep command") };
        assert_eq!(session, "4567cdef");
        assert_eq!(pattern, "QuickNode");
        assert_eq!(context, 80);
    }

    #[test]
    fn parses_span_around_mode() {
        let cli = Cli::try_parse_from(["claude-session-restore", "span", "89abcdef", "--around", "@o42", "-n", "10"])
            .expect("parse span command");
        let Commands::Span { session, around, n, from, .. } = cli.command else { panic!("expected span command") };
        assert_eq!(session, "89abcdef");
        assert_eq!(around, Some("@o42".to_string()));
        assert_eq!(n, 10);
        assert_eq!(from, None);
    }

    #[test]
    fn span_requires_from_or_around() {
        let cli = Cli::try_parse_from(["claude-session-restore", "span", "89abcdef"]).expect("parse span command");
        let Commands::Span { from, around, .. } = cli.command else { panic!("expected span command") };
        assert_eq!(from, None);
        assert_eq!(around, None);
        // main()'s dispatch bails on this combination; the parser itself
        // accepts it since `from` is optional syntax-wise.
    }
}
