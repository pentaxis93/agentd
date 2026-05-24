use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::{fs, panic};

use agentd::config::{Config, ConfigError};
use agentd::{DispatchError, RunRequest, SessionExecutor, dispatch_run};
use agentd_runner::{
    InvocationInput, ResolvedEnvironmentVariable, RunnerError, SessionInvocation, SessionOutcome,
    SessionSpec,
};
use serde_json::json;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after the unix epoch")
            .as_nanos()
    ))
}

struct RecordingExecutor {
    state: Arc<Mutex<RecordingState>>,
}

struct RecordingState {
    outcome: SessionOutcome,
    last_spec: Option<SessionSpec>,
    last_invocation: Option<SessionInvocation>,
}

impl RecordingExecutor {
    fn succeeding(outcome: SessionOutcome) -> (Self, Arc<Mutex<RecordingState>>) {
        let state = Arc::new(Mutex::new(RecordingState {
            outcome,
            last_spec: None,
            last_invocation: None,
        }));

        (
            Self {
                state: state.clone(),
            },
            state,
        )
    }
}

impl SessionExecutor for RecordingExecutor {
    fn run_session(
        &self,
        spec: SessionSpec,
        invocation: SessionInvocation,
    ) -> Result<SessionOutcome, RunnerError> {
        let mut state = self.state.lock().expect("recording state should lock");
        state.last_spec = Some(spec);
        state.last_invocation = Some(invocation);
        Ok(state.outcome.clone())
    }
}

fn config_with_repo_token_source(repo_token_source: &str) -> Config {
    Config::from_str(&format!(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
repo_token_source = "{repo_token_source}"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.credentials]]
name = "GITHUB_TOKEN"
source = "AGENTD_GITHUB_TOKEN"
"#
    ))
    .expect("config should parse")
}

fn config_with_audit_root(audit_root: &str) -> Config {
    Config::from_str(&format!(
        r#"
[daemon]
audit_root = "{audit_root}"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.credentials]]
name = "GITHUB_TOKEN"
source = "AGENTD_GITHUB_TOKEN"
"#
    ))
    .expect("config should parse")
}

fn config_with_mounts() -> Config {
    Config::from_str(
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
    .expect("config should parse")
}

fn config_with_ssh_default_repo_and_agent_scoped_mounts() -> Config {
    Config::from_str(
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
repo = "ssh://git@example.com/tesserine/agentd.git"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.mounts]]
source = "/home/core/.ssh/site-builder"
target = "/home/site-builder/.ssh"
read_only = true

[[agents]]
name = "code-reviewer"
base_image = "ghcr.io/example/code-reviewer:latest"
methodology_dir = "../groundwork"
repo = "ssh://git@example.com/tesserine/review-target.git"

[agents.command]
argv = ["code-reviewer", "exec"]
"#,
    )
    .expect("config should parse")
}

#[test]
fn dispatch_run_resolves_repo_token_without_injecting_it_into_runtime_environment() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
        std::env::set_var("SITE_BUILDER_REPO_TOKEN", "clone-only-secret");
    }
    let config = config_with_repo_token_source("SITE_BUILDER_REPO_TOKEN");
    let request = RunRequest {
        agent: "site-builder".to_string(),
        repo_url: Some("https://example.com/repo.git".to_string()),
        work_unit: Some("task-42".to_string()),
        input: None,
    };
    let (executor, state) = RecordingExecutor::succeeding(SessionOutcome::Success { exit_code: 0 });

    let outcome = dispatch_run(&config, &request, &executor).expect("dispatch should succeed");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });

    let state = state.lock().expect("recording state should lock");
    let spec = state
        .last_spec
        .as_ref()
        .expect("executor should receive spec");
    let invocation = state
        .last_invocation
        .as_ref()
        .expect("executor should receive invocation");

    assert_eq!(spec.agent_name, "site-builder");
    assert_eq!(spec.base_image, "ghcr.io/example/site-builder:latest");
    assert_eq!(spec.methodology_dir, Path::new("../groundwork"));
    assert_eq!(
        spec.daemon_instance_id,
        config
            .daemon()
            .daemon_instance_id()
            .expect("daemon instance id should resolve")
    );
    assert_eq!(
        spec.audit_root,
        config
            .daemon()
            .resolve_audit_root()
            .expect("audit root should resolve")
    );
    assert_eq!(
        spec.agent_command,
        vec!["site-builder".to_string(), "exec".to_string()]
    );
    assert_eq!(
        spec.environment,
        vec![ResolvedEnvironmentVariable {
            name: "GITHUB_TOKEN".to_string(),
            value: "runtime-secret".to_string(),
        }]
    );
    assert_eq!(invocation.repo_url, "https://example.com/repo.git");
    assert_eq!(invocation.repo_token.as_deref(), Some("clone-only-secret"));
    assert_eq!(invocation.work_unit.as_deref(), Some("task-42"));
    assert_eq!(invocation.timeout, None);

    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
        std::env::remove_var("SITE_BUILDER_REPO_TOKEN");
    }
}

