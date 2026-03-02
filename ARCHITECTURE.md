# Architecture

## 1. What agentd Is

agentd is an autonomous AI agent runtime daemon. It runs autonomous AI agents on infrastructure you control, scheduling sessions, constructing isolated execution environments, wiring tools via MCP, and managing credentials.

agentd is:
- **Self-hosted** — runs on your machines, under your control
- **Plugin-based** — agent capabilities come from MCP server plugins, not from the core runtime
- **Model-agnostic** — the runtime manages execution environments, not model inference

agentd is not:
- **A cloud platform** — there is no hosted service; you run it
- **An AI model** — it orchestrates agent sessions, it does not perform inference
- **A framework for building agents** — it provides the runtime environment agents execute in, not an SDK for writing agent logic
- **Opinionated about tool domains** — domain-specific capabilities (code forges, databases, APIs) live in plugins

## 2. Agent Capability Needs

Every architectural decision in agentd traces to a capability that agents need in order to do useful work. These seven needs are the grounding layer — the rest of the document derives from them.

### Network

**Need:** Agents reach external services — APIs, code forges, databases, web endpoints.

**Constraint:** The runtime provides network access from within the execution environment. Network policy controls what an agent can reach, balancing capability against isolation.

### Credentials

**Need:** Agents authenticate to external services — API tokens, SSH keys, platform passwords.

**Constraint:** The runtime injects credentials into the agent's environment without baking them into images or exposing them in code. Each agent's credentials are isolated from every other agent's.

### Identity

**Need:** Agents know who they are and retain state across sessions.

**Constraint:** Each agent has a persistent home directory, a declared name, and environment variables establishing identity. State written to the home directory survives session boundaries.

### Mission

**Need:** Agents know what to do — what triggered this session, what the objective is.

**Constraint:** The runtime provides session context and any mission-specific parameters. Scheduling configuration determines when agents run.

### Tools

**Need:** Agents act on the world — create pull requests, query databases, send messages, modify infrastructure.

**Constraint:** CLI tools are installed in the container image and available directly. Domain-specific tools are provided by MCP server plugins, discovered and wired by the runtime at session setup.

### Context

**Need:** Agents understand their operating environment — platform documentation, shared configuration, deployment-specific data.

**Constraint:** The runtime mounts deployment-specific read-only data into the agent's environment. Agents can read this context but cannot modify it.

### Skills

**Need:** Agents have reusable capabilities — workflows, procedures, domain knowledge packaged for re-use across sessions and agents.

**Constraint:** The runtime loads skills into the agent's environment during session setup. Skills are files on disk, available to the agent's tooling, not compiled into the runtime.

## 3. Crate Layout and Constraint Mapping

The workspace is organized so that each crate owns a distinct concern. Boundaries exist where responsibility, dependency direction, or rate of change differs.

| Crate | Responsibility | Needs Served | Boundary Rationale |
|---|---|---|---|
| `agentd` | Composition root. Parses configuration, assembles components, starts the daemon. | All (orchestration) | Wiring logic changes independently of any single subsystem. Keeps the binary crate thin. |
| `agentd-runner` | Session lifecycle. Constructs containers, injects identity and credentials, wires MCP servers, manages execution, persists state on teardown. | Identity, Credentials, Mission, Tools, Context, Skills | The largest concentration of capability-need logic. Isolated so session mechanics can evolve without touching scheduling or transport. |
| `agentd-scheduler` | Job scheduling. Determines when agents run based on cron expressions, events, or manual triggers. | Mission | Scheduling policy changes independently of session execution. Different deployments may need different scheduling strategies. |
| `mcp-transport` | Shared MCP protocol. Handles stdio JSON-RPC framing, message serialization, and transport mechanics for communication between the runner and MCP server plugins. | Tools | Protocol logic is shared across all plugins. Isolating it prevents each plugin from reimplementing transport. |
| `forgejo-mcp` | First plugin. MCP server providing Forgejo/Gitea domain tools — repository operations, issue management, pull requests. | Tools | Domain-specific logic lives outside the core runtime. Demonstrates the plugin pattern that other MCP servers follow. |

