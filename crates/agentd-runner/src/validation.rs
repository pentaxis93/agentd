//! Input validation for session specs and invocations.
//!
//! All validation runs before any filesystem or podman interaction, so invalid
//! inputs are rejected without side effects. The public validators
//! ([`validate_agent_name`], [`validate_environment_name`], and
//! [`validate_mount_target`], [`validate_mount_overlap`]) are also used by the
//! configuration layer in the `agentd` crate.

use crate::input::INVOCATION_INPUT_MOUNT_PATH;
use crate::naming::is_daemon_instance_id;
use crate::session_paths::{session_home_dir, session_internal_agentd_dir, session_repo_dir};
use crate::types::{
    AgentNameValidationError, BindMount, EnvironmentNameValidationError, InvocationInput,
    MountOverlapError, MountTargetValidationError, RunnerError, SessionInvocation, SessionSpec,
};
use std::collections::HashSet;
use std::path::Path;

const AGENT_NAME_ENV: &str = "AGENT_NAME";
const GROUNDWORK_FORGE_TYPE_ENV: &str = "GROUNDWORK_FORGE_TYPE";
const WORK_UNIT_ENV: &str = "AGENTD_WORK_UNIT";
pub(crate) const REPO_TOKEN_ENV: &str = "AGENTD_REPO_TOKEN";
pub(crate) const TRANSCRIPT_DIR_ENV: &str = "RUNA_TRANSCRIPT_DIR";
pub(crate) const TRANSCRIPT_REDACT_ENV: &str = "RUNA_TRANSCRIPT_REDACT_ENV";
const RESERVED_AGENT_NAMES: [&str; 7] = ["root", "nobody", "daemon", "bin", "sys", "man", "mail"];
const SUPPORTED_REPO_URL_FORMS: &str = "https://, http://, git://, ssh://, or user@host:path";
const METHODOLOGY_MOUNT_PATH: &str = "/agentd/methodology";
const TRANSCRIPT_MOUNT_PATH: &str = "/agentd/transcript";

pub(crate) fn validate_spec(spec: &SessionSpec) -> Result<(), RunnerError> {
    if !is_daemon_instance_id(&spec.daemon_instance_id) {
        return Err(RunnerError::InvalidDaemonInstanceId);
    }
    if validate_agent_name(&spec.agent_name).is_err() {
        return Err(RunnerError::InvalidAgentName);
    }
    if spec.base_image.trim().is_empty() || spec.base_image != spec.base_image.trim() {
        return Err(RunnerError::InvalidBaseImage);
    }
    if !spec.audit_root.is_absolute() {
        return Err(RunnerError::InvalidAuditRoot {
            path: spec.audit_root.clone(),
        });
    }
    if spec.forge_type.trim().is_empty() || spec.forge_type != spec.forge_type.trim() {
        return Err(RunnerError::InvalidForgeType {
            forge_type: spec.forge_type.clone(),
        });
    }
    if spec.agent_command.is_empty() || spec.agent_command.iter().any(|arg| arg.is_empty()) {
        return Err(RunnerError::InvalidCommand);
    }

    let mut seen_mount_targets = HashSet::new();
    for mount in &spec.mounts {
        if !mount.source.is_absolute() {
            return Err(RunnerError::InvalidMountSource {
                path: mount.source.clone(),
            });
        }
        match validate_mount_target(&mount.target, &spec.agent_name) {
            Ok(()) => {}
            Err(MountTargetValidationError::Invalid { path }) => {
                return Err(RunnerError::InvalidMountTarget { path });
            }
            Err(MountTargetValidationError::Reserved { target }) => {
                return Err(RunnerError::ReservedMountTarget { target });
            }
        }
        if !seen_mount_targets.insert(mount.target.clone()) {
            return Err(RunnerError::DuplicateMountTarget {
                target: mount.target.clone(),
            });
        }
    }
    if let Err(MountOverlapError { first, second }) = validate_mount_overlap(&spec.mounts) {
        return Err(RunnerError::OverlappingMountTargets { first, second });
    }

    for variable in &spec.environment {
        match validate_environment_name(&variable.name) {
            Ok(()) => {}
            Err(EnvironmentNameValidationError::Invalid) => {
                return Err(RunnerError::InvalidEnvironmentName {
                    name: variable.name.clone(),
                });
            }
            Err(EnvironmentNameValidationError::Reserved) => {
                return Err(RunnerError::ReservedEnvironmentName {
                    name: variable.name.clone(),
                });
            }
        }
    }

    Ok(())
}

/// Validates an in-container bind-mount target against the runner contract.
///
/// Rejects targets that are not absolute, contain `.` or `..` components,
/// contain `,`, end with `/`, contain `find -path` metacharacters, or collide
/// with runner-managed paths such as `/agentd/methodology`,
/// `/home/{agent}`, or `/home/{agent}/repo`.
pub fn validate_mount_target(
    target: &Path,
    agent_name: &str,
) -> Result<(), MountTargetValidationError> {
    if !target.is_absolute()
        || has_relative_mount_target_component(target)
        || mount_target_contains_comma(target)
        || mount_target_has_trailing_slash(target)
        || mount_target_contains_find_metacharacters(target)
    {
        return Err(MountTargetValidationError::Invalid {
            path: target.to_path_buf(),
        });
    }
    if is_reserved_mount_target(target, agent_name) {
        return Err(MountTargetValidationError::Reserved {
            target: target.to_path_buf(),
        });
    }

    Ok(())
}

/// Validates that declared bind-mount targets are pairwise non-overlapping.
///
/// Rejects distinct targets when one is a component-aware prefix of the other,
/// such as `/home/{agent}/.config` and `/home/{agent}/.config/claude`.
pub fn validate_mount_overlap(mounts: &[BindMount]) -> Result<(), MountOverlapError> {
    for (index, mount) in mounts.iter().enumerate() {
        for other in mounts.iter().skip(index + 1) {
            // Exact-equal targets are ignored here because this public validator
            // must remain a standalone overlap check; duplicate detection lives
            // elsewhere and preserves a more precise error message.
            if mount.target == other.target {
                continue;
            }

            if mount.target.starts_with(&other.target) || other.target.starts_with(&mount.target) {
                return Err(MountOverlapError {
                    first: mount.target.clone(),
                    second: other.target.clone(),
                });
            }
        }
    }

    Ok(())
}

