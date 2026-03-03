#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/skills.manifest.toml"
LOADOUT="$ROOT/.loadout/agentd.toml"

norm_csv() {
  tr -d ' ' | tr ',' '\n' | sed '/^$/d' | sort
}

extract_inline_list() {
  local file="$1"
  local section="$2"
  local key="$3"
  awk -v sec="$section" -v key="$key" '
    $0 == "[" sec "]" {in_sec=1; next}
    /^\[/ {in_sec=0}
    in_sec && $0 ~ ("^" key " = \\[") {print; exit}
  ' "$file"
}

defaults_line="$(extract_inline_list "$MANIFEST" "defaults" "skills")"
if [[ -z "$defaults_line" ]]; then
  echo "manifest defaults.skills missing"
  exit 1
fi

loadout_line="$(extract_inline_list "$LOADOUT" "global" "skills")"
if [[ -z "$loadout_line" ]]; then
  echo "loadout global.skills missing"
  exit 1
fi

manifest_defaults="$(echo "$defaults_line" | sed -E 's/^skills = \[(.*)\]$/\1/' | tr -d '"' | norm_csv)"
loadout_defaults="$(echo "$loadout_line" | sed -E 's/^skills = \[(.*)\]$/\1/' | tr -d '"' | norm_csv)"

if [[ "$manifest_defaults" != "$loadout_defaults" ]]; then
  echo "mismatch: manifest defaults vs loadout global skills"
  echo "manifest:"
  echo "$manifest_defaults"
  echo "loadout:"
  echo "$loadout_defaults"
  exit 1
fi

sources_line="$(extract_inline_list "$LOADOUT" "sources" "skills")"
if [[ "$sources_line" != 'skills = ["../skills"]' ]]; then
  echo "unexpected loadout [sources].skills: $sources_line"
  exit 1
fi

mapfile -t MANIFEST_NAMES < <(
  awk -F'"' '/^name = / {print $2}' "$MANIFEST" | sort
)

mapfile -t SKILL_DIRS < <(
  find "$ROOT/skills" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort
)

if [[ "${MANIFEST_NAMES[*]}" != "${SKILL_DIRS[*]}" ]]; then
  echo "mismatch: manifest skill names vs skills directory set"
  echo "manifest: ${MANIFEST_NAMES[*]}"
  echo "skills:   ${SKILL_DIRS[*]}"
  exit 1
fi

for name in "${MANIFEST_NAMES[@]}"; do
  if [[ ! -f "$ROOT/skills/$name/SKILL.md" ]]; then
    echo "missing SKILL.md for $name"
    exit 1
  fi
done

echo "skills manifest and loadout config verified"
