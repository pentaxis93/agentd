use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use agentd::config::{Config, ConfigError};
use agentd::daemon::run_daemon_until_shutdown_with_reconciler;
use agentd::{
    ClientError, DaemonError, RunRequest, RunnerSessionExecutor, SessionExecutor, request_run,
};
use agentd_runner::InvocationInput;
use agentd_runner::{
    RunnerError, SessionInvocation, SessionOutcome, SessionSpec, StartupReconciliationReport,
};
use serde_json::json;

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

#[derive(Clone)]
struct BlockingFirstRunExecutor {
    state: Arc<BlockingFirstRunState>,
    first_outcome: SessionOutcome,
    later_outcome: SessionOutcome,
}

struct BlockingFirstRunState {
    calls: AtomicUsize,
    first_started: (Mutex<bool>, Condvar),
    first_released: (Mutex<bool>, Condvar),
}

impl BlockingFirstRunExecutor {
    fn new(first_outcome: SessionOutcome, later_outcome: SessionOutcome) -> Self {
        Self {
            state: Arc::new(BlockingFirstRunState {
                calls: AtomicUsize::new(0),
                first_started: (Mutex::new(false), Condvar::new()),
                first_released: (Mutex::new(false), Condvar::new()),
            }),
            first_outcome,
            later_outcome,
        }
    }

    fn wait_for_first_run_to_start(&self) {
        let (lock, cvar) = &self.state.first_started;
        let started = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let timeout = Duration::from_secs(5);
        let (started, _) = cvar
            .wait_timeout_while(started, timeout, |started| !*started)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(*started, "timed out waiting for first executor call");
    }

    fn release_first_run(&self) {
        let (lock, cvar) = &self.state.first_released;
        let mut released = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *released = true;
        cvar.notify_all();
    }
}

impl SessionExecutor for BlockingFirstRunExecutor {
    fn run_session(
        &self,
        _spec: SessionSpec,
        _invocation: SessionInvocation,
    ) -> Result<SessionOutcome, RunnerError> {
        let call_index = self.state.calls.fetch_add(1, Ordering::AcqRel);
        if call_index == 0 {
            let (started_lock, started_cvar) = &self.state.first_started;
            let mut started = started_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *started = true;
            started_cvar.notify_all();
            drop(started);

            let (released_lock, released_cvar) = &self.state.first_released;
            let released = released_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (_released, _) = released_cvar
                .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            return Ok(self.first_outcome.clone());
        }

        Ok(self.later_outcome.clone())
    }
}

fn unique_runtime_dir(name: &str) -> PathBuf {
    let unique = format!(
        "agentd-daemon-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    );
    let path = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&path).expect("runtime dir should be created");
    path
}

fn config_in_runtime_dir(runtime_dir: &std::path::Path) -> Config {
    Config::from_str(&format!(
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
        socket_path = runtime_dir.join("agentd.sock").display(),
        pid_file = runtime_dir.join("agentd.pid").display(),
    ))
    .expect("config should parse")
}

fn wait_for_path(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }

    panic!("timed out waiting for {}", path.display());
}

fn wait_for_path_removal(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }

    panic!("timed out waiting for removal of {}", path.display());
}

fn run_daemon_until_shutdown_for_test(
    config: Config,
    executor: impl SessionExecutor + Send + Sync + Clone + 'static,
    shutdown: Arc<AtomicBool>,
) -> Result<(), DaemonError> {
    let _daemon_instance_id = config.daemon().daemon_instance_id()?;
    run_daemon_until_shutdown_with_reconciler(config, executor, shutdown, || {
        Ok(StartupReconciliationReport::default())
    })
}

#[test]
fn daemon_reports_run_outcome_back_through_client_request() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("manual-run");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let executor = FixedOutcomeExecutor {
        outcome: SessionOutcome::GenericFailure { exit_code: 23 },
    };
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let outcome = request_run(
        config.daemon(),
        &RunRequest {
            agent: "site-builder".to_string(),
            repo_url: Some("https://example.com/repo.git".to_string()),
            work_unit: Some("task-42".to_string()),
            input: None,
        },
    )
    .expect("client request should succeed");

    assert_eq!(outcome, SessionOutcome::GenericFailure { exit_code: 23 });

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }
}

