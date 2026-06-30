# Redeploy agentd

Upgrade or recover a deployed agentd daemon to a chosen release ref. This is
the generic procedure; a host operations repository that automates it (image
build, artifact install, convergence, verification in one script) supersedes
the manual steps on that host.

The deployment shape itself — Quadlet-run daemon container, mounted Podman
socket, path-visibility requirements — is reference material in
[README § Deployment](../../README.md#deployment). This runbook is the
ordered operator procedure over it.

## Parameters

- `<ref>` — immutable agentd release ref: a `vX.Y.Z[-rc.N]` tag or full
  commit SHA. Never a branch name
  ([commons RELEASE.md](https://github.com/tesserine/commons/blob/main/RELEASE.md)).

## Preconditions

- Shell access to the agentd host as the daemon user.
- The reviewed deployment artifacts (`agentd.toml`, the Quadlet `.container`
  unit) are at the state you intend to converge to.
- No session you are unwilling to interrupt is running — check the daemon
  journal (`journalctl --user -u agentd.service`) or `podman ps` for live
  session containers.

## Procedure

### 1. Build the image at the pinned ref

```bash
podman build \
  --build-arg AGENTD_REF=<ref> \
  --tag localhost/agentd:<ref> .
```

### 2. Verify the built image before touching live state

```bash
podman inspect localhost/agentd:<ref> | jq '.[0].Config.Labels'
podman run --rm --entrypoint /usr/local/bin/agentd localhost/agentd:<ref> --version
```

The labels and self-reported version must name `<ref>`. A mismatch is a
build-input defect — stop before any live mutation.

### 3. Install artifacts and promote

Install the reviewed `agentd.toml` and Quadlet unit to their host
locations, update the unit's image reference to `localhost/agentd:<ref>`,
and install the matching host-side `agentd` CLI client if the host uses one
(the client and daemon should be the same build).

### 4. Restart and reload

```bash
systemctl --user daemon-reload
systemctl --user restart agentd.service
```

## Verification

```bash
systemctl --user is-active agentd.service
podman ps --format '{{.Image}}' | grep agentd     # running image names <ref>
agentd --version                                   # host CLI matches, if installed
```

Then run a session against the canonical integration fixture and confirm a
sealed audit record appears:

```sh
agentd run <agent> https://github.com/tesserine/example-hello --intent 'add a `greet(name)` function'
```

## Failure modes

- **Daemon fails startup probe** — the probe verifies create/chmod/restore/
  remove under `audit_root`; check that the audit mount and runtime paths
  are reachable from inside the daemon container (README § Deployment, path
  visibility).
- **Sessions fail after upgrade with mount errors** — `agentd.toml` mount
  sources must exist on the host and be visible to host Podman at the same
  absolute paths; a host reprovision may have wiped them (see
  [equip-agent-ssh-identity.md](equip-agent-ssh-identity.md) failure modes).
- **Rollback** — repeat this runbook with the previous known-good `<ref>`;
  refs are immutable, so the prior image either still exists locally or
  rebuilds reproducibly.
