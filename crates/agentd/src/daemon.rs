use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError, mpsc};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use agentd_runner::{
    RunnerError, SessionOutcome, SessionProgressEvent, StartupReconciliationReport,
    reconcile_startup_resources,
};

use crate::audit_root::prepare_audit_root;
use crate::config::{Config, ConfigError};
use crate::dispatch::dispatch_run_after_preflight;
use crate::protocol::{ProgressMessage, RequestMessage, ResponseMessage};
use crate::scheduler::{join_scheduler_thread, spawn_scheduler_thread};
use crate::{DispatchError, RunRequest, SessionExecutor};

const ACCEPT_TIMEOUT: Duration = Duration::from_millis(100);
const RUNTIME_DIR_MODE: u32 = 0o700;
const SOCKET_MODE: u32 = 0o600;
const SHUTDOWN_MESSAGE: &str = "agentd is shutting down";
const MAX_PROGRESS_FRAME_BYTES: usize = 128 * 1024;
const PROGRESS_QUEUE_CAPACITY: usize = 64;
const PROGRESS_TERMINAL_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const SUMMARY_PREVIEW_CHARS: usize = 240;
const SUMMARY_MAX_FIELDS: usize = 6;

/// Startup or runtime failures for the foreground daemon loop.
#[derive(Debug)]
pub enum DaemonError {
    AlreadyRunning { pid: Option<u32> },
    Config(ConfigError),
    Io(io::Error),
    StartupReconciliation(RunnerError),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning { pid: Some(pid) } => {
                write!(f, "agentd is already running (pid {pid})")
            }
            Self::AlreadyRunning { pid: None } => write!(f, "agentd is already running"),
            Self::Config(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::StartupReconciliation(error) => {
                write!(f, "startup reconciliation failed: {error}")
            }
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::StartupReconciliation(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ConfigError> for DaemonError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<io::Error> for DaemonError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Errors returned to daemon client commands.
#[derive(Debug)]
pub enum ClientError {
    DaemonNotRunning { path: PathBuf },
    Io(io::Error),
    Protocol(serde_json::Error),
    Server { message: String },
}

/// Client-side rendering level for live session observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveObservationLevel {
    /// Print compact followable transcript activity while waiting for the terminal outcome.
    Summary,
    /// Print raw progress frame detail for live session inspection.
    Full,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DaemonNotRunning { path } => {
                write!(f, "agentd is not running (socket {})", path.display())
            }
            Self::Io(error) => write!(f, "{error}"),
            Self::Protocol(error) => write!(f, "{error}"),
            Self::Server { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::DaemonNotRunning { .. } | Self::Server { .. } => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(error: serde_json::Error) -> Self {
        Self::Protocol(error)
    }
}

fn log_manual_run_completed(agent: &str, work_unit: Option<&str>, outcome: &SessionOutcome) {
    match outcome {
        SessionOutcome::TimedOut => tracing::warn!(
            event = "agentd.manual_run_completed",
            agent = agent,
            work_unit = work_unit.unwrap_or(""),
            work_unit_present = work_unit.is_some(),
            outcome = outcome.label(),
            "manual run completed"
        ),
        SessionOutcome::Success { .. }
        | SessionOutcome::Blocked { .. }
        | SessionOutcome::NothingReady { .. } => tracing::info!(
            event = "agentd.manual_run_completed",
            agent = agent,
            work_unit = work_unit.unwrap_or(""),
            work_unit_present = work_unit.is_some(),
            outcome = outcome.label(),
            exit_code = outcome.exit_code(),
            signal = outcome.signal(),
            "manual run completed"
        ),
        _ => tracing::warn!(
            event = "agentd.manual_run_completed",
            agent = agent,
            work_unit = work_unit.unwrap_or(""),
            work_unit_present = work_unit.is_some(),
            outcome = outcome.label(),
            exit_code = outcome.exit_code(),
            signal = outcome.signal(),
            "manual run completed"
        ),
    }
}

/// Run the foreground daemon through one structured lifecycle: claim runtime,
/// reconcile startup resources, bind the listener, start the scheduler, accept
/// connections until shutdown begins or listener accept fails, then assert the
/// shared shutdown flag, stop accepting new connections, drain started
/// handlers, stop the scheduler, and clean up runtime-owned resources.
pub fn run_daemon_until_shutdown(
    config: Config,
    executor: impl SessionExecutor + Send + Sync + Clone + 'static,
    shutdown: Arc<AtomicBool>,
) -> Result<(), DaemonError> {
    let daemon_instance_id = config.daemon().daemon_instance_id()?;
    let _audit_root = prepare_audit_root(config.daemon())?;
    run_daemon_until_shutdown_with_reconciler(config, executor, shutdown, || {
        reconcile_startup_resources(&daemon_instance_id)
    })
}

#[doc(hidden)]
pub fn run_daemon_until_shutdown_with_reconciler<F>(
    config: Config,
    executor: impl SessionExecutor + Send + Sync + Clone + 'static,
    shutdown: Arc<AtomicBool>,
    reconcile_startup: F,
) -> Result<(), DaemonError>
where
    F: FnOnce() -> Result<StartupReconciliationReport, RunnerError>,
{
    let mut runtime =
        DaemonRuntime::claim(config.daemon().socket_path(), config.daemon().pid_file())?;
    let reconciliation_report = reconcile_startup().map_err(|error| {
        tracing::error!(
            event = "agentd.startup_reconciliation_failed",
            error = %error,
            "agentd startup reconciliation failed"
        );
        DaemonError::StartupReconciliation(error)
    })?;
    tracing::info!(
        event = "agentd.startup_reconciliation_completed",
        removed_container_count = reconciliation_report.removed_container_names.len(),
        removed_secret_count = reconciliation_report.removed_secret_names.len(),
        "agentd startup reconciliation completed"
    );
    runtime.bind_listener()?;
    let executor = Arc::new(executor);
    let mut handlers = Vec::new();
    let scheduler_handle = spawn_scheduler_thread(&config, Arc::clone(&shutdown))?;
    tracing::info!(
        event = "agentd.daemon_started",
        socket_path = %config.daemon().socket_path().display(),
        pid_file = %config.daemon().pid_file().display(),
        "agentd daemon started"
    );

    let loop_result = loop {
        if shutdown.load(Ordering::Acquire) {
            break Ok(());
        }

        match runtime.accept() {
            Ok((stream, _)) => {
                if shutdown.load(Ordering::Acquire) {
                    reject_connection_during_shutdown(stream);
                    continue;
                }

                reap_finished_handlers(&mut handlers);
                handlers.push(spawn_connection_handler(
                    stream,
                    config.clone(),
                    executor.clone(),
                ));
            }
            Err(error) if accept_was_interrupted(&error) => continue,
            Err(error) => break Err(error),
        }
    };

    let finish_result = shutdown_daemon(
        shutdown.as_ref(),
        || runtime.begin_shutdown(),
        handlers,
        scheduler_handle,
        loop_result,
    );
    finish_result.map_err(DaemonError::Io)?;
    tracing::info!(event = "agentd.daemon_stopped", "agentd daemon stopped");
    Ok(())
}

fn shutdown_daemon<F>(
    shutdown: &AtomicBool,
    begin_shutdown: F,
    handlers: Vec<JoinHandle<()>>,
    scheduler_handle: Option<JoinHandle<()>>,
    loop_result: Result<(), io::Error>,
) -> Result<(), io::Error>
where
    F: FnOnce() -> Result<(), io::Error>,
{
    shutdown.store(true, Ordering::Release);
    let shutdown_result = begin_shutdown();
    join_connection_handlers(handlers);
    join_scheduler_thread(scheduler_handle);

    match (loop_result, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(loop_error), Err(shutdown_error)) => {
            tracing::warn!(
                event = "agentd.daemon_shutdown_cleanup_failed_after_accept_error",
                accept_error = %loop_error,
                cleanup_error = %shutdown_error,
                "daemon cleanup failed after listener accept error"
            );
            Err(loop_error)
        }
    }
}

fn accept_was_interrupted(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

fn spawn_connection_handler<E>(
    stream: UnixStream,
    config: Config,
    executor: Arc<E>,
) -> JoinHandle<()>
where
    E: SessionExecutor + Send + Sync + 'static,
{
    thread::spawn(move || {
        handle_connection(stream, &config, executor.as_ref());
    })
}

fn reject_connection_during_shutdown(mut stream: UnixStream) {
    if let Err(error) = write_response(
        &mut stream,
        &ResponseMessage::Error {
            message: SHUTDOWN_MESSAGE.to_string(),
        },
    ) {
        tracing::warn!(
            event = "agentd.operator_connection_rejected_during_shutdown_failed",
            error = %error,
            "failed to reject operator connection during shutdown"
        );
    }
}

fn join_connection_handlers(handlers: Vec<JoinHandle<()>>) {
    for handler in handlers {
        log_handler_panic(handler);
    }
}

fn reap_finished_handlers(handlers: &mut Vec<JoinHandle<()>>) {
    let mut active_handlers = Vec::with_capacity(handlers.len());
    for handler in std::mem::take(handlers) {
        if handler.is_finished() {
            log_handler_panic(handler);
        } else {
            active_handlers.push(handler);
        }
    }
    *handlers = active_handlers;
}

fn log_handler_panic(handler: JoinHandle<()>) {
    if handler.join().is_err() {
        tracing::error!(
            event = "agentd.operator_connection_panicked",
            "operator connection handler panicked"
        );
    }
}

/// Trigger a run against the local daemon and wait for its terminal outcome.
pub fn request_run(
    socket_path: impl AsRef<Path>,
    request: &RunRequest,
) -> Result<SessionOutcome, ClientError> {
    request_run_inner::<io::Sink>(socket_path.as_ref(), request, None)
}

/// Trigger a run and render daemon progress messages while waiting.
pub fn request_run_with_live_observation<W: Write>(
    socket_path: impl AsRef<Path>,
    request: &RunRequest,
    level: LiveObservationLevel,
    writer: &mut W,
) -> Result<SessionOutcome, ClientError> {
    request_run_inner(socket_path.as_ref(), request, Some((level, writer)))
}

fn request_run_inner<W: Write>(
    socket_path: &Path,
    request: &RunRequest,
    observer: Option<(LiveObservationLevel, &mut W)>,
) -> Result<SessionOutcome, ClientError> {
    match send_request(
        socket_path,
        &RequestMessage::Run {
            agent: request.agent.clone(),
            repo_url: request.repo_url.clone(),
            work_unit: request.work_unit.clone(),
            input: request.input.clone(),
        },
        observer,
    )? {
        ResponseMessage::SessionOutcome { outcome } => Ok(outcome.into()),
        ResponseMessage::Error { message } => Err(ClientError::Server { message }),
        ResponseMessage::Pong => Err(ClientError::Server {
            message: "unexpected pong from daemon".to_string(),
        }),
        ResponseMessage::Progress { .. } => Err(ClientError::Server {
            message: "daemon closed the connection before reporting a terminal outcome".to_string(),
        }),
    }
}

pub(crate) fn request_run_without_waiting(
    socket_path: impl AsRef<Path>,
    request: &RunRequest,
) -> Result<(), ClientError> {
    send_request_without_response(
        socket_path.as_ref(),
        &RequestMessage::Run {
            agent: request.agent.clone(),
            repo_url: request.repo_url.clone(),
            work_unit: request.work_unit.clone(),
            input: request.input.clone(),
        },
    )
}

fn send_request<W: Write>(
    socket_path: &Path,
    request: &RequestMessage,
    observer: Option<(LiveObservationLevel, &mut W)>,
) -> Result<ResponseMessage, ClientError> {
    let mut stream = connect_to_daemon(socket_path)?;
    write_request(&mut stream, request)?;

    read_terminal_response(stream, observer)
}

fn send_request_without_response(
    socket_path: &Path,
    request: &RequestMessage,
) -> Result<(), ClientError> {
    let mut stream = connect_to_daemon(socket_path)?;
    write_request(&mut stream, request)?;
    Ok(())
}

fn connect_to_daemon(socket_path: &Path) -> Result<UnixStream, ClientError> {
    UnixStream::connect(socket_path).map_err(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ) {
            ClientError::DaemonNotRunning {
                path: socket_path.to_path_buf(),
            }
        } else {
            ClientError::Io(error)
        }
    })
}

fn write_request(stream: &mut UnixStream, request: &RequestMessage) -> Result<(), ClientError> {
    let payload = serde_json::to_vec(request)?;
    stream.write_all(&payload)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    Ok(())
}

fn read_terminal_response<W: Write>(
    stream: UnixStream,
    mut observer: Option<(LiveObservationLevel, &mut W)>,
) -> Result<ResponseMessage, ClientError> {
    let mut reader = BufReader::new(stream);
    let mut saw_message = false;
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            let message = if saw_message {
                "daemon closed the connection before reporting a terminal outcome"
            } else {
                "daemon closed the connection without a response"
            };
            return Err(ClientError::Server {
                message: message.to_string(),
            });
        }
        saw_message = true;

