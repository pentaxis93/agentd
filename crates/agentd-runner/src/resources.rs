//! Session resource allocation and cleanup.
//!
//! Resources (staged mount sources, podman secrets) are allocated as a unit by
//! [`prepare_session_resources`] and cleaned up as a unit after the session
//! completes. Partial-failure cleanup during allocation ensures no leaked
//! secrets or stale staging directories when resource creation fails midway.

use crate::audit::SessionAuditRecord;
use crate::input::{INVOCATION_INPUT_MOUNT_PATH, ResolvedInvocationInput};
use crate::lifecycle::{LifecycleFailureKind, log_lifecycle_failure};
use crate::naming::{SESSION_ID_LEN, format_secret_name};
use crate::podman::run_podman_command_with_input;
use crate::session_paths::session_internal_audit_runa_dir;
use crate::types::{RunnerError, SessionInvocation, SessionSpec};
use crate::validation::REPO_TOKEN_ENV;
use getrandom::fill as fill_random_bytes;
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const METHODOLOGY_STAGE_LINK_NAME: &str = "methodology";
const INVOCATION_INPUT_STAGE_DIR_NAME: &str = "invocation-input";
const INVOCATION_INPUT_FILE_NAME: &str = "document.json";
const INVOCATION_INPUT_STAGE_DIR_MODE: u32 = 0o755;
const INVOCATION_INPUT_FILE_MODE: u32 = 0o644;
const AUDIT_RUNA_STAGE_LINK_NAME: &str = "audit-runa";
const ADDITIONAL_MOUNT_STAGE_PREFIX: &str = "mount-";
const SESSION_STAGE_PREFIX: &str = "agentd-session-stage-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SecretBinding {
    pub(crate) secret_name: String,
    pub(crate) target_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedBindMount {
    pub(crate) source: PathBuf,
    pub(crate) target: PathBuf,
    pub(crate) read_only: bool,
    pub(crate) relabel_shared: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionResources {
    pub(crate) container_name: String,
    pub(crate) methodology_staging_dir: PathBuf,
    pub(crate) methodology_mount_source: PathBuf,
    pub(crate) audit_record: SessionAuditRecord,
    pub(crate) audit_mount: PreparedBindMount,
    pub(crate) invocation_input_mount: Option<PreparedBindMount>,
    pub(crate) additional_mounts: Vec<PreparedBindMount>,
    pub(crate) environment_secret_bindings: Vec<SecretBinding>,
    pub(crate) repo_token_secret_binding: Option<SecretBinding>,
}

impl SessionResources {
    pub(crate) fn all_secret_bindings(&self) -> Vec<SecretBinding> {
        let mut bindings = self.environment_secret_bindings.clone();
        if let Some(binding) = &self.repo_token_secret_binding {
            bindings.push(binding.clone());
        }
        bindings
    }
}

#[derive(Debug)]
pub(crate) struct ResourceAllocationFailure {
    pub(crate) allocation_error: RunnerError,
    pub(crate) rollback_result: Result<(), RunnerError>,
}

impl ResourceAllocationFailure {
    fn without_rollback_failure(error: RunnerError) -> Self {
        Self {
            allocation_error: error,
            rollback_result: Ok(()),
        }
    }

    fn with_rollback_result(
        allocation_error: RunnerError,
        rollback_result: Result<(), RunnerError>,
    ) -> Self {
        Self {
            allocation_error,
            rollback_result,
        }
    }
}

pub(crate) fn unique_suffix() -> Result<String, RunnerError> {
    unique_suffix_with(|bytes| fill_random_bytes(bytes).map_err(std::io::Error::other))
}

pub(crate) fn prepare_session_resources(
    container_name: &str,
    spec: &SessionSpec,
    invocation: &SessionInvocation,
    session_id: &str,
    audit_record: SessionAuditRecord,
    resolved_input: Option<&ResolvedInvocationInput>,
) -> Result<SessionResources, ResourceAllocationFailure> {
    let manifest_path = spec.methodology_dir.join("manifest.toml");
    if !manifest_path.is_file() {
        return Err(ResourceAllocationFailure::without_rollback_failure(
            RunnerError::MissingMethodologyManifest {
                path: manifest_path,
            },
        ));
    }

    let methodology_staging_dir = create_methodology_staging_dir(&spec.methodology_dir, session_id)
        .map_err(ResourceAllocationFailure::without_rollback_failure)?;
    let audit_mount = match create_audit_mount(&audit_record, spec, &methodology_staging_dir) {
        Ok(mount) => mount,
        Err(error) => {
            return Err(rollback_failed_staging_dir_allocation(
                container_name,
                session_id,
                &methodology_staging_dir,
                error,
            ));
        }
    };
    let methodology_mount_source = methodology_staging_dir.join(METHODOLOGY_STAGE_LINK_NAME);
    let invocation_input_mount =
        match create_invocation_input_mount(resolved_input, &methodology_staging_dir) {
            Ok(mount) => mount,
            Err(error) => {
                return Err(rollback_failed_staging_dir_allocation(
                    container_name,
                    session_id,
                    &methodology_staging_dir,
                    error,
                ));
            }
        };
    let additional_mounts = match create_additional_mounts(&spec.mounts, &methodology_staging_dir) {
        Ok(mounts) => mounts,
        Err(error) => {
            return Err(rollback_failed_staging_dir_allocation(
                container_name,
                session_id,
                &methodology_staging_dir,
                error,
            ));
        }
    };
    let mut resources = SessionResources {
        container_name: container_name.to_string(),
        methodology_staging_dir,
        methodology_mount_source,
        audit_record,
        audit_mount,
        invocation_input_mount,
        additional_mounts,
        environment_secret_bindings: Vec::new(),
        repo_token_secret_binding: None,
    };

    for (index, variable) in spec.environment.iter().enumerate() {
        if variable.value.is_empty() {
            continue;
        }

        let secret_name =
            format_secret_name(&spec.daemon_instance_id, session_id, &index.to_string());
        if let Err(error) = create_podman_secret(&secret_name, &variable.value) {
            return Err(rollback_failed_resource_allocation(
                &resources, session_id, error,
            ));
        }
        resources.environment_secret_bindings.push(SecretBinding {
            secret_name,
            target_name: variable.name.clone(),
        });
    }

    if let Some(repo_token) = invocation
        .repo_token
        .as_deref()
        .filter(|token| !token.is_empty())
    {
        let secret_name = format_secret_name(&spec.daemon_instance_id, session_id, "repo-token");
        if let Err(error) = create_podman_secret(&secret_name, repo_token) {
            return Err(rollback_failed_resource_allocation(
                &resources, session_id, error,
            ));
        }
        resources.repo_token_secret_binding = Some(SecretBinding {
            secret_name,
            target_name: REPO_TOKEN_ENV.to_string(),
        });
    }

    Ok(resources)
}

fn rollback_failed_resource_allocation(
    resources: &SessionResources,
    session_id: &str,
    error: RunnerError,
) -> ResourceAllocationFailure {
    let mut rollback_error = None;

    if let Err(cleanup_error) = cleanup_podman_secrets(&resources.all_secret_bindings()) {
        log_lifecycle_failure(
            LifecycleFailureKind::Cleanup,
            "session resource allocation",
            &resources.container_name,
            session_id,
            &cleanup_error,
        );
        rollback_error = Some(cleanup_error);
    }
    if let Err(cleanup_error) = cleanup_methodology_staging_dir(&resources.methodology_staging_dir)
    {
        log_lifecycle_failure(
            LifecycleFailureKind::Cleanup,
            "session resource allocation",
            &resources.container_name,
            session_id,
            &cleanup_error,
        );
        rollback_error.get_or_insert(cleanup_error);
    }

    ResourceAllocationFailure::with_rollback_result(error, rollback_error.map_or(Ok(()), Err))
}

fn rollback_failed_staging_dir_allocation(
    container_name: &str,
    session_id: &str,
    staging_dir: &Path,
    error: RunnerError,
) -> ResourceAllocationFailure {
    let rollback_result = cleanup_methodology_staging_dir(staging_dir);
    if let Err(cleanup_error) = &rollback_result {
        log_lifecycle_failure(
            LifecycleFailureKind::Cleanup,
            "session resource allocation",
            container_name,
            session_id,
            cleanup_error,
        );
    }

    ResourceAllocationFailure::with_rollback_result(error, rollback_result)
}

pub(crate) fn cleanup_podman_secrets(secret_bindings: &[SecretBinding]) -> Result<(), RunnerError> {
    let secret_names = secret_bindings
        .iter()
        .map(|binding| binding.secret_name.clone())
        .collect::<Vec<_>>();
    cleanup_podman_secret_names(&secret_names)
}

pub(crate) fn cleanup_podman_secret_names(secret_names: &[String]) -> Result<(), RunnerError> {
    if secret_names.is_empty() {
        return Ok(());
    }

    let existing_secret_names = list_podman_secret_names()?;
    let mut args = vec!["secret".to_string(), "rm".to_string()];
    args.extend(
        secret_names
            .iter()
            .filter(|name| existing_secret_names.contains(name.as_str()))
            .cloned(),
    );
    if args.len() == 2 {
        return Ok(());
    }

    crate::podman::run_podman_command(args).map(|_| ())
}

fn list_podman_secret_names() -> Result<HashSet<String>, RunnerError> {
    Ok(crate::podman::run_podman_command(vec![
        "secret".to_string(),
        "ls".to_string(),
        "--format".to_string(),
        "{{.Name}}".to_string(),
    ])?
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .map(ToString::to_string)
    .collect())
}

pub(crate) fn cleanup_methodology_staging_dir(path: &Path) -> Result<(), RunnerError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RunnerError::Io(error)),
    }
}