#[test]
fn dispatch_run_forwards_resolved_audit_root_into_session_spec() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let audit_root = unique_temp_dir("agentd-dispatch-audit-root");
    let config = config_with_audit_root(
        audit_root
            .to_str()
            .expect("temporary audit root should be valid utf-8"),
    );
    let request = RunRequest {
        agent: "site-builder".to_string(),
        repo_url: Some("https://example.com/repo.git".to_string()),
        work_unit: None,
        input: None,
    };
    let (executor, state) = RecordingExecutor::succeeding(SessionOutcome::Success { exit_code: 0 });

    let result = panic::catch_unwind(|| {
        dispatch_run(&config, &request, &executor).expect("dispatch should succeed");

        let state = state.lock().expect("recording state should lock");
        let spec = state
            .last_spec
            .as_ref()
            .expect("executor should receive spec");

        assert_eq!(spec.audit_root, audit_root);
    });

    let _ = fs::remove_dir_all(&audit_root);
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }

    result.expect("dispatch assertions should succeed");
}

#[test]
#[cfg(unix)]
fn dispatch_run_rejects_an_unwritable_audit_root_before_session_execution() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let audit_root = unique_temp_dir("agentd-dispatch-unwritable-audit-root");
    fs::create_dir_all(&audit_root).expect("audit root should be created");
    fs::set_permissions(&audit_root, fs::Permissions::from_mode(0o555))
        .expect("audit root should become read-only");

    let config = config_with_audit_root(
        audit_root
            .to_str()
            .expect("temporary audit root should be valid utf-8"),
    );
    let request = RunRequest {
        agent: "site-builder".to_string(),
        repo_url: Some("https://example.com/repo.git".to_string()),
        work_unit: None,
        input: None,
    };
    let (executor, _state) =
        RecordingExecutor::succeeding(SessionOutcome::Success { exit_code: 0 });

    let error = dispatch_run(&config, &request, &executor)
        .expect_err("unwritable audit root should fail dispatch");

    match error {
        DispatchError::Config(ConfigError::AuditRootNotWritable { path, .. }) => {
            assert_eq!(path, audit_root);
        }
        other => panic!("expected audit-root config error, got {other:?}"),
    }

    fs::set_permissions(&audit_root, fs::Permissions::from_mode(0o755))
        .expect("audit root should become writable again");
    fs::remove_dir_all(&audit_root).expect("temporary audit root should be removed");

    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }
}

#[test]
fn dispatch_run_omits_repo_token_when_source_env_var_is_missing() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
        std::env::remove_var("SITE_BUILDER_REPO_TOKEN");
    }
    let config = config_with_repo_token_source("SITE_BUILDER_REPO_TOKEN");
    let request = RunRequest {
        agent: "site-builder".to_string(),
        repo_url: Some("https://example.com/repo.git".to_string()),
        work_unit: None,
        input: None,
    };
    let (executor, state) = RecordingExecutor::succeeding(SessionOutcome::Success { exit_code: 0 });

    dispatch_run(&config, &request, &executor).expect("dispatch should succeed");

    let state = state.lock().expect("recording state should lock");
    let invocation = state
        .last_invocation
        .as_ref()
        .expect("executor should receive invocation");

    assert_eq!(invocation.repo_token, None);

    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }
}

#[test]
fn dispatch_run_omits_repo_token_when_source_env_var_is_empty() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
        std::env::set_var("SITE_BUILDER_REPO_TOKEN", "");
    }
    let config = config_with_repo_token_source("SITE_BUILDER_REPO_TOKEN");
    let request = RunRequest {
        agent: "site-builder".to_string(),
        repo_url: Some("https://example.com/repo.git".to_string()),
        work_unit: None,
        input: None,
    };
    let (executor, state) = RecordingExecutor::succeeding(SessionOutcome::Success { exit_code: 0 });

    dispatch_run(&config, &request, &executor).expect("dispatch should succeed");

    let state = state.lock().expect("recording state should lock");
    let invocation = state
        .last_invocation
        .as_ref()
        .expect("executor should receive invocation");

    assert_eq!(invocation.repo_token, None);

    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
        std::env::remove_var("SITE_BUILDER_REPO_TOKEN");
    }
}

#[test]
fn dispatch_run_errors_when_runtime_credential_source_is_missing() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
        std::env::set_var("SITE_BUILDER_REPO_TOKEN", "clone-only-secret");
    }
    let config = config_with_repo_token_source("SITE_BUILDER_REPO_TOKEN");
    let request = RunRequest {
        agent: "site-builder".to_string(),
        repo_url: Some("https://example.com/repo.git".to_string()),
        work_unit: None,
        input: None,
    };
    let (executor, _state) =
        RecordingExecutor::succeeding(SessionOutcome::Success { exit_code: 0 });

    let error = dispatch_run(&config, &request, &executor)
        .expect_err("missing runtime credential sources should fail dispatch");

    match error {
        DispatchError::MissingCredentialSource {
            agent,
            credential,
            source,
        } => {
            assert_eq!(agent, "site-builder");
            assert_eq!(credential, "GITHUB_TOKEN");
            assert_eq!(source, "AGENTD_GITHUB_TOKEN");
        }
        other => panic!("expected missing credential source error, got {other:?}"),
    }

    unsafe {
        std::env::remove_var("SITE_BUILDER_REPO_TOKEN");
    }
}

