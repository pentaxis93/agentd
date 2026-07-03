use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use agentd::SessionExecutor;
use agentd::config::Config;
use agentd::daemon::run_daemon_until_shutdown_with_reconciler;
use agentd_runner::{
    InvocationInput, RunnerError, SessionInvocation, SessionOutcome, SessionSpec,
    StartupReconciliationReport,
};
use serde_json::json;

type DaemonHandle = thread::JoinHandle<Result<(), agentd::DaemonError>>;
type RecordedInvocations = Arc<Mutex<Vec<SessionInvocation>>>;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Clone)]
struct FixedOutcomeExecutor {
    outcome: SessionOutcome,
}

impl SessionExecutor for FixedOutcomeExecutor {
    fn run_session(
        &self,
        _spec: SessionSpec,
        _invocation: SessionInvocation,
    ) -> Result<SessionOutcome, RunnerError> {
        Ok(self.outcome.clone())
    }
}

#[derive(Clone)]
struct RecordingInvocationExecutor {
    outcome: SessionOutcome,
    invocations: Arc<Mutex<Vec<SessionInvocation>>>,
}

impl RecordingInvocationExecutor {
    fn new(outcome: SessionOutcome) -> (Self, Arc<Mutex<Vec<SessionInvocation>>>) {
        let invocations = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                outcome,
                invocations: Arc::clone(&invocations),
            },
            invocations,
        )
    }
}

impl SessionExecutor for RecordingInvocationExecutor {
    fn run_session(
        &self,
        _spec: SessionSpec,
        invocation: SessionInvocation,
    ) -> Result<SessionOutcome, RunnerError> {
        self.invocations
            .lock()
            .expect("recorded invocations should lock")
            .push(invocation);
        Ok(self.outcome.clone())
    }
}

fn write_temp_config(name: &str, contents: &str) -> PathBuf {
    let unique = format!(
        "agentd-cli-test-{name}-{}-{}",
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

fn daemon_test_config(socket_path: &Path, pid_file: &Path) -> String {
    format!(
        r#"
[daemon]
socket_path = "{socket_path}"
pid_file = "{pid_file}"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]
"#,
        socket_path = socket_path.display(),
        pid_file = pid_file.display()
    )
}

fn daemon_test_config_with_default_repo(socket_path: &Path, pid_file: &Path, repo: &str) -> String {
    format!(
        r#"
[daemon]
socket_path = "{socket_path}"
pid_file = "{pid_file}"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
repo = "{repo}"

[agents.command]
argv = ["site-builder", "exec"]
"#,
        socket_path = socket_path.display(),
        pid_file = pid_file.display(),
        repo = repo
    )
}

fn daemon_test_config_with_credential(socket_path: &Path, pid_file: &Path) -> String {
    format!(
        r#"
[daemon]
socket_path = "{socket_path}"
pid_file = "{pid_file}"

[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"

[agents.command]
argv = ["site-builder", "exec"]

[[agents.credentials]]
name = "GITHUB_TOKEN"
source = "AGENTD_GITHUB_TOKEN"
"#,
        socket_path = socket_path.display(),
        pid_file = pid_file.display()
    )
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }

    panic!("timed out waiting for {}", path.display());
}

fn fake_podman_path(name: &str) -> OsString {
    let unique = format!(
        "agentd-cli-fake-podman-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    );
    let bin_dir = std::env::temp_dir().join(unique).join("bin");
    std::fs::create_dir_all(&bin_dir).expect("fake podman bin dir should be created");
    let podman_path = bin_dir.join("podman");
    std::fs::write(
        &podman_path,
        r#"#!/bin/sh
case "$1" in
  ps)
    printf '[]\n'
    ;;
  secret)
    case "$2" in
      ls)
        exit 0
        ;;
      rm)
        exit 0
        ;;
      *)
        echo "unexpected podman secret command: $*" >&2
        exit 1
        ;;
    esac
    ;;
  rm)
    exit 0
    ;;
  *)
    echo "unexpected podman command: $*" >&2
    exit 1
    ;;
esac
"#,
    )
    .expect("fake podman script should be written");
    let mut permissions = std::fs::metadata(&podman_path)
        .expect("fake podman metadata should be readable")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&podman_path, permissions)
        .expect("fake podman script should be executable");

    std::env::join_paths(std::iter::once(bin_dir).chain(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH should exist for tests"),
    )))
    .expect("fake podman PATH should be constructible")
}