**Reading the table:** To determine which crate to modify for a change, identify which capability need the change serves, then find the crate that owns that need. If the change is about *when* agents run, it belongs in `agentd-scheduler`. If it's about *how* sessions execute, it belongs in `agentd-runner`. If it's about *what tools* an agent has, it's either `mcp-transport` (protocol) or a plugin crate (domain logic).

## 4. The Plugin Boundary

This section is written for plugin authors — people building MCP server plugins that give agents new capabilities.

### What a plugin is

A plugin is an MCP server: a process that speaks the Model Context Protocol over stdio, exposing tools that agents can invoke during sessions. The plugin handles domain logic (talking to a Forgejo API, querying a database, managing infrastructure). The runtime handles everything else.

### What the runtime provides

The runtime is responsible for:
- **Discovery** — reading the agent's configuration to determine which MCP servers to start for a session
- **Process lifecycle** — starting the MCP server process at session setup and stopping it at teardown
- **Transport wiring** — connecting the MCP server's stdio to the agent's session via JSON-RPC, using the `mcp-transport` crate
- **Credential delivery** — injecting the plugin's required credentials into its environment before it starts
- **Tool registration** — presenting the plugin's declared tools to the agent so the agent can invoke them

### What a plugin provides

A plugin is responsible for:
- **An MCP server binary** — a process that reads JSON-RPC requests from stdin and writes responses to stdout
- **Tool declarations** — the set of tools the plugin exposes, with names, parameter schemas, and descriptions
- **Domain logic** — the implementation behind each tool (API calls, data transformations, side effects)
- **Credential requirements** — declaring what credentials it needs (the runtime delivers them, the plugin consumes them)

### Plugin lifecycle

1. **Start** — The runner starts the MCP server process as part of session setup. Credentials the plugin declared as required are available in its environment.
2. **Serve** — The plugin receives tool-call requests via stdin (JSON-RPC) and returns results via stdout. It runs for the duration of the agent's session.
3. **Stop** — The runner terminates the MCP server process at session teardown. No cleanup protocol — the process ends.

### Transport protocol

All communication between the runtime and MCP server plugins uses JSON-RPC over stdio, handled by the `mcp-transport` crate. Plugins do not open network listeners. The runtime connects to them through their stdin/stdout file descriptors.

## 5. Session Lifecycle

A session is a single execution of an agent — from trigger to teardown. Four phases, each naming the responsible crate.

### Phase 1: Scheduling (`agentd-scheduler`)

The scheduler determines when to invoke an agent. Triggers include cron-based schedules, external events, and manual invocation. When a trigger fires, the scheduler passes the agent's identity and trigger context to the runner.

**Need served:** Mission — the agent knows *why* this session exists.

### Phase 2: Session Setup (`agentd-runner`)

The runner constructs the agent's execution environment:

1. **Container construction** — A Podman container is created from the agent's configured base image.
2. **Identity injection** — The agent's name, home directory, and identity environment variables are set. The persistent home directory is mounted.
3. **Credential mounting** — Secrets configured for the agent are injected into the container's environment. Each agent receives only its own credentials.
4. **Context mounting** — Deployment-specific read-only data (documentation, shared configuration) is mounted into the container.
5. **Skill loading** — Skill files are placed into the agent's environment where its tooling can discover them.
6. **MCP server wiring** — The runner starts each MCP server plugin configured for the agent, wires their stdio through `mcp-transport`, and registers their tools.

**Needs served:** Identity, Credentials, Context, Skills, Tools.

### Phase 3: Execution (`agentd-runner` + `mcp-transport`)

The agent's session runs inside the container. When the agent invokes a tool, the request flows through `mcp-transport` to the appropriate MCP server plugin, and the response returns the same way. The runner monitors the session stream.

**Needs served:** Tools, Network, Mission.

### Phase 4: Teardown (`agentd-runner`)

When the session completes (or times out):

1. **State persistence** — The agent's home directory already persists on the host filesystem. Any state the agent wrote during the session survives.
2. **MCP server shutdown** — Plugin processes are terminated.
3. **Container cleanup** — The container is removed.

**Needs served:** Identity (persistence across sessions).

## 6. Container Isolation Model

This section is written from the agent's perspective — what the world looks like from inside the container.

