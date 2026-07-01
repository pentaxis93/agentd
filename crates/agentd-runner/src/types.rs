//! Public data types for the session lifecycle.
//!
//! Defines the inputs ([`SessionSpec`], [`SessionInvocation`]), outputs
//! ([`SessionOutcome`]), and error types ([`RunnerError`]) that
//! [`run_session`](crate::run_session) operates on. Validation error types
//! for the standalone validators are also defined here.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

/// Static configuration for a session, derived from an agent.
///
/// Describes the agent identity, container image, methodology, active forge type,
/// command, and caller-resolved environment variables. Validated by
/// [`run_session`](crate::run_session) before any resources are allocated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpec {
    /// Stable daemon-instance identifier used in runner-managed Podman
    /// resource names so startup reconciliation can scope ownership to one
    /// daemon instance.
    pub daemon_instance_id: String,
    /// Agent identity string. Doubles as the in-container unix username, a
    /// component of the container name, and the value of the `AGENT_NAME`
    /// environment variable. Must pass
    /// [`validate_agent_name`](crate::validate_agent_name).
    pub agent_name: String,
    /// Container image reference. The image must provide `/bin/sh`, `git`,
    /// `find`, and the setup/session binaries required by the configured
    /// agent command in `PATH`, including `groupadd`, `useradd`, and `gosu`.
    /// UID/GID `1000` must be available for the session user.
    pub base_image: String,
    /// Host-side path to the methodology directory. Mounted read-only into
    /// the container at `/agentd/methodology`. Must contain `manifest.toml`.
    pub methodology_dir: PathBuf,
    /// Host-side root where persistent audit records are created. The runner
    /// stores each session under `<audit_root>/<agent>/<session_id>/`.
    pub audit_root: PathBuf,
    /// Active forge type exposed to the session as `GROUNDWORK_FORGE_TYPE` so
    /// Groundwork can resolve forge-invariant operations.
    pub forge_type: String,
    /// Additional host bind mounts declared by the selected agent.
    pub mounts: Vec<BindMount>,
    /// Command array executed directly from the cloned repository after
    /// workspace setup. Not a shell command unless the agent explicitly
    /// configures one (for example, `["/bin/sh", "-lc", "..."]`).
    pub agent_command: Vec<String>,
    /// Caller-resolved environment variables injected into the container.
    /// Non-empty values are passed via ephemeral podman secrets; empty values
    /// are passed as direct `--env` assignments.
    /// Names reserved for runner-owned values such as `AGENT_NAME`,
    /// `GROUNDWORK_FORGE_TYPE`, `AGENTD_WORK_UNIT`, `AGENTD_REPO_TOKEN`,
    /// `RUNA_TRANSCRIPT_DIR`, and `RUNA_TRANSCRIPT_REDACT_ENV` are rejected
    /// during validation.
    pub environment: Vec<ResolvedEnvironmentVariable>,
}

/// A host bind mount declared by a session agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindMount {
    /// Host-side source path to bind into the container.
    pub source: PathBuf,
    /// Absolute in-container target path for the bind mount. Must satisfy
    /// [`validate_mount_target`](crate::validate_mount_target). When
    /// validated as part of a mount list, targets must also be pairwise
    /// non-overlapping via [`validate_mount_overlap`](crate::validate_mount_overlap).
    pub target: PathBuf,
    /// Whether the mount is read-only inside the container.
    pub read_only: bool,
}

/// A name-value pair representing an environment variable whose value has
/// already been resolved by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEnvironmentVariable {
    /// Variable name. Must not be empty, contain `,` or `=`, or collide with
    /// runner-managed names.
    pub name: String,
    /// Variable value. Empty values are legal and are injected as direct
    /// `--env NAME=` assignments rather than podman secrets, which reject
    /// zero-byte payloads.
    pub value: String,
}

/// Typed operator-supplied input materialized into the session workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvocationInput {
    /// Convenience text input synthesized into a canonical intent artifact.
    IntentText {
        statement: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// A complete JSON artifact supplied by the operator.
    Artifact {
        artifact_type: String,
        artifact_id: String,
        document: Value,
    },
}

