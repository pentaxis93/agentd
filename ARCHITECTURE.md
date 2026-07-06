# Architecture

## 1. What agentd Is

agentd is an autonomous AI agent runtime daemon. It runs agent sessions on infrastructure you control, decides when they run, prepares isolated execution environments, injects identity and credentials, and supervises execution from start to teardown.

agentd is:
- **Self-hosted**: operators run it on their own infrastructure
- **Runtime-oriented**: it prepares and supervises agent sessions rather than providing model inference
- **Modular**: scheduling and session execution evolve behind separate crate boundaries

agentd is not:
- **A hosted platform**: there is no control plane operated elsewhere
- **An AI model**: inference belongs to the chosen runtime or provider
- **An MCP transport layer**: MCP-capable runtimes already speak MCP directly
- **An in-tree domain tool suite**: domain integrations live outside this workspace

The key architectural consequence is simple: agentd may configure tool availability for a runtime, but it does not proxy the MCP wire protocol or ship domain-specific MCP servers inside this repository.

Platform contract: `agentd-runner` targets Linux only. Non-Linux compilation
failure is intentional and matches the runtime contract: rootless Podman,
systemd user services, SELinux-aware host filesystem handling, and Linux UID
mapping semantics are all part of the supported execution model.

### Deployment Shape

The supported daemon deployment is a Quadlet-managed container image. The
containerized daemon is still the same foreground `agentd daemon` process: it
parses one config file, owns the Unix socket intake, schedules work, and
dispatches sessions into `agentd-runner`. Quadlet supervises the persistent
daemon container only. Session containers remain normal short-lived
agentd-runner Podman workloads and are not represented as Quadlet units.

The daemon image includes the Podman client and talks to the host's rootless
Podman service through a mounted Podman socket. In the repository image, the
client is configured with `CONTAINER_HOST=unix:///run/podman/podman.sock`, so
deployment mounts the host user's Podman socket to that in-container path. The
daemon does not run a nested Podman service and does not own a second container
storage graph.

Path responsibilities split across the daemon container and the host Podman
service:

| Path class | Who opens it | Deployment requirement |
|---|---|---|
| Daemon config | daemon process | Mount the config at the documented in-image config path. |
| Daemon socket and PID file | daemon process; host clients use the socket through the host mount | Configure explicit daemon runtime paths inside the container and mount their parent directory from the host runtime location. |
| Host Podman socket | daemon process through the Podman client | Mount the host rootless Podman socket to the path named by `CONTAINER_HOST`. |
| Credential sources | daemon process environment | Inject the environment variables named by agent credentials into the daemon container. |
| Active forge type | agent config | Set one forge type per session; omitted values default to `github` and are injected as `GROUNDWORK_FORGE_TYPE`. |
| `methodology_dir` and request/artifact schemas | daemon process reads; host Podman resolves direct relabelled methodology source | The path must exist for the daemon and be valid from the host Podman service's filesystem view. |
| `audit_root` | daemon process creates/chmods; host Podman resolves direct relabelled audit sources; session containers write audit entries | Mount durable host storage at the configured audit root, with the daemon UID aligned to the session writer's mapped host UID or granted equivalent chmod authority. |
| Agent-declared `mounts.source` | daemon process canonicalizes; host Podman resolves staged bind sources | Mount or expose each source so the daemon and host Podman agree on the source path. |
| Runner staging directory | daemon process writes; host Podman resolves staged bind sources | The path named by `TMPDIR` must exist at the same absolute path for the daemon container and host Podman. The image default requires host `/var/lib/agentd/tmp` mounted to container `/var/lib/agentd/tmp`, or a changed `TMPDIR` exposed identically on both sides. |

