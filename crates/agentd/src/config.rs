//! TOML configuration parsing and validation for the daemon and agent registry.
//!
//! Bridges operator-facing configuration to daemon dispatch and the runner's
//! [`SessionSpec`] model. Validation here is stricter than the runner's own
//! validation — it enforces uniqueness (no duplicate agent or credential
//! names), non-empty fields, active forge type defaulting, and whitespace hygiene
//! in addition to the runner's format and reservation rules. Relative daemon
//! runtime paths and `methodology_dir` are resolved against the configuration
//! file location when loaded from disk.
//!
//! [`SessionSpec`]: agentd_runner::SessionSpec

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use agentd_runner::{
    BindMount, MountTargetValidationError, RunnerError, validate_agent_name,
    validate_environment_name, validate_mount_overlap, validate_mount_target, validate_repo_url,
};
use croner::parser::{CronParser, Seconds, Year};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::runtime_paths::{RuntimePathError, default_daemon_runtime_paths};

const DEFAULT_FORGE_TYPE: &str = "github";

/// Validated daemon and agent registry parsed from a TOML configuration file.
///
/// Guarantees that daemon runtime paths are present, all agent names are
/// unique and valid, all required fields are non-empty, active forge type defaults
/// are applied, and all credential names are valid environment variable names.
/// Relative daemon runtime paths and `methodology_dir` paths are resolved
/// against the configuration file's parent directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    daemon: DaemonConfig,
    agents: Vec<Agent>,
}

