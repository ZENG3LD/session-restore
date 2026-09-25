//! Session path resolution and filesystem safety.
//!
//! Accepts an exact JSONL path, an exact UUID, or a UUID prefix of at least
//! 8 hex characters, and resolves it to a canonical path that is guaranteed
//! to live under one of the selected Claude home's `projects`/`archive`
//! roots, with no symlink or junction component anywhere along the way.

use crate::topic::quick_topic;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Shortest accepted UUID prefix. Below this, too many sessions are likely
/// to collide on the shared leading hex digits.
const MIN_UUID_PREFIX: usize = 8;

#[derive(Debug)]
struct SessionRoot {
    lexical: PathBuf,
    canonical: PathBuf,
}

/// Resolve a load argument without allowing it to escape the selected Claude home.
pub fn resolve_session_path(session: &str, home: &Path) -> Result<PathBuf> {
    let roots = session_roots(home)?;
    let raw_path = Path::new(session);

    if looks_like_jsonl_path(session, raw_path) {
        return validate_session_path(raw_path, &roots);
    }

    if !is_uuid(session) && !is_uuid_prefix(session) {
        anyhow::bail!(
            "Session must be an exact JSONL path, an exact UUID, or a UUID prefix of at least {MIN_UUID_PREFIX} characters"
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
        _ => Err(ambiguous_error(session, &matches)),
    }
}

fn ambiguous_error(session: &str, matches: &[PathBuf]) -> anyhow::Error {
    let mut lines = vec![format!("Session identifier is ambiguous ({} matches): {session}", matches.len())];
    for path in matches {
        let uuid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
        let title = quick_topic(path);
        lines.push(format!("  {uuid}  {title}"));
    }
    anyhow::anyhow!(lines.join("\n"))
}

fn session_roots(home: &Path) -> Result<Vec<SessionRoot>> {
    let home = absolute_lexical(home)?;
    let mut roots = Vec::new();

    for directory in ["projects", "archive"] {
        let lexical = home.join(directory);
        let metadata = match fs::symlink_metadata(&lexical) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to inspect Claude session root: {}", lexical.display())
                })
            }
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
        anyhow::bail!("Claude session roots were not found under: {}", home.display());
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

    let root = roots
        .iter()
        .find(|root| lexical.starts_with(&root.lexical) && canonical.starts_with(&root.canonical));
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
        has_windows_reparse_attribute(metadata.file_attributes())
    }

    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
