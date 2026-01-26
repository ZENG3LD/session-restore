---
name: restore-session
description: Restore context from previous Claude session by analyzing session files and git history. Triggers on phrases like "restore session", "previous session", "восстанови сессию", "восстанов сессию", "предыдущая сессия".
---

# Session Restoration Skill

This skill helps restore full context when starting a new Claude session after a previous session was closed, crashed, or became too large.

## When to Use

Use this skill when:
- Starting a new session after a previous one ended
- User asks to restore previous context
- Session was interrupted or overflowed
- User mentions "restore", "previous session", "last session"
- User mentions (Russian): "восстанови", "восстанов", "предыдущая сессия", "прошлая сессия", "продолжи работу"

## Restoration Process

### Step 1: Find and Analyze Recent Session Files

Use the `session-summary` CLI tool to list recent sessions with automatic topic extraction:

```bash
# List recent sessions (default: last 12 hours, includes both projects/ and archive/)
session-summary.exe list

# Extend time window to 24 hours
session-summary.exe list --max-age-hours 24

# Only search projects directory (exclude archive)
session-summary.exe list --projects-only
```

**What you'll see for each session:**
- **Date/time**: When it was last modified
- **Size**: File size (indicates session length)
- **Source**: [projects] or [archive]
- **Topic**: Simple label (Agent tasks, User session, Tool usage, etc.)
- **Multi-vector data**: Raw data from different event sources with emoji labels:
  - 📋 Tasks - Agent task prompts
  - 💬 User - User messages
  - 🔧 Tools - Tool operations with file paths
  - ⚙️ Bash - Bash command outputs
  - 🔍 Search - Web search queries

**Example output**:
```
Recent Sessions:

1. 8f59d651-cada-4484-9153-5cc577137486
   Jan 26 04:33 | 32.42 MB | [projects] | Agent tasks
   📋 Tasks: Fix the dropdown z-order problem... → Fix the dropdown... → Fix the dropdown...
   💬 User: дропдауны либо с 0 опасити... → закомить работу...
   🔧 Tools: Bash, Bash, Bash, Write

2. 4e0b5d3d-c6d1-497d-9c6f-96e83980c7a0
   Jan 26 05:47 | 103.69 MB | [projects] | Agent tasks
   📋 Tasks: Implement MOEX ISS API connector... → Implement MOEX ISS API connector... → Implement MOEX ISS API connector...
   💬 User: <ide_opened_file>The user opened... → да, какие проблемы выявлены...
   🔧 Tools: Bash, Bash, Bash, Task, Task

3. 3162998f-09ca-4efc-b659-8507eb57bd37
   Jan 26 00:21 | 232.79 MB | [archive] | User session
   💬 User: Create 2-turn conversation test... → Run the tests...
   🔧 Tools: Task, Bash, Grep, Read, Write
   ⚙️ Bash: cargo check; compiling...

To load a session, use:
  1. session-summary.exe load "C:\Users\...\8f59d651-cada-4484-9153-5cc577137486.jsonl"
  2. session-summary.exe load "C:\Users\...\4e0b5d3d-c6d1-497d-9c6f-96e83980c7a0.jsonl"
  3. session-summary.exe load "C:\Users\...\3162998f-09ca-4efc-b659-8507eb57bd37.jsonl"
```

**Ask user which session to restore, then copy-paste the corresponding load command**

### Step 2: Deep Dive into Selected Session

Once user selects a session, copy-paste the corresponding load command from the list output:

```bash
# Copy the full command from "To load a session" section above
session-summary.exe load "C:\Users\...\session-id.jsonl"
```

**Example output**:
```
═══════════════════════════════════════
Session: 8f59d651-cada-4484-9153-5cc577137486
═══════════════════════════════════════
Date: 2026-01-26 04:33:18
Size: 32.42 MB
Topic: Agent tasks

Agent Tasks 📋 (126 tasks)
  1. СРОЧНО! Dropdown перекрывается другими элементами (footer, другие UI)...
  2. Fix dropdown z-order problem in Chart Settings modal
  3. Implement hover states for dropdown items
  4. Add auto-sizing to dropdown containers
  ... (122 more)

User Messages 💬 (2 messages)
  1. закомить работу в ваших крейтах (чужую не комить)
  2. итого подведи итог что мы сделали по унификации...

Tool Operations 🔧 (7 operations)
  1. Task
  2. Bash
  3. Edit (zengeld-terminal/ui/chart_settings.rs)
  4. Read (zengeld-terminal/ui/dropdown.rs)
  ... (3 more)

Files Modified 📁 (17 files)
  1. zengeld-terminal/ui/chart_settings.rs
  2. zengeld-terminal/ui/dropdown.rs
  3. zengeld-terminal/ui/modal.rs
  ... (14 more)

Git Branch: zengeld-chart
```

The tool automatically:
- Reads last segment of session from compact_boundary to end
- Collects data from 6+ event types (AgentProgress, User, Assistant, BashProgress, QueryUpdate, FileSnapshot)
- Shows all data vectors with counts and previews
- Does NOT try to infer topics - just shows raw data
- Extracts git branch and commit hints for further analysis

