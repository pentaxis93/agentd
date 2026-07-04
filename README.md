# agentd

**Run autonomous agents like production workloads — isolated, credentialed,
and evidence-leaving.**

agentd is the daemon that makes "an agent ran unattended on my hardware"
something you can defend. Each session gets an ephemeral rootless Podman
container with its own unprivileged identity, a fresh repository clone, and
read-only methodology context; privileged setup ends in a permanent `gosu`
drop. Credentials are minimized by construction — session secrets are
delivered as ephemeral podman secrets and removed from the host-side store
the moment the container is running, and an HTTPS repo token is consumed by
the clone step and unset before the agent process ever starts.

And when the session ends, it leaves **evidence**: a sealed audit record —
directories read-only, metadata published by atomic replace, symlink and
hardlink tampering refused loudly — whose
[format is a documented contract](docs/audit-record.md). A record with an
outcome is proof the tree was already sealed when that outcome was written.
That is the property the rest of the
[Tesserine](https://github.com/tesserine) stack's trust story rests on
(ecosystem map:
[commons SOURCE-OF-TRUTH.md](https://github.com/tesserine/commons/blob/main/SOURCE-OF-TRUTH.md)).

The operator declares *what* through agent configuration — which image, which
credentials, which methodology. agentd owns *how* — container lifecycle,
privilege management, resource cleanup. Model inference and MCP transport
belong to the agent runtime inside the container; agentd deliberately does
neither.

The project targets Linux hosts. Non-Linux builds fail intentionally because
the runner depends on Linux runtime primitives including rootless Podman,
systemd user services, and SELinux-aware filesystem handling.

## Status

Pre-1.0 — early development.

The session lifecycle works end-to-end: agent configuration, containerized daemon
startup, operator-triggered sessions, ephemeral Podman containers, credential
injection, execution, and teardown. Startup reconciliation cleans stale
resources from prior runs. Structured JSON tracing provides operational
visibility.
Agents may now declare a default repository and an optional cron schedule.
Manual operator sessions flow through `agentd wish` or `agentd run`, and
scheduled runs dispatch through the same daemon socket intake without
introducing a separate job type.
Operators may also start an intent-seeded session with `agentd wish`, which
prompts for the desired-state statement and optional target before dispatching
through that same socket intake. Manual runs may carry per-invocation work input
without modifying the agent: intent text can be synthesized into a canonical
intent artifact, and complete JSON artifacts can be placed directly into the
session workspace when the active methodology declares the relevant artifact
type and schema.
Agents may also declare additional bind mounts for host-managed state such as
subscription auth directories. Independently of agent mounts, agentd now
persists each session's audit record under the rootless default
`$XDG_STATE_HOME/tesserine/audit/`, falling back to
`$HOME/.local/state/tesserine/audit/`, with `daemon.audit_root` available as an
explicit override for root-owned installs such as `/var/lib/tesserine/audit/`.

## Configuration

An agent is a named environment specification: base image, methodology
directory, optional active forge type, optional additional bind mounts, optional
default repo, optional cron schedule, credentials, and exact command argv. Define agents in a TOML
config file — start from
[`examples/agentd.toml`](examples/agentd.toml):

```toml
# Static agent registry for agentd.
# An agent can carry its own default repo and optional schedule.

#[daemon]
# Optional explicit host path for persistent audit records. Rootless installs
# default to $XDG_STATE_HOME/tesserine/audit, falling back to
# $HOME/.local/state/tesserine/audit when XDG_STATE_HOME is unset.
# Root-owned system installs should typically point this at
# /var/lib/tesserine/audit.
#audit_root = "/var/lib/tesserine/audit"

[[agents]]
# Stable operator-facing agent name used for lookup and container identity.
name = "site-builder"
# Prebuilt image containing the agent runtime and runa.
base_image = "ghcr.io/example/site-builder:latest"
# Methodology directory to mount read-only into the session environment.
methodology_dir = "../groundwork"
# Optional active forge type injected as GROUNDWORK_FORGE_TYPE for Groundwork.
# Defaults to "github" when omitted.
#forge_type = "github"
# Default repository URL cloned for manual runs when `agentd run` omits a repo,
# and for every scheduled run of this agent. HTTPS, HTTP, git://, ssh://, and
# user@host:path SSH forms are accepted.
repo = "git@github.com:pentaxis93/agentd.git"
# Optional five-field cron expression in daemon-local time.
schedule = "*/15 * * * *"
# Optional environment variable name for HTTPS clone-only authentication.
# SSH clones use mounted `.ssh` material instead.
#repo_token_source = "SITE_BUILDER_REPO_TOKEN"
# Exact argv for the agent process. agentd handles runa init and runa run.
[agents.command]
argv = ["site-builder", "exec"]

#[[agents.mounts]]
# Additional host bind mounts are declared explicitly per agent.
# `source` must be an absolute host path and must already exist.
# `target` must be an absolute path inside the container and must not
# duplicate or overlap another mount target in the same agent.
# `read_only = true` is appropriate for host-managed auth directories,
# including OpenSSH material used for SSH repository clone and push.
#source = "/home/core/.ssh/site-builder"
#target = "/home/site-builder/.ssh"
#read_only = true

[[agents.credentials]]
# Secret name exposed inside the session environment.
name = "GITHUB_TOKEN"
# Environment variable name read from the daemon's own process environment.
source = "AGENTD_GITHUB_TOKEN"

[[agents]]
# A home-repo review agent that carries its own review configuration and scans
# repositories beyond the repo used to launch the session.
name = "code-reviewer"
base_image = "ghcr.io/example/code-reviewer:latest"
methodology_dir = "../groundwork"
repo = "https://github.com/pentaxis93/agentd.git"
repo_token_source = "CODE_REVIEWER_REPO_TOKEN"
[agents.command]
argv = ["code-reviewer", "exec"]

[[agents.credentials]]
name = "GITHUB_TOKEN"
source = "AGENTD_GITHUB_TOKEN"
```

Credential `source` fields name environment variables in the daemon's process
environment — export them before starting the daemon. Additional `mounts`
entries are bind mounts: `source` must be an absolute existing host path,
`target` must be an absolute container path, targets must be unique within the
agent, and runner-managed targets are reserved: `/agentd/methodology`,
`/agentd/transcript`, `/home/{agent}`, `/home/{agent}/.agentd`, and
`/home/{agent}/repo` plus their descendants. Other targets under `/home/{agent}` remain supported,
including read-only auth mounts such as `/home/site-builder/.claude`.
For SSH repository URLs, mount OpenSSH-compatible identity, config, and
`known_hosts` material under `/home/{agent}/.ssh`; clone uses that material
from the session user's home and does not require `repo_token_source`.
Additional mounts are not relabelled; on SELinux-enabled hosts, operators must
ensure each host path already has a container-compatible label. Runner-owned
mounts such as
methodology, invocation input, and the internal audit `runa/` subtree are
relabelled by Podman from their canonical host source paths. The base image
must provide `/bin/sh`, `find`, `git`, `groupadd`, `useradd`, `gosu`, `runa`,
and whatever binaries the declared agent command uses. SSH repository clone
also requires an OpenSSH-compatible client in the base image. UID/GID `1000`
must be available for the session user. The optional `forge_type` value declares one
active forge type for each session and is injected as `GROUNDWORK_FORGE_TYPE`; when
omitted, agentd injects `github`. When an agent declares `schedule`, it must
also declare `repo`. Schedules are evaluated in daemon-local time and missed
fires are not backfilled after downtime. Persistent audit records default to
`$XDG_STATE_HOME/tesserine/audit` or `$HOME/.local/state/tesserine/audit`; set
`daemon.audit_root` to override that for system installations.

## Deployment

The supported daemon deployment shape is a locally built container image run by
Quadlet. Build the image on the target host or another trusted build host with
access to the checked-out source or release tag. Deployment builds must pin the
daemon source to an immutable tag or full commit SHA; do not deploy from `main`
or another mutable branch ref. The shared release-candidate convention is
defined in
[commons RELEASE.md](https://github.com/tesserine/commons/blob/main/RELEASE.md).
Repository release operations are described in [`RELEASING.md`](RELEASING.md).

```bash
podman build \
  --build-arg AGENTD_REF=v0.1.2 \
  --tag localhost/agentd:v0.1.2 .
```

Confirm the deployed image metadata with:

```bash
podman inspect localhost/agentd:v0.1.2 | jq '.[0].Config.Labels'
```

The image's default command starts the daemon and reads
`/etc/agentd/agentd.toml`. The image includes the `agentd` binary and the
Podman client. The daemon container is a supervisor for ordinary agentd session
containers; the session containers themselves are not Quadlets.

The daemon container talks to the host's rootless Podman service through a
mounted Podman socket. The image expects that socket at
`/run/podman/podman.sock` and sets `CONTAINER_HOST` accordingly. On a typical
rootless host, mount the user's Podman socket from
`$XDG_RUNTIME_DIR/podman/podman.sock` to that in-container path.

Use explicit daemon runtime paths in the config so the socket can be mounted
back to the host for `agentd run` clients:

```toml
[daemon]
socket_path = "/run/agentd/agentd.sock"
pid_file = "/run/agentd/agentd.pid"
audit_root = "/var/lib/tesserine/audit"
```

Mount the host runtime directory that should hold the client socket to
`/run/agentd` in the daemon container. With a host mount such as
`$XDG_RUNTIME_DIR/agentd:/run/agentd`, host-side clients can use the normal
default `$XDG_RUNTIME_DIR/agentd/agentd.sock` path, or pass the same host path
with `--socket-path`.

Path visibility matters because the daemon process and the host Podman service
see different filesystems. The config file, socket path, PID file, credential
environment, and mounted Podman socket must be reachable by the daemon process
inside the daemon container. Session bind sources that the daemon opens and
then forwards to host Podman must also be valid from the host Podman service's
view: `methodology_dir`, each agent-declared `mounts.source`, `audit_root`, and
the runner staging directory. Runner-owned relabelled sources are passed to
Podman as canonical host paths so SELinux relabeling applies to the real tree,
not to a staging alias. This image sets `TMPDIR=/var/lib/agentd/tmp`, so
the host must also expose that staging directory at `/var/lib/agentd/tmp` when
using the image default. Mount host `/var/lib/agentd/tmp` to container
`/var/lib/agentd/tmp`, or set `TMPDIR` to another path the operator can expose
at the same absolute path on both the host and daemon container. In practice,
mount all shared session source trees into the daemon container at the same
absolute paths recorded in `agentd.toml` or used by `TMPDIR`.

Audit sealing is performed by the daemon process with direct filesystem chmod
operations; it does not enter Podman's user namespace during finalization. The
startup probe verifies the daemon can create, chmod, restore, and remove its
own probe entries under `audit_root`. Session containers are created with
`--userns keep-id:uid=1000,gid=1000`, and agentd creates the in-container
session user at UID/GID `1000`. The audit mount therefore appears owned by the
same identity that runs `runa init`, and session-written audit files remain
within the daemon's host-side chmod authority. This is a single-tenant security
tradeoff: compared with default rootless subuid mapping, a session-user escape
has the daemon's host-file authority over daemon-owned paths. That daemon
identity still needs access to the mounted Podman socket and configured runtime
paths.

A host-installed `agentd` binary remains useful as a same-build CLI client for
`agentd run`, but host-binary daemon supervision is out of band for supported
deployments.

Confirm the image contents with:

```bash
podman run --rm --entrypoint /usr/local/bin/agentd localhost/agentd:v0.1.2 --version
```

## Running a Session

The daemon runs in the container, reconciles stale resources from prior runs,
and binds a Unix socket for operator control. `agentd` with no subcommand is
equivalent to `agentd daemon`.

When `daemon.socket_path` and `daemon.pid_file` are omitted, agentd chooses
coordinated defaults from the current XDG runtime context:

- `$XDG_RUNTIME_DIR/agentd/agentd.sock`
- `$XDG_RUNTIME_DIR/agentd/agentd.pid`

`XDG_RUNTIME_DIR` must be set to an absolute path for daemon defaults. Container
deployments should configure `daemon.socket_path` and `daemon.pid_file`
explicitly so the daemon socket location is also a deliberate host mount.

On SIGINT or SIGTERM, the daemon stops accepting connections and drains
in-flight sessions; a second signal exits immediately.
The Unix socket protocol is internal to `agentd` in `v0.1.x`: daemon and CLI
must be the same build, and operators must restart the daemon after replacing
the binary before using `agentd wish` or `agentd run` again. The wire format is
documented in [docs/socket-protocol.md](docs/socket-protocol.md).

Trigger a session through the running daemon:

```bash
agentd run site-builder --work-unit 42
```

For operator intent, use `wish`:

```bash
agentd wish site-builder
```

`wish` greets the operator with `Speak a wish: the state you want made true.`,
asks `What do you wish to be true?`, and then offers the optional target prompt
`What is this wish aimed at? Leave blank if it has no target.` When the target
prompt is left blank, agentd authors `{statement, source}`. When a target is
supplied, agentd authors `{statement, target, source}` and passes the target
through unchanged for the runtime to interpret.

Manual invocation also supports lower-level intake-mode and work-mode
surfaces:

- `--work-unit <ID>` seeds an existing work-unit reference
- `--intent <TEXT>` synthesizes a canonical intent artifact at
  `.runa/workspace/intent/operator-input.json`
- `--artifact-type <TYPE> --artifact-file <PATH>` validates and places a
  complete JSON artifact at `.runa/workspace/<TYPE>/<file-stem>.json`

`--intent` is intake mode and is mutually exclusive with `--work-unit` and
`--artifact-file`. To start work mode from an operator-supplied work-unit
artifact, combine the selected work-unit id with a matching `work-unit` artifact
file:

```bash
agentd run site-builder --work-unit 42 --artifact-type work-unit --artifact-file ./42.json
```

The file stem becomes the artifact id, so the file stem must match
`--work-unit`. agentd still routes the reference through runa's resolving entry:
it materializes a target-bearing `intent` seed and launches unscoped `runa run`
so runa resolves the reference fail-closed before scoped work begins.

`agentd run` does not read `agentd.toml`. The client connects to the daemon by
either:

- explicit override with `--socket-path <PATH>`
- default XDG resolution to `$XDG_RUNTIME_DIR/agentd/agentd.sock`

Default resolution is deterministic: the client does not probe candidate
sockets and does not fall back to `/tmp` or `/run`. When `XDG_RUNTIME_DIR` is
unset, empty, or relative, `agentd run` exits with an actionable error pointing
to either setting `XDG_RUNTIME_DIR` or using `--socket-path`.

Agent lookup and default-repo resolution now happen daemon-side. The client
may omit the positional repo argument when the named agent declares `repo`,
and an explicit repo still overrides the configured default:

```bash
agentd run --socket-path /custom/agentd.sock site-builder --work-unit 42
agentd run site-builder https://github.com/pentaxis93/agentd.git --work-unit 42
```

Text input is methodology-gated. `--intent` is available only when the active
methodology declares artifact type `intent`, ships `schemas/intent.schema.json`,
and that schema advertises a supported canonical intent version through
`x-tesserine-canonical.version`. In `agentd v0.1.x`, the supported set is
`2.0.0` only. `agentd wish` uses the same intent intake and may include an
operator-entered `target`; `agentd run --intent` authors `{statement, source}`
and has no target flag. Unsupported or undeclared intent support is rejected
before the container is created.

Artifact-file input is generic. The CLI reads the file locally, requires UTF-8
JSON, derives the artifact id from the file stem, and sends structured JSON to
the daemon. The runner accepts that input only when the methodology declares
the artifact type in `manifest.toml` and ships a matching
`schemas/<type>.schema.json`.

Examples:

```bash
agentd wish site-builder
agentd run site-builder --intent "Add a status page"
agentd run site-builder --artifact-type claim --artifact-file ./claim.json
agentd run site-builder --work-unit 42 --artifact-type work-unit --artifact-file ./42.json
agentd run site-builder https://github.com/pentaxis93/agentd.git --intent "Review the last release candidate"
```

Both manual and scheduled dispatches use the same daemon socket intake. Inside
the container, the agent sees:

- An unprivileged user with `$HOME` at `/home/site-builder`
- A fresh clone of the repository at `/home/site-builder/repo`
- Repo-root `.runa` bridged to persistent audit storage
- Read-only methodology mount at `/agentd/methodology`
- A runner-managed read-only invocation-input mount at `/agentd/invocation-input` when manual input is supplied
- Any operator-declared additional bind mounts, read-only or read-write per agent
- Credentials injected as environment variables
- `GROUNDWORK_FORGE_TYPE` with the configured active forge type, defaulting to `github`
- `AGENTD_WORK_UNIT` when the invocation includes one
- A pre-materialized artifact under `.runa/workspace/...` when the invocation includes `wish`, `--intent`, `--work-unit`, or `--artifact-file`; `--work-unit` is materialized as `intent/operator-input.json` with the reference in `target`
- `runa init` state followed by `runa run --agent-command -- <argv>` from the repo directory

The container is force-removed on completion. The session's audit record
persists on the host under the resolved audit root
`<audit_root>/<agent>/<session_id>/`, with runa state in `runa/`, agentd
metadata in `agentd/session.json`, and transcript artifacts in
`agentd/transcript/`. Transcript artifacts include structured JSON Lines events
under `deployments/<deployment>/work-units/<work-unit>/runs/<run-id>/events.jsonl`,
a human-readable `transcript.md`, and `manifest.json` with coverage of `full`,
`missing_mcp_events`, `no_events`, or `finalization_failed`. agentd injects the
deployment and run id that address the runa event store, and the manifest records
the runa event schema versions it assembled. Full MCP tool-call coverage depends
on the agent runtime launching `runa-mcp`; otherwise the transcript still records
the observable runa boundary without claiming MCP events that never occurred.

If teardown cleanup fails, or if audit finalization attempts closeout and
fails, metadata remains intentionally incomplete with no `end_timestamp` or
`outcome`. On successful finalization, agentd seals persisted runa and
transcript entries read-only and publishes a read-only `session.json` as the
final commit point. Ancestor directories remain writable so the final
same-directory atomic replace can occur. The on-disk metadata does not encode
which incomplete path occurred; operators should use
`runner.lifecycle_failure` plus the surrounding `runner.session_outcome`,
`runner.session_error`, and `runner.session_teardown` events to disambiguate
cause.

## Scheduled Runs

Agents with `schedule` run autonomously while the daemon is up. The scheduler
evaluates cron expressions in daemon-local time and opens the same Unix-socket
client path that `agentd run` uses. Multiple scheduled agents may overlap,
and their sessions dispatch independently. Session outcomes do not affect later
schedule evaluation: the next occurrence runs at its next scheduled time.

## Going Deeper

- **[docs/quickstart.md](docs/quickstart.md)** — end-to-end tutorial: build
  the image, configure an agent, run the first session, inspect the sealed
  audit record.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — session lifecycle phases, container
  isolation model, credential flow, and workspace crate boundaries. How the
  system is built and why.
- **[AGENTS.md](AGENTS.md)** — development discipline, BDD workflow, commit and
  branch conventions. Read this before contributing.
- **[examples/agentd.toml](examples/agentd.toml)** — annotated agent
  configuration. Starting point for writing your own.
- **[docs/runbooks/](docs/runbooks/README.md)** — operator runbooks: equip an
  agent SSH identity, provision session secrets, redeploy the daemon.
- **[docs/audit-record.md](docs/audit-record.md)** — the sealed audit record
  format: layout, `session.json` and `manifest.json` schemas, sealing
  semantics. The supported surface for inspecting what a session did.
- **[docs/socket-protocol.md](docs/socket-protocol.md)** — daemon socket wire
  format (internal, unversioned in `v0.1.x`).
- **[docs/environment.md](docs/environment.md)** — every environment variable
  the daemon reads or injects, by scope.

## License

[MIT](LICENSE)
