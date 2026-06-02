use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

use agentd::config::{Config, ConfigError, DaemonConfig};
use agentd::{RuntimePathError, default_daemon_runtime_paths};
use agentd_runner::MountTargetValidationError;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn example_config() -> String {
    std::fs::read_to_string(workspace_root().join("examples/agentd.toml"))
        .expect("example config should be readable")
}

fn assert_invalid_agent_name_parse_error(name: &str) {
    let error = Config::from_str(&format!(
        r#"
[[agents]]
name = "{name}"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#
    ))
    .expect_err("invalid agent names should be rejected at parse time");

    match error {
        ConfigError::InvalidAgentName { name: invalid_name } => assert_eq!(invalid_name, name),
        other => panic!("expected invalid agent name error, got {other}"),
    }
}

fn write_temp_config(name: &str, contents: &str) -> PathBuf {
    let unique = format!(
        "agentd-config-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(unique);

    std::fs::create_dir_all(&dir).expect("temp test directory should be created");

    let path = dir.join("agentd.toml");
    std::fs::write(&path, contents).expect("temp config should be written");
    path
}

fn set_xdg_runtime_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agentd-xdg-runtime-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &path);
    }
    path
}

fn write_temp_config_under(base_dir: &Path, name: &str, contents: &str) -> PathBuf {
    let unique = format!(
        "agentd-config-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    );
    let dir = base_dir.join(unique);

    std::fs::create_dir_all(&dir).expect("temp test directory should be created");

    let path = dir.join("agentd.toml");
    std::fs::write(&path, contents).expect("temp config should be written");
    path
}

fn assert_invalid_mount_target_parse_error(
    target: &str,
    expected_error: MountTargetValidationError,
) {
    let target = target.replace('\\', "\\\\").replace('"', "\\\"");
    let error = Config::from_str(&format!(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.mounts]]
source = "/home/core/.claude"
target = "{target}"
read_only = true
"#
    ))
    .expect_err("runner-invalid mount targets should be rejected during config parse");

    match error {
        ConfigError::InvalidMountTarget { agent, error } => {
            assert_eq!(agent, "site-builder");
            assert_eq!(error, expected_error);
        }
        other => panic!("expected invalid mount target error, got {other}"),
    }
}

