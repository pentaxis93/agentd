# ground

First-principles cognitive discipline for design and implementation work in agentd.

## Purpose
Prevent cargo-cult design and implementation drift by establishing what a change must enable before deriving solution shape.

## Workflow
1. State the required capability in behavioral terms.
2. List constraints that must hold for the capability to be true.
3. Mark each constraint as verified fact or assumption.
4. Convert assumptions into checks, tests, or explicit open questions.
5. Propose design choices only after constraints are explicit.
6. Validate the resulting design against the original behavioral need.

## Quality Bar
- Need is explicit before solution discussion.
- Constraints are traceable to requirements, interfaces, or tests.
- Assumptions are identified and either validated or removed.
- Chosen design is justified by constraints, not by familiarity.

## Anti-Patterns
- Starting from existing code shape and backfilling rationale.
- Treating current implementation as required architecture.
- Copying patterns from other projects without local constraint checks.
- Writing code before defining observable done behavior.