// Creates a staging directory containing a symlink to the canonical methodology
// path. The symlink indirection ensures the podman bind-mount source is always
// an absolute, canonical path free of characters that break mount syntax, even
// when the original methodology_dir is relative or contains problematic chars.
fn create_methodology_staging_dir(
    methodology_dir: &Path,
    session_id: &str,
) -> Result<PathBuf, RunnerError> {
    let canonical_methodology_dir = methodology_dir.canonicalize()?;
    let staging_dir = safe_staging_root().join(format!("{SESSION_STAGE_PREFIX}{session_id}"));
    fs::create_dir_all(&staging_dir)?;
    let staged_link = staging_dir.join(METHODOLOGY_STAGE_LINK_NAME);

    if let Err(error) = create_path_symlink(&canonical_methodology_dir, &staged_link) {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(error);
    }

    Ok(staging_dir)
}

fn create_additional_mounts(
    mounts: &[crate::BindMount],
    staging_dir: &Path,
) -> Result<Vec<PreparedBindMount>, RunnerError> {
    let mut prepared_mounts = Vec::with_capacity(mounts.len());

    for (index, mount) in mounts.iter().enumerate() {
        prepared_mounts.push(create_prepared_bind_mount(
            &mount.source,
            staging_dir.join(format!("{ADDITIONAL_MOUNT_STAGE_PREFIX}{index}")),
            mount.target.clone(),
            mount.read_only,
            false,
        )?);
    }

    Ok(prepared_mounts)
}