#[test]
fn dispatch_run_rejects_relative_daemon_runtime_paths_as_config_errors() {
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
    let request = RunRequest {
        agent: "site-builder".to_string(),
        repo_url: Some("https://example.com/repo.git".to_string()),
        work_unit: None,
        input: None,
    };
    let (executor, _state) =
        RecordingExecutor::succeeding(SessionOutcome::Success { exit_code: 0 });

    let error =
        dispatch_run(&config, &request, &executor).expect_err("relative daemon paths should fail");

    match error {
        DispatchError::Config(ConfigError::RelativeDaemonRuntimePath { field, path }) => {
            assert_eq!(field, "daemon.socket_path");
            assert_eq!(path, Path::new("runtime/agentd.sock"));
        }
        other => panic!("expected config error, got {other:?}"),
    }
}

#[test]
fn dispatch_run_forwards_agent_mounts_into_session_spec() {
    let config = config_with_mounts();
    let request = RunRequest {
        agent: "site-builder".to_string(),
        repo_url: Some("https://example.com/repo.git".to_string()),
        work_unit: None,
        input: None,
    };
    let (executor, state) = RecordingExecutor::succeeding(SessionOutcome::Success { exit_code: 0 });

    dispatch_run(&config, &request, &executor).expect("dispatch should succeed");

    let state = state.lock().expect("recording state should lock");
    let spec = state
        .last_spec
        .as_ref()
        .expect("executor should receive spec");

    assert_eq!(
        spec.mounts,
        vec![
            agentd_runner::BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/home/site-builder/.claude"),
                read_only: true,
            },
            agentd_runner::BindMount {
                source: PathBuf::from("/var/lib/tesserine/audit"),
                target: PathBuf::from("/home/site-builder/.runa"),
                read_only: false,
            },
        ]
    );
}

#[test]
fn dispatch_run_forwards_ssh_default_repo_without_repo_token_source() {
    let config = config_with_ssh_default_repo_and_agent_scoped_mounts();
    let request = RunRequest {
        agent: "site-builder".to_string(),
        repo_url: None,
        work_unit: None,
        input: None,
    };
    let (executor, state) = RecordingExecutor::succeeding(SessionOutcome::Success { exit_code: 0 });

    dispatch_run(&config, &request, &executor).expect("dispatch should succeed");

    let state = state.lock().expect("recording state should lock");
    let invocation = state
        .last_invocation
        .as_ref()
        .expect("executor should receive invocation");

    assert_eq!(
        invocation.repo_url,
        "ssh://git@example.com/tesserine/agentd.git"
    );
    assert_eq!(invocation.repo_token, None);
}

#[test]
fn dispatch_run_does_not_forward_another_agents_ssh_mount() {
    let config = config_with_ssh_default_repo_and_agent_scoped_mounts();
    let request = RunRequest {
        agent: "code-reviewer".to_string(),
        repo_url: None,
        work_unit: None,
        input: None,
    };
    let (executor, state) = RecordingExecutor::succeeding(SessionOutcome::Success { exit_code: 0 });

    dispatch_run(&config, &request, &executor).expect("dispatch should succeed");

    let state = state.lock().expect("recording state should lock");
    let spec = state
        .last_spec
        .as_ref()
        .expect("executor should receive spec");
    let invocation = state
        .last_invocation
        .as_ref()
        .expect("executor should receive invocation");

    assert_eq!(spec.agent_name, "code-reviewer");
    assert!(spec.mounts.is_empty());
    assert_eq!(
        invocation.repo_url,
        "ssh://git@example.com/tesserine/review-target.git"
    );
    assert_eq!(invocation.repo_token, None);
}

#[test]
fn dispatch_run_forwards_typed_invocation_input_into_session_invocation() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let config = config_with_repo_token_source("SITE_BUILDER_REPO_TOKEN");
    let request = RunRequest {
        agent: "site-builder".to_string(),
        repo_url: Some("https://example.com/repo.git".to_string()),
        work_unit: None,
        input: Some(InvocationInput::Artifact {
            artifact_type: "claim".to_string(),
            artifact_id: "claim".to_string(),
            document: json!({ "summary": "Ship it" }),
        }),
    };
    let (executor, state) = RecordingExecutor::succeeding(SessionOutcome::Success { exit_code: 0 });

    dispatch_run(&config, &request, &executor).expect("dispatch should succeed");

    let state = state.lock().expect("recording state should lock");
    let invocation = state
        .last_invocation
        .as_ref()
        .expect("executor should receive invocation");
    assert_eq!(invocation.input, request.input);

    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }
}