### Why containers

agentd uses Podman containers for agent execution. Containers provide:
- **Security boundary** — an agent's processes are isolated from the host and from other agents
- **Reproducibility** — the same base image produces the same starting environment
- **Credential isolation** — secrets are scoped to a single container, inaccessible to other agents

### What gets mounted

| Mount | Type | Source | Purpose | Need |
|---|---|---|---|---|
| Home directory | Read-write volume | Host path per agent | Persistent identity, state, working files | Identity |
| Credentials | Secrets / env vars | Host secret store | API tokens, SSH keys, passwords | Credentials |
| Context | Read-only bind mount | Deployment-specific path | Documentation, shared configuration | Context |
| Skills | Read-only bind mount | Skill directories | Reusable workflows and procedures | Skills |

### The agent's view from inside

The agent sees a Linux environment with:

| Guarantee | Detail |
|---|---|
| `HOME` env var | `/home/{agent_name}` — persistent across sessions |
| `AGENT_NAME` env var | The agent's declared name |
| XDG directories | `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` — all under `$HOME` |
| Writability | Full read-write access to `$HOME` and everything under it |
| Tools | MCP server plugins are wired and available through the agent's session |
| Skills | Loaded into the environment at paths the agent's tooling can discover |

Sessions that exceed the configured timeout are killed. Work persisted to the home directory before timeout survives.

### Network policy

Agents have network access from within the container, controlled by deployment-specific policy. The runtime does not impose a single network model — operators configure network rules appropriate to their environment and trust model.

## 7. Credential Flow

Credentials move from configuration to execution environment without appearing in code, images, or logs.

### Where credentials are configured

Agent configuration declares what credentials each agent needs. Credential values are stored in a secret store on the host, separate from agent configuration files.

### How the runner injects them

During session setup, the runner reads the agent's declared credential requirements, retrieves the corresponding values from the host secret store, and injects them into the container as environment variables or mounted secret files. This happens before the agent's session begins — credentials are available from the first moment of execution.

MCP server plugins that require credentials receive them the same way: the runner injects plugin-specific credentials into the plugin's process environment before starting it.

### Isolation between agents

Each agent's container receives only the credentials declared in its own configuration. There is no shared credential namespace. One agent cannot access another agent's secrets. If two agents need access to the same service, each has its own credential entry — compromise of one agent's environment does not expose another's.

## 8. Verification Matrix

Every architectural decision traces to a capability need. If the decision were reversed, the corresponding capability would break.

| Need | Architectural Decision | Evidence in Workspace | Failure if Violated |
|---|---|---|---|
| Network | Containers have configurable network access | `agentd-runner` owns container construction | Agents cannot reach external services |
| Credentials | Runner injects secrets from host store into container env | `agentd-runner` owns credential mounting; per-agent isolation | Agents cannot authenticate; credential leakage between agents |
| Identity | Persistent home directory mounted per agent; `AGENT_NAME` and `HOME` env vars set | `agentd-runner` owns identity injection | Agents lose state across sessions; agents cannot self-identify |
| Mission | Scheduler passes trigger context to runner; runner injects session context | `agentd-scheduler` triggers sessions; `agentd-runner` constructs context | Agents run without knowing why or what to do |
| Tools | MCP server plugins wired via `mcp-transport`; stdio JSON-RPC | `mcp-transport` crate; `forgejo-mcp` as reference plugin | Agents cannot act on the world; no tool invocation path |
| Context | Read-only deployment-specific data mounted into container | `agentd-runner` owns context mounting | Agents operate without environmental awareness |
| Skills | Skill files loaded into agent environment during session setup | `agentd-runner` owns skill loading | Agents lack reusable capabilities; every session starts from scratch |
| Plugin separation | Domain logic in plugin crates, protocol in `mcp-transport`, lifecycle in `agentd-runner` | Crate boundaries in `Cargo.toml` workspace | Adding a domain requires modifying core runtime; transport reimplemented per plugin |
| Scheduling independence | `agentd-scheduler` owns trigger logic, separate from session execution | Crate boundary between `agentd-scheduler` and `agentd-runner` | Changing scheduling policy requires modifying session lifecycle code |
