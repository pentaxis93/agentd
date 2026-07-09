use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use agentd::config::{Config, ConfigError};
use agentd::daemon::run_daemon_until_shutdown_with_reconciler;
use agentd::{
    ClientError, DaemonError, LiveObservationLevel, RunRequest, RunnerSessionExecutor,
    SessionExecutor, request_run, request_run_with_live_observation,
};
use agentd_runner::InvocationInput;
use agentd_runner::{
    RunnerError, SessionInvocation, SessionOutcome, SessionProgressEvent, SessionProgressObserver,
    SessionSpec, StartupReconciliationReport,
};
use serde_json::json;

struct ChannelWriter {
    tx: mpsc::Sender<String>,
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf).to_string();
        self.tx
            .send(text)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "receiver closed"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

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

#[derive(Clone)]
struct BlockingFirstRunExecutor {
    state: Arc<BlockingFirstRunState>,
    first_outcome: SessionOutcome,
    later_outcome: SessionOutcome,
    first_progress_lines: Vec<String>,
}

struct BlockingFirstRunState {
    calls: AtomicUsize,
    first_started: (Mutex<bool>, Condvar),
    first_released: (Mutex<bool>, Condvar),
}

const SATURATING_PROGRESS_EVENTS: usize = 256;
const SMALL_SOCKET_RECEIVE_BUFFER_BYTES: libc::c_int = 4096;
#[derive(Clone)]
struct BlockingBurstProgressExecutor {
    state: Arc<BlockingBurstProgressState>,
    progress_line: String,
}

struct BlockingBurstProgressState {
    progress_emitted: (Mutex<bool>, Condvar),
    released: AtomicBool,
}

impl BlockingBurstProgressExecutor {
    fn new() -> Self {
        Self {
            state: Arc::new(BlockingBurstProgressState {
                progress_emitted: (Mutex::new(false), Condvar::new()),
                released: AtomicBool::new(false),
            }),
            progress_line: format!(
                r#"{{"schema_version":1,"source":"runa","kind":"agent_input","content":"{}"}}"#,
                "x".repeat(96 * 1024)
            ),
        }
    }

    fn wait_for_progress_to_emit(&self) {
        let (lock, cvar) = &self.state.progress_emitted;
        let emitted = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (emitted, _) = cvar
            .wait_timeout_while(emitted, Duration::from_secs(10), |emitted| !*emitted)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(*emitted, "timed out waiting for progress burst");
    }

    fn release(&self) {
        self.state.released.store(true, Ordering::Release);
    }
}

impl SessionExecutor for BlockingBurstProgressExecutor {
    fn run_session(
        &self,
        _spec: SessionSpec,
        _invocation: SessionInvocation,
        progress: &dyn SessionProgressObserver,
    ) -> Result<SessionOutcome, RunnerError> {
        let mut index = 0_usize;
        while !self.state.released.load(Ordering::Acquire) {
            progress.observe(SessionProgressEvent::TranscriptEvent {
                session_id: format!("fake-session-{index}"),
                line: self.progress_line.clone(),
            });
            index += 1;

            if index == SATURATING_PROGRESS_EVENTS {
                let (emitted_lock, emitted_cvar) = &self.state.progress_emitted;
                let mut emitted = emitted_lock
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *emitted = true;
                emitted_cvar.notify_all();
            }

            thread::sleep(Duration::from_millis(1));
        }
        Ok(SessionOutcome::Success { exit_code: 0 })
    }
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
            first_progress_lines: Vec::new(),
        }
    }

    fn with_first_progress_line(mut self, line: &str) -> Self {
        self.first_progress_lines.push(line.to_string());
        self
    }

    fn with_first_progress_lines(mut self, lines: &[&str]) -> Self {
        self.first_progress_lines
            .extend(lines.iter().map(|line| (*line).to_string()));
        self
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
        progress: &dyn SessionProgressObserver,
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

            for line in &self.first_progress_lines {
                progress.observe(SessionProgressEvent::TranscriptEvent {
                    session_id: "fake-session-1".to_string(),
                    line: line.clone(),
                });
            }

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

fn set_small_receive_buffer(stream: &UnixStream) {
    let size = SMALL_SOCKET_RECEIVE_BUFFER_BYTES;
    let result = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&size as *const libc::c_int).cast(),
            std::mem::size_of_val(&size) as libc::socklen_t,
        )
    };
    assert_eq!(
        result,
        0,
        "failed to constrain client receive buffer: {}",
        std::io::Error::last_os_error()
    );
}

fn write_raw_run_request(client: &mut UnixStream, work_unit: &str) {
    writeln!(
        client,
        "{}",
        json!({
            "type": "run",
            "agent": "site-builder",
            "repo_url": "https://example.com/repo.git",
            "work_unit": work_unit,
            "input": null
        })
    )
    .expect("client request should write");
}