impl Config {
    /// Reads and parses a TOML configuration file at `path`.
    ///
    /// Relative daemon runtime paths and `methodology_dir` values in the file
    /// are resolved against the directory containing `path`. Returns
    /// [`ConfigError`] on I/O failure, parse failure, or any validation
    /// violation.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        let base_dir = absolute_config_dir(path)?;
        Self::parse(&contents, base_dir.as_deref())
    }

    /// Returns all configured agents.
    pub fn agents(&self) -> &[Agent] {
        &self.agents
    }

    /// Returns the daemon-wide runtime paths.
    pub fn daemon(&self) -> &DaemonConfig {
        &self.daemon
    }

    /// Looks up an agent by name. Returns `None` if no agent matches.
    pub fn agent(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    fn parse(contents: &str, base_dir: Option<&Path>) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(contents)?;
        let raw_daemon = raw.daemon;

        if raw.agents.is_empty() {
            return Err(ConfigError::NoAgents);
        }

        let mut seen_agents = HashSet::new();
        let mut agents = Vec::with_capacity(raw.agents.len());

        for raw_agent in raw.agents {
            validate_lookup_key("name", &raw_agent.name, None, None)?;
            if validate_agent_name(&raw_agent.name).is_err() {
                return Err(ConfigError::InvalidAgentName {
                    name: raw_agent.name,
                });
            }

            if !seen_agents.insert(raw_agent.name.clone()) {
                return Err(ConfigError::DuplicateAgentName {
                    name: raw_agent.name,
                });
            }

            validate_non_empty(
                "base_image",
                &raw_agent.base_image,
                Some(raw_agent.name.as_str()),
                None,
            )?;
            validate_non_empty(
                "methodology_dir",
                &raw_agent.methodology_dir,
                Some(raw_agent.name.as_str()),
                None,
            )?;
            let repo = match raw_agent.repo {
                Some(value) => {
                    validate_lookup_key("repo", &value, Some(raw_agent.name.as_str()), None)?;
                    validate_repo_url(&value).map_err(|error| ConfigError::InvalidRepo {
                        agent: raw_agent.name.clone(),
                        message: error.to_string(),
                    })?;
                    Some(value)
                }
                None => None,
            };
            let schedule = match raw_agent.schedule {
                Some(value) => {
                    validate_lookup_key("schedule", &value, Some(raw_agent.name.as_str()), None)?;
                    validate_schedule(&value).map_err(|_| ConfigError::InvalidSchedule {
                        agent: raw_agent.name.clone(),
                        schedule: value.clone(),
                    })?;
                    Some(value)
                }
                None => None,
            };
            let repo_token_source = match raw_agent.repo_token_source {
                Some(value) if value.is_empty() => None,
                Some(value) => {
                    validate_lookup_key(
                        "repo_token_source",
                        &value,
                        Some(raw_agent.name.as_str()),
                        None,
                    )?;
                    Some(value)
                }
                None => None,
            };
            let forge_type = match raw_agent.forge_type {
                Some(value) => {
                    validate_lookup_key("forge_type", &value, Some(raw_agent.name.as_str()), None)?;
                    value
                }
                None => DEFAULT_FORGE_TYPE.to_string(),
            };

            let forge_owner = match raw_agent.forge_owner {
                Some(value) => {
                    validate_lookup_key(
                        "forge_owner",
                        &value,
                        Some(raw_agent.name.as_str()),
                        None,
                    )?;
                    Some(value)
                }
                None => None,
            };
            let forge_name = match raw_agent.forge_name {
                Some(value) => {
                    validate_lookup_key("forge_name", &value, Some(raw_agent.name.as_str()), None)?;
                    Some(value)
                }
                None => None,
            };
            if forge_owner.is_some() != forge_name.is_some() {
                return Err(ConfigError::PartialForgeIdentity {
                    agent: raw_agent.name.clone(),
                });
            }

            if raw_agent.command.argv.is_empty() {
                return Err(ConfigError::EmptyAgentCommand {
                    agent: raw_agent.name.clone(),
                });
            }

            let mut agent_command = Vec::with_capacity(raw_agent.command.argv.len());
            for element in raw_agent.command.argv {
                validate_non_empty(
                    "command.argv",
                    &element,
                    Some(raw_agent.name.as_str()),
                    None,
                )?;
                agent_command.push(element);
            }

            let mut seen_mount_targets = HashSet::new();
            let mut mounts = Vec::with_capacity(raw_agent.mounts.len());
            for raw_mount in raw_agent.mounts {
                validate_lookup_key(
                    "mounts.source",
                    &raw_mount.source,
                    Some(raw_agent.name.as_str()),
                    None,
                )?;
                validate_lookup_key(
                    "mounts.target",
                    &raw_mount.target,
                    Some(raw_agent.name.as_str()),
                    None,
                )?;

                let source = PathBuf::from(&raw_mount.source);
                if !source.is_absolute() {
                    return Err(ConfigError::MountSourceMustBeAbsolute {
                        agent: raw_agent.name.clone(),
                        source,
                    });
                }

                let target = PathBuf::from(&raw_mount.target);
                if let Err(error) = validate_mount_target(&target, &raw_agent.name) {
                    return Err(ConfigError::InvalidMountTarget {
                        agent: raw_agent.name.clone(),
                        error,
                    });
                }

                if !seen_mount_targets.insert(target.clone()) {
                    return Err(ConfigError::DuplicateMountTarget {
                        agent: raw_agent.name.clone(),
                        target,
                    });
                }

                mounts.push(AgentMountConfig {
                    source,
                    target,
                    read_only: raw_mount.read_only,
                });
            }
            let runner_mounts = mounts
                .iter()
                .map(AgentMountConfig::to_runner_mount)
                .collect::<Vec<_>>();
            if let Err(error) = validate_mount_overlap(&runner_mounts) {
                return Err(ConfigError::OverlappingMountTargets {
                    agent: raw_agent.name.clone(),
                    first: error.first,
                    second: error.second,
                });
            }

            let mut seen_credentials = HashSet::new();
            let mut credentials = Vec::with_capacity(raw_agent.credentials.len());
            for raw_credential in raw_agent.credentials {
                validate_lookup_key(
                    "credentials.name",
                    &raw_credential.name,
                    Some(raw_agent.name.as_str()),
                    None,
                )?;
                if validate_environment_name(&raw_credential.name).is_err() {
                    return Err(ConfigError::InvalidCredentialName {
                        agent: raw_agent.name.clone(),
                        name: raw_credential.name,
                    });
                }
                validate_non_empty(
                    "credentials.source",
                    &raw_credential.source,
                    Some(raw_agent.name.as_str()),
                    Some(raw_credential.name.as_str()),
                )?;

                if !seen_credentials.insert(raw_credential.name.clone()) {
                    return Err(ConfigError::DuplicateCredentialName {
                        agent: raw_agent.name.clone(),
                        name: raw_credential.name,
                    });
                }

                credentials.push(CredentialConfig {
                    name: raw_credential.name,
                    source: raw_credential.source,
                });
            }

            if schedule.is_some() && repo.is_none() {
                return Err(ConfigError::ScheduleRequiresRepo {
                    agent: raw_agent.name.clone(),
                });
            }

            let methodology_dir = resolve_methodology_dir(base_dir, &raw_agent.methodology_dir);
            agents.push(Agent {
                name: raw_agent.name,
                base_image: raw_agent.base_image,
                methodology_dir,
                mounts,
                repo,
                schedule,
                repo_token_source,
                forge_type,
                forge_owner,
                forge_name,
                credentials,
                agent_command,
            });
        }

        let daemon = DaemonConfig::parse(raw_daemon, base_dir)?;

        Ok(Self { daemon, agents })
    }
}