#[test]
fn parses_agents_with_declarative_command_argv() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("declarative-command");
    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("agent config should parse");

    let agent = config
        .agent("site-builder")
        .expect("site-builder agent should exist");

    assert_eq!(config.agents().len(), 1);
    assert_eq!(agent.name(), "site-builder");
    assert_eq!(agent.agent_command(), ["site-builder", "exec"]);
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn parses_example_config_into_static_agent_settings() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("example-config");
    let config = Config::from_str(&example_config()).expect("example config should parse");
    let site_builder = config
        .agent("site-builder")
        .expect("site-builder agent should exist");
    let code_reviewer = config
        .agent("code-reviewer")
        .expect("code-reviewer agent should exist");

    assert_eq!(config.agents().len(), 2);
    let runtime_paths =
        default_daemon_runtime_paths().expect("xdg runtime dir should provide daemon defaults");
    assert_eq!(config.daemon().socket_path(), runtime_paths.socket_path());
    assert_eq!(config.daemon().pid_file(), runtime_paths.pid_file());
    assert_eq!(site_builder.name(), "site-builder");
    assert_eq!(
        site_builder.base_image(),
        "ghcr.io/example/site-builder:latest"
    );
    assert_eq!(site_builder.methodology_dir(), Path::new("../groundwork"));
    assert_eq!(
        site_builder.repo(),
        Some("https://github.com/pentaxis93/agentd.git")
    );
    assert_eq!(site_builder.schedule(), Some("*/15 * * * *"));
    assert_eq!(
        site_builder.repo_token_source(),
        Some("SITE_BUILDER_REPO_TOKEN")
    );
    assert_eq!(site_builder.forge(), "github");
    assert_eq!(site_builder.agent_command(), ["site-builder", "exec"]);
    assert_eq!(site_builder.credentials().len(), 1);
    assert_eq!(site_builder.credentials()[0].name(), "GITHUB_TOKEN");
    assert_eq!(
        site_builder.credentials()[0].source(),
        "AGENTD_GITHUB_TOKEN"
    );
    assert!(site_builder.mounts().is_empty());

    assert_eq!(code_reviewer.name(), "code-reviewer");
    assert_eq!(
        code_reviewer.base_image(),
        "ghcr.io/example/code-reviewer:latest"
    );
    assert_eq!(code_reviewer.methodology_dir(), Path::new("../groundwork"));
    assert_eq!(
        code_reviewer.repo(),
        Some("https://github.com/pentaxis93/agentd.git")
    );
    assert_eq!(code_reviewer.schedule(), None);
    assert_eq!(
        code_reviewer.repo_token_source(),
        Some("CODE_REVIEWER_REPO_TOKEN")
    );
    assert_eq!(code_reviewer.forge(), "github");
    assert_eq!(code_reviewer.agent_command(), ["code-reviewer", "exec"]);
    assert_eq!(code_reviewer.credentials().len(), 1);
    assert_eq!(code_reviewer.credentials()[0].name(), "GITHUB_TOKEN");
    assert_eq!(
        code_reviewer.credentials()[0].source(),
        "AGENTD_GITHUB_TOKEN"
    );
    assert!(code_reviewer.mounts().is_empty());
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn loading_config_resolves_relative_methodology_path_from_file_location() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("relative-methodology");
    let path = write_temp_config(
        "relative-path",
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    );

    let config = Config::load(&path).expect("config file should parse");
    let agent = config.agent("site-builder").expect("agent should exist");

    assert_eq!(
        agent.methodology_dir(),
        path.parent()
            .expect("config file should have a parent directory")
            .join("../groundwork")
    );
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn loading_config_from_a_relative_path_resolves_methodology_dir_from_an_absolute_base_dir() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("relative-load-methodology");
    let current_dir = std::env::current_dir().expect("current directory should be available");
    let path = write_temp_config_under(
        &current_dir.join("target"),
        "relative-load-path",
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    );
    let relative_path = path
        .strip_prefix(&current_dir)
        .expect("fixture path should be under the current directory");

    let config = Config::load(relative_path).expect("config file should parse");
    let agent = config.agent("site-builder").expect("agent should exist");

    assert_eq!(
        agent.methodology_dir(),
        path.parent()
            .expect("config file should have a parent directory")
            .join("../groundwork")
    );
    assert!(
        agent.methodology_dir().is_absolute(),
        "loaded methodology_dir should be absolute when loaded from a file"
    );
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn loading_config_resolves_relative_daemon_paths_from_file_location() {
    let path = write_temp_config(
        "relative-daemon-paths",
        r#"
[daemon]
socket_path = "runtime/agentd.sock"
pid_file = "runtime/agentd.pid"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    );

    let config = Config::load(&path).expect("config file should parse");
    let base_dir = path
        .parent()
        .expect("config file should have a parent directory");

    assert_eq!(
        config.daemon().socket_path(),
        base_dir.join("runtime/agentd.sock")
    );
    assert_eq!(
        config.daemon().pid_file(),
        base_dir.join("runtime/agentd.pid")
    );
}

#[test]
fn loading_config_from_a_relative_path_resolves_relative_daemon_paths_from_an_absolute_base_dir() {
    let current_dir = std::env::current_dir().expect("current directory should be available");
    let path = write_temp_config_under(
        &current_dir.join("target"),
        "relative-daemon-load-path",
        r#"
[daemon]
socket_path = "runtime/agentd.sock"
pid_file = "runtime/agentd.pid"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    );
    let relative_path = path
        .strip_prefix(&current_dir)
        .expect("fixture path should be under the current directory");

    let config = Config::load(relative_path).expect("config file should parse");
    let base_dir = path
        .parent()
        .expect("config file should have a parent directory");

    assert_eq!(
        config.daemon().socket_path(),
        base_dir.join("runtime/agentd.sock")
    );
    assert_eq!(
        config.daemon().pid_file(),
        base_dir.join("runtime/agentd.pid")
    );
    assert!(
        config.daemon().socket_path().is_absolute(),
        "loaded socket_path should be absolute when loaded from a file"
    );
    assert!(
        config.daemon().pid_file().is_absolute(),
        "loaded pid_file should be absolute when loaded from a file"
    );
}

