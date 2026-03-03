# Personal Skills Overlay

How to layer personal skills on top of project skills using `loadout`.

## Goal

- Keep shared project skills committed at `skills/`.
- Allow each developer to layer private personal skills locally.
- Keep personal skill names, URLs, and paths out of committed files.

## Project Pilot Contract

- Canonical tracked source in this repo: `skills/<skill>/SKILL.md`
- Project loadout config: `.loadout/agentd.toml`
- Project default sources are ordered for precedence:
  1. `../../agentd-personal-skills`
  2. `../../agents/skills/workflow/issue-craft`
  3. `../../agents/skills/workflow/planning`
  4. `../skills`
- Project default enabled skills: `ground`, `land`, `issue-craft`, `planning`
- Loadout-managed project targets:
  - `.agents/skills` (Codex)
  - `.claude/skills` (Claude Code)
  - `.opencode/skills` (OpenCode)

Install project skills:

```bash
LOADOUT_CONFIG="$PWD/.loadout/agentd.toml" loadout install
```

## Personal Overlay Pattern (Local Only)

1. Store personal skills in a private location (for example a personal repo
   clone).
2. Add that location to your local loadout config `sources.skills` before the
   shared source so first match wins.
3. Enable personal skill names in `global.skills` or project skills for your
   local config.

Example local config snippet:

```toml
[sources]
skills = [
  "/path/to/personal-skills",
  "/path/to/agentd/skills",
]
```

Do not commit personal config edits. Keep them in your user-scoped
`~/.config/loadout/loadout.toml` or in an untracked local copy.

## Collision Policy

- Name collisions are allowed by source ordering.
- First matching source wins; this enables personal override of a shared skill.
- Shared defaults remain unchanged for other developers.

## Git Hygiene Policy

- Do not commit personal source paths or private skill names.
- Keep personal local setup out of tracked files.
- Generated target directories are ignored by this repo.

## Verification Checklist

Run after personal overlay changes:

```bash
LOADOUT_CONFIG="$PWD/.loadout/agentd.toml" loadout list
LOADOUT_CONFIG="$PWD/.loadout/agentd.toml" loadout check
git status --porcelain
```

Expected:

- `loadout list` resolves skills from expected source paths.
- `loadout check` reports no blocking errors.
- `git status --porcelain` contains no personal path leakage.

## Rollback To Manual Alias Flow

If pilot behavior fails, use manual links:

```bash
mkdir -p .agents .claude .opencode
ln -sfn ../skills .agents/skills
ln -sfn ../.agents/skills .claude/skills
ln -sfn ../.agents/skills .opencode/skills
```

This restores manual project-level discovery paths for Codex, Claude Code, and
OpenCode.

When using normal pilot operation, do not hand-manage symlinks inside these
target directories; let `loadout install` and `loadout clean` own them.