/// Parses a configuration from a TOML string without file-path context.
///
/// Unlike [`Config::load`], relative `methodology_dir` values are preserved
/// as-is because there is no file path to resolve them against.
impl FromStr for Config {
    type Err = ConfigError;

    fn from_str(contents: &str) -> Result<Self, Self::Err> {
        Self::parse(contents, None)
    }
}

/// Configuration for a single agent in the registry.
///
/// Fields are private; use the accessor methods to read values. The agent
/// name has been validated as a legal unix username and is unique within the
/// [`Config`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    name: String,
    base_image: String,
    methodology_dir: PathBuf,
    mounts: Vec<AgentMountConfig>,
    repo: Option<String>,
    schedule: Option<String>,
    repo_token_source: Option<String>,
    forge_type: String,
    forge_owner: Option<String>,
    forge_name: Option<String>,
    credentials: Vec<CredentialConfig>,
    agent_command: Vec<String>,
}

impl Agent {
    /// Agent identity string, validated as a legal unix username.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Container image reference for this agent's sessions.
    pub fn base_image(&self) -> &str {
        &self.base_image
    }

    /// Path to the methodology directory. Always absolute when constructed
    /// via [`Config::load`]; preserved as-is (potentially relative) when
    /// constructed via the [`FromStr`] impl.
    pub fn methodology_dir(&self) -> &Path {
        &self.methodology_dir
    }

    /// Additional bind mounts declared for this agent.
    pub fn mounts(&self) -> &[AgentMountConfig] {
        &self.mounts
    }

    /// Optional default repository URL for sessions launched from this agent.
    pub fn repo(&self) -> Option<&str> {
        self.repo.as_deref()
    }

    /// Optional five-field cron expression evaluated in daemon-local time.
    pub fn schedule(&self) -> Option<&str> {
        self.schedule.as_deref()
    }

    /// Optional environment variable name resolved by the daemon at dispatch
    /// time to authenticate the runner-managed `git clone` only. This value is
    /// not injected into the session runtime environment.
    pub fn repo_token_source(&self) -> Option<&str> {
        self.repo_token_source.as_deref()
    }

    /// Active forge type injected into each launched session for Groundwork
    /// forge-operation resolution.
    pub fn forge_type(&self) -> &str {
        &self.forge_type
    }

    /// Forge account owning the deployment's repository. Present exactly when
    /// [`Agent::forge_name`] is present; the pair is injected into each
    /// launched session so runa's entry can bind a ticket reference to the
    /// deployment's forge identity.
    pub fn forge_owner(&self) -> Option<&str> {
        self.forge_owner.as_deref()
    }

    /// Repository name of the deployment on its forge. Present exactly when
    /// [`Agent::forge_owner`] is present.
    pub fn forge_name(&self) -> Option<&str> {
        self.forge_name.as_deref()
    }

    /// Declared credentials for this agent. Each credential's name is a
    /// valid environment variable name, unique within this agent.
    pub fn credentials(&self) -> &[CredentialConfig] {
        &self.credentials
    }

    /// Agent command argv passed to runa for live execution.
    pub fn agent_command(&self) -> &[String] {
        &self.agent_command
    }
}

