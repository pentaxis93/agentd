# Audit Record Format

The persistent, sealed record every agentd session leaves behind. This is
the supported integration surface for inspecting what a session did.
Implemented by
[`crates/agentd-runner/src/audit.rs`](../crates/agentd-runner/src/audit.rs);
security model and tradeoffs: [ARCHITECTURE.md](../ARCHITECTURE.md)
§ "Host audit records".

## Location and layout

One record per session at `<audit_root>/<agent>/<session_id>/`:

```
<audit_root>/<agent>/<session_id>/
├── runa/                          # preserved .runa/ runtime state (store/, workspace/)
└── agentd/
    ├── session.json               # agentd-written session metadata (schema below)
    └── transcript/
        ├── deployments/<deployment>/work-units/<work-unit>/runs/<run-id>/events.jsonl
        │                           # runa-owned structured event stream(s)
        ├── transcript.md          # human-readable rendering of the event stream
        └── manifest.json          # transcript schema version + coverage verdict
```

`audit_root` resolution: `daemon.audit_root` if set, else
`$XDG_STATE_HOME/tesserine/audit`, else `$HOME/.local/state/tesserine/audit`.
Session ids are 16 lowercase hex characters.

## Sealing

A record is **active** while its session runs (directories `0755`, the
`runa/` and transcript subtrees session-writable) and **sealed** once it
completes: directories `0555`, non-symlink files `0444`. Sealed records are
world-readable by design — a single-tenant tradeoff documented in
ARCHITECTURE.md.

Sealing happens **before** the finalized metadata is published, so:

> A `session.json` containing an `outcome` field is proof the record tree
> was already sealed when that metadata was written.

A record whose `session.json` lacks `end_timestamp`/`outcome` is either a
live session or an interrupted one (daemon crash, kill); README
§ Running a Session describes diagnosing incomplete records.

To delete a sealed record: `chmod -R u+w <record_dir> && rm -rf <record_dir>`.

## `session.json` (`schema_version: 2`)

| Field | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `schema_version` | integer | always | `2` |
| `session_id` | string | always | 16-hex session identifier |
| `agent` | string | always | `[[agents]].name` that ran |
| `repo_url` | string | always | repository the session cloned |
| `work_unit` | string | when work-mode | work-unit identifier |
| `start_timestamp` | string (RFC 3339) | always | written at preparation |
| `end_timestamp` | string (RFC 3339) | finalized only | written at sealing |
| `outcome` | string | finalized only | outcome label per [commons/EXIT-CODES.md](https://github.com/tesserine/commons/blob/main/EXIT-CODES.md), or `error` when the runner failed before a session outcome existed |
| `exit_code` | integer | finalized, when applicable | raw exit code, preserved per the commons caller contract |

`session.json` is always published by atomic temp-file-and-rename within
the record directory: readers see a complete old or new document, never a
torn write.

## `manifest.json` (`schema_version: 1`)

| Field | Type | Meaning |
| --- | --- | --- |
| `schema_version` | integer | `1` for this manifest format, not the runa event schema |
| `coverage` | string | `full`, `missing_mcp_events`, `no_events`, or `finalization_failed` |
| `event_schema_versions` | array of integers | sorted distinct `schema_version` values found in assembled runa events; empty when no events were found or finalization failed before events could be read |
| `finalization_error` | string | present only with `finalization_failed` |

Coverage is honest by construction: a failure while rendering the
transcript is itself recorded as `finalization_failed` with the error, so
the manifest never overstates what the transcript captured.
`missing_mcp_events` means runa ran but the agent runtime never launched
`runa-mcp`, so tool-call events were not observable.

agentd sets `RUNA_TRANSCRIPT_DEPLOYMENT` and `RUNA_TRANSCRIPT_RUN_ID` before
launching runa. It reads event files only below the matching nested deployment
and run id, while allowing multiple work-unit stage directories to appear
during one session. Every nested directory component below the trusted
`agentd/transcript` base is traversed without following symlinks, and unsafe
symlinked ancestors are refused rather than read through.

## Change policy

`session.json` and `manifest.json` breaking changes bump the respective
`schema_version` and this document in the same change. Additive optional
fields do not.
