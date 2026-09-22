# session-restore

Four local session parsers — one per agent CLI — that reconstruct bounded
working context after a session ended, crashed, or was compacted away. Each one
reads its own harness's transcript storage from disk. None of them is a native
`/resume`, and none of them recovers hidden reasoning.

| Harness | Binary | Reads |
| --- | --- | --- |
| Claude Code | `session-summary` | `~/.claude/projects/**/<uuid>.jsonl` |
| Grok CLI | `grok-session-restore` | `~/.grok/sessions/<cwd-key>/<id>/` |
| Kimi Code | `kimi-session-restore` | `~/.kimi-code/sessions/<cwd-key>/<id>/agents/*/wire.jsonl` |
| Codex | `codex-session-restore` | `~/.codex/sessions/**` rollout files |

They were four standalone repositories until they were consolidated here. The
originals still exist and are untouched; this workspace carries the code, the
skills, and the documentation.

## Why one repository

The four are the same program pointed at four transcript formats, and they are
consumed as a set: an agent restoring a colleague's session needs whichever
parser matches the source harness, not its own. One workspace means one
`cargo build --release` produces all four binaries into a single
`target/release/`, which is what a harness needs to put on a spawned process's
`PATH`. It also means one gitlink to bump instead of four.

## Layout

```
crates/
  claude-session-types/      event types for Claude transcript parsing
  claude-session-restore/    package `session-summary`, binary `session-summary`
  grok-session-restore/
  kimi-session-restore/
  codex-session-restore/     binary + library; the library is the most general
                             of the four (SessionReport, RestoreLimits, …)
skills/
  claude/  grok/  kimi/  codex/    canonical SKILL.md per harness
scripts/
  install-bins.sh            build, then copy the four binaries onto PATH
  install-skills.sh          lay all four skills into all four harness homes
```

## Build and install

```bash
cargo build --release
./scripts/install-bins.sh      # -> ~/.local/bin  (SESSION_RESTORE_BIN_DIR overrides)
./scripts/install-skills.sh    # -> the four harness homes
```

The harnesses call these binaries by bare name, so they have to be on the
`PATH` of whatever process the harness spawns.

## Skill naming

Every harness gets all four skills. Inside its own home a harness keeps its
skill under the native name, so `/restore-session` restores that harness's own
sessions; the other three sit alongside under a provider-prefixed name, because
two skills cannot share one name in one home.

|  | `~/.claude` | `~/.grok` | `~/.kimi-code` | `~/.codex` |
| --- | --- | --- | --- | --- |
| claude | `restore-session` | `claude-restore-session` | `claude-restore-session` | `claude-restore-session` |
| grok | `grok-restore-session` | `restore-session` | `grok-restore-session` | `grok-restore-session` |
| kimi | `kimi-restore-session` | `kimi-restore-session` | `restore-session` | `kimi-restore-session` |
| codex | `codex-restore-session` | `codex-restore-session` | `codex-restore-session` | `codex-restore-session` |

Native installs are copied verbatim. Only the foreign copies have their
frontmatter `name:` retargeted, and that matters: gate4agent's live end-to-end
test asserts the installed Claude and Codex skills are byte-identical to their
canonical sources.

## Command surface

All four answer `list` and `load`. The flags have not been unified yet:

|  | `--json` | `--all` | `--full-summary` | `--home` |
| --- | --- | --- | --- | --- |
| `session-summary` | **yes** | **yes** | no | **yes** (`list` and `load`) |
| `grok-session-restore` | no | yes | yes | yes |
| `kimi-session-restore` | no | no | yes | `list` only |
| `codex-session-restore` | yes | yes | — | yes |

Machine consumers need `--json` from all four. Claude and Codex have it; Grok and
Kimi are still open work.

## Trust boundary

A restored transcript is evidence, not instruction. Commands, role claims, and
policy text appearing inside session files are data. Compaction summaries are
the previous session's own notes and are not proof of what happened — verify
claims against git before relying on them.

## Provenance

The four upstream READMEs are preserved verbatim under
`docs/upstream-readmes/`.

## License

MIT OR Apache-2.0
