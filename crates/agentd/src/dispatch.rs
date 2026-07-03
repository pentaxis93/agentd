use std::fmt;

use agentd_runner::{
    InvocationInput, NoopSessionProgressObserver, ResolvedEnvironmentVariable, RunnerError,
    SessionInvocation, SessionOutcome, SessionProgressObserver, SessionSpec,
    run_session_with_progress,
};

use crate::audit_root::prepare_audit_root;
use crate::config::{Config, ConfigError};

/// Parameters for a daemon run request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub agent: String,
    pub repo_url: Option<String>,
    /// Optional manual work-unit target. Mutually exclusive with `input`.
    pub work_unit: Option<String>,
    /// Optional manual invocation input. Mutually exclusive with `work_unit`.
    pub input: Option<InvocationInput>,
}

/// Errors produced while mapping a run request into a runner session.
#[derive(Debug)]
pub enum DispatchError {
    UnknownAgent {
        agent: String,
    },
    MissingRepo {
        agent: String,
    },
    MissingCredentialSource {
        agent: String,
        credential: String,
        source: String,
    },
    Config(ConfigError),
    Runner(RunnerError),
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownAgent { agent } => write!(f, "unknown agent '{agent}'"),
            Self::MissingRepo { agent } => write!(
                f,
                "agent '{agent}' requires a repo argument or configured agent repo"
            ),
            Self::MissingCredentialSource {
                agent,
                credential,
                source,
            } => write!(
                f,
                "agent '{agent}' credential '{credential}' requires daemon environment variable '{source}'"
            ),
            Self::Config(error) => write!(f, "{error}"),
            Self::Runner(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for DispatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Runner(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ConfigError> for DispatchError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<RunnerError> for DispatchError {
    fn from(error: RunnerError) -> Self {
        Self::Runner(error)
    }
}

/// Trait boundary used by daemon dispatch so tests can replace the runner.
pub trait SessionExecutor {
    fn run_session(
        &self,
        spec: SessionSpec,
        invocation: SessionInvocation,
        progress: &dyn SessionProgressObserver,
    ) -> Result<SessionOutcome, RunnerError>;
}

/// Production executor that forwards directly into `agentd-runner`.
#[derive(Clone, Copy, Debug, Default)]
pub struct RunnerSessionExecutor;

impl SessionExecutor for RunnerSessionExecutor {
    fn run_session(
        &self,
        spec: SessionSpec,
        invocation: SessionInvocation,
        progress: &dyn SessionProgressObserver,
    ) -> Result<SessionOutcome, RunnerError> {
        run_session_with_progress(spec, invocation, progress)
    }
}

/// Resolve a named agent plus run request into a runner session and run it.
pub fn dispatch_run(
    config: &Config,
    request: &RunRequest,
    executor: &impl SessionExecutor,
) -> Result<SessionOutcome, DispatchError> {
    dispatch_run_after_preflight(
        config,
        request,
        executor,
        || {},
        &NoopSessionProgressObserver,
    )
}

/// Resolve a named agent plus run request into a runner session, notify once
/// preflight has passed, and run it.
pub fn dispatch_run_after_preflight(
    config: &Config,
    request: &RunRequest,
    executor: &impl SessionExecutor,
    after_preflight: impl FnOnce(),
    progress: &dyn SessionProgressObserver,
) -> Result<SessionOutcome, DispatchError> {
    let agent = config
        .agent(&request.agent)
        .ok_or_else(|| DispatchError::UnknownAgent {
            agent: request.agent.clone(),
        })?;
    let repo_url = request
        .repo_url
        .clone()
        .or_else(|| agent.repo().map(str::to_owned));
    let repo_url = repo_url.ok_or_else(|| DispatchError::MissingRepo {
        agent: agent.name().to_string(),
    })?;
    let daemon_instance_id = config.daemon().daemon_instance_id()?;
    let audit_root = prepare_audit_root(config.daemon())?;

    let environment = agent
        .credentials()
        .iter()
        .map(|credential| {
            let value = std::env::var(credential.source()).map_err(|_| {
                DispatchError::MissingCredentialSource {
                    agent: agent.name().to_string(),
                    credential: credential.name().to_string(),
                    source: credential.source().to_string(),
                }
            })?;

            Ok(ResolvedEnvironmentVariable {
                name: credential.name().to_string(),
                value,
            })
        })
        .collect::<Result<Vec<_>, DispatchError>>()?;

    let repo_token = agent
        .repo_token_source()
        .and_then(|source| std::env::var(source).ok())
        .filter(|value| !value.is_empty());

    after_preflight();

    executor
        .run_session(
            SessionSpec {
                daemon_instance_id,
                agent_name: agent.name().to_string(),
                base_image: agent.base_image().to_string(),
                methodology_dir: agent.methodology_dir().to_path_buf(),
                audit_root,
                forge_type: agent.forge_type().to_string(),
                mounts: agent
                    .mounts()
                    .iter()
                    .map(|mount| mount.to_runner_mount())
                    .collect(),
                agent_command: agent.agent_command().to_vec(),
                environment,
            },
            SessionInvocation {
                repo_url,
                repo_token,
                work_unit: request.work_unit.clone(),
                input: request.input.clone(),
                timeout: None,
            },
            progress,
        )
        .map_err(DispatchError::from)
}
