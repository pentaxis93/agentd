#!/usr/bin/env bash
set -euo pipefail

command -v loadout >/dev/null 2>&1 || {
  echo "loadout is required in PATH"
  exit 1
}

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

PROJECT="$WORKDIR/agentd-smoke"
mkdir -p "$PROJECT/.loadout"

mkdir -p \
  "$WORKDIR/agentd-personal-skills/land" \
  "$WORKDIR/agents/skills/workflow/issue-craft" \
  "$WORKDIR/agents/skills/workflow/planning" \
  "$PROJECT/skills/ground"

cat > "$WORKDIR/agentd-personal-skills/land/SKILL.md" <<'EOF'
---
name: land
description: test land skill
---
# Land
EOF

cat > "$WORKDIR/agents/skills/workflow/issue-craft/SKILL.md" <<'EOF'
---
name: issue-craft
description: test issue-craft skill
---
# Issue Craft
EOF

cat > "$WORKDIR/agents/skills/workflow/planning/SKILL.md" <<'EOF'
---
name: planning
description: test planning skill
---
# Planning
EOF

cat > "$PROJECT/skills/ground/SKILL.md" <<'EOF'
---
name: ground
description: test ground skill
---
# Ground
EOF

cat > "$PROJECT/.loadout/agentd.toml" <<'EOF'
[sources]
skills = [
  "../../agentd-personal-skills",
  "../../agents/skills/workflow/issue-craft",
  "../../agents/skills/workflow/planning",
  "../skills",
]

[target_aliases.claude_code]
global = "~/.claude/skills"
project = ".claude/skills"

[target_aliases.opencode]
global = "~/.config/opencode/skills"
project = ".opencode/skills"

[target_aliases.codex]
global = "~/.agents/skills"
project = ".agents/skills"

[global]
targets = []
skills = ["ground", "land", "issue-craft", "planning"]

[projects.".."]
inherit = true
skills = []
targets = ["claude_code", "opencode", "codex"]
EOF

cd "$PROJECT"

LOADOUT_CONFIG="$PROJECT/.loadout/agentd.toml" loadout validate
LOADOUT_CONFIG="$PROJECT/.loadout/agentd.toml" loadout check
LOADOUT_CONFIG="$PROJECT/.loadout/agentd.toml" loadout install

list_names() {
  local dir="$1"
  if [[ ! -d "$dir" ]]; then
    return
  fi
  find "$dir" -mindepth 1 -maxdepth 1 -type l -printf '%f\n' | sort
}

CODEX_LIST="$(list_names ".agents/skills")"
CLAUDE_LIST="$(list_names ".claude/skills")"
OPENCODE_LIST="$(list_names ".opencode/skills")"

[[ "$CODEX_LIST" == "$CLAUDE_LIST" ]] || {
  echo "mismatch: codex vs claude"
  echo "codex:   $CODEX_LIST"
  echo "claude:  $CLAUDE_LIST"
  exit 1
}

[[ "$CODEX_LIST" == "$OPENCODE_LIST" ]] || {
  echo "mismatch: codex vs opencode"
  echo "codex:    $CODEX_LIST"
  echo "opencode: $OPENCODE_LIST"
  exit 1
}

grep -q 'skills = \["ground", "land", "issue-craft", "planning"\]' ".loadout/agentd.toml"
sed -i 's/skills = \["ground", "land", "issue-craft", "planning"\]/skills = []/' ".loadout/agentd.toml"

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
