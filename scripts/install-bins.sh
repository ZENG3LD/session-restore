#!/usr/bin/env bash
# Put the four restore CLIs on PATH.
#
# The harnesses invoke these by bare name — hatchery's live E2E asserts the
# provider calls `claude-session-restore load <id>` with no path prefix — so
# they have to resolve through the ambient PATH of whatever process the
# harness spawns.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bindir="${SESSION_RESTORE_BIN_DIR:-$HOME/.local/bin}"
ext=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) ext=".exe" ;; esac

cargo build --release --manifest-path "$here/Cargo.toml"

mkdir -p "$bindir"
for bin in claude-session-restore grok-session-restore kimi-session-restore codex-session-restore; do
  cp "$here/target/release/$bin$ext" "$bindir/"
  printf '  %-24s -> %s\n' "$bin$ext" "$bindir"
done