pub(crate) fn validate_invocation(invocation: &SessionInvocation) -> Result<(), RunnerError> {
    validate_repo_url(&invocation.repo_url)?;

    if invocation.repo_token.is_some() && repo_url_kind(&invocation.repo_url) != RepoUrlKind::Https
    {
        return Err(repo_token_requires_https_error());
    }
    if let Some(work_unit) = &invocation.work_unit {
        if let Some(input) = &invocation.input {
            validate_work_mode_input(work_unit, input)?;
        }
    }

    Ok(())
}

fn validate_work_mode_input(work_unit: &str, input: &InvocationInput) -> Result<(), RunnerError> {
    match input {
        InvocationInput::IntentText { .. } => Err(RunnerError::InvalidInvocationInput {
            message: "manual invocation cannot combine work_unit with intent input".to_string(),
        }),
        InvocationInput::Artifact {
            artifact_type,
            artifact_id,
            ..
        } => {
            if artifact_type != "work-unit" {
                return Err(RunnerError::InvalidInvocationInput {
                    message: "manual work-mode artifact input must use artifact type 'work-unit'"
                        .to_string(),
                });
            }
            if artifact_id != work_unit {
                return Err(RunnerError::InvalidInvocationInput {
                    message: format!(
                        "manual work-mode artifact id must match work_unit '{work_unit}'"
                    ),
                });
            }
            Ok(())
        }
    }
}

/// Validates an environment variable name against naming rules.
///
/// Rejects names that are empty, contain `,` or `=`, or collide with
/// runner-managed names such as `AGENT_NAME`, `GROUNDWORK_FORGE_TYPE`,
/// `AGENTD_WORK_UNIT`, `AGENTD_REPO_TOKEN`, `RUNA_TRANSCRIPT_DIR`, and
/// `RUNA_TRANSCRIPT_REDACT_ENV`. Used both by
/// [`run_session`](crate::run_session) during spec validation and by the
/// configuration layer for credential name validation.
pub fn validate_environment_name(name: &str) -> Result<(), EnvironmentNameValidationError> {
    if name.is_empty() || name.contains('=') || name.contains(',') {
        return Err(EnvironmentNameValidationError::Invalid);
    }
    if is_reserved_environment_name(name) {
        return Err(EnvironmentNameValidationError::Reserved);
    }

    Ok(())
}

/// Validates an agent name against unix username rules and reserved names.
///
/// The name must start with a lowercase ASCII letter, contain only lowercase
/// letters, digits, `_`, or `-`, and be at most 32 characters. Names matching
/// reserved system usernames (`root`, `nobody`, `daemon`, `bin`, `sys`, `man`,
/// `mail`) are also rejected. Used both by [`run_session`](crate::run_session)
/// during spec validation and by the configuration layer.
pub fn validate_agent_name(name: &str) -> Result<(), AgentNameValidationError> {
    if !is_valid_unix_username(name) {
        return Err(AgentNameValidationError::Invalid);
    }
    if is_reserved_agent_name(name) {
        return Err(AgentNameValidationError::Reserved);
    }

    Ok(())
}

/// Validates a remote repository URL against the runner's supported forms.
///
/// Accepts only trimmed `https://`, `http://`, `git://`, `ssh://`, and
/// `user@host:path` remote URLs with a non-empty authority and path.
/// Credential-bearing URLs and URLs with query or fragment components are
/// rejected.
pub fn validate_repo_url(repo_url: &str) -> Result<(), RunnerError> {
    if repo_url.trim().is_empty() || repo_url != repo_url.trim() {
        return Err(unsupported_repo_url_error());
    }

    if is_credential_bearing_repo_url(repo_url) {
        return Err(credential_bearing_repo_url_error());
    }

    if repo_url_kind(repo_url) == RepoUrlKind::Unsupported {
        return Err(unsupported_repo_url_error());
    }

    Ok(())
}