### Step 3: Search Git History

Search for related commits to understand what was accomplished:

```bash
# Extract keywords from session (project names, features, etc.)
# Then search git log with those keywords

git log --all --oneline --grep="keyword1" --grep="keyword2" -i --since="1 week ago" | head -30

# Get detailed commit info
git show --stat <commit-hash>
```

**Look for**:
- Commits made during or after the session timestamp
- Commit messages describing the feature/fix being worked on
- File changes that match the session's tracked files

### Step 4: Analyze Multi-Vector Data & Provide Context Summary

**IMPORTANT**: The parser shows RAW DATA in multiple vectors. Your job is to ANALYZE this data and understand what was being worked on.

Look at all vectors:
- **Agent Tasks** - What agents were doing (feature implementation, debugging, refactoring)
- **User Messages** - What user was asking for, decisions made
- **Tool Operations** - Which files were read/edited, what operations performed
- **Bash Activities** - Build/test commands, git operations
- **Web Searches** - Research topics, API documentation lookups
- **Files Modified** - Scope of changes

Present a structured summary:

```markdown
## Session Restoration Summary

**Session**: [session-id]
**Date**: [timestamp]
**Project**: [project-name]

### What Was Being Worked On:
[Analyze the multi-vector data to determine the main task/feature/bugfix]

### Files Being Worked On:
- path/to/file1.rs (main implementation)
- path/to/file2.rs (supporting changes)
- ...

### Related Git Commits:
- [hash] commit message (date)
- [hash] commit message (date)

### Context from Session Data:
- **Agent focus**: [from Tasks vector]
- **User requests**: [from User Messages vector]
- **Technical operations**: [from Tools/Bash vectors]

### Recommendations:
- Continue from: [specific file or task]
- Next steps: [based on commit history and unfinished work]
```

## Important Notes

- **Session files can be VERY large** (2GB+) - session-summary handles this efficiently
- **Tool uses tail + reverse parsing** - reads from end, stops early
- **Multi-vector approach** - parser collects raw data, you analyze it
- **Don't assume topic from label** - "Agent tasks" is just a simple label, analyze the actual data
- **Git commits are the best context** - they show what was actually accomplished
- **Timestamps help** - cross-reference session time with commit times
- **Time filtering** - default 12 hours, can extend to 24 hours with --max-age-hours

## Example Usage

```
User: "restore previous session"
Assistant: [Runs restore-session skill]

I'll list recent sessions from the last 12 hours:

[runs: session-summary.exe list]

Recent Sessions:

1. 8f59d651-cada-4484-9153-5cc577137486
   Jan 26 04:33 | 32.42 MB | [projects] | Agent tasks
   📋 Tasks: Fix the dropdown z-order problem... → Fix the dropdown... → Fix the dropdown...
   💬 User: дропдауны либо с 0 опасити... → закомить работу...
   🔧 Tools: Bash, Bash, Bash, Write

2. 4e0b5d3d-c6d1-497d-9c6f-96e83980c7a0
   Jan 26 05:47 | 103.69 MB | [projects] | Agent tasks
   📋 Tasks: Implement MOEX ISS API connector... → Implement MOEX ISS API connector...
   💬 User: <ide_opened_file>... → да, какие проблемы выявлены...
   🔧 Tools: Bash, Bash, Task, Task

3. 3162998f-09ca-4efc-b659-8507eb57bd37
   Jan 26 00:21 | 232.79 MB | [archive] | User session
   💬 User: Create 2-turn conversation test... → Run the tests...
   🔧 Tools: Task, Bash, Grep, Read
   ⚙️ Bash: cargo check; compiling...

To load a session, use:
  1. session-summary.exe load "C:\Users\...\8f59d651-cada-4484-9153-5cc577137486.jsonl"
  ...

Which session would you like to restore? (1-3)

User: "1"
Assistant: [Runs the load command]

[runs: session-summary.exe load "C:\Users\...\8f59d651-cada-4484-9153-5cc577137486.jsonl"]

[Analyzes multi-vector data]

Based on the session data:
- **Main task**: Fixing dropdown z-order issue in Chart Settings modal
- **User requested**: Commit work in your crates, summarize unification work
- **Agent worked on**: UI dropdown rendering, z-order fixes, hover states
- **Files modified**: chart_settings.rs, dropdown.rs, modal.rs

[Searches git log for related commits]
[Provides full context summary]
```

## Implementation Tips

1. **Always start with the most recent** sessions from the active project
2. **Check both locations**: `~/.claude/projects/` and `~/.claude/archive/`
3. **Parse JSONL carefully**: Each line is a separate JSON object
4. **Cross-reference**: Session files + git log = complete picture
5. **Ask user**: If multiple sessions found, let user choose

## Limitations

- Cannot restore actual conversation history (that's internal to Claude)
- Can only infer context from files and commits
- Very old sessions might not have related commits anymore
- Archived sessions may be compressed or incomplete