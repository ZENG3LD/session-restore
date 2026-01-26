# Claude Session Restore

Efficiently restore context from previous Claude Code sessions by analyzing session files and git history.

## Features

- **Multi-Vector Data Collection**: Extracts tasks, user messages, tool operations, bash activities, and web searches
- **Time-Based Filtering**: Filter sessions by last modification time (12-24 hours)
- **Efficient Parsing**: Tail-based reverse parsing for large session files (2GB+)
- **Claude Code Integration**: Works seamlessly with Claude Code skill system

## Components

### 1. Session Summary CLI

A command-line tool for analyzing Claude Code session files.

**Commands:**
- `list` - List recent sessions with multi-vector summaries
- `load` - Load full context from a selected session

**Example:**
```bash
# List sessions from last 12 hours
session-summary list

# Extend to 24 hours
session-summary list --max-age-hours 24

# Load full context
session-summary load "/path/to/session-id.jsonl"
```

### 2. Claude Code Skill

A skill for Claude Code that enables automatic session restoration.

**Usage:**
Simply say "restore session" or "восстанови сессию" and Claude will:
1. List recent sessions with multi-vector data
2. Let you select which session to restore
3. Analyze the session data and provide context summary
4. Search git history for related commits

## Installation

### Prerequisites

- Rust 1.70+ (for building from source)
- Claude Code CLI

### Build from Source

```bash
git clone https://github.com/ZENG3LD/claude-session-restore
cd claude-session-restore
cargo build --release --workspace
```

The binary will be at `target/release/session-summary` (or `session-summary.exe` on Windows).

### Install to PATH

#### Linux/macOS
```bash
cargo install --path session-summary
```

#### Windows (Git Bash/MSYS2)
```bash
cargo build --release
cp target/release/session-summary.exe ~/.local/bin/
```

### Install Claude Code Skill

```bash
# Copy skill to Claude Code skills directory
mkdir -p ~/.claude/skills/restore-session
cp skill/SKILL.md ~/.claude/skills/restore-session/
```

## Usage

### CLI Tool

**List recent sessions:**
```bash
$ session-summary list

Recent Sessions:

1. 8f59d651-cada-4484-9153-5cc577137486
   Jan 26 04:33 | 32.42 MB | [projects] | Agent tasks
   📋 Tasks: Fix the dropdown z-order problem... → Fix the dropdown...
   💬 User: дропдауны либо с 0 опасити... → закомить работу...
   🔧 Tools: Bash, Bash, Bash, Write

2. 4e0b5d3d-c6d1-497d-9c6f-96e83980c7a0
   Jan 26 05:47 | 103.69 MB | [projects] | Agent tasks
   📋 Tasks: Implement MOEX ISS API connector...
   💬 User: <ide_opened_file>... → да, какие проблемы выявлены...
   🔧 Tools: Bash, Bash, Task, Task

To load a session, use:
  1. session-summary load "C:\Users\...\8f59d651-cada-4484-9153-5cc577137486.jsonl"
  2. session-summary load "C:\Users\...\4e0b5d3d-c6d1-497d-9c6f-96e83980c7a0.jsonl"
```

**Load full context:**
```bash
$ session-summary load "path/to/session.jsonl"

═══════════════════════════════════════
Session: 8f59d651-cada-4484-9153-5cc577137486
═══════════════════════════════════════
Date: 2026-01-26 04:33:18
Size: 32.42 MB
Topic: Agent tasks

Agent Tasks 📋 (126 tasks)
  1. СРОЧНО! Dropdown перекрывается другими элементами...
  2. Fix dropdown z-order problem in Chart Settings modal
  ...

User Messages 💬 (2 messages)
  1. закомить работу в ваших крейтах
  2. итого подведи итог что мы сделали...

Tool Operations 🔧 (7 operations)
  1. Task
  2. Bash
  ...

Files Modified 📁 (17 files)
  1. zengeld-terminal/ui/chart_settings.rs
  2. zengeld-terminal/ui/dropdown.rs
  ...

Git Branch: zengeld-chart
```

### Claude Code Skill

In a new Claude Code session:

```
User: restore session

Claude: I'll list recent sessions from the last 12 hours...
[Shows multi-vector data for each session]

Which session would you like to restore? (1-3)

User: 1

Claude: [Analyzes session data and provides context summary]
```

## Architecture

### Multi-Vector Data Collection

The tool collects raw data from multiple event sources:

- **📋 Agent Tasks** - What agents were doing
- **💬 User Messages** - User requests and decisions
- **🔧 Tool Operations** - Files read/edited and operations performed
- **⚙️ Bash Activities** - Build/test commands and git operations
- **🔍 Web Searches** - Research topics and API documentation

### Efficient Parsing

- Uses `tail` command to read only the end of session files
- Parses events in reverse order
- Stops early after collecting required data
- Handles large session files (2GB+) without loading entire file

### Design Philosophy

The parser **does not try to infer topics** - it simply collects raw data from all event types. The Claude Code skill then analyzes this data to understand what was being worked on.

## Project Structure

```
claude-session-restore/
├── types/              # Type definitions for Claude session events
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       └── events/     # Event type hierarchy
├── session-summary/    # CLI tool
│   ├── Cargo.toml
│   └── src/main.rs
├── skill/              # Claude Code skill
│   └── SKILL.md
├── README.md
└── LICENSE
```

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Support the Project

If you find this tool useful, consider supporting development:

| Currency | Network | Address |
|----------|---------|---------|
| USDT | TRC20 | `TNxMKsvVLYViQ5X5sgCYmkzH4qjhhh5U7X` |
| USDC | Arbitrum | `0xEF3B94Fe845E21371b4C4C5F2032E1f23A13Aa6e` |
| ETH | Ethereum | `0xEF3B94Fe845E21371b4C4C5F2032E1f23A13Aa6e` |
| BTC | Bitcoin | `bc1qjgzthxja8umt5tvrp5tfcf9zeepmhn0f6mnt40` |
| SOL | Solana | `DZJjmH8Cs5wEafz5Ua86wBBkurSA4xdWXa3LWnBUR94c` |

## Acknowledgments

Built for use with [Claude Code](https://claude.com/claude-code) - Anthropic's CLI tool for Claude.
