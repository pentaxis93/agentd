use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn read_workspace_file(path: &str) -> String {
    std::fs::read_to_string(workspace_root().join(path))
        .unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

fn read_workspace_toml(path: &str) -> toml::Value {
    read_workspace_file(path)
        .parse::<toml::Value>()
        .unwrap_or_else(|error| panic!("{path} should be valid TOML: {error}"))
}

fn string_array(values: &[&str]) -> Vec<toml::Value> {
    values
        .iter()
        .map(|value| toml::Value::String((*value).to_string()))
        .collect()
}

fn release_workflow_tag_patterns() -> Vec<String> {
    let workflow = read_workspace_file(".github/workflows/release.yml");
    let mut patterns = Vec::new();
    let mut in_tags = false;

    for line in workflow.lines() {
        if line == "    tags:" {
            in_tags = true;
            continue;
        }

        if in_tags {
            if !line.starts_with("      - ") {
                break;
            }
            patterns.push(
                line.trim()
                    .trim_start_matches("- ")
                    .trim_matches('"')
                    .to_string(),
            );
        }
    }

    patterns
}

fn line_number_containing(contents: &str, needle: &str) -> Option<usize> {
    contents
        .lines()
        .position(|line| line.contains(needle))
        .map(|index| index + 1)
}

fn first_line_number_containing_any(contents: &str, needles: &[&str]) -> Option<usize> {
    contents
        .lines()
        .position(|line| needles.iter().any(|needle| line.contains(needle)))
        .map(|index| index + 1)
}

#[test]
fn workspace_metadata_lists_only_grounded_crates() {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(workspace_root())
        .output()
        .expect("cargo metadata should run");

    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("cargo metadata stdout should be utf-8");

    assert!(stdout.contains("\"name\":\"agentd\""));
    assert!(stdout.contains("\"name\":\"agentd-runner\""));
    assert!(stdout.contains("\"name\":\"agentd-scheduler\""));
    assert!(
        !stdout.contains("\"name\":\"mcp-transport\""),
        "workspace metadata still includes mcp-transport"
    );
    assert!(
        !stdout.contains("\"name\":\"forgejo-mcp\""),
        "workspace metadata still includes forgejo-mcp"
    );
}

#[test]
fn cargo_release_configuration_lives_at_the_workspace_root() {
    assert!(
        workspace_root().join("release.toml").is_file(),
        "release.toml should exist at the workspace root"
    );
}

#[test]
fn cargo_release_configuration_pins_compliance_critical_values() {
    let config = read_workspace_toml("release.toml");

    assert_eq!(
        config.get("release").and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(config.get("tag").and_then(toml::Value::as_bool), Some(true));
    assert_eq!(
        config.get("verify").and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        config.get("sign-commit").and_then(toml::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        config.get("sign-tag").and_then(toml::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        config
            .get("pre-release-replacements")
            .and_then(toml::Value::as_array)
            .map(Vec::as_slice),
        Some(&[][..])
    );
    assert_eq!(
        config
            .get("pre-release-hook")
            .and_then(toml::Value::as_array),
        Some(&string_array(&["true"]))
    );
    assert_eq!(
        config
            .get("push-options")
            .and_then(toml::Value::as_array)
            .map(Vec::as_slice),
        Some(&[][..])
    );
    assert_eq!(
        config
            .get("owners")
            .and_then(toml::Value::as_array)
            .map(Vec::as_slice),
        Some(&[][..])
    );
    assert_eq!(
        config
            .get("enable-features")
            .and_then(toml::Value::as_array)
            .map(Vec::as_slice),
        Some(&[][..])
    );
    assert_eq!(
        config
            .get("enable-all-features")
            .and_then(toml::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        config.get("metadata").and_then(toml::Value::as_str),
        Some("optional")
    );
    assert_eq!(
        config.get("certs-source").and_then(toml::Value::as_str),
        Some("webpki")
    );
}

#[test]
fn workspace_packages_inherit_the_workspace_version() {
    for manifest in [
        "crates/agentd/Cargo.toml",
        "crates/agentd-runner/Cargo.toml",
        "crates/agentd-scheduler/Cargo.toml",
    ] {
        let manifest_toml = read_workspace_toml(manifest);
        let version = manifest_toml
            .get("package")
            .and_then(|package| package.get("version"))
            .unwrap_or_else(|| panic!("{manifest} should declare package.version"));

        assert_eq!(
            version.get("workspace").and_then(toml::Value::as_bool),
            Some(true),
            "{manifest} should inherit version.workspace"
        );
    }
}

#[test]
fn changelog_keeps_an_unreleased_section_for_release_rolls() {
    let changelog = read_workspace_file("CHANGELOG.md");

    assert!(
        changelog.contains("\n## [Unreleased]\n"),
        "CHANGELOG.md should keep a rollable [Unreleased] heading"
    );
}

#[test]
fn changelog_records_release_configuration_user_config_hardening() {
    let changelog = read_workspace_file("CHANGELOG.md");

    assert!(
        changelog.contains("compliance-critical `cargo-release` behavior")
            && changelog.contains("user-level config"),
        "CHANGELOG.md should record the cargo-release user-config hardening"
    );
}

#[test]
fn release_adoption_verification_checks_hostile_user_config_cannot_override_workspace_pins() {
    let script = read_workspace_file("scripts/verify-release-adoption.sh");

    assert!(
        script.contains("hostile_home=\"$scratch/hostile-home\""),
        "release verification should create an isolated hostile cargo-release home"
    );
    assert!(
        script.contains("release = false") && script.contains("tag = false"),
        "release verification should model hostile release and tag overrides"
    );
    assert!(
        script.contains("pre-release-hook = [\"/bin/false\"]"),
        "release verification should model hostile hook inheritance"
    );
    assert!(
        script.contains("XDG_CONFIG_HOME=\"$hostile_home/.config\""),
        "release verification should scope XDG_CONFIG_HOME to the hostile config root"
    );
    assert!(
        script.contains("cargo release config"),
        "release verification should inspect resolved cargo-release config"
    );
    assert!(
        script.contains("release = true") && script.contains("tag = true"),
        "release verification should assert workspace-pinned release and tag behavior"
    );
    assert!(
        script.contains("pre-release-hook = [\"true\"]"),
        "release verification should assert the workspace-pinned no-op hook"
    );
}

#[test]
fn release_adoption_verification_exercises_the_rc_release_path() {
    let script = read_workspace_file("scripts/verify-release-adoption.sh");

    assert!(
        script.contains("cargo release rc --execute --no-confirm"),
        "release verification should run the documented RC release command"
    );
    assert!(
        script.contains("./scripts/release-check release \"$tag_name\""),
        "release verification should prove the RC tag passes release-check release"
    );
}

#[test]
fn release_adoption_verification_exercises_release_check_for_stable_and_rc_tags() {
    let script = read_workspace_file("scripts/verify-release-adoption.sh");

    assert!(
        script.contains(
            "verify_fresh_release_checkout \"$stable_fresh_checkout\" \"$stable_remote_repo\" \"$tag_name\""
        ),
        "release verification should run release-check release against the stable tag checkout"
    );
    assert!(
        script.contains(
            "verify_fresh_release_checkout \"$rc_fresh_checkout\" \"$rc_remote_repo\" \"$tag_name\""
        ),
        "release verification should run release-check release against the RC tag checkout"
    );
}

#[test]
fn release_adoption_verification_uses_release_check_as_the_workspace_version_parser() {
    let script = read_workspace_file("scripts/verify-release-adoption.sh");

    assert!(
        script.contains("source \"$workspace_root/scripts/release-check\""),
        "release verification should source release-check instead of defining a second parser"
    );
    assert!(
        !script.contains("sed -n '/^\\[workspace.package\\]/"),
        "release verification should not parse workspace versions with its own sed expression"
    );
}

#[test]
fn release_candidate_documentation_uses_the_shared_cargo_release_path() {
    let releasing = read_workspace_file("RELEASING.md");

    assert!(
        releasing.contains("cargo release rc --execute"),
        "RELEASING.md should document the shared RC cargo-release path"
    );
    assert!(
        !releasing.contains("cargo release --config release.toml --isolated rc --execute"),
        "RELEASING.md should not document an isolated RC release path"
    );
}

#[test]
fn changelog_release_rolls_are_enabled_for_release_candidates() {
    let manifest = read_workspace_toml("crates/agentd/Cargo.toml");
    let replacements = manifest
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("release"))
        .and_then(|release| release.get("pre-release-replacements"))
        .and_then(toml::Value::as_array)
        .expect("agentd should declare cargo-release replacements");

    assert!(
        replacements.iter().any(|replacement| {
            replacement.get("file").and_then(toml::Value::as_str) == Some("../../CHANGELOG.md")
                && replacement.get("prerelease").and_then(toml::Value::as_bool) == Some(true)
        }),
        "CHANGELOG.md replacement should run during RC releases"
    );
}

#[test]
fn github_release_workflow_delegates_v_prefixed_tags_to_release_check() {
    let patterns = release_workflow_tag_patterns();

    assert_eq!(
        patterns,
        ["v*"],
        "release workflow should delegate v-prefixed tag shape validation to release-check"
    );
}

#[test]
fn github_release_workflow_validates_tags_before_expensive_release_work() {
    let workflow = read_workspace_file(".github/workflows/release.yml");
    let validation_line = line_number_containing(
        &workflow,
        "run: ./scripts/release-check release \"$GITHUB_REF_NAME\"",
    )
    .expect("release workflow should validate the tag with release-check");
    let container_setup_line = line_number_containing(&workflow, "sudo apt-get install -y podman")
        .or_else(|| line_number_containing(&workflow, "podman build"))
        .expect("release workflow should install or run container tooling");

    assert!(
        validation_line < container_setup_line,
        "release workflow should validate the release tag before container setup"
    );
}

#[test]
fn github_release_workflow_establishes_tag_trust_before_repository_code_execution() {
    let workflow = read_workspace_file(".github/workflows/release.yml");
    let annotated_tag_line = line_number_containing(&workflow, "git cat-file -t")
        .expect("release workflow should require an annotated tag");
    let main_ancestry_line = line_number_containing(&workflow, "git merge-base --is-ancestor")
        .expect("release workflow should require the tag target on main");
    let repository_code_line = first_line_number_containing_any(
        &workflow,
        &[
            "./scripts/release-check release \"$GITHUB_REF_NAME\"",
            "cargo build",
            "podman build",
        ],
    )
    .expect("release workflow should run trusted repository release code");

    assert!(
        annotated_tag_line < repository_code_line && main_ancestry_line < repository_code_line,
        "release workflow should establish tag trust before running repository code"
    );
}

#[test]
fn github_release_publication_is_not_coupled_to_path_filters() {
    let workflow = read_workspace_file(".github/workflows/release.yml");

    assert!(
        workflow.contains("    tags:"),
        "release publication workflow should be triggered by release tags"
    );
    assert!(
        !workflow.contains("    paths:"),
        "release publication workflow should not path-filter tag pushes"
    );
}

#[test]
fn release_metadata_workflow_keeps_path_filtered_branch_and_pr_checks() {
    let workflow = read_workspace_file(".github/workflows/release-metadata.yml");

    assert!(
        workflow.contains("name: Release Metadata"),
        "release metadata workflow should exist separately from tag publication"
    );
    assert!(
        workflow.contains("  push:") && workflow.contains("    branches: [main]"),
        "release metadata workflow should run on main branch pushes"
    );
    assert!(
        workflow.contains("  pull_request:") && workflow.contains("    paths:"),
        "release metadata workflow should retain PR path filtering"
    );
    assert!(
        workflow.contains("./scripts/release-check metadata"),
        "release metadata workflow should run release-check metadata"
    );
}

#[test]
fn github_release_workflow_marks_only_rc_tags_as_prereleases() {
    let workflow = read_workspace_file(".github/workflows/release.yml");

    assert!(
        workflow
            .contains("^v(0|[1-9][0-9]*)[.](0|[1-9][0-9]*)[.](0|[1-9][0-9]*)-rc[.][1-9][0-9]*$"),
        "release workflow should mark only documented RC tags as GitHub prereleases"
    );
    assert!(
        !workflow.contains("[[ \"$GITHUB_REF_NAME\" == *-* ]]"),
        "release workflow should not treat every hyphenated tag as a prerelease"
    );
}

#[test]
fn release_documentation_describes_rc_only_github_prerelease_publication() {
    let releasing = read_workspace_file("RELEASING.md");

    assert!(
        releasing.contains("Only `vX.Y.Z-rc.N` tags are published as GitHub prereleases."),
        "RELEASING.md should describe prerelease publication with the same RC precision as the workflow"
    );
    assert!(
        !releasing
            .contains("Tags containing a prerelease suffix are published as GitHub prereleases."),
        "RELEASING.md should not imply every prerelease suffix is published as a GitHub prerelease"
    );
}

#[test]
fn removed_crate_directories_are_absent() {
    let workspace_root = workspace_root();

    assert!(
        !workspace_root.join("crates/mcp-transport").exists(),
        "crates/mcp-transport still exists"
    );
    assert!(
        !workspace_root.join("crates/forgejo-mcp").exists(),
        "crates/forgejo-mcp still exists"
    );
}

#[test]
fn workspace_docs_describe_only_the_three_grounded_crates() {
    let readme = read_workspace_file("README.md");
    let architecture = read_workspace_file("ARCHITECTURE.md");
    let agents = read_workspace_file("AGENTS.md");

    for document in [&readme, &architecture, &agents] {
        assert!(
            !document.contains("mcp-transport"),
            "documentation still references mcp-transport"
        );
        assert!(
            !document.contains("forgejo-mcp"),
            "documentation still references forgejo-mcp"
        );
    }

    assert!(architecture.contains("`agentd`"));
    assert!(architecture.contains("`agentd-runner`"));
    assert!(architecture.contains("`agentd-scheduler`"));
}

#[test]
fn architecture_describes_uniform_socket_intake_for_session_triggers() {
    let architecture = read_workspace_file("ARCHITECTURE.md");

    assert!(
        architecture.contains("single intake for all session triggers"),
        "architecture should describe the Unix socket as the single session intake"
    );
    assert!(
        architecture.contains("scheduler is a socket client"),
        "architecture should describe the scheduler as a socket client"
    );
    assert!(
        !architecture
            .contains("The scheduler passes agent identity plus mission context to the runner."),
        "architecture should not describe the scheduler as handing work directly to the runner"
    );
}

#[test]
fn workspace_docs_declare_same_build_socket_policy() {
    let readme = read_workspace_file("README.md");
    let architecture = read_workspace_file("ARCHITECTURE.md");

    assert!(
        readme.contains("must restart the daemon after replacing the binary"),
        "README should declare the restart requirement after replacing the binary"
    );
    assert!(
        readme.contains("daemon and CLI must be the same build"),
        "README should declare the same-build daemon/CLI requirement"
    );
    assert!(
        architecture.contains("internal and unversioned"),
        "architecture should describe the socket protocol as internal and unversioned"
    );
    assert!(
        architecture.contains("daemon and CLI must be the same build"),
        "architecture should declare the same-build daemon/CLI requirement"
    );
    assert!(
        readme.contains("$XDG_RUNTIME_DIR/agentd/agentd.sock")
            && readme.contains("does not fall back to `/tmp` or `/run`"),
        "README should describe deterministic XDG socket resolution"
    );
    assert!(
        architecture.contains("$XDG_RUNTIME_DIR/agentd/agentd.sock")
            && architecture.contains("There is no implicit `/tmp` or `/run` fallback"),
        "architecture should describe deterministic XDG socket resolution"
    );
}

#[test]
fn workspace_docs_describe_persistent_audit_record_contract() {
    let readme = read_workspace_file("README.md");
    let architecture = read_workspace_file("ARCHITECTURE.md");

    assert!(
        readme.contains("/var/lib/tesserine/audit"),
        "README should describe the persistent audit record root"
    );
    assert!(
        architecture.contains("/var/lib/tesserine/audit"),
        "architecture should describe the persistent audit record root"
    );
    assert!(
        architecture.contains("non-configurable `.runa/store/` and `.runa/workspace/`"),
        "architecture should document full audit coverage for runa's fixed workspace layout"
    );
    assert!(
        !architecture.contains("artifacts_dir"),
        "architecture should not describe removed artifacts_dir configurability"
    );
    assert!(
        architecture.contains("accumulate") && architecture.contains("indefinitely"),
        "architecture should document the lack of retention policy"
    );
    assert!(
        architecture.contains("single-tenant"),
        "architecture should document the host security assumption"
    );
    assert!(
        architecture.contains("incomplete"),
        "architecture should explain incomplete session records"
    );
    assert!(
        architecture.contains("runner.lifecycle_failure")
            && architecture.contains("session audit finalization"),
        "architecture should explain tracing-based disambiguation for incomplete session records"
    );
    assert!(
        architecture.contains("must not contain a `.runa` entry"),
        "architecture should describe the repo-root .runa contract"
    );
}
