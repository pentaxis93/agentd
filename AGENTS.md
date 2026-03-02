# AGENTS

## Project Identity
agentd is an autonomous AI agent runtime daemon for running autonomous AI agents on infrastructure you control. It is for builders and operators who need predictable, self-hosted execution of agent workflows without relying on hosted runtimes. The project is being built as a modular Rust workspace so runtime, scheduling, and integration concerns can evolve independently.

## Architecture
agentd uses a workspace-and-plugin shape: the `agentd` binary crate composes focused crates for runner lifecycle (`agentd-runner`), scheduling (`agentd-scheduler`), shared MCP transport (`mcp-transport`), and Forgejo MCP integration (`forgejo-mcp`). Keep architectural decisions aligned to this modular boundary unless a constraint requires change. See `ARCHITECTURE.md` for design rationale and the full constraint derivation.

## Development Discipline

This workflow is always in effect for contributions to this repository and is not optional.

### Ground Before Designing
For any new module, API surface, protocol, or data structure, define what capability must exist when the change is complete before inspecting existing implementation patterns. State required outcomes first, then derive constraints from what must be true for those outcomes to hold. Separate actual constraints from inherited assumptions and challenge assumptions unless they are verified by requirements, interfaces, or tests. Compare against existing approaches only after a need-first design exists. Reference: `.agents/skills/ground/SKILL.md`.

### BDD First
Every change must follow this sequence: behavioral spec -> test -> implementation -> verification. Define done as observable behavior before coding. Write or update tests that fail without the change and pass when behavior is correct. Implement only what is necessary to satisfy the behavioral contract. No PR is complete without behavioral coverage for the change.

### Coherence on Landing
Each landing PR must verify documentation and code remain aligned. Confirm README claims still match repository reality. Confirm `ARCHITECTURE.md` still describes the actual architecture. Confirm doc comments match runtime behavior and interfaces. Confirm `AGENTS.md` still reflects required agent workflow and quality gates. If drift is found, fix it in the same PR.

## Conventions
- Commit messages must use conventional commits: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`.
- Branch names must follow `issue-N-brief-description`.
- Keep PR scope to one issue and make it small enough for focused review.
- Rust changes must be `cargo fmt` formatted, `cargo clippy` clean, and warning-free.
