# Provision session secrets

Deliver credentials to the agentd daemon so agent sessions receive their
configured secrets. agentd reads each configured credential from the
**daemon's own process environment** and forwards it into the session as an
ephemeral Podman secret; the operator's job is getting the right variables
into that environment without persisting secret values anywhere
world-readable.

How a session receives credentials (reference:
[README § Configuration](../../README.md#configuration)):

- `[[agents.credentials]]` — each entry names a session-visible variable
  (`name`) and the daemon-environment variable it is read from (`source`).
- `repo_token_source` — daemon-environment variable holding an HTTPS
  clone-only token. SSH clones use mounted `.ssh` material instead
  ([equip-agent-ssh-identity.md](equip-agent-ssh-identity.md)) and need no
  token.

## Parameters

- `<env-file>` — host path of the rendered environment file, owner-only for
  the daemon user. Convention: `/var/lib/tesserine/config/credentials.env`.
- The set of `source` variable names declared in `agentd.toml`.

## Preconditions

- Shell access to the agentd host as the daemon user.
- A secret store the host can query non-interactively (a secrets-manager
  CLI with a machine token, or systemd-creds material). Which store is a
  host decision; this runbook is store-agnostic.

## Procedure

### 1. Render the environment file from your secret store

Produce `<env-file>` containing one `KEY=value` line per `source` variable.
Render it atomically and owner-only, and never echo values:

```bash
umask 077
tmp="$(mktemp "$(dirname <env-file>)/.credentials.XXXXXX")"
# Query your secret store here, writing KEY=value lines to "$tmp".
# Do not log, echo, or command-line-pass the values; do not run under set -x.
mv "$tmp" <env-file>
```

Rules the renderer must follow, whatever the store:

- The file is created `0600` before any secret byte is written (the `umask`
  + `mktemp` pattern above).
- Values are written exactly — decode any store-side JSON string escapes.
- A failed render leaves the previous file intact (atomic `mv`).
- Machine tokens for the store live in their own owner-only file, never in
  the rendered output and never in shell history.

### 2. Point the daemon at the environment file

For a Quadlet/systemd-managed daemon container, reference the file from the
unit so every daemon start carries the variables:

```ini
[Container]
EnvironmentFile=<env-file>
```

For a directly-run daemon, export the variables into its process
environment from the same file before start. Either way, re-render then
restart the daemon to pick up rotated values:

```bash
systemctl --user restart agentd.service
```

### 3. Rotate

Rotation is re-running this runbook: update the value in the secret store,
re-render `<env-file>`, restart the daemon. Sessions started after the
restart use the new value; running sessions keep the secret they were
started with until they end.

## Verification

Without printing any secret value:

```bash
stat -c '%a %U' <env-file>          # expect: 600 <daemon-user>
cut -d= -f1 <env-file> | sort       # expect: exactly the configured source names
```

Then start a session for an agent whose work requires the credential
(`agentd run <agent>`) and confirm it proceeds past the step that consumes
it — e.g. HTTPS clone for `repo_token_source`.

## Failure modes

- **Daemon starts but sessions lack a credential** — the `source` variable
  was absent from the daemon's environment at start; agentd reads sources
  at session creation from its own process environment, so fix the env file
  or unit reference and restart.
- **Stale value after rotation** — the daemon was not restarted after
  re-rendering; restart it.
- **Secret value in logs or history** — treat as exposure: rotate the value
  at the store, then re-render. Audit the renderer for `set -x`, `echo`, or
  command-line passing.

## History

The retired `tesserine/ops` repository carried a worked, tested
implementation of step 1 against Bitwarden Secrets Manager
(`agentd-secrets-loader`, 479 lines, with config-file/env-var precedence,
timeouts, JSON unescaping, and atomic replace). It is recoverable for
reference:

```sh
git -C ops show 'c394550^:scripts/agentd-secrets-loader'
```