#[test]
fn daemon_instance_id_is_stable_for_the_same_runtime_paths() {
    let config = Config::from_str(
        r#"
[daemon]
socket_path = "/run/agentd/a.sock"
pid_file = "/run/agentd/a.pid"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse");

    let first = config
        .daemon()
        .daemon_instance_id()
        .expect("daemon instance id should resolve");
    let second = config
        .daemon()
        .daemon_instance_id()
        .expect("daemon instance id should resolve");

    assert_eq!(first, second);
    assert_eq!(first.len(), 8);
    assert!(
        first
            .chars()
            .all(|character| matches!(character, '0'..='9' | 'a'..='f')),
        "daemon instance id should be lowercase hex: {first}"
    );
}

#[test]
fn daemon_instance_id_changes_when_daemon_runtime_paths_change() {
    let first = Config::from_str(
        r#"
[daemon]
socket_path = "/run/agentd/a.sock"
pid_file = "/run/agentd/a.pid"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("first config should parse");
    let second = Config::from_str(
        r#"
[daemon]
socket_path = "/run/agentd/b.sock"
pid_file = "/run/agentd/a.pid"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("second config should parse");

    assert_ne!(
        first
            .daemon()
            .daemon_instance_id()
            .expect("first daemon instance id should resolve"),
        second
            .daemon()
            .daemon_instance_id()
            .expect("second daemon instance id should resolve")
    );
}

#[test]
fn daemon_instance_id_rejects_relative_daemon_runtime_paths_from_str_configs() {
    let config = Config::from_str(
        r#"
[daemon]
socket_path = "runtime/agentd.sock"
pid_file = "runtime/agentd.pid"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse");

    let error = config
        .daemon()
        .daemon_instance_id()
        .expect_err("relative daemon runtime paths should be rejected");

    match error {
        ConfigError::RelativeDaemonRuntimePath { field, path } => {
            assert_eq!(field, "daemon.socket_path");
            assert_eq!(path, Path::new("runtime/agentd.sock"));
        }
        other => panic!("expected relative daemon runtime path error, got {other}"),
    }
}

#[test]
fn parses_default_daemon_paths_when_daemon_section_is_omitted() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("config-defaults");
    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse with daemon defaults");

    let runtime_paths =
        default_daemon_runtime_paths().expect("xdg runtime dir should provide daemon defaults");
    assert_eq!(config.daemon().socket_path(), runtime_paths.socket_path());
    assert_eq!(config.daemon().pid_file(), runtime_paths.pid_file());
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn default_daemon_paths_require_xdg_runtime_dir_when_omitted() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }

    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect_err("omitted daemon paths should require xdg runtime dir");

    match error {
        ConfigError::DefaultDaemonRuntimePaths(RuntimePathError::XdgRuntimeDirUnavailable) => {}
        other => panic!("expected missing xdg runtime dir error, got {other}"),
    }
}

#[test]
fn daemon_instance_id_rejects_relative_pid_file_after_absolute_socket_path() {
    let config = Config::from_str(
        r#"
[daemon]
socket_path = "/run/agentd/agentd.sock"
pid_file = "runtime/agentd.pid"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse");

    let error = config
        .daemon()
        .daemon_instance_id()
        .expect_err("relative pid file should be rejected");

    match error {
        ConfigError::RelativeDaemonRuntimePath { field, path } => {
            assert_eq!(field, "daemon.pid_file");
            assert_eq!(path, Path::new("runtime/agentd.pid"));
        }
        other => panic!("expected relative daemon runtime path error, got {other}"),
    }
}

#[test]
fn parses_explicit_daemon_paths() {
    let config = Config::from_str(
        r#"
[daemon]
socket_path = "/tmp/agentd-test.sock"
pid_file = "/tmp/agentd-test.pid"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse explicit daemon paths");

    assert_eq!(
        config.daemon().socket_path(),
        Path::new("/tmp/agentd-test.sock")
    );
    assert_eq!(
        config.daemon().pid_file(),
        Path::new("/tmp/agentd-test.pid")
    );
}

#[test]
fn parses_explicit_daemon_audit_root() {
    let config = Config::from_str(
        r#"
[daemon]
socket_path = "/tmp/agentd-test.sock"
pid_file = "/tmp/agentd-test.pid"
audit_root = "/srv/agentd/audit"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse explicit daemon paths");

    assert_eq!(
        config
            .daemon()
            .resolve_audit_root()
            .expect("audit root should resolve"),
        Path::new("/srv/agentd/audit")
    );
}

#[test]
fn loading_daemon_config_resolves_relative_audit_root_from_file_location() {
    let path = write_temp_config(
        "daemon-only-relative-audit-root",
        r#"
[daemon]
socket_path = "runtime/agentd.sock"
pid_file = "runtime/agentd.pid"
audit_root = "state/audit"

[[agents]]
name = "Site-Builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    );

    let config = DaemonConfig::load(&path).expect("daemon config should parse");
    let base_dir = path
        .parent()
        .expect("config file should have a parent directory");

    assert_eq!(
        config
            .resolve_audit_root()
            .expect("audit root should resolve"),
        base_dir.join("state/audit")
    );
}

#[test]
fn daemon_audit_root_uses_xdg_state_home_before_home_fallback() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("audit-xdg-state");
    unsafe {
        std::env::set_var("XDG_STATE_HOME", "/tmp/xdg-state-home");
        std::env::set_var("HOME", "/tmp/home-fallback");
    }

    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse");

    assert_eq!(
        config
            .daemon()
            .resolve_audit_root()
            .expect("audit root should resolve"),
        Path::new("/tmp/xdg-state-home/tesserine/audit")
    );

    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
        std::env::remove_var("XDG_STATE_HOME");
        std::env::remove_var("HOME");
    }
}

#[test]
fn daemon_audit_root_falls_back_to_home_local_state_when_xdg_state_home_is_unset() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("audit-home-fallback");
    unsafe {
        std::env::remove_var("XDG_STATE_HOME");
        std::env::set_var("HOME", "/tmp/home-fallback");
    }

    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse");

    assert_eq!(
        config
            .daemon()
            .resolve_audit_root()
            .expect("audit root should resolve"),
        Path::new("/tmp/home-fallback/.local/state/tesserine/audit")
    );

    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
        std::env::remove_var("HOME");
    }
}

#[test]
fn daemon_audit_root_requires_a_usable_default_when_not_explicitly_configured() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("audit-missing-default");
    unsafe {
        std::env::remove_var("XDG_STATE_HOME");
        std::env::remove_var("HOME");
    }

    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse");

    let error = config
        .daemon()
        .resolve_audit_root()
        .expect_err("missing environment-backed audit root should fail");

    assert!(
        error.to_string().contains("audit root"),
        "expected audit-root error, got {error}"
    );
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn parses_repo_token_source_as_optional_clone_auth_lookup_key() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("repo-token-source");
    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
repo_token_source = "SITE_BUILDER_REPO_TOKEN"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse repo token source");

    let agent = config.agent("site-builder").expect("agent should exist");

    assert_eq!(agent.repo_token_source(), Some("SITE_BUILDER_REPO_TOKEN"));
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn parses_agent_forge_as_active_session_forge() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("agent-forge");
    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
forge = "sourcehut"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse active forge");

    let agent = config.agent("site-builder").expect("agent should exist");

    assert_eq!(agent.forge(), "sourcehut");
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn defaults_agent_forge_to_github_when_omitted() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("default-agent-forge");
    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should default active forge");

    let agent = config.agent("site-builder").expect("agent should exist");

    assert_eq!(agent.forge(), "github");
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn parses_agent_repo_as_optional_default_clone_url() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("agent-repo");
    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
repo = "https://example.com/agentd.git"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse repo");

    let agent = config.agent("site-builder").expect("agent should exist");

    assert_eq!(agent.repo(), Some("https://example.com/agentd.git"));
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn parses_agent_schedule_as_optional_cron_expression() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("agent-schedule");
    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
repo = "https://example.com/agentd.git"
schedule = "*/15 * * * *"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("config should parse schedule");

    let agent = config.agent("site-builder").expect("agent should exist");

    assert_eq!(agent.schedule(), Some("*/15 * * * *"));
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn parses_agent_mounts_as_operator_declared_bind_mounts() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("agent-mounts");
    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.mounts]]
