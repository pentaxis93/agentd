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