fn terminate(child: &mut Child) -> io::Result<()> {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()?;

    assert!(status.success(), "kill should succeed");
    Ok(())
}

fn start_test_daemon(
    config_path: &Path,
    outcome: SessionOutcome,
) -> (Arc<AtomicBool>, DaemonHandle, Config) {
    let config = Config::load(config_path).expect("test config should load");
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let executor = FixedOutcomeExecutor { outcome };
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_with_reconciler(daemon_config, executor, daemon_shutdown, || {
            Ok(StartupReconciliationReport::default())
        })
    });
    wait_for_path(config.daemon().socket_path());
    (shutdown, handle, config)
}

fn start_recording_test_daemon(
    config_path: &Path,
    outcome: SessionOutcome,
) -> (Arc<AtomicBool>, DaemonHandle, Config, RecordedInvocations) {
    let config = Config::load(config_path).expect("test config should load");
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let (executor, invocations) = RecordingInvocationExecutor::new(outcome);
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_with_reconciler(daemon_config, executor, daemon_shutdown, || {
            Ok(StartupReconciliationReport::default())
        })
    });
    wait_for_path(config.daemon().socket_path());
    (shutdown, handle, config, invocations)
}

#[test]
fn binary_top_level_version_reports_crate_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .arg("--version")
        .output()
        .expect("agentd binary should run");

    assert!(
        output.status.success(),
        "version command should exit successfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert_eq!(stdout, format!("agentd {}\n", env!("CARGO_PKG_VERSION")));

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert_eq!(stderr, "");
}

#[test]
fn binary_run_version_reports_crate_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["run", "--version"])
        .output()
        .expect("agentd binary should run");

    assert!(
        output.status.success(),
        "run version command should exit successfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert_eq!(stdout, format!("agentd {}\n", env!("CARGO_PKG_VERSION")));

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert_eq!(stderr, "");
}

#[test]
fn binary_daemon_subcommand_starts_daemon_mode() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "daemon-default-command",
        &daemon_test_config(&socket_path, &pid_file),
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "daemon",
            "--config",
            config_path.to_str().expect("config path should be utf-8"),
        ])
        .env("AGENTD_LOG_FORMAT", "text")
        .env("PATH", fake_podman_path("daemon-subcommand"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agentd binary should spawn");

    wait_for_path(&socket_path);
    wait_for_path(&pid_file);
    terminate(&mut child).expect("daemon should accept SIGTERM");
    let output = child
        .wait_with_output()
        .expect("daemon output should be available");

    assert!(
        output.status.success(),
        "daemon should exit cleanly: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("\"event\":\"agentd.logging_format_invalid\""),
        "expected tracing bootstrap warning, got: {stderr}"
    );
}

#[test]
fn binary_bare_command_with_config_starts_daemon_mode() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-bare-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "daemon-bare-command",
        &daemon_test_config(&socket_path, &pid_file),
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--config",
            config_path.to_str().expect("config path should be utf-8"),
        ])
        .env("AGENTD_LOG_FORMAT", "text")
        .env("PATH", fake_podman_path("bare-command"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agentd binary should spawn");

    wait_for_path(&socket_path);
    wait_for_path(&pid_file);
    terminate(&mut child).expect("daemon should accept SIGTERM");
    let output = child
        .wait_with_output()
        .expect("daemon output should be available");

    assert!(
        output.status.success(),
        "daemon should exit cleanly: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn binary_run_help_shows_socket_path_and_not_config() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["run", "--help"])
        .output()
        .expect("agentd binary should run");

    assert!(
        output.status.success(),
        "run help should exit successfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert!(
        stdout.contains("--socket-path"),
        "run help should advertise socket override: {stdout}"
    );
    assert!(
        stdout.contains("--progress <PROGRESS>")
            && stdout.contains("summary")
            && stdout.contains("full"),
        "run help should document live progress verbosity: {stdout}"
    );
    assert!(
        !stdout.contains("--config"),
        "run help should not advertise daemon config coupling: {stdout}"
    );
    assert!(
        stdout.contains("--work-unit <ID> --artifact-type work-unit --artifact-file <ID>.json"),
        "run help should document work-mode artifact invocation: {stdout}"
    );
}

#[test]
fn binary_top_level_help_shows_wish_operator_verb() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .arg("--help")
        .output()
        .expect("agentd binary should run");

    assert!(
        output.status.success(),
        "help should exit successfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert!(
        stdout.contains("wish"),
        "top-level help should advertise the wish operator verb: {stdout}"
    );
}

#[test]
fn binary_wish_help_shows_evocative_intent_prompting_surface() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["wish", "--help"])
        .output()
        .expect("agentd binary should run");

    assert!(
        output.status.success(),
        "wish help should exit successfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert!(
        stdout.contains("--socket-path"),
        "wish help should advertise socket override: {stdout}"
    );
    assert!(
        stdout.contains("--progress <PROGRESS>")
            && stdout.contains("summary")
            && stdout.contains("full"),
        "wish help should document live progress verbosity: {stdout}"
    );
    assert!(
        stdout.contains("What do you wish the agent to do?"),
        "wish help should document the statement prompt: {stdout}"
    );
    assert!(
        stdout.contains("What is this wish aimed at?"),
        "wish help should document the optional target prompt: {stdout}"
    );
}

