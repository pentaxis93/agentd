//! Session lifecycle management for agentd.
//!
//! Owns the four phases of a session: input validation, resource allocation
//! (mount staging, podman secrets), container execution (create, start in
//! attached mode, supervise), and teardown (force-remove container, release
//! secrets, remove staging directory). The public entry point is
//! [`run_session`], which accepts a [`SessionSpec`] and
//! [`SessionInvocation`] and returns a [`SessionOutcome`] or
//! [`RunnerError`].
//!
//! See `ARCHITECTURE.md` section "Session Lifecycle" for the design-level
//! treatment of these phases.

mod audit;
mod container;
mod input;
mod lifecycle;
mod naming;
mod podman;
mod reconcile;
mod resources;
mod session_paths;
mod types;
mod validation;

#[cfg(test)]
pub(crate) mod test_support;

pub use reconcile::reconcile_startup_resources;
pub use types::{
    AgentNameValidationError, BindMount, EnvironmentNameValidationError, InvocationInput,
    MountOverlapError, MountTargetValidationError, ResolvedEnvironmentVariable, RunnerError,
    SessionInvocation, SessionOutcome, SessionSpec, StartupReconciliationReport,
};
pub use validation::{
    validate_agent_name, validate_environment_name, validate_mount_overlap, validate_mount_target,
    validate_repo_url,
};

use audit::{
    SessionAuditCompletion, finalize_session_audit_record, finalize_session_transcript,
    prepare_session_audit_record,
};
use container::{create_container, run_container_to_completion, run_container_with_timeout};
use input::{resolve_invocation_input, resolve_work_unit_seed_input};
use lifecycle::{
    LifecycleFailureKind, log_lifecycle_failure, log_session_error, log_session_outcome,
    log_session_started, log_session_teardown,
};
use naming::format_container_name;
use resources::{
    ResourceAllocationFailure, SessionResources, cleanup_methodology_staging_dir,
    cleanup_podman_secrets, prepare_session_resources, unique_suffix,
};
use validation::{validate_invocation, validate_spec};

/// Executes a single session from validation through teardown.
///
/// Validates `spec` and `invocation`, allocates session resources (mount
/// staging directory, podman secrets for non-empty environment values), creates
/// and runs an ephemeral podman container, then cleans up all resources
/// regardless of outcome.
///
/// Returns a semantic [`SessionOutcome`] interpreted from the container exit
/// code according to the shared commons contract, or
/// [`SessionOutcome::TimedOut`] when the optional timeout fires. Returns
/// [`RunnerError`] for validation failures, I/O errors, or podman command
/// failures before a terminal session outcome can be established.
pub fn run_session(
    spec: SessionSpec,
    invocation: SessionInvocation,
) -> Result<SessionOutcome, RunnerError> {
    validate_spec(&spec)?;
    validate_invocation(&invocation)?;
    let resolved_input =
        resolve_invocation_input(&spec.methodology_dir, invocation.input.as_ref())?;
    if let Some(work_unit) = invocation.work_unit.as_deref() {
        resolve_work_unit_seed_input(&spec.methodology_dir, work_unit)?;
    }
    let session_id = unique_suffix()?;

    let container_name =
        format_container_name(&spec.daemon_instance_id, &spec.agent_name, &session_id);
    log_session_started(
        &session_id,
        &container_name,
        &spec.agent_name,
        invocation.work_unit.is_some(),
        invocation.timeout,
    );

    let audit_record = match prepare_session_audit_record(&session_id, &spec, &invocation) {
        Ok(record) => record,
        Err(error) => {
            log_session_error(
                &session_id,
                &container_name,
                "session_audit_allocation",
                &error,
            );
            tracing::info!(
                event = "runner.session_teardown",
                session_id = session_id,
                container_name = container_name,
                result = "skipped",
                "runner session teardown skipped"
            );
            return Err(error);
        }
    };

    let resources = match prepare_session_resources(
        &container_name,
        &spec,
        &invocation,
        &session_id,
        audit_record.clone(),
        resolved_input.as_ref(),
    ) {
        Ok(resources) => resources,
        Err(ResourceAllocationFailure {
            allocation_error,
            rollback_result,
        }) => {
            let audit_result = finalize_session_audit_record_if_cleanup_succeeded(
                &rollback_result,
                &audit_record,
                SessionAuditCompletion::Error,
            );
            if let Err(audit_error) = &audit_result {
                log_lifecycle_failure(
                    LifecycleFailureKind::Cleanup,
                    "session audit finalization",
                    &container_name,
                    &session_id,
                    audit_error,
                );
            }
            log_session_error(
                &session_id,
                &container_name,
                "session_resource_allocation",
                &allocation_error,
            );
            log_session_teardown(
                &session_id,
                &container_name,
                combined_teardown_result(&rollback_result, &audit_result),
            );
            return Err(allocation_error);
        }
    };

    if let Err(error) = create_container(&resources, &spec, &invocation, resolved_input.as_ref()) {
        let cleanup_result = cleanup_session_resources(&resources);
        let audit_result = finalize_session_audit_record_if_cleanup_succeeded(
            &cleanup_result,
            &resources.audit_record,
            SessionAuditCompletion::Error,
        );
        if let Err(cleanup_error) = &cleanup_result {
            log_lifecycle_failure(
                LifecycleFailureKind::Cleanup,
                "container creation",
                &resources.container_name,
                &session_id,
                cleanup_error,
            );
        }
        if let Err(audit_error) = &audit_result {
            log_lifecycle_failure(
                LifecycleFailureKind::Cleanup,
                "session audit finalization",
                &resources.container_name,
                &session_id,
                audit_error,
            );
        }
        log_session_error(
            &session_id,
            &resources.container_name,
            "container_creation",
            &error,
        );
        log_session_teardown(
            &session_id,
            &resources.container_name,
            combined_teardown_result(&cleanup_result, &audit_result),
        );
        return Err(error);
    }

    let secret_bindings = resources.all_secret_bindings();
    let start_result = match invocation.timeout {
        Some(timeout) => run_container_with_timeout(
            &resources.container_name,
            &session_id,
            &secret_bindings,
            timeout,
        ),
        None => {
            run_container_to_completion(&resources.container_name, &session_id, &secret_bindings)
        }
    };

    match &start_result {
        Ok(outcome) => log_session_outcome(&session_id, &resources.container_name, outcome),
        Err(error) => log_session_error(
            &session_id,
            &resources.container_name,
            "session_execution",
            error,
        ),
    }

    let cleanup_result = cleanup_session_resources(&resources);
    let audit_result = finalize_session_audit_record_if_cleanup_succeeded(
        &cleanup_result,
        &resources.audit_record,
        match &start_result {
            Ok(outcome) => SessionAuditCompletion::Outcome(outcome),
            Err(_) => SessionAuditCompletion::Error,
        },
    );
    if let Err(cleanup_error) = &cleanup_result {
        log_lifecycle_failure(
            LifecycleFailureKind::Cleanup,
            "session execution",
            &resources.container_name,
            &session_id,
            cleanup_error,
        );
    }
    if let Err(audit_error) = &audit_result {
        log_lifecycle_failure(
            LifecycleFailureKind::Cleanup,
            "session audit finalization",
            &resources.container_name,
            &session_id,
            audit_error,
        );
    }
    let teardown_result = combined_teardown_result(&cleanup_result, &audit_result);
    log_session_teardown(&session_id, &resources.container_name, teardown_result);

    match (start_result, cleanup_result) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), _) => Err(error),
    }
}