/// Per-invocation parameters for a session launch.
///
/// Describes the repository to clone, optional clone-only repository
/// authentication, optional work-mode target and materialized invocation input,
/// and an optional timeout. Validated by [`run_session`](crate::run_session)
/// before any resources are allocated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInvocation {
    /// Remote repository URL cloned into the container workspace. Must use
    /// `https://`, `http://`, `git://`, `ssh://`, or `user@host:path` form.
    /// Credential-bearing URLs are rejected. When [`Self::repo_token`] is
    /// present, this must use `https://`.
    pub repo_url: String,
    /// Optional bearer token used only for the runner-managed `git clone`
    /// request for `https://` repository URLs. This token is not passed
    /// through to the agent runtime.
    pub repo_token: Option<String>,
    /// Optional work unit identifier passed to `runa run --work-unit` and
    /// exposed through the runner-managed `AGENTD_WORK_UNIT` environment
    /// variable when set.
    pub work_unit: Option<String>,
    /// Optional operator-supplied input to materialize into the repo
    /// workspace after `runa init` and before `runa run`. Artifact names supplied
    /// through [`InvocationInput::Artifact`] must each be a single path
    /// segment. When combined with [`Self::work_unit`], the input must be a
    /// `work-unit` artifact whose id matches the work unit.
    pub input: Option<InvocationInput>,
    /// Optional session timeout. When set, the runner force-removes the
    /// container after this duration and returns
    /// [`SessionOutcome::TimedOut`].
    pub timeout: Option<Duration>,
}

/// Terminal outcome of a completed session.
///
/// Labels and exit-code semantics implement the shared session-outcome
/// vocabulary — canonical: commons/EXIT-CODES.md
/// (<https://github.com/tesserine/commons/blob/main/EXIT-CODES.md>).
/// agentd does not redefine them. `TimedOut` is the one agentd-layer
/// addition: commons scopes caller-enforced timeout outside the shared
/// vocabulary, and agentd is that caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    /// The container process exited with code 0.
    Success { exit_code: i32 },
    /// The session failed without a more specific semantic classification.
    GenericFailure { exit_code: i32 },
    /// The caller invoked the runtime incorrectly.
    UsageError { exit_code: i32 },
    /// Actionable work exists, but execution is blocked on prerequisites.
    Blocked { exit_code: i32 },
    /// No actionable work is currently available.
    NothingReady { exit_code: i32 },
    /// Work was attempted but required completion checks failed.
    WorkFailed { exit_code: i32 },
    /// The runtime or environment failed independently of the work item.
    InfrastructureFailure { exit_code: i32 },
    /// The configured command was found but could not be executed.
    CommandNotExecutable { exit_code: i32 },
    /// The configured command was not found.
    CommandNotFound { exit_code: i32 },
    /// The process terminated from signal `signal`, preserving `128 + signal`.
    TerminatedBySignal { exit_code: i32, signal: i32 },
    /// The session exceeded its timeout and was force-removed.
    TimedOut,
}

impl SessionOutcome {
    /// Interpret a process exit code according to the shared commons contract.
    pub fn from_exit_code(exit_code: i32) -> Self {
        match exit_code {
            0 => Self::Success { exit_code },
            1 => Self::GenericFailure { exit_code },
            2 => Self::UsageError { exit_code },
            3 => Self::Blocked { exit_code },
            4 => Self::NothingReady { exit_code },
            5 => Self::WorkFailed { exit_code },
            6 => Self::InfrastructureFailure { exit_code },
            126 => Self::CommandNotExecutable { exit_code },
            127 => Self::CommandNotFound { exit_code },
            129.. => Self::TerminatedBySignal {
                exit_code,
                signal: exit_code - 128,
            },
            _ => Self::GenericFailure { exit_code },
        }
    }