/// A validated agent-declared bind mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMountConfig {
    source: PathBuf,
    target: PathBuf,
    read_only: bool,
}

impl AgentMountConfig {
    /// Host-side source path for this mount.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Absolute in-container target path for this mount.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Whether the mount is read-only inside the container.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub(crate) fn to_runner_mount(&self) -> BindMount {
        BindMount {
            source: self.source.clone(),
            target: self.target.clone(),
            read_only: self.read_only,
        }
    }
}

/// A declared credential for an agent.
///
/// The `name` becomes the environment variable name inside the container.
/// The `source` is the name of an environment variable that the daemon reads
/// from its own process environment before passing the resolved value to the
/// runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialConfig {
    name: String,
    source: String,
}

impl CredentialConfig {
    /// Environment variable name for this credential inside the container.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Environment variable name that the daemon later resolves from its own
    /// process environment.
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// Daemon-wide paths for the operator socket and PID file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    socket_path: PathBuf,
    pid_file: PathBuf,
    audit_root: Option<PathBuf>,
}

impl DaemonConfig {
    /// Reads and parses only the daemon-wide runtime paths from a TOML
    /// configuration file at `path`.
    ///
    /// Relative daemon runtime paths are resolved against the directory
    /// containing `path`. The agent registry is ignored entirely, but other
    /// top-level sections must match the daemon config schema.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        let base_dir = absolute_config_dir(path)?;
        Self::parse_from_str(&contents, base_dir.as_deref())
    }

    /// Filesystem path for the daemon's local Unix socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Filesystem path for the daemon's PID file and advisory lock.
    pub fn pid_file(&self) -> &Path {
        &self.pid_file
    }

    /// Resolved host-side root for persistent session audit records.
    ///
    /// When `daemon.audit_root` is configured, that value is used. Otherwise
    /// agentd defaults to `$XDG_STATE_HOME/tesserine/audit`, falling back to
    /// `$HOME/.local/state/tesserine/audit` when `XDG_STATE_HOME` is unset.
    pub fn resolve_audit_root(&self) -> Result<PathBuf, ConfigError> {
        if let Some(path) = &self.audit_root {
            if path.is_absolute() {
                return Ok(path.clone());
            }

            return Err(ConfigError::RelativeDaemonAuditRootPath { path: path.clone() });
        }

        if let Some(path) = std::env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        {
            if path.is_absolute() {
                return Ok(path.join("tesserine/audit"));
            }
        }

        if let Some(path) = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        {
            if path.is_absolute() {
                return Ok(path.join(".local/state/tesserine/audit"));
            }
        }

        Err(ConfigError::MissingDaemonAuditRootDefault)
    }

    /// Stable daemon-instance identifier derived from the configured runtime
    /// path pair. Used to scope runner-managed Podman resource ownership to a
    /// single daemon instance.
    ///
    /// Returns an error when either runtime path is relative. [`Config::load`]
    /// resolves daemon runtime paths to absolute paths, but [`Config::from_str`]
    /// preserves relative values as-is because it has no file base.
    pub fn daemon_instance_id(&self) -> Result<String, ConfigError> {
        validate_absolute_daemon_runtime_path("daemon.socket_path", &self.socket_path)?;
        validate_absolute_daemon_runtime_path("daemon.pid_file", &self.pid_file)?;

        let mut hasher = Sha256::new();
        hasher.update(b"socket=");
        hasher.update(normalize_path_lexically(&self.socket_path).as_bytes());
        hasher.update(b"\npid=");
        hasher.update(normalize_path_lexically(&self.pid_file).as_bytes());
        hasher.update(b"\n");

        let digest = hasher.finalize();
        Ok(hex_encode(&digest[..4]))
    }

    fn parse(raw: RawDaemonConfig, base_dir: Option<&Path>) -> Result<Self, ConfigError> {
        let default_runtime_paths = if raw.socket_path.is_none() || raw.pid_file.is_none() {
            Some(default_daemon_runtime_paths().map_err(ConfigError::DefaultDaemonRuntimePaths)?)
        } else {
            None
        };

        let socket_path = match raw.socket_path {
            Some(path) => {
                validate_non_empty("daemon.socket_path", &path, None, None)?;
                resolve_config_path(base_dir, &path)
            }
            None => default_runtime_paths
                .as_ref()
                .expect("default runtime paths should be resolved when socket_path is omitted")
                .socket_path()
                .to_path_buf(),
        };

        let pid_file = match raw.pid_file {
            Some(path) => {
                validate_non_empty("daemon.pid_file", &path, None, None)?;
                resolve_config_path(base_dir, &path)
            }
            None => default_runtime_paths
                .as_ref()
                .expect("default runtime paths should be resolved when pid_file is omitted")
                .pid_file()
                .to_path_buf(),
        };

        Ok(Self {
            socket_path,
            pid_file,
            audit_root: raw
                .audit_root
                .as_deref()
                .map(|path| resolve_config_path(base_dir, path)),
        })
    }

    fn parse_from_str(contents: &str, base_dir: Option<&Path>) -> Result<Self, ConfigError> {
        let raw: RawDaemonOnlyConfig = toml::from_str(contents)?;
        Self::parse(raw.daemon, base_dir)
    }
}

