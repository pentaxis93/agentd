# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
### Added

- Release operations now use per-repo `cargo-release` configuration that
  follows the commons ADR-0006 workspace-version discipline and pins
  compliance-critical `cargo-release` behavior against user-level config
  fallthrough.

### Changed

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

- Session setup now prepares the internal audit mount without recursively
  traversing the bind-mounted `runa/` directory, allowing containerized daemon
  deployments to reach `runa init` when the audit mount is not readable during
  recursive ownership transfer.
- Session setup no longer chowns the host-backed audit mount or staged input
  workspace from inside the rootless session container, and runner-created
  invocation-input staging now uses explicit host-side read modes, preserving
  the mode-based audit writability contract for credential-bearing runs.
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