    /// Canonical snake_case semantic label for this outcome.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Success { .. } => "success",
            Self::GenericFailure { .. } => "generic_failure",
            Self::UsageError { .. } => "usage_error",
            Self::Blocked { .. } => "blocked",
            Self::NothingReady { .. } => "nothing_ready",
            Self::WorkFailed { .. } => "work_failed",
            Self::InfrastructureFailure { .. } => "infrastructure_failure",
            Self::CommandNotExecutable { .. } => "command_not_executable",
            Self::CommandNotFound { .. } => "command_not_found",
            Self::TerminatedBySignal { .. } => "terminated_by_signal",
            Self::TimedOut => "timed_out",
        }
    }

    /// Raw process exit code when this outcome came from process termination.
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Success { exit_code }
            | Self::GenericFailure { exit_code }
            | Self::UsageError { exit_code }
            | Self::Blocked { exit_code }
            | Self::NothingReady { exit_code }
            | Self::WorkFailed { exit_code }
            | Self::InfrastructureFailure { exit_code }
            | Self::CommandNotExecutable { exit_code }
            | Self::CommandNotFound { exit_code } => Some(*exit_code),
            Self::TerminatedBySignal { exit_code, .. } => Some(*exit_code),
            Self::TimedOut => None,
        }
    }

    /// Signal number when this outcome represents signal-derived termination.
    pub fn signal(&self) -> Option<i32> {
        match self {
            Self::TerminatedBySignal { signal, .. } => Some(*signal),
            _ => None,
        }
    }

    /// Whether the CLI should treat this terminal outcome as process success.
    pub fn is_cli_success(&self) -> bool {
        matches!(
            self,
            Self::Success { .. } | Self::Blocked { .. } | Self::NothingReady { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::SessionOutcome;

    #[test]
    fn exit_code_128_is_generic_failure() {
        assert_eq!(
            SessionOutcome::from_exit_code(128),
            SessionOutcome::GenericFailure { exit_code: 128 }
        );
    }

    #[test]
    fn signal_derived_exit_codes_above_128_remain_signal_terminations() {
        assert_eq!(
            SessionOutcome::from_exit_code(130),
            SessionOutcome::TerminatedBySignal {
                exit_code: 130,
                signal: 2,
            }
        );
    }
}

/// Summary of what startup reconciliation removed before the daemon accepted
/// any new sessions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StartupReconciliationReport {
    /// Stale runner-managed session containers matching
    /// `agentd-{daemon8}-{agent}-{session16}` that were removed during startup.
    pub removed_container_names: Vec<String>,
    /// Orphaned runner-managed secrets matching `agentd-{daemon8}-{session16}-{suffix}`
    /// that were removed during startup.
    pub removed_secret_names: Vec<String>,
}

/// Error returned by
/// [`validate_environment_name`](crate::validate_environment_name) when a
/// name violates naming rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentNameValidationError {
    /// The name is empty or contains `,` or `=`.
    Invalid,
    /// The name collides with a runner-managed environment variable.
    Reserved,
}

/// Error returned by [`validate_agent_name`](crate::validate_agent_name)
/// when a name violates naming rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentNameValidationError {
    /// The name is not a valid unix username: must start with a lowercase
    /// letter, contain only lowercase letters, digits, `_`, or `-`, and be
    /// at most 32 characters.
    Invalid,
    /// The name matches a reserved system username (`root`, `nobody`,
    /// `daemon`, `bin`, `sys`, `man`, `mail`).
    Reserved,
}

/// Error returned by [`validate_mount_target`](crate::validate_mount_target)
/// when a bind-mount target violates runner rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountTargetValidationError {
    /// The target is not absolute, contains `.` or `..`, contains `,`, ends
    /// with `/`, or includes `find -path` metacharacters.
    Invalid { path: PathBuf },
    /// The target collides with a runner-managed path.
    Reserved { target: PathBuf },
}

/// Error returned by [`validate_mount_overlap`](crate::validate_mount_overlap)
/// when two distinct bind-mount targets overlap by path components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountOverlapError {
    /// The first offending in-container mount target path.
    pub first: PathBuf,
    /// The second offending in-container mount target path.
    pub second: PathBuf,
}