impl AsRef<Path> for DaemonConfig {
    fn as_ref(&self) -> &Path {
        self.socket_path()
    }
}

/// Errors produced when loading or validating a configuration file.
#[derive(Debug)]
pub enum ConfigError {
    /// Failed to read the configuration file from disk.
    Io(std::io::Error),
    /// The file contains invalid TOML or violates the expected schema.
    Parse(toml::de::Error),
    /// The configuration defines zero agents. At least one must be declared.
    NoAgents,
    /// Two agents share the same name.
    DuplicateAgentName { name: String },
    /// An agent name fails the runner's [`validate_agent_name`] rules.
    InvalidAgentName { name: String },
    /// Two credentials within the same agent share a name.
    DuplicateCredentialName { agent: String, name: String },
    /// A credential name fails [`validate_environment_name`] (contains `,`
    /// or `=`, or uses a runner-owned environment name).
    InvalidCredentialName { agent: String, name: String },
    /// A required string field is empty or whitespace-only.
    EmptyField {
        field: &'static str,
        agent: Option<String>,
        credential: Option<String>,
    },
    /// A lookup-key field has leading or trailing whitespace. Caught
    /// separately from empty because the trimmed value may be valid — the
    /// whitespace itself is the error.
    FieldHasOuterWhitespace {
        field: &'static str,
        agent: Option<String>,
        credential: Option<String>,
    },
    /// Deriving the daemon instance id requires absolute daemon runtime paths.
    RelativeDaemonRuntimePath { field: &'static str, path: PathBuf },
    /// Resolving the daemon audit root requires an absolute configured path.
    RelativeDaemonAuditRootPath { path: PathBuf },
    /// No usable default audit-root environment was available.
    MissingDaemonAuditRootDefault,
    /// No usable default daemon runtime-path environment was available.
    DefaultDaemonRuntimePaths(RuntimePathError),
    /// The resolved daemon audit root could not be created or probed.
    AuditRootNotWritable {
        path: PathBuf,
        error: std::io::Error,
    },
    /// The `command` array is empty for an agent.
    EmptyAgentCommand { agent: String },
    /// An agent declares a default repository URL the runner would reject.
    InvalidRepo { agent: String, message: String },
    /// An agent declares an invalid cron expression.
    InvalidSchedule { agent: String, schedule: String },
    /// A scheduled agent must declare a default repo for autonomous runs.
    ScheduleRequiresRepo { agent: String },
    /// An agent declares exactly one of `forge_owner` / `forge_name`. The
    /// forge identity a session binds its ticket reference against is the
    /// owner/name pair; half of it resolves no reference.
    PartialForgeIdentity { agent: String },
    /// A configured mount source is not an absolute path.
    MountSourceMustBeAbsolute { agent: String, source: PathBuf },
    /// A configured mount target violates the runner's target rules.
    InvalidMountTarget {
        agent: String,
        error: MountTargetValidationError,
    },
    /// Two configured mounts in one agent share the same target path.
    DuplicateMountTarget { agent: String, target: PathBuf },
    /// Two configured mounts in one agent overlap by path components.
    OverlappingMountTargets {
        agent: String,
        first: PathBuf,
        second: PathBuf,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(error) => write!(f, "failed to read config: {error}"),
            ConfigError::Parse(error) => write!(f, "invalid config: {error}"),
            ConfigError::NoAgents => write!(f, "config must define at least one agent"),
            ConfigError::DuplicateAgentName { name } => {
                write!(f, "duplicate agent name: {name}")
            }
            ConfigError::InvalidAgentName { name } => {
                write!(
                    f,
                    "invalid agent name '{name}'; {}",
                    RunnerError::InvalidAgentName
                )
            }
            ConfigError::DuplicateCredentialName { agent, name } => {
                write!(
                    f,
                    "agent '{agent}' defines duplicate credential name '{name}'"
                )
            }
            ConfigError::InvalidCredentialName { agent, name } => write!(
                f,
                "agent '{agent}' defines invalid credential name '{name}'; credential names must not contain ',' or '=' and must not use runner-owned names such as AGENT_NAME, GROUNDWORK_FORGE_TYPE, AGENTD_WORK_UNIT, AGENTD_REPO_TOKEN, RUNA_TRANSCRIPT_DIR, RUNA_TRANSCRIPT_DEPLOYMENT, RUNA_TRANSCRIPT_RUN_ID, or RUNA_TRANSCRIPT_REDACT_ENV; transcript subsystem names are set internally"
            ),
            ConfigError::EmptyField {
                field,
                agent,
                credential,
            } => {
                write!(f, "field '{field}' must not be empty")?;

                if let Some(agent) = agent {
                    write!(f, " for agent '{agent}'")?;
                }
                if let Some(credential) = credential {
                    write!(f, " credential '{credential}'")?;
                }

                Ok(())
            }
            ConfigError::FieldHasOuterWhitespace {
                field,
                agent,
                credential,
            } => {
                write!(
                    f,
                    "field '{field}' must not have leading or trailing whitespace"
                )?;

                if let Some(agent) = agent {
                    write!(f, " for agent '{agent}'")?;
                }
                if let Some(credential) = credential {
                    write!(f, " credential '{credential}'")?;
                }

                Ok(())
            }
            ConfigError::RelativeDaemonRuntimePath { field, path } => {
                write!(
                    f,
                    "field '{field}' must be absolute before deriving daemon instance id: {}",
                    path.display()
                )
            }
            ConfigError::RelativeDaemonAuditRootPath { path } => {
                write!(
                    f,
                    "daemon audit root must be absolute before use: {}",
                    path.display()
                )
            }
            ConfigError::MissingDaemonAuditRootDefault => write!(
                f,
                "daemon audit root could not be resolved; set absolute XDG_STATE_HOME, set absolute HOME, or configure daemon.audit_root explicitly"
            ),
            ConfigError::DefaultDaemonRuntimePaths(error) => {
                write!(f, "daemon runtime paths could not be resolved; {error}")
            }
            ConfigError::AuditRootNotWritable { path, error } => {
                write!(
                    f,
                    "daemon audit root failed daemon-local create/chmod/remove probe: {} ({error}); deployment must run agentd with a UID-aligned identity, or equivalent chmod authority, over session-written audit files",
                    path.display()
                )
            }
            ConfigError::EmptyAgentCommand { agent } => {
                write!(f, "agent '{agent}' must define a non-empty command")
            }
            ConfigError::InvalidRepo { agent, message } => {
                write!(f, "agent '{agent}' defines invalid repo: {message}")
            }
            ConfigError::InvalidSchedule { agent, schedule } => {
                write!(f, "agent '{agent}' defines invalid schedule '{schedule}'")
            }
            ConfigError::ScheduleRequiresRepo { agent } => {
                write!(
                    f,
                    "agent '{agent}' defines schedule but no repo; scheduled agents must define a repo"
                )
            }
            ConfigError::PartialForgeIdentity { agent } => {
                write!(
                    f,
                    "agent '{agent}' defines only one of forge_owner and forge_name; a forge identity requires both"
                )
            }
            ConfigError::MountSourceMustBeAbsolute { agent, source } => {
                write!(
                    f,
                    "agent '{agent}' defines mount source that must be absolute: {}",
                    source.display()
                )
            }
            ConfigError::InvalidMountTarget { agent, error } => {
                write!(f, "agent '{agent}' defines invalid mount target: {error}")
            }
            ConfigError::DuplicateMountTarget { agent, target } => {
                write!(
                    f,
                    "agent '{agent}' defines duplicate mount target: {}",
                    target.display()
                )
            }
            ConfigError::OverlappingMountTargets {
                agent,
                first,
                second,
            } => {
                write!(
                    f,
                    "agent '{agent}' defines overlapping mount targets: {} and {}",
                    first.display(),
                    second.display()
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(error) => Some(error),
            ConfigError::Parse(error) => Some(error),
            ConfigError::DefaultDaemonRuntimePaths(error) => Some(error),
            ConfigError::AuditRootNotWritable { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(error: toml::de::Error) -> Self {
        Self::Parse(error)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    daemon: RawDaemonConfig,
    #[serde(default)]
    agents: Vec<RawAgent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDaemonOnlyConfig {
    #[serde(default)]
    daemon: RawDaemonConfig,
    #[serde(default, rename = "agents")]
    _agents: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDaemonConfig {
    socket_path: Option<String>,
    pid_file: Option<String>,
    audit_root: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    name: String,
    base_image: String,
    methodology_dir: String,
    command: RawAgentCommand,
    #[serde(default)]
    mounts: Vec<RawMountConfig>,
    repo: Option<String>,
    schedule: Option<String>,
    repo_token_source: Option<String>,
    forge_type: Option<String>,
    forge_owner: Option<String>,
    forge_name: Option<String>,
    #[serde(default)]
    credentials: Vec<RawCredentialConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgentCommand {
    argv: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMountConfig {
    source: String,
    target: String,
    read_only: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCredentialConfig {
    name: String,
    source: String,
}

fn normalize_path_lexically(path: &Path) -> String {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(_) | Component::RootDir | Component::Prefix(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        ".".to_string()
    } else {
        normalized.to_string_lossy().into_owned()
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        encoded.push(HEX_DIGITS[(byte & 0x0f) as usize] as char);
    }

    encoded
}

fn validate_non_empty(
    field: &'static str,
    value: &str,
    agent: Option<&str>,
    credential: Option<&str>,
) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::EmptyField {
            field,
            agent: agent.map(str::to_owned),
            credential: credential.map(str::to_owned),
        });
    }

    Ok(())
}

fn validate_lookup_key(
    field: &'static str,
    value: &str,
    agent: Option<&str>,
    credential: Option<&str>,
) -> Result<(), ConfigError> {
    validate_non_empty(field, value, agent, credential)?;

    if value != value.trim() {
        return Err(ConfigError::FieldHasOuterWhitespace {
            field,
            agent: agent.map(str::to_owned),
            credential: credential.map(str::to_owned),
        });
    }

    Ok(())
}

fn absolute_config_dir(path: &Path) -> Result<Option<PathBuf>, ConfigError> {
    let base_dir = path.parent().unwrap_or(Path::new("."));
    let absolute_base_dir = if base_dir.is_absolute() {
        base_dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(base_dir)
    };

    Ok(Some(absolute_base_dir))
}

fn resolve_methodology_dir(base_dir: Option<&Path>, methodology_dir: &str) -> PathBuf {
    resolve_config_path(base_dir, methodology_dir)
}

fn validate_schedule(schedule: &str) -> Result<(), croner::errors::CronError> {
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(schedule)
        .map(|_| ())
}

fn validate_absolute_daemon_runtime_path(
    field: &'static str,
    path: &Path,
) -> Result<(), ConfigError> {
    if path.is_absolute() {
        return Ok(());
    }

    Err(ConfigError::RelativeDaemonRuntimePath {
        field,
        path: path.to_path_buf(),
    })
}

fn resolve_config_path(base_dir: Option<&Path>, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_path_buf();
    }

    match base_dir {
        Some(base_dir) => base_dir.join(path),
        None => path.to_path_buf(),
    }
}