fn create_audit_mount(
    audit_record: &SessionAuditRecord,
    spec: &SessionSpec,
    staging_dir: &Path,
) -> Result<PreparedBindMount, RunnerError> {
    create_prepared_bind_mount(
        &audit_record.runa_dir,
        staging_dir.join(AUDIT_RUNA_STAGE_LINK_NAME),
        session_internal_audit_runa_dir(&spec.agent_name),
        false,
        true,
    )
}

fn create_invocation_input_mount(
    resolved_input: Option<&ResolvedInvocationInput>,
    staging_dir: &Path,
) -> Result<Option<PreparedBindMount>, RunnerError> {
    let Some(resolved_input) = resolved_input else {
        return Ok(None);
    };

    let staged_dir = staging_dir.join(INVOCATION_INPUT_STAGE_DIR_NAME);
    fs::create_dir_all(&staged_dir)?;
    fs::set_permissions(
        &staged_dir,
        fs::Permissions::from_mode(INVOCATION_INPUT_STAGE_DIR_MODE),
    )?;
    let staged_document_path = staged_dir.join(INVOCATION_INPUT_FILE_NAME);
    fs::write(&staged_document_path, &resolved_input.document_json)?;
    fs::set_permissions(
        &staged_document_path,
        fs::Permissions::from_mode(INVOCATION_INPUT_FILE_MODE),
    )?;

    Ok(Some(PreparedBindMount {
        source: staged_dir,
        target: PathBuf::from(INVOCATION_INPUT_MOUNT_PATH),
        read_only: true,
        relabel_shared: true,
    }))
}

