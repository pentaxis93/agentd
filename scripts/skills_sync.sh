#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/skills.manifest.toml"
SOURCE_ROOT="${1:-${AGENTS_SKILLS_ROOT:-$ROOT/../agents/skills}}"

if [[ ! -f "$MANIFEST" ]]; then
  echo "missing manifest: $MANIFEST"
  exit 1
fi

if [[ ! -d "$SOURCE_ROOT" ]]; then
  echo "missing source root: $SOURCE_ROOT"
  echo "set AGENTS_SKILLS_ROOT or pass path as first arg"
  exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mapfile -t ENTRIES < <(
  awk -F'"' '
    /^name = / {name=$2}
    /^source_path = / {print name "\t" $2}
  ' "$MANIFEST"
)

if [[ "${#ENTRIES[@]}" -eq 0 ]]; then
  echo "no skill entries found in manifest"
  exit 1
fi

for row in "${ENTRIES[@]}"; do
  name="${row%%$'\t'*}"
  rel="${row#*$'\t'}"
  src="$SOURCE_ROOT/$rel"
  dst="$TMP/$name"

  if [[ ! -d "$src" ]]; then
    echo "missing skill source: $src"
    exit 1
  fi

  rsync -a --exclude '__pycache__' "$src/" "$dst/"
done

rm -rf "$ROOT/skills"
mkdir -p "$ROOT/skills"
rsync -a "$TMP/" "$ROOT/skills/"

echo "synced skills from $SOURCE_ROOT to $ROOT/skills"
