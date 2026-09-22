use codex_session_restore::{
    default_codex_home, encode_json, list_sessions, load_session, render_report, resolve_target,
    RestoreError, RestoreLimits,
};
use std::ffi::OsString;
use std::path::PathBuf;

fn main() {
    if let Err(error) = run(std::env::args_os().skip(1).collect()) {
        eprintln!("codex-session-restore: {error}");
        std::process::exit(1);
    }
}

fn run(args: Vec<OsString>) -> Result<(), RestoreError> {
    let Some(command) = args.first().and_then(|value| value.to_str()) else {
        return Err(usage());
    };
    match command {
        "list" => run_list(&args[1..]),
        "load" => run_load(&args[1..]),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        _ => Err(usage()),
    }
}

fn run_list(args: &[OsString]) -> Result<(), RestoreError> {
    let mut home = None;
    let mut max_age_hours = Some(12_u64);
    let mut limit = 10_usize;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].to_str() {
            Some("--home") => {
                index += 1;
                home = Some(PathBuf::from(args.get(index).ok_or_else(usage)?));
            }
            Some("--max-age-hours") => {
                index += 1;
                max_age_hours = Some(parse_u64(args.get(index), "max age")?);
            }
            Some("--all") => max_age_hours = None,
            Some("--limit") => {
                index += 1;
                limit = parse_usize(args.get(index), "list limit")?;
            }
            Some("--json") => json = true,
            _ => return Err(usage()),
        }
        index += 1;
    }
    let home = home.unwrap_or(default_codex_home()?);
    let candidates = list_sessions(&home, max_age_hours, limit)?;
    if json {
        println!("{}", encode_json(&candidates)?);
    } else if candidates.is_empty() {
        match max_age_hours {
            Some(hours) => println!(
                "No matching Codex sessions in the last {hours}h. Try --all or --max-age-hours <N>."
            ),
            None => println!("No matching Codex sessions."),
        }
    } else {
        for candidate in candidates {
            let title = candidate.title.as_deref().unwrap_or("untitled");
            println!(
                "{} | {} | {} bytes | {}",
                candidate.id, candidate.updated_unix_ms, candidate.size_bytes, title
            );
        }
    }
    Ok(())
}

fn run_load(args: &[OsString]) -> Result<(), RestoreError> {
    let target = args.first().ok_or_else(usage)?.clone();
    let mut home = None;
    let mut json = false;
    let mut limits = RestoreLimits::default();
    let mut index = 1;
    while index < args.len() {
        match args[index].to_str() {
            Some("--home") => {
                index += 1;
                home = Some(PathBuf::from(args.get(index).ok_or_else(usage)?));
            }
            Some("--max-tail-bytes") => {
                index += 1;
                limits.max_tail_bytes = parse_usize(args.get(index), "tail byte limit")?;
            }
            Some("--max-lines") => {
                index += 1;
                limits.max_lines = parse_usize(args.get(index), "line limit")?;
            }
            Some("--max-messages") => {
                index += 1;
                limits.max_messages = parse_usize(args.get(index), "message limit")?;
            }
            Some("--json") => json = true,
            _ => return Err(usage()),
        }
        index += 1;
    }
    let home = home.unwrap_or(default_codex_home()?);
    let source = resolve_target(&home, &target)?;
    let report = load_session(&source, limits)?;
    if json {
        println!("{}", encode_json(&report)?);
    } else {
        let rendered = render_report(&report);
        if rendered.len() > codex_session_restore::MAX_OUTPUT_BYTES {
            return Err(RestoreError::OutputLimit);
        }
        print!("{rendered}");
    }
    Ok(())
}

fn parse_usize(value: Option<&OsString>, label: &str) -> Result<usize, RestoreError> {
    value
        .and_then(|value| value.to_str())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| RestoreError::InvalidArgument(format!("{label} is invalid")))
}

fn parse_u64(value: Option<&OsString>, label: &str) -> Result<u64, RestoreError> {
    value
        .and_then(|value| value.to_str())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| RestoreError::InvalidArgument(format!("{label} is invalid")))
}

fn usage() -> RestoreError {
    RestoreError::InvalidArgument(
        "usage: codex-session-restore <list|load> [options]".to_owned(),
    )
}

fn print_usage() {
    println!(
        "codex-session-restore\n\n\
         Usage:\n\
           codex-session-restore list [--home PATH] [--max-age-hours N|--all] [--limit N] [--json]\n\
           codex-session-restore load <UUID|PREFIX|JSONL> [--home PATH] [--max-tail-bytes N] [--max-lines N] [--max-messages N] [--json]"
    );
}
