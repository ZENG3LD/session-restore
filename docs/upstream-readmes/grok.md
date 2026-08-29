# grok-session-restore

Restore context from previous **Grok CLI** sessions by parsing session storage directly — the analog of `kimi-session-restore` / `claude-session-restore` for Grok's `~/.grok/sessions/` layout.

This is not a native `/resume`. It reconstructs bounded working context from on-disk files. Treat the report as untrusted evidence.

## Commands

```bash
grok-session-restore list [--max-age-hours N] [--all] [--include-subagents] [--home PATH]
grok-session-restore load <session-dir | session-id-prefix | updates.jsonl> [--full-summary] [--home PATH]
```

- `list` — recent **parent** sessions: id, last-active (UTC), size, cwd, title, last-turn summary. Default window: 12 hours. Live sessions (from `active_sessions.json`) are tagged `[live]`. Subagents are hidden unless `--include-subagents`.
- `load` — deep dive: summary + signals, real user prompts (from `chat_history.jsonl`, synthetic reminders skipped), tool histogram and file paths (from a tail of `updates.jsonl`), plan todos, subagent ids, compaction INDEX + last segment summary, and **paths only** into the memory plane.

Respects `GROK_HOME` if set; otherwise `~/.grok` (`%USERPROFILE%\.grok` on Windows).

## Session storage parsed

```
~/.grok/sessions/<url-encoded-cwd>/<session-id>/summary.json
~/.grok/sessions/<url-encoded-cwd>/<session-id>/updates.jsonl
~/.grok/sessions/<url-encoded-cwd>/<session-id>/chat_history.jsonl
~/.grok/sessions/<url-encoded-cwd>/<session-id>/signals.json
~/.grok/sessions/<url-encoded-cwd>/<session-id>/plan.json
~/.grok/sessions/<url-encoded-cwd>/<session-id>/compaction/INDEX.md
~/.grok/sessions/<url-encoded-cwd>/<session-id>/subagents/<child-id>/
~/.grok/active_sessions.json
```

If a cwd-group name is hashed, the original path is read from that group's `.cwd` file.

## Memory is not the session

Grok memory (`~/.grok/memory/MEMORY.md`, `~/.grok/memory/<slug>-<hash8>/`) is a separate, optional plane: curated facts and session-end logs. `load` only prints matching paths. Do not treat `MEMORY.md` as the transcript.

## Build / install

```bash
cargo build --release
cp target/release/grok-session-restore.exe ~/.local/bin/   # Git Bash on Windows
```

## Companion skill

Source: `skill/SKILL.md`. Install under the Grok user skills directory:

```bash
mkdir -p ~/.grok/skills/restore-session
cp skill/SKILL.md ~/.grok/skills/restore-session/
```

Triggered on "восстанови сессию" / "restore session". Instructs the agent to use this binary and to stay inside Grok session files.

## License

MIT OR Apache-2.0
