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
    InvocationInput, RunnerError, SessionInvocation, SessionOutcome, SessionProgressObserver,
    SessionSpec, StartupReconciliationReport,
};
use clap::{CommandFactory as _, Parser as _};
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
        _progress: &dyn SessionProgressObserver,
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
        _progress: &dyn SessionProgressObserver,
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

fn assert_complete_help(command: &clap::Command) {
    let about = command
        .get_about()
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(
        !about.trim().is_empty(),
        "{} must state what it does and when to use it",
        command.get_name()
    );

    for argument in command.get_arguments() {
        let help = argument
            .get_help()
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(
            !help.trim().is_empty(),
            "{} input {} must state its purpose",
            command.get_name(),
            argument.get_id()
        );
    }

    for subcommand in command.get_subcommands() {
        assert_complete_help(subcommand);
    }
}

#[test]
fn command_help_structurally_explains_every_surface_and_examples_parse() {
    let command = agentd::cli::Cli::command();
    assert_complete_help(&command);

    let examples = [
        agentd::cli::ROOT_EXAMPLES,
        agentd::cli::DAEMON_EXAMPLES,
        agentd::cli::RUN_EXAMPLES,
        agentd::cli::WISH_EXAMPLES,
    ];

    for example in examples.into_iter().flatten() {
        agentd::cli::Cli::try_parse_from(*example)
            .unwrap_or_else(|error| panic!("example {example:?} must parse: {error}"));
    }
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

fn assert_help(args: &[&str], expected: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(args)
        .output()
        .expect("agentd binary should run");

    assert!(
        output.status.success(),
        "help {args:?} should exit successfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stderr).expect("stderr should be valid UTF-8"),
        ""
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout should be valid UTF-8"),
        expected
    );
}

const ROOT_HELP: &str = r#"Run the agentd service or submit agent sessions through it.

With no subcommand, agentd starts the foreground daemon using /etc/agentd/agentd.toml. Use daemon to make service startup explicit, run to submit prepared session input, or wish to elicit a desired state interactively.

Usage: agentd [OPTIONS] [COMMAND]

Commands:
  daemon  Run the foreground service that accepts and supervises agent sessions.
  run     Submit one manual session request with explicitly supplied input.
  wish    Elicit a desired state and seed one agent session through the running daemon.
  help    Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>
          Load daemon configuration from this path when agentd starts in daemon mode [default: /etc/agentd/agentd.toml]

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version

Examples:
  agentd
  agentd --config <PATH>
"#;

#[test]
fn binary_root_help_matches_the_invocation_contract_exactly() {
    assert_help(&["--help"], ROOT_HELP);
}

const DAEMON_HELP: &str = r#"Run the foreground service that accepts manual and scheduled session requests over its control socket and supervises their containerized agent sessions.

Start daemon before using run or wish. Use run or wish to submit work to an already-running daemon.

Usage: agentd daemon [OPTIONS]

Options:
      --config <CONFIG>
          Load daemon configuration from this path [default: /etc/agentd/agentd.toml]

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version

Examples:
  agentd daemon
  agentd daemon --config <PATH>
"#;

#[test]
fn binary_daemon_help_matches_the_invocation_contract_exactly() {
    assert_help(&["daemon", "--help"], DAEMON_HELP);
}

const RUN_HELP: &str = concat!(
    r#"Submit one manual session request with explicitly supplied input to the running agentd daemon.

Use run when the session input is already prepared. Use wish to elicit a desired state interactively, or daemon to start the service that accepts requests.

Usage: agentd run [OPTIONS] <AGENT> [REPO]

Arguments:
  <AGENT>
          Select an agent declared under this name in the daemon configuration

  [REPO]
          Clone this Git repository for the session; when omitted, use the selected agent's daemon-configured repository

Options:
      --socket-path <SOCKET_PATH>
          Send the request through this control socket instead of $XDG_RUNTIME_DIR/agentd/agentd.sock

      --progress <PROGRESS>
          Choose how much live transcript activity to print while the session runs

          Possible values:
          - summary: Print compact live transcript activity
          - full:    Print raw live transcript event detail
"#,
    "          \n",
    r#"          [default: summary]

      --work-unit <WORK_UNIT>
          Seed the session from this tracker work-unit reference through runa; conflicts with --intent

      --intent <INTENT>
          Synthesize this prose statement into a canonical intent artifact; the active methodology must declare a compatible intent schema

      --artifact-file <ARTIFACT_FILE>
          Supply this complete JSON artifact document as invocation input; its file stem becomes the artifact ID and --artifact-type is required

      --artifact-type <ARTIFACT_TYPE>
          Declare the active methodology's artifact type for --artifact-file; --artifact-file is required

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version

Live observation:
  agentd run streams compact followable transcript activity by default while the session executes. Use --progress full for raw transcript event detail.

Examples:
  agentd run <AGENT> [REPO]
  agentd run <AGENT> [REPO] --intent <STATEMENT>
  agentd run <AGENT> [REPO] --work-unit <REFERENCE>
  agentd run <AGENT> [REPO] --artifact-type <TYPE> --artifact-file <ID>.json
  agentd run <AGENT> [REPO] --work-unit <REFERENCE> --artifact-type work-unit --artifact-file <ID>.json
"#
);

#[test]
fn binary_run_help_matches_the_invocation_contract_exactly() {
    assert_help(&["run", "--help"], RUN_HELP);
}

const WISH_HELP: &str = concat!(
    r#"Interactively elicit the state the operator wants made true and an optional target, or accept an existing tracker work-unit reference, then ask the running daemon to seed one agent session. Prose input is validated as a canonical intent against the active methodology's intent schema.

Use wish for guided desired-state entry. Use run when invocation input is already prepared, or daemon to start the service that accepts requests.

Usage: agentd wish [OPTIONS] <AGENT> [REPO]

Arguments:
  <AGENT>
          Select an agent declared under this name in the daemon configuration

  [REPO]
          Clone this Git repository for the session; when omitted, use the selected agent's daemon-configured repository

Options:
      --socket-path <SOCKET_PATH>
          Send the request through this control socket instead of $XDG_RUNTIME_DIR/agentd/agentd.sock

      --work-unit <WORK_UNIT>
          Seed from this existing tracker work-unit reference instead of eliciting prose; runa resolves it before scoped work begins

      --progress <PROGRESS>
          Choose how much live transcript activity to print while the session runs

          Possible values:
          - summary: Print compact live transcript activity
          - full:    Print raw live transcript event detail
"#,
    "          \n",
    r#"          [default: summary]

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version

Live observation:
  agentd wish streams compact followable transcript activity by default while the session executes. Use --progress full for raw transcript event detail.

Prompts:
  Speak a wish: the state you want made true.
  What do you wish to be true?
  What is this wish aimed at? Leave blank if it has no target.

Examples:
  agentd wish <AGENT> [REPO]
  agentd wish <AGENT> [REPO] --work-unit <REFERENCE>
"#
);

#[test]
fn binary_wish_help_matches_the_invocation_contract_exactly() {
    assert_help(&["wish", "--help"], WISH_HELP);
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
            "81",
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
        stdout.contains("Speak a wish: the state you want made true."),
        "wish should greet the operator before prompting: {stdout}"
    );
    assert!(
        stdout.contains("What do you wish to be true?"),
        "wish should elicit an intent statement: {stdout}"
    );
    assert!(
        stdout.contains("What is this wish aimed at? Leave blank if it has no target."),
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

    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert!(
        stdout.contains("Speak a wish: the state you want made true."),
        "wish should greet the operator before prompting: {stdout}"
    );
    assert!(
        stdout.contains("What do you wish to be true?"),
        "wish should elicit an intent statement: {stdout}"
    );
    assert!(
        stdout.contains("What is this wish aimed at? Leave blank if it has no target."),
        "wish should elicit an optional target: {stdout}"
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
    let artifact_path = runtime_dir.join("76.json");
    std::fs::write(&artifact_path, r#"{"id":"76","title":"Execute work mode"}"#)
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
            "76",
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
    assert_eq!(invocation.work_unit.as_deref(), Some("76"));
    assert_eq!(
        invocation.input,
        Some(InvocationInput::Artifact {
            artifact_type: "work-unit".to_string(),
            artifact_id: "76".to_string(),
            document: json!({
                "id": "76",
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

#[test]
fn binary_wish_command_seeds_work_unit_arm_and_skips_prose_elicitation() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agentd-cli-runtime-wish-work-unit-arm-{}-{}",
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
        "client-command-wish-work-unit-arm",
        &daemon_test_config(&socket_path, &pid_file),
    );
    let (shutdown, handle, _config, invocations) =
        start_recording_test_daemon(&config_path, SessionOutcome::Success { exit_code: 0 });

    // stdin is closed: the work-unit arm must not block on or read prose.
    let output = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "wish",
            "--socket-path",
            socket_path.to_str().expect("socket path should be utf-8"),
            "site-builder",
            "https://example.com/repo.git",
            "--work-unit",
            "81",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("agentd binary should run");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        output.status.success(),
        "wish work-unit arm should succeed without prose input: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");
    assert!(
        !stdout.contains("Speak a wish: the state you want made true."),
        "wish work-unit arm must not elicit prose, so a single invocation \
         cannot carry both an intent and a work-unit: {stdout}"
    );

    let invocation = invocations.lock().expect("invocations should lock")[0].clone();
    assert_eq!(
        invocation.work_unit.as_deref(),
        Some("81"),
        "wish work-unit arm should enter the reference as a work-unit"
    );
    assert_eq!(
        invocation.input, None,
        "wish work-unit arm should reach the same downstream request shape as `run --work-unit`: \
         a work-unit reference with no explicit invocation input"
    );
}
