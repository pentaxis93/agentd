# agentd

`agentd` is an autonomous AI agent runtime daemon. Run autonomous AI agents on infrastructure you control.

## Status

[![Status](https://img.shields.io/badge/status-placeholder-lightgrey)](#)

Early development. Not yet functional.

## Architecture Overview

The system is organized as a Rust workspace with focused crates for runtime, scheduling, and MCP transport/plugin integration. See [ARCHITECTURE.md](ARCHITECTURE.md) for the architecture document.

## Quick Start

Coming soon.

## Developer Tooling: Skills

agentd pilots `loadout` for project-level skill discovery.

- Project skills are tracked in this repository at `skills/` and used as the
  sole default `loadout` source for project setup.
- Skill vendoring is manifest-driven via `skills.manifest.toml`.
  - Sync vendored skills from upstream: `make skills-sync`
  - Verify manifest + config coherence: `make skills-verify`
- `loadout` installs enabled skills into tool discovery directories:
  - `.agents/skills/` (Codex)
  - `.claude/skills/` (Claude Code)
  - `.opencode/skills/` (OpenCode)
- Project pilot config lives at `.loadout/agentd.toml` and is intended to be used with:
  - `LOADOUT_CONFIG=$PWD/.loadout/agentd.toml loadout install`

See `docs/personal-skill-overlay.md` for personal overlay layering and rollback to manual links.

## License

Licensed under the terms in [LICENSE](LICENSE).