source = "/home/core/.claude"
target = "/home/site-builder/.claude"
read_only = true

[[agents.mounts]]
source = "/var/lib/tesserine/audit"
target = "/home/site-builder/.runa"
read_only = false
"#,
    )
    .expect("config should parse declared mounts");

    let agent = config.agent("site-builder").expect("agent should exist");

    assert_eq!(agent.mounts().len(), 2);
    assert_eq!(agent.mounts()[0].source(), Path::new("/home/core/.claude"));
    assert_eq!(
        agent.mounts()[0].target(),
        Path::new("/home/site-builder/.claude")
    );
    assert!(agent.mounts()[0].read_only());
    assert_eq!(
        agent.mounts()[1].source(),
        Path::new("/var/lib/tesserine/audit")
    );
    assert_eq!(
        agent.mounts()[1].target(),
        Path::new("/home/site-builder/.runa")
    );
    assert!(!agent.mounts()[1].read_only());
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn rejects_agent_mount_sources_that_are_not_absolute() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.mounts]]
source = "../claude"
target = "/home/site-builder/.claude"
read_only = true
"#,
    )
    .expect_err("relative mount sources should be rejected");

    match error {
        ConfigError::MountSourceMustBeAbsolute { agent, source } => {
            assert_eq!(agent, "site-builder");
            assert_eq!(source, Path::new("../claude"));
        }
        other => panic!("expected absolute mount source error, got {other}"),
    }
}

#[test]
fn rejects_agent_mount_targets_that_are_not_absolute() {
    assert_invalid_mount_target_parse_error(
        ".claude",
        MountTargetValidationError::Invalid {
            path: PathBuf::from(".claude"),
        },
    );
}

#[test]
fn rejects_duplicate_agent_mount_targets() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.mounts]]
source = "/home/core/.claude"
target = "/home/site-builder/.claude"
read_only = true