The path split follows directly from the current runner implementation. The
daemon process validates and canonicalizes methodology, audit,
invocation-input, and staging paths, then sends bind-mount source
strings to the host Podman service. Runner-owned relabelled sources are passed
as direct canonical host paths so Podman's SELinux relabel operation applies to
the real source tree; operator-declared mounts continue to use staged aliases.
Operator-declared additional-mount sources are not canonicalized in the daemon
namespace: the alias points at the source as declared, and existence is
resolved by the host Podman that performs the mount, so a host source absent
from the daemon container's own filesystem view is still accepted.
Runner-owned relabelled sources containing both `,` and `:` are rejected
because neither Podman's comma-delimited `--mount` form nor its colon-delimited
`--volume` fallback can encode that path unambiguously.
Podman resolves those source strings on the host that owns the socket, not
inside the daemon container. Any future
host/container path translation would be a code change, not a deployment
property.

### Terminology

- **Agent**: a named, reusable environment specification in the daemon config — base image, methodology, optional active forge type, optional default repo, optional schedule, credentials, and command. What the operator declares.
- **Session**: a single execution created from an agent plus invocation parameters (repo, work unit, timeout). What the runner manages.

## 2. Agent Capability Needs

Every structural decision in the workspace traces to a capability an agent session must eventually have.

### Network

**Need:** agents reach external services such as APIs, code forges, and web endpoints.

**Constraint:** the execution environment provides network access under deployment-specific policy.

### Credentials

**Need:** agents authenticate to those external services.

**Constraint:** credentials are injected at session setup and remain scoped to the owning agent.

### Identity

**Need:** agents know who they are and can be distinguished clearly per session.

**Constraint:** each session receives stable identity variables and an agent-derived container name inside an ephemeral runtime.

### Mission

**Need:** agents know why this session exists and what objective it serves.

**Constraint:** scheduling and invocation context flow into the session at launch time.

### Tool Availability

**Need:** agents can act on the world through installed tools and runtime integrations.

**Constraint:** CLI tools are present in the image or mounted environment, and any MCP-capable runtime receives configuration pointing at the external servers it should use. The runtime handles protocol communication directly.

### Context

**Need:** agents understand deployment-specific facts such as documentation, shared configuration, and local policy.

**Constraint:** context is mounted read-only into the execution environment.

## 3. Workspace Boundaries

The workspace contains three crates because there are three distinct rates of change and responsibility centers.

| Crate | Responsibility | Needs Served | Boundary Rationale |
|---|---|---|---|
| `agentd` | Composition root and daemon entrypoint. Parses configuration, owns the Unix socket intake, assembles runner and scheduler components, and starts the process. | All, as orchestration | Keeps the binary thin and prevents subsystem concerns from collapsing into the entrypoint while preserving one uniform dispatch path into session execution. |
| `agentd-runner` | Session lifecycle. Creates execution environments, injects identity, credentials, context, and tool configuration, launches runtimes, and tears sessions down. | Identity, Credentials, Mission, Tool Availability, Context, Network policy application | Session mechanics change independently of scheduling policy and should remain isolated. |
| `agentd-scheduler` | Triggering and timing. Determines when a session should start and with what mission context. | Mission | Scheduling policy has its own evolution path and should not be coupled to session setup mechanics. |

**Reading the table:** if the change is about when work starts, it belongs in `agentd-scheduler`. If it is about how a session is prepared, launched, or cleaned up, it belongs in `agentd-runner`. If it is about wiring the whole daemon together, it belongs in `agentd`.

## 4. Session Lifecycle

A session is one execution created from an agent, spanning trigger to teardown.

Before the daemon accepts any session trigger, it first reconciles stale
runner-managed Podman resources from prior runs of the same daemon instance.
Dead session containers named `agentd-{daemon8}-{agent}-{session16}` are
removed, then orphaned runner-managed secrets named
`agentd-{daemon8}-{session16}-{suffix}` whose session container no longer
exists are removed. The daemon instance id is derived from the configured
socket and PID paths, so distinct runtime-path pairs on the same host keep
separate ownership scopes. Only after that cleanup succeeds does the daemon
bind its Unix socket and begin accepting requests.