#[test]
fn client_reports_clear_error_when_daemon_is_not_running() {
    let runtime_dir = unique_runtime_dir("not-running");
    let config = config_in_runtime_dir(&runtime_dir);

    let error = request_run(
        config.daemon(),
        &RunRequest {
            agent: "site-builder".to_string(),
            repo_url: Some("https://example.com/repo.git".to_string()),
            work_unit: None,
            input: None,
        },
    )
    .expect_err("missing daemon should be reported");

    match error {
        ClientError::DaemonNotRunning { path } => {
            assert_eq!(path, config.daemon().socket_path());
        }
        other => panic!("expected daemon-not-running error, got {other:?}"),
    }
}

#[test]
fn daemon_round_trips_typed_invocation_input_through_the_socket_protocol() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("typed-input");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let (executor, invocations) =
        RecordingInvocationExecutor::new(SessionOutcome::Success { exit_code: 0 });
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let outcome = request_run(
        config.daemon(),
        &RunRequest {
            agent: "site-builder".to_string(),
            repo_url: Some("https://example.com/repo.git".to_string()),
            work_unit: None,
            input: Some(InvocationInput::Artifact {
                artifact_type: "claim".to_string(),
                artifact_id: "claim".to_string(),
                document: json!({ "summary": "Ship it" }),
            }),
        },
    )
    .expect("client request should succeed");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    let invocation = invocations.lock().expect("invocations should lock")[0].clone();
    assert_eq!(
        invocation.input,
        Some(InvocationInput::Artifact {
            artifact_type: "claim".to_string(),
            artifact_id: "claim".to_string(),
            document: json!({ "summary": "Ship it" }),
        })
    );

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }
}

#[test]
fn daemon_round_trips_target_bearing_intent_text_through_the_socket_protocol() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("target-bearing-intent");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let (executor, invocations) =
        RecordingInvocationExecutor::new(SessionOutcome::Success { exit_code: 0 });
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let outcome = request_run(
        config.daemon(),
        &RunRequest {
            agent: "site-builder".to_string(),
            repo_url: Some("https://example.com/repo.git".to_string()),
            work_unit: None,
            input: Some(InvocationInput::IntentText {
                statement: "Work on the tracker item".to_string(),
                target: Some("tesserine/agentd#154".to_string()),
            }),
        },
    )
    .expect("client request should succeed");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    let invocation = invocations.lock().expect("invocations should lock")[0].clone();
    assert_eq!(
        invocation.input,
        Some(InvocationInput::IntentText {
            statement: "Work on the tracker item".to_string(),
            target: Some("tesserine/agentd#154".to_string()),
        })
    );

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }
}

#[test]
fn daemon_round_trips_work_unit_artifact_input_through_the_socket_protocol() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("work-unit-artifact-input");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let (executor, invocations) =
        RecordingInvocationExecutor::new(SessionOutcome::Success { exit_code: 0 });
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let outcome = request_run(
        config.daemon(),
        &RunRequest {
            agent: "site-builder".to_string(),
            repo_url: Some("https://example.com/repo.git".to_string()),
            work_unit: Some("76".to_string()),
            input: Some(InvocationInput::Artifact {
                artifact_type: "work-unit".to_string(),
                artifact_id: "76".to_string(),
                document: json!({ "id": "76" }),
            }),
        },
    )
    .expect("client request should succeed");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    let invocation = invocations.lock().expect("invocations should lock")[0].clone();
    assert_eq!(invocation.work_unit.as_deref(), Some("76"));
    assert_eq!(
        invocation.input,
        Some(InvocationInput::Artifact {
            artifact_type: "work-unit".to_string(),
            artifact_id: "76".to_string(),
            document: json!({ "id": "76" }),
        })
    );

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }
}

#[test]
fn daemon_rejects_conflicting_work_unit_and_input_from_socket_callers() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("conflicting-manual-intent");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, RunnerSessionExecutor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let error = request_run(
        config.daemon(),
        &RunRequest {
            agent: "site-builder".to_string(),
            repo_url: Some("https://example.com/repo.git".to_string()),
            work_unit: Some("42".to_string()),
            input: Some(InvocationInput::IntentText {
                statement: "Add a status page".to_string(),
                target: None,
            }),
        },
    )
    .expect_err("conflicting work_unit and input should be rejected");

    match error {
        ClientError::Server { message } => {
            assert!(
                message.contains("work_unit"),
                "expected work_unit guidance in server message, got {message}"
            );
            assert!(
                message.contains("input"),
                "expected input guidance in server message, got {message}"
            );
        }
        other => panic!("expected server-side validation error, got {other:?}"),
    }

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }
}