fn read_response_stream(client: &mut UnixStream) -> Vec<u8> {
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("client read timeout should be set");
    let mut response = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match client.read(&mut buffer) {
            Ok(0) => break,
            Ok(bytes_read) => response.extend_from_slice(&buffer[..bytes_read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                panic!("client timed out before daemon closed the response stream")
            }
            Err(error) => panic!("client failed to read daemon response: {error}"),
        }
    }
    response
}

fn unread_socket_bytes(stream: &UnixStream) -> usize {
    let mut bytes: libc::c_int = 0;
    let result = unsafe { libc::ioctl(stream.as_raw_fd(), libc::FIONREAD, &mut bytes) };
    assert_eq!(
        result,
        0,
        "failed to inspect unread client bytes: {}",
        std::io::Error::last_os_error()
    );
    usize::try_from(bytes).expect("unread byte count should be non-negative")
}

fn wait_for_backpressured_client(stream: &UnixStream, minimum_unread_bytes: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_observed = 0_usize;
    let mut stable_since = Instant::now();
    loop {
        let unread = unread_socket_bytes(stream);
        if unread > last_observed {
            last_observed = unread;
            stable_since = Instant::now();
        }

        if last_observed >= minimum_unread_bytes
            && stable_since.elapsed() >= Duration::from_millis(100)
        {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for client receive queue to backpressure the writer; last unread bytes: {last_observed}"
        );
        thread::sleep(Duration::from_millis(10));
    }
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
fn client_receives_execution_progress_while_session_is_still_running() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("live-progress");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let executor = BlockingFirstRunExecutor::new(
        SessionOutcome::Success { exit_code: 0 },
        SessionOutcome::Success { exit_code: 0 },
    )
    .with_first_progress_lines(&[
        r#"{"schema_version":1,"source":"runa","kind":"agent_input","content":"working step"}"#,
        r#"{"schema_version":1,"source":"runa-mcp","kind":"tool_call","protocol":"mcp","action":"tools/call","tool":"shell"}"#,
        r#"{"schema_version":1,"source":"runa","kind":"agent_exit","success":true,"exit_code":0}"#,
    ]);
    let daemon_executor = executor.clone();
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, daemon_executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let (progress_tx, progress_rx) = mpsc::channel();
    let client_config = config.clone();
    let client_request = thread::spawn(move || {
        let mut writer = ChannelWriter { tx: progress_tx };
        request_run_with_live_observation(
            client_config.daemon(),
            &RunRequest {
                agent: "site-builder".to_string(),
                repo_url: Some("https://example.com/repo.git".to_string()),
                work_unit: Some("issue-122".to_string()),
                input: None,
            },
            LiveObservationLevel::Summary,
            &mut writer,
        )
    });

    executor.wait_for_first_run_to_start();
    let mut progress = String::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !progress.contains("success=true") {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "execution progress should reach the client before the session completes"
        );
        progress.push_str(
            &progress_rx
                .recv_timeout(remaining)
                .expect("execution progress should reach the client before the session completes"),
        );
    }
    for expected in [
        "session event: runa/agent_input working step",
        "session event: runa-mcp/tool_call protocol=mcp action=tools/call tool=shell",
        "session event: runa/agent_exit success=true exit_code=0",
    ] {
        assert!(
            progress.contains(expected),
            "default summary progress should include followable transcript detail {expected:?}: {progress}"
        );
    }
    assert!(
        !client_request.is_finished(),
        "client should still be waiting for the terminal outcome after progress arrives"
    );

    executor.release_first_run();
    assert_eq!(
        client_request
            .join()
            .expect("client request thread should join")
            .expect("client request should succeed"),
        SessionOutcome::Success { exit_code: 0 }
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
fn client_full_progress_includes_raw_execution_event_detail() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("full-live-progress");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let executor = BlockingFirstRunExecutor::new(
        SessionOutcome::Success { exit_code: 0 },
        SessionOutcome::Success { exit_code: 0 },
    )
    .with_first_progress_line(
        r#"{"schema_version":1,"source":"runa-mcp","kind":"tool_call","protocol":"mcp","action":"tools/call","success":true,"content":"working step"}"#,
    );
    let daemon_executor = executor.clone();
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, daemon_executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let (progress_tx, progress_rx) = mpsc::channel();
    let client_config = config.clone();
    let client_request = thread::spawn(move || {
        let mut writer = ChannelWriter { tx: progress_tx };
        request_run_with_live_observation(
            client_config.daemon(),
            &RunRequest {
                agent: "site-builder".to_string(),
                repo_url: Some("https://example.com/repo.git".to_string()),
                work_unit: Some("issue-122".to_string()),
                input: None,
            },
            LiveObservationLevel::Full,
            &mut writer,
        )
    });

    executor.wait_for_first_run_to_start();
    let mut progress = String::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !progress.contains(r#""content":"working step""#) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "full execution progress should reach the client before the session completes"
        );
        progress.push_str(
            &progress_rx
                .recv_timeout(remaining)
                .expect("full execution progress should reach the client before completion"),
        );
    }
    assert!(progress.contains("session_id=fake-session-1"), "{progress}");
    for expected in [
        r#""source":"runa-mcp""#,
        r#""kind":"tool_call""#,
        r#""protocol":"mcp""#,
        r#""action":"tools/call""#,
        r#""success":true"#,
    ] {
        assert!(
            progress.contains(expected),
            "full progress should include raw event detail {expected:?}: {progress}"
        );
    }
    assert!(
        !client_request.is_finished(),
        "client should still be waiting for the terminal outcome after full progress arrives"
    );

    executor.release_first_run();
    assert_eq!(
        client_request
            .join()
            .expect("client request thread should join")
            .expect("client request should succeed"),
        SessionOutcome::Success { exit_code: 0 }
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
fn non_reading_progress_client_does_not_block_daemon_completion() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::set_var("AGENTD_GITHUB_TOKEN", "runtime-secret");
    }
    let runtime_dir = unique_runtime_dir("non-reading-progress-client");
    let config = config_in_runtime_dir(&runtime_dir);
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_config = config.clone();
    let daemon_shutdown = shutdown.clone();
    let executor = BlockingBurstProgressExecutor::new();
    let daemon_executor = executor.clone();
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, daemon_executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let mut client =
        Some(UnixStream::connect(config.daemon().socket_path()).expect("client should connect"));
    set_small_receive_buffer(client.as_ref().expect("client should exist"));
    let backpressure_work_unit = format!("issue-122-{}", "w".repeat(112 * 1024));
    write_raw_run_request(
        client.as_mut().expect("client should exist"),
        &backpressure_work_unit,
    );

    thread::sleep(Duration::from_millis(50));

    executor.wait_for_progress_to_emit();
    wait_for_backpressured_client(client.as_ref().expect("client should exist"), 200 * 1024);

    shutdown.store(true, Ordering::Release);
    let (join_tx, join_rx) = mpsc::channel();
    let joiner = thread::spawn(move || {
        let result = handle.join().expect("daemon thread should not panic");
        join_tx
            .send(result)
            .expect("daemon join result should send");
    });
    executor.release();

    let mut client = client.take().expect("client should still be connected");
    let response = read_response_stream(&mut client);
    assert!(
        !response.is_empty(),
        "daemon should have written progress before abandoning the stalled response"
    );
    let join_result = join_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("daemon should join after bounded non-reader cleanup");
    joiner.join().expect("join helper should join");
    join_result.expect("daemon should exit cleanly");
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
    let executor = BlockingBurstProgressExecutor::new();
    let daemon_executor = executor.clone();
    let handle = thread::spawn(move || {
        run_daemon_until_shutdown_for_test(daemon_config, daemon_executor, daemon_shutdown)
    });
    wait_for_path(config.daemon().socket_path());

    let mut client =
        Some(UnixStream::connect(config.daemon().socket_path()).expect("client should connect"));
    set_small_receive_buffer(client.as_ref().expect("client should exist"));
    write_raw_run_request(client.as_mut().expect("client should exist"), "shutdown");
    executor.wait_for_progress_to_emit();
    wait_for_backpressured_client(client.as_ref().expect("client should exist"), 128 * 1024);

    shutdown.store(true, Ordering::Release);
    let (join_tx, join_rx) = mpsc::channel();
    let joiner = thread::spawn(move || {
        let result = handle.join().expect("daemon thread should not panic");
        join_tx
            .send(result)
            .expect("daemon join result should send");
    });

    executor.release();
    thread::sleep(Duration::from_millis(250));

    let client = client.take().expect("client should still be connected");
    let (terminal_tx, terminal_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("client read timeout should be set");
        let mut reader = BufReader::new(client);
        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = reader
                .read_line(&mut line)
                .expect("client should read daemon JSONL");
            if bytes_read == 0 {
                return false;
            }
            let value: serde_json::Value =
                serde_json::from_str(&line).expect("daemon must emit complete JSON frames");
            if value.get("type").and_then(serde_json::Value::as_str) == Some("session_outcome") {
                terminal_tx
                    .send(())
                    .expect("terminal observation should send");
                return true;
            }
        }
    });

    let mut daemon_finished_before_terminal = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if terminal_rx.try_recv().is_ok() {
            break;
        }
        match join_rx.try_recv() {
            Ok(result) => {
                daemon_finished_before_terminal = Some(result);
                break;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                panic!("daemon join helper disconnected before reporting a result")
            }
        }
        assert!(
            Instant::now() < deadline,
            "terminal outcome should reach the client before the drain bound expires"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let saw_terminal = reader.join().expect("reader thread should join");
    let daemon_finished_first = daemon_finished_before_terminal.is_some();
    let daemon_result = match daemon_finished_before_terminal {
        Some(result) => result,
        None => join_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("daemon should join after terminal drain"),
    };
    joiner.join().expect("join helper should join");
    daemon_result.expect("daemon should exit cleanly");
    assert!(saw_terminal, "client should parse the terminal outcome");
    assert!(
        !daemon_finished_first,
        "daemon shutdown completed before the terminal outcome crossed the writer-to-client boundary"
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
