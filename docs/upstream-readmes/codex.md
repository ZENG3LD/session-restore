# codex-session-restore

`codex-session-restore` is an independent, local Codex session parser and the source for the user-invoked `$codex-restore-session` skill. The skill can also delegate extraction to the existing Claude and Kimi restore helpers, then correlate the bounded report with read-only Git evidence.

This is the manual comparison path for Gate4Agent's automatic ContextPack. The two paths remain independent:

| Path | Trigger | Extraction boundary | Purpose |
| --- | --- | --- | --- |
| `$codex-restore-session` | Explicit skill invocation | Standalone local provider parser | User-controlled restore and quality baseline |
| ContextPack | Automatic harness injection | Gate4Agent Node/C2 pipeline | Centralized orchestration candidate |

Neither path is a native provider resume and neither should expose hidden reasoning, raw tool output, or credentials.

The standalone parser assumes a user-owned local Codex home. It rejects symlink/reparse candidates and reads active rollout files through a fixed opened-handle snapshot, but it is not an authority for adversarial shared transcript storage.

## Local commands

```powershell
codex-session-restore.exe list
codex-session-restore.exe load "<uuid-or-unambiguous-prefix-or-jsonl-path>" --json
```

The canonical skill source is in `skill/codex-restore-session/`. Install the validated folder under the user's Codex skills directory without coupling the standalone parser to Gate4Agent ContextPack code.
