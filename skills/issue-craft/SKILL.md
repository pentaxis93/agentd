---
name: issue-craft
description: Full issue lifecycle for GitHub/Forgejo projects. Use for creating, decomposing, refining, triaging, and closing issues that autonomous agents can execute without clarification.
---

# Issue Craft

Issues are the contract between intent and execution. A strong issue is
agent-executable without clarification.

For concrete task/epic/bug templates, see
[references/templates.md](references/templates.md).

## Goal
Produce and maintain issues that autonomous agents can execute end-to-end
without clarification.

## Constraints
- `agent-executable`: each task issue should be completable in one focused session.
- `independently-verifiable`: each acceptance criterion supports yes/no verification.
- `explicit-dependencies`: dependency links are explicit issue references.
- `hard-dependencies-only`: dependencies represent true blockers, not preferred ordering.
- `single-concern`: one logical change per issue.
- `scope-bounded`: scope names specific modules/files.
- `vertical-slice-bias`: decomposition favors independently shippable slices.

## Requirements
- `summary-states-what-and-why`: concise summary with explicit rationale.
- `scope-names-artifacts`: scope lists concrete code locations.
- `criteria-are-behaviors`: criteria describe outcomes, not implementation steps.
- `criteria-include-tests`: testing expectation is explicit.
- `criteria-include-docs`: user-facing changes include documentation updates.
- `criteria-are-binary`: each criterion can be verified as pass/fail.
- `dependencies-link-issues`: dependency references use issue numbers.
- `size-is-visible`: size class included (`small`, `medium`, `large`).
- `epic-has-dependency-graph`: epics with 4+ tasks include execution order graph.
- `tasks-are-session-sized`: each task can complete in one focused session.

## Procedures

### create-issue
1. Classify issue type (`task`, `epic`, `bug`, `spike`).
2. Write summary (what and why, not how).
3. Define scope with concrete files/modules.
4. Write acceptance criteria by category:
- functional outcome
- verification/testing
- documentation
5. Identify dependencies by searching existing issues.
6. Estimate size.
7. Title format: `<type>(<scope>): <what>`.
8. Run deterministic lint:
`python scripts/issue_lint.py --type <task|epic|bug|spike> <issue.md>`.
9. Assemble using template from `references/templates.md`.

### decompose-epic
1. Extract deliverables (artifacts that must exist when done).
2. Split into vertical slices that are independently verifiable.
3. Group by module boundary where it clarifies ownership.
4. Build dependency graph using hard blockers only.
5. Size-check each candidate:
- split if oversized
- merge if trivial
6. Create topologically ordered task issues.
7. Validate each task has binary acceptance criteria.
8. Create parent epic with checklist and graph.

### define-task-boundary
Task template:
- title: verb + object + short outcome
- scope: concrete files/modules touched
- goal: one sentence observable outcome
- acceptance criteria: binary pass/fail checks
- test plan: exact verification command or scenario
- effort: `small`, `medium`, or `large`

Pre-save checks:
- one logical concern only
- executable in one session
- dependencies are hard blockers only
- no implementation prescription in acceptance criteria

### refine-issue
1. Diagnose problems:
- vague summary
- missing scope
- untestable criteria
- missing tests/docs criteria
- implicit dependencies
- oversized/mixed concern
- missing size
2. Apply targeted fixes only where weak.
3. Keep already-strong criteria unchanged.
4. Re-run `scripts/issue_lint.py`.
Prefer strict mode when type is known:
`python scripts/issue_lint.py --type <task|epic|bug|spike> <issue.md>`.

### triage-issues
1. Refine non-ready issues first.
2. Build dependency graph for backlog.
3. Create topological execution layers.
4. Apply labels (`size:*`, module/area).
5. Assign milestones.
6. Flag stale issues for review.

### close-issue
1. Verify all acceptance criteria against implementation.
2. Check scope deviations and split unintended extra work.
3. Update parent epic/task checklist.
4. Close with commit/PR reference (`Closes #N`).

## Triggers
- creating issues
- decomposing large goals
- refining vague issues
- triaging/prioritizing backlog
- closing completed work
- planning milestones/releases

## Corruption Modes
- `activity-criteria`: criteria describe activities, not outcomes.
- `scope-sprawl`: issue spans unrelated modules.
- `implicit-how`: implementation prescription leaks into issue contract.
- `orphan-issues`: no epic/milestone context.
- `dependency-blindness`: hidden blockers.
- `kitchen-sink-epic`: oversized epic without phasing.
- `premature-issues`: filing work too far out.
- `test-afterthought`: no testing expectation.
- `docs-afterthought`: user-facing change without docs criterion.

## Principles
- `contract-not-conversation`: issue must stand alone.
- `outcomes-over-activities`: define required end state.
- `right-sized`: optimize for single focused execution session.

## Cross-References
- `planning`: session-level prioritization and execution discipline.
- `bdd`: behavior framing and test naming discipline.
- `dev-workflow`: issue-to-commit linkage and completion hygiene.
