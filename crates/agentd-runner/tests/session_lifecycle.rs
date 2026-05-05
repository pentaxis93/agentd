use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use agentd_runner::{
    BindMount, InvocationInput, ResolvedEnvironmentVariable, SessionInvocation, SessionOutcome,
    SessionSpec, run_session,
};
use serde_json::Value;

const TEST_DAEMON_INSTANCE_ID: &str = "1a2b3c4d";
const SESSION_USER_ID: u32 = 1000;
const SESSION_GROUP_ID: u32 = 1000;

fn run_session_with_test_audit_root(
    audit_root: &Path,
    mut spec: SessionSpec,
    invocation: SessionInvocation,
) -> Result<SessionOutcome, agentd_runner::RunnerError> {
    spec.audit_root = audit_root.to_path_buf();
    run_session(spec, invocation)
}

fn wait_for_session_record_dir(audit_root: &Path, agent_name: &str, timeout: Duration) -> PathBuf {
    let deadline = Instant::now() + timeout;
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

fn write_methodology_manifest(path: &Path, artifact_types: &[&str]) {
    let mut manifest = String::from("name = \"test-methodology\"\n");
    for artifact_type in artifact_types {
        manifest.push_str("\n[[artifact_types]]\nname = \"");
        manifest.push_str(artifact_type);
        manifest.push_str("\"\n");
    }
    fs::write(path.join("manifest.toml"), manifest)
        .expect("methodology manifest should be written");
}

fn install_request_schema(path: &Path, version: &str) {
    let schema_dir = path.join("schemas");
    fs::create_dir_all(&schema_dir).expect("schema dir should be created");
    fs::write(
        schema_dir.join("request.schema.json"),
        format!(
            r#"{{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "x-tesserine-canonical": {{
    "version": "{version}",
    "schema_url": "https://example.com/request.schema.json",
    "prose_url": "https://example.com/REQUEST.md"
  }},
  "type": "object",
  "required": ["description", "source"],
  "additionalProperties": false,
  "properties": {{
    "description": {{ "type": "string", "minLength": 1 }},
    "source": {{ "type": "string", "minLength": 1 }},
    "context": {{ "type": "string" }}
  }}
}}
"#
        ),
    )
    .expect("request schema should be written");
}

fn install_claim_schema(path: &Path) {
    let schema_dir = path.join("schemas");
    fs::create_dir_all(&schema_dir).expect("schema dir should be created");
    fs::write(
        schema_dir.join("claim.schema.json"),
        r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "required": ["summary"],
  "additionalProperties": false,
  "properties": {
    "summary": { "type": "string", "minLength": 1 }
  }
}
"#,
    )
    .expect("claim schema should be written");
}

#[test]
fn succeeds_without_timeout_and_cleans_up_container() {
    if skip_if_podman_unavailable("succeeds_without_timeout_and_cleans_up_container") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("success-run");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "success-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec![
                "site-builder".to_string(),
                "exec".to_string(),
                "--sandbox".to_string(),
                "workspace-write".to_string(),
            ],
            environment: vec![
                ResolvedEnvironmentVariable {
                    name: "GITHUB_TOKEN".to_string(),
                    value: "test-token".to_string(),
                },
                ResolvedEnvironmentVariable {
                    name: "SESSION_TEST_BEHAVIOR".to_string(),
                    value: "success".to_string(),
                },
            ],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: Some("task-42".to_string()),
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn materializes_request_text_input_before_session_command_runs() {
    if skip_if_podman_unavailable("materializes_request_text_input_before_session_command_runs") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("request-input-run");
    write_methodology_manifest(&fixture.methodology_dir(), &["request"]);
    install_request_schema(&fixture.methodology_dir(), "1.0.0");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "request-input-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "assert-request-input-present".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: Some(InvocationInput::RequestText {
                description: "Add a status page".to_string(),
            }),
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn materializes_generic_artifact_input_before_session_command_runs() {
    if skip_if_podman_unavailable("materializes_generic_artifact_input_before_session_command_runs")
    {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("artifact-input-run");
    write_methodology_manifest(&fixture.methodology_dir(), &["claim"]);
    install_claim_schema(&fixture.methodology_dir());
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "artifact-input-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "assert-claim-input-present".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: Some(InvocationInput::Artifact {
                artifact_type: "claim".to_string(),
                artifact_id: "claim".to_string(),
                document: serde_json::json!({ "summary": "Ship it" }),
            }),
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn rejects_request_text_when_methodology_declares_an_unsupported_request_version() {
    if skip_if_podman_unavailable(
        "rejects_request_text_when_methodology_declares_an_unsupported_request_version",
    ) {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("unsupported-request-version-run");
    write_methodology_manifest(&fixture.methodology_dir(), &["request"]);
    install_request_schema(&fixture.methodology_dir(), "2.0.0");
    let image = fixture.build_image();

    let error = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "unsupported-request-version-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: Vec::new(),
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: Some(InvocationInput::RequestText {
                description: "Add a status page".to_string(),
            }),
            timeout: None,
        },
    )
    .expect_err("unsupported request version should fail before session start");

    assert!(
        error
            .to_string()
            .contains("canonical request version 2.0.0 is not supported"),
        "expected unsupported-version error, got: {error}"
    );
}