The daemon's Unix socket is the single intake for all session triggers. Manual
CLI invocation connects to that socket as a client. In the supported
containerized daemon deployment, the socket is created inside the daemon
container at the configured path and exposed to the host through a bind mount.
Scheduling policy also connects to that socket as a client when it decides work
should start. The daemon accepts those run requests uniformly and dispatches
them into session execution. In `v0.1.x` this socket protocol is internal and unversioned:
daemon and CLI must be the same build, and replacing the daemon image requires
restarting the daemon before new CLI invocations are supported.

Manual CLI dispatch is intentionally decoupled from daemon-owned config files.
`agentd run` resolves the daemon through a socket path rather than by reading
`agentd.toml`: explicit `--socket-path` wins, otherwise the client resolves the
single default path `$XDG_RUNTIME_DIR/agentd/agentd.sock`. Daemon defaults use
the same XDG construction for the socket and PID file, so client and daemon
agree by construction rather than by probing a candidate list. If
`XDG_RUNTIME_DIR` is unset, empty, or relative, the default path is unavailable
and operators must either fix the XDG runtime environment or provide explicit
paths. There is no implicit `/tmp` or `/run` fallback. Agent lookup and
default-repo resolution happen daemon-side after the socket request is
received, so client and daemon responsibility boundaries stay clean.
While a manual `agentd wish` or `agentd run` request is in flight, the daemon
may send progress frames over that same client connection before the terminal
session outcome. The runner tails the active transcript event stream it already
owns under runa's nested per-stage transcript event store, and the daemon
forwards those events to the launching client while the session executes. The
CLI renders those frames according to the operator's selected progress level,
so the launching terminal can observe the session without knowing container
names, daemon log layout, or audit-record paths.

Operational visibility for that lifecycle uses structured tracing events written
to stderr. The production default is timestamped JSON lines at `info` so
operators can monitor normal session start, outcome, teardown, and lifecycle
failures mechanically without extra log-filter setup; callers that invoke the
runner without installing tracing still retain direct stderr diagnostics for
failure paths. Local development can switch to a human-readable format through
environment configuration.

### Phase 1: Scheduling (`agentd-scheduler`)

The scheduler determines when a session should run. Today it evaluates each
agent's optional cron schedule in daemon-local time. When scheduling decides
to start a session, the scheduler is a socket client: it dispatches a run
request through the daemon's Unix socket, using the same intake path as manual
CLI invocation. The scheduler does not call the runner directly. Missed
occurrences while the daemon is down are not backfilled, and session outcomes
do not influence later schedule evaluation.

### Phase 2: Session Setup (`agentd-runner`)

The runner prepares the execution environment:

