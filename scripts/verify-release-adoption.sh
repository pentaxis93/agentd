#!/usr/bin/env bash
set -euo pipefail

workspace_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
real_home="${HOME:?}"
cargo_home="${CARGO_HOME:-$real_home/.cargo}"
rustup_home="${RUSTUP_HOME:-$real_home/.rustup}"

if ! cargo release --version >/dev/null 2>&1; then
    echo "cargo-release is required: install the cargo-release cargo subcommand" >&2
    exit 1
fi

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

stable_source_repo="$scratch/stable-source"
stable_remote_repo="$scratch/stable-origin.git"
stable_fresh_checkout="$scratch/stable-fresh-checkout"
stable_release_log="$scratch/stable-cargo-release.log"
rc_source_repo="$scratch/rc-source"
rc_remote_repo="$scratch/rc-origin.git"
rc_fresh_checkout="$scratch/rc-fresh-checkout"
rc_release_log="$scratch/rc-cargo-release.log"
hostile_home="$scratch/hostile-home"
resolved_config_log="$scratch/cargo-release-config.log"

seed_release_repo() {
    local source_repo="$1"
    local remote_repo="$2"

    mkdir "$source_repo"
    tar \
        --exclude=.git \
        --exclude=target \
        --exclude=.idea \
        --exclude=.vscode \
        -C "$workspace_root" \
        -cf - . \
        | tar -C "$source_repo" -xf -

    git -C "$source_repo" init -q
    git -C "$source_repo" config user.name "agentd release verification"
    git -C "$source_repo" config user.email "agentd-release-verification@example.invalid"
    git -C "$source_repo" checkout -q -b main
    git -C "$source_repo" -c core.excludesFile=/dev/null add .
    git -C "$source_repo" commit -q -m "test: seed release verification"

    git init --bare -q "$remote_repo"
    git -C "$source_repo" remote add origin "$remote_repo"
    git -C "$source_repo" push -q -u origin main
}

workspace_version() {
    sed -n '/^\[workspace.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p}' "$1/Cargo.toml"
}

assert_release_commit_state() {
    local source_repo="$1"
    local remote_repo="$2"
    local tag_name="$3"
    local version="$4"

    local release_commit tag_commit
    release_commit="$(git -C "$source_repo" rev-parse HEAD)"
    tag_commit="$(git -C "$source_repo" rev-list -n 1 "$tag_name")"

    if ! grep -Fq "## [$version] — $(date +%F)" "$source_repo/CHANGELOG.md"; then
        echo "CHANGELOG.md was not rolled to [$version] with today's date" >&2
        exit 1
    fi

    if [[ "$(git -C "$source_repo" cat-file -t "$tag_name")" != "tag" ]]; then
        echo "$tag_name is not an annotated tag" >&2
        exit 1
    fi

    if [[ "$tag_commit" != "$release_commit" ]]; then
        echo "$tag_name does not point at the release commit" >&2
        exit 1
    fi

    if ! git --git-dir="$remote_repo" rev-parse --verify --quiet "refs/heads/main" >/dev/null; then
        echo "release branch was not pushed to the remote" >&2
        exit 1
    fi

    if ! git --git-dir="$remote_repo" rev-parse --verify --quiet "refs/tags/$tag_name" >/dev/null; then
        echo "$tag_name was not pushed to the remote" >&2
        exit 1
    fi
}

seed_release_repo "$stable_source_repo" "$stable_remote_repo"
old_version="$(workspace_version "$stable_source_repo")"

mkdir -p "$hostile_home/.config/cargo-release"
cat >"$hostile_home/.config/cargo-release/release.toml" <<'EOF'
release = false
tag = false
verify = false
pre-release-hook = ["/bin/false"]
push-options = ["--repo", "unexpected"]
owners = ["unexpected"]
enable-features = ["unexpected"]
enable-all-features = true
metadata = "required"
certs-source = "native"
EOF

(
    cd "$stable_source_repo"
    HOME="$hostile_home" \
        XDG_CONFIG_HOME="$hostile_home/.config" \
        CARGO_HOME="$cargo_home" \
        RUSTUP_HOME="$rustup_home" \
        cargo release config >"$resolved_config_log"
)

assert_resolved_config() {
    local expected="$1"

    if ! grep -Fq "$expected" "$resolved_config_log"; then
        echo "cargo-release config did not preserve workspace pin: $expected" >&2
        exit 1
    fi
}

assert_resolved_config 'release = true'
assert_resolved_config 'tag = true'
assert_resolved_config 'verify = true'
assert_resolved_config 'pre-release-hook = ["true"]'
assert_resolved_config 'push-options = []'
assert_resolved_config 'owners = []'
assert_resolved_config 'enable-features = []'
assert_resolved_config 'enable-all-features = false'
assert_resolved_config 'metadata = "optional"'
assert_resolved_config 'certs-source = "webpki"'

(
    cd "$stable_source_repo"
    cargo release patch --execute --no-confirm 2>&1 | tee "$stable_release_log"
)

new_version="$(workspace_version "$stable_source_repo")"
tag_name="v$new_version"

if [[ "$new_version" == "$old_version" ]]; then
    echo "workspace version did not change from $old_version" >&2
    exit 1
fi

assert_release_commit_state "$stable_source_repo" "$stable_remote_repo" "$tag_name" "$new_version"

if grep -Fq "Uploading" "$stable_release_log"; then
    echo "release log indicates registry publishing occurred" >&2
    exit 1
fi

git clone -q "$stable_remote_repo" "$stable_fresh_checkout"
git -C "$stable_fresh_checkout" checkout -q "$tag_name"
cargo build --release -p agentd --manifest-path "$stable_fresh_checkout/Cargo.toml"

version_output="$("$stable_fresh_checkout/target/release/agentd" --version)"
if [[ "$version_output" != "agentd $new_version" ]]; then
    echo "agentd --version reported '$version_output', expected 'agentd $new_version'" >&2
    exit 1
fi

echo "verified stable release adoption for $tag_name"

seed_release_repo "$rc_source_repo" "$rc_remote_repo"
old_version="$(workspace_version "$rc_source_repo")"

(
    cd "$rc_source_repo"
    cargo release rc --execute --no-confirm 2>&1 | tee "$rc_release_log"
)

new_version="$(workspace_version "$rc_source_repo")"
tag_name="v$new_version"

if [[ "$new_version" == "$old_version" ]]; then
    echo "workspace version did not change from $old_version" >&2
    exit 1
fi

assert_release_commit_state "$rc_source_repo" "$rc_remote_repo" "$tag_name" "$new_version"

if grep -Fq "Uploading" "$rc_release_log"; then
    echo "RC release log indicates registry publishing occurred" >&2
    exit 1
fi

git clone -q "$rc_remote_repo" "$rc_fresh_checkout"
git -C "$rc_fresh_checkout" checkout -q "$tag_name"
(
    cd "$rc_fresh_checkout"
    ./scripts/release-check release "$tag_name"
)

echo "verified RC release adoption for $tag_name"