#[test]
fn binary_daemon_help_shows_config() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["daemon", "--help"])
        .output()
        .expect("agentd binary should run");

    assert!(
        output.status.success(),
        "daemon help should exit successfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert!(
        stdout.contains("--config"),
        "daemon help should retain config loading: {stdout}"
    );
}

#[test]
fn binary_run_command_reports_clear_error_when_daemon_is_not_running() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-not-running-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let _config_path = write_temp_config(
        "client-command",
        &daemon_test_config(&socket_path, &pid_file),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
        ])
        .output()
        .expect("agentd binary should run");

    assert!(
        !output.status.success(),
        "run command should fail without daemon"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("agentd is not running"),
        "expected daemon-not-running error, got: {stderr}"
    );
}

#[test]
fn binary_run_command_requires_xdg_runtime_dir_without_socket_override() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["run", "site-builder"])
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .expect("agentd binary should run");

    assert!(
        !output.status.success(),
        "run command should fail without xdg runtime dir"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("XDG_RUNTIME_DIR") && stderr.contains("--socket-path"),
        "expected actionable xdg runtime error, got: {stderr}"
    );
}

#[test]
fn binary_run_command_rejects_root_level_config() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-root-config-rejected-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-root-config-rejected",
        &daemon_test_config(&socket_path, &pid_file),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--config",
            config_path.to_str().expect("config path should be utf-8"),
            "run",
            "site-builder",
            "https://example.com/repo.git",
        ])
        .output()
        .expect("agentd binary should run");

    assert!(
        !output.status.success(),
        "run command should reject daemon config at the root surface"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("--config") && stderr.contains("run"),
        "expected root-level config rejection mentioning run, got: {stderr}"
    );
}

#[test]
fn binary_run_command_uses_agent_default_repo_when_repo_argument_is_omitted() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-default-repo-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let default_repo = "https://example.com/default.git";
    let config_path = write_temp_config(
        "client-command-default-repo",
        &daemon_test_config_with_default_repo(&socket_path, &pid_file, default_repo),
    );

    let (shutdown, handle, _config, invocations) =
        start_recording_test_daemon(&config_path, SessionOutcome::Success { exit_code: 0 });

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        output.status.success(),
        "run command should succeed with an agent default repo: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        invocations.lock().expect("invocations should lock")[0].repo_url,
        default_repo
    );
}

#[test]
fn binary_run_command_prefers_explicit_repo_over_agent_default_repo() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-explicit-repo-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-explicit-repo",
        &daemon_test_config_with_default_repo(
            &socket_path,
            &pid_file,
            "https://example.com/default.git",
        ),
    );
    let explicit_repo = "https://example.com/override.git";

    let (shutdown, handle, _config, invocations) =
        start_recording_test_daemon(&config_path, SessionOutcome::Success { exit_code: 0 });

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            explicit_repo,
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        output.status.success(),
        "run command should succeed with an explicit repo override: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        invocations.lock().expect("invocations should lock")[0].repo_url,
        explicit_repo
    );
}