[[agents.mounts]]
source = "/var/lib/tesserine/audit"
target = "/home/site-builder/.claude"
read_only = false
"#,
    )
    .expect_err("duplicate mount targets should be rejected");

    match error {
        ConfigError::DuplicateMountTarget { agent, target } => {
            assert_eq!(agent, "site-builder");
            assert_eq!(target, Path::new("/home/site-builder/.claude"));
        }
        other => panic!("expected duplicate mount target error, got {other}"),
    }
}

#[test]
fn load_rejects_overlapping_agent_mount_targets() {
    let path = write_temp_config(
        "overlapping-mount-targets",
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.mounts]]
source = "/home/core/.config"
target = "/home/site-builder/.config"
read_only = true

[[agents.mounts]]
source = "/home/core/.config/claude"
target = "/home/site-builder/.config/claude"
read_only = true
"#,
    );

    let error = Config::load(&path).expect_err("overlapping mount targets should be rejected");

    match error {
        ConfigError::OverlappingMountTargets {
            agent,
            first,
            second,
        } => {
            assert_eq!(agent, "site-builder");
            assert_eq!(first, Path::new("/home/site-builder/.config"));
            assert_eq!(second, Path::new("/home/site-builder/.config/claude"));
        }
        other => panic!("expected overlapping mount target error, got {other}"),
    }
}

#[test]
fn rejects_agent_mount_targets_with_parent_dir_components() {
    assert_invalid_mount_target_parse_error(
        "/home/site-builder/x/../repo/.git",
        MountTargetValidationError::Invalid {
            path: PathBuf::from("/home/site-builder/x/../repo/.git"),
        },
    );
}

#[test]
fn rejects_agent_mount_targets_with_current_dir_components() {
    assert_invalid_mount_target_parse_error(
        "/home/site-builder/./a",
        MountTargetValidationError::Invalid {
            path: PathBuf::from("/home/site-builder/./a"),
        },
    );
}

#[test]
fn rejects_agent_mount_targets_containing_commas() {
    assert_invalid_mount_target_parse_error(
        "/home/site-builder/data,archive",
        MountTargetValidationError::Invalid {
            path: PathBuf::from("/home/site-builder/data,archive"),
        },
    );
}

#[test]
fn rejects_agent_mount_targets_with_trailing_slashes() {
    assert_invalid_mount_target_parse_error(
        "/home/site-builder/.claude/",
        MountTargetValidationError::Invalid {
            path: PathBuf::from("/home/site-builder/.claude/"),
        },
    );
}

#[test]
fn rejects_agent_mount_targets_containing_find_metacharacters() {
    for target in [
        "/home/site-builder/foo*bar",
        "/home/site-builder/foo?bar",
        "/home/site-builder/[x]",
        r"/home/site-builder/foo\bar",
    ] {
        assert_invalid_mount_target_parse_error(
            target,
            MountTargetValidationError::Invalid {
                path: PathBuf::from(target),
            },
        );
    }
}

#[test]
fn load_rejects_agent_mount_targets_with_trailing_slashes() {
    let path = write_temp_config(
        "invalid-mount-target-trailing-slash",
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.mounts]]
source = "/home/core/.claude"
target = "/home/site-builder/.claude/"
read_only = true
"#,
    );

    let error = Config::load(&path).expect_err("trailing-slash mount targets should be rejected");

    match &error {
        ConfigError::InvalidMountTarget { agent, error } => {
            assert_eq!(agent, "site-builder");
            assert_eq!(
                error,
                &MountTargetValidationError::Invalid {
                    path: PathBuf::from("/home/site-builder/.claude/"),
                }
            );
        }
        other => panic!("expected invalid mount target error, got {other}"),
    }

    assert_eq!(
        error.to_string(),
        "agent 'site-builder' defines invalid mount target: mount target must be an absolute path without trailing '/', '.' or '..' components, ',', or find metacharacters ('*', '?', '[', ']', '\\\\'): /home/site-builder/.claude/"
    );
}

#[test]
fn rejects_agent_mount_targets_that_are_ancestors_of_methodology_mount() {
    assert_invalid_mount_target_parse_error(
        "/agentd",
        MountTargetValidationError::Reserved {
            target: PathBuf::from("/agentd"),
        },
    );
}

#[test]
fn rejects_agent_mount_targets_that_collide_with_methodology_mount() {
    assert_invalid_mount_target_parse_error(
        "/agentd/methodology",
        MountTargetValidationError::Reserved {
            target: PathBuf::from("/agentd/methodology"),
        },
    );
}

#[test]
fn rejects_agent_mount_targets_that_collide_with_home_directory() {
    assert_invalid_mount_target_parse_error(
        "/home/site-builder",
        MountTargetValidationError::Reserved {
            target: PathBuf::from("/home/site-builder"),
        },
    );
}

