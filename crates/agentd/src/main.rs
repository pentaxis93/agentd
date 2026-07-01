use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::{error::Error, fmt};

use agentd::config::Config;
use agentd::{
    RunRequest, RunnerSessionExecutor, configure_tracing, request_run, resolve_client_socket_path,
    run_daemon_until_shutdown,
};
use agentd_runner::InvocationInput;
use clap::{Args, Parser, Subcommand};
use signal_hook::consts::signal::{SIGINT, SIGTERM};

const DEFAULT_CONFIG_PATH: &str = "/etc/agentd/agentd.toml";

#[derive(Debug)]
enum RunCommandError {
    Outcome(agentd_runner::SessionOutcome),
    ArtifactFileUnreadable {
        path: PathBuf,
        error: std::io::Error,
    },
    ArtifactFileNonUtf8 {
        path: PathBuf,
    },
    ArtifactFileInvalidJson {
        path: PathBuf,
        error: serde_json::Error,
    },
    ArtifactFileMissingStem {
        path: PathBuf,
    },
}

impl fmt::Display for RunCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Outcome(outcome) => match outcome {
                agentd_runner::SessionOutcome::TimedOut => write!(f, "session timed out"),
                agentd_runner::SessionOutcome::TerminatedBySignal { exit_code, signal } => write!(
                    f,
                    "session {} (exit code {exit_code}, signal {signal})",
                    outcome.label()
                ),
                _ => {
                    if let Some(exit_code) = outcome.exit_code() {
                        write!(f, "session {} (exit code {exit_code})", outcome.label())
                    } else {
                        write!(f, "session {}", outcome.label())
                    }
                }
            },
            Self::ArtifactFileUnreadable { path, error } => {
                write!(
                    f,
                    "failed to read artifact file {}: {error}",
                    path.display()
                )
            }
            Self::ArtifactFileNonUtf8 { path } => {
                write!(
                    f,
                    "artifact file must be valid UTF-8 JSON: {}",
                    path.display()
                )
            }
            Self::ArtifactFileInvalidJson { path, error } => {
                write!(
                    f,
                    "artifact file must contain valid JSON {}: {error}",
                    path.display()
                )
            }
            Self::ArtifactFileMissingStem { path } => {
                write!(
                    f,
                    "artifact file must have a non-empty UTF-8 file stem: {}",
                    path.display()
                )
            }
        }
    }
}

impl Error for RunCommandError {}

#[derive(Parser, Debug)]
#[command(name = "agentd", version, propagate_version = true)]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Args, Debug, Default)]
struct DaemonArgs {
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the foreground daemon.
    Daemon(DaemonArgs),
    /// Trigger a manual session through the running daemon.
    #[command(
        display_name = "agentd",
        after_help = "Work-mode artifact invocation:\n  agentd run <AGENT> [REPO] --work-unit <ID> --artifact-type work-unit --artifact-file <ID>.json"
    )]
    Run {
        agent: String,
        repo: Option<String>,
        #[arg(long)]
        socket_path: Option<PathBuf>,
        #[arg(long, conflicts_with = "intent")]
        work_unit: Option<String>,
        #[arg(long, conflicts_with_all = ["work_unit", "artifact_file"])]
        intent: Option<String>,
        #[arg(long, requires = "artifact_type", conflicts_with = "intent")]
        artifact_file: Option<PathBuf>,
        #[arg(long, requires = "artifact_file")]
        artifact_type: Option<String>,
    },
}

fn main() -> ExitCode {
    if let Err(error) = configure_tracing() {
        eprintln!("failed to initialize tracing: {error}");
        return ExitCode::FAILURE;
    }

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        None => run_daemon(Config::load(resolve_daemon_config_path(
            cli.config.as_deref(),
            None,
        )?)?),
        Some(Command::Daemon(daemon_args)) => run_daemon(Config::load(
            resolve_daemon_config_path(cli.config.as_deref(), daemon_args.config.as_deref())?,
        )?),
        Some(Command::Run {
            agent,
            repo,
            socket_path,
            work_unit,
            intent,
            artifact_file,
            artifact_type,
        }) => {
            if cli.config.is_some() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--config is only supported for daemon mode, not `agentd run`",
                )));
            }
            run_client(
                socket_path.as_deref(),
                agent,
                repo,
                work_unit,
                intent,
                artifact_file,
                artifact_type,
            )
        }
    }
}

