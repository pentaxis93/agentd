# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
### Added

- `AGENTS.md` carries the canonical principles pointer
  (`pentaxis93/principles`), matching the runa/groundwork entry-line
  convention (#148).
- `docs/environment.md` — environment-variable reference covering the
  daemon-process, session-injection, and test-only scopes.
- `AGENTS.md` Build and Test section: MSRV, the cargo loop, and where the
  integration tests live.
- `docs/quickstart.md` — end-to-end tutorial from image build through a
  completed session to the sealed audit record, using the canonical
  example-hello integration request.
- `SessionOutcome` documents commons/EXIT-CODES.md as the canonical home
  of its outcome vocabulary, with `TimedOut` identified as the one
  agentd-layer (caller-enforced timeout) addition.
- `RELEASING.md` Tooling Provenance section and a `scripts/release-check`
  provenance header: the script is agentd-owned, the ceremony convention is
  canonical in commons, and no repo is the tooling upstream.

- Invariant-level documentation next to security-critical code: `audit.rs`
  gains a module contract (lifecycle, sealing invariants, symlink skipping,
  multi-link refusal, atomic metadata publish) with doc comments tying the
  sealing constants and lifecycle functions to the security model;
  `protocol.rs` documents the socket message enums and links the outcome
  vocabulary to its canonical home at commons/EXIT-CODES.md;
  `resources.rs` and `container.rs` document the secret-lifetime and
  privilege-drop paths.
- `docs/socket-protocol.md` — daemon Unix-socket wire reference: framing,
  connection lifecycle, message shapes, outcome-status table, and the
  internal/unversioned same-build rationale.
- `docs/audit-record.md` — audit record format reference: layout,
  `session.json` (`schema_version: 2`) and transcript `manifest.json`
  schemas, sealing semantics, and change policy.

- Operator runbooks under `docs/runbooks/`: equip an agent SSH identity,
  provision session secrets, and redeploy the daemon. Generic successors to
  the runbooks retired with `tesserine/ops` (recoverable at ops commit
  `c394550^`); host-specific deployment state remains with the operator's
  host operations repository.

- Runner setup now supports SSH repository clone through agent-scoped mounted
  OpenSSH material, including `ssh://` and `user@host:path` repository URLs.
- Agent configuration now accepts an optional `forge_type` field and injects the
  selected forge type as `GROUNDWORK_FORGE_TYPE`, defaulting omitted values to `github`.
- `agentd run` now supports work-mode invocation from a matching supplied
  `work-unit` artifact, using `--work-unit <ID> --artifact-type work-unit
  --artifact-file <ID>.json`.
- `agentd wish` starts an intent-seeded session through a desired-state
  operator prompt, collecting the statement and optional target before reusing
  the existing canonical intent intake.
- `agentd wish` and `agentd run` now stream live session progress in the
  invoking terminal, with `--progress summary` as the default and
  `--progress full` for all fields currently carried by progress frames.

### Changed

- `agentd run --work-unit` and `agentd wish --work-unit` now route the
  operator-supplied reference through runa's resolving entry by materializing a
  target-bearing intent seed and invoking unscoped `runa run`, instead of
  bypassing resolution with `runa run --work-unit`.

- Manual text input now authors canonical intent v2 (`statement`, optional
  `target`, and `source`) end to end: `agentd run --intent`, the daemon
  socket `IntentText` variant, and `.runa/workspace/intent/operator-input.json`
  materialization. The generic `--artifact-file` path remains a full-document
  intake for any declared artifact type.

- `agentd wish` now frames its greeting, statement prompt, optional target
  prompt, help text, and wish-input errors around the desired state the operator
  wants made true.

- README hero rewritten for the ecosystem README pass: positions agentd as
  the daemon for running autonomous agents like production workloads, with
  the isolation, credential-lifetime, and sealed-evidence guarantees stated
  with their proof links. All contract-bearing statements (same-build socket
  policy, socket resolution, audit root) preserved verbatim.

- Deployment and architecture documentation no longer names a specific
  operator host when describing supported single-tenant deployment tradeoffs.

### Fixed

- Operator-declared additional bind-mount sources are no longer canonicalized
  in the daemon's own filesystem namespace. On a containerized-daemon
  deployment that namespace holds only the daemon's own mounts, so a host
  source declared in `[[agents.mounts]]` but absent from the daemon container
  was rejected with `mount source path does not exist` even though the host
  Podman that performs the bind mount resolves it. Additional-mount sources are
  now staged as aliases pointing at the source as declared, with existence
  deferred to the host boundary that owns the mount; daemon-internal mounts
  (methodology, audit, transcript, invocation-input) still canonicalize
  unchanged (#158).
- README deployment examples reference the released `v0.1.2` tag (they
  were pinned to `v0.1.2-rc.1`).

## [0.1.2] — 2026-05-17
### Fixed

- Release verification now rejects empty `--agentd-bin` and
  `--container-image` option values instead of treating supplied empty values
  as omitted artifact checks.

## [0.1.2-rc.3] — 2026-05-14
### Fixed

- Release publication now restores annotated tag refs after checkout before
  tag-trust validation, preserving the annotated-tag security check under
  `actions/checkout@v4` tag-push behavior. Refs tesserine/commons#34.

## [0.1.2-rc.2] — 2026-05-13
### Added

- Container image builds now expose OCI labels and `org.tesserine.agentd.ref`
  so operators can inspect the daemon source ref from image metadata.
- Session audit records now include agentd-managed transcript artifacts under
  `agentd/transcript/`, with structured events, a human-readable Markdown
  rendering, and a coverage manifest.
- Release operations now use per-repo `cargo-release` configuration that
  follows the commons ADR-0006 workspace-version discipline and pins
  compliance-critical `cargo-release` behavior against user-level config
  fallthrough.
- Release ceremony tooling now documents the operator workflow, checks
  workspace, binary, container-label, extracted-CLI, and changelog identity,
  and publishes GitHub Releases from annotated version tags.

### Changed

- Deployment documentation now requires image builds to pin immutable tags or
  full SHAs instead of mutable branch refs such as `main`.
- The supported daemon deployment shape is now a locally built container image
  for Quadlet-managed operation; host-installed daemon supervision is out of
  band, while `agentd run` remains a host-side same-build client.
- `agentd --version` and `agentd run --version` now report the crate release
  version for operator deployment checks.
- Agent configuration is now declarative and uses `[[agents]]` with `[agents.command].argv`; the old profile-table vocabulary and shell-wrapper command shape are removed as a pre-1.0 breaking change. agentd now composes `runa init` and `runa run --agent-command -- <argv>` itself, leaving runa-owned `.runa/` config formats to runa.
- `agentd run` no longer reads `agentd.toml`: it now accepts `--socket-path <PATH>`, otherwise resolves the daemon socket deterministically as `$XDG_RUNTIME_DIR/agentd/agentd.sock`; the implicit `/tmp/agentd-$UID/agentd.sock` and `/run/agentd/agentd.sock` fallbacks are removed as a pre-1.0 breaking change, and agent lookup plus default-repo resolution now happen daemon-side.
- `agentd run` now accepts one per-invocation work surface without agent edits: `--work-unit <id>`, `--request <text>`, or `--artifact-type <type> --artifact-file <path>`. Request text is synthesized into `.runa/workspace/request/operator-input.json`, while artifact-file input places validated JSON at `.runa/workspace/<type>/<file-stem>.json`.
- `agentd-runner` now declares its real platform contract at compile time: the crate targets Linux only, and downstream non-Linux builds now fail explicitly instead of compiling dead fallback code into a non-functional binary.
- Session outcomes now follow the shared `commons` exit-code convention across `agentd` and `agentd-runner`: outcomes carry semantic labels plus raw exit codes, daemon and CLI surfaces report labels such as `blocked` and `generic_failure`, `agentd run` exits successfully for normal terminal states (`success`, `blocked`, `nothing_ready`), and timeout remains an agentd-layer outcome outside the shared exit-code vocabulary.
- Additional bind mounts now reserve only runner-owned targets (`/agentd/methodology`, `/home/{agent}`, and `/home/{agent}/repo` plus descendants), allowing supported read-only and read-write mounts elsewhere under `$HOME` without runner setup mutating host-backed data.
- Agent-declared bind mounts now reject overlapping container targets within the same agent, so nested targets fail validation before startup instead of reaching the container setup script.
- Persistent audit records now default to `$XDG_STATE_HOME/tesserine/audit/<agent>/<session_id>/`, falling back to `$HOME/.local/state/tesserine/audit/<agent>/<session_id>/` for rootless installs, with `daemon.audit_root` available as an explicit override for root-owned system installs such as `/var/lib/tesserine/audit/`.
- Audit metadata now uses `schema_version: 2` for the breaking identity field rename from `profile` to `agent`.
- Completed audit records now seal directories to `0555` and non-symlink entries to `0444`, skip symlinks while sealing, and update `agentd/session.json` through atomic temp-file replacement instead of truncate-and-write.
- `agentd_runner::SessionSpec` now requires an explicit `audit_root` field, making the audit-record destination part of the runner API instead of an implicit process-environment override.

### Fixed

- Empty transcript event streams now report manifest coverage as `no_events`
  instead of claiming an outer-stream classification not present in
  `events.jsonl`.
- Release workflow metadata validation now checks executable `run:` content
  instead of raw workflow text, so YAML and shell comments cannot satisfy
  release ordering or tag-trust invariants.
- Release publication now uses a broad `v*` workflow trigger, validates tag
  trust before running repository release code, and rejects leading-zero
  numeric identifiers and `rc.0` per the ecosystem SemVer grammar.
- Release governance checks now cover workflow tag matching behavior, decouple
  tag publication from path-filtered metadata checks, derive release metadata
  validation from workspace members, and use one workspace-version parser across
  release scripts.
- Release verification now rejects unsupported prerelease tag shapes, and the
  documented release-candidate `cargo-release` path now rolls the changelog
  before tagging.
- Runner-owned SELinux relabelled mounts now pass canonical host source paths
  to Podman, so FCOS Enforcing deployments relabel the real audit `runa/`
  subtree instead of a staging alias before `runa init`.
- Session setup now prepares the internal audit mount without recursively
  traversing the bind-mounted `runa/` directory, allowing containerized daemon
  deployments to reach `runa init` when the audit mount is not readable during
  recursive ownership transfer.
- Session setup no longer chowns the host-backed audit mount or staged input
  workspace from inside the rootless session container, and runner-created
  invocation-input staging now uses explicit host-side read modes, preserving
  the mode-based audit writability contract for credential-bearing runs.
- Session containers now map the daemon host identity to the in-container
  session user with `--userns keep-id:uid=1000,gid=1000`, so runa observes the
  audit-backed `.runa` state as owned by the user invoking `runa init`. This is
  an explicit single-tenant security tradeoff: a session-user escape has the
  daemon's host-file authority over daemon-owned paths.
- Session secret cleanup now works on the supported Debian Bookworm Podman
  version floor by avoiding `podman secret rm --ignore` while preserving
  idempotent cleanup for already-removed secrets.
- Default socket resolution now fails explicitly when `XDG_RUNTIME_DIR` is
  unset, empty, or relative instead of silently selecting a fallback path that
  may belong to a different daemon.
- Session teardown now skips audit finalization and sealing when cleanup fails, leaving `agentd/session.json` intentionally incomplete instead of marking a session complete while its audit bind mount may still be live.
- Completed session outcomes now remain caller-visible when only audit finalization fails after teardown cleanup succeeds.
- Audit sealing now refuses multi-linked entries before rewriting metadata, preventing host file mode changes through hard-linked audit aliases.
- Audit sealing now uses daemon-local filesystem chmod operations only, avoiding remote Podman client failures during finalization and surfacing the required audit-root UID alignment at daemon startup.
- Allocation rollback failure now preserves the incomplete audit-record signal instead of finalizing `agentd/session.json` after leaked cleanup state.
- Manual request-text input now rejects methodologies that do not declare canonical request support or that advertise an unsupported canonical request version, instead of synthesizing unchecked workspace content.

## [0.1.0] — 2026-04-10

First release. agentd is a daemon that runs autonomous AI agent sessions in
ephemeral Podman containers, enforcing isolation, credential hygiene, and
methodology governance.

### Daemon

- Foreground daemon with single-instance enforcement via PID file.
- Two-signal shutdown: first SIGTERM/SIGINT drains in-flight sessions,
  second force-exits.
- Startup reconciliation removes stale containers and orphaned secrets from
  prior runs, scoped per daemon instance.
- Structured JSON logging via `tracing`, with `AGENTD_LOG_FORMAT=json|pretty`
  format selection and `RUST_LOG`/`AGENTD_LOG` filter control.

### Operator interface

- Unix socket API for session dispatch.
- `agentd run <agent>` for manual single-session execution.
- Optional `repo` argument overrides the agent's configured default.

### Agents

- Static TOML configuration: base image, methodology directory, credentials,
  and session command per agent.
- Agent names validated as safe unix usernames (used for in-container
  unprivileged execution via `gosu`).
- Optional agent-level `repo` default and cron `schedule` for automated
  dispatch.
- `methodology_dir` paths resolve relative to the config file's directory.

### Session lifecycle

- Ephemeral Podman containers: created per session, force-removed on
  teardown regardless of outcome.
- Methodology directory mounted read-only into the container.
- Fresh repository clone into the container workspace. HTTPS-only URL
  validation; SSH and local paths rejected.
- Unprivileged execution: session command runs as a non-root user via
  `gosu`, with the agent name as the unix username.
- Optional per-session timeout with forced teardown on expiry.

### Credentials

- Credential injection via Podman-managed secrets for non-empty values;
  direct environment assignment for empty values.
- Optional `repo_token_source` for private HTTPS clone authentication
  without exposing tokens in process arguments or git config.
- Credential source names resolve against daemon-process environment
  variables at dispatch time.

### Scheduling

- Cron-based agent scheduling evaluated in daemon-local time.
- Scheduled sessions dispatch through the daemon's Unix socket, sharing the
  same execution path as manual `agentd run` invocations.