1. Creates an ephemeral Podman container from the agent's configured base image. That image must provide a POSIX-compatible shell at `/bin/sh` because the runner's container entrypoint executes through that path.
2. Sets identity inside the container, including `AGENT_NAME` and a unique container name derived from the agent.
3. Injects caller-resolved credentials as environment variables for that session only via Podman-managed secrets rather than inline CLI arguments, and injects the non-secret active forge type as `GROUNDWORK_FORGE_TYPE`.
4. Mounts the configured methodology directory read-only.
5. Creates an unprivileged unix user whose username is the configured agent name, with home directory `/home/{username}` and UID/GID `1000`, and clones the requested repository into `/home/{username}/repo`. This clone step is a plain in-container `git clone`: the base image must provide `git`, `find`, `groupadd`, `useradd`, and `gosu` in `PATH`, it accepts `https://`, `http://`, `git://`, `ssh://`, and `user@host:path` SSH repository URLs, rejects credential-bearing URLs up front, and can authenticate private HTTPS clones with an invocation-scoped bearer `repo_token`. SSH clone runs as the session user with `HOME=/home/{username}` so OpenSSH reads agent-scoped mounted material under that home, typically `/home/{username}/.ssh`; it does not use `repo_token_source`. The token is injected through a Podman secret, converted into one-shot git configuration for the clone process only, and removed before `runa init` and the agent command start. Base images that lack `/bin/sh`, `find`, `git`, `groupadd`, `useradd`, `gosu`, or `runa`, or that cannot reserve UID/GID `1000` for the session user, are not supported. SSH clone additionally requires an OpenSSH-compatible client in the base image.
6. Resolves the host audit root, creates it if needed, and probes writability before accepting work. The default for rootless deployments is `$XDG_STATE_HOME/tesserine/audit`, falling back to `$HOME/.local/state/tesserine/audit` when `XDG_STATE_HOME` is unset. Operators may override that with `daemon.audit_root`; root-owned system installs should typically point it at `/var/lib/tesserine/audit`. After resolution, the runner allocates a host audit record at `{audit_root}/{agent}/{session_id}/`, writes start metadata to `agentd/session.json`, bind-mounts the `runa/` subtree into the container at `/home/{username}/.agentd/audit/runa`, and bind-mounts `agentd/transcript/` at `/agentd/transcript` before the runtime initializes runa state.
7. Recursively transfers ownership of pre-existing content under `/home/{username}` while pruning host-backed bind-mount targets, the runner-owned audit leaf `/home/{username}/.agentd/audit/runa`, and `/home/{username}/repo`, then transfers ownership of `/home/{username}/repo` after the clone, sets `HOME=/home/{username}`, and keeps setup privileged only until the workspace is ready. The runner reserves `/home/{username}` itself, `/home/{username}/.agentd` plus its descendants, and `/home/{username}/repo` plus its descendants so host-backed bind mounts cannot collide with runner-managed paths.
8. When manual invocation includes operator input, reads the active methodology's `manifest.toml` plus `schemas/<type>.schema.json` on the host, validates the input there, stages the final JSON under a runner-managed bind mount at `/agentd/invocation-input`, and rejects unsupported intent canonical versions before any container is created. The runner-created invocation-input mount source is staged with explicit host-side read/traverse modes so session-user access does not depend on the daemon process's umask. In `v0.1.x`, text input supports canonical intent version `2.0.0` only, keyed by `x-tesserine-canonical.version`; `agentd wish` can author `{statement, target, source}` from its optional target prompt, while `agentd run --intent` authors `{statement, source}` and has no target flag.
9. Creates `/home/{username}/repo/.runa` as a symlink to `/home/{username}/.agentd/audit/runa`; writability comes from the host-side active `runa/` mode established during audit-record allocation, not from container-side ownership transfer. This is a runner-owned repo contract: cloned repositories must not contain a `.runa` entry at repo root. If the clone already contains one, setup fails explicitly rather than overwriting repository content.
10. Invokes `runa init --methodology /agentd/methodology/manifest.toml` as the session user. Runa owns `.runa/` file formats; agentd does not write `.runa/config.toml` or an `[agent]` section.
11. When staged invocation input is present, copies it into `.runa/workspace/<type>/<id>.json` after runa initialization and before launching the agent command.

### Phase 3: Execution (`agentd-runner`)

The runner drops privileges with `gosu` and launches `runa run [--work-unit <id>] --agent-command -- <argv>` as the unprivileged session user from `/home/{username}/repo`. The argv comes from the declarative agent command, and `runa run` owns protocol execution from there. Tool invocations happen directly from the runtime to installed CLIs or configured external MCP servers; agentd does not sit in the middle of that protocol exchange. agentd sets `RUNA_TRANSCRIPT_DIR=/agentd/transcript`, `RUNA_TRANSCRIPT_DEPLOYMENT`, `RUNA_TRANSCRIPT_RUN_ID`, and `RUNA_TRANSCRIPT_REDACT_ENV` so runa can persist the execution events it observes at an agentd-addressable nested path without linking agentd to runa internals.

### Phase 4: Teardown (`agentd-runner`)

When the session ends or times out, the runner first force-removes the
container. Only after cleanup succeeds does it render transcript artifacts and
finalize `agentd/session.json` with end timestamp and outcome through an
atomic same-directory temp-file rename. Before that publish step it seals
persisted non-metadata audit entries read-only on the host, then publishes a
read-only `session.json` as the final commit point. Ancestor directories remain
writable because the atomic replace requires a writable parent directory. The
ephemeral container workspace still disappears, but the host audit record
remains at
`{audit_root}/{agent}/{session_id}/`.

