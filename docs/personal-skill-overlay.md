# Personal Skills Overlay

This document defines the supported way to combine shared public skills with personal developer skills at project scope.

## Goal

- Keep shared project skills committed with the repository.
- Allow each developer to add personal skills locally at project level.
- Keep personal skill content and identity details out of committed files.

## Directory Contract

- Public skills (tracked): `.agents/skills/<public-skill>/SKILL.md`
- Personal overlay (local only): `.agents/skills/<personal-skill>/`

## Installation Pattern (Personal Overlay)

1. Store personal skills in a personal location (for example a personal repo clone).
2. Install them into this project as symlinks under `.agents/skills/`.
3. Add personal overlay paths to local-only excludes in `.git/info/exclude`.

Example local installation pattern:

```bash
PROJECT_ROOT=/path/to/agentd
PERSONAL_ROOT=/path/to/personal-skills

mkdir -p "$PROJECT_ROOT/.agents/skills"
ln -sfn "$PERSONAL_ROOT/skills/my-personal-skill" \
  "$PROJECT_ROOT/.agents/skills/my-personal-skill"
printf '%s\n' '.agents/skills/my-personal-skill' >> "$PROJECT_ROOT/.git/info/exclude"
```

Use an idempotent local installer script if you have many personal skills.

## Collision Policy

- Personal overlay names may intentionally collide with tracked public skill directory names.
- On collision, the personal overlay skill overrides the project skill for that developer's local environment.
- Use this for personal adaptation of shared workflows without changing team-wide defaults.

## Git Hygiene Policy

- Do not add personal overlay patterns to `.gitignore`.
- Do not commit personal skill names, URLs, or local paths.
- Use `.git/info/exclude` (or equivalent local-only ignore) for personal overlay entries.

## Verification Checklist

Run after installing or updating personal overlays:

```bash
git status --porcelain
```

Expected: no personal overlay paths appear in output.

Optional link verification:

```bash
find .agents/skills -maxdepth 1 -type l -print
```

Expected: only intentionally installed personal symlinks are listed.
