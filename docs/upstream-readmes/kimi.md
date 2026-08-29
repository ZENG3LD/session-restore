# kimi-session-restore

Restore context from previous **Kimi Code CLI** sessions by parsing session storage directly — the analog of `claude-session-restore` for Kimi's `wire.jsonl` format.

## Commands

```bash
kimi-session-restore list [--max-age-hours N] [--all]
kimi-session-restore load <session-dir | session-id-prefix | wire.jsonl> [--full-summary]
```

- `list` — recent sessions: id, last-modified (UTC), wire size, workdir, title, last user prompt. Default window: 12 hours.
- `load` — deep dive into one session: user messages with timestamps, steer/cron activity, tool-call histogram, files touched, last assistant texts, compaction history, and the last compaction summary (the session's own handoff notes).

## Session storage parsed

```
~/.kimi-code/sessions/<workdir-key>/<session_id>/state.json
~/.kimi-code/sessions/<workdir-key>/<session_id>/agents/<agent>/wire.jsonl
```

Respects `KIMI_CODE_HOME` if set.

## Build / install

```bash
cargo build --release
cp target/release/kimi-session-restore.exe ~/.local/bin/   # Git Bash on Windows
```

## Companion skill

Source: `skill/SKILL.md`. Install under the Kimi Code user skills directory:

```bash
mkdir -p ~/.kimi-code/skills/restore-session
cp skill/SKILL.md ~/.kimi-code/skills/restore-session/
```

Triggered on "восстанови сессию" / "restore session". Instructs the agent to use this binary instead of hand-parsing `wire.jsonl`.

## License

MIT OR Apache-2.0