If agentd is interrupted after writing start metadata but before finalization,
if teardown cleanup fails before finalization can begin, or if audit
finalization attempts closeout and fails, the session record remains
**incomplete**: `agentd/session.json` has `start_timestamp` but no
`end_timestamp` or `outcome`. The filesystem alone does not distinguish
"cleanup never completed" from "finalization attempted and failed." Operators
should use tracing to disambiguate when it matters:
`runner.lifecycle_failure` reports the failing stage (`"session resource allocation"`,
`"container creation"`, `"session execution"`, or `"session audit finalization"`),
while `runner.session_outcome`, `runner.session_error`, and
`runner.session_teardown` provide the semantic outcome and teardown status.
On disk, all of those failure paths intentionally preserve the same
incomplete-record signal rather than inventing multiple partially-finalized
states.

## 5. Container Isolation Model

agentd runs sessions in ephemeral Podman containers so agents remain separated from the host and from one another.

| Mount or Injection | Purpose | Need Served |
|---|---|---|
| Read-only methodology directory | Expose the configured methodology manifest and protocol assets without allowing mutation | Context |
| Read-only invocation input mount at `/agentd/invocation-input` | Carry validated operator-supplied JSON into the container so the runner can materialize it in `.runa/workspace` after `runa init` and before `runa run` | Mission |
| Runner-owned audit bind mount at `/home/{username}/.agentd/audit/runa` | Persist runa state on the host while keeping agentd metadata distinct in the same session record | Context, Mission |
| Agent-declared bind mounts | Expose host-managed state such as subscription auth or persistent artifact storage with per-mount read-only vs read-write policy | Context, Credentials |
| Credentials | Authenticate to external systems without baking secrets into images | Credentials |
| Home workspace at `/home/{username}` with repo at `/home/{username}/repo` | Give the session a writable standard Linux home and a clean project workspace that starts fresh each run | Mission, Tool Availability, Identity |

From inside the environment, an agent should see:
- identity-related environment variables
- `GROUNDWORK_FORGE_TYPE`, the active forge type Groundwork reads to resolve forge-specific mechanics
- `$HOME` set to `/home/{username}`
- a read-only methodology mount rooted at `manifest.toml`
- a read-only invocation-input mount at `/agentd/invocation-input` when manual input is supplied
- a runner-managed audit bridge at `/home/{username}/repo/.runa -> /home/{username}/.agentd/audit/runa`
- any additional bind mounts declared by the selected agent
- a fresh writable repository checkout at `/home/{username}/repo`
- any runtime-managed state runa or the configured agent command creates inside the repo or home directory
- the tools and runtime configuration needed for its assigned work

Additional bind mounts are declared in agent configuration as `source`,
`target`, and `read_only`. agentd validates absolute container targets plus a
per-agent disjointness invariant: target paths must be unique and no target
may be a path-component prefix of another. It then stages operator-declared
host sources through runner-managed symlinks before calling Podman so additional
mounts stay separate from runner-owned relabel handling; the host Podman
resolves each aliased source at mount time, so a source that exists on the host
but not in the daemon container's own namespace is accepted. Subscription auth is
the first read-only consumer of this mechanism; persistent audit storage in
`#76` builds on the same path with read-write mounts. Additional mounts are not
relabelled; on SELinux-enabled hosts, operators must pre-label those host paths
with a container-compatible context.

The invocation-input mount is runner-owned, not operator-owned. Its target
`/agentd/invocation-input` is reserved alongside `/agentd/methodology` and
`/agentd/transcript`, so agent-declared mounts cannot collide with
runner-managed input or transcript paths.