#[test]
fn rejects_agent_mount_targets_that_collide_with_repo_directory() {
    assert_invalid_mount_target_parse_error(
        "/home/site-builder/repo",
        MountTargetValidationError::Reserved {
            target: PathBuf::from("/home/site-builder/repo"),
        },
    );
}

#[test]
fn normalizes_empty_repo_token_source_to_none() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("empty-repo-token-source");
    let config = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
repo_token_source = ""

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect("empty repo token source should disable clone auth");

    let agent = config.agent("site-builder").expect("agent should exist");

    assert_eq!(agent.repo_token_source(), None);
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn rejects_schedule_without_repo() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
schedule = "*/15 * * * *"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect_err("scheduled agents without repos should be rejected");

    match error {
        ConfigError::ScheduleRequiresRepo { agent } => assert_eq!(agent, "site-builder"),
        other => panic!("expected schedule-requires-repo error, got {other}"),
    }
}

#[test]
fn rejects_invalid_agent_repo_url() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
repo = "/srv/test-repo.git"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect_err("invalid agent repos should be rejected");

    match error {
        ConfigError::InvalidRepo { agent, .. } => assert_eq!(agent, "site-builder"),
        other => panic!("expected invalid repo error, got {other}"),
    }
}

#[test]
fn rejects_invalid_agent_schedule() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
repo = "https://example.com/agentd.git"
schedule = "* * *"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect_err("invalid agent schedules should be rejected");

    match error {
        ConfigError::InvalidSchedule { agent, schedule } => {
            assert_eq!(agent, "site-builder");
            assert_eq!(schedule, "* * *");
        }
        other => panic!("expected invalid schedule error, got {other}"),
    }
}

#[test]
fn rejects_repo_token_source_with_outer_whitespace() {
    for repo_token_source in [" SITE_BUILDER_REPO_TOKEN", "SITE_BUILDER_REPO_TOKEN "] {
        let error = Config::from_str(&format!(
            r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
repo_token_source = "{repo_token_source}"

[agents.command]
argv = ["site-builder", "exec"]
"#
        ))
        .expect_err("whitespace-padded repo token sources should be rejected");

        match error {
            ConfigError::FieldHasOuterWhitespace {
                field,
                agent,
                credential,
            } => {
                assert_eq!(field, "repo_token_source");
                assert_eq!(agent.as_deref(), Some("site-builder"));
                assert_eq!(credential, None);
            }
            other => panic!("expected whitespace validation error, got {other}"),
        }
    }
}

#[test]
fn rejects_agent_forge_with_outer_whitespace() {
    for forge in [" sourcehut", "sourcehut "] {
        let error = Config::from_str(&format!(
            r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
forge = "{forge}"

[agents.command]
argv = ["site-builder", "exec"]
"#
        ))
        .expect_err("whitespace-padded forge values should be rejected");

        match error {
            ConfigError::FieldHasOuterWhitespace {
                field,
                agent,
                credential,
            } => {
                assert_eq!(field, "forge");
                assert_eq!(agent.as_deref(), Some("site-builder"));
                assert_eq!(credential, None);
            }
            other => panic!("expected whitespace validation error, got {other}"),
        }
    }
}

#[test]
fn rejects_empty_agent_forge() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
forge = ""

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect_err("empty forge values should be rejected");

    match error {
        ConfigError::EmptyField {
            field,
            agent,
            credential,
        } => {
            assert_eq!(field, "forge");
            assert_eq!(agent.as_deref(), Some("site-builder"));
            assert_eq!(credential, None);
        }
        other => panic!("expected empty field validation error, got {other}"),
    }
}

#[test]
fn rejects_unknown_fields_in_agent_config() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
extra = "nope"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect_err("unknown fields should be rejected");

    assert!(error.to_string().contains("unknown field"));
    assert!(error.to_string().contains("extra"));
}

#[test]
fn rejects_unknown_fields_in_daemon_config() {
    let error = Config::from_str(
        r#"
[daemon]
socket_path = "/tmp/agentd.sock"
extra = "nope"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect_err("unknown daemon fields should be rejected");

    assert!(error.to_string().contains("unknown field"));
    assert!(error.to_string().contains("extra"));
}

#[test]
fn rejects_duplicate_agent_names() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:stable"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect_err("duplicate agent names should be rejected");

    match error {
        ConfigError::DuplicateAgentName { name } => assert_eq!(name, "site-builder"),
        other => panic!("expected duplicate agent name error, got {other}"),
    }
}

