# Quickstart: First Session to Sealed Audit Record

From nothing to one completed agent session against the canonical
integration fixture, with the audit record to prove it. Each step links the
reference documentation it abbreviates.

Prerequisites: Linux host with rootless Podman, Rust toolchain, an
Anthropic API key.

## 1. Build the session image

Sessions run in the [base](https://github.com/tesserine/base) image (runa +
verified Claude Code):

```sh
git clone https://github.com/tesserine/base && cd base
podman build --tag localhost/tesserine/base .
cd ..
```

## 2. Get a methodology

Sessions execute a runa methodology; the reference methodology is
[groundwork](https://github.com/tesserine/groundwork):

```sh
git clone https://github.com/tesserine/groundwork
```

## 3. Configure an agent

Write `agentd.toml` (full reference: [README § Configuration](../README.md#configuration),
annotated example: [examples/agentd.toml](../examples/agentd.toml)):

```toml
[[agents]]
name = "site-builder"
base_image = "localhost/tesserine/base"
methodology_dir = "./groundwork"
repo = "https://github.com/tesserine/example-hello"

[agents.command]
argv = ["claude", "-p", "--dangerously-skip-permissions"]

[[agents.credentials]]
name = "ANTHROPIC_API_KEY"
source = "AGENTD_ANTHROPIC_KEY"
```

## 4. Build agentd and start the daemon

```sh
git clone https://github.com/tesserine/agentd && cd agentd
cargo build --release
export AGENTD_ANTHROPIC_KEY="<your key>"   # the credential source above
./target/release/agentd daemon --config ../agentd.toml
```

The daemon probes its audit root at startup and then listens on its Unix
socket. (Production deployments run the daemon as a Quadlet-managed
container instead — [README § Deployment](../README.md#deployment).)

## 5. Run the smoke-test session

In a second shell, trigger the canonical integration intent
([example-hello](https://github.com/tesserine/example-hello)):

```sh
./target/release/agentd wish site-builder
```

`wish` greets you and prompts for the statement. For this smoke test, enter
``add a `greet(name)` function`` at the intent prompt and leave the target
prompt blank. The command blocks until the session reaches a terminal outcome
and reports it. `success` means the work completed; any other label is
interpreted per
[commons EXIT-CODES.md](https://github.com/tesserine/commons/blob/main/EXIT-CODES.md).

## 6. Inspect the audit record

Every session leaves a sealed record
(format: [audit-record.md](audit-record.md)):

```sh
record=$(ls -dt "${XDG_STATE_HOME:-$HOME/.local/state}/tesserine/audit/site-builder/"* | head -1)
cat "$record/agentd/session.json"            # outcome, exit code, timestamps
cat "$record/agentd/transcript/manifest.json" # transcript coverage verdict
less "$record/agentd/transcript/transcript.md"
```

Pass criteria: `session.json` shows `"outcome": "success"`, the record
tree is read-only (sealed), and `manifest.json` coverage is `full` or
`missing_mcp_events`.

## Where to go next

- Operate it for real: [runbooks](runbooks/README.md) — SSH identities,
  session secrets, redeploys.
- Scheduled runs: [README § Scheduled Runs](../README.md#scheduled-runs).
- What happened inside: [ARCHITECTURE.md](../ARCHITECTURE.md) — the
  four-phase session lifecycle and isolation model.