/// Returns the environment variables managed by the runner itself.
///
/// These names are reserved — callers cannot use them in
/// [`SessionSpec::environment`] because the runner injects them directly.
pub(crate) fn runner_managed_environment(spec: &SessionSpec) -> [(&str, &str); 2] {
    [
        (AGENT_NAME_ENV, &spec.agent_name),
        (GROUNDWORK_FORGE_TYPE_ENV, &spec.forge_type),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoUrlKind {
    Https,
    OtherPlainGit,
    Ssh,
    Unsupported,
}

pub(crate) fn repo_url_kind(repo_url: &str) -> RepoUrlKind {
    if repo_url.contains(['?', '#']) {
        return RepoUrlKind::Unsupported;
    }

    if is_supported_scp_like_ssh_repo_url(repo_url) {
        return RepoUrlKind::Ssh;
    }

    let Some(scheme) = repo_url_scheme(repo_url) else {
        return RepoUrlKind::Unsupported;
    };
    let Some((authority, path)) = repo_url_authority(repo_url).zip(repo_url_path(repo_url)) else {
        return RepoUrlKind::Unsupported;
    };

    let supported_authority_and_path = !authority.is_empty()
        && !authority.starts_with('/')
        && path.starts_with('/')
        && path.len() > 1;
    if !supported_authority_and_path {
        return RepoUrlKind::Unsupported;
    }

    match scheme {
        "https" => RepoUrlKind::Https,
        "http" | "git" => RepoUrlKind::OtherPlainGit,
        "ssh" if is_supported_ssh_authority(authority) => RepoUrlKind::Ssh,
        _ => RepoUrlKind::Unsupported,
    }
}

fn is_credential_bearing_repo_url(repo_url: &str) -> bool {
    if let Some(authority) = repo_url_authority(repo_url) {
        if repo_url_scheme(repo_url) == Some("ssh") {
            return ssh_authority_user(authority).is_some_and(|user| user.contains(':'));
        }

        return authority.contains('@');
    }

    scp_like_ssh_user(repo_url).is_some_and(|user| user.contains(':'))
}

fn repo_url_scheme(repo_url: &str) -> Option<&str> {
    repo_url.get(..repo_url.find("://")?)
}

fn repo_url_authority(repo_url: &str) -> Option<&str> {
    let scheme_end = repo_url.find("://")?;
    let remainder = repo_url.get(scheme_end + 3..)?;
    let path_start = remainder.find('/').unwrap_or(remainder.len());
    remainder.get(..path_start)
}

fn repo_url_path(repo_url: &str) -> Option<&str> {
    let scheme_end = repo_url.find("://")?;
    let remainder = repo_url.get(scheme_end + 3..)?;
    let path_start = remainder.find('/')?;
    remainder.get(path_start..)
}

fn is_supported_ssh_authority(authority: &str) -> bool {
    let host_port = match authority.rsplit_once('@') {
        Some((user, host_port)) if !user.is_empty() && !user.contains(':') => host_port,
        Some(_) => return false,
        None => authority,
    };

    let host = host_port
        .rsplit_once(':')
        .map(|(host, port)| {
            if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                return "";
            }
            host
        })
        .unwrap_or(host_port);

    !host.is_empty()
}

fn is_supported_scp_like_ssh_repo_url(repo_url: &str) -> bool {
    let Some((user_host, path)) = repo_url.rsplit_once(':') else {
        return false;
    };
    if path.is_empty() || user_host.contains("://") {
        return false;
    }
    let Some((user, host)) = user_host.rsplit_once('@') else {
        return false;
    };

    !user.is_empty() && !user.contains(':') && !host.is_empty()
}

fn ssh_authority_user(authority: &str) -> Option<&str> {
    authority.rsplit_once('@').map(|(user, _)| user)
}

fn scp_like_ssh_user(repo_url: &str) -> Option<&str> {
    let (user_host, _) = repo_url.rsplit_once(':')?;
    user_host.rsplit_once('@').map(|(user, _)| user)
}

fn unsupported_repo_url_error() -> RunnerError {
    RunnerError::InvalidRepoUrl {
        message: format!("must be a remote URL using {SUPPORTED_REPO_URL_FORMS}"),
    }
}

fn credential_bearing_repo_url_error() -> RunnerError {
    RunnerError::InvalidRepoUrl {
        message: "must not embed credentials in the URL; credential-bearing URLs are not accepted until #32 lands".to_string(),
    }
}

fn repo_token_requires_https_error() -> RunnerError {
    RunnerError::InvalidRepoUrl {
        message: "must use https:// when repo_token is set".to_string(),
    }
}

fn is_reserved_environment_name(name: &str) -> bool {
    matches!(
        name,
        AGENT_NAME_ENV
            | GROUNDWORK_FORGE_TYPE_ENV
            | WORK_UNIT_ENV
            | REPO_TOKEN_ENV
            | TRANSCRIPT_DIR_ENV
            | TRANSCRIPT_REDACT_ENV
    )
}

fn is_reserved_agent_name(name: &str) -> bool {
    RESERVED_AGENT_NAMES.contains(&name)
}

fn is_reserved_mount_target(target: &Path, agent_name: &str) -> bool {
    let home_dir = session_home_dir(agent_name);
    let internal_agentd_dir = session_internal_agentd_dir(agent_name);
    let repo_dir = session_repo_dir(agent_name);
    let methodology_dir = Path::new(METHODOLOGY_MOUNT_PATH);
    let invocation_input_dir = Path::new(INVOCATION_INPUT_MOUNT_PATH);
    let transcript_dir = Path::new(TRANSCRIPT_MOUNT_PATH);

    // Each rule states the invariant for one runner-owned path. Intentional
    // overlap is part of the contract: targets like `/home` or `/` can
    // legitimately collide with more than one runner-owned path, and a future
    // refactor should preserve that instead of collapsing these checks.
    //
    // Target and runner-owned methodology path must be disjoint by path
    // components: neither may be a prefix of the other.
    if target.starts_with(methodology_dir) || methodology_dir.starts_with(target) {
        return true;
    }

    if target.starts_with(invocation_input_dir) || invocation_input_dir.starts_with(target) {
        return true;
    }

    if target.starts_with(transcript_dir) || transcript_dir.starts_with(target) {
        return true;
    }

    // Target and runner-owned repo path must be disjoint by path components:
    // neither may be a prefix of the other. This keeps `/home/{agent}/repo-cache`
    // valid while reserving `/home/{agent}/repo`, its descendants, and its
    // ancestors.
    if target.starts_with(&repo_dir) || repo_dir.starts_with(target) {
        return true;
    }

    // agentd's internal audit bridge under HOME is runner-owned. This keeps
    // operator-declared mounts from colliding with the reserved `.agentd`
    // subtree while still permitting other supported descendants like
    // `.claude`.
    if target.starts_with(&internal_agentd_dir) || internal_agentd_dir.starts_with(target) {
        return true;
    }

    // Home is narrower: the home root and its ancestors are reserved, while
    // descendants such as `.claude` and `.runa` are the supported mount surface.
    home_dir.starts_with(target)
}

fn has_relative_mount_target_component(target: &Path) -> bool {
    target
        .as_os_str()
        .to_string_lossy()
        .split('/')
        .any(|component| matches!(component, "." | ".."))
}

fn mount_target_contains_comma(target: &Path) -> bool {
    target.as_os_str().to_string_lossy().contains(',')
}

fn mount_target_has_trailing_slash(target: &Path) -> bool {
    target != Path::new("/") && target.as_os_str().to_string_lossy().ends_with('/')
}

fn mount_target_contains_find_metacharacters(target: &Path) -> bool {
    target
        .as_os_str()
        .to_string_lossy()
        .chars()
        .any(|character| matches!(character, '*' | '?' | '[' | ']' | '\\'))
}

fn is_valid_unix_username(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first_character) = characters.next() else {
        return false;
    };

    if !first_character.is_ascii_lowercase() || name.len() > 32 {
        return false;
    }

    characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '-'
            || character == '_'
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BindMount, InvocationInput, ResolvedEnvironmentVariable, test_support::test_session_spec,
    };
    use std::path::PathBuf;

    #[test]
    fn validate_spec_rejects_reserved_environment_names() {
        for reserved_name in [
            "AGENT_NAME",
            "GROUNDWORK_FORGE_TYPE",
            WORK_UNIT_ENV,
            REPO_TOKEN_ENV,
            "RUNA_TRANSCRIPT_DIR",
            "RUNA_TRANSCRIPT_REDACT_ENV",
        ] {
            let error = validate_spec(&SessionSpec {
                environment: vec![ResolvedEnvironmentVariable {
                    name: reserved_name.to_string(),
                    value: "spoofed".to_string(),
                }],
                ..test_session_spec()
            })
            .expect_err("reserved runner environment names should be rejected");

            match &error {
                RunnerError::ReservedEnvironmentName { name } => {
                    assert_eq!(name, reserved_name);
                }
                other => panic!("expected ReservedEnvironmentName, got {other:?}"),
            }

            let message = error.to_string();
            assert!(
                message.contains(reserved_name),
                "reserved environment error should name {reserved_name}: {message}"
            );
            if reserved_name.starts_with("RUNA_TRANSCRIPT_") {
                assert!(
                    message.contains("transcript subsystem") && message.contains("set internally"),
                    "transcript reserved environment error should explain ownership: {message}"
                );
            }
        }
    }

    #[test]
    fn validate_spec_rejects_invalid_daemon_instance_ids() {
        for daemon_instance_id in ["", "abcd123", "abcd12345", "ABCDEF12", "zzzzzzzz"] {
            let error = validate_spec(&SessionSpec {
                daemon_instance_id: daemon_instance_id.to_string(),
                ..test_session_spec()
            })
            .expect_err("daemon instance ids must be eight lowercase hex characters");

            assert!(
                matches!(error, RunnerError::InvalidDaemonInstanceId),
                "expected InvalidDaemonInstanceId for {daemon_instance_id:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn validate_spec_rejects_empty_or_whitespace_padded_forge_type() {
        for forge_type in ["", " ", " sourcehut", "sourcehut "] {
            let error = validate_spec(&SessionSpec {
                forge_type: forge_type.to_string(),
                ..test_session_spec()
            })
            .expect_err("forge type values must be non-empty and trimmed");

            match &error {
                RunnerError::InvalidForgeType {
                    forge_type: invalid_forge_type,
                } => {
                    assert_eq!(invalid_forge_type, forge_type);
                }
                other => panic!("expected InvalidForgeType, got {other:?}"),
            }

            assert!(
                error.to_string().contains("forge type"),
                "invalid forge type error should explain forge type validation: {error}"
            );
        }
    }

    #[test]
    fn validate_spec_rejects_environment_names_containing_commas() {
        let error = validate_spec(&SessionSpec {
            environment: vec![ResolvedEnvironmentVariable {
                name: "TOKEN,EXTRA".to_string(),
                value: "secret".to_string(),
            }],
            ..test_session_spec()
        })
        .expect_err("comma-delimited environment names should be rejected");

        match error {
            RunnerError::InvalidEnvironmentName { name } => {
                assert_eq!(name, "TOKEN,EXTRA");
            }
            other => panic!("expected InvalidEnvironmentName, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_environment_names_containing_equals_signs() {
        let error = validate_spec(&SessionSpec {
            environment: vec![ResolvedEnvironmentVariable {
                name: "TOKEN=EXTRA".to_string(),
                value: "secret".to_string(),
            }],
            ..test_session_spec()
        })
        .expect_err("environment names containing '=' should be rejected");

        match error {
            RunnerError::InvalidEnvironmentName { name } => {
                assert_eq!(name, "TOKEN=EXTRA");
            }
            other => panic!("expected InvalidEnvironmentName, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_base_image_with_surrounding_whitespace() {
        for base_image in [" image", "image ", " image "] {
            let error = validate_spec(&SessionSpec {
                base_image: base_image.to_string(),
                ..test_session_spec()
            })
            .expect_err("base_image values with surrounding whitespace should be rejected");

            assert!(
                matches!(error, RunnerError::InvalidBaseImage),
                "expected InvalidBaseImage for {base_image:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn validate_spec_rejects_relative_audit_roots() {
        let error = validate_spec(&SessionSpec {
            audit_root: PathBuf::from("relative/audit-root"),
            ..test_session_spec()
        })
        .expect_err("relative audit roots should be rejected");

        match error {
            RunnerError::InvalidAuditRoot { path } => {
                assert_eq!(path, PathBuf::from("relative/audit-root"));
            }
            other => panic!("expected InvalidAuditRoot, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_sources_that_are_not_absolute() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("relative/source"),
                target: PathBuf::from("/home/site-builder/.claude"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("relative mount sources should be rejected");

        match error {
            RunnerError::InvalidMountSource { path } => {
                assert_eq!(path, PathBuf::from("relative/source"));
            }
            other => panic!("expected InvalidMountSource, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_that_are_not_absolute() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from(".claude"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("relative mount targets should be rejected");

        match error {
            RunnerError::InvalidMountTarget { path } => {
                assert_eq!(path, PathBuf::from(".claude"));
            }
            other => panic!("expected InvalidMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_duplicate_mount_targets() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![
                BindMount {
                    source: PathBuf::from("/home/core/.claude"),
                    target: PathBuf::from("/home/site-builder/.claude"),
                    read_only: true,
                },
                BindMount {
                    source: PathBuf::from("/var/lib/tesserine/audit"),
                    target: PathBuf::from("/home/site-builder/.claude"),
                    read_only: false,
                },
            ],
            ..test_session_spec()
        })
        .expect_err("duplicate mount targets should be rejected");

        match error {
            RunnerError::DuplicateMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/home/site-builder/.claude"));
            }
            other => panic!("expected DuplicateMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_that_collide_with_methodology_mount() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/agentd/methodology"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets must not collide with the methodology mount");

        match error {
            RunnerError::ReservedMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/agentd/methodology"));
            }
            other => panic!("expected ReservedMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_under_methodology_mount() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/agentd/methodology/manifest.toml"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets under the methodology mount should be reserved");

        match error {
            RunnerError::ReservedMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/agentd/methodology/manifest.toml"));
            }
            other => panic!("expected ReservedMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_that_collide_with_invocation_input_mount() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/agentd/invocation-input"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets must not collide with the invocation input mount");

        match error {
            RunnerError::ReservedMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/agentd/invocation-input"));
            }
            other => panic!("expected ReservedMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_that_collide_with_transcript_mount() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/agentd/transcript"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets must not collide with the transcript mount");

        match error {
            RunnerError::ReservedMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/agentd/transcript"));
            }
            other => panic!("expected ReservedMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_that_are_ancestors_of_methodology_mount() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/agentd"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets that are ancestors of the methodology mount should be reserved");

        match error {
            RunnerError::ReservedMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/agentd"));
            }
            other => panic!("expected ReservedMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_that_collide_with_home_directory() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/home/site-builder"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets must not collide with the runner-managed home directory");

        match error {
            RunnerError::ReservedMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/home/site-builder"));
            }
            other => panic!("expected ReservedMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_that_are_ancestors_of_home_directory() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/home"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets that are ancestors of the runner-managed home directory should be reserved");

        match error {
            RunnerError::ReservedMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/home"));
            }
            other => panic!("expected ReservedMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_that_collide_with_repo_directory() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/var/lib/tesserine/repo-cache"),
                target: PathBuf::from("/home/site-builder/repo"),
                read_only: false,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets must not collide with the runner-managed repo directory");

        match error {
            RunnerError::ReservedMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/home/site-builder/repo"));
            }
            other => panic!("expected ReservedMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_under_repo_directory() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/var/lib/tesserine/git"),
                target: PathBuf::from("/home/site-builder/repo/.git"),
                read_only: false,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets under the repo directory should be reserved");

        match error {
            RunnerError::ReservedMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/home/site-builder/repo/.git"));
            }
            other => panic!("expected ReservedMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_that_are_ancestors_of_all_runner_managed_paths() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err(
            "mount targets that are ancestors of every runner-managed path should be reserved",
        );

        match error {
            RunnerError::ReservedMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/"));
            }
            other => panic!("expected ReservedMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_with_parent_dir_components() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/var/lib/tesserine/git"),
                target: PathBuf::from("/home/site-builder/x/../repo/.git"),
                read_only: false,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets with '..' components should be rejected");

        match error {
            RunnerError::InvalidMountTarget { path } => {
                assert_eq!(path, PathBuf::from("/home/site-builder/x/../repo/.git"));
            }
            other => panic!("expected InvalidMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_with_current_dir_components() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/home/site-builder/./a"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets with '.' components should be rejected");

        match error {
            RunnerError::InvalidMountTarget { path } => {
                assert_eq!(path, PathBuf::from("/home/site-builder/./a"));
            }
            other => panic!("expected InvalidMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_containing_commas() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/home/site-builder/data,archive"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets containing commas should be rejected");

        match error {
            RunnerError::InvalidMountTarget { path } => {
                assert_eq!(path, PathBuf::from("/home/site-builder/data,archive"));
            }
            other => panic!("expected InvalidMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_with_trailing_slashes() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/home/site-builder/.claude/"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets with trailing slashes should be rejected");

        match error {
            RunnerError::InvalidMountTarget { path } => {
                assert_eq!(path, PathBuf::from("/home/site-builder/.claude/"));
            }
            other => panic!("expected InvalidMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_rejects_mount_targets_containing_find_metacharacters() {
        for target in [
            "/home/site-builder/foo*bar",
            "/home/site-builder/foo?bar",
            "/home/site-builder/[x]",
            r"/home/site-builder/foo\bar",
        ] {
            let error = validate_spec(&SessionSpec {
                mounts: vec![BindMount {
                    source: PathBuf::from("/home/core/.claude"),
                    target: PathBuf::from(target),
                    read_only: true,
                }],
                ..test_session_spec()
            })
            .expect_err("mount targets with find metacharacters should be rejected");

            match error {
                RunnerError::InvalidMountTarget { path } => {
                    assert_eq!(path, PathBuf::from(target));
                }
                other => panic!("expected InvalidMountTarget, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_spec_accepts_mount_targets_under_home_outside_runner_managed_paths() {
        validate_spec(&SessionSpec {
            mounts: vec![
                BindMount {
                    source: PathBuf::from("/home/core/.claude"),
                    target: PathBuf::from("/home/site-builder/.claude"),
                    read_only: true,
                },
                BindMount {
                    source: PathBuf::from("/var/lib/tesserine/session-runtime"),
                    target: PathBuf::from("/home/site-builder/.runa"),
                    read_only: false,
                },
                BindMount {
                    source: PathBuf::from("/var/lib/tesserine/repo-cache"),
                    target: PathBuf::from("/home/site-builder/repo-cache"),
                    read_only: false,
                },
            ],
            ..test_session_spec()
        })
        .expect("mount targets under home outside runner-managed paths should be accepted");
    }

    #[test]
    fn validate_spec_rejects_mount_targets_under_internal_agentd_tree() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/var/lib/tesserine/audit"),
                target: PathBuf::from("/home/site-builder/.agentd/audit"),
                read_only: false,
            }],
            ..test_session_spec()
        })
        .expect_err("mount targets under the internal .agentd tree should be reserved");

        match error {
            RunnerError::ReservedMountTarget { target } => {
                assert_eq!(target, PathBuf::from("/home/site-builder/.agentd/audit"));
            }
            other => panic!("expected ReservedMountTarget, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_accepts_mount_targets_without_trailing_slashes_or_find_metacharacters() {
        validate_spec(&SessionSpec {
            mounts: vec![
                BindMount {
                    source: PathBuf::from("/home/core/.claude"),
                    target: PathBuf::from("/home/site-builder/.claude"),
                    read_only: true,
                },
                BindMount {
                    source: PathBuf::from("/home/core/.config/claude"),
                    target: PathBuf::from("/home/site-builder/.config/claude"),
                    read_only: true,
                },
            ],
            ..test_session_spec()
        })
        .expect("mount targets without trailing slashes or find metacharacters should be accepted");
    }

    #[test]
    fn validate_spec_accepts_mount_targets_with_methodology_prefix_outside_reserved_tree() {
        validate_spec(&SessionSpec {
            mounts: vec![BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/agentd/methodology-cache"),
                read_only: true,
            }],
            ..test_session_spec()
        })
        .expect("mount targets outside the methodology path components should be accepted");
    }

    #[test]
    fn validate_mount_overlap_rejects_nested_mount_targets() {
        let error = validate_mount_overlap(&[
            BindMount {
                source: PathBuf::from("/home/core/.config"),
                target: PathBuf::from("/home/site-builder/.config"),
                read_only: true,
            },
            BindMount {
                source: PathBuf::from("/home/core/.config/claude"),
                target: PathBuf::from("/home/site-builder/.config/claude"),
                read_only: true,
            },
        ])
        .expect_err("nested mount targets should be rejected");

        assert_eq!(
            error,
            MountOverlapError {
                first: PathBuf::from("/home/site-builder/.config"),
                second: PathBuf::from("/home/site-builder/.config/claude"),
            }
        );
    }

    #[test]
    fn validate_mount_overlap_rejects_nested_mount_targets_in_reverse_order() {
        let error = validate_mount_overlap(&[
            BindMount {
                source: PathBuf::from("/home/core/.config/claude"),
                target: PathBuf::from("/home/site-builder/.config/claude"),
                read_only: true,
            },
            BindMount {
                source: PathBuf::from("/home/core/.config"),
                target: PathBuf::from("/home/site-builder/.config"),
                read_only: true,
            },
        ])
        .expect_err("nested mount targets should be rejected regardless of order");

        assert_eq!(
            error,
            MountOverlapError {
                first: PathBuf::from("/home/site-builder/.config/claude"),
                second: PathBuf::from("/home/site-builder/.config"),
            }
        );
    }

    #[test]
    fn validate_mount_overlap_accepts_disjoint_sibling_targets() {
        validate_mount_overlap(&[
            BindMount {
                source: PathBuf::from("/home/core/.config"),
                target: PathBuf::from("/home/site-builder/.config"),
                read_only: true,
            },
            BindMount {
                source: PathBuf::from("/home/core/.claude"),
                target: PathBuf::from("/home/site-builder/.claude"),
                read_only: true,
            },
        ])
        .expect("disjoint sibling targets should be accepted");
    }

    #[test]
    fn validate_mount_overlap_accepts_component_distinct_targets() {
        validate_mount_overlap(&[
            BindMount {
                source: PathBuf::from("/home/core/.config-alt"),
                target: PathBuf::from("/home/site-builder/.config-alt"),
                read_only: true,
            },
            BindMount {
                source: PathBuf::from("/home/core/.config"),
                target: PathBuf::from("/home/site-builder/.config"),
                read_only: true,
            },
        ])
        .expect("component-distinct targets should be accepted");
    }

    #[test]
    fn validate_spec_rejects_overlapping_mount_targets() {
        let error = validate_spec(&SessionSpec {
            mounts: vec![
                BindMount {
                    source: PathBuf::from("/home/core/.config"),
                    target: PathBuf::from("/home/site-builder/.config"),
                    read_only: true,
                },
                BindMount {
                    source: PathBuf::from("/home/core/.config/claude"),
                    target: PathBuf::from("/home/site-builder/.config/claude"),
                    read_only: true,
                },
            ],
            ..test_session_spec()
        })
        .expect_err("overlapping mount targets should be rejected");

        match error {
            RunnerError::OverlappingMountTargets { first, second } => {
                assert_eq!(first, PathBuf::from("/home/site-builder/.config"));
                assert_eq!(second, PathBuf::from("/home/site-builder/.config/claude"));
            }
            other => panic!("expected OverlappingMountTargets, got {other:?}"),
        }
    }

    #[test]
    fn validate_spec_accepts_valid_unix_agent_names() {
        for agent_name in [
            "site-builder",
            "site-builder-01",
            "site-builder_01",
            "site-builder-name_01",
            &"a".repeat(32),
        ] {
            validate_spec(&SessionSpec {
                agent_name: agent_name.to_string(),
                ..test_session_spec()
            })
            .unwrap_or_else(|error| panic!("expected {agent_name:?} to be accepted, got {error}"));
        }
    }

    #[test]
    fn validate_agent_name_accepts_valid_unix_agent_names() {
        for agent_name in [
            "site-builder",
            "site-builder-01",
            "site-builder_01",
            "site-builder-name_01",
            &"a".repeat(32),
        ] {
            validate_agent_name(agent_name).unwrap_or_else(|error| {
                panic!("expected {agent_name:?} to be accepted, got {error:?}")
            });
        }
    }

    #[test]
    fn validate_agent_name_rejects_invalid_unix_usernames() {
        for agent_name in [
            "",
            "   ",
            "Site-Builder 01",
            "123site-builder",
            "---",
            "_site-builder",
            "site-builder__name!",
            &format!("a{}", "b".repeat(32)),
        ] {
            let error = validate_agent_name(agent_name)
                .expect_err("invalid unix usernames should be rejected");

            assert_eq!(
                error,
                AgentNameValidationError::Invalid,
                "expected Invalid for {agent_name:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn validate_agent_name_rejects_reserved_names() {
        for agent_name in ["root", "nobody", "daemon", "bin", "sys", "man", "mail"] {
            let error =
                validate_agent_name(agent_name).expect_err("reserved names should be rejected");

            assert_eq!(
                error,
                AgentNameValidationError::Reserved,
                "expected Reserved for {agent_name:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn validate_spec_maps_invalid_or_reserved_agent_names_to_runner_error() {
        for agent_name in ["123site-builder", "root"] {
            let error = validate_spec(&SessionSpec {
                agent_name: agent_name.to_string(),
                ..test_session_spec()
            })
            .expect_err("invalid agent names should be rejected");

            assert!(
                matches!(error, RunnerError::InvalidAgentName),
                "expected InvalidAgentName for {agent_name:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn invalid_agent_name_error_mentions_format_and_reserved_names() {
        let message = RunnerError::InvalidAgentName.to_string();

        assert!(
            message.contains("must already be a unix username"),
            "expected unix username requirement in message, got {message}"
        );
        assert!(
            message.contains("root"),
            "expected reserved-name guidance in message, got {message}"
        );
    }

    #[test]
    fn validate_invocation_accepts_supported_remote_repo_urls() {
        for repo_url in [
            "https://example.com/agentd.git",
            "https://example.com/agentd.git/",
            "http://example.com/agentd.git",
            "http://example.com/agentd.git/",
            "git://example.com/agentd.git",
            "git://example.com/agentd.git/",
            "ssh://git@example.com/agentd.git",
            "ssh://git@example.com:2222/tesserine/agentd.git",
            "git@example.com:tesserine/agentd.git",
            "git@example.com:/srv/git/agentd.git",
        ] {
            validate_invocation(&SessionInvocation {
                repo_url: repo_url.to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            })
            .unwrap_or_else(|error| panic!("expected {repo_url} to be accepted, got {error}"));
        }
    }

    #[test]
    fn validate_invocation_accepts_repo_token_for_https_repo_urls() {
        for repo_url in [
            "https://example.com/private-agentd.git",
            "https://example.com/private-agentd.git/",
        ] {
            validate_invocation(&SessionInvocation {
                repo_url: repo_url.to_string(),
                repo_token: Some("repo-token".to_string()),
                work_unit: None,
                input: None,
                timeout: None,
            })
            .unwrap_or_else(|error| panic!("expected {repo_url} to be accepted, got {error}"));
        }
    }

    #[test]
    fn validate_invocation_rejects_repo_token_for_non_https_repo_urls() {
        for repo_url in [
            "http://example.com/private-agentd.git",
            "git://example.com/private-agentd.git",
            "ssh://git@example.com/private-agentd.git",
            "git@example.com:private-agentd.git",
        ] {
            let error = validate_invocation(&SessionInvocation {
                repo_url: repo_url.to_string(),
                repo_token: Some("repo-token".to_string()),
                work_unit: None,
                input: None,
                timeout: None,
            })
            .expect_err("repo_token should be rejected for non-https repo URLs");

            assert!(
                matches!(error, RunnerError::InvalidRepoUrl { .. }),
                "expected InvalidRepoUrl for {repo_url}, got {error:?}"
            );
            assert!(
                error
                    .to_string()
                    .contains("must use https:// when repo_token is set"),
                "expected repo_token https-only message for {repo_url}, got {error}"
            );
        }
    }

    #[test]
    fn validate_invocation_rejects_non_remote_repo_urls() {
        for repo_url in [
            "",
            " ",
            " repo ",
            "repo",
            "./repo",
            "../repo.git",
            "/srv/test-repo.git",
            "file:///srv/test-repo.git",
            "ftp://example.com/agentd.git",
            "gopher://example.com/agentd.git",
            "https://user:token@example.com/repo.git",
            "https://",
            "http://",
            "git://",
            "https://github.com",
            "http:///repo.git",
            "https://?ref=main",
            "https://#readme",
            "https://example.com/repo.git?token=secret",
            "https://example.com/repo.git#readme",
            "example.com:agentd.git",
            "git@example.com",
            "@example.com:agentd.git",
            "git@:agentd.git",
            "ssh://git@example.com",
            "ssh://git@example.com/repo.git?token=secret",
            "ssh://git@example.com/repo.git#readme",
        ] {
            let error = validate_invocation(&SessionInvocation {
                repo_url: repo_url.to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            })
            .expect_err("non-remote repo URL should be rejected");

            assert!(
                matches!(error, RunnerError::InvalidRepoUrl { .. }),
                "expected InvalidRepoUrl for {repo_url}, got {error:?}"
            );
        }
    }

    #[test]
    fn validate_invocation_rejects_credential_bearing_repo_urls() {
        for repo_url in [
            "https://user:token@example.com/repo.git",
            "ssh://git:token@example.com/repo.git",
            "git:token@example.com:repo.git",
        ] {
            let error = validate_invocation(&SessionInvocation {
                repo_url: repo_url.to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            })
            .expect_err("credential-bearing repo URLs should be rejected");

            let message = error.to_string();
            assert!(
                message.contains("credential-bearing URLs are not accepted"),
                "expected credential-bearing URL rejection message for {repo_url}, got {message}"
            );
            assert!(
                message.contains("#32"),
                "expected credential-bearing URL rejection to reference #32 for {repo_url}, got {message}"
            );
        }
    }

    #[test]
    fn run_session_rejects_invalid_repo_url_before_methodology_validation() {
        let error = crate::run_session(
            SessionSpec {
                methodology_dir: PathBuf::from("/tmp/does-not-exist"),
                ..test_session_spec()
            },
            SessionInvocation {
                repo_url: "/srv/test-repo.git".to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            },
        )
        .expect_err("invalid repo URL should be rejected before setup");

        assert!(
            matches!(error, RunnerError::InvalidRepoUrl { .. }),
            "expected InvalidRepoUrl, got {error:?}"
        );
    }

    #[test]
    fn run_session_rejects_credential_bearing_repo_url_before_methodology_validation() {
        let error = crate::run_session(
            SessionSpec {
                methodology_dir: PathBuf::from("/tmp/does-not-exist"),
                ..test_session_spec()
            },
            SessionInvocation {
                repo_url: "https://user:token@example.com/repo.git".to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            },
        )
        .expect_err("credential-bearing repo URL should be rejected before setup");

        assert!(
            matches!(error, RunnerError::InvalidRepoUrl { .. }),
            "expected InvalidRepoUrl, got {error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("credential-bearing URLs are not accepted until #32 lands"),
            "expected credential-bearing URL message, got {error}"
        );
    }

    #[test]
    fn run_session_rejects_non_https_repo_token_before_methodology_validation() {
        for repo_url in [
            "http://example.com/private-agentd.git",
            "git://example.com/private-agentd.git",
        ] {
            let error = crate::run_session(
                SessionSpec {
                    methodology_dir: PathBuf::from("/tmp/does-not-exist"),
                    ..test_session_spec()
                },
                SessionInvocation {
                    repo_url: repo_url.to_string(),
                    repo_token: Some("repo-token".to_string()),
                    work_unit: None,
                    input: None,
                    timeout: None,
                },
            )
            .expect_err("non-https repo token should be rejected before setup");

            assert!(
                matches!(error, RunnerError::InvalidRepoUrl { .. }),
                "expected InvalidRepoUrl for {repo_url}, got {error:?}"
            );
            assert!(
                error
                    .to_string()
                    .contains("must use https:// when repo_token is set"),
                "expected repo_token https-only message for {repo_url}, got {error}"
            );
        }
    }

    #[test]
    fn validate_invocation_accepts_supported_manual_intent_surfaces() {
        for (work_unit, input) in [
            (None, None),
            (Some("issue-42".to_string()), None),
            (
                None,
                Some(InvocationInput::IntentText {
                    description: "Add a status page".to_string(),
                }),
            ),
            (
                Some("issue-42".to_string()),
                Some(InvocationInput::Artifact {
                    artifact_type: "work-unit".to_string(),
                    artifact_id: "issue-42".to_string(),
                    document: serde_json::json!({ "id": "issue-42" }),
                }),
            ),
        ] {
            validate_invocation(&SessionInvocation {
                repo_url: "https://example.com/agentd.git".to_string(),
                repo_token: None,
                work_unit,
                input,
                timeout: None,
            })
            .expect("supported manual intent surfaces should be accepted");
        }
    }

    #[test]
    fn validate_invocation_rejects_intent_text_with_work_unit() {
        let error = validate_invocation(&SessionInvocation {
            repo_url: "https://example.com/agentd.git".to_string(),
            repo_token: None,
            work_unit: Some("issue-42".to_string()),
            input: Some(InvocationInput::IntentText {
                description: "Add a status page".to_string(),
            }),
            timeout: None,
        })
        .expect_err("conflicting work_unit and input should be rejected");

        assert!(
            matches!(error, RunnerError::InvalidInvocationInput { .. }),
            "expected InvalidInvocationInput, got {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("work_unit"),
            "expected work_unit guidance in message, got {message}"
        );
        assert!(
            message.contains("input"),
            "expected input guidance in message, got {message}"
        );
    }

    #[test]
    fn validate_invocation_rejects_non_work_unit_artifact_with_work_unit() {
        let error = validate_invocation(&SessionInvocation {
            repo_url: "https://example.com/agentd.git".to_string(),
            repo_token: None,
            work_unit: Some("issue-42".to_string()),
            input: Some(InvocationInput::Artifact {
                artifact_type: "claim".to_string(),
                artifact_id: "issue-42".to_string(),
                document: serde_json::json!({ "summary": "Ship it" }),
            }),
            timeout: None,
        })
        .expect_err("work mode should reject non-work-unit artifacts");

        assert!(
            matches!(error, RunnerError::InvalidInvocationInput { .. }),
            "expected InvalidInvocationInput, got {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("artifact type 'work-unit'"),
            "expected work-unit artifact guidance in message, got {message}"
        );
    }

    #[test]
    fn validate_invocation_rejects_mismatched_work_unit_artifact_id() {
        let error = validate_invocation(&SessionInvocation {
            repo_url: "https://example.com/agentd.git".to_string(),
            repo_token: None,
            work_unit: Some("issue-42".to_string()),
            input: Some(InvocationInput::Artifact {
                artifact_type: "work-unit".to_string(),
                artifact_id: "issue-43".to_string(),
                document: serde_json::json!({ "id": "issue-43" }),
            }),
            timeout: None,
        })
        .expect_err("work mode should reject mismatched work-unit artifacts");

        assert!(
            matches!(error, RunnerError::InvalidInvocationInput { .. }),
            "expected InvalidInvocationInput, got {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("artifact id") && message.contains("issue-42"),
            "expected matching-id guidance in message, got {message}"
        );
    }

    #[test]
    fn run_session_rejects_reserved_agent_name_before_methodology_validation() {
        let error = crate::run_session(
            SessionSpec {
                agent_name: "root".to_string(),
                methodology_dir: PathBuf::from("/tmp/does-not-exist"),
                ..test_session_spec()
            },
            SessionInvocation {
                repo_url: "https://example.com/agentd.git".to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            },
        )
        .expect_err("reserved agent name should be rejected before setup");

        assert!(
            matches!(error, RunnerError::InvalidAgentName),
            "expected InvalidAgentName, got {error:?}"
        );
    }
}
