---
name: ground
description: >-
  First-principles cognitive discipline for all generative work. Use when
  creating specs, architectures, processes, solutions, methodologies, problem
  framings — any task requiring original design. Also applies to migration,
  upgrade, and technology selection decisions. Establishes what the work must
  enable before decomposing to verified constraints, then builds from what
  is actually true.
metadata:
  version: "2.0.0"
  updated: "2026-03-01"
  origin: >-
    Successor to clean-slate. The predecessor caught migration failures
    (fabricated switching costs, compatibility layering) but missed the
    broader pattern: agents default to inherited thinking in ALL generative
    work. The same instinct that fabricates switching costs also accepts
    problem statements uncritically, copies categories from adjacent systems,
    and preserves complexity out of fear.
  replaces: "clean-slate"
---

# Ground

*What must this enable? What is actually required? Build from that.*

## The Move

Four steps. Always the same.

0. **Orient.** Before touching anything, establish the need. What must this enable? Who does it serve? What do they need to accomplish? These answers are the actual constraints. Everything else — existing code, existing patterns, existing implementations — is evidence about one attempt to meet those constraints, not the constraints themselves.

1. **Decompose.** Strip the problem to its actual constraints. What must be true? What is assumed? What was inherited from the prompt, the existing system, or the adjacent example?

2. **Verify.** For each constraint: is this real (physics, contract, measured need) or inherited (convention, precedent, comfort)? If you cannot point to evidence, it is assumed.

3. **Reconstruct.** Build from verified constraints only. What emerges may resemble existing solutions or may not. Both are fine. What matters is that every element earns its place.

80% of solving a problem is defining the problem. Orient exists because without it, decomposition has no anchor — you will decompose whatever is in front of you, which is usually the existing system. Orient points decomposition at the right target: the need, not the implementation.

**The descriptive/normative distinction.** There are two kinds of truth relevant to design work. *Descriptive truth* is what currently exists — the code, the configuration, the running system. *Normative truth* is what's actually needed — the requirements, the capabilities, the outcomes that matter. For design work, you ground in normative truth. Descriptive truth is evidence about one implementation, useful for gap analysis after the design exists, but never the starting point. Confusing these — treating what the system currently does as the definition of what it should do — is the most common grounding failure.

This is the opposite of reasoning by analogy. Analogy copies solutions and their embedded assumptions. Grounding derives solutions from constraints and discovers which assumptions were load-bearing.

**Why this matters more for agents:** LLMs exaggerate human cognitive biases — anchoring, confirmation, primacy, status quo. Research shows effect sizes "unusually large," behaving as "caricatures of human cognitive behavior." First information in context disproportionately shapes all subsequent reasoning. Without Orient, the first information is typically the existing system, and everything flows from there. Orient ensures the first information is the need.

---

## Assumed-Constraint Patterns

These fire on all generative work. When you notice any of these, stop and ground.

### 1. Problem-as-Given

Accepting the problem statement without questioning scope, framing, or premises.

**Recognition:** You are optimizing within the frame you were handed. You have not asked whether the frame is correct.
**Corrective:** "What problem are we actually solving? Is this the right question, or the question we were given?"

### 2. Implementation as Requirement

Treating what the current system does as the definition of what it should do.

**Recognition:** Your "requirements" are descriptions of existing behavior. Your design reproduces what the code currently provides rather than what users actually need. You read the implementation and organized your findings instead of asking what the implementation must enable.
**Corrective:** "If this implementation did not exist, what would the users/agents/system need?" Start from the need. Use the existing implementation for gap analysis only after the design exists.

### 3. Category Inheritance

Using existing categories as the skeleton for a new design.

**Recognition:** Your design's structure mirrors an adjacent system or the categories in the request. You did not derive them — you inherited them.
**Corrective:** "What categories would emerge from the requirements alone?" Design the taxonomy fresh. Compare with inherited categories afterward.

### 4. Pattern Matching

Copying a pattern from another context without verifying fit.

**Recognition:** "This is like X, so we should do what X does." The analogy feels natural — perhaps too natural.
**Corrective:** "Does this pattern fit because it is correct for these constraints, or because it is familiar?" Verify the mapping. Identify where the analogy breaks.

### 5. Precedent as Constraint

Treating past decisions or existing implementations as requirements.

**Recognition:** "We did it this way before" or "the existing system does X" appears in your reasoning as constraint rather than data point.
**Corrective:** Precedent is evidence, not constraint. "If this precedent did not exist, what would we build?"

### 6. Complexity Preservation

Maintaining complexity because removing it seems risky.

**Recognition:** You are preserving structure not because it serves current requirements, but because removing it might break something you do not understand.
**Corrective:** "What is the simplest design that meets requirements?" If simpler than what exists, the complexity needs justification or removal.

