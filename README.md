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

agentd supports project-level skills with two layers:

- Shared public skills tracked in this repository at `.agents/skills/`.
- Optional personal skills overlay installed locally at project level and kept untracked.

See `docs/personal-skill-overlay.md` for the personal overlay workflow, collision policy, and git-clean verification steps.

## License

Licensed under the terms in [LICENSE](LICENSE).