#[test]
fn starting_second_daemon_instance_fails_with_existing_pid() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("already-running");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let first_config = config.clone();
    let first_shutdown = shutdown.clone();
    let executor = FixedOutcomeExecutor {
        outcome: SessionOutcome::Success { exit_code: 0 },
    };
    let first_handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(first_config, executor, first_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let second_result = run_daemon_until_shutdown_for_test(
        config.clone(),
        FixedOutcomeExecutor {
            outcome: SessionOutcome::Success { exit_code: 0 },
        },
        Arc::new(AtomicBool::new(false)),
    );

    match second_result.expect_err("second daemon should fail to start") {
        DaemonError::AlreadyRunning { pid } => {
            assert!(pid.is_some(), "expected locked pid to be reported");
        }
        other => panic!("expected already-running error, got {other:?}"),
    }

    shutdown.store(true, Ordering::Release);
    first_handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }
}

#[test]
fn daemon_shutdown_removes_pid_file_and_socket() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("cleanup");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let executor = FixedOutcomeExecutor {
        outcome: SessionOutcome::Success { exit_code: 0 },
    };
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());
    wait_for_path(config.daemon().pid_file());

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");

    assert!(
        !config.daemon().socket_path().exists(),
        "socket path should be removed on shutdown"
    );
    assert!(
        !config.daemon().pid_file().exists(),
        "pid file should be removed on shutdown"
    );
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }
}

#[test]
fn daemon_accepts_additional_runs_while_a_previous_run_is_still_executing() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("concurrent-runs");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let executor = BlockingFirstRunExecutor::new(
        SessionOutcome::Success { exit_code: 0 },
        SessionOutcome::GenericFailure { exit_code: 23 },
    );
    let daemon_executor = executor.clone();
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, daemon_executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let first_config = config.clone();
    let first_request = thread::spawn(move || {
        request_run(
            first_config.daemon(),
            &RunRequest {
                agent: "site-builder".to_string(),
                repo_url: Some("https://example.com/repo.git".to_string()),
                work_unit: Some("first".to_string()),
                input: None,
            },
        )
    });
    executor.wait_for_first_run_to_start();

    let second_config = config.clone();
    let (second_tx, second_rx) = mpsc::channel();
    let second_request = thread::spawn(move || {
        let outcome = request_run(
            second_config.daemon(),
            &RunRequest {
                agent: "site-builder".to_string(),
                repo_url: Some("https://example.com/repo.git".to_string()),
                work_unit: Some("second".to_string()),
                input: None,
            },
        );
        second_tx
            .send(outcome)
            .expect("second request result should be reported");
    });

    let second_completed_promptly = match second_rx.recv_timeout(Duration::from_millis(500)) {
        Ok(result) => {
            assert_eq!(
                result.expect("second client request should succeed"),
                SessionOutcome::GenericFailure { exit_code: 23 }
            );
            true
        }
        Err(mpsc::RecvTimeoutError::Timeout) => false,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("second request thread disconnected before reporting a result");
        }
    };

    executor.release_first_run();
    assert_eq!(
        first_request
            .join()
            .expect("first request thread should join")
            .expect("first client request should succeed"),
        SessionOutcome::Success { exit_code: 0 }
    );
    second_request
        .join()
        .expect("second request thread should join");

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }

    assert!(
        second_completed_promptly,
        "daemon did not service a second run request while the first run was still executing"
    );
}

#[test]
fn daemon_shutdown_waits_for_an_in_flight_run_to_finish() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("shutdown-during-run");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let executor = BlockingFirstRunExecutor::new(
        SessionOutcome::Success { exit_code: 0 },
        SessionOutcome::Success { exit_code: 0 },
    );
    let daemon_executor = executor.clone();
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, daemon_executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let client_config = config.clone();
    let client_request = thread::spawn(move || {
        request_run(
            client_config.daemon(),
            &RunRequest {
                agent: "site-builder".to_string(),
                repo_url: Some("https://example.com/repo.git".to_string()),
                work_unit: Some("shutdown".to_string()),
                input: None,
            },
        )
    });
    executor.wait_for_first_run_to_start();

    shutdown.store(true, Ordering::Release);

    thread::sleep(Duration::from_millis(500));
    let exited_before_release = handle.is_finished();

    assert!(
        !exited_before_release,
        "daemon exited before the in-flight run finished"
    );

    executor.release_first_run();
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");
    assert_eq!(
        client_request
            .join()
            .expect("client request thread should join")
            .expect("client request should eventually succeed"),
        SessionOutcome::Success { exit_code: 0 }
    );
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }
}