fn cleanup_session_resources(resources: &SessionResources) -> Result<(), RunnerError> {
    let container_result = container::cleanup_container(&resources.container_name);
    let secret_bindings = resources.all_secret_bindings();
    let secret_result = cleanup_podman_secrets(&secret_bindings);
    let staging_result = cleanup_methodology_staging_dir(&resources.methodology_staging_dir);

    container_result?;
    secret_result?;
    staging_result
}

fn finalize_session_audit_record_if_cleanup_succeeded(
    cleanup_result: &Result<(), RunnerError>,
    record: &audit::SessionAuditRecord,
    completion: SessionAuditCompletion<'_>,
) -> Result<(), RunnerError> {
    if cleanup_result.is_err() {
        return Ok(());
    }

    finalize_session_transcript(record)?;
    finalize_session_audit_record(record, completion)
}

fn combined_teardown_result<'a>(
    cleanup_result: &'a Result<(), RunnerError>,
    audit_result: &'a Result<(), RunnerError>,
) -> Result<(), &'a RunnerError> {
    match (cleanup_result, audit_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(_)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        CommandBehavior, CommandOutcome, FakePodmanFixture, FakePodmanScenario,
        capture_tracing_events, fake_podman_lock, fake_podman_ps_json, test_session_spec,
        unique_temp_dir,
    };
    use serde_json::Value;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{Duration, Instant};

    fn only_session_record_dir(audit_root: &Path, agent_name: &str) -> PathBuf {
        let agent_root = audit_root.join(agent_name);
        let entries = fs::read_dir(&agent_root)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", agent_root.display()))
            .map(|entry| {
                entry
                    .expect("session record entry should be readable")
                    .path()
            })
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one session record under {}",
            agent_root.display()
        );
        entries[0].clone()
    }

    fn read_session_metadata(record_dir: &Path) -> Value {
        serde_json::from_str(
            &fs::read_to_string(record_dir.join("agentd/session.json"))
                .expect("session metadata should be readable"),
        )
        .expect("session metadata should be valid json")
    }

    fn make_tree_writable(path: &Path) {
        let metadata = fs::symlink_metadata(path).expect("path metadata should exist");
        if metadata.file_type().is_symlink() {
            return;
        }

        if metadata.is_dir() {
            for entry in fs::read_dir(path).expect("directory should be readable") {
                let entry = entry.expect("directory entry should be readable");
                make_tree_writable(&entry.path());
            }
        }

        let writable_mode = metadata.permissions().mode() | 0o700;
        fs::set_permissions(path, fs::Permissions::from_mode(writable_mode))
            .expect("path should become writable for cleanup");
    }

    const TEST_DAEMON_INSTANCE_ID: &str = "1a2b3c4d";

    fn reconcile_startup_resources_for_tests() -> Result<StartupReconciliationReport, RunnerError> {
        reconcile_startup_resources(TEST_DAEMON_INSTANCE_ID)
    }

    #[test]
    fn run_session_emits_start_outcome_and_teardown_events() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(&FakePodmanScenario::new());
        let methodology_dir = fixture.create_methodology_dir("runner-methodology");

        let events = capture_tracing_events(|| {
            let outcome = fixture.run_with_fake_podman(SessionSpec {
                methodology_dir,
                ..test_session_spec()
            });
            assert_eq!(
                outcome.expect("session should succeed"),
                SessionOutcome::Success { exit_code: 0 }
            );
        });

        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["fields"]["event"], "runner.session_started");
        assert_eq!(events[1]["fields"]["event"], "runner.session_outcome");
        assert_eq!(events[1]["fields"]["outcome"], "success");
        assert_eq!(events[1]["fields"]["exit_code"], 0);
        assert_eq!(events[2]["fields"]["event"], "runner.session_teardown");
        assert_eq!(events[2]["fields"]["result"], "ok");
    }

    #[test]
    fn run_session_emits_a_durability_warning_without_reporting_audit_finalization_failure() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(&FakePodmanScenario::new());
        let methodology_dir = fixture.create_methodology_dir("runner-methodology");
        let audit_root = unique_temp_dir("runner-audit-post-rename-sync-warning");
        fs::create_dir_all(&audit_root).expect("audit root should be created");

        let events = capture_tracing_events(|| {
            let outcome = audit::with_sync_parent_dir_failure_on_call_for_tests(2, || {
                fixture.run_with_fake_podman(SessionSpec {
                    audit_root: audit_root.clone(),
                    methodology_dir,
                    ..test_session_spec()
                })
            });

            assert_eq!(
                outcome.expect("session should succeed despite durability warning"),
                SessionOutcome::Success { exit_code: 0 }
            );
        });

        assert_eq!(events.len(), 4);
        assert_eq!(events[0]["fields"]["event"], "runner.session_started");
        assert_eq!(events[1]["fields"]["event"], "runner.session_outcome");
        assert_eq!(events[1]["fields"]["outcome"], "success");
        assert_eq!(events[2]["fields"]["event"], "runner.audit_warning");
        assert_eq!(
            events[2]["fields"]["warning_kind"],
            "post_rename_parent_sync"
        );
        assert_eq!(events[3]["fields"]["event"], "runner.session_teardown");
        assert_eq!(events[3]["fields"]["result"], "ok");
        assert!(
            events
                .iter()
                .all(|event| event["fields"]["event"] != "runner.lifecycle_failure"),
            "post-rename parent sync failures must not be reported as lifecycle failures"
        );

        let record_dir = only_session_record_dir(&audit_root, "site-builder");
        let metadata = read_session_metadata(&record_dir);
        assert!(
            metadata["end_timestamp"].is_string(),
            "durability warnings must still leave finalized metadata on disk"
        );
        assert_eq!(metadata["outcome"], "success");

        make_tree_writable(&audit_root);
        fs::remove_dir_all(&audit_root).expect("temporary audit root should be removed");
    }

    #[test]
    fn run_session_emits_teardown_after_resource_allocation_failure() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        let methodology_dir = unique_temp_dir("runner-missing-manifest");
        fs::create_dir_all(&methodology_dir).expect("methodology directory should be created");

        let events = fixture.run_with_fake_podman_env(|| {
            capture_tracing_events(|| {
                let error = run_session(
                    SessionSpec {
                        methodology_dir: methodology_dir.clone(),
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
                .expect_err("session should fail during resource allocation");

                assert!(
                    matches!(error, RunnerError::MissingMethodologyManifest { .. }),
                    "expected missing manifest error, got {error:?}"
                );
            })
        });

        fs::remove_dir_all(&methodology_dir)
            .expect("temporary methodology directory should be removed");

        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["fields"]["event"], "runner.session_started");
        assert_eq!(events[1]["fields"]["event"], "runner.session_error");
        assert_eq!(events[1]["fields"]["stage"], "session_resource_allocation");
        assert_eq!(events[2]["fields"]["event"], "runner.session_teardown");
        assert_eq!(events[2]["fields"]["result"], "ok");
    }

    #[test]
    fn run_session_leaves_audit_record_incomplete_and_reports_teardown_error_when_allocation_rollback_fails()
     {
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
        let audit_root = unique_temp_dir("runner-audit-allocation-rollback-failure");
        fs::create_dir_all(&audit_root).expect("audit root should be created");

        let events = capture_tracing_events(|| {
            let error = fixture
                .run_with_fake_podman_env(|| {
                    run_session(
                        SessionSpec {
                            methodology_dir,
                            audit_root: audit_root.clone(),
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
                        SessionInvocation {
                            repo_url: "https://example.com/agentd.git".to_string(),
                            repo_token: None,
                            work_unit: None,
                            input: None,
                            timeout: None,
                        },
                    )
                })
                .expect_err("session should fail during resource allocation");

            assert!(
                matches!(error, RunnerError::PodmanCommandFailed { .. }),
                "expected podman command failure, got {error:?}"
            );
        });

        assert_eq!(events.len(), 4);
        assert_eq!(events[0]["fields"]["event"], "runner.session_started");
        assert_eq!(events[1]["fields"]["event"], "runner.lifecycle_failure");
        assert_eq!(events[1]["fields"]["stage"], "session resource allocation");
        assert_eq!(events[2]["fields"]["event"], "runner.session_error");
        assert_eq!(events[2]["fields"]["stage"], "session_resource_allocation");
        assert_eq!(events[3]["fields"]["event"], "runner.session_teardown");
        assert_eq!(events[3]["fields"]["result"], "error");

        let record_dir = only_session_record_dir(&audit_root, "site-builder");
        let metadata = read_session_metadata(&record_dir);
        assert!(
            metadata.get("end_timestamp").is_none(),
            "allocation rollback failure must not finalize end_timestamp"
        );
        assert!(
            metadata.get("outcome").is_none(),
            "allocation rollback failure must not finalize outcome"
        );

        use std::os::unix::fs::PermissionsExt;

        let runa_mode = fs::metadata(record_dir.join("runa"))
            .expect("runa dir metadata should exist")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            runa_mode, 0o777,
            "allocation rollback failure must leave the top-level runa dir in its active writable mode"
        );

        make_tree_writable(&audit_root);
        fs::remove_dir_all(&audit_root).expect("temporary audit root should be removed");
    }

    #[test]
    fn run_session_leaves_audit_record_incomplete_and_unsealed_when_container_creation_cleanup_fails()
     {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new()
                .with_create(CommandBehavior::from_outcome(
                    CommandOutcome::new().stderr("create failed").exit_code(42),
                ))
                .with_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new()
                        .append_args_with_prefix("rm-commands.log", "rm")
                        .stderr("rm failed")
                        .exit_code(51),
                )),
        );
        let methodology_dir = fixture.create_methodology_dir("runner-methodology");
        let audit_root = unique_temp_dir("runner-audit-create-cleanup-failure");
        fs::create_dir_all(&audit_root).expect("audit root should be created");

        let error = fixture
            .run_with_fake_podman_env(|| {
                run_session(
                    SessionSpec {
                        methodology_dir,
                        audit_root: audit_root.clone(),
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
            })
            .expect_err("session should fail during container creation");

        match error {
            RunnerError::PodmanCommandFailed {
                args,
                status,
                stderr,
            } => {
                assert_eq!(args[0], "create");
                assert_eq!(status.code(), Some(42));
                assert_eq!(stderr.trim(), "create failed");
            }
            other => panic!("expected create failure, got {other:?}"),
        }

        let record_dir = only_session_record_dir(&audit_root, "site-builder");
        let metadata = read_session_metadata(&record_dir);
        assert!(
            metadata.get("end_timestamp").is_none(),
            "cleanup failure must not finalize end_timestamp"
        );
        assert!(
            metadata.get("outcome").is_none(),
            "cleanup failure must not finalize outcome"
        );

        use std::os::unix::fs::PermissionsExt;

        let runa_mode = fs::metadata(record_dir.join("runa"))
            .expect("runa dir metadata should exist")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            runa_mode, 0o777,
            "cleanup failure must leave the top-level runa dir in its active writable mode"
        );
        assert_ne!(
            runa_mode, 0o555,
            "cleanup failure must not seal the top-level runa dir"
        );

        make_tree_writable(&audit_root);
        fs::remove_dir_all(&audit_root).expect("temporary audit root should be removed");
    }

    #[test]
    fn run_session_leaves_audit_record_incomplete_and_unsealed_when_post_execution_cleanup_fails() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new().with_rm(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .append_args_with_prefix("rm-commands.log", "rm")
                    .stderr("rm failed")
                    .exit_code(51),
            )),
        );
        let methodology_dir = fixture.create_methodology_dir("runner-methodology");
        let audit_root = unique_temp_dir("runner-audit-post-cleanup-failure");
        fs::create_dir_all(&audit_root).expect("audit root should be created");

        let error = fixture
            .run_with_fake_podman_env(|| {
                run_session(
                    SessionSpec {
                        methodology_dir,
                        audit_root: audit_root.clone(),
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
            })
            .expect_err("cleanup failure should surface as a session error");

        match error {
            RunnerError::PodmanCommandFailed {
                args,
                status,
                stderr,
            } => {
                assert_eq!(
                    args.len(),
                    4,
                    "cleanup failure should come from podman rm --force --ignore <container>"
                );
                assert_eq!(args[0], "rm");
                assert_eq!(args[1], "--force");
                assert_eq!(args[2], "--ignore");
                assert!(
                    args[3].starts_with("agentd-1a2b3c4d-site-builder-"),
                    "unexpected cleanup target: {}",
                    args[3]
                );
                assert_eq!(status.code(), Some(51));
                assert_eq!(stderr.trim(), "rm failed");
            }
            other => panic!("expected cleanup failure, got {other:?}"),
        }

        let record_dir = only_session_record_dir(&audit_root, "site-builder");
        let metadata = read_session_metadata(&record_dir);
        assert!(
            metadata.get("end_timestamp").is_none(),
            "cleanup failure must not finalize end_timestamp"
        );
        assert!(
            metadata.get("outcome").is_none(),
            "cleanup failure must not finalize outcome"
        );

        use std::os::unix::fs::PermissionsExt;

        let runa_mode = fs::metadata(record_dir.join("runa"))
            .expect("runa dir metadata should exist")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            runa_mode, 0o777,
            "cleanup failure must leave the top-level runa dir in its active writable mode"
        );
        assert_ne!(
            runa_mode, 0o555,
            "cleanup failure must not seal the top-level runa dir"
        );

        make_tree_writable(&audit_root);
        fs::remove_dir_all(&audit_root).expect("temporary audit root should be removed");
    }

    #[test]
    fn run_session_preserves_session_outcome_when_audit_finalization_fails_after_successful_cleanup()
     {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new().with_start(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .touch_file("start-blocked")
                    .wait_for_file(
                        "release-start",
                        Duration::from_secs(5),
                        "timed out waiting to release start",
                        91,
                    ),
            )),
        );
        let methodology_dir = fixture.create_methodology_dir("runner-methodology");
        let audit_root = unique_temp_dir("runner-audit-finalization-failure");
        fs::create_dir_all(&audit_root).expect("audit root should be created");
        let helper_audit_root = audit_root.clone();

        let events = fixture.run_with_fake_podman_env(|| {
            let log_dir = PathBuf::from(
                std::env::var("AGENTD_FAKE_PODMAN_LOG_DIR")
                    .expect("fake podman log dir should be configured"),
            );
            let helper = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline && !log_dir.join("start-blocked").exists() {
                    thread::sleep(Duration::from_millis(25));
                }
                assert!(
                    log_dir.join("start-blocked").exists(),
                    "fake podman start should block before audit finalization"
                );

                let record_dir =
                    wait_for_only_session_record_dir(&helper_audit_root, "site-builder", deadline);
                let agentd_dir = record_dir.join("agentd");
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&agentd_dir, fs::Permissions::from_mode(0o555))
                    .expect("agentd dir should become read-only");
                fs::write(log_dir.join("release-start"), b"release\n")
                    .expect("start should be released");
            });

            let events = capture_tracing_events(|| {
                let outcome = run_session(
                    SessionSpec {
                        methodology_dir,
                        audit_root: audit_root.clone(),
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
                .expect("session outcome should survive audit finalization failure");

                assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
            });

            helper
                .join()
                .expect("audit-finalization helper thread should complete");
            events
        });

        assert_eq!(events.len(), 4);
        assert_eq!(events[0]["fields"]["event"], "runner.session_started");
        assert_eq!(events[1]["fields"]["event"], "runner.session_outcome");
        assert_eq!(events[1]["fields"]["outcome"], "success");
        assert_eq!(events[2]["fields"]["event"], "runner.lifecycle_failure");
        assert_eq!(events[2]["fields"]["stage"], "session audit finalization");
        assert_eq!(events[3]["fields"]["event"], "runner.session_teardown");
        assert_eq!(events[3]["fields"]["result"], "error");

        let record_dir = only_session_record_dir(&audit_root, "site-builder");
        let metadata = read_session_metadata(&record_dir);
        assert!(
            metadata.get("end_timestamp").is_none(),
            "audit finalization failure must leave end_timestamp incomplete"
        );
        assert!(
            metadata.get("outcome").is_none(),
            "audit finalization failure must leave outcome incomplete"
        );

        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(record_dir.join("agentd"), fs::Permissions::from_mode(0o755))
            .expect("agentd dir should become removable for test cleanup");
        make_tree_writable(&audit_root);
        fs::remove_dir_all(&audit_root).expect("temporary audit root should be removed");
    }

    #[test]
    fn run_session_finalizes_direct_audit_entries() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new().with_start(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .touch_file("start-blocked")
                    .wait_for_file(
                        "release-start",
                        Duration::from_secs(5),
                        "timed out waiting to release start",
                        91,
                    ),
            )),
        );
        let methodology_dir = fixture.create_methodology_dir("runner-methodology");
        let audit_root = unique_temp_dir("runner-audit-direct-finalization");
        fs::create_dir_all(&audit_root).expect("audit root should be created");
        let helper_audit_root = audit_root.clone();

        let events = fixture.run_with_fake_podman_env(|| {
            let log_dir = PathBuf::from(
                std::env::var("AGENTD_FAKE_PODMAN_LOG_DIR")
                    .expect("fake podman log dir should be configured"),
            );
            let helper = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline && !log_dir.join("start-blocked").exists() {
                    thread::sleep(Duration::from_millis(25));
                }
                assert!(
                    log_dir.join("start-blocked").exists(),
                    "fake podman start should block before audit finalization"
                );

                let record_dir =
                    wait_for_only_session_record_dir(&helper_audit_root, "site-builder", deadline);
                let direct_dir = record_dir.join("runa/direct");
                fs::create_dir_all(&direct_dir).expect("direct audit dir should be created");
                fs::write(direct_dir.join("artifact.txt"), "persisted\n")
                    .expect("direct audit file should be created");
                fs::write(log_dir.join("release-start"), b"release\n")
                    .expect("start should be released");
            });

            let events = capture_tracing_events(|| {
                let outcome = run_session(
                    SessionSpec {
                        methodology_dir,
                        audit_root: audit_root.clone(),
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
                .expect("session outcome should survive audit finalization failure");

                assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
            });

            helper
                .join()
                .expect("audit-finalization helper thread should complete");
            events
        });

        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["fields"]["event"], "runner.session_started");
        assert_eq!(events[1]["fields"]["event"], "runner.session_outcome");
        assert_eq!(events[1]["fields"]["outcome"], "success");
        assert_eq!(events[2]["fields"]["event"], "runner.session_teardown");
        assert_eq!(events[2]["fields"]["result"], "ok");

        let record_dir = only_session_record_dir(&audit_root, "site-builder");
        let metadata = read_session_metadata(&record_dir);
        assert_eq!(metadata["outcome"], "success");
        assert!(
            metadata["end_timestamp"].is_string(),
            "successful audit finalization must publish end_timestamp"
        );
        use std::os::unix::fs::PermissionsExt;

        let direct_dir = record_dir.join("runa/direct");
        let direct_file = direct_dir.join("artifact.txt");
        assert_eq!(
            fs::metadata(&direct_dir)
                .expect("direct audit dir metadata should exist")
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
        assert_eq!(
            fs::metadata(&direct_file)
                .expect("direct audit file metadata should exist")
                .permissions()
                .mode()
                & 0o777,
            0o444
        );

        fs::set_permissions(&direct_dir, fs::Permissions::from_mode(0o755))
            .expect("direct dir should become removable for test cleanup");
        fs::set_permissions(record_dir.join("runa"), fs::Permissions::from_mode(0o755))
            .expect("runa dir should become removable for test cleanup");
        make_tree_writable(&audit_root);
        fs::remove_dir_all(&audit_root).expect("temporary audit root should be removed");
    }

    fn wait_for_only_session_record_dir(
        audit_root: &Path,
        agent_name: &str,
        deadline: Instant,
    ) -> PathBuf {
        loop {
            let agent_root = audit_root.join(agent_name);
            if let Ok(entries) = fs::read_dir(&agent_root) {
                let entries = entries
                    .map(|entry| {
                        entry
                            .expect("session record entry should be readable")
                            .path()
                    })
                    .filter(|path| path.is_dir())
                    .collect::<Vec<_>>();
                if entries.len() == 1 {
                    return entries[0].clone();
                }
            }

            assert!(
                Instant::now() < deadline,
                "timed out waiting for session record under {}",
                agent_root.display()
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn startup_reconciliation_removes_only_terminal_agentd_containers_and_orphaned_secrets() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new()
                .with_ps(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    &fake_podman_ps_json(&[
                        (
                            &["agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa"],
                            "exited",
                            "Exited (0) 10s ago",
                        ),
                        (&["agentd-1a2b3c4d-review-bbbbbbbbbbbbbbbb"], "dead", "Dead"),
                        (
                            &["agentd-1a2b3c4d-build-cccccccccccccccc"],
                            "stopped",
                            "Exited (137) 5m ago",
                        ),
                        (
                            &["agentd-1a2b3c4d-prepare-dddddddddddddddd"],
                            "created",
                            "Created",
                        ),
                        (
                            &["agentd-1a2b3c4d-init-1212121212121212"],
                            "initialized",
                            "Initialized",
                        ),
                        (
                            &["agentd-1a2b3c4d-pause-eeeeeeeeeeeeeeee"],
                            "paused",
                            "Up 2 minutes",
                        ),
                        (
                            &["agentd-1a2b3c4d-stop-ffffffffffffffff"],
                            "stopping",
                            "Stopping",
                        ),
                        (
                            &["agentd-1a2b3c4d-live-9999999999999999"],
                            "running",
                            "Up 4 hours",
                        ),
                        (
                            &["agentd-deadbeef-foreign-3434343434343434"],
                            "exited",
                            "Exited (0) 1m ago",
                        ),
                        (&["postgres-db"], "exited", "Exited (0) 1h ago"),
                    ]),
                )))
                .with_secret_ls(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    "agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0\n\
                         agentd-1a2b3c4d-bbbbbbbbbbbbbbbb-0\n\
                         agentd-1a2b3c4d-cccccccccccccccc-repo-token\n\
                         agentd-1a2b3c4d-dddddddddddddddd-0\n\
                         agentd-1a2b3c4d-1212121212121212-0\n\
                         agentd-1a2b3c4d-eeeeeeeeeeeeeeee-0\n\
                         agentd-1a2b3c4d-ffffffffffffffff-0\n\
                         agentd-1a2b3c4d-9999999999999999-0\n\
                         agentd-deadbeef-3434343434343434-repo-token\n\
                         foreign-secret",
                )))
                .with_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new().append_args_with_prefix("rm-commands.log", "rm"),
                ))
                .with_secret_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new().append_args_with_prefix("secret-commands.log", "rm"),
                )),
        );

        let report = fixture
            .run_with_fake_podman_env(reconcile_startup_resources_for_tests)
            .expect("startup reconciliation should succeed");

        assert_eq!(
            report.removed_container_names,
            vec![
                "agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa".to_string(),
                "agentd-1a2b3c4d-review-bbbbbbbbbbbbbbbb".to_string(),
                "agentd-1a2b3c4d-build-cccccccccccccccc".to_string(),
                "agentd-1a2b3c4d-prepare-dddddddddddddddd".to_string(),
                "agentd-1a2b3c4d-init-1212121212121212".to_string(),
            ]
        );
        assert_eq!(
            report.removed_secret_names,
            vec![
                "agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0".to_string(),
                "agentd-1a2b3c4d-bbbbbbbbbbbbbbbb-0".to_string(),
                "agentd-1a2b3c4d-cccccccccccccccc-repo-token".to_string(),
                "agentd-1a2b3c4d-dddddddddddddddd-0".to_string(),
                "agentd-1a2b3c4d-1212121212121212-0".to_string(),
            ]
        );
        assert_eq!(
            fixture.read_log("rm-commands.log"),
            "rm --force --ignore agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa agentd-1a2b3c4d-review-bbbbbbbbbbbbbbbb agentd-1a2b3c4d-build-cccccccccccccccc agentd-1a2b3c4d-prepare-dddddddddddddddd agentd-1a2b3c4d-init-1212121212121212\n"
        );
        assert_eq!(
            fixture.secret_commands(),
            "rm agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0 agentd-1a2b3c4d-bbbbbbbbbbbbbbbb-0 agentd-1a2b3c4d-cccccccccccccccc-repo-token agentd-1a2b3c4d-dddddddddddddddd-0 agentd-1a2b3c4d-1212121212121212-0\n"
        );
    }

    #[test]
    fn startup_reconciliation_keeps_running_agentd_containers_and_their_secrets() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new()
                .with_ps(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    &fake_podman_ps_json(&[(
                        &["agentd-1a2b3c4d-site-builder-dddddddddddddddd"],
                        "running",
                        "Up 2 minutes",
                    )]),
                )))
                .with_secret_ls(CommandBehavior::from_outcome(
                    CommandOutcome::new().stdout("agentd-1a2b3c4d-dddddddddddddddd-0"),
                )),
        );

        let report = fixture
            .run_with_fake_podman_env(reconcile_startup_resources_for_tests)
            .expect("startup reconciliation should succeed");

        assert!(report.removed_container_names.is_empty());
        assert!(report.removed_secret_names.is_empty());
        assert_eq!(fixture.read_log("rm-commands.log"), "");
        assert_eq!(fixture.secret_commands(), "");
    }

    #[test]
    fn startup_reconciliation_keeps_unknown_state_agentd_containers_and_their_secrets() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new()
                .with_ps(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    &fake_podman_ps_json(&[(
                        &["agentd-1a2b3c4d-site-builder-dddddddddddddddd"],
                        "mystery-state",
                        "Something odd just happened",
                    )]),
                )))
                .with_secret_ls(CommandBehavior::from_outcome(
                    CommandOutcome::new().stdout("agentd-1a2b3c4d-dddddddddddddddd-0"),
                )),
        );

        let report = fixture
            .run_with_fake_podman_env(reconcile_startup_resources_for_tests)
            .expect("startup reconciliation should succeed");

        assert!(report.removed_container_names.is_empty());
        assert!(report.removed_secret_names.is_empty());
        assert_eq!(fixture.read_log("rm-commands.log"), "");
        assert_eq!(fixture.secret_commands(), "");
    }

    #[test]
    fn startup_reconciliation_ignores_terminal_agentd_prefixed_non_session_containers() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new()
                .with_ps(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    &fake_podman_ps_json(&[
                        (&["agentd-proxy"], "exited", "Exited (0) 10s ago"),
                        (
                            &["agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa"],
                            "exited",
                            "Exited (0) 10s ago",
                        ),
                    ]),
                )))
                .with_secret_ls(CommandBehavior::from_outcome(
                    CommandOutcome::new().stdout("agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0"),
                ))
                .with_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new().append_args_with_prefix("rm-commands.log", "rm"),
                ))
                .with_secret_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new().append_args_with_prefix("secret-commands.log", "rm"),
                )),
        );

        let report = fixture
            .run_with_fake_podman_env(reconcile_startup_resources_for_tests)
            .expect("startup reconciliation should succeed");

        assert_eq!(
            report.removed_container_names,
            vec!["agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa".to_string(),]
        );
        assert_eq!(
            report.removed_secret_names,
            vec!["agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0".to_string()]
        );
        assert_eq!(
            fixture.read_log("rm-commands.log"),
            "rm --force --ignore agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa\n"
        );
        assert_eq!(
            fixture.secret_commands(),
            "rm agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0\n"
        );
    }

    #[test]
    fn startup_reconciliation_ignores_terminal_agentd_containers_with_uppercase_session_ids() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new()
                .with_ps(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    &fake_podman_ps_json(&[
                        (
                            &["agentd-1a2b3c4d-site-builder-AAAAAAAAAAAAAAAA"],
                            "exited",
                            "Exited (0) 10s ago",
                        ),
                        (
                            &["agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa"],
                            "exited",
                            "Exited (0) 10s ago",
                        ),
                    ]),
                )))
                .with_secret_ls(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    "agentd-1a2b3c4d-AAAAAAAAAAAAAAAA-0\n\
                     agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0",
                )))
                .with_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new().append_args_with_prefix("rm-commands.log", "rm"),
                ))
                .with_secret_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new().append_args_with_prefix("secret-commands.log", "rm"),
                )),
        );

        let report = fixture
            .run_with_fake_podman_env(reconcile_startup_resources_for_tests)
            .expect("startup reconciliation should succeed");

        assert_eq!(
            report.removed_container_names,
            vec!["agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa".to_string()]
        );
        assert_eq!(
            report.removed_secret_names,
            vec!["agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0".to_string()]
        );
        assert_eq!(
            fixture.read_log("rm-commands.log"),
            "rm --force --ignore agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa\n"
        );
        assert_eq!(
            fixture.secret_commands(),
            "rm agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0\n"
        );
    }

    #[test]
    fn startup_reconciliation_ignores_secrets_with_non_runner_suffixes() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new()
                .with_ps(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    &fake_podman_ps_json(&[(
                        &["agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa"],
                        "exited",
                        "Exited (0) 10s ago",
                    )]),
                )))
                .with_secret_ls(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    "agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0\n\
                     agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-backup",
                )))
                .with_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new().append_args_with_prefix("rm-commands.log", "rm"),
                ))
                .with_secret_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new().append_args_with_prefix("secret-commands.log", "rm"),
                )),
        );

        let report = fixture
            .run_with_fake_podman_env(reconcile_startup_resources_for_tests)
            .expect("startup reconciliation should succeed");

        assert_eq!(
            report.removed_container_names,
            vec!["agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa".to_string()]
        );
        assert_eq!(
            report.removed_secret_names,
            vec!["agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0".to_string()]
        );
        assert_eq!(
            fixture.read_log("rm-commands.log"),
            "rm --force --ignore agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa\n"
        );
        assert_eq!(
            fixture.secret_commands(),
            "rm agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0\n"
        );
    }

    #[test]
    fn startup_reconciliation_returns_an_error_when_container_listing_json_is_malformed() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new().with_ps(CommandBehavior::from_outcome(
                CommandOutcome::new().stdout("{not-json"),
            )),
        );

        let error = fixture
            .run_with_fake_podman_env(reconcile_startup_resources_for_tests)
            .expect_err(
                "startup reconciliation should fail when container listing JSON is invalid",
            );

        match error {
            RunnerError::Io(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
                assert!(
                    error.to_string().contains("podman ps --format json"),
                    "expected invalid json error to mention podman ps output, got {error}"
                );
            }
            other => panic!("expected invalid-data io error, got {other:?}"),
        }
        assert_eq!(fixture.read_log("rm-commands.log"), "");
        assert_eq!(fixture.secret_commands(), "");
    }

    #[test]
    fn startup_reconciliation_returns_an_error_when_container_listing_fails() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new().with_ps(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .stderr("container list failed")
                    .exit_code(29),
            )),
        );

        let error = fixture
            .run_with_fake_podman_env(reconcile_startup_resources_for_tests)
            .expect_err("startup reconciliation should fail when container listing fails");

        match error {
            RunnerError::PodmanCommandFailed {
                args,
                status,
                stderr,
            } => {
                assert_eq!(args, vec!["ps", "-a", "--format", "json"]);
                assert_eq!(status.code(), Some(29));
                assert_eq!(stderr.trim(), "container list failed");
            }
            other => panic!("expected podman command failure, got {other:?}"),
        }
    }

    #[test]
    fn startup_reconciliation_returns_an_error_when_container_removal_fails() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new()
                .with_ps(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    &fake_podman_ps_json(&[(
                        &["agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa"],
                        "exited",
                        "Exited (0) 10s ago",
                    )]),
                )))
                .with_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new()
                        .write_args_to("rm-commands.log")
                        .stderr("rm failed")
                        .exit_code(51),
                )),
        );

        let error = fixture
            .run_with_fake_podman_env(reconcile_startup_resources_for_tests)
            .expect_err("startup reconciliation should fail when container removal fails");

        match error {
            RunnerError::PodmanCommandFailed {
                args,
                status,
                stderr,
            } => {
                assert_eq!(
                    args,
                    vec![
                        "rm",
                        "--force",
                        "--ignore",
                        "agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa",
                    ]
                );
                assert_eq!(status.code(), Some(51));
                assert_eq!(stderr.trim(), "rm failed");
            }
            other => panic!("expected podman command failure, got {other:?}"),
        }
        assert_eq!(
            fixture.read_log("rm-commands.log"),
            "--force --ignore agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa\n"
        );
    }

    #[test]
    fn startup_reconciliation_keeps_resources_owned_by_other_daemon_instances() {
        let _guard = fake_podman_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = FakePodmanFixture::new();
        fixture.install(
            &FakePodmanScenario::new()
                .with_ps(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    &fake_podman_ps_json(&[
                        (
                            &["agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa"],
                            "exited",
                            "Exited (0) 10s ago",
                        ),
                        (
                            &["agentd-deadbeef-site-builder-bbbbbbbbbbbbbbbb"],
                            "exited",
                            "Exited (0) 10s ago",
                        ),
                    ]),
                )))
                .with_secret_ls(CommandBehavior::from_outcome(CommandOutcome::new().stdout(
                    "agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0\n\
                     agentd-deadbeef-bbbbbbbbbbbbbbbb-0",
                )))
                .with_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new().append_args_with_prefix("rm-commands.log", "rm"),
                ))
                .with_secret_rm(CommandBehavior::from_outcome(
                    CommandOutcome::new().append_args_with_prefix("secret-commands.log", "rm"),
                )),
        );

        let report = fixture
            .run_with_fake_podman_env(reconcile_startup_resources_for_tests)
            .expect("startup reconciliation should succeed");

        assert_eq!(
            report.removed_container_names,
            vec!["agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa".to_string()]
        );
        assert_eq!(
            report.removed_secret_names,
            vec!["agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0".to_string()]
        );
        assert_eq!(
            fixture.read_log("rm-commands.log"),
            "rm --force --ignore agentd-1a2b3c4d-site-builder-aaaaaaaaaaaaaaaa\n"
        );
        assert_eq!(
            fixture.secret_commands(),
            "rm agentd-1a2b3c4d-aaaaaaaaaaaaaaaa-0\n"
        );
    }
}