The internal audit mount is different from operator-declared mounts. It is
runner-owned, not operator-owned, and agentd applies shared SELinux relabeling
to the canonical host `runa/` source so the persisted `runa/` subtree remains
writable on SELinux-enforcing hosts such as Fedora CoreOS. `agentd/session.json` is not
mounted into the container; it stays host-only so runa-written state and
agentd-written metadata are distinguishable on disk without disambiguation.
Session containers use `--userns keep-id:uid=1000,gid=1000`, and agentd creates
the in-container agent user at UID/GID `1000`, so the audit bind mount appears
owned by the same unprivileged identity that invokes `runa init`.

Final audit sealing is daemon-local. agentd uses direct filesystem operations
to refuse unsafe multi-linked entries and chmod completed audit records; it
does not invoke a Podman user-namespace helper. The startup audit-root probe
verifies the daemon can create, chmod, restore, and remove probe entries it owns
under the resolved audit root. That probe catches local path and permission
failures, but it does not prove authority over future session-written files.
The deployment contract supplies that second half: files written by the session
container's unprivileged agent user must be owned by an identity the daemon can
chmod.

The primary supported contract is daemon identity alignment through Podman's
`keep-id` user namespace mode. The in-container session user is UID/GID `1000`,
and that ID maps to the daemon's host UID/GID. That keeps runa's state
ownership preflight and agentd's daemon-local audit sealing authority aligned
without session-side audit mount chown. Mount-level idmapped bind mounts would
fit this ownership mismatch more narrowly, but the supported daemon image uses
Debian Bookworm's Podman 4.3 client and current rootless bind mounts do not
provide that surface. The `keep-id` contract is therefore an explicit
single-tenant security tradeoff: compared with default rootless subuid mapping,
a session-user escape has the daemon's host-file authority over daemon-owned
paths. The daemon identity must also retain access to the mounted Podman socket
and the configured runtime paths.

Host audit records live under the resolved audit root, by default
`$XDG_STATE_HOME/tesserine/audit/<agent>/<session_id>/` or
`$HOME/.local/state/tesserine/audit/<agent>/<session_id>/` when
`XDG_STATE_HOME` is unset. Root-owned system installs should set
`daemon.audit_root = "/var/lib/tesserine/audit"`. Each record has this
layout:

- `runa/` — preserved runa state written naturally by the runtime
- `agentd/transcript/deployments/<deployment>/work-units/<work-unit>/runs/<run-id>/events.jsonl` — structured transcript events emitted by
  runa and runa-mcp when observable; deployment and run id are agentd-owned
  values injected through the runa environment. agentd opens this nested path
  component by component from the trusted transcript base without following
  symlinked ancestors.
- `agentd/transcript/transcript.md` — human-readable rendering of the event
  stream
- `agentd/transcript/manifest.json` — transcript manifest schema version,
  event schema versions, and coverage: `full`, `missing_mcp_events`,
  `no_events`, or `finalization_failed`
- `agentd/session.json` — agentd-written metadata (`schema_version: 2`,
  `session_id`, `agent`, `repo_url`, optional `work_unit`, timestamps, outcome,
  exit code when applicable) written by atomic temp-file replacement within the
  record directory

Audit state coverage is intentionally scoped to the repo-root `.runa/` tree.
That captures `runa`'s non-configurable `.runa/store/` and `.runa/workspace/`,
so persisted runtime state stays inside the audit mount. Transcript coverage is
separate: it captures runa's observable execution boundary and only includes
MCP tool events when the agent runtime actually launches `runa-mcp`.

Retention is intentionally out of scope here. Audit records accumulate
indefinitely under the resolved audit root; pruning and retention policy are
future work, so disk growth is currently an operator concern. Completed records
seal directories to `0555` and non-symlink entries to `0444`, so deleting old
records requires restoring write permission first, for example
`chmod -R u+w <record_dir> && rm -rf <record_dir>`.

The host security model is intentionally single-tenant. While a session is
running, agentd opens the mounted `runa/` subtree with mode `0o777` so writes
through the session user's mapped daemon identity succeed. Any user with host
shell access can therefore read or write that subtree during the active
session. On completion, agentd seals directories to `0555` and non-symlink
entries to `0444`, making finished records world-readable on the host. That
tradeoff is acceptable only for single-tenant deployments; a multi-tenant host
would need a different permission model before deployment.