#[test]
fn binary_run_command_rejects_intent_when_work_unit_is_also_supplied() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-intent-work-unit-conflict-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let _config_path = write_temp_config(
        "client-command-intent-work-unit-conflict",
        &daemon_test_config(&socket_path, &pid_file),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
            "--work-unit",
            "issue-81",
            "--intent",
            "Add a status page",
        ])
        .output()
        .expect("agentd binary should run");

    assert!(
        !output.status.success(),
        "run command should reject conflicting intent/work-unit flags"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("--intent") && stderr.contains("--work-unit"),
        "expected clap conflict mentioning both flags, got: {stderr}"
    );
}

#[test]
fn binary_run_command_requires_artifact_type_when_artifact_file_is_supplied() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-artifact-type-required-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let _config_path = write_temp_config(
        "client-command-artifact-type-required",
        &daemon_test_config(&socket_path, &pid_file),
    );
    let artifact_path = runtime_dir.join("request.json");
    std::fs::write(
        &artifact_path,
        r#"{"statement":"Add a status page","source":"operator"}"#,
    )
    .expect("artifact file should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
            "--artifact-file",
            artifact_path
                .to_str()
                .expect("artifact path should be utf-8"),
        ])
        .output()
        .expect("agentd binary should run");

    assert!(
        !output.status.success(),
        "run command should reject artifact files without an artifact type"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("--artifact-type"),
        "expected missing-artifact-type error, got: {stderr}"
    );
}

#[test]
fn binary_run_command_forwards_intent_text_as_typed_invocation_input() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-intent-input-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-intent-input",
        &daemon_test_config(&socket_path, &pid_file),
    );
    let (shutdown, handle, _config, invocations) =
        start_recording_test_daemon(&config_path, SessionOutcome::Success { exit_code: 0 });

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
            "--intent",
            "Add a status page",
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        output.status.success(),
        "run command should succeed with intent input: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let invocation = invocations.lock().expect("invocations should lock")[0].clone();
    assert_eq!(
        invocation.input,
        Some(InvocationInput::IntentText {
            statement: "Add a status page".to_string(),
            target: None,
        })
    );
}

#[test]
fn binary_wish_command_prompts_and_forwards_prose_as_intent_text() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-wish-prose-input-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-wish-prose-input",
        &daemon_test_config(&socket_path, &pid_file),
    );
    let (shutdown, handle, _config, invocations) =
        start_recording_test_daemon(&config_path, SessionOutcome::Success { exit_code: 0 });

    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "wish",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agentd binary should spawn");

    child
        .stdin
        .as_mut()
        .expect("stdin should be piped")
        .write_all(b"Add a status page\n\n")
        .expect("wish input should be written");
    let output = child
        .wait_with_output()
        .expect("wish output should be available");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        output.status.success(),
        "wish command should succeed with prose input: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert!(
        stdout.contains("Speak your wish."),
        "wish should greet the operator before prompting: {stdout}"
    );
    assert!(
        stdout.contains("What do you wish the agent to do?"),
        "wish should elicit an intent statement: {stdout}"
    );
    assert!(
        stdout.contains("What is this wish aimed at?"),
        "wish should elicit an optional target: {stdout}"
    );

    let invocation = invocations.lock().expect("invocations should lock")[0].clone();
    assert_eq!(
        invocation.input,
        Some(InvocationInput::IntentText {
            statement: "Add a status page".to_string(),
            target: None,
        })
    );
}

#[test]
fn binary_wish_command_forwards_target_bearing_intent_text_verbatim() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-wish-target-input-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-wish-target-input",
        &daemon_test_config(&socket_path, &pid_file),
    );
    let (shutdown, handle, _config, invocations) =
        start_recording_test_daemon(&config_path, SessionOutcome::Success { exit_code: 0 });

    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "wish",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agentd binary should spawn");

    child
        .stdin
        .as_mut()
        .expect("stdin should be piped")
        .write_all(b"Work the tracker item\ntesserine/agentd#152\n")
        .expect("wish input should be written");
    let output = child
        .wait_with_output()
        .expect("wish output should be available");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        output.status.success(),
        "wish command should succeed with target-bearing input: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let invocation = invocations.lock().expect("invocations should lock")[0].clone();
    assert_eq!(
        invocation.input,
        Some(InvocationInput::IntentText {
            statement: "Work the tracker item".to_string(),
            target: Some("tesserine/agentd#152".to_string()),
        })
    );
}

