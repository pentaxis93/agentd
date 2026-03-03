#!/usr/bin/env bash
set -euo pipefail

command -v loadout >/dev/null 2>&1 || {
  echo "loadout is required in PATH"
  exit 1
}

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

PROJECT="$WORKDIR/agentd-smoke"
mkdir -p "$PROJECT"
cp -a skills "$PROJECT/skills"
cp -a .loadout "$PROJECT/.loadout"

cd "$PROJECT"

LOADOUT_CONFIG="$PROJECT/.loadout/agentd.toml" loadout validate
LOADOUT_CONFIG="$PROJECT/.loadout/agentd.toml" loadout check
LOADOUT_CONFIG="$PROJECT/.loadout/agentd.toml" loadout install

EXPECTED="$(printf '%s\n' bdd ground issue-craft land planning | sort)"

list_names() {
  local dir="$1"
  find "$dir" -mindepth 1 -maxdepth 1 -type l -printf '%f\n' | sort
}

CODEX_LIST="$(list_names ".agents/skills")"
CLAUDE_LIST="$(list_names ".claude/skills")"
OPENCODE_LIST="$(list_names ".opencode/skills")"

[[ "$CODEX_LIST" == "$CLAUDE_LIST" ]] || {
  echo "mismatch: codex vs claude"
  exit 1
}

[[ "$CODEX_LIST" == "$OPENCODE_LIST" ]] || {
  echo "mismatch: codex vs opencode"
  exit 1
}

[[ "$CODEX_LIST" == "$EXPECTED" ]] || {
  echo "unexpected enabled skill set"
  echo "expected:"
  echo "$EXPECTED"
  echo "actual:"
  echo "$CODEX_LIST"
  exit 1
}

awk '
  BEGIN { in_global = 0 }
  /^\[global\]$/ { in_global = 1; print; next }
  /^\[/ { in_global = 0 }
  in_global && /^skills = / { print "skills = []"; next }
  { print }
' ".loadout/agentd.toml" > ".loadout/agentd.toml.tmp"
mv ".loadout/agentd.toml.tmp" ".loadout/agentd.toml"

LOADOUT_CONFIG="$PROJECT/.loadout/agentd.toml" loadout install
LOADOUT_CONFIG="$PROJECT/.loadout/agentd.toml" loadout clean
LOADOUT_CONFIG="$PROJECT/.loadout/agentd.toml" loadout install

[[ -z "$(find .agents -type l -print)" ]] || {
  echo "expected empty codex target after disabling skills"
  exit 1
}
[[ -z "$(find .claude -type l -print)" ]] || {
  echo "expected empty claude target after disabling skills"
  exit 1
}
[[ -z "$(find .opencode -type l -print)" ]] || {
  echo "expected empty opencode target after disabling skills"
  exit 1
}

echo "loadout pilot smoke test passed"