#[test]
fn succeeds_with_empty_and_non_empty_environment_values() {
    if skip_if_podman_unavailable("succeeds_with_empty_and_non_empty_environment_values") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("mixed-env-run");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "mixed-env-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![
                ResolvedEnvironmentVariable {
                    name: "GITHUB_TOKEN".to_string(),
                    value: "test-token".to_string(),
                },
                ResolvedEnvironmentVariable {
                    name: "EMPTY_SESSION_ENV".to_string(),
                    value: String::new(),
                },
                ResolvedEnvironmentVariable {
                    name: "SESSION_TEST_BEHAVIOR".to_string(),
                    value: "success-empty-env".to_string(),
                },
            ],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: Some("task-42".to_string()),
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn clears_inherited_work_unit_when_invocation_omits_it() {
    if skip_if_podman_unavailable("clears_inherited_work_unit_when_invocation_omits_it") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("unset-work-unit-run");
    let image = fixture.build_image_with_agentd_work_unit("stale-from-image");

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "unset-work-unit-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "success-without-work-unit".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn returns_failed_exit_code_without_timeout_and_cleans_up_container() {
    if skip_if_podman_unavailable(
        "returns_failed_exit_code_without_timeout_and_cleans_up_container",
    ) {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("failure-run");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "failure-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![
                ResolvedEnvironmentVariable {
                    name: "GITHUB_TOKEN".to_string(),
                    value: "test-token".to_string(),
                },
                ResolvedEnvironmentVariable {
                    name: "SESSION_TEST_BEHAVIOR".to_string(),
                    value: "fail".to_string(),
                },
            ],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::GenericFailure { exit_code: 23 });
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn returns_failed_exit_code_125_without_timeout_and_cleans_up_runner_resources() {
    if skip_if_podman_unavailable(
        "returns_failed_exit_code_125_without_timeout_and_cleans_up_runner_resources",
    ) {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("failure-run-125");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "failure-run-125".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![
                ResolvedEnvironmentVariable {
                    name: "GITHUB_TOKEN".to_string(),
                    value: "test-token".to_string(),
                },
                ResolvedEnvironmentVariable {
                    name: "SESSION_TEST_BEHAVIOR".to_string(),
                    value: "fail-125".to_string(),
                },
            ],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::GenericFailure { exit_code: 125 });
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn succeeds_when_methodology_dir_path_contains_commas() {
    if skip_if_podman_unavailable("succeeds_when_methodology_dir_path_contains_commas") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new_with_root_prefix(
        "comma-methodology-run",
        "agentd-runner,comma,methodology",
    );
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "comma-methodology-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![
                ResolvedEnvironmentVariable {
                    name: "GITHUB_TOKEN".to_string(),
                    value: "test-token".to_string(),
                },
                ResolvedEnvironmentVariable {
                    name: "SESSION_TEST_BEHAVIOR".to_string(),
                    value: "success".to_string(),
                },
            ],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: Some("task-42".to_string()),
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn validates_read_only_additional_mounts_from_paths_containing_commas() {
    if skip_if_podman_unavailable(
        "validates_read_only_additional_mounts_from_paths_containing_commas",
    ) {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("readonly-mount-run");
    let image = fixture.build_image();
    let host_mount = fixture.root.join("host,readonly");
    fs::create_dir_all(&host_mount).expect("read-only host mount should be created");
    fs::write(host_mount.join("auth.json"), "{\"token\":\"test\"}\n")
        .expect("read-only host fixture file should be written");
    fs::write(
        host_mount.join("sentinel.txt"),
        "host data should remain untouched\n",
    )
    .expect("read-only host sentinel file should be written");

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "readonly-mount-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: vec![BindMount {
                source: host_mount.clone(),
                target: PathBuf::from("/home/readonly-mount-run/.claude"),
                read_only: true,
            }],
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "verify-read-only-mount".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    assert!(
        !host_mount.join("write-should-fail").exists(),
        "read-only mount should not permit in-container writes"
    );
    assert_eq!(
        fs::read_to_string(host_mount.join("auth.json"))
            .expect("read-only host auth fixture should remain readable"),
        "{\"token\":\"test\"}\n"
    );
    assert_eq!(
        fs::read_to_string(host_mount.join("sentinel.txt"))
            .expect("read-only host sentinel should remain readable"),
        "host data should remain untouched\n"
    );
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn preserves_host_writes_through_read_write_additional_mounts() {
    if skip_if_podman_unavailable("preserves_host_writes_through_read_write_additional_mounts") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("readwrite-mount-run");
    let image = fixture.build_image();
    let host_mount = fixture.root.join("host-readwrite");
    fs::create_dir_all(&host_mount).expect("read-write host mount should be created");
    fs::write(host_mount.join("sentinel.txt"), "host sentinel\n")
        .expect("read-write host sentinel should be written");
    let sentinel_metadata_before =
        fs::metadata(host_mount.join("sentinel.txt")).expect("sentinel metadata should exist");
    fs::set_permissions(&host_mount, fs::Permissions::from_mode(0o777))
        .expect("read-write host mount should permit container writes");

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "readwrite-mount-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: vec![BindMount {
                source: host_mount.clone(),
                target: PathBuf::from("/home/readwrite-mount-run/.runa"),
                read_only: false,
            }],
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "write-read-write-mount".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    assert_eq!(
        fs::read_to_string(host_mount.join("session-artifact.txt"))
            .expect("read-write mount should persist host-visible writes"),
        "persisted from container\n"
    );
    assert_eq!(
        fs::read_to_string(host_mount.join("sentinel.txt"))
            .expect("read-write host sentinel should remain readable"),
        "host sentinel\n"
    );
    let sentinel_metadata_after =
        fs::metadata(host_mount.join("sentinel.txt")).expect("sentinel metadata should exist");
    assert_eq!(
        sentinel_metadata_after.uid(),
        sentinel_metadata_before.uid(),
        "runner setup must not re-own host-backed files under home mounts"
    );
    assert_eq!(
        sentinel_metadata_after.gid(),
        sentinel_metadata_before.gid(),
        "runner setup must not re-own host-backed files under home mounts"
    );
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn preserves_writable_home_for_nested_additional_mount_parents() {
    if skip_if_podman_unavailable("preserves_writable_home_for_nested_additional_mount_parents") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("nested-home-mount-run");
    let image = fixture.build_image();
    let host_mount = fixture.root.join("host-nested-claude");
    fs::create_dir_all(&host_mount).expect("nested host mount should be created");
    fs::set_permissions(&host_mount, fs::Permissions::from_mode(0o777))
        .expect("nested host mount should permit container writes");

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "nested-home-mount-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: vec![BindMount {
                source: host_mount.clone(),
                target: PathBuf::from("/home/nested-home-mount-run/.config/claude"),
                read_only: false,
            }],
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "write-nested-home-mount".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    assert_eq!(
        fs::read_to_string(host_mount.join("nested-artifact.txt"))
            .expect("nested home mount should persist host-visible writes"),
        "persisted from nested mount\n"
    );
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn preserves_session_user_access_to_preexisting_home_content() {
    if skip_if_podman_unavailable("preserves_session_user_access_to_preexisting_home_content") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("preexisting-home-run");
    let image = fixture.build_image_with_preexisting_home_file();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "preexisting-home-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "write-preexisting-home-file".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn preserves_host_audit_record_after_successful_session_teardown() {
    if skip_if_podman_unavailable("preserves_host_audit_record_after_successful_session_teardown") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("audit-success-run");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "audit-success-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "write-repo-audit-state".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: Some("issue-76".to_string()),
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });

    let record_dir = fixture.only_session_record_dir();
    assert_eq!(
        fs::read_to_string(record_dir.join("runa/workspace/session-artifact.txt"))
            .expect("workspace artifact should persist"),
        "persisted through repo bridge\n"
    );
    assert_eq!(
        fs::read_to_string(record_dir.join("runa/store/executions/0001.json"))
            .expect("execution record should persist"),
        "{\"protocols\":[\"begin\"],\"postconditions\":[\"passed\"]}\n"
    );
    assert_eq!(
        fs::read_to_string(record_dir.join("runa/calls.log"))
            .expect("runa call log should persist"),
        "init --methodology /agentd/methodology/manifest.toml\nrun --work-unit issue-76 --agent-command -- site-builder exec\n"
    );
    let runa_config = fs::read_to_string(record_dir.join("runa/config.toml"))
        .expect("runa config should persist");
    assert!(
        !runa_config.contains("[agent]"),
        "agentd-managed runa config must not contain an [agent] section: {runa_config}"
    );

    let metadata: Value = serde_json::from_str(
        &fs::read_to_string(record_dir.join("agentd/session.json"))
            .expect("session metadata should persist"),
    )
    .expect("session metadata should be valid json");
    assert_eq!(metadata["agent"], "audit-success-run");
    assert_eq!(metadata["repo_url"], fixture.repo_url());
    assert_eq!(metadata["work_unit"], "issue-76");
    assert_eq!(metadata["outcome"], "success");
    assert_eq!(metadata["exit_code"], 0);
    assert!(metadata["start_timestamp"].is_string());
    assert!(metadata["end_timestamp"].is_string());

    let runa_mode = fs::metadata(record_dir.join("runa"))
        .expect("runa dir metadata should exist")
        .permissions()
        .mode();
    let metadata_mode = fs::metadata(record_dir.join("agentd/session.json"))
        .expect("session metadata permissions should exist")
        .permissions()
        .mode();
    assert_eq!(
        runa_mode & 0o222,
        0,
        "completed runa dir should be read-only"
    );
    assert_eq!(
        metadata_mode & 0o222,
        0,
        "completed metadata file should be read-only"
    );

    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn preserves_host_readability_for_restrictive_container_written_audit_entries_after_teardown() {
    if skip_if_podman_unavailable(
        "preserves_host_readability_for_restrictive_container_written_audit_entries_after_teardown",
    ) {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("audit-restrictive-modes-run");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "audit-restrictive-modes-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "write-restrictive-repo-audit-state".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: Some("issue-76".to_string()),
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });

    let record_dir = fixture.only_session_record_dir();
    let artifact_path = record_dir.join("runa/workspace/private/session-artifact.txt");
    assert_eq!(
        fs::read_to_string(&artifact_path)
            .expect("container-written restrictive audit artifact should remain host-readable"),
        "host should still read this after teardown\n"
    );

    use std::os::unix::fs::PermissionsExt;

    let runa_mode = fs::metadata(record_dir.join("runa"))
        .expect("runa dir metadata should exist")
        .permissions()
        .mode()
        & 0o777;
    let workspace_mode = fs::metadata(record_dir.join("runa/workspace/private"))
        .expect("workspace dir metadata should exist")
        .permissions()
        .mode()
        & 0o777;
    let artifact_mode = fs::metadata(&artifact_path)
        .expect("artifact metadata should exist")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(runa_mode, 0o555);
    assert_eq!(workspace_mode, 0o555);
    assert_eq!(artifact_mode, 0o444);

    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn refuses_hard_linked_audit_entries_without_mutating_operator_mount_file_modes() {
    if skip_if_podman_unavailable(
        "refuses_hard_linked_audit_entries_without_mutating_operator_mount_file_modes",
    ) {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("audit-hard-link-run");
    let image = fixture.build_image();
    let host_mount = fixture.root.join("host-hard-link");
    fs::create_dir_all(&host_mount).expect("hard-link host mount should be created");
    fs::set_permissions(&host_mount, fs::Permissions::from_mode(0o777))
        .expect("hard-link host mount should permit container writes");
    let operator_file = host_mount.join("operator-state.txt");
    fs::write(&operator_file, "operator managed\n").expect("operator file should be written");
    fs::set_permissions(&operator_file, fs::Permissions::from_mode(0o666))
        .expect("operator file should be writable");

    let audit_root = fixture.audit_root();
    let helper_audit_root = audit_root.clone();
    let helper_operator_file = operator_file.clone();
    let helper = thread::spawn(move || {
        let record_dir = wait_for_session_record_dir(
            &helper_audit_root,
            "audit-hard-link-run",
            Duration::from_secs(5),
        );
        fs::hard_link(
            &helper_operator_file,
            record_dir.join("runa/escaped-hard-link.txt"),
        )
        .expect("host should be able to plant a hard-linked audit entry");
    });

    let outcome = run_session_with_test_audit_root(
        &audit_root,
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "audit-hard-link-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: audit_root.clone(),
            mounts: vec![BindMount {
                source: host_mount.clone(),
                target: PathBuf::from("/home/audit-hard-link-run/shared"),
                read_only: false,
            }],
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "sleep-short".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
    )
    .expect("session outcome should survive hard-link audit refusal");

    helper.join().expect("hard-link helper should complete");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });

    let operator_mode = fs::metadata(&operator_file)
        .expect("operator file metadata should exist")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(operator_mode, 0o666);

    let record_dir = fixture.only_session_record_dir();
    let metadata: Value = serde_json::from_str(
        &fs::read_to_string(record_dir.join("agentd/session.json"))
            .expect("session metadata should persist"),
    )
    .expect("session metadata should be valid json");
    assert!(
        metadata.get("end_timestamp").is_none(),
        "hard-link refusal must leave end_timestamp incomplete"
    );
    assert!(
        metadata.get("outcome").is_none(),
        "hard-link refusal must leave outcome incomplete"
    );

    let runa_mode = fs::metadata(record_dir.join("runa"))
        .expect("runa dir metadata should exist")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(runa_mode, 0o777);

    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn preserves_failing_audit_trail_for_post_mortem_reconstruction() {
    if skip_if_podman_unavailable("preserves_failing_audit_trail_for_post_mortem_reconstruction") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("audit-failure-run");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "audit-failure-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "write-failing-audit-trail".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: Some("issue-76".to_string()),
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::WorkFailed { exit_code: 5 });

    let record_dir = fixture.only_session_record_dir();
    let execution_record = fs::read_to_string(record_dir.join("runa/store/executions/0001.json"))
        .expect("execution record should persist");
    assert!(execution_record.contains("\"protocol\":\"begin\""));
    assert!(execution_record.contains("\"protocol\":\"decompose\""));
    assert!(execution_record.contains("\"postcondition\":\"passed\""));
    assert!(execution_record.contains("\"postcondition\":\"failed\""));
    assert!(execution_record.contains("\"artifact\":\"claim.md\""));
    assert!(execution_record.contains("\"artifact\":\"plan.md\""));
    assert_eq!(
        fs::read_to_string(record_dir.join("runa/workspace/decompose/plan.md"))
            .expect("workspace artifact should persist"),
        "draft plan\n"
    );

    let metadata: Value = serde_json::from_str(
        &fs::read_to_string(record_dir.join("agentd/session.json"))
            .expect("session metadata should persist"),
    )
    .expect("session metadata should be valid json");
    assert_eq!(metadata["outcome"], "work_failed");
    assert_eq!(metadata["exit_code"], 5);
    assert!(metadata["end_timestamp"].is_string());

    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn times_out_when_a_timeout_is_provided_and_cleans_up_container() {
    if skip_if_podman_unavailable("times_out_when_a_timeout_is_provided_and_cleans_up_container") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("timeout-run");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "timeout-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![
                ResolvedEnvironmentVariable {
                    name: "GITHUB_TOKEN".to_string(),
                    value: "test-token".to_string(),
                },
                ResolvedEnvironmentVariable {
                    name: "SESSION_TEST_BEHAVIOR".to_string(),
                    value: "sleep".to_string(),
                },
            ],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: Some(Duration::from_secs(1)),
        },
    )
    .expect("session should run");

    assert_eq!(outcome, SessionOutcome::TimedOut);
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn releases_session_secret_after_container_reaches_running_state() {
    if skip_if_podman_unavailable("releases_session_secret_after_container_reaches_running_state") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("running-secret-run");
    let image = fixture.build_image();
    let audit_root = fixture.audit_root();
    let methodology_dir = fixture.methodology_dir();
    let repo_url = fixture.repo_url();

    let session = thread::spawn(move || {
        run_session_with_test_audit_root(
            &audit_root,
            SessionSpec {
                daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
                agent_name: "running-secret-run".to_string(),
                base_image: image,
                methodology_dir,
                audit_root: audit_root.clone(),
                mounts: Vec::new(),
                agent_command: vec!["site-builder".to_string(), "exec".to_string()],
                environment: vec![
                    ResolvedEnvironmentVariable {
                        name: "GITHUB_TOKEN".to_string(),
                        value: "test-token".to_string(),
                    },
                    ResolvedEnvironmentVariable {
                        name: "SESSION_TEST_BEHAVIOR".to_string(),
                        value: "sleep-short".to_string(),
                    },
                ],
            },
            SessionInvocation {
                repo_url,
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            },
        )
    });

    let session_id = fixture.wait_for_runner_container_to_be_running(Duration::from_secs(5));
    fixture.wait_for_runner_secrets_to_be_released(&session_id, Duration::from_secs(5));

    let outcome = session
        .join()
        .expect("session thread should complete")
        .expect("session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

struct SessionFixture {
    root: PathBuf,
    agent_name: String,
    baseline_runner_secret_names: BTreeSet<String>,
    repo_server: RepoHttpServer,
}

impl SessionFixture {
    fn new(agent_name: &str) -> Self {
        Self::new_with_repo_server(agent_name, &format!("agentd-runner-{agent_name}"))
    }

    fn new_with_root_prefix(agent_name: &str, root_prefix: &str) -> Self {
        Self::new_with_repo_server(agent_name, root_prefix)
    }

    fn new_with_repo_server(agent_name: &str, root_prefix: &str) -> Self {
        let root = unique_temp_dir(root_prefix);
        fs::create_dir_all(&root).expect("fixture root should be created");

        let methodology_dir = root.join("methodology");
        fs::create_dir_all(&methodology_dir).expect("methodology directory should be created");
        fs::write(
            methodology_dir.join("manifest.toml"),
            "name = \"test-methodology\"\n",
        )
        .expect("methodology manifest should be written");
        let repo_root = root.join("repo-server");
        let bare_repo_dir = repo_root.join("repo.git");
        fs::create_dir_all(&repo_root).expect("repo root should be created");
        write_test_repo(&bare_repo_dir);

        Self {
            root,
            agent_name: agent_name.to_string(),
            baseline_runner_secret_names: list_runner_secret_names(),
            repo_server: RepoHttpServer::start(repo_root),
        }
    }

    fn methodology_dir(&self) -> PathBuf {
        self.root.join("methodology")
    }

    fn audit_root(&self) -> PathBuf {
        self.root.join("audit-root")
    }

    fn only_session_record_dir(&self) -> PathBuf {
        let agent_root = self.audit_root().join(&self.agent_name);
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

    fn repo_url(&self) -> String {
        format!(
            "http://host.containers.internal:{}/repo.git",
            self.repo_server.port()
        )
    }

    fn build_image(&self) -> String {
        self.build_image_with_agentd_work_unit_line(None)
    }

    fn build_image_with_preexisting_home_file(&self) -> String {
        self.build_image_with_customizations(
            None,
            "RUN mkdir -p /home/preexisting-home-run \\\n    && printf 'root owned fixture\\n' > /home/preexisting-home-run/.preexisting\n",
        )
    }

    fn build_image_with_agentd_work_unit(&self, work_unit: &str) -> String {
        self.build_image_with_agentd_work_unit_line(Some(work_unit))
    }

    fn build_image_with_agentd_work_unit_line(&self, work_unit: Option<&str>) -> String {
        self.build_image_with_customizations(work_unit, "")
    }

    fn build_image_with_customizations(
        &self,
        work_unit: Option<&str>,
        extra_containerfile_lines: &str,
    ) -> String {
        let context_dir = self.root.join("image-context");
        fs::create_dir_all(&context_dir).expect("image context should be created");

        fs::write(context_dir.join("site-builder"), SITE_BUILDER_STUB)
            .expect("site-builder stub should be written");
        fs::write(context_dir.join("runa"), RUNA_STUB).expect("runa stub should be written");
        fs::write(context_dir.join("entrypoint.sh"), ENTRYPOINT_SH)
            .expect("entrypoint script should be written");
        let mut containerfile = work_unit
            .map(|work_unit| CONTAINERFILE.replace(
                "FROM docker.io/library/debian:bookworm-slim\n",
                &format!("FROM docker.io/library/debian:bookworm-slim\nENV AGENTD_WORK_UNIT={work_unit}\n"),
            ))
            .unwrap_or_else(|| CONTAINERFILE.to_string());
        if !extra_containerfile_lines.is_empty() {
            containerfile.push_str(extra_containerfile_lines);
        }
        fs::write(context_dir.join("Containerfile"), containerfile)
            .expect("containerfile should be written");

        let tag = format!("agentd-runner-test:{}", self.agent_name);
        let status = Command::new("podman")
            .args(["build", "--tag", &tag, context_dir.to_str().unwrap()])
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .expect("podman build should start");

        assert!(status.success(), "podman build failed");
        tag
    }

    fn assert_no_runner_container_left_behind(&self) {
        let output = Command::new("podman")
            .args(["ps", "-a", "--format", "{{.Names}}"])
            .output()
            .expect("podman ps should run");
        assert!(
            output.status.success(),
            "podman ps failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let names = String::from_utf8(output.stdout).expect("podman ps output should be utf-8");
        let expected_prefix = format!("agentd-{TEST_DAEMON_INSTANCE_ID}-{}-", self.agent_name);
        assert!(
            !names.lines().any(|name| name.starts_with(&expected_prefix)),
            "runner left container behind with prefix {expected_prefix}: {names}"
        );
    }

    fn assert_no_runner_secret_left_behind(&self) {
        let current_runner_secret_names = list_runner_secret_names();
        let leaked_secret_names = current_runner_secret_names
            .difference(&self.baseline_runner_secret_names)
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            leaked_secret_names.is_empty(),
            "runner left secrets behind: {}",
            leaked_secret_names.join("\n")
        );
    }

    fn wait_for_runner_container_to_be_running(&self, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let expected_prefix = format!("agentd-{TEST_DAEMON_INSTANCE_ID}-{}-", self.agent_name);

        loop {
            let running_container_names = list_running_container_names();
            if let Some(session_id) = running_container_names
                .iter()
                .find_map(|name| name.strip_prefix(&expected_prefix))
            {
                return session_id.to_string();
            }

            assert!(
                Instant::now() < deadline,
                "runner container with prefix {expected_prefix} did not reach running state"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_runner_secrets_to_be_released(&self, session_id: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let expected_secret_prefix = format!("agentd-{TEST_DAEMON_INSTANCE_ID}-{session_id}-");
        let expected_container_prefix = format!(
            "agentd-{TEST_DAEMON_INSTANCE_ID}-{}-{session_id}",
            self.agent_name
        );

        loop {
            let matching_secret_names = list_runner_secret_names()
                .into_iter()
                .filter(|name| name.starts_with(&expected_secret_prefix))
                .collect::<Vec<_>>();
            let running_container_names = list_running_container_names();
            let container_is_running = running_container_names
                .iter()
                .any(|name| name == &expected_container_prefix);

            if matching_secret_names.is_empty() {
                assert!(
                    container_is_running,
                    "runner secrets for {expected_secret_prefix} were only released after the container stopped"
                );
                return;
            }

            assert!(
                container_is_running,
                "runner left secrets behind until the container stopped: {}",
                matching_secret_names.join("\n")
            );
            assert!(
                Instant::now() < deadline,
                "runner left secrets behind for {expected_secret_prefix}: {}",
                matching_secret_names.join("\n")
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for SessionFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn skip_if_podman_unavailable(test_name: &str) -> bool {
    if !podman_available() {
        eprintln!("skipping {test_name}: podman is unavailable");
        return true;
    }

    if !direct_audit_sealing_available() {
        eprintln!(
            "skipping {test_name}: current UID cannot directly chmod session-written audit files"
        );
        return true;
    }

    false
}

fn podman_available() -> bool {
    let status = Command::new("podman")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match status {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

fn direct_audit_sealing_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let _guard = podman_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        probe_direct_audit_sealing()
    })
}

fn probe_direct_audit_sealing() -> bool {
    let audit_root = unique_temp_dir("agentd-runner-direct-audit-probe");
    if fs::create_dir_all(&audit_root).is_err() {
        return false;
    }
    if fs::set_permissions(&audit_root, fs::Permissions::from_mode(0o777)).is_err() {
        let _ = fs::remove_dir_all(&audit_root);
        return false;
    }

    let mount_arg = format!("{}:/audit:Z", audit_root.display());
    // Real Podman lifecycle tests only model the supported direct-seal
    // deployment when the host test process can chmod files written by the
    // session user's mapped UID.
    let status = Command::new("podman")
        .args(probe_direct_audit_sealing_args(&mount_arg))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let can_seal = status.is_ok_and(|status| status.success())
        && fs::set_permissions(
            audit_root.join("probe-file"),
            fs::Permissions::from_mode(0o444),
        )
        .is_ok();

    let _ = fs::remove_dir_all(&audit_root);
    can_seal
}

fn probe_direct_audit_sealing_args(mount_arg: &str) -> Vec<String> {
    vec![
        "run".to_string(),
        "--rm".to_string(),
        "--userns".to_string(),
        format!("keep-id:uid={SESSION_USER_ID},gid={SESSION_GROUP_ID}"),
        "--user".to_string(),
        format!("{SESSION_USER_ID}:{SESSION_GROUP_ID}"),
        "-v".to_string(),
        mount_arg.to_string(),
        "docker.io/library/debian:bookworm-slim".to_string(),
        "sh".to_string(),
        "-lc".to_string(),
        "printf 'probe\\n' > /audit/probe-file".to_string(),
    ]
}

#[test]
fn direct_audit_sealing_probe_maps_daemon_identity_to_session_user_id() {
    let args = probe_direct_audit_sealing_args("/tmp/audit:/audit:Z");

    let userns_index = args
        .iter()
        .position(|arg| arg == "--userns")
        .expect("podman probe should receive --userns");
    assert_eq!(
        args.get(userns_index + 1).map(String::as_str),
        Some("keep-id:uid=1000,gid=1000")
    );
}

fn podman_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let unique = format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after the unix epoch")
            .as_nanos()
    );

    std::env::temp_dir().join(unique)
}

fn list_running_container_names() -> Vec<String> {
    let output = Command::new("podman")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
        .expect("podman ps should run");
    assert!(
        output.status.success(),
        "podman ps failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout)
        .expect("podman ps output should be utf-8")
        .lines()
        .map(str::to_string)
        .collect()
}

fn list_runner_secret_names() -> BTreeSet<String> {
    let output = Command::new("podman")
        .args(["secret", "ls", "--format", "{{.Name}}"])
        .output()
        .expect("podman secret ls should run");
    assert!(
        output.status.success(),
        "podman secret ls failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout)
        .expect("podman secret ls output should be utf-8")
        .lines()
        .filter(|name| name.starts_with("agentd-"))
        .map(str::to_string)
        .collect()
}

fn write_test_repo(destination: &Path) {
    let source_dir = destination
        .parent()
        .expect("repo destination should have a parent")
        .join("repo-source");
    fs::create_dir_all(&source_dir).expect("repo source directory should be created");
    fs::write(source_dir.join("README.md"), "# test repo\n")
        .expect("fixture repo readme should be written");

    run_git(&source_dir, ["init"]);
    run_git(&source_dir, ["config", "user.name", "agentd-runner-tests"]);
    run_git(
        &source_dir,
        ["config", "user.email", "agentd-runner-tests@example.com"],
    );
    run_git(&source_dir, ["add", "README.md"]);
    run_git(&source_dir, ["commit", "-m", "initial commit"]);
    run_git_in(
        destination
            .parent()
            .expect("repo destination should have a parent"),
        [
            "clone",
            "--bare",
            source_dir.to_str().unwrap(),
            destination.to_str().unwrap(),
        ],
    );
    run_git_in(destination, ["update-server-info"]);
}

fn run_git<const N: usize>(directory: &Path, args: [&str; N]) {
    run_git_in(directory, args);
}

fn run_git_in<const N: usize>(directory: &Path, args: [&str; N]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(directory)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("git command should run");

    assert!(
        status.success(),
        "git command failed in {}",
        directory.display()
    );
}

const CONTAINERFILE: &str = r#"
FROM docker.io/library/debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends findutils git gosu passwd \
    && rm -rf /var/lib/apt/lists/*
COPY site-builder /usr/local/bin/site-builder
COPY runa /usr/local/bin/runa
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /usr/local/bin/site-builder /usr/local/bin/runa /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
"#;

const ENTRYPOINT_SH: &str = r#"#!/bin/sh
set -eu

echo "image entrypoint should not run" >&2
exit 97
"#;

const RUNA_STUB: &str = r#"#!/bin/sh
set -eu

subcommand="${1:-}"
if [ "$#" -gt 0 ]; then
    shift
fi

case "$subcommand" in
    init)
        methodology=""
        while [ "$#" -gt 0 ]; do
            case "$1" in
                --methodology)
                    shift
                    methodology="${1:-}"
                    ;;
                *)
                    echo "unexpected runa init argument: $1" >&2
                    exit 97
                    ;;
            esac
            shift
        done
        [ "$methodology" = "/agentd/methodology/manifest.toml" ]
        [ -f "$methodology" ]
        mkdir -p .runa/workspace .runa/store
        cat > .runa/config.toml <<EOF
methodology = "$methodology"
EOF
        printf 'initialized = true\n' > .runa/state.toml
        printf 'init --methodology %s\n' "$methodology" >> .runa/calls.log
        if grep -F '[agent]' .runa/config.toml >/dev/null; then
            echo "runa config unexpectedly contains [agent]" >&2
            exit 96
        fi
        ;;
    run)
        work_unit=""
        while [ "$#" -gt 0 ]; do
            case "$1" in
                --work-unit)
                    shift
                    work_unit="${1:-}"
                    ;;
                --agent-command)
                    shift
                    if [ "${1:-}" = "--" ]; then
                        shift
                    fi
                    if [ "$#" -eq 0 ]; then
                        echo "missing agent command" >&2
                        exit 95
                    fi
                    if [ -n "$work_unit" ]; then
                        export AGENTD_WORK_UNIT="$work_unit"
                        printf 'run --work-unit %s --agent-command -- %s\n' "$work_unit" "$*" >> .runa/calls.log
                    else
                        printf 'run --agent-command -- %s\n' "$*" >> .runa/calls.log
                    fi
                    exec "$@"
                    ;;
                *)
                    echo "unexpected runa run argument: $1" >&2
                    exit 94
                    ;;
            esac
            shift
        done
        echo "missing --agent-command" >&2
        exit 93
        ;;
    *)
        echo "unexpected runa subcommand: $subcommand" >&2
        exit 92
        ;;
esac
"#;

struct RepoHttpServer {
    port: u16,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RepoHttpServer {
    fn start(root: PathBuf) -> Self {
        let listener = TcpListener::bind(("0.0.0.0", 0))
            .expect("fixture repo HTTP server should bind an ephemeral port");
        listener
            .set_nonblocking(true)
            .expect("fixture repo HTTP server should become nonblocking");
        let port = listener
            .local_addr()
            .expect("fixture repo HTTP server should expose a local address")
            .port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_signal = Arc::clone(&shutdown);
        let thread = thread::spawn(move || serve_repo_http(listener, root, shutdown_signal));

        Self {
            port,
            shutdown,
            thread: Some(thread),
        }
    }

    fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for RepoHttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .expect("fixture repo HTTP server thread should stop cleanly");
        }
    }
}

fn serve_repo_http(listener: TcpListener, root: PathBuf, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let root = root.clone();
                thread::spawn(move || handle_repo_http_request(stream, &root));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("fixture repo HTTP server accept failed: {error}"),
        }
    }
}

fn handle_repo_http_request(stream: TcpStream, root: &Path) {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }

    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header == "\r\n" {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let request_target = parts.next().unwrap_or_default();
    let path = request_target.split('?').next().unwrap_or_default();
    let mut stream = reader.into_inner();

    if method != "GET" && method != "HEAD" {
        write_http_response(
            &mut stream,
            "405 Method Not Allowed",
            b"method not allowed",
            false,
        );
        return;
    }

    let relative_path = path.trim_start_matches('/');
    if relative_path.is_empty() || relative_path.split('/').any(|segment| segment == "..") {
        write_http_response(&mut stream, "404 Not Found", b"not found", method == "HEAD");
        return;
    }

    let file_path = root.join(relative_path);
    let Ok(body) = fs::read(&file_path) else {
        write_http_response(&mut stream, "404 Not Found", b"not found", method == "HEAD");
        return;
    };

    write_http_response(&mut stream, "200 OK", &body, method == "HEAD");
}

fn write_http_response(stream: &mut TcpStream, status: &str, body: &[u8], head_only: bool) {
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .expect("fixture repo HTTP server should write response headers");
    if !head_only {
        stream
            .write_all(body)
            .expect("fixture repo HTTP server should write response body");
    }
}

const SITE_BUILDER_STUB: &str = r#"#!/bin/sh
set -eu

command_name="$1"
shift

case "$command_name" in
    exec)
        [ -f /agentd/methodology/manifest.toml ]
        [ "${AGENT_NAME:-}" != "" ]
        if [ "${GITHUB_TOKEN+set}" = "set" ]; then
            [ "${GITHUB_TOKEN}" = "test-token" ]
        fi
        [ "$(id -u)" != "0" ]
        [ "$(id -un)" = "${AGENT_NAME}" ]
        [ "${HOME:-}" = "/home/${AGENT_NAME}" ]
        [ "$(pwd)" = "/home/${AGENT_NAME}/repo" ]
        [ -w "${HOME}" ]
        [ -w "${HOME}/repo" ]
        [ -f "${HOME}/repo/README.md" ]

        if [ "${SESSION_TEST_BEHAVIOR:-success}" = "success" ]; then
            [ "${AGENTD_WORK_UNIT:-}" = "task-42" ]
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "success-empty-env" ]; then
            [ "${EMPTY_SESSION_ENV-__missing__}" = "" ]
            [ "${AGENTD_WORK_UNIT:-}" = "task-42" ]
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "success-without-work-unit" ]; then
            [ "${AGENTD_WORK_UNIT+set}" != "set" ]
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "assert-request-input-present" ]; then
            [ -f "${HOME}/repo/.runa/workspace/request/operator-input.json" ]
            grep -F '"description":"Add a status page"' "${HOME}/repo/.runa/workspace/request/operator-input.json"
            grep -F '"source":"operator"' "${HOME}/repo/.runa/workspace/request/operator-input.json"
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "assert-claim-input-present" ]; then
            [ -f "${HOME}/repo/.runa/workspace/claim/claim.json" ]
            grep -F '"summary":"Ship it"' "${HOME}/repo/.runa/workspace/claim/claim.json"
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "verify-read-only-mount" ]; then
            [ -f "${HOME}/.claude/auth.json" ]
            if touch "${HOME}/.claude/write-should-fail" 2>/dev/null; then
                echo "read-only mount unexpectedly allowed writes" >&2
                exit 91
            fi
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "write-read-write-mount" ]; then
            printf 'persisted from container\n' > "${HOME}/.runa/session-artifact.txt"
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "write-nested-home-mount" ]; then
            # The mkdir/write below is the regression probe: if setup leaves
            # $HOME/.config owned by root, creating the sibling git config
            # fails and we never reach the mounted-target sentinel write.
            mkdir -p "${HOME}/.config/git"
            printf 'sibling write succeeded\n' > "${HOME}/.config/git/config"
            printf 'persisted from nested mount\n' > "${HOME}/.config/claude/nested-artifact.txt"
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "write-preexisting-home-file" ]; then
            [ -f "${HOME}/.preexisting" ]
            [ "$(cat "${HOME}/.preexisting")" = "root owned fixture" ]
            printf 'session write succeeded\n' > "${HOME}/.preexisting"
            [ "$(cat "${HOME}/.preexisting")" = "session write succeeded" ]
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "write-repo-audit-state" ]; then
            [ -L "${HOME}/repo/.runa" ]
            [ "$(readlink "${HOME}/repo/.runa")" = "${HOME}/.agentd/audit/runa" ]
            [ "$(stat -Lc '%u:%g' "${HOME}/repo/.runa")" = "$(id -u):$(id -g)" ]
            [ -w "${HOME}/repo/.runa" ]
            mkdir -p "${HOME}/repo/.runa/workspace" "${HOME}/repo/.runa/store/executions"
            printf 'persisted through repo bridge\n' > "${HOME}/repo/.runa/workspace/session-artifact.txt"
            printf '{"protocols":["begin"],"postconditions":["passed"]}\n' > "${HOME}/repo/.runa/store/executions/0001.json"
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "write-restrictive-repo-audit-state" ]; then
            [ -L "${HOME}/repo/.runa" ]
            mkdir -p "${HOME}/repo/.runa/workspace/private" "${HOME}/repo/.runa/store/executions"
            chmod 0700 "${HOME}/repo/.runa/workspace/private" "${HOME}/repo/.runa/store" "${HOME}/repo/.runa/store/executions"
            printf 'host should still read this after teardown\n' > "${HOME}/repo/.runa/workspace/private/session-artifact.txt"
            chmod 0600 "${HOME}/repo/.runa/workspace/private/session-artifact.txt"
            printf '{"protocols":["begin"],"postconditions":["passed"]}\n' > "${HOME}/repo/.runa/store/executions/0001.json"
            chmod 0600 "${HOME}/repo/.runa/store/executions/0001.json"
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "write-failing-audit-trail" ]; then
            [ -L "${HOME}/repo/.runa" ]
            mkdir -p "${HOME}/repo/.runa/workspace/decompose" "${HOME}/repo/.runa/store/executions"
            printf 'draft plan\n' > "${HOME}/repo/.runa/workspace/decompose/plan.md"
            cat > "${HOME}/repo/.runa/store/executions/0001.json" <<'EOF'
{"events":[{"protocol":"begin","artifact":"claim.md","postcondition":"passed"},{"protocol":"decompose","artifact":"plan.md","postcondition":"failed"}]}
EOF
            exit 5
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "fail" ]; then
            [ "$#" = "0" ]
            exit 23
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "fail-125" ]; then
            [ "$#" = "0" ]
            exit 125
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "sleep" ]; then
            sleep 30
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "sleep-short" ]; then
            sleep 5
            exit 0
        fi

        echo "unknown SESSION_TEST_BEHAVIOR=${SESSION_TEST_BEHAVIOR:-}" >&2
        exit 99
        ;;
    *)
        echo "unexpected site-builder subcommand: $command_name" >&2
        exit 98
        ;;
esac
"#;