### 7. Audience Assumption

Designing for the requester, yourself, or an imagined user rather than verified actual users.

**Recognition:** You have not identified who this serves. You are designing for the voice in the prompt.
**Corrective:** "Who actually uses this? What do they actually need?" Ground the audience before grounding the design.

### 8. Abstraction Gravity

Defaulting to the abstraction level of adjacent or existing systems.

**Recognition:** Your design operates at the same abstraction level as the system it replaces or resembles, without questioning whether that level is correct.
**Corrective:** "What abstraction level does this problem actually require?" The existing system's level is a data point, not a default.

### 9. Descriptive-Normative Confusion

Documenting what is instead of designing what's needed.

**Recognition:** Your output reads as a description of the current system rather than a design derived from requirements. Every claim traces to existing code or configuration, none trace to user needs or capability requirements.
**Corrective:** "Am I describing what exists or defining what's needed?" If every statement traces to implementation and none trace to need, you have written a description, not a design. Return to Orient.

---

## Backward-Compatibility Patterns

These fire when existing state creates gravitational pull — migration decisions, upgrades, technology selection. They are assumed-constraint patterns specialized for the preservation instinct.

**1. Fabricated Costs** — Presenting migration costs that do not exist. Before claiming any cost, verify it. Run the command. Read the docs.

**2. Compatibility Layering** — "Support both old and new." Two systems is almost always worse than one clean migration. Pick one. Migrate fully.

**3. Scope Anchoring** — Using the existing solution as design starting point. Start from requirements. Design fresh. Compare only after.

**4. Risk Asymmetry** — Treating change as risky and stasis as safe. Stasis accumulates hidden debt. Evaluate both risks explicitly.

**5. Sunk Cost Protection** — "We already invested in X." Past investment is irrelevant. Evaluate from current state and future value.

**6. Premature Abstraction** — Building flexibility now for hypothetical future migration. Build for today's requirements.

**7. "It Works" as Sufficient** — Working is the minimum bar. "It works and there is no materially better approach today" is the actual defense.

---

## When to Ground

**Always:** Before generating any design — specs, architectures, processes, methodologies, problem framings, solution proposals.

**Specifically:** Technology selection, infrastructure decisions, dependency management, workflow design, API design, category creation, taxonomy design.

**The trigger:** You are about to create something. Ask: "Have I established what this must enable, or am I starting from what already exists?"

## When NOT to Ground

- **Mid-execution.** Finish the current step, then reassess. Grounding fires at decision points.
- **Verified external constraints.** Users at scale, contracts, and regulations are ground truth — they survive decomposition.
- **Diminishing returns.** If grounding produces the same design as the inherited approach, the approach was correct. Grounding is verification, not contrarianism.

---

## Decision Protocol

0. **Orient.** What must this enable? Who does it serve? What do they need? Write this down before proceeding. If you cannot answer these questions, resolve them first — everything downstream depends on this.
1. **Decompose.** What does this actually need to do? List requirements, not features. Derive from the need established in Orient, not from existing implementations.
2. **Surface assumptions.** What are you treating as given? Which are verified? Distinguish descriptive truth (what exists) from normative truth (what's needed).
3. **Design from constraints.** Build what requirements demand.
4. **Compare.** Does this match existing approach? If yes, existing approach is validated. If no: real migration cost vs. carrying cost.
5. **Default to the grounded design.** In early-stage systems, inherited assumptions compound faster than fresh starts.

---

## Corruption Modes

**Skipped Orient.** Jumping straight to decomposition without establishing what this must enable. The need shapes everything. If you decomposed without it, your decomposition targeted the wrong thing.

**Performative grounding.** Going through the motions without questioning. "I considered the requirements and they match what was given." If decomposition always confirms the inherited frame, you are rationalizing, not decomposing.

**Implementation survey as design.** Thorough research of the existing system presented as a design document. The research is valuable — for gap analysis. But organizing implementation facts is not designing. If your output would be equally true as a README for the current system, you have not designed anything.

**Infinite decomposition.** Using grounding to delay decisions. Decomposition serves reconstruction. If you are decomposing without rebuilding, you have stalled.

**Rejection as reflex.** Dismissing all inherited structure because it is inherited. Some precedents are correct. Grounding is verification, not contrarianism.

---

## The Exponential Context

AI-accelerated tooling inverts the historical calculus. Migration cost approaches zero. Carrying cost compounds. Tools improve faster than abstractions age. Nimbleness compounds; rigidity compounds. Choose which curve you are on.

---

*The default is to float — in inherited frames, borrowed categories, precedent as constraint, descriptions of what is. Orient returns you to what is needed. Grounding returns you to what is true. Build from there.*
