# Daemon Socket Protocol

Wire protocol between the agentd daemon and its CLI client over the Unix
socket at `daemon.socket_path`. Implemented by
[`crates/agentd/src/protocol.rs`](../crates/agentd/src/protocol.rs) (message
types) and `crates/agentd/src/daemon.rs` (framing and dispatch).

**Stability: internal and unversioned in `v0.1.x`.** Daemon and client must
be the same build; messages carry no version field. After replacing the
binary, restart the daemon before using `agentd wish` or `agentd run` again.
Do not build external integrations against this protocol — the supported
integration surfaces are the CLI and the audit record
([audit-record.md](audit-record.md)).

## Framing and connection lifecycle

- Transport: `SOCK_STREAM` Unix domain socket.
- Framing: newline-delimited JSON — one request line, followed by one or
  more response lines.
- Lifecycle: connect → write one request + `\n` → read response lines until
  one terminal `session_outcome` or `error` line → connection closes. One
  session trigger per connection; `run` connections stay open for the
  session's duration and may carry progress lines before the terminal line.
- An empty connection (EOF before a line) is ignored. A malformed request
  line gets an `error` response.

## Requests

`{"type": "ping"}` — liveness probe.

`{"type": "run", ...}` — trigger one session:

| Field | Type | Meaning |
| --- | --- | --- |
| `agent` | string | Exact `[[agents]].name` in `agentd.toml` |
| `repo_url` | string \| null | Clone URL; `null` uses the agent's configured `repo` |
| `work_unit` | string \| null | Work-unit identifier for work-mode invocations |
| `input` | object \| null | Operator-supplied session input, see below |

`input` is one of (externally tagged):

```json
{"IntentText": {"statement": "add a `greet(name)` function"}}
```

`IntentText.target` is optional and, when present, is copied into the
materialized intent artifact. `agentd wish` can populate it from the optional
target prompt; `agentd run --intent` has no target flag.

```json
{"Artifact": {"artifact_type": "work-unit", "artifact_id": "42", "document": { "...": "..." }}}
```

`IntentText` is synthesized into a canonical intent artifact
([commons INTENT.md](https://github.com/tesserine/commons/blob/main/INTENT.md));
`Artifact` is materialized verbatim after validation.

## Responses

`{"type": "pong"}` — answer to `ping`.

`{"type": "progress", "progress": {"stage": "...", ...}}` — non-terminal
live progress for a run request. `dispatch_started` is a start marker;
`transcript_event` carries execution-phase events from the running session's
`agentd/transcript/events.jsonl` stream. `agentd wish` and `agentd run` render
these frames in the invoking terminal while they wait for the terminal outcome;
`--progress summary` prints concise event names and `--progress full` includes
the raw transcript event line.

`{"type": "error", "message": "..."}` — the request was rejected before a
session outcome existed (malformed request, unknown agent, dispatch
failure).

`{"type": "session_outcome", "outcome": {"status": "...", ...}}` — the
session ran to a terminal outcome. A `session_outcome` response means the
transport and dispatch succeeded; the work may still have failed — read
`status`.

### Progress stages

`dispatch_started` means the daemon has accepted the run request on the
operator connection and is dispatching it into session execution:

```json
{"type":"progress","progress":{"stage":"dispatch_started","agent":"site-builder","work_unit":"issue-42","input_present":false}}
```

`transcript_event` means the running session appended a structured transcript
event that the daemon forwarded before the terminal outcome:

```json
{"type":"progress","progress":{"stage":"transcript_event","session_id":"7f5a1c2d9e0b3a44","line":"{\"schema_version\":1,\"source\":\"runa\",\"kind\":\"agent_input\",\"content\":\"...\"}"}}
```

## Outcome statuses

`status` values and exit codes implement the shared session-outcome
vocabulary — canonical:
[commons/EXIT-CODES.md](https://github.com/tesserine/commons/blob/main/EXIT-CODES.md).
The raw exit code is always preserved alongside the label, per the commons
caller contract.

| `status` | Fields | Commons code |
| --- | --- | --- |
| `success` | `exit_code` | 0 |
| `generic_failure` | `exit_code` | 1 (and unrecognized non-zero codes) |
| `usage_error` | `exit_code` | 2 |
| `blocked` | `exit_code` | 3 |
| `nothing_ready` | `exit_code` | 4 |
| `work_failed` | `exit_code` | 5 |
| `infrastructure_failure` | `exit_code` | 6 |
| `command_not_executable` | `exit_code` | 126 (reserved external) |
| `command_not_found` | `exit_code` | 127 (reserved external) |
| `terminated_by_signal` | `exit_code`, `signal` | 128+N (reserved external) |
| `timed_out` | — | agentd-layer addition: commons scopes caller-enforced timeout outside the shared vocabulary, and agentd is that caller |

## Examples

```sh
printf '{"type":"ping"}\n' | socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/agentd/agentd.sock"
```

```json
{"type":"run","agent":"site-builder","repo_url":"https://github.com/tesserine/example-hello","work_unit":null,"input":{"IntentText":{"statement":"add a `greet(name)` function","target":"tesserine/example-hello#7"}}}
```
