# Issue Templates

Concrete templates for each issue type. Copy and fill in the sections.
All templates use GitHub-flavored markdown.

## Table of Contents

- [Task Issue](#task-issue)
- [Epic Issue](#epic-issue)
- [Bug Report](#bug-report)
- [Spike / Investigation](#spike--investigation)
- [Acceptance Criteria Patterns](#acceptance-criteria-patterns)
- [Dependency Graph Notation](#dependency-graph-notation)

---

## Task Issue

A single deliverable unit of work. Completable in one focused session.

**Title format:** `<type>(<scope>): <what it does>`

```markdown
## Summary

[1-3 sentences. What needs to exist and why. Reference parent epic if applicable.]

Part of: #N

## Scope

- `src/module/file.rs` — [what changes here]
- `src/module/other.rs` — [what changes here]

## Acceptance criteria

- [ ] [Observable outcome using a verb: creates, returns, validates, rejects, reports]
- [ ] [Another observable outcome]
- [ ] [Testing expectation: unit tests, integration tests, or both]

## Dependencies

Depends on: #A (reason), #B (reason)
```

### Example: task issue

```markdown
Title: feat(config): TOML parsing, path expansion, XDG resolution

## Summary

Implement the `config/` module: parse `loadout.toml` with serde,
expand `~` to `$HOME`, resolve config location via XDG, and support
`$LOADOUT_CONFIG` override.

Part of: #1 (Phase 2: Rust Parity)

## Scope

- `src/config/mod.rs` — Config loading + path resolution
- `src/config/types.rs` — Serde structs for `loadout.toml`

## Acceptance criteria

- [ ] Parses `loadout.toml` with all sections: `[sources]`, `[global]`, `[projects.*]`
- [ ] Expands `~` and `~/` to `$HOME` in all path fields
- [ ] Resolves config from `$LOADOUT_CONFIG`, then `$XDG_CONFIG_HOME/loadout/loadout.toml`, then `~/.config/loadout/loadout.toml`
- [ ] Returns typed errors for missing config, parse failures, invalid paths
- [ ] Unit tests covering path expansion and config resolution

## Dependencies

None — this is the first module to implement.
```

---

## Epic Issue

A tracking issue that decomposes into task issues. Contains no
implementation work itself.

**Title format:** `Phase N: <goal>` or `epic: <goal>`

```markdown
## Summary

[2-4 sentences. The goal of this body of work and what's different
when it's complete.]

Full spec: [link to design doc or roadmap section]

## Task issues

### [Layer name] (e.g., Library modules, CLI commands)

- [ ] #N — `module/` — [brief description]
- [ ] #M — `other/` — [brief description]

## Dependency graph

```
#A ──┬── #C
     │
#B ──┴── #D
```

## Acceptance criteria

[System-level criteria, not per-task. What's true when the epic is done.]

- [ ] [Overall behaviour that proves the epic is complete]
- [ ] [Quality gate: no regressions, all tests pass, docs updated]
```

### Example: epic issue

```markdown
Title: Phase 2: Rust Parity (v0.2.0)

## Summary

Replace the three bash scripts with a single Rust binary. The
`loadout` command should be installable via `cargo install --path .`
with zero Python dependency at runtime.

Full spec: docs/ROADMAP.md — Phase 2

## Task issues

### Library modules

- [ ] #5 — `config/` — TOML parsing, path expansion, XDG resolution
- [ ] #6 — `skill/frontmatter.rs` — YAML frontmatter parsing + validation
- [ ] #7 — `skill/mod.rs` — Source directory walking, skill resolution
- [ ] #8 — `linker/` — Symlink creation/removal, marker management

### CLI commands

- [ ] #9 — `loadout install` (depends on #5, #7, #8)
- [ ] #10 — `loadout clean` (depends on #5, #8)

## Dependency graph

```
#5 config ──────┬── #9 install
#6 frontmatter ─┤
#7 skill/mod ───┤── #10 clean
#8 linker ──────┘
```

## Acceptance criteria

- [ ] `loadout install` produces identical symlink layout to `install.sh`
- [ ] No Python dependency at runtime
- [ ] `cargo install --path .` places binary in `~/.cargo/bin/loadout`
```

---

## Bug Report

A defect with reproduction steps and expected vs actual behaviour.

**Title format:** `fix(<scope>): <what's wrong>`

```markdown
## Bug

[One sentence: what's broken.]

## Reproduction

1. [Step to reproduce]
2. [Step to reproduce]
3. [Observe: what actually happens]

## Expected behaviour

[What should happen instead.]

## Environment

- OS: [e.g., Ubuntu 24.04]
- Version: [e.g., v0.2.0, commit abc123]
- Config: [relevant config if applicable]

## Acceptance criteria

- [ ] [The fixed behaviour, stated as an observable outcome]
- [ ] [Regression test covering this case]
```

---

## Spike / Investigation

Time-boxed research. Produces answers and follow-up issues, not code.

**Title format:** `spike: <question to answer>`

```markdown
## Question

[The specific question this spike answers.]

## Time box

[Maximum time to spend before reporting findings, e.g., "2 hours"]

## Context

[Why we need to investigate this. What decision depends on the answer.]

## Expected output

- [ ] Written summary of findings (comment on this issue)
- [ ] Recommendation with rationale
- [ ] Follow-up issues created if work is needed
```

---

## Acceptance Criteria Patterns

Good criteria describe **outcomes**, not activities.

| Pattern | Example |
|---------|---------|
| Creates artifact | Creates `.managed-by-loadout` marker files in managed directories |
| Returns value | Returns typed error for missing config file |
| Validates input | Rejects skill names containing uppercase characters |
| Handles edge case | Returns empty list when no source directories are configured |
| Reports to user | Prints resolved skill paths during `--dry-run` |
| Preserves invariant | Never removes content it didn't create |
| Testing | Unit tests covering path expansion and XDG resolution |
| Integration | Integration tests verifying end-to-end install with temp directories |

### Anti-patterns

| Anti-pattern | Problem | Rewrite as |
|-------------|---------|------------|
| "Research best approach" | Activity, not outcome | Spike issue with specific question |
| "Clean up the code" | Vague, no verification | "Extract X into separate module with public API" |
| "Handle errors properly" | No specific behaviour | "Returns ConfigError::NotFound when file missing" |
| "Add tests" | No scope | "Unit tests covering path expansion and config resolution" |
| "Update docs" | No specifics | "README install section reflects cargo install method" |

---

## Dependency Graph Notation

Use ASCII art for dependency graphs in epic issues. Keep it simple.

### Linear chain

```
#5 config → #7 skill → #9 install
```

### Fan-out (one foundation, many dependents)

```
#5 config ──┬── #9 install
             ├── #10 clean
             ├── #11 list
             └── #13 new
```

### Diamond (multiple foundations merge)

```
#5 config ──────┬── #9 install
#7 skill ───────┤
#8 linker ──────┘
```

### Layered (with labels)

```
Foundation:   #5 config    #6 frontmatter    #8 linker
                  │              │                │
Integration:  #7 skill (depends on #5, #6)       │
                  │                               │
Commands:     #9 install (depends on #5, #7, #8) ─┘
```