fn create_prepared_bind_mount(
    source: &Path,
    staged_source: PathBuf,
    target: PathBuf,
    read_only: bool,
    relabel_shared: bool,
) -> Result<PreparedBindMount, RunnerError> {
    let canonical_source = source.canonicalize().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => RunnerError::MissingMountSource {
            path: source.to_path_buf(),
        },
        _ => RunnerError::Io(error),
    })?;
    create_path_symlink(&canonical_source, &staged_source)?;
    Ok(PreparedBindMount {
        source: staged_source,
        target,
        read_only,
        relabel_shared,
    })
}

// Returns a base directory for session staging that is safe for podman
// bind-mount syntax. On systems where `std::env::temp_dir()` returns a path
// containing commas (e.g., certain Nix or sandbox environments), the comma
// breaks podman's comma-delimited mount option parsing. Falls back to `/tmp`
// on unix in that case.
fn safe_staging_root() -> PathBuf {
    let temp_dir = std::env::temp_dir();
    if !path_requires_mount_staging_alias(&temp_dir) {
        return temp_dir;
    }

    PathBuf::from("/tmp")
}

fn path_requires_mount_staging_alias(path: &Path) -> bool {
    path.to_string_lossy().contains(',')
}

fn create_podman_secret(secret_name: &str, value: &str) -> Result<(), RunnerError> {
    run_podman_command_with_input(
        vec![
            "secret".to_string(),
            "create".to_string(),
            secret_name.to_string(),
            "-".to_string(),
        ],
        Some(value.as_bytes()),
    )
    .map(|_| ())
}

fn create_path_symlink(source: &Path, destination: &Path) -> Result<(), RunnerError> {
    std::os::unix::fs::symlink(source, destination).map_err(RunnerError::Io)
}

