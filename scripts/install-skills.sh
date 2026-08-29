#!/usr/bin/env bash
# Lay all four restore skills into all four harness homes.
#
# Naming rule: inside its OWN home a harness keeps the skill under its native
# name, so `/restore-session` in Grok restores Grok sessions. The other three
# are installed alongside under a provider-prefixed name. Codex is the only
# harness whose native name is already prefixed.
#
# The native copy is byte-identical to the canonical source under skills/.
# gate4agent's live E2E asserts exactly that for the Claude and Codex homes,
# so never rewrite a native install. Only the foreign copies get their
# frontmatter `name:` retargeted, because two skills cannot share one name
# inside a single home.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
skills="$here/skills"

claude_home="${CLAUDE_HOME:-$HOME/.claude}"
grok_home="${GROK_HOME:-$HOME/.grok}"
kimi_home="${KIMI_CODE_HOME:-$HOME/.kimi-code}"
codex_home="${CODEX_HOME:-$HOME/.codex}"

# provider | native skill directory name
native_dir() {
  case "$1" in
    claude|grok|kimi) echo "restore-session" ;;
    codex)            echo "codex-restore-session" ;;
  esac
}

home_of() {
  case "$1" in
    claude) echo "$claude_home" ;;
    grok)   echo "$grok_home" ;;
    kimi)   echo "$kimi_home" ;;
    codex)  echo "$codex_home" ;;
  esac
}

install_one() {
  local provider="$1" home="$2" dest_name="$3" rewrite="$4"
  local src="$skills/$provider" dest="$home/skills/$dest_name"
  mkdir -p "$dest"
  if [ "$rewrite" = "verbatim" ]; then
    cp "$src/SKILL.md" "$dest/SKILL.md"
  else
    sed "s/^name: .*$/name: $dest_name/" "$src/SKILL.md" > "$dest/SKILL.md"
  fi
  # codex ships an interface descriptor next to its skill
  [ -d "$src/agents" ] && cp -r "$src/agents" "$dest/"
  printf '  %-24s -> %s\n' "$dest_name" "$dest"
}

for target in claude grok kimi codex; do
  home="$(home_of "$target")"
  [ -d "$home" ] || { echo "skip $target: no $home"; continue; }
  echo "$target ($home)"
  for provider in claude grok kimi codex; do
    if [ "$provider" = "$target" ]; then
      install_one "$provider" "$home" "$(native_dir "$provider")" verbatim
    else
      install_one "$provider" "$home" "$provider-restore-session" rename
    fi
  done
done