fn has_windows_reparse_attribute(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Directory basenames that never hold selectable sessions: subagent
/// transcripts and their raw tool-result blobs live alongside a session's
/// own `<uuid>.jsonl` under `<uuid>/subagents/` and `<uuid>/tool-results/`.
/// Skipping them here means `load`'s UUID/prefix resolution can never select
/// a subagent transcript (whose filename is `agent-<hex>.jsonl`, not a UUID,
/// so it was already excluded downstream — this also avoids the wasted
/// recursion on sessions with many delegated subagents).
const NON_SESSION_DIRECTORIES: [&str; 2] = ["subagents", "tool-results"];

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
                let is_non_session_dir = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| NON_SESSION_DIRECTORIES.contains(&name));
                if !is_non_session_dir {
                    pending.push(path);
                }
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
    if value.len() < MIN_UUID_PREFIX || value.len() >= 36 {
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
        std::env::current_dir().context("Failed to get current directory")?.join(path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    struct TestHome {
        path: PathBuf,
    }

    impl TestHome {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("claude-session-restore-paths-tests-{}-{unique}", std::process::id()));
            fs::create_dir_all(path.join("projects").join("project-a")).expect("create projects root");
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
        assert_eq!(resolve_session_path(id, &home.path).expect("resolve exact UUID"), expected);
        assert_eq!(
            resolve_session_path("aaaaaaaa-a", &home.path).expect("resolve unique prefix"),
            expected
        );
    }

    #[test]
    fn resolves_eight_char_prefix() {
        let home = TestHome::new();
        let id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let path = home.session("projects/project-a", id);
        let expected = fs::canonicalize(&path).expect("canonical fixture path");

        assert_eq!(
            resolve_session_path("bbbbbbbb", &home.path).expect("resolve 8-char prefix"),
            expected
        );
    }

    #[test]
    fn rejects_short_and_lists_ambiguous_prefixes() {
        let home = TestHome::new();
        home.session("projects/project-a", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        home.session("archive", "aaaaaaaa-aaaa-4aaa-8aaa-bbbbbbbbbbbb");

        let short = resolve_session_path("aaaaaaa", &home.path)
            .expect_err("short prefix must fail")
            .to_string();
        assert!(short.contains("at least 8"), "unexpected error: {short}");

        let ambiguous = resolve_session_path("aaaaaaaa", &home.path)
            .expect_err("ambiguous prefix must fail")
            .to_string();
        assert!(ambiguous.contains("ambiguous"), "unexpected error: {ambiguous}");
        assert!(ambiguous.contains("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"), "must list candidate: {ambiguous}");
        assert!(ambiguous.contains("aaaaaaaa-aaaa-4aaa-8aaa-bbbbbbbbbbbb"), "must list candidate: {ambiguous}");
    }

    #[test]
    fn rejects_outside_and_nonregular_paths() {
        let home = TestHome::new();
        let outside = home.path.with_extension("outside.jsonl");
        fs::write(&outside, "{}\n").expect("write outside fixture");
        let nonregular = home.path.join("projects").join("directory.jsonl");
        fs::create_dir(&nonregular).expect("create nonregular fixture");

        let outside_error =
            resolve_session_path(outside.to_str().expect("UTF-8 outside path"), &home.path)
                .expect_err("outside path must fail")
                .to_string();
        assert!(outside_error.contains("outside"), "unexpected error: {outside_error}");

        let nonregular_error =
            resolve_session_path(nonregular.to_str().expect("UTF-8 nonregular path"), &home.path)
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
        let target = home.session("projects/project-a", "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        let link = home
            .path
            .join("projects")
            .join("project-a")
            .join("cccccccc-cccc-4ccc-8ccc-cccccccccccc.jsonl");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).expect("create session symlink");

        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_file(&target, &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("create session symlink: {error}");
        }

        let error = resolve_session_path(link.to_str().expect("UTF-8 link path"), &home.path)
            .expect_err("symlink must fail")
            .to_string();
        assert!(error.contains("symlink"), "unexpected symlink error: {error}");
    }

    #[test]
    fn subagent_transcripts_are_never_selected_as_load_targets() {
        let home = TestHome::new();
        let session_id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        home.session("projects/project-a", session_id);
        let subagents_dir =
            home.path.join("projects").join("project-a").join(session_id).join("subagents");
        fs::create_dir_all(&subagents_dir).expect("create subagents dir");
        fs::write(subagents_dir.join("agent-deadbeef.jsonl"), "{}\n").expect("write subagent fixture");

        let mut matches = Vec::new();
        let roots = session_roots(&home.path).expect("session roots");
        for root in &roots {
            collect_session_files(&root.lexical, &mut matches).expect("collect session files");
        }
        assert!(
            matches.iter().all(|path| path.file_stem().and_then(|s| s.to_str()).is_some_and(is_uuid)),
            "collect_session_files must never surface a non-UUID-stemmed subagent transcript: {matches:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn recognizes_windows_reparse_attribute() {
        assert!(has_windows_reparse_attribute(0x400));
        assert!(has_windows_reparse_attribute(0x420));
        assert!(!has_windows_reparse_attribute(0x20));
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_junction_component() {
        let home = TestHome::new();
        let id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        let target = home.path.join("projects").join("real-project");
        fs::create_dir_all(&target).expect("create junction target");
        fs::write(target.join(format!("{id}.jsonl")), "{}\n").expect("write junction session fixture");
        let junction = home.path.join("projects").join("linked-project");
        let output = std::process::Command::new("cmd.exe")
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
        let error = resolve_session_path(linked_session.to_str().expect("UTF-8 junction path"), &home.path)
            .expect_err("junction component must fail")
            .to_string();
        assert!(error.contains("symlink"), "unexpected junction error: {error}");

        fs::remove_dir(&junction).expect("remove junction fixture");
    }
}
