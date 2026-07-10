# Environment Variable Reference

Every environment variable agentd reads or injects, by scope. Configuration
that lives in `agentd.toml` is documented in
[README § Configuration](../README.md#configuration); this reference covers
only the environment.

## Read by the daemon process

| Variable | Who sets it | Effect | Default |
| --- | --- | --- | --- |
| `RUST_LOG` | operator | Log filter; overrides `AGENTD_LOG` when set | — |
| `AGENTD_LOG` | operator | Log filter when `RUST_LOG` is unset | `info` |
| `AGENTD_LOG_FORMAT` | operator | `json` or `pretty` log output on stderr | `json` |
| credential `source` variables (e.g. `AGENTD_GITHUB_TOKEN`) | operator, per `[[agents.credentials]].source` in `agentd.toml` | Value forwarded into the session as the credential's `name`; read at session creation from the daemon's own process environment ([runbook](runbooks/provision-session-secrets.md)) | — (session creation fails if missing) |
| `repo_token_source` variable | operator, per agent config | HTTPS clone-only token for the agent's repository | — |
| `XDG_RUNTIME_DIR` | systemd/login session | Default socket directory: `$XDG_RUNTIME_DIR/agentd/agentd.sock` | — |
| `XDG_STATE_HOME`, `HOME` | login session | Default audit root: `$XDG_STATE_HOME/tesserine/audit`, falling back to `$HOME/.local/state/tesserine/audit` | — |
| `TMPDIR` | operator / image | Runner staging directory root; the daemon container image sets `/var/lib/agentd/tmp` ([README § Deployment](../README.md#deployment)) | system default |
| `CONTAINER_HOST` | daemon container image | Points the in-container Podman client at the mounted host socket | set by the image |

## Injected into the session container by agentd

| Variable | Value | Consumed by |
| --- | --- | --- |
| `AGENT_NAME` | the agent's configured name | session tooling |
| `GROUNDWORK_FORGE_TYPE` | `forge_type` from agent config, defaulting to `github` | groundwork forge mechanics |
| `RUNA_FORGE_TYPE` | `forge_type` from agent config, defaulting to `github` | runa deployment forge identity |
| `RUNA_FORGE_OWNER` | `forge_owner` from agent config, when configured | runa deployment forge identity: binds a ticket reference to the deployment |
| `RUNA_FORGE_NAME` | `forge_name` from agent config, when configured | runa deployment forge identity: binds a ticket reference to the deployment |
| `AGENTD_WORK_UNIT` | the `--work-unit` identifier, when present | session tooling |
| `AGENTD_REPO_TOKEN` | the resolved repo token, when configured | the generated clone step only: consumed for the one HTTPS `git clone` and unset before the agent command runs — it cannot authorize pushes. HTTPS pushes need a credential delivered via `[[agents.credentials]]`; SSH pushes use the mounted `.ssh` identity |
| `RUNA_TRANSCRIPT_DIR` | the mounted transcript directory | runa / runa-mcp transcript emission |
| `RUNA_TRANSCRIPT_DEPLOYMENT` | deterministic project identity composed by agentd from the session forge type and repository URL | runa transcript path selection |
| `RUNA_TRANSCRIPT_RUN_ID` | the agentd session id for this run | runa transcript path selection |
| `RUNA_TRANSCRIPT_REDACT_ENV` | names of session env vars whose values must be redacted from transcripts | runa / runa-mcp |
| credential `name` variables (e.g. `GITHUB_TOKEN`) | value of the corresponding daemon-side `source` variable, delivered via ephemeral podman secret | agent process |

runa additionally injects its own session variables (`RUNA_FORGE_*`,
`RUNA_ENTRY_TICKET`, `RUNA_MCP_CONFIG`) — those are runa's surface,
documented in [runa's contracts](https://github.com/tesserine/runa/tree/main/docs),
not agentd's.

## Test-only

`AGENTD_FAKE_PODMAN_LOG_DIR` and `AGENTD_LARGE_TRANSCRIPT_STREAMING_CHILD`
are used by the test suites' fake-podman and streaming fixtures; they have
no production effect.

Adding a variable to either production table requires updating this
reference in the same change.