fn resolve_daemon_config_path<'a>(
    root_config: Option<&'a std::path::Path>,
    daemon_config: Option<&'a std::path::Path>,
) -> Result<&'a std::path::Path, Box<dyn std::error::Error>> {
    match (root_config, daemon_config) {
        (Some(_), Some(_)) => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--config may be provided only once",
        ))),
        (Some(config), None) | (None, Some(config)) => Ok(config),
        (None, None) => Ok(std::path::Path::new(DEFAULT_CONFIG_PATH)),
    }
}

fn run_daemon(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    register_termination_handlers(shutdown.clone())?;

    let executor = RunnerSessionExecutor;
    run_daemon_until_shutdown(config, executor, shutdown)?;
    Ok(())
}

fn register_termination_handlers(shutdown: Arc<AtomicBool>) -> Result<(), std::io::Error> {
    for signal in [SIGINT, SIGTERM] {
        signal_hook::flag::register_conditional_shutdown(signal, 1, shutdown.clone())?;
        signal_hook::flag::register(signal, shutdown.clone())?;
    }

    Ok(())
}

fn run_client(
    explicit_socket_path: Option<&std::path::Path>,
    agent: String,
    repo: Option<String>,
    work_unit: Option<String>,
    intent: Option<String>,
    artifact_file: Option<PathBuf>,
    artifact_type: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = resolve_client_socket_path(explicit_socket_path)?;
    let input = resolve_invocation_input(intent, artifact_file, artifact_type)?;
    let outcome = request_run(
        &socket_path,
        &RunRequest {
            agent,
            repo_url: repo,
            work_unit,
            input,
        },
    )?;

    if outcome.is_cli_success() {
        println!("session {}", outcome.label());
        Ok(())
    } else {
        Err(Box::new(RunCommandError::Outcome(outcome)))
    }
}

fn resolve_invocation_input(
    intent: Option<String>,
    artifact_file: Option<PathBuf>,
    artifact_type: Option<String>,
) -> Result<Option<InvocationInput>, Box<dyn std::error::Error>> {
    if let Some(statement) = intent {
        return Ok(Some(InvocationInput::IntentText {
            statement,
            target: None,
        }));
    }

    let Some(path) = artifact_file else {
        return Ok(None);
    };
    let artifact_type = artifact_type.expect("clap should require artifact_type");
    let bytes = std::fs::read(&path).map_err(|error| {
        Box::new(RunCommandError::ArtifactFileUnreadable {
            path: path.clone(),
            error,
        }) as Box<dyn std::error::Error>
    })?;
    let contents = String::from_utf8(bytes).map_err(|_| {
        Box::new(RunCommandError::ArtifactFileNonUtf8 { path: path.clone() })
            as Box<dyn std::error::Error>
    })?;
    let document = serde_json::from_str(&contents).map_err(|error| {
        Box::new(RunCommandError::ArtifactFileInvalidJson {
            path: path.clone(),
            error,
        }) as Box<dyn std::error::Error>
    })?;
    let artifact_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .ok_or_else(|| {
            Box::new(RunCommandError::ArtifactFileMissingStem { path: path.clone() })
                as Box<dyn std::error::Error>
        })?;

    Ok(Some(InvocationInput::Artifact {
        artifact_type,
        artifact_id: artifact_id.to_string(),
        document,
    }))
}

#[cfg(test)]
mod tests {
    use super::register_termination_handlers;
    use std::io::Error;
    use std::ptr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn second_sigterm_exits_immediately_after_first_starts_shutdown() {
        unsafe {
            libc::alarm(10);
            match libc::fork() {
                -1 => panic!("fork failed: {}", Error::last_os_error()),
                0 => {
                    let shutdown = Arc::new(AtomicBool::new(false));
                    register_termination_handlers(Arc::clone(&shutdown))
                        .expect("termination handlers should register");

                    while !shutdown.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(10));
                    }

                    loop {
                        thread::sleep(Duration::from_secs(1));
                    }
                }
                pid => {
                    thread::sleep(Duration::from_millis(250));
                    assert_eq!(
                        0,
                        libc::kill(pid, libc::SIGTERM),
                        "first SIGTERM should send"
                    );
                    thread::sleep(Duration::from_millis(100));

                    let terminated = libc::waitpid(pid, ptr::null_mut(), libc::WNOHANG);
                    assert_eq!(
                        0, terminated,
                        "process should still be draining after the first SIGTERM"
                    );

                    assert_eq!(
                        0,
                        libc::kill(pid, libc::SIGTERM),
                        "second SIGTERM should send"
                    );
                    let terminated = libc::waitpid(pid, ptr::null_mut(), 0);
                    assert_eq!(pid, terminated, "process should exit on the second SIGTERM");
                }
            }
        }
    }
}