        let response = serde_json::from_str(&line)?;
        match response {
            ResponseMessage::Progress { progress } => {
                if let Some((level, writer)) = observer.as_mut() {
                    render_progress(progress, *level, &mut **writer)?;
                }
            }
            terminal => return Ok(terminal),
        }
    }
}

fn render_progress<W: Write>(
    progress: ProgressMessage,
    level: LiveObservationLevel,
    writer: &mut W,
) -> Result<(), ClientError> {
    writeln!(
        writer,
        "{}",
        render_operator_progress_line(&progress, level)
    )?;
    writer.flush()?;
    Ok(())
}

fn render_operator_progress_line(
    progress: &ProgressMessage,
    level: LiveObservationLevel,
) -> String {
    match (progress, level) {
        (
            ProgressMessage::DispatchStarted {
                agent, work_unit, ..
            },
            LiveObservationLevel::Summary,
        ) => {
            if let Some(work_unit) = work_unit {
                format!(
                    "session running: {} ({})",
                    OperatorField(agent),
                    OperatorField(work_unit)
                )
            } else {
                format!("session running: {}", OperatorField(agent))
            }
        }
        (
            ProgressMessage::DispatchStarted {
                agent,
                work_unit,
                input_present,
            },
            LiveObservationLevel::Full,
        ) => format!(
            "session running: agent={} work_unit={} input={}",
            OperatorField(agent),
            work_unit
                .as_deref()
                .map(OperatorField)
                .unwrap_or(OperatorField("-")),
            if *input_present { "present" } else { "absent" }
        ),
        (ProgressMessage::TranscriptEvent { line, .. }, LiveObservationLevel::Summary) => {
            let summary = summarize_transcript_event(line);
            format!("session event: {}", OperatorField(&summary))
        }
        (ProgressMessage::TranscriptEvent { session_id, line }, LiveObservationLevel::Full) => {
            format!(
                "session event: session_id={} {}",
                OperatorField(session_id),
                OperatorField(line)
            )
        }
    }
}