/// Errors produced during session execution.
///
/// Validation errors ([`InvalidAgentName`](Self::InvalidAgentName),
/// [`InvalidBaseImage`](Self::InvalidBaseImage), etc.) are returned before
/// any resources are allocated. Resource and execution errors
/// ([`MissingMethodologyManifest`](Self::MissingMethodologyManifest),
/// [`Io`](Self::Io), [`PodmanCommandFailed`](Self::PodmanCommandFailed))
/// occur after resource allocation has begun.
#[derive(Debug)]
pub enum RunnerError {
    /// The daemon-instance identifier must be exactly eight lowercase hex
    /// characters.
    InvalidDaemonInstanceId,
    /// The methodology directory does not contain `manifest.toml`. Produced
    /// during resource allocation, after spec and invocation validation pass.
    MissingMethodologyManifest { path: PathBuf },
    /// The agent name fails unix username rules or matches a reserved system
    /// name. Produced during spec validation.
    InvalidAgentName,
    /// The base image string is empty or has surrounding whitespace. Produced
    /// during spec validation.
    InvalidBaseImage,
    /// The configured audit root path is not absolute. Produced during spec
    /// validation.
    InvalidAuditRoot { path: PathBuf },
    /// The active forge type is empty or has surrounding whitespace. Produced
    /// during spec validation.
    InvalidForgeType { forge_type: String },
    /// The repository URL is not a supported remote form (`https://`,
    /// `http://`, `git://`), embeds credentials, or is paired with
    /// `repo_token` without using `https://`. Produced during invocation
    /// validation.
    InvalidRepoUrl { message: String },
    /// The operator-supplied invocation input is unsupported by the
    /// methodology, malformed, or does not match the methodology schema.
    InvalidInvocationInput { message: String },
    /// The command array is empty or contains an empty element.
    /// Produced during spec validation.
    InvalidCommand,
    /// An environment variable name is empty or contains `,` or `=`.
    /// Produced during spec validation.
    InvalidEnvironmentName { name: String },
    /// An environment variable name collides with a runner-managed name.
    /// Produced during spec validation.
    ReservedEnvironmentName { name: String },
    /// A configured bind mount source path is not absolute.
    InvalidMountSource { path: PathBuf },
    /// A configured bind mount target path is not absolute, contains `.` or
    /// `..` components, or contains `,`.
    InvalidMountTarget { path: PathBuf },
    /// Two configured bind mounts share the same target path.
    DuplicateMountTarget { target: PathBuf },
    /// Two configured bind mounts target overlapping container paths.
    OverlappingMountTargets { first: PathBuf, second: PathBuf },
    /// A configured bind mount target collides with a runner-managed mount.
    ReservedMountTarget { target: PathBuf },
    /// Filesystem failure, process I/O failure, or invalid external command
    /// output received after a successful process exit.
    Io(std::io::Error),
    /// A configured bind mount source path does not exist.
    MissingMountSource { path: PathBuf },
    /// A runner-owned shared-relabel bind mount source cannot be encoded safely
    /// for Podman.
    UnencodableRelabelMountSource { path: PathBuf },
    /// A podman CLI invocation returned a non-zero exit status. Captures the
    /// argument list, exit status, and stderr for diagnostics.
    PodmanCommandFailed {
        args: Vec<String>,
        status: ExitStatus,
        stderr: String,
    },
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunnerError::InvalidDaemonInstanceId => {
                write!(
                    f,
                    "daemon_instance_id must be exactly 8 lowercase hex characters"
                )
            }
            RunnerError::MissingMethodologyManifest { path } => {
                write!(
                    f,
                    "methodology directory must contain manifest.toml: {}",
                    path.display()
                )
            }
            RunnerError::InvalidAgentName => write!(
                f,
                "agent_name must already be a unix username starting with a lowercase letter, containing only lowercase letters, digits, '_', or '-', be at most 32 characters, and not be one of the reserved system names root, nobody, daemon, bin, sys, man, or mail"
            ),
            RunnerError::InvalidBaseImage => write!(f, "base_image must not be empty"),
            RunnerError::InvalidAuditRoot { path } => {
                write!(f, "audit_root must be an absolute path: {}", path.display())
            }
            RunnerError::InvalidForgeType { forge_type } => write!(
                f,
                "forge type (`forge_type`) must not be empty or contain surrounding whitespace: {forge_type:?}"
            ),
            RunnerError::InvalidRepoUrl { message } => write!(f, "repo_url {message}"),
            RunnerError::InvalidInvocationInput { message } => write!(f, "{message}"),
            RunnerError::InvalidCommand => {
                write!(f, "command must contain at least one argument")
            }
            RunnerError::InvalidEnvironmentName { name } => write!(
                f,
                "environment variable names must not be empty and must not contain ',' or '=': {name}"
            ),
            RunnerError::ReservedEnvironmentName { name } => {
                if matches!(
                    name.as_str(),
                    "RUNA_TRANSCRIPT_DIR" | "RUNA_TRANSCRIPT_REDACT_ENV"
                ) {
                    write!(
                        f,
                        "environment variable name is reserved by the runner: {name} (runner-owned; set internally by transcript subsystem)"
                    )
                } else {
                    write!(
                        f,
                        "environment variable name is reserved by the runner: {name}"
                    )
                }
            }
            RunnerError::InvalidMountSource { path } => {
                write!(
                    f,
                    "mount source must be an absolute path: {}",
                    path.display()
                )
            }
            RunnerError::InvalidMountTarget { path } => mount_target_invalid_message(f, path),
            RunnerError::DuplicateMountTarget { target } => {
                write!(f, "mount targets must be unique: {}", target.display())
            }
            RunnerError::OverlappingMountTargets { first, second } => {
                mount_target_overlap_message(f, first, second)
            }
            RunnerError::ReservedMountTarget { target } => mount_target_reserved_message(f, target),
            RunnerError::Io(error) => write!(f, "{error}"),
            RunnerError::MissingMountSource { path } => {
                write!(f, "mount source path does not exist: {}", path.display())
            }
            RunnerError::UnencodableRelabelMountSource { path } => write!(
                f,
                "shared relabel bind mount source cannot contain both ',' and ':': {}",
                path.display()
            ),
            RunnerError::PodmanCommandFailed {
                args,
                status,
                stderr,
            } => write!(
                f,
                "podman {} failed with status {}: {}",
                args.join(" "),
                exit_status_label(status),
                stderr.trim()
            ),
        }
    }
}

