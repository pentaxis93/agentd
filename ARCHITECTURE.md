# Architecture

This document captures the design rationale for `agentd`: what capabilities the system must enable, what constraints those capabilities impose, and how those constraints derive the current workspace shape.

## What agentd Is

`agentd` is an autonomous AI agent runtime daemon that orchestrates agent execution on infrastructure you control. The runtime handles lifecycle and coordination concerns, while plugins provide domain-specific capabilities. This keeps `agentd` self-hosted, model-agnostic, and extensible without hard-coding tool domains into the core.

## Agent Capability Needs

The architecture is derived from capability requirements, not implementations. Each need below is written as a requirement and then translated into an architectural constraint.

### Network

The system must allow agents to reach external systems (APIs, services) needed to complete work.

Constraint implication: runtime components must support controlled network connectivity for agent workloads and plugin traffic.

### Credentials

The system must allow agents to authenticate to external services without embedding secrets in code or images.

Constraint implication: the runtime must provide credential injection and isolation boundaries between sessions.

### Identity

The system must let an agent know who it is and preserve that identity across sessions.

Constraint implication: session lifecycle and state handling must keep stable identity context available to the runner.

### Mission

The system must provide agents a durable statement of what they are trying to accomplish and why.

Constraint implication: orchestration must carry mission context through scheduling and execution boundaries.

### Tools

The system must let agents act on the world through tool interfaces.

Constraint implication: architecture must separate a stable tool transport from domain-specific tool implementations.

### Context

The system must expose environment context needed for grounded decisions (repository state, runtime state, platform signals).

Constraint implication: orchestration and plugins must exchange structured context over defined interfaces.

### Skills

The system must support reusable procedural guidance that can be applied across sessions and tasks.

Constraint implication: runtime and plugin integration must allow skill material to be loaded and used without coupling skills to one plugin.

## The Plugin Boundary

The plugin boundary exists to keep core orchestration stable while enabling independent capability growth.

Framework responsibilities:
- Scheduling and execution coordination
- Container/session lifecycle management
- Shared MCP transport
- Session stream parsing and wiring
- Credential injection into execution contexts

Plugin responsibilities:
- Domain-specific tools exposed through MCP servers
- Domain integration logic and API semantics
- Tool contracts specific to a capability domain

The framework is intentionally not opinionated about which tool domains exist. `forgejo-mcp` is the first plugin and demonstrates the boundary for forge operations; additional plugins (for example GitHub, Jira, or home automation) follow the same transport and lifecycle contracts.

## Crate Layout and Constraint Mapping

The workspace is decomposed so lifecycle, scheduling, transport, and domain tools can evolve independently. This decomposition minimizes coupling, allows isolated testing, and keeps plugin replacement or addition local instead of forcing core-runtime changes.

| Crate | Primary responsibility | Needs served | Why this boundary exists |
| --- | --- | --- | --- |
| `agentd` | Composition root and daemon entrypoint | Mission, Context | Keeps top-level orchestration wiring separate from subsystem internals. |
| `agentd-runner` | Agent/session lifecycle management | Identity, Mission, Skills | Isolates lifecycle behavior and session-level invariants. |
| `agentd-scheduler` | Job scheduling primitives | Mission, Context | Decouples planning/execution timing from runner implementation. |
| `mcp-transport` | Shared MCP transport components | Tools, Context | Provides a stable protocol boundary reused by all plugins. |
| `forgejo-mcp` | Forgejo/Gitea MCP plugin | Tools, Network, Credentials | Encapsulates forge-domain behavior without leaking into core crates. |

## What agentd Is Not

- Not a cloud platform; it runs on infrastructure you control.
- Not an AI model; it executes with whatever model backend you configure.
- Not a framework for building agents; it is a runtime for running them.
- Not opinionated about tool domains; plugins provide domain capabilities.

## Verification Matrix

Every architectural decision should trace to a capability requirement and a concrete repository boundary.

| Need/Constraint | Architectural decision | Evidence in workspace | Failure if violated |
| --- | --- | --- | --- |
| Tools require stable transport separated from domain logic | Keep MCP transport in its own crate and domain tools in plugins | `crates/mcp-transport`, `crates/forgejo-mcp` | Domain logic leaks into core, making new plugins costly. |
| Lifecycle and scheduling are distinct concerns | Split runner and scheduler crates | `crates/agentd-runner`, `crates/agentd-scheduler` | Scheduling changes risk breaking session invariants. |
| Core must remain plugin-agnostic | Compose in `agentd` crate, avoid domain APIs in core crate contracts | `crates/agentd` dependencies on abstractions/crates | Core becomes coupled to one tool domain. |
| Credentials and network access must be controlled by runtime boundaries | Runtime handles injection/wiring, plugins consume through contracts | Boundary described in plugin section; plugin crate isolation | Secret handling becomes ad hoc and unsafe. |
| Architecture claims must remain understandable to newcomers | Keep this rationale document aligned with README and AGENTS references | `README.md`, `AGENTS.md`, `ARCHITECTURE.md` | Contributors cannot trace decisions back to requirements. |
