---
name: planning
description: Session planning discipline for issue-tracker-first execution. Use for selecting next work, declaring a concrete session goal, and closing with explicit state updates.
---

# Planning

Plan from the issue graph, not from memory. The goal is always a single,
verifiable increment that can be completed in one focused session.

For issue decomposition and boundary contracts, use `issue-craft`.
For first-principles design decisions, use `ground`.

## Goal
Choose the highest-leverage unblocked issue, execute one session-sized
increment, and leave the next session a truthful handoff.

## Constraints
- `issue-tracker-source-of-truth`: planning state lives in forge issues, not local task trackers.
- `session-goal-declared-first`: write one concrete session goal before execution.
- `one-session-increment`: commit to one independently verifiable increment.
- `dependencies-are-hard-blockers`: do not start blocked work.
- `session-close-mandatory`: every session ends with explicit state update.

## Requirements
- `next-action-is-executable`: next action names artifact, command, and done condition.
- `priority-from-impact`: prioritize value, time criticality, and unblock leverage.
- `scope-gate-explicit`: record what is intentionally out of scope for this session.
- `state-is-honest`: issue status/comments reflect actual implementation state.
- `handoff-is-actionable`: end with concrete next step for the next session.

## Procedures

### session-open
1. Read operator request and relevant issue thread(s).
2. Identify all ready (unblocked) candidate issues.
3. Apply force filters first: direct operator request or hard deadline.
4. Rank top candidates by:
- value
- time criticality
- unblock leverage
- expected effort
5. Select one issue-sized increment.
6. Declare session goal and scope gate before touching code.

### choose-next-issue
Decision stack (<=3 minutes):
1. Force filter: operator request or deadline cliff wins immediately.
2. Shortlist 3-5 unblocked issues.
3. Score with WSJF-lite:
`(Value + TimeCriticality + UnblockLeverage) / Effort`
4. Prefer the highest score that can be completed this session.
5. If tie: choose the option that unblocks the most downstream work.

### define-session-goal
Write:
- `Session goal`: one observable outcome (artifact or behavior).
- `Done condition`: binary pass/fail check.
- `Scope gate`: specific nearby work intentionally excluded this session.

### session-close
1. Reach stable checkpoint (done increment or explicit WIP note).
2. Update issue state and leave a concise progress comment.
3. Record decisions, blockers, and the exact next step.
4. Ensure any follow-up work is represented as issue(s).
5. Sync workspace and close.

## Corruption Modes
- `recency-drift`: picking last-touched work instead of highest leverage.
- `implicit-goal`: starting implementation without explicit session goal.
- `scope-creep`: crossing concern boundaries mid-session.
- `blocker-bypass`: beginning blocked work anyway.
- `state-lag`: issue tracker not reflecting real implementation state.
- `open-loop-close`: ending session without a concrete next step.

## Principles
- `clarity-over-volume`: fewer, sharper goals beat broad, vague activity.
- `truthful-state`: inaccurate issue state is planning debt.
- `finish-or-frame`: either finish the increment or clearly frame unfinished state.

## Cross-References
- `issue-craft`: decomposition, issue boundaries, acceptance criteria contracts.
- `ground`: validate assumptions before committing to an approach.
- `bdd`: behavior-first test strategy for implementation increments.