impl fmt::Display for MountTargetValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MountTargetValidationError::Invalid { path } => mount_target_invalid_message(f, path),
            MountTargetValidationError::Reserved { target } => {
                mount_target_reserved_message(f, target)
            }
        }
    }
}

impl fmt::Display for MountOverlapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        mount_target_overlap_message(f, &self.first, &self.second)
    }
}

impl std::error::Error for RunnerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunnerError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl std::error::Error for MountTargetValidationError {}

impl std::error::Error for MountOverlapError {}

impl From<std::io::Error> for RunnerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn mount_target_invalid_message(f: &mut fmt::Formatter<'_>, path: &Path) -> fmt::Result {
    write!(
        f,
        "mount target must be an absolute path without trailing '/', '.' or '..' components, ',', or find metacharacters ('*', '?', '[', ']', '\\\\'): {}",
        path.display()
    )
}

fn mount_target_reserved_message(f: &mut fmt::Formatter<'_>, target: &Path) -> fmt::Result {
    write!(
        f,
        "mount target is reserved by the runner: {}",
        target.display()
    )
}

fn mount_target_overlap_message(
    f: &mut fmt::Formatter<'_>,
    first: &Path,
    second: &Path,
) -> fmt::Result {
    write!(
        f,
        "mount targets must not overlap: {} and {}",
        first.display(),
        second.display()
    )
}

fn exit_status_label(status: &ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string())
}