// Generates a 16-character hex string from 8 bytes of cryptographic randomness.
// Used to create unique container names and secret names that are unpredictable
// across sessions, preventing name collisions and name-guessing attacks.
fn unique_suffix_with<F>(fill_random: F) -> Result<String, RunnerError>
where
    F: FnOnce(&mut [u8]) -> std::io::Result<()>,
{
    let mut bytes = [0_u8; SESSION_ID_LEN / 2];
    fill_random(&mut bytes)?;
    Ok(hex_encode(&bytes))
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

#[cfg(test)]
mod tests {
    use super::{
        INVOCATION_INPUT_FILE_NAME, INVOCATION_INPUT_STAGE_DIR_NAME, create_invocation_input_mount,
        prepare_session_resources, rollback_failed_staging_dir_allocation, unique_suffix_with,
    };
    use crate::audit::SessionAuditRecord;
    use crate::input::{INVOCATION_INPUT_MOUNT_PATH, ResolvedInvocationInput};
    use crate::test_support::{
        CommandBehavior, CommandOutcome, FakePodmanFixture, FakePodmanScenario,
        capture_tracing_events, fake_podman_lock, test_session_spec,
    };
    use crate::{BindMount, ResolvedEnvironmentVariable, RunnerError, SessionInvocation};
    use std::cell::RefCell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn test_audit_record(session_id: &str) -> SessionAuditRecord {
        let record_dir = std::env::temp_dir().join(format!("agentd-audit-record-{session_id}"));
        let runa_dir = record_dir.join("runa");
        let metadata_path = record_dir.join("agentd/session.json");
        std::fs::create_dir_all(&runa_dir).expect("test runa dir should be created");
        std::fs::create_dir_all(
            metadata_path
                .parent()
                .expect("metadata path should have a parent"),
        )
        .expect("test metadata dir should be created");

        SessionAuditRecord {
            record_dir,
            runa_dir,
            metadata_path,
            session_id: session_id.to_string(),
            agent: "site-builder".to_string(),
            repo_url: "https://example.com/repo.git".to_string(),
            work_unit: None,
            start_timestamp: "2026-04-16T00:00:00Z".to_string(),
        }
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after the unix epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn unique_suffix_with_encodes_eight_random_bytes_as_sixteen_lower_hex_characters() {
        let suffix = unique_suffix_with(|bytes| {
            bytes.copy_from_slice(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
            Ok(())
        })
        .expect("suffix generation should succeed");

        assert_eq!(suffix, "0123456789abcdef");
        assert_eq!(suffix.len(), 16);
        assert!(
            suffix
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
            "session suffix should be lowercase hex: {suffix}"
        );
    }

    #[test]
    fn rollback_failed_staging_dir_allocation_returns_ok_rollback_when_cleanup_succeeds() {
        let root = unique_test_dir("agentd-staging-rollback-ok");
        let staging_dir = root.join("staging");
        fs::create_dir_all(&staging_dir).expect("staging dir should be created");
        fs::write(staging_dir.join("mounted"), "test\n").expect("staging child should be created");

        let failure = rollback_failed_staging_dir_allocation(
            "agentd-agent-session",
            "session-ok",
            &staging_dir,
            RunnerError::MissingMountSource {
                path: PathBuf::from("/missing"),
            },
        );

        assert!(
            matches!(
                failure.allocation_error,
                RunnerError::MissingMountSource { ref path } if path == &PathBuf::from("/missing")
            ),
            "original allocation error should be preserved: {:?}",
            failure.allocation_error
        );
        assert!(
            failure.rollback_result.is_ok(),
            "successful rollback should report Ok(()): {:?}",
            failure.rollback_result
        );
        assert!(
            !staging_dir.exists(),
            "successful rollback should remove the staging dir"
        );

        fs::remove_dir_all(&root).expect("temporary rollback root should be removed");
    }

    #[test]
    fn rollback_failed_staging_dir_allocation_returns_cleanup_error_and_logs_failure() {
        let root = unique_test_dir("agentd-staging-rollback-error");
        let staging_dir = root.join("staging");
        fs::create_dir_all(&staging_dir).expect("staging dir should be created");
        fs::write(staging_dir.join("mounted"), "test\n").expect("staging child should be created");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555))
            .expect("rollback parent should become read-only");

        let failure = RefCell::new(None);
        let events = capture_tracing_events(|| {
            failure.replace(Some(rollback_failed_staging_dir_allocation(
                "agentd-agent-session",
                "session-error",
                &staging_dir,
                RunnerError::MissingMountSource {
                    path: PathBuf::from("/missing"),
                },
            )));
        });

        let failure = failure
            .into_inner()
            .expect("staging rollback failure should be captured");
        assert!(
            matches!(
                failure.allocation_error,
                RunnerError::MissingMountSource { ref path } if path == &PathBuf::from("/missing")
            ),
            "original allocation error should be preserved: {:?}",
            failure.allocation_error
        );
        match failure.rollback_result {
            Err(RunnerError::Io(error)) => {
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected permission denied rollback failure, got {other:?}"),
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["event"], "runner.lifecycle_failure");
        assert_eq!(events[0]["fields"]["stage"], "session resource allocation");

        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))
            .expect("rollback parent should become writable for cleanup");
        fs::remove_dir_all(&root).expect("temporary rollback root should be removed");
    }

    #[test]
    fn prepare_session_resources_logs_cleanup_failure_after_environment_secret_create_failure() {
        let events = capture_tracing_events(run_environment_secret_create_failure_scenario);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["event"], "runner.lifecycle_failure");
        assert_eq!(events[0]["fields"]["stage"], "session resource allocation");
        assert!(
            events[0]["fields"]["error"]
                .as_str()
                .expect("error field should be a string")
                .contains("secret cleanup failed after create failure"),
            "expected cleanup failure detail, got: {}",
            events[0]["fields"]["error"]
        );
    }

    #[test]
    fn prepare_session_resources_logs_cleanup_failure_after_repo_token_secret_create_failure() {
        let events = capture_tracing_events(run_repo_token_secret_create_failure_scenario);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["event"], "runner.lifecycle_failure");
        assert_eq!(events[0]["fields"]["stage"], "session resource allocation");
        assert!(
            events[0]["fields"]["error"]
                .as_str()
                .expect("error field should be a string")
                .contains("secret cleanup failed after repo token create failure"),
            "expected cleanup failure detail, got: {}",
            events[0]["fields"]["error"]
        );
    }

    fn run_environment_secret_create_failure_scenario() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new()
                .with_secret_create(CommandBehavior::sequence(vec![
                    CommandOutcome::new()
                        .append_args_with_prefix("secret-commands.log", "create")
                        .capture_stdin_to("secret-value.log")
                        .track_secret_create(),
                    CommandOutcome::new()
                        .append_args_with_prefix("secret-commands.log", "create")
                        .stderr("secret create failed")
                        .exit_code(37),
                ]))
                .with_secret_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new()
                        .append_args_with_prefix("secret-commands.log", "rm")
                        .stderr("secret cleanup failed after create failure")
                        .exit_code(41),
                )),
        );

        let methodology_dir = fixture.create_methodology_dir("runner-methodology");
        let session_id = format!("session-env-{}", std::process::id());
        let result = fixture.run_with_fake_podman_env(|| {
            prepare_session_resources(
                "agentd-agent-session",
                &crate::SessionSpec {
                    methodology_dir,
                    environment: vec![
                        ResolvedEnvironmentVariable {
                            name: "GITHUB_TOKEN".to_string(),
                            value: "test-token".to_string(),
                        },
                        ResolvedEnvironmentVariable {
                            name: "SECOND_SECRET".to_string(),
                            value: "second-token".to_string(),
                        },
                    ],
                    ..test_session_spec()
                },
                &SessionInvocation {
                    repo_url: "https://example.com/repo.git".to_string(),
                    repo_token: None,
                    work_unit: None,
                    input: None,
                    timeout: None,
                },
                &session_id,
                test_audit_record(&session_id),
                None,
            )
        });

        match result
            .expect_err("second secret create should fail")
            .allocation_error
        {
            RunnerError::PodmanCommandFailed {
                args,
                status,
                stderr,
            } => {
                assert_eq!(
                    args,
                    vec![
                        "secret".to_string(),
                        "create".to_string(),
                        format!(
                            "agentd-{}-{session_id}-1",
                            test_session_spec().daemon_instance_id
                        ),
                        "-".to_string(),
                    ]
                );
                assert_eq!(status.code(), Some(37));
                assert_eq!(stderr.trim(), "secret create failed");
            }
            other => panic!("expected PodmanCommandFailed, got {other:?}"),
        }
    }

    fn run_repo_token_secret_create_failure_scenario() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new()
                .with_secret_create(CommandBehavior::sequence(vec![
                    CommandOutcome::new()
                        .append_args_with_prefix("secret-commands.log", "create")
                        .capture_stdin_to("secret-value.log")
                        .track_secret_create(),
                    CommandOutcome::new()
                        .append_args_with_prefix("secret-commands.log", "create")
                        .stderr("repo token secret create failed")
                        .exit_code(43),
                ]))
                .with_secret_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new()
                        .append_args_with_prefix("secret-commands.log", "rm")
                        .stderr("secret cleanup failed after repo token create failure")
                        .exit_code(47),
                )),
        );

        let methodology_dir = fixture.create_methodology_dir("runner-methodology");
        let session_id = format!("session-repo-token-{}", std::process::id());
        let result = fixture.run_with_fake_podman_env(|| {
            prepare_session_resources(
                "agentd-agent-session",
                &crate::SessionSpec {
                    methodology_dir,
                    environment: vec![ResolvedEnvironmentVariable {
                        name: "GITHUB_TOKEN".to_string(),
                        value: "test-token".to_string(),
                    }],
                    ..test_session_spec()
                },
                &SessionInvocation {
                    repo_url: "https://example.com/repo.git".to_string(),
                    repo_token: Some("repo-token".to_string()),
                    work_unit: None,
                    input: None,
                    timeout: None,
                },
                &session_id,
                test_audit_record(&session_id),
                None,
            )
        });

        match result
            .expect_err("repo token secret create should fail")
            .allocation_error
        {
            RunnerError::PodmanCommandFailed {
                args,
                status,
                stderr,
            } => {
                assert_eq!(
                    args,
                    vec![
                        "secret".to_string(),
                        "create".to_string(),
                        format!(
                            "agentd-{}-{session_id}-repo-token",
                            test_session_spec().daemon_instance_id
                        ),
                        "-".to_string(),
                    ]
                );
                assert_eq!(status.code(), Some(43));
                assert_eq!(stderr.trim(), "repo token secret create failed");
            }
            other => panic!("expected PodmanCommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn prepare_session_resources_rejects_missing_additional_mount_sources() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        let methodology_dir = fixture.create_methodology_dir("runner-methodology");
        let missing_mount_source = std::env::temp_dir().join(format!(
            "agentd-runner-missing-mount-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after the unix epoch")
                .as_nanos()
        ));
        let session_id = format!("session-missing-mount-{}", std::process::id());

        let result = prepare_session_resources(
            "agentd-agent-session",
            &crate::SessionSpec {
                methodology_dir,
                mounts: vec![BindMount {
                    source: missing_mount_source.clone(),
                    target: PathBuf::from("/mnt/readonly"),
                    read_only: true,
                }],
                ..test_session_spec()
            },
            &SessionInvocation {
                repo_url: "https://example.com/repo.git".to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            },
            &session_id,
            test_audit_record(&session_id),
            None,
        );

        match result
            .expect_err("missing mount sources should be rejected")
            .allocation_error
        {
            RunnerError::MissingMountSource { path } => {
                assert_eq!(path, missing_mount_source);
            }
            other => panic!("expected MissingMountSource, got {other:?}"),
        }
    }

    #[test]
    fn create_invocation_input_mount_repairs_runner_created_stage_modes() {
        let staging_dir = unique_test_dir("agentd-invocation-input-stage");
        let invocation_input_dir = staging_dir.join(INVOCATION_INPUT_STAGE_DIR_NAME);
        let document_path = invocation_input_dir.join(INVOCATION_INPUT_FILE_NAME);
        fs::create_dir_all(&invocation_input_dir).expect("staged input dir should be created");
        fs::set_permissions(&invocation_input_dir, fs::Permissions::from_mode(0o700))
            .expect("staged input dir should become restrictive");
        fs::write(&document_path, "stale\n").expect("staged input file should be written");
        fs::set_permissions(&document_path, fs::Permissions::from_mode(0o600))
            .expect("staged input file should become restrictive");

        let mount = create_invocation_input_mount(
            Some(&ResolvedInvocationInput {
                artifact_type: "request".to_string(),
                artifact_id: "operator-input".to_string(),
                document_json: "{\"description\":\"Add a status page\",\"source\":\"operator\"}\n"
                    .to_string(),
            }),
            &staging_dir,
        )
        .expect("invocation input mount should be created")
        .expect("resolved input should create a mount");

        assert_eq!(
            fs::metadata(&invocation_input_dir)
                .expect("staged input dir metadata should exist")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&document_path)
                .expect("staged input document metadata should exist")
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            fs::read_to_string(&document_path).expect("staged input document should be readable"),
            "{\"description\":\"Add a status page\",\"source\":\"operator\"}\n"
        );
        assert_eq!(mount.source, invocation_input_dir);
        assert_eq!(mount.target, PathBuf::from(INVOCATION_INPUT_MOUNT_PATH));
        assert!(mount.read_only);
        assert!(mount.relabel_shared);

        fs::remove_dir_all(&staging_dir).expect("temporary staging dir should be removed");
    }
}
