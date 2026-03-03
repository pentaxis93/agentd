#!/usr/bin/env bash
set -euo pipefail

OUT="$(printf '%s\n' \
  '.agents/skills' \
  '.agents/skills/' \
  '.claude/skills' \
  '.claude/skills/' \
  '.opencode/skills' \
  '.opencode/skills/' \
  | git check-ignore -v --stdin --non-matching)"

echo "$OUT"

if echo "$OUT" | grep -q '^::'; then
  echo "expected all managed target forms to be ignored"
  exit 1
fi

for pattern in '/.agents/skills' '/.claude/skills' '/.opencode/skills'; do
  if ! echo "$OUT" | grep -q "$pattern"; then
    echo "missing ignore match for $pattern"
    exit 1
  fi
done

echo "gitignore managed target smoke test passed"