struct OperatorField<'a>(&'a str);

impl fmt::Display for OperatorField<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for character in self.0.chars() {
            if character.is_control() {
                for escaped in character.escape_default() {
                    write!(f, "{escaped}")?;
                }
            } else {
                write!(f, "{character}")?;
            }
        }
        Ok(())
    }
}

fn summarize_transcript_event(line: &str) -> String {
    let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
        return format!("unparsed_event {}", preview_field(line));
    };
    let Some(event) = event.as_object() else {
        return format!("unparsed_event {}", preview_field(line));
    };

    let role = transcript_event_role(event);
    if let Some(payload) = ["content", "message", "text"]
        .iter()
        .find_map(|key| nonempty_string_field(event, key))
    {
        return format!("{role} {}", preview_field(payload));
    }

    let fields = transcript_scalar_summary(event);
    if fields.is_empty() {
        format!("{role} {}", preview_field(line))
    } else {
        format!("{role} {}", fields.join(" "))
    }
}

fn transcript_event_role(event: &serde_json::Map<String, serde_json::Value>) -> String {
    let source = nonempty_string_field(event, "source");
    let kind = nonempty_string_field(event, "kind");

    match (source, kind) {
        (Some(source), Some(kind)) => format!("{source}/{kind}"),
        (None, Some(kind)) => kind.to_string(),
        _ => "event".to_string(),
    }
}

fn transcript_scalar_summary(event: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    const PRIORITY_FIELDS: &[&str] = &[
        "protocol",
        "action",
        "tool",
        "name",
        "success",
        "exit_code",
        "signal",
        "error",
    ];
    const RESERVED_FIELDS: &[&str] = &[
        "schema_version",
        "source",
        "kind",
        "content",
        "message",
        "text",
    ];

    let mut fields = Vec::new();
    for key in PRIORITY_FIELDS {
        if let Some(value) = scalar_field_display(event.get(*key)) {
            fields.push(format!("{key}={value}"));
        }
    }

    let mut other_keys = event
        .keys()
        .filter(|key| !PRIORITY_FIELDS.contains(&key.as_str()))
        .filter(|key| !RESERVED_FIELDS.contains(&key.as_str()))
        .collect::<Vec<_>>();
    other_keys.sort();

    for key in other_keys {
        if fields.len() >= SUMMARY_MAX_FIELDS {
            break;
        }
        if let Some(value) = scalar_field_display(event.get(key)) {
            fields.push(format!("{key}={value}"));
        }
    }

    fields
}

fn nonempty_string_field<'a>(
    event: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    event
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn scalar_field_display(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::String(value) if !value.trim().is_empty() => {
            Some(preview_field(value.trim()))
        }
        _ => None,
    }
}

fn preview_field(value: &str) -> String {
    let mut preview = String::new();
    let mut chars = value.chars();

    for _ in 0..SUMMARY_PREVIEW_CHARS {
        let Some(character) = chars.next() else {
            return preview;
        };
        preview.push(character);
    }

    if chars.next().is_some() {
        preview.push_str("...");
    }

    preview
}

fn handle_connection(stream: UnixStream, config: &Config, executor: &impl SessionExecutor) {
    if let Err(error) = handle_connection_inner(stream, config, executor) {
        tracing::warn!(
            event = "agentd.operator_connection_failed",
            error = %error,
            "operator connection handling failed"
        );
    }
}

fn handle_connection_inner(
    mut stream: UnixStream,
    config: &Config,
    executor: &impl SessionExecutor,
) -> Result<(), io::Error> {
    let request = {
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            return Ok(());
        }

        match serde_json::from_str::<RequestMessage>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut stream,
                    &ResponseMessage::Error {
                        message: format!("invalid request: {error}"),
                    },
                )?;
                return Ok(());
            }
        }
    };

    let response = match request {
        RequestMessage::Ping => ResponseMessage::Pong,
        RequestMessage::Run {
            agent,
            repo_url,
            work_unit,
            input,
        } => {
            return handle_run_connection(
                stream, config, executor, agent, repo_url, work_unit, input,
            );
        }
    };

    write_response(&mut stream, &response)
}

fn handle_run_connection(
    stream: UnixStream,
    config: &Config,
    executor: &impl SessionExecutor,
    agent: String,
    repo_url: Option<String>,
    work_unit: Option<String>,
    input: Option<agentd_runner::InvocationInput>,
) -> Result<(), io::Error> {
    let writer = ProgressWriter::spawn(stream)?;
    let progress_sink = writer.sink();
    let response = {
        let dispatch_progress = ResponseMessage::Progress {
            progress: ProgressMessage::DispatchStarted {
                agent: agent.clone(),
                work_unit: work_unit.clone(),
                input_present: input.is_some(),
            },
        };
        let write_progress = |event: SessionProgressEvent| {
            let SessionProgressEvent::TranscriptEvent { session_id, line } = event;
            let progress = ResponseMessage::Progress {
                progress: ProgressMessage::TranscriptEvent { session_id, line },
            };
            progress_sink.enqueue_progress(progress);
        };
        match dispatch_run_after_preflight(
            config,
            &RunRequest {
                agent: agent.clone(),
                repo_url,
                work_unit: work_unit.clone(),
                input,
            },
            executor,
            || {
                progress_sink.enqueue_dispatch_progress(dispatch_progress);
            },
            &write_progress,
        ) {
            Ok(outcome) => {
                log_manual_run_completed(&agent, work_unit.as_deref(), &outcome);
                ResponseMessage::SessionOutcome {
                    outcome: outcome.into(),
                }
            }
            Err(error) => {
                tracing::warn!(
                    event = "agentd.manual_run_rejected",
                    error = %error,
                    "run request rejected"
                );
                ResponseMessage::Error {
                    message: dispatch_error_message(&error),
                }
            }
        }
    };

    writer.finish(response)
}

