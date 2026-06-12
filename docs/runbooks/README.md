# Operator Runbooks

Generic operator procedures for hosts running agentd. Each runbook is one
complete use case: an operator with a goal follows the one runbook for that
goal, start to finish, and ends with a verification step.

This directory owns the **generic** agentd-host procedures
(canonical per [commons SOURCE-OF-TRUTH.md](https://github.com/tesserine/commons/blob/main/SOURCE-OF-TRUTH.md)).
Host-specific deployment state — concrete hostnames, deployment manifests,
convergence scripts, secret-manager choices — belongs to the operator's own
host operations repository, not here. These runbooks name the points where
host-specific procedure takes over.

| Runbook | Use case |
| --- | --- |
| [equip-agent-ssh-identity.md](equip-agent-ssh-identity.md) | Give an agent an SSH identity so its sessions can clone and push a repository over SSH |
| [provision-session-secrets.md](provision-session-secrets.md) | Deliver credentials to the daemon so sessions receive their configured secrets |
| [redeploy-agentd.md](redeploy-agentd.md) | Upgrade or recover a deployed agentd daemon to a chosen release ref |

## History

Predecessor runbooks lived in [`tesserine/ops`](https://github.com/tesserine/ops),
which is retired. The host-bound originals (babbie-specific paths, the
Bitwarden secrets loader, the `tesserine-rebuild-babbie` convergence script)
are recoverable from ops git history at commit `c394550`'s parent:

```sh
git -C ops show 'c394550^:runbooks/README.md'
git -C ops show 'c394550^:runbooks/equip-agent-ssh-identity.md'
git -C ops show 'c394550^:scripts/agentd-secrets-loader'
```

The runbooks here are their generic successors.