#[test]
fn daemon_shutdown_stops_accepting_new_runs() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("shutdown-rejects-new-runs");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let executor = BlockingFirstRunExecutor::new(
        SessionOutcome::Success { exit_code: 0 },
        SessionOutcome::Success { exit_code: 0 },
    );
    let daemon_executor = executor.clone();
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, daemon_executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let first_config = config.clone();
    let first_request = thread::spawn(move || {
        request_run(
            first_config.daemon(),
            &RunRequest {
                agent: "site-builder".to_string(),
                repo_url: Some("https://example.com/repo.git".to_string()),
                work_unit: Some("draining".to_string()),
                input: None,
            },
        )
    });
    executor.wait_for_first_run_to_start();

    shutdown.store(true, Ordering::Release);
    wait_for_path_removal(config.daemon().socket_path());

    let error = request_run(
        config.daemon(),
        &RunRequest {
            agent: "site-builder".to_string(),
            repo_url: Some("https://example.com/repo.git".to_string()),
            work_unit: Some("rejected".to_string()),
            input: None,
        },
    )
    .expect_err("new run should be rejected once shutdown begins");

    executor.release_first_run();
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");
    assert_eq!(
        first_request
            .join()
            .expect("first request thread should join")
            .expect("first client request should succeed"),
        SessionOutcome::Success { exit_code: 0 }
    );
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }

    match error {
        ClientError::DaemonNotRunning { path } => {
            assert_eq!(path, config.daemon().socket_path());
        }
        other => panic!("expected daemon-not-running error, got {other:?}"),
    }
}

#[test]
fn daemon_created_runtime_socket_and_directory_are_private() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_root = unique_runtime_dir("runtime-permissions");
    let socket_dir = runtime_root.join("private-runtime");
    let config = Config::from_str(&format!(
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
        socket_path = socket_dir.join("agentd.sock").display(),
        pid_file = socket_dir.join("agentd.pid").display(),
    ))
    .expect("config should parse");
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(
            daemon_config,
            FixedOutcomeExecutor {
                outcome: SessionOutcome::Success { exit_code: 0 },
            },
            daemon_shutdown,
        )
    });
    wait_for_path(config.daemon().socket_path());

    let socket_mode = std::fs::metadata(config.daemon().socket_path())
        .expect("socket metadata should be readable")
        .permissions()
        .mode()
        & 0o777;
    let runtime_dir_mode = std::fs::metadata(&socket_dir)
        .expect("runtime directory metadata should be readable")
        .permissions()
        .mode()
        & 0o777;

    shutdown.store(true, Ordering::Release);
    handle
        .join()
        .expect("daemon thread should join")
        .expect("daemon should exit cleanly");
    unsafe {
        std::env::remove_var("AGENTD_GITHUB_TOKEN");
    }

    assert_eq!(socket_mode, 0o600, "socket should be private to the daemon");
    assert_eq!(
        runtime_dir_mode, 0o700,
        "daemon-created runtime directory should be private to the daemon"
    );
}

#[test]
fn daemon_startup_refuses_to_delete_a_non_socket_socket_path() {
    let runtime_dir = unique_runtime_dir("non-socket-path");
    let config = config_in_runtime_dir(&runtime_dir);
    let original_contents = "do not delete me";
    std::fs::write(config.daemon().socket_path(), original_contents)
        .expect("non-socket placeholder file should be written");

    let error = run_daemon_until_shutdown_for_test(
        config.clone(),
        FixedOutcomeExecutor {
            outcome: SessionOutcome::Success { exit_code: 0 },
        },
        Arc::new(AtomicBool::new(false)),
    )
    .expect_err("daemon startup should fail for a non-socket socket_path");

    assert_eq!(
        error.to_string(),
        format!(
            "socket_path exists but is not a Unix socket: {}",
            config.daemon().socket_path().display()
        )
    );
    assert_eq!(
        std::fs::read_to_string(config.daemon().socket_path())
            .expect("non-socket placeholder file should remain"),
        original_contents
    );
}

#[test]
fn daemon_startup_rejects_relative_daemon_runtime_paths_before_claiming_runtime() {
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

    let error = run_daemon_until_shutdown_for_test(
        config,
        FixedOutcomeExecutor {
            outcome: SessionOutcome::Success { exit_code: 0 },
        },
        Arc::new(AtomicBool::new(false)),
    )
    .expect_err("relative daemon paths should abort startup");

    match error {
        DaemonError::Config(ConfigError::RelativeDaemonRuntimePath { field, path }) => {
            assert_eq!(field, "daemon.socket_path");
            assert_eq!(path, PathBuf::from("runtime/agentd.sock"));
        }
        other => panic!("expected config error, got {other:?}"),
    }
}
