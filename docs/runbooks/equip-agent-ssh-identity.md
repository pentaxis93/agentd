# Equip agent SSH identity

Equip an agentd agent with an SSH identity so the agent's sessions can clone
and push a private repository over SSH. SSH clones use mounted `.ssh`
material; they do not use `repo_token_source` (which is HTTPS-only — see
[provision-session-secrets.md](provision-session-secrets.md)).

## Parameters

- `<agent>` — the exact `[[agents]].name` in `agentd.toml`.
- `<git-ssh-host>` — the SSH host of the agent's private repository, such as
  `github.com`.
- `<private-repo-ssh-url>` — the SSH clone URL used for end-to-end
  verification.
- `<identity-root>` — host directory holding per-agent identity directories,
  owner-only for the daemon user. Convention: a dedicated root such as
  `/var/lib/tesserine/agent-ssh-identities`.

## Preconditions

- Shell access to the agentd host as the daemon user.
- The agent exists in `agentd.toml`.
- You can register an SSH public key with the Git service hosting
  `<private-repo-ssh-url>`.
- Changes to `agentd.toml` go through your host's change-review process
  where one exists.

## Procedure

### 1. Provision the agent's keypair and host-key trust

The identity directory convention for `<agent>`:

- directory: `<identity-root>/<agent>`, mode `0700`, owned by the daemon user
- private key: `<identity-root>/<agent>/id_ed25519`, mode `0600`
- public key: `<identity-root>/<agent>/id_ed25519.pub`, mode `0644`
- known hosts: `<identity-root>/<agent>/known_hosts`, mode `0644`

```bash
agent=<agent>
git_ssh_host=<git-ssh-host>
identity_dir="<identity-root>/$agent"

test ! -e "$identity_dir/id_ed25519"
test ! -e "$identity_dir/known_hosts"
install -d -m 0700 "$identity_dir"
ssh-keygen -t ed25519 -N "" -f "$identity_dir/id_ed25519" -C "$agent@$(hostname)"
ssh-keyscan -T 10 -t rsa,ecdsa,ed25519 -H "$git_ssh_host" > "$identity_dir/known_hosts"
test -s "$identity_dir/known_hosts"
ssh-keygen -lf "$identity_dir/known_hosts"
chmod 0600 "$identity_dir/id_ed25519"
chmod 0644 "$identity_dir/id_ed25519.pub" "$identity_dir/known_hosts"
```

The key is deliberately **passphraseless**: agent sessions have no
interactive terminal for a passphrase prompt, and the `.ssh` mount is
read-only, so OpenSSH cannot prompt for host-key trust or write first-use
`known_hosts` entries during clone or push. The private key never leaves
the host.

On SELinux-enforcing hosts, the identity directory must carry a container
file context or the session cannot read the mount. Register a durable
mapping and apply it:

```bash
sudo semanage fcontext -a -t container_file_t "<identity-root>(/.*)?"
sudo restorecon -RFv "<identity-root>"
```

On hosts without `semanage` (e.g. Fedora CoreOS), append the rule to
`/etc/selinux/targeted/contexts/files/file_contexts.local` directly, then
run `restorecon -RFv`.

### 2. Authorize the public key with the Git service

Before trusting the identity, compare the fingerprints printed by
`ssh-keygen -lf` against the Git provider's published SSH host-key
fingerprints. Then register `id_ed25519.pub` with the service for
`<private-repo-ssh-url>` — as a repository deploy key (write-enabled if the
agent pushes) or on a machine account scoped to that repository.

### 3. Mount the identity into the agent's session

Add a read-only mount of **only the owning agent's directory** to that
agent's configuration in `agentd.toml`, through review:

```toml
[[agents.mounts]]
source = "<identity-root>/<agent>"
target = "/home/<agent>/.ssh"
read_only = true
```

Never mount the shared identity root, and never mount another agent's
directory. Sessions run with `HOME=/home/<agent>`, so OpenSSH reads the
mounted material from the session user's home.

### 4. Restart the daemon on the reviewed configuration

Apply the reviewed `agentd.toml` to the live host and restart the daemon —
via your host's deployment procedure, or
[redeploy-agentd.md](redeploy-agentd.md) when the change ships together
with a release-ref change.

## Verification

Run the agent against its configured private repository:

```sh
agentd run <agent> <private-repo-ssh-url>
```

The run succeeds past repository clone using the mounted identity.

## Failure modes

- **SSH permission failure at clone** — the public key is not registered
  with the Git service, or is registered without access to this repository.
- **Host-key prompt or `known_hosts` failure** — `known_hosts` is missing,
  empty, or does not cover `<git-ssh-host>`; regenerate it with
  `ssh-keyscan` and re-verify fingerprints.
- **Permission denied reading the mount on SELinux hosts** — the
  `container_file_t` mapping was not applied; rerun `restorecon -RFv`.
- **Identity lost after host reprovisioning** — immutable-OS hosts wipe
  host state; recreate or restore the identity directory, and if a new
  keypair is generated, replace the registered public key before running
  the agent.