#[test]
fn binary_run_command_reads_artifact_file_and_forwards_structured_input() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-artifact-input-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-artifact-input",
        &daemon_test_config(&socket_path, &pid_file),
    );
    let artifact_path = runtime_dir.join("intent.json");
    std::fs::write(
        &artifact_path,
        r#"{"statement":"Add a status page","source":"operator"}"#,
    )
    .expect("artifact file should be written");
    let (shutdown, handle, _config, invocations) =
        start_recording_test_daemon(&config_path, SessionOutcome::Success { exit_code: 0 });

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
            "--artifact-type",
            "intent",
            "--artifact-file",
            artifact_path
                .to_str()
                .expect("artifact path should be utf-8"),
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        output.status.success(),
        "run command should succeed with artifact input: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let invocation = invocations.lock().expect("invocations should lock")[0].clone();
    assert_eq!(
        invocation.input,
        Some(InvocationInput::Artifact {
            artifact_type: "intent".to_string(),
            artifact_id: "intent".to_string(),
            document: json!({
                "statement": "Add a status page",
                "source": "operator",
            }),
        })
    );
}

#[test]
fn binary_run_command_forwards_work_unit_with_matching_artifact_file() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-work-unit-artifact-input-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-work-unit-artifact-input",
        &daemon_test_config(&socket_path, &pid_file),
    );
    let artifact_path = runtime_dir.join("issue-76.json");
    std::fs::write(
        &artifact_path,
        r#"{"id":"issue-76","title":"Execute work mode"}"#,
    )
    .expect("artifact file should be written");
    let (shutdown, handle, _config, invocations) =
        start_recording_test_daemon(&config_path, SessionOutcome::Success { exit_code: 0 });

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
            "--work-unit",
            "issue-76",
            "--artifact-type",
            "work-unit",
            "--artifact-file",
            artifact_path
                .to_str()
                .expect("artifact path should be utf-8"),
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        output.status.success(),
        "run command should succeed with work-unit artifact input: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let invocation = invocations.lock().expect("invocations should lock")[0].clone();
    assert_eq!(invocation.work_unit.as_deref(), Some("issue-76"));
    assert_eq!(
        invocation.input,
        Some(InvocationInput::Artifact {
            artifact_type: "work-unit".to_string(),
            artifact_id: "issue-76".to_string(),
            document: json!({
                "id": "issue-76",
                "title": "Execute work mode",
            }),
        })
    );
}

#[test]
fn binary_run_command_reports_clear_error_when_repo_is_missing_from_cli_and_agent() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-missing-repo-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-missing-repo",
        &daemon_test_config(&socket_path, &pid_file),
    );
    let (shutdown, handle, _config) =
        start_test_daemon(&config_path, SessionOutcome::Success { exit_code: 0 });

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        !output.status.success(),
        "run command should fail when no repo is available"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("requires a repo argument or configured agent repo"),
        "expected missing-repo error, got: {stderr}"
    );
}

#[test]
fn binary_run_command_reports_unknown_agent_when_repo_argument_is_omitted() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-unknown-agent-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-unknown-agent",
        &daemon_test_config(&socket_path, &pid_file),
    );
    let (shutdown, handle, _config) =
        start_test_daemon(&config_path, SessionOutcome::Success { exit_code: 0 });

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "unknown-agent",
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        !output.status.success(),
        "run command should fail for an unknown agent"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("unknown agent 'unknown-agent'"),
        "expected unknown-agent error, got: {stderr}"
    );
    assert!(
        !stderr.contains("requires a repo argument or configured agent repo"),
        "unknown-agent failure should not be reported as missing repo: {stderr}"
    );
}

#[test]
fn binary_run_command_exits_non_zero_and_reports_failed_sessions_on_stderr() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-failed-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-failed",
        &daemon_test_config_with_credential(&socket_path, &pid_file),
    );

    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let (shutdown, handle, _config) = start_test_daemon(
        &config_path,
        SessionOutcome::GenericFailure { exit_code: 23 },
    );

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }

    assert!(
        !output.status.success(),
        "run command should fail when the daemon reports a failed session"
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout should be valid UTF-8"),
        "session running: site-builder\n",
        "failed run should print only live progress on stdout"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("session generic_failure (exit code 23)"),
        "expected failed-session error on stderr, got: {stderr}"
    );
}