#[test]
fn rejects_configs_without_agents() {
    let error = Config::from_str("").expect_err("configs must define at least one agent");

    assert!(error.to_string().contains("at least one agent"));
}

#[test]
fn rejects_duplicate_credential_names_within_a_agent() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.credentials]]
name = "GITHUB_TOKEN"
source = "AGENTD_GITHUB_TOKEN"

[[agents.credentials]]
name = "GITHUB_TOKEN"
source = "AGENTD_GITHUB_OTHER_TOKEN"
"#,
    )
    .expect_err("duplicate credential names should be rejected");

    match error {
        ConfigError::DuplicateCredentialName { agent, name } => {
            assert_eq!(agent, "site-builder");
            assert_eq!(name, "GITHUB_TOKEN");
        }
        other => panic!("expected duplicate credential name error, got {other}"),
    }
}

#[test]
fn rejects_empty_required_string_fields() {
    let error = Config::from_str(
        r#"
[[agents]]
name = ""
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect_err("empty names should be rejected");

    assert!(error.to_string().contains("name"));
}

#[test]
fn rejects_agent_names_with_leading_or_trailing_whitespace() {
    for name in [" site-builder", "site-builder "] {
        let error = Config::from_str(&format!(
            r#"
[[agents]]
name = "{name}"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#
        ))
        .expect_err("whitespace-padded agent names should be rejected");

        match error {
            ConfigError::FieldHasOuterWhitespace {
                field,
                agent,
                credential,
            } => {
                assert_eq!(field, "name");
                assert_eq!(agent, None);
                assert_eq!(credential, None);
            }
            other => panic!("expected whitespace validation error, got {other}"),
        }
    }
}

#[test]
fn rejects_uppercase_agent_names_at_parse_time() {
    assert_invalid_agent_name_parse_error("Site-Builder");
}

#[test]
fn rejects_digit_prefixed_agent_names_at_parse_time() {
    assert_invalid_agent_name_parse_error("123site-builder");
}

#[test]
fn rejects_reserved_agent_names_at_parse_time() {
    assert_invalid_agent_name_parse_error("root");
}

#[test]
fn rejects_agent_names_longer_than_thirty_two_characters_at_parse_time() {
    assert_invalid_agent_name_parse_error(&format!("a{}", "b".repeat(32)));
}

#[test]
fn rejects_credential_names_with_leading_or_trailing_whitespace() {
    for name in [" GITHUB_TOKEN", "GITHUB_TOKEN "] {
        let error = Config::from_str(&format!(
            r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.credentials]]
name = "{name}"
source = "AGENTD_GITHUB_TOKEN"
"#
        ))
        .expect_err("whitespace-padded credential names should be rejected");

        match error {
            ConfigError::FieldHasOuterWhitespace {
                field,
                agent,
                credential,
            } => {
                assert_eq!(field, "credentials.name");
                assert_eq!(agent.as_deref(), Some("site-builder"));
                assert_eq!(credential, None);
            }
            other => panic!("expected whitespace validation error, got {other}"),
        }
    }
}

#[test]
fn rejects_credential_names_containing_commas() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.credentials]]
name = "TOKEN,EXTRA"
source = "AGENTD_GITHUB_TOKEN"
"#,
    )
    .expect_err("comma-delimited credential names should be rejected");

    match error {
        ConfigError::InvalidCredentialName { agent, name } => {
            assert_eq!(agent, "site-builder");
            assert_eq!(name, "TOKEN,EXTRA");
        }
        other => panic!("expected invalid credential name error, got {other}"),
    }
}

#[test]
fn rejects_credential_names_containing_equals_signs() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.credentials]]
name = "FOO=BAR"
source = "AGENTD_GITHUB_TOKEN"
"#,
    )
    .expect_err("credential names containing '=' should be rejected");

    match error {
        ConfigError::InvalidCredentialName { agent, name } => {
            assert_eq!(agent, "site-builder");
            assert_eq!(name, "FOO=BAR");
        }
        other => panic!("expected invalid credential name error, got {other}"),
    }
}

#[test]
fn rejects_reserved_credential_names() {
    for reserved_name in [
        "AGENT_NAME",
        "RUNA_TRANSCRIPT_DIR",
        "RUNA_TRANSCRIPT_REDACT_ENV",
    ] {
        let config = format!(
            r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.credentials]]
name = "{reserved_name}"
source = "AGENTD_GITHUB_TOKEN"
"#,
        );
        let error = Config::from_str(&config)
            .expect_err("runner-reserved credential names should be rejected");

        match &error {
            ConfigError::InvalidCredentialName { agent, name } => {
                assert_eq!(agent, "site-builder");
                assert_eq!(name, reserved_name);
            }
            other => panic!("expected invalid credential name error, got {other}"),
        }

        let message = error.to_string();
        assert!(
            message.contains(reserved_name),
            "reserved credential error should name {reserved_name}: {message}"
        );
        if reserved_name.starts_with("RUNA_TRANSCRIPT_") {
            assert!(
                message.contains("transcript subsystem") && message.contains("set internally"),
                "transcript reserved credential error should explain ownership: {message}"
            );
        }
    }
}

