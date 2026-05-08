# Releasing agentd

Audience: the release operator cutting an agentd repository release or release
candidate. This document assumes access to the repository, GitHub, Rust, `jq`,
and a local container runtime compatible with Docker or Podman commands.

## Release Identity

agentd uses one repository tag for the Rust workspace, daemon container image,
and host CLI extracted from that image. The tag is `vX.Y.Z` for stable
releases and `vX.Y.Z-rc.N` for deployment release candidates.

Artifacts built from the tag must report that identity:

- `Cargo.toml` `[workspace.package].version` is `X.Y.Z` or `X.Y.Z-rc.N`.
- `agentd --version` reports `agentd X.Y.Z` or `agentd X.Y.Z-rc.N`.
- The container image exposes `org.tesserine.agentd.ref=<tag>`.
- The `/usr/local/bin/agentd` CLI extracted from the image reports the same
  version as the workspace.

This adapts the Tesserine release conventions in commons ADR-0006, ADR-0010,
and ADR-0011 to agentd's release surface.

## Pre-Release Gate

A releasable commit is on `main`, up to date with `origin/main`, and has a
clean working tree. `--allow-dirty` is not part of the release path.

Before tagging:

```sh
git checkout main
git pull --ff-only
git status --short
./scripts/release-check metadata
```

For a final tag-time check against a version already rolled into the source:

```sh
cargo build --release --locked --bin agentd
podman build --build-arg AGENTD_REF="vX.Y.Z" --tag "localhost/agentd:vX.Y.Z" .
./scripts/release-check release "vX.Y.Z" \
  --agentd-bin target/release/agentd \
  --container-image "localhost/agentd:vX.Y.Z"
```

Use `AGENTD_CONTAINER_RUNTIME=docker` when Docker should be used instead of
Podman.

## Atomic Release Operation

Stable cargo-workspace releases use the configured `cargo-release` path:

```sh
cargo release patch --execute
```

Use `minor` or `major` instead of `patch` when the release semantics require
that version level. The command bumps the workspace version, applies the
configured changelog roll, commits, creates an annotated tag named
`vX.Y.Z`, and pushes the commit plus tag.

Deployment release candidates use the same tool path:

```sh
cargo release rc --execute
```

Release candidates are immutable refs for deployment testing. A bad or
superseded candidate is corrected by cutting the next `rc.N`, not by rewriting
the existing tag.

## Post-Release Gate

The tag push runs `.github/workflows/release.yml`. That workflow verifies the
annotated tag, builds the release binary, builds a local container image with
`AGENTD_REF` set to the tag, verifies the workspace/binary/container identity,
extracts release notes from `CHANGELOG.md`, and publishes the GitHub Release.
Only `vX.Y.Z-rc.N` tags are published as GitHub prereleases.

Manual GitHub Release creation, when needed after a workflow failure, uses the
same notes source:

```sh
./scripts/release-check notes "vX.Y.Z" > /tmp/agentd-release-notes.md
gh release create "vX.Y.Z" \
  --title "agentd vX.Y.Z" \
  --notes-file /tmp/agentd-release-notes.md \
  --verify-tag
```

## Failure Modes

If a published tag points at source that violates the release identity checks,
the tag is invalid. If it has no external consumers, delete it locally and
remotely and re-run the release operation. If it has external consumers, leave
the bad tag in the public record and cut the next version.

If the GitHub Release workflow fails after the tag is valid, repair the
workflow or environment and create the GitHub Release from
`scripts/release-check notes`. Do not edit release notes by hand unless the
changelog section is also corrected in source.