#[test]
fn binary_run_command_exits_non_zero_and_reports_timed_out_sessions_on_stderr() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-timeout-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-timeout",
        &daemon_test_config(&socket_path, &pid_file),
    );

    let (shutdown, handle, _config) = start_test_daemon(&config_path, SessionOutcome::TimedOut);

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        !output.status.success(),
        "run command should fail when the daemon reports a timed-out session"
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout should be valid UTF-8"),
        "session running: site-builder\n",
        "timed-out run should print only live progress on stdout"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("session timed out"),
        "expected timed-out session error on stderr, got: {stderr}"
    );
}

#[test]
fn binary_run_command_exits_non_zero_and_reports_signal_terminated_sessions_on_stderr() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-signaled-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-signaled",
        &daemon_test_config(&socket_path, &pid_file),
    );

    let (shutdown, handle, _config) = start_test_daemon(
        &config_path,
        SessionOutcome::TerminatedBySignal {
            exit_code: 130,
            signal: 2,
        },
    );

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        !output.status.success(),
        "run command should fail when the daemon reports a signal-terminated session"
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout should be valid UTF-8"),
        "session running: site-builder\n",
        "signal-terminated run should print only live progress on stdout"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be valid UTF-8");
    assert!(
        stderr.contains("session terminated_by_signal (exit code 130, signal 2)"),
        "expected signal-terminated session error on stderr, got: {stderr}"
    );
}

#[test]
fn binary_run_command_exits_zero_and_reports_blocked_sessions_on_stdout() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-blocked-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-blocked",
        &daemon_test_config(&socket_path, &pid_file),
    );

    let (shutdown, handle, _config) =
        start_test_daemon(&config_path, SessionOutcome::Blocked { exit_code: 3 });

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        output.status.success(),
        "run command should treat blocked as a normal terminal state: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout should be valid UTF-8"),
        "session running: site-builder\nsession blocked\n"
    );
    assert!(
        String::from_utf8(output.stderr)
            .expect("stderr should be valid UTF-8")
            .is_empty(),
        "blocked run should not print an error-style stderr message"
    );
}

#[test]
fn binary_run_command_exits_zero_and_reports_nothing_ready_sessions_on_stdout() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-nothing-ready-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let socket_path = runtime_dir.join("agentd.sock");
    let pid_file = runtime_dir.join("agentd.pid");
    let config_path = write_temp_config(
        "client-command-nothing-ready",
        &daemon_test_config(&socket_path, &pid_file),
    );

    let (shutdown, handle, _config) =
        start_test_daemon(&config_path, SessionOutcome::NothingReady { exit_code: 4 });

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "run",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
        ])
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        output.status.success(),
        "run command should treat nothing_ready as a normal terminal state: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout should be valid UTF-8"),
        "session running: site-builder\nsession nothing_ready\n"
    );
    assert!(
        String::from_utf8(output.stderr)
            .expect("stderr should be valid UTF-8")
            .is_empty(),
        "nothing_ready run should not print an error-style stderr message"
    );
}

#[test]
fn binary_run_command_succeeds_without_client_config_when_using_xdg_socket_discovery() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-invalid-registry-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let xdg_runtime_dir = runtime_dir.join("xdg");
    std::fs::create_dir_all(&xdg_runtime_dir).expect("xdg runtime dir should be created");
    let config_path = write_temp_config(
        "client-command-invalid-registry-after-start",
        r#"
[[agents]]
name = "site-builder"
base_image = "ghcr.io/example/site-builder:latest"
methodology_dir = "../groundwork"
repo = "https://example.com/default.git"

[agents.command]
argv = ["site-builder", "exec"]
"#,
    );

    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &xdg_runtime_dir);
    }
    let (shutdown, handle, _config, invocations) =
        start_recording_test_daemon(&config_path, SessionOutcome::Success { exit_code: 0 });
    wait_for_path(&xdg_runtime_dir.join("agentd/agentd.sock"));
    std::fs::remove_file(&config_path).expect("config should be removable after daemon startup");

    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["run", "site-builder"])
        .env("XDG_RUNTIME_DIR", &xdg_runtime_dir)
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        output.status.success(),
        "run command should still succeed while daemon is healthy: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout should be valid UTF-8"),
        "session running: site-builder\nsession success\n"
    );
    assert_eq!(
        invocations.lock().expect("invocations should lock")[0].repo_url,
        "https://example.com/default.git"
    );
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}