fn write_response(stream: &mut UnixStream, response: &ResponseMessage) -> Result<(), io::Error> {
    let payload = serde_json::to_vec(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_response_part(stream, &payload)?;
    write_response_part(stream, b"\n")?;
    match stream.flush() {
        Ok(()) => Ok(()),
        Err(error) if peer_disconnected_during_response(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn write_response_part(stream: &mut UnixStream, bytes: &[u8]) -> Result<(), io::Error> {
    match stream.write_all(bytes) {
        Ok(()) => Ok(()),
        Err(error) if peer_disconnected_during_response(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn serialize_response_frame(response: &ResponseMessage) -> Result<Vec<u8>, io::Error> {
    let mut payload = serde_json::to_vec(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    payload.push(b'\n');
    Ok(payload)
}

struct ProgressWriter {
    shared: Arc<ProgressWriterShared>,
    completion: mpsc::Receiver<Result<(), io::Error>>,
    handle: JoinHandle<()>,
    abort_stream: UnixStream,
}

#[derive(Clone)]
struct ProgressWriterSink {
    shared: Arc<ProgressWriterShared>,
}

struct ProgressWriterShared {
    state: Mutex<ProgressWriterState>,
    available: Condvar,
}

struct ProgressWriterState {
    progress: VecDeque<QueuedProgressFrame>,
    terminal: Option<Vec<u8>>,
    closed: bool,
}

struct QueuedProgressFrame {
    bytes: Vec<u8>,
    deliver_before_terminal: bool,
}

enum ProgressWriterFrame {
    Progress(Vec<u8>),
    Terminal(Vec<u8>),
}

impl ProgressWriter {
    fn spawn(stream: UnixStream) -> Result<Self, io::Error> {
        let abort_stream = stream.try_clone()?;
        let shared = Arc::new(ProgressWriterShared {
            state: Mutex::new(ProgressWriterState {
                progress: VecDeque::new(),
                terminal: None,
                closed: false,
            }),
            available: Condvar::new(),
        });
        let (completion_tx, completion) = mpsc::channel();
        let writer_shared = Arc::clone(&shared);
        let handle = thread::spawn(move || {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                run_progress_writer(stream, writer_shared)
            }));
            let completion = match result {
                Ok(result) => result,
                Err(_) => {
                    tracing::error!(
                        event = "agentd.manual_run_progress_writer_panicked",
                        "manual run progress writer panicked"
                    );
                    Err(io::Error::other("manual run progress writer panicked"))
                }
            };
            let _ = completion_tx.send(completion);
        });

        Ok(Self {
            shared,
            completion,
            handle,
            abort_stream,
        })
    }

    fn sink(&self) -> ProgressWriterSink {
        ProgressWriterSink {
            shared: Arc::clone(&self.shared),
        }
    }

    fn finish(self, response: ResponseMessage) -> Result<(), io::Error> {
        let terminal = serialize_response_frame(&response)?;
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.progress.retain(|frame| frame.deliver_before_terminal);
        state.terminal = Some(terminal);
        self.shared.available.notify_one();
        drop(state);

        match self
            .completion
            .recv_timeout(PROGRESS_TERMINAL_DRAIN_TIMEOUT)
        {
            Ok(result) => {
                join_progress_writer(self.handle);
                result
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    event = "agentd.manual_run_progress_terminal_drain_abandoned",
                    timeout_ms = PROGRESS_TERMINAL_DRAIN_TIMEOUT.as_millis(),
                    "manual run progress writer did not drain the terminal response before the deadline"
                );
                abort_progress_writer(self.abort_stream);
                join_progress_writer(self.handle);
                Ok(())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                join_progress_writer(self.handle);
                Err(io::Error::other(
                    "manual run progress writer exited without reporting completion",
                ))
            }
        }
    }
}

impl ProgressWriterSink {
    fn enqueue_progress(&self, response: ResponseMessage) {
        self.enqueue_progress_frame(response, false);
    }

    fn enqueue_dispatch_progress(&self, response: ResponseMessage) {
        let frame = match serialize_response_frame(&response) {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!(
                    event = "agentd.manual_run_progress_serialization_failed",
                    error = %error,
                    "failed to serialize manual run dispatch progress"
                );
                return;
            }
        };
        if frame.len() > MAX_PROGRESS_FRAME_BYTES {
            tracing::debug!(
                event = "agentd.manual_run_progress_dropped",
                frame_bytes = frame.len(),
                max_frame_bytes = MAX_PROGRESS_FRAME_BYTES,
                "dropped oversized manual run dispatch progress frame"
            );
            return;
        }

        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed || state.terminal.is_some() {
            tracing::debug!(
                event = "agentd.manual_run_progress_dropped",
                "dropped manual run dispatch progress after terminal response was queued"
            );
            return;
        }

        state.progress.push_back(QueuedProgressFrame {
            bytes: frame,
            deliver_before_terminal: true,
        });
        self.shared.available.notify_one();
    }

    fn enqueue_progress_frame(&self, response: ResponseMessage, deliver_before_terminal: bool) {
        let frame = match serialize_response_frame(&response) {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!(
                    event = "agentd.manual_run_progress_serialization_failed",
                    error = %error,
                    "failed to serialize manual run progress"
                );
                return;
            }
        };

        if frame.len() > MAX_PROGRESS_FRAME_BYTES {
            tracing::debug!(
                event = "agentd.manual_run_progress_dropped",
                frame_bytes = frame.len(),
                max_frame_bytes = MAX_PROGRESS_FRAME_BYTES,
                "dropped oversized manual run progress frame"
            );
            return;
        }

        match self.shared.state.try_lock() {
            Ok(mut state) => {
                if state.closed || state.terminal.is_some() {
                    tracing::debug!(
                        event = "agentd.manual_run_progress_dropped",
                        "dropped manual run progress after terminal response was queued"
                    );
                    return;
                }

                if state.progress.len() >= PROGRESS_QUEUE_CAPACITY {
                    tracing::debug!(
                        event = "agentd.manual_run_progress_dropped",
                        queue_capacity = PROGRESS_QUEUE_CAPACITY,
                        "dropped manual run progress because the bounded queue is full"
                    );
                    return;
                }

                state.progress.push_back(QueuedProgressFrame {
                    bytes: frame,
                    deliver_before_terminal,
                });
                self.shared.available.notify_one();
            }
            Err(TryLockError::WouldBlock) => {
                tracing::debug!(
                    event = "agentd.manual_run_progress_dropped",
                    "dropped manual run progress while the writer queue was busy"
                );
            }
            Err(TryLockError::Poisoned(_)) => {
                tracing::warn!(
                    event = "agentd.manual_run_progress_queue_poisoned",
                    "failed to lock manual run progress writer queue"
                );
            }
        }
    }
}

fn abort_progress_writer(stream: UnixStream) {
    if let Err(error) = stream.shutdown(Shutdown::Both) {
        tracing::debug!(
            event = "agentd.manual_run_progress_writer_abort_failed",
            error = %error,
            "failed to abort manual run progress writer stream"
        );
    }
}

fn join_progress_writer(handle: JoinHandle<()>) {
    if handle.join().is_err() {
        tracing::error!(
            event = "agentd.manual_run_progress_writer_panicked",
            "manual run progress writer panicked"
        );
    }
}

fn run_progress_writer(
    mut stream: UnixStream,
    shared: Arc<ProgressWriterShared>,
) -> Result<(), io::Error> {
    while let Some(frame) = next_progress_writer_frame(&shared) {
        let is_terminal = matches!(frame, ProgressWriterFrame::Terminal(_));
        let bytes = match frame {
            ProgressWriterFrame::Progress(bytes) | ProgressWriterFrame::Terminal(bytes) => bytes,
        };

        match stream.write_all(&bytes) {
            Ok(()) if is_terminal => return Ok(()),
            Ok(()) => {}
            Err(error) if peer_disconnected_during_response(&error) => return Ok(()),
            Err(error) => {
                tracing::warn!(
                    event = "agentd.manual_run_progress_writer_failed",
                    error = %error,
                    "manual run progress writer closed the client connection after a write failure"
                );
                return Err(error);
            }
        }
    }

    Ok(())
}

fn next_progress_writer_frame(shared: &ProgressWriterShared) -> Option<ProgressWriterFrame> {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        if let Some(terminal) = state.terminal.take() {
            if state
                .progress
                .front()
                .is_some_and(|frame| frame.deliver_before_terminal)
            {
                let progress = state
                    .progress
                    .pop_front()
                    .expect("front progress frame should exist");
                state.terminal = Some(terminal);
                return Some(ProgressWriterFrame::Progress(progress.bytes));
            }
            state.progress.clear();
            state.closed = true;
            return Some(ProgressWriterFrame::Terminal(terminal));
        }

        if let Some(progress) = state.progress.pop_front() {
            return Some(ProgressWriterFrame::Progress(progress.bytes));
        }

        if state.closed {
            return None;
        }

        state = shared
            .available
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

fn peer_disconnected_during_response(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    )
}

fn dispatch_error_message(error: &DispatchError) -> String {
    error.to_string()
}

struct DaemonRuntime {
    listener: Option<UnixListener>,
    _pid_lock: File,
    pid_file: PathBuf,
    socket_path: PathBuf,
    socket_cleanup_state: SocketCleanupState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketCleanupState {
    Unbound,
    Bound,
    Cleaned,
}

impl DaemonRuntime {
    fn claim(socket_path: &Path, pid_file: &Path) -> Result<Self, DaemonError> {
        if let Some(parent) = socket_path.parent() {
            prepare_runtime_directory(parent)?;
        }
        if let Some(parent) = pid_file.parent() {
            if socket_path.parent() != Some(parent) {
                prepare_runtime_directory(parent)?;
            }
        }

        let mut pid_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(pid_file)?;

        if !try_lock_exclusive(&pid_lock)? {
            return Err(DaemonError::AlreadyRunning {
                pid: read_pid(pid_file),
            });
        }

        pid_lock.set_len(0)?;
        write!(&mut pid_lock, "{}", std::process::id())?;
        pid_lock.sync_data()?;

        prepare_socket_path(socket_path)?;

        Ok(Self {
            listener: None,
            _pid_lock: pid_lock,
            pid_file: pid_file.to_path_buf(),
            socket_path: socket_path.to_path_buf(),
            socket_cleanup_state: SocketCleanupState::Unbound,
        })
    }

    fn bind_listener(&mut self) -> Result<(), io::Error> {
        let listener = UnixListener::bind(&self.socket_path)?;
        self.socket_cleanup_state = SocketCleanupState::Bound;
        restrict_file_permissions(&self.socket_path, SOCKET_MODE)?;
        set_listener_receive_timeout(&listener, ACCEPT_TIMEOUT)?;
        self.listener = Some(listener);
        Ok(())
    }

    fn accept(&self) -> Result<(UnixStream, std::os::unix::net::SocketAddr), io::Error> {
        self.listener
            .as_ref()
            .expect("listener should exist while the daemon is accepting connections")
            .accept()
    }

    fn begin_shutdown(&mut self) -> Result<(), io::Error> {
        self.listener.take();
        if self.socket_cleanup_state != SocketCleanupState::Bound {
            return Ok(());
        }

        remove_socket_file_if_present(&self.socket_path)?;
        self.socket_cleanup_state = SocketCleanupState::Cleaned;
        Ok(())
    }
}

impl Drop for DaemonRuntime {
    fn drop(&mut self) {
        if self.socket_cleanup_state == SocketCleanupState::Bound {
            let _ = remove_socket_file_if_present(&self.socket_path);
        }
        let _ = fs::remove_file(&self.pid_file);
    }
}

fn prepare_runtime_directory(path: &Path) -> Result<(), DaemonError> {
    let created = ensure_directory_exists(path)?;
    if created {
        restrict_directory_permissions(path, RUNTIME_DIR_MODE)?;
    }

    Ok(())
}

fn prepare_socket_path(socket_path: &Path) -> Result<(), DaemonError> {
    match fs::symlink_metadata(socket_path) {
        Ok(metadata) => {
            if metadata.file_type().is_socket() {
                fs::remove_file(socket_path)?;
                Ok(())
            } else {
                Err(DaemonError::Io(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "socket_path exists but is not a Unix socket: {}",
                        socket_path.display()
                    ),
                )))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DaemonError::Io(error)),
    }
}

fn remove_socket_file_if_present(socket_path: &Path) -> Result<(), io::Error> {
    match fs::symlink_metadata(socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(socket_path),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn ensure_directory_exists(path: &Path) -> Result<bool, io::Error> {
    let existed = path.exists();
    fs::create_dir_all(path)?;
    Ok(!existed)
}

fn restrict_directory_permissions(path: &Path, mode: u32) -> Result<(), io::Error> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn restrict_file_permissions(path: &Path, mode: u32) -> Result<(), io::Error> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn set_listener_receive_timeout(
    listener: &UnixListener,
    timeout: Duration,
) -> Result<(), io::Error> {
    let timeout = libc::timeval {
        tv_sec: timeout.as_secs().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "listener timeout too large")
        })?,
        tv_usec: i64::from(timeout.subsec_micros()),
    };

    let result = unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &timeout as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn try_lock_exclusive(file: &File) -> Result<bool, io::Error> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }

    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
        Ok(false)
    } else {
        Err(error)
    }
}

