---
name: bdd
description: Behaviour-driven development for writing tests as executable behavior specifications. Use when deciding what to test next, structuring tests with Given/When/Then, or diagnosing failing tests.
---

# Behaviour-Driven Development

BDD reframes testing as behavior specification: what the system should do,
under what context, with observable outcomes.

References:
- [Dan North: Introducing BDD](https://dannorth.net/introducing-bdd/)
- [language patterns](references/language-patterns.md)

## Goal
Drive development through sentence-named behaviors and Given/When/Then
structured scenarios.

## Constraints
- `behaviour-not-test`: ask "what should this do?" before "how to test?"
- `sentences-not-labels`: test names read as behavior statements.
- `given-when-then`: every case contains setup, action, assertion phases.
- `native-tooling`: use language-native test framework.
- `one-behaviour-per-test`: split cases containing multiple behaviors.

## Requirements
- `name-reads-as-spec`: test names form readable module specification.
- `given-establishes-context`: setup only.
- `when-performs-one-action`: single behavior trigger.
- `then-verifies-outcome`: assert observable outcomes, not internals.
- `behaviour-drives-priority`: next test chosen by behavior gap importance.

## Procedures

### identify-next-behaviour
1. List existing behaviors from test names.
2. Extract needed behaviors from AC/issue/module purpose.
3. Compute missing behaviors.
4. Rank by importance:
- happy path before error path
- core before edge
- dependency foundation before dependents
- user-visible impact first
5. Pick highest-priority behavior that can be expressed as one sentence.

### write-behaviour
1. Name behavior as sentence-style test.
2. Write Given/When/Then structure.
3. Red: run and confirm failure.
4. Green: implement minimal code to pass.
5. Refactor while keeping full suite green.

### evaluate-existing-tests
When a test fails after change, classify failure as one of:
- `bug_introduced`: fix implementation.
- `behaviour_moved`: move/redirect test.
- `behaviour_obsolete`: delete outdated test.

## Triggers
- writing tests
- starting new module
- implementing issue acceptance criteria
- diagnosing test failures
- deciding what to implement next

## Corruption Modes
- `testing-implementation`: asserting internals instead of behavior.
- `vague-names`: non-descriptive test names.
- `missing-given`: unclear setup context.
- `multi-behaviour-tests`: multiple behaviors in one case.
- `test-hoarding`: retaining obsolete tests.
- `framework-over-thinking`: choosing framework over method.

## Principles
- `words-shape-thinking`: behavior vocabulary improves test design.
- `specification-not-verification`: tests are living executable spec.
- `delete-freely`: obsolete behavior tests should be removed.

## Cross-References
- `planning`: session-level prioritization and execution sequencing.
- `issue-craft`: acceptance-criteria rigor and issue decomposition.
