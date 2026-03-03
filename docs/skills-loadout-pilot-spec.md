# Skills Loadout Pilot Spec

Behavioral specification for issue #14: use `loadout` to manage project-level
skill discovery for Codex, Claude Code, and OpenCode in `agentd`.

## Scenario 1: Fresh project install exposes identical project skills

Given a clone of `agentd` with `skills/` populated  
And no sibling skill repositories present  
And `LOADOUT_CONFIG` set to `.loadout/agentd.toml`  
When `loadout install` runs in the project root  
Then `.agents/skills`, `.claude/skills`, and `.opencode/skills` each contain
the same enabled skill names  
And each entry is a symlink to the canonical source under `skills/`.

## Scenario 2: Config change propagates across all project targets

Given an installed project from Scenario 1  
When the enabled skill set in `.loadout/agentd.toml` changes  
And `loadout clean` then `loadout install` runs again  
Then all project target directories reflect the same updated skill set  
And no target has stale managed links.

## Scenario 3: Skill behavior is unchanged

Given an existing tracked skill in `skills/`  
When `loadout validate` runs  
Then frontmatter and markdown content validation results match pre-pilot
expectations  
And only discovery plumbing has changed.

## Scenario 4: Manual rollback remains available

Given loadout pilot setup is removed or disabled  
When the documented manual symlink commands are applied  
Then Codex and Claude Code discover project skills through manual links  
And the rollback path is deterministic and documented.