fn read_pid(pid_file: &Path) -> Option<u32> {
    fs::read_to_string(pid_file)
        .ok()
        .and_then(|contents| contents.trim().parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::{
        DaemonError, LiveObservationLevel, PROGRESS_QUEUE_CAPACITY,
        PROGRESS_TERMINAL_DRAIN_TIMEOUT, ProgressWriter, ResponseMessage, reap_finished_handlers,
        run_daemon_until_shutdown_with_reconciler, write_response,
    };
    use crate::config::Config;
    use crate::dispatch::SessionExecutor;
    use crate::protocol::ProgressMessage;
    use agentd_runner::{
        RunnerError, SessionInvocation, SessionOutcome, SessionProgressObserver, SessionSpec,
        StartupReconciliationReport,
    };
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::mpsc::Sender;
    use std::thread;
    use std::time::{Duration, Instant};
    use std::{
        os::fd::{AsRawFd, RawFd},
        os::unix::fs::FileTypeExt,
        os::unix::net::{UnixListener, UnixStream},
    };

    #[test]
    fn response_message_deserializes_blocked_outcome_payloads() {
        let response: ResponseMessage = serde_json::from_str(
            r#"{"type":"session_outcome","outcome":{"status":"blocked","exit_code":3}}"#,
        )
        .expect("blocked outcome payload should deserialize");

        match response {
            ResponseMessage::SessionOutcome { outcome } => {
                assert_eq!(
                    SessionOutcome::from(outcome),
                    SessionOutcome::Blocked { exit_code: 3 }
                );
            }
            other => panic!("expected session outcome response, got {other:?}"),
        }
    }

    #[derive(Clone)]
    struct FixedOutcomeExecutor;

    impl SessionExecutor for FixedOutcomeExecutor {
        fn run_session(
            &self,
            _spec: SessionSpec,
            _invocation: SessionInvocation,
            _progress: &dyn SessionProgressObserver,
        ) -> Result<SessionOutcome, RunnerError> {
            Ok(SessionOutcome::Success { exit_code: 0 })
        }
    }

    fn unique_runtime_dir(name: &str) -> PathBuf {
        let unique = format!(
            "agentd-daemon-unit-test-{name}-{}-{}",
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

    fn set_socket_buffer(stream: &UnixStream, option_name: libc::c_int, size: libc::c_int) {
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                option_name,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as libc::socklen_t,
            )
        };
        assert_eq!(
            result,
            0,
            "failed to constrain socket buffer: {}",
            io::Error::last_os_error()
        );
    }

    fn unread_socket_bytes(stream: &UnixStream) -> usize {
        let mut bytes: libc::c_int = 0;
        let result = unsafe { libc::ioctl(stream.as_raw_fd(), libc::FIONREAD, &mut bytes) };
        assert_eq!(
            result,
            0,
            "failed to inspect unread socket bytes: {}",
            io::Error::last_os_error()
        );
        usize::try_from(bytes).expect("unread byte count should be non-negative")
    }

    fn wait_for_unread_socket_bytes(stream: &UnixStream, minimum_unread_bytes: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if unread_socket_bytes(stream) >= minimum_unread_bytes {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }

        panic!(
            "timed out waiting for {minimum_unread_bytes} unread socket bytes; last observed {}",
            unread_socket_bytes(stream)
        );
    }

    fn fd_is_open(fd: RawFd) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    /// A blocked handler that probes the join ordering directly: when it
    /// wakes, it reports whether `shutdown_daemon` had already returned. When
    /// the join is in place the handler wakes strictly before the join can
    /// return, so the probe deterministically observes `false`; a
    /// `shutdown_daemon` that returns without joining is observed as `true`.
    /// The report doubles as the handler's completion signal.
    fn spawn_ordering_probe_handler(
        shutdown_returned: Arc<AtomicBool>,
    ) -> (thread::JoinHandle<()>, Sender<()>, mpsc::Receiver<bool>) {
        let (release_tx, release_rx) = mpsc::channel();
        let (report_tx, report_rx) = mpsc::channel();
        let handler = thread::spawn(move || {
            release_rx
                .recv()
                .expect("blocked handler should be released");
            report_tx
                .send(shutdown_returned.load(Ordering::SeqCst))
                .expect("ordering probe report should be received");
        });
        (handler, release_tx, report_rx)
    }

    #[test]
    fn reaping_finished_handlers_keeps_only_live_threads() {
        let finished = thread::spawn(|| {});
        finished
            .join()
            .expect("finished thread should join cleanly");

        let (tx, rx) = mpsc::channel();
        let blocked = thread::spawn(move || {
            rx.recv().expect("blocked thread should be released");
        });

        let mut handlers = vec![thread::spawn(|| {}), blocked];
        thread::sleep(Duration::from_millis(50));

        reap_finished_handlers(&mut handlers);

        assert_eq!(handlers.len(), 1, "only the live handler should remain");
        tx.send(()).expect("blocked thread should be released");
        handlers
            .pop()
            .expect("live handler should remain")
            .join()
            .expect("blocked thread should join cleanly");
    }

    #[test]
    fn reaping_finished_panicked_handlers_does_not_panic() {
        let mut handlers = vec![thread::spawn(|| panic!("expected test panic"))];
        thread::sleep(Duration::from_millis(50));

        reap_finished_handlers(&mut handlers);

        assert!(
            handlers.is_empty(),
            "finished panicked handlers should be reaped"
        );
    }

    #[test]
    fn response_writes_ignore_a_peer_that_already_disconnected() {
        let (mut daemon_stream, client_stream) =
            UnixStream::pair().expect("stream pair should be created");
        drop(client_stream);

        let result = write_response(
            &mut daemon_stream,
            &ResponseMessage::Error {
                message: "ignored disconnect".to_string(),
            },
        );

        assert!(
            result.is_ok(),
            "closed peer during response write should be treated as normal completion"
        );
    }

    #[test]
    fn progress_writer_finish_timeout_forces_stalled_writer_closed() {
        let (daemon_stream, client_stream) =
            UnixStream::pair().expect("stream pair should be created");
        set_socket_buffer(&daemon_stream, libc::SO_SNDBUF, 4096);
        set_socket_buffer(&client_stream, libc::SO_RCVBUF, 4096);
        let daemon_fd = daemon_stream.as_raw_fd();
        let writer = ProgressWriter::spawn(daemon_stream).expect("progress writer should spawn");
        let sink = writer.sink();
        let progress_line = format!(
            r#"{{"schema_version":1,"source":"runa","kind":"agent_output","content":"{}"}}"#,
            "x".repeat(96 * 1024)
        );
        for index in 0..PROGRESS_QUEUE_CAPACITY {
            sink.enqueue_progress(ResponseMessage::Progress {
                progress: ProgressMessage::TranscriptEvent {
                    session_id: format!("stalled-session-{index}"),
                    line: progress_line.clone(),
                },
            });
        }
        drop(sink);
        wait_for_unread_socket_bytes(&client_stream, 4 * 1024);

        let started = Instant::now();
        writer
            .finish(ResponseMessage::SessionOutcome {
                outcome: SessionOutcome::Success { exit_code: 0 }.into(),
            })
            .expect("timeout cleanup should not fail daemon completion");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= PROGRESS_TERMINAL_DRAIN_TIMEOUT,
            "test must exercise the terminal drain timeout path, completed in {elapsed:?}"
        );
        assert!(
            elapsed < PROGRESS_TERMINAL_DRAIN_TIMEOUT + Duration::from_secs(5),
            "terminal drain timeout cleanup should remain bounded, took {elapsed:?}"
        );
        assert!(
            !fd_is_open(daemon_fd),
            "finish must not return while a stalled progress writer still owns the stream fd"
        );
        drop(client_stream);
    }

    fn render_transcript_line(line: &str, level: LiveObservationLevel) -> String {
        let mut output = Vec::new();

        super::render_progress(
            ProgressMessage::TranscriptEvent {
                session_id: "fake-session-1".to_string(),
                line: line.to_string(),
            },
            level,
            &mut output,
        )
        .expect("transcript progress should render");

        String::from_utf8(output).expect("transcript progress should be utf8")
    }

    #[test]
    fn summary_progress_renders_followable_transcript_activity() {
        let cases = [
            (
                r#"{"schema_version":1,"source":"runa","kind":"agent_input","content":"build the release checklist"}"#,
                ["runa/agent_input", "build the release checklist"].as_slice(),
            ),
            (
                r#"{"schema_version":1,"source":"runa","kind":"agent_output","content":"reading repository state"}"#,
                ["runa/agent_output", "reading repository state"].as_slice(),
            ),
            (
                r#"{"schema_version":1,"source":"runa","kind":"agent_stderr","message":"warning: retrying tool call"}"#,
                ["runa/agent_stderr", "warning: retrying tool call"].as_slice(),
            ),
            (
                r#"{"schema_version":1,"source":"runa-mcp","kind":"tool_call","protocol":"mcp","action":"tools/call","tool":"shell"}"#,
                [
                    "runa-mcp/tool_call",
                    "protocol=mcp",
                    "action=tools/call",
                    "tool=shell",
                ]
                .as_slice(),
            ),
            (
                r#"{"schema_version":1,"source":"runa","kind":"agent_exit","success":true,"exit_code":0}"#,
                ["runa/agent_exit", "success=true", "exit_code=0"].as_slice(),
            ),
        ];

        for (line, expected_parts) in cases {
            let output = render_transcript_line(line, LiveObservationLevel::Summary);
            for expected in expected_parts {
                assert!(
                    output.contains(expected),
                    "summary output should contain {expected:?}: {output:?}"
                );
            }
        }
    }

    #[test]
    fn every_observation_level_renders_followable_transcript_content() {
        let line = r#"{"schema_version":1,"source":"runa","kind":"agent_output","content":"useful live payload"}"#;

        for level in [LiveObservationLevel::Summary, LiveObservationLevel::Full] {
            let output = render_transcript_line(line, level);
            assert!(
                output.contains("useful live payload"),
                "{level:?} output should include transcript payload: {output:?}"
            );
        }
    }

    #[test]
    fn full_progress_includes_session_context_and_raw_transcript_detail() {
        let output = render_transcript_line(
            r#"{"schema_version":1,"source":"runa-mcp","kind":"tool_call","protocol":"mcp","action":"tools/call","success":true,"content":"inspect"}"#,
            LiveObservationLevel::Full,
        );

        for expected in [
            "session_id=fake-session-1",
            r#""source":"runa-mcp""#,
            r#""kind":"tool_call""#,
            r#""protocol":"mcp""#,
            r#""action":"tools/call""#,
            r#""success":true"#,
            r#""content":"inspect""#,
        ] {
            assert!(
                output.contains(expected),
                "full output should contain raw detail {expected:?}: {output:?}"
            );
        }
    }

    #[test]
    fn transcript_fallbacks_remain_observable_at_every_level() {
        let cases = [
            (
                "not json\nwith controls\u{1b}[2J",
                ["unparsed_event", "not json\\nwith controls\\u{1b}[2J"].as_slice(),
                ["not json\\nwith controls\\u{1b}[2J"].as_slice(),
            ),
            (
                r#"{"schema_version":1,"source":"runa","kind":"heartbeat","sequence":7,"success":false}"#,
                ["runa/heartbeat", "sequence=7", "success=false"].as_slice(),
                [
                    r#""kind":"heartbeat""#,
                    r#""sequence":7"#,
                    r#""success":false"#,
                ]
                .as_slice(),
            ),
            (
                r#"{"schema_version":1,"kind":"custom_event","detail":"field summary"}"#,
                ["custom_event", "detail=field summary"].as_slice(),
                [r#""kind":"custom_event""#, r#""detail":"field summary""#].as_slice(),
            ),
        ];

        for (line, summary_expected_parts, full_expected_parts) in cases {
            let summary = render_transcript_line(line, LiveObservationLevel::Summary);
            assert!(
                summary.starts_with("session event: "),
                "summary output should render a progress line: {summary:?}"
            );
            for expected in summary_expected_parts {
                assert!(
                    summary.contains(expected),
                    "summary output should contain fallback detail {expected:?}: {summary:?}"
                );
            }
            assert!(
                !summary[..summary.len() - 1].contains('\n') && !summary.contains('\u{1b}'),
                "summary output should escape fallback controls: {summary:?}"
            );

            let full = render_transcript_line(line, LiveObservationLevel::Full);
            assert!(
                full.starts_with("session event: session_id=fake-session-1 "),
                "full output should include session context: {full:?}"
            );
            for expected in full_expected_parts {
                assert!(
                    full.contains(expected),
                    "full output should contain raw detail {expected:?}: {full:?}"
                );
            }
            assert!(
                !full[..full.len() - 1].contains('\n') && !full.contains('\u{1b}'),
                "full output should escape fallback controls: {full:?}"
            );
        }
    }

    #[test]
    fn summary_progress_escapes_untrusted_display_fields_and_bounds_payloads() {
        let output = render_transcript_line(
            &format!(
                r#"{{"source":"runa","kind":"agent_input\nspoof\u001b[2J","content":"{}"}}"#,
                "x".repeat(600)
            ),
            LiveObservationLevel::Summary,
        );

        assert!(
            !output[..output.len() - 1].contains('\n') && !output.contains('\u{1b}'),
            "summary output should not contain embedded terminal controls: {output:?}"
        );
        assert!(
            output.len() < 360,
            "summary output should keep long payload previews bounded: {} bytes",
            output.len()
        );
        assert!(
            output.contains("agent_input\\nspoof\\u{1b}[2J") && output.contains("xxx"),
            "summary output should retain escaped role and payload preview: {output:?}"
        );
    }

    #[test]
    fn full_progress_escapes_untrusted_raw_transcript_line_controls() {
        let mut output = Vec::new();

        super::render_progress(
            ProgressMessage::TranscriptEvent {
                session_id: "fake-session-1\rspoof".to_string(),
                line: "{\"kind\":\"agent_input\"}\nspoof\u{1b}[2J".to_string(),
            },
            LiveObservationLevel::Full,
            &mut output,
        )
        .expect("full progress should render");

        let output = String::from_utf8(output).expect("full progress should be utf8");
        assert_eq!(
            output,
            "session event: session_id=fake-session-1\\rspoof {\"kind\":\"agent_input\"}\\nspoof\\u{1b}[2J\n"
        );
        assert!(
            !output[..output.len() - 1].contains('\n') && !output.contains('\u{1b}'),
            "full output should not contain embedded terminal controls: {output:?}"
        );
    }

    #[test]
    fn dispatch_progress_escapes_untrusted_request_fields_at_every_level() {
        for level in [LiveObservationLevel::Summary, LiveObservationLevel::Full] {
            let mut output = Vec::new();

            super::render_progress(
                ProgressMessage::DispatchStarted {
                    agent: "site-builder\nspoof\u{1b}[2J".to_string(),
                    work_unit: Some("issue-122\rrewrite".to_string()),
                    input_present: true,
                },
                level,
                &mut output,
            )
            .expect("dispatch progress should render");

            let output = String::from_utf8(output).expect("dispatch progress should be utf8");
            assert!(
                output.contains("site-builder\\nspoof\\u{1b}[2J")
                    && output.contains("issue-122\\rrewrite"),
                "dispatch output should escape request fields: {output:?}"
            );
            assert!(
                !output[..output.len() - 1].contains('\n') && !output.contains('\u{1b}'),
                "dispatch output should not contain embedded terminal controls: {output:?}"
            );
        }
    }

    #[test]
    fn finishing_after_accept_error_waits_for_in_flight_handlers() {
        let shutdown_returned = Arc::new(AtomicBool::new(false));
        let (handler, release_tx, report_rx) =
            spawn_ordering_probe_handler(Arc::clone(&shutdown_returned));
        // The handler is released only after `shutdown_daemon` has begun its
        // cleanup (`begin_shutdown` runs before handlers are joined), so the
        // handler is genuinely in flight when the join starts, regardless of
        // scheduling.
        let (cleanup_tx, cleanup_rx) = mpsc::channel();
        let releaser = thread::spawn(move || {
            cleanup_rx
                .recv()
                .expect("shutdown cleanup should signal before handlers are joined");
            release_tx
                .send(())
                .expect("blocked handler should be released");
        });

        let error = super::shutdown_daemon(
            Arc::new(AtomicBool::new(false)).as_ref(),
            move || {
                cleanup_tx
                    .send(())
                    .expect("release helper should be waiting for the cleanup signal");
                Ok(())
            },
            vec![handler],
            None,
            Err(io::Error::other("accept failed")),
        )
        .expect_err("accept error should be returned");
        shutdown_returned.store(true, Ordering::SeqCst);

        releaser
            .join()
            .expect("release helper thread should join cleanly");
        // Happens-before verdict: with the join in place the handler's probe
        // runs strictly before `shutdown_daemon` can return, so it observes
        // `false`; a return that skipped the join is observed as `true`.
        // Receiving the report also proves the handler ran to completion.
        let returned_before_handler = report_rx
            .recv()
            .expect("in-flight handler should complete and report");
        assert!(
            !returned_before_handler,
            "shutdown_daemon returned while its in-flight handler was still blocked"
        );
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "accept failed");
    }

    #[test]
    fn finishing_after_shutdown_error_still_joins_handlers() {
        let shutdown_returned = Arc::new(AtomicBool::new(false));
        let (handler, release_tx, report_rx) =
            spawn_ordering_probe_handler(Arc::clone(&shutdown_returned));
        // The handler is released only after the failing cleanup has run, so
        // the join that follows a cleanup error is exercised against a handler
        // that is still in flight, regardless of scheduling.
        let (cleanup_tx, cleanup_rx) = mpsc::channel();
        let releaser = thread::spawn(move || {
            cleanup_rx
                .recv()
                .expect("shutdown cleanup should signal before handlers are joined");
            release_tx
                .send(())
                .expect("blocked handler should be released");
        });

        let error = super::shutdown_daemon(
            Arc::new(AtomicBool::new(false)).as_ref(),
            move || {
                cleanup_tx
                    .send(())
                    .expect("release helper should be waiting for the cleanup signal");
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cleanup failed",
                ))
            },
            vec![handler],
            None,
            Ok(()),
        )
        .expect_err("cleanup failure should be returned");
        shutdown_returned.store(true, Ordering::SeqCst);

        releaser
            .join()
            .expect("release helper thread should join cleanly");
        // Happens-before verdict: even on the cleanup-error path, the join
        // orders the handler's probe strictly before `shutdown_daemon`'s
        // return, so it observes `false`; a return that skipped the join is
        // observed as `true`. Receiving the report proves the handler ran.
        let returned_before_handler = report_rx
            .recv()
            .expect("in-flight handler should complete and report");
        assert!(
            !returned_before_handler,
            "shutdown_daemon returned while its in-flight handler was still blocked"
        );
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "cleanup failed");
    }

    #[test]
    fn finishing_after_accept_error_prefers_the_accept_error_over_cleanup_error() {
        let error = super::shutdown_daemon(
            Arc::new(AtomicBool::new(false)).as_ref(),
            || {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cleanup failed",
                ))
            },
            Vec::new(),
            None,
            Err(io::Error::other("accept failed")),
        )
        .expect_err("accept error should win over cleanup error");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "accept failed");
    }

    #[test]
    fn shutting_down_sets_the_shutdown_flag_before_runtime_cleanup() {
        let shutdown = Arc::new(AtomicBool::new(false));

        let error = super::shutdown_daemon(
            shutdown.as_ref(),
            || {
                assert!(
                    shutdown.load(std::sync::atomic::Ordering::Acquire),
                    "shutdown should be asserted before runtime cleanup begins"
                );
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cleanup failed",
                ))
            },
            Vec::new(),
            None,
            Err(io::Error::other("accept failed")),
        )
        .expect_err("accept error should still be returned");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "accept failed");
    }

    #[test]
    fn shutting_down_sets_shutdown_before_joining_the_scheduler() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let scheduler_shutdown = Arc::clone(&shutdown);
        let scheduler = thread::spawn(move || {
            while !scheduler_shutdown.load(std::sync::atomic::Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
        });
        let (done_tx, done_rx) = mpsc::channel();
        let join_shutdown = Arc::clone(&shutdown);
        let joiner = thread::spawn(move || {
            let error = super::shutdown_daemon(
                join_shutdown.as_ref(),
                || Ok(()),
                Vec::new(),
                Some(scheduler),
                Err(io::Error::other("accept failed")),
            )
            .expect_err("accept error should still be returned");
            done_tx
                .send(error.to_string())
                .expect("unified shutdown should report completion");
        });

        let error = done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("unified shutdown should assert shutdown before joining scheduler");
        joiner.join().expect("unified shutdown should join cleanly");

        assert_eq!(error, "accept failed");
        assert!(
            shutdown.load(std::sync::atomic::Ordering::Acquire),
            "unified shutdown should leave shutdown asserted"
        );
    }

    #[test]
    fn startup_reconciliation_completes_before_socket_binding() {
        let runtime_dir = unique_runtime_dir("startup-order");
        let config = config_in_runtime_dir(&runtime_dir);
        let shutdown = Arc::new(AtomicBool::new(false));
        let daemon_config = config.clone();
        let daemon_shutdown = shutdown.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (daemon_id_tx, daemon_id_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let expected_daemon_instance_id = config
            .daemon()
            .daemon_instance_id()
            .expect("daemon instance id should resolve");

        let handle = thread::spawn(move || {
            run_daemon_until_shutdown_with_reconciler(
                daemon_config,
                FixedOutcomeExecutor,
                daemon_shutdown,
                move || {
                    daemon_id_tx
                        .send(expected_daemon_instance_id)
                        .expect("reconciliation daemon id should be reported");
                    started_tx
                        .send(())
                        .expect("reconciliation start should be reported");
                    release_rx
                        .recv()
                        .expect("test should release reconciliation");
                    Ok(StartupReconciliationReport::default())
                },
            )
        });

        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("startup reconciliation should start");
        assert_eq!(
            daemon_id_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("reconciliation daemon id should be available"),
            config
                .daemon()
                .daemon_instance_id()
                .expect("daemon instance id should resolve")
        );
        assert!(
            !config.daemon().socket_path().exists(),
            "socket should not exist while startup reconciliation is still running"
        );

        release_tx
            .send(())
            .expect("reconciliation should be released");
        wait_for_path(config.daemon().socket_path());

        shutdown.store(true, std::sync::atomic::Ordering::Release);
        handle
            .join()
            .expect("daemon thread should join")
            .expect("daemon should exit cleanly");
    }

    #[test]
    fn startup_reconciliation_failure_aborts_daemon_before_socket_binding() {
        let runtime_dir = unique_runtime_dir("startup-failure");
        let config = config_in_runtime_dir(&runtime_dir);

        let error = run_daemon_until_shutdown_with_reconciler(
            config.clone(),
            FixedOutcomeExecutor,
            Arc::new(AtomicBool::new(false)),
            || Err(RunnerError::InvalidBaseImage),
        )
        .expect_err("startup reconciliation failure should abort daemon startup");

        match error {
            DaemonError::StartupReconciliation(inner) => {
                assert!(matches!(inner, RunnerError::InvalidBaseImage));
            }
            other => panic!("expected startup reconciliation error, got {other:?}"),
        }

        assert!(
            !config.daemon().socket_path().exists(),
            "socket should not be created when startup reconciliation fails"
        );
    }

    #[test]
    fn dropping_claimed_but_unbound_runtime_does_not_remove_socket_it_does_not_own() {
        let runtime_dir = unique_runtime_dir("drop-unbound-runtime");
        let config = config_in_runtime_dir(&runtime_dir);
        let socket_path = config.daemon().socket_path();
        let pid_file = config.daemon().pid_file();

        let runtime = super::DaemonRuntime::claim(socket_path, pid_file)
            .expect("daemon runtime should claim pid file and prepare socket path");
        let _foreign_listener = UnixListener::bind(socket_path)
            .expect("test should bind a foreign listener after claim");

        drop(runtime);

        assert!(
            !pid_file.exists(),
            "dropping the runtime should still clean up the pid file"
        );

        let socket_metadata =
            fs::symlink_metadata(socket_path).expect("foreign listener socket should remain");
        assert!(
            socket_metadata.file_type().is_socket(),
            "foreign listener socket should still be present after dropping the unbound runtime"
        );
    }
}
