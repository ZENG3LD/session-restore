---
name: restore-session
description: Restore context from a previous Kimi Code session by analyzing session files (wire.jsonl) and git history. Triggers on phrases like "restore session", "previous session", "last session", "восстанови сессию", "восстанов сессию", "предыдущая сессия", "прошлая сессия", "найди последнюю сессию", "продолжи работу".
whenToUse: When the user asks to restore, find, or continue a previous/last Kimi Code session, or asks what was done before a restart/crash/new session.
---

# Session Restoration Skill (Kimi Code)

Restore full context when starting a new session after the previous one was closed, crashed, rebooted, or compacted away.

Uses the `kimi-session-restore` CLI (installed at `~/.local/bin/kimi-session-restore.exe`, source: `nemo/session-restore/crates/kimi-session-restore/`). It parses Kimi Code session storage directly — do NOT parse `wire.jsonl` by hand with python/grep; use the tool.

## Session storage layout (reference)

- `~/.kimi-code/sessions/<workdir-key>/<session_id>/state.json` — title, workDir, createdAt/updatedAt, agents, lastPrompt
- `.../agents/<agent>/wire.jsonl` — full event stream (main agent = `agents/main`)
- Key event types: `turn.prompt` (user input), `turn.steer` (cron-fire / background notifications), `context.append_loop_event` (assistant text, tool.call), `context.apply_compaction` (contains a hand-written context summary — the single most valuable artifact)

## Restoration process

### Step 1: List recent sessions

```bash
kimi-session-restore.exe list            # last 12 hours
kimi-session-restore.exe list --all      # no time filter
kimi-session-restore.exe list --max-age-hours 48
kimi-session-restore.exe list --home "D:\alt\.kimi-code"   # non-default Kimi home
kimi-session-restore.exe list --all --json                 # machine-readable
```

Each entry shows: session id, last-modified time (local, with UTC offset), wire size, workdir, topic, last user prompt. The topic is `state.json`'s own `title` when it is set, otherwise the first genuine human prompt in the session (never a cron fire, task notification, or harness-injected turn). If several sessions fit, ask the user which one to restore.

### Step 2: Load the selected session

Copy the load command from the list output, or use the id prefix:

```bash
kimi-session-restore.exe load "C:\Users\...\.kimi-code\sessions\<wd>\<session_id>"
kimi-session-restore.exe load session_72cefc81          # id prefix works
kimi-session-restore.exe load <target> --full-summary   # untruncated compaction summary
kimi-session-restore.exe load <target> --json           # machine-readable
kimi-session-restore.exe --help                         # full flag reference
```

The report is a verbatim digest, never a paraphrase: the last 3 human prompts and last 3 assistant texts print in full (older ones in the window collapse to one line), followed by a "Recent Tool Operations" section — the last ~15 tool.call events in chronological order with their key argument (path, command, pattern, …) — then the usual tool histogram, files-touched inventory, steer/cron activity, and compaction history. The last compaction summary is the previous session's own handoff notes — read it first, it usually contains state, conventions, and the TODO list.

Reads are byte-budgeted (last 32 MiB of `wire.jsonl` for the digest, first 1 MiB for the title/first-prompt fallback), so a huge session still loads in well under a second. If the file is bigger than that window, the report says so explicitly instead of silently showing an incomplete picture.

### Step 3: Cross-reference git history

Extract project/feature keywords from the session, then:

```bash
git log --all --oneline --grep="keyword" -i --since="1 week ago" | head -30
git show --stat <hash>
```

Check handoff/plan docs the session references (e.g. `nemo/docs/**/handoff-*.md`) and read them.

### Step 4: Present a structured summary

```markdown
## Session Restoration Summary
**Session**: [id] | **Date**: [span] | **Project**: [workdir]
### What was being worked on
### Open threads / next steps (from last prompt + TODO in compaction summary)
### Related commits and docs
```

## Notes

- The current live session also appears in `list` — usually pick the second one unless the user means otherwise.
- `state.json`'s `lastPrompt` = the very last thing the user said; the reply to it may be missing if the session died mid-turn.
- Subagent transcripts live in `agents/agent-N/wire.jsonl`; the tool loads `main` by default, pass a subagent wire path explicitly if needed.
- Treat compaction summaries as the previous session's own notes, not proof — verify claims (tests, commits) before relying on them.