#[test]
fn rejects_empty_command_arrays_and_empty_command_elements() {
    let empty_command_error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = []
"#,
    )
    .expect_err("empty command arrays should be rejected");

    let empty_element_error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", ""]
"#,
    )
    .expect_err("empty command elements should be rejected");

    assert!(empty_command_error.to_string().contains("command"));
    assert!(empty_element_error.to_string().contains("command"));
}

#[test]
fn rejects_missing_command_field() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
"#,
    )
    .expect_err("missing command field should be rejected");

    assert!(error.to_string().contains("command"));
}

#[test]
fn rejects_legacy_runa_table() {
    let error = Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.runa]
[agents.command]
argv = ["site-builder", "exec"]
"#,
    )
    .expect_err("legacy runa table should be rejected");

    assert!(error.to_string().contains("runa"));
}

#[test]
fn reports_io_errors_when_loading_missing_files() {
    let missing_path = std::env::temp_dir().join(format!(
        "agentd-missing-config-{}-{}.toml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));

    let error = Config::load(&missing_path).expect_err("missing config file should fail");

    match error {
        ConfigError::Io(io_error) => assert_eq!(io_error.kind(), std::io::ErrorKind::NotFound),
        other => panic!("expected io error, got {other}"),
    }
}

#[test]
fn loading_daemon_config_resolves_relative_paths_from_file_location() {
    let path = write_temp_config(
        "daemon-only-relative-paths",
        r#"
[daemon]
socket_path = "runtime/agentd.sock"
pid_file = "runtime/agentd.pid"

[[agents]]
name = "Site-Builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    );

    let config = DaemonConfig::load(&path).expect("daemon config should parse");
    let base_dir = path
        .parent()
        .expect("config file should have a parent directory");

    assert_eq!(config.socket_path(), base_dir.join("runtime/agentd.sock"));
    assert_eq!(config.pid_file(), base_dir.join("runtime/agentd.pid"));
}

#[test]
fn loading_daemon_config_uses_defaults_when_daemon_section_is_omitted() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_xdg_runtime_dir("daemon-config-defaults");
    let path = write_temp_config(
        "daemon-only-defaults",
        r#"
[[agents]]
name = "Site-Builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    );

    let config = DaemonConfig::load(&path).expect("daemon config should parse");

    let runtime_paths =
        default_daemon_runtime_paths().expect("xdg runtime dir should provide daemon defaults");
    assert_eq!(config.socket_path(), runtime_paths.socket_path());
    assert_eq!(config.pid_file(), runtime_paths.pid_file());
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}

#[test]
fn loading_daemon_config_rejects_unknown_fields_in_daemon_section() {
    let path = write_temp_config(
        "daemon-only-unknown-field",
        r#"
[daemon]
socket_path = "/tmp/agentd.sock"
extra = "nope"

[[agents]]
name = "Site-Builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    );

    let error = DaemonConfig::load(&path).expect_err("unknown daemon fields should be rejected");

    assert!(error.to_string().contains("unknown field"));
    assert!(error.to_string().contains("extra"));
}

#[test]
fn loading_daemon_config_rejects_unknown_top_level_sections() {
    let path = write_temp_config(
        "daemon-only-unknown-top-level",
        r#"
[deamon]
socket_path = "/tmp/agentd.sock"

[[agents]]
unexpected = "still allowed to exist here"
"#,
    );

    let error =
        DaemonConfig::load(&path).expect_err("unknown top-level sections should be rejected");

    assert!(error.to_string().contains("unknown field"));
    assert!(error.to_string().contains("deamon"));
}

#[test]
fn loading_daemon_config_ignores_invalid_agent_registry_entries() {
    let path = write_temp_config(
        "daemon-only-ignores-agents",
        r#"
[daemon]
socket_path = "runtime/agentd.sock"
pid_file = "runtime/agentd.pid"

[[agents]]
unexpected = "daemon loader should ignore agent schema entirely"
"#,
    );

    let config = DaemonConfig::load(&path).expect("daemon config should parse");
    let base_dir = path
        .parent()
        .expect("config file should have a parent directory");

    assert_eq!(config.socket_path(), base_dir.join("runtime/agentd.sock"));
    assert_eq!(config.pid_file(), base_dir.join("runtime/agentd.pid"));
}