The startup audit-root probe is intentionally local-filesystem scoped. It
verifies that the daemon can create, chmod, restore, and remove daemon-owned
probe entries under the resolved audit root before dispatch begins. That catches
ordinary permission and path errors early, but it does not validate authority
over future session-written files or network-filesystem behavior beyond the
probe; NFS and similar targets can still fail later with semantics the probe
does not model. If probe cleanup fails, the audit root can retain that uniquely
named probe tree as leftover cruft.

Session ids are 16 lowercase hex characters generated from `getrandom`, giving
roughly `2^64` possible values per agent and a birthday bound near `2^32`
sessions before collisions become materially likely. On collision,
`create_dir_all` would silently reuse the existing directory tree and merge two
records. That is not an operational concern at current scale, but operators
planning very long-lived or very high-volume deployments should understand the
risk envelope.

## 6. Credential Flow

Credentials are declared by agent configuration as daemon-side environment variable names. For each configured credential, the daemon resolves `source` with `std::env::var(source)` from its own process environment before calling `agentd-runner`. Operators provide those values to the daemon container through normal container environment injection, such as Quadlet environment directives or mounted environment files. During session setup, the runner receives only the already-resolved credential values, creates Podman-managed ephemeral secrets for non-empty values, and injects those values into the execution environment as environment variables without placing the secret values on the Podman command line. Empty assignments are injected directly as `NAME=` because Podman secrets reject zero-byte payloads. Once the container reaches the running state, the runner removes the backing Podman secret objects and relies on the in-container environment copy for the rest of the session.

Because startup reconciliation is scoped per daemon instance rather than to the
whole Podman namespace, the daemon removes only runner-managed resources whose
names carry its own derived daemon id: dead
`agentd-{daemon8}-{agent}-{session16}` containers are removed first, then
orphaned `agentd-{daemon8}-{session16}-{suffix}` secrets whose session
container is gone.

Repository clone authentication is a separate invocation concern rather than an agent runtime credential. When an agent declares `repo_token_source`, the daemon resolves that environment variable at dispatch time and, when the resolved value is non-empty, maps it to `SessionInvocation.repo_token`. The runner then injects that bearer token through its own ephemeral secret, uses it only for HTTPS `git clone`, and unsets the internal token variable before `runa init` or the agent command starts so the token does not persist in git config or the agent runtime environment. SSH clone authentication comes from agent-scoped file material mounted into the session, not from environment variables or `repo_token_source`.

Isolation is per agent: one agent receives only its own declared credentials. Sharing access to the same external service still requires separate credential declarations per agent so compromise remains scoped.

## 7. Verification Matrix

| Need | Architectural Decision | Workspace Evidence | Failure if Violated |
|---|---|---|---|
| Network | Session environments receive deployment-controlled network access | `agentd-runner` owns session setup | Agents cannot reach external services |
| Credentials | Secrets are injected at launch, not stored in code or images | `agentd` resolves configured environment-variable sources and `agentd-runner` accepts the resolved values | Sessions cannot authenticate or credentials leak across agents |
| Identity | Each session receives stable in-container identity variables and container naming | `agentd-runner` session contract and Podman lifecycle | Operators cannot distinguish which agent a session belongs to |
| Mission | Scheduling or CLI invocation hands repo and optional work unit into session launch | `agentd-scheduler` plus `agentd-runner` boundary | Agents run without a reason or target |
| Tool Availability | Tools are provided through the environment; MCP remains a runtime concern | Three-crate workspace with no transport crate | agentd would absorb protocol work it does not need |
| Context | Methodology assets are mounted read-only into sessions and the repo is freshly cloned per run | `agentd-runner` boundary and crate intent | Agents operate without local awareness |
| Scheduling independence | Timing policy remains separate from execution setup | `agentd-scheduler` crate boundary | Scheduling changes would destabilize runner logic |
