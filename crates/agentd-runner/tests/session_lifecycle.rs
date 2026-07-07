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

fn nested_transcript_event_files(transcript_dir: &Path) -> Vec<PathBuf> {
    let mut event_files = Vec::new();
    collect_nested_transcript_event_files(transcript_dir, &mut event_files);
    event_files.sort();
    event_files
}

fn collect_nested_transcript_event_files(path: &Path, event_files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries {
        let entry = entry.expect("transcript directory entry should be readable");
        let path = entry.path();
        if path.is_dir() {
            collect_nested_transcript_event_files(&path, event_files);
        } else if path.file_name().and_then(|name| name.to_str()) == Some("events.jsonl")
            && path
                .components()
                .any(|component| component.as_os_str() == std::ffi::OsStr::new("deployments"))
        {
            event_files.push(path);
        }
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

fn install_intent_schema(path: &Path, version: &str) {
    let schema_dir = path.join("schemas");
    fs::create_dir_all(&schema_dir).expect("schema dir should be created");
    fs::write(
        schema_dir.join("intent.schema.json"),
        format!(
            r#"{{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "x-tesserine-canonical": {{
    "version": "{version}",
    "schema_url": "https://example.com/intent.schema.json",
    "prose_url": "https://example.com/INTENT.md"
  }},
  "type": "object",
  "required": ["statement", "source"],
  "additionalProperties": false,
  "properties": {{
    "statement": {{ "type": "string", "minLength": 1 }},
    "source": {{ "type": "string", "minLength": 1 }},
    "target": {{ "type": "string", "minLength": 1 }}
  }}
}}
"#
        ),
    )
    .expect("intent schema should be written");
}

fn install_work_unit_seed_methodology(path: &Path) {
    write_methodology_manifest(path, &["intent"]);
    install_intent_schema(path, "2.0.0");
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

fn install_work_unit_schema(path: &Path) {
    let schema_dir = path.join("schemas");
    fs::create_dir_all(&schema_dir).expect("schema dir should be created");
    fs::write(
        schema_dir.join("work-unit.schema.json"),
        r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "required": ["id", "title"],
  "additionalProperties": true,
  "properties": {
    "id": { "type": "string", "minLength": 1 },
    "title": { "type": "string", "minLength": 1 }
  }
}
"#,
    )
    .expect("work-unit schema should be written");
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
    install_work_unit_seed_methodology(&fixture.methodology_dir());
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "success-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
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
fn materializes_intent_text_input_before_session_command_runs() {
    if skip_if_podman_unavailable("materializes_intent_text_input_before_session_command_runs") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("intent-input-run");
    write_methodology_manifest(&fixture.methodology_dir(), &["intent"]);
    install_intent_schema(&fixture.methodology_dir(), "2.0.0");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "intent-input-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "assert-intent-input-present".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: Some(InvocationInput::IntentText {
                statement: "Add a status page".to_string(),
                target: None,
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
            forge_type: "github".to_string(),
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
fn executes_work_mode_against_injected_work_unit_artifact() {
    if skip_if_podman_unavailable("executes_work_mode_against_injected_work_unit_artifact") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("work-mode-artifact-run");
    write_methodology_manifest(&fixture.methodology_dir(), &["intent", "work-unit"]);
    install_intent_schema(&fixture.methodology_dir(), "2.0.0");
    install_work_unit_schema(&fixture.methodology_dir());
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "work-mode-artifact-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "execute-work-mode-cascade".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: Some("76".to_string()),
            input: Some(InvocationInput::Artifact {
                artifact_type: "work-unit".to_string(),
                artifact_id: "76".to_string(),
                document: serde_json::json!({
                    "id": "76",
                    "title": "Execute work mode",
                }),
            }),
            timeout: None,
        },
    )
    .expect("work-mode session should run");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });

    let record_dir = fixture.only_session_record_dir();
    assert_eq!(
        fs::read_to_string(record_dir.join("runa/calls.log"))
            .expect("runa call log should persist"),
        "init --methodology /agentd/methodology/manifest.toml\nrun --agent-command -- site-builder exec\n"
    );
    assert_eq!(
        serde_json::from_str::<Value>(
            &fs::read_to_string(record_dir.join("runa/workspace/intent/operator-input.json"))
                .expect("work-unit reference seed should persist"),
        )
        .expect("work-unit reference seed should be valid JSON"),
        serde_json::json!({
            "statement": "Work on the referenced work unit.",
            "source": "operator",
            "target": "76",
        })
    );
    assert!(
        record_dir.join("runa/workspace/work-unit/76.json").exists(),
        "injected work-unit artifact should persist"
    );
    for artifact_path in [
        "runa/workspace/behavior-contract/specify.json",
        "runa/workspace/implementation-plan/plan.json",
        "runa/workspace/patch/implement.json",
        "runa/workspace/test-evidence/verify.json",
        "runa/workspace/documentation-record/document.json",
        "runa/workspace/completion-record/submit.json",
        "runa/workspace/completion-record/land.json",
    ] {
        assert!(
            record_dir.join(artifact_path).exists(),
            "expected cascade artifact {artifact_path} to persist"
        );
    }
    let execution_record = fs::read_to_string(record_dir.join("runa/store/executions/0001.json"))
        .expect("execution record should persist");
    assert!(execution_record.contains("\"specify\""));
    assert!(execution_record.contains("\"land\""));

    let metadata: Value = serde_json::from_str(
        &fs::read_to_string(record_dir.join("agentd/session.json"))
            .expect("session metadata should persist"),
    )
    .expect("session metadata should be valid json");
    assert_eq!(metadata["work_unit"], "76");
    assert_eq!(metadata["outcome"], "success");
    assert_eq!(metadata["exit_code"], 0);

    let transcript_manifest: Value = serde_json::from_str(
        &fs::read_to_string(record_dir.join("agentd/transcript/manifest.json"))
            .expect("transcript manifest should persist"),
    )
    .expect("transcript manifest should be valid json");
    assert_eq!(transcript_manifest["coverage"], "full");

    let session_id = record_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("session record dir should be named by the session id");
    let event_files = nested_transcript_event_files(&record_dir.join("agentd/transcript"));
    assert_eq!(
        event_files.len(),
        1,
        "multi-stage runa events should share one agentd-owned run segment: {event_files:?}"
    );
    let event_path = event_files
        .first()
        .expect("full transcript coverage should have an event file");
    let path_components = event_path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let run_component_index = path_components
        .iter()
        .position(|component| component == "runs")
        .expect("nested transcript event path should include runs component")
        + 1;
    assert_eq!(
        path_components.get(run_component_index),
        Some(&session_id.to_string()),
        "runa must honor agentd's session-stable RUNA_TRANSCRIPT_RUN_ID: {}",
        event_path.display()
    );

    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn rejects_intent_text_when_methodology_declares_a_legacy_intent_version() {
    if skip_if_podman_unavailable(
        "rejects_intent_text_when_methodology_declares_a_legacy_intent_version",
    ) {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("legacy-intent-version-run");
    write_methodology_manifest(&fixture.methodology_dir(), &["intent"]);
    install_intent_schema(&fixture.methodology_dir(), "1.0.0");
    let image = fixture.build_image();

    let error = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "legacy-intent-version-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: Vec::new(),
        },
        SessionInvocation {
            repo_url: fixture.repo_url(),
            repo_token: None,
            work_unit: None,
            input: Some(InvocationInput::IntentText {
                statement: "Add a status page".to_string(),
                target: None,
            }),
            timeout: None,
        },
    )
    .expect_err("unsupported intent version should fail before session start");

    assert!(
        error
            .to_string()
            .contains("canonical intent version 1.0.0 is not supported"),
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
    install_work_unit_seed_methodology(&fixture.methodology_dir());
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "mixed-env-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
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
            forge_type: "github".to_string(),
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
            forge_type: "github".to_string(),
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
            forge_type: "github".to_string(),
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
    install_work_unit_seed_methodology(&fixture.methodology_dir());
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "comma-methodology-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
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
    relabel_container_mount_source_if_possible(&host_mount);

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "readonly-mount-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
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
    relabel_container_mount_source_if_possible(&host_mount);

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "readwrite-mount-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
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
    relabel_container_mount_source_if_possible(&host_mount);

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "nested-home-mount-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
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
            forge_type: "github".to_string(),
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
    install_work_unit_seed_methodology(&fixture.methodology_dir());
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "audit-success-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
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
            work_unit: Some("76".to_string()),
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
        "init --methodology /agentd/methodology/manifest.toml\nrun --agent-command -- site-builder exec\n"
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
    assert_eq!(metadata["work_unit"], "76");
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
fn persists_session_transcript_under_agentd_audit_dir() {
    if skip_if_podman_unavailable("persists_session_transcript_under_agentd_audit_dir") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("audit-transcript-run");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "audit-transcript-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
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

    let transcript_dir = fixture.only_session_record_dir().join("agentd/transcript");
    assert!(
        !transcript_dir.join("events.jsonl").exists(),
        "agentd should not use a flat transcript event stream"
    );
    let event_files = nested_transcript_event_files(&transcript_dir);
    assert_eq!(event_files.len(), 1, "expected one nested runa event file");
    let events =
        fs::read_to_string(&event_files[0]).expect("structured transcript events should persist");
    assert!(events.contains("\"kind\":\"agent_input\""), "{events}");
    assert!(events.contains("\"kind\":\"agent_exit\""), "{events}");

    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(transcript_dir.join("manifest.json"))
            .expect("transcript manifest should persist"),
    )
    .expect("transcript manifest should be json");
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["coverage"], "missing_mcp_events");
    assert_eq!(manifest["event_schema_versions"], serde_json::json!([2]));

    let markdown = fs::read_to_string(transcript_dir.join("transcript.md"))
        .expect("human-readable transcript should persist");
    assert!(markdown.contains("# Session Transcript"), "{markdown}");
    assert!(markdown.contains("agent_input"), "{markdown}");

    let events_mode = fs::metadata(&event_files[0])
        .expect("events permissions should exist")
        .permissions()
        .mode();
    assert_eq!(events_mode & 0o222, 0, "transcript should be sealed");

    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn finalizes_session_after_runtime_restricts_transcript_directory_permissions() {
    if skip_if_podman_unavailable(
        "finalizes_session_after_runtime_restricts_transcript_directory_permissions",
    ) {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("audit-transcript-dir-mode");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "audit-transcript-dir-mode".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "restrict-transcript-dir".to_string(),
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

    let record_dir = fixture.only_session_record_dir();
    let transcript_dir = record_dir.join("agentd/transcript");
    let manifest_result = fs::read_to_string(transcript_dir.join("manifest.json"));
    if manifest_result.is_err() {
        let _ = fs::set_permissions(&transcript_dir, fs::Permissions::from_mode(0o755));
    }
    let manifest: Value =
        serde_json::from_str(&manifest_result.expect("transcript manifest should persist"))
            .expect("transcript manifest should be json");
    assert_eq!(manifest["coverage"], "missing_mcp_events");
    assert_eq!(manifest["event_schema_versions"], serde_json::json!([2]));

    let markdown = fs::read_to_string(transcript_dir.join("transcript.md"))
        .expect("human-readable transcript should persist");
    assert!(markdown.contains("agent_input"), "{markdown}");

    let session: Value = serde_json::from_str(
        &fs::read_to_string(record_dir.join("agentd/session.json"))
            .expect("session metadata should persist"),
    )
    .expect("session metadata should be json");
    assert_eq!(session["outcome"], "success");

    let transcript_mode = fs::metadata(&transcript_dir)
        .expect("transcript dir metadata should exist")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(transcript_mode, 0o555);

    fixture.assert_no_runner_container_left_behind();
    fixture.assert_no_runner_secret_left_behind();
}

#[test]
fn finalizes_session_after_runtime_restricts_events_jsonl_permissions() {
    if skip_if_podman_unavailable(
        "finalizes_session_after_runtime_restricts_events_jsonl_permissions",
    ) {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("audit-transcript-events-mode");
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "audit-transcript-events-mode".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "restrict-transcript-events".to_string(),
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

    let record_dir = fixture.only_session_record_dir();
    let transcript_dir = record_dir.join("agentd/transcript");
    let manifest_result = fs::read_to_string(transcript_dir.join("manifest.json"));
    let manifest: Value =
        serde_json::from_str(&manifest_result.expect("transcript manifest should persist"))
            .expect("transcript manifest should be json");
    assert_eq!(manifest["coverage"], "missing_mcp_events");
    assert_eq!(manifest["event_schema_versions"], serde_json::json!([2]));

    let event_files = nested_transcript_event_files(&transcript_dir);
    assert_eq!(event_files.len(), 1, "expected one nested runa event file");
    let events_path = &event_files[0];
    let events = fs::read_to_string(events_path).expect("structured transcript should persist");
    assert!(events.contains("\"kind\":\"agent_input\""), "{events}");

    let session: Value = serde_json::from_str(
        &fs::read_to_string(record_dir.join("agentd/session.json"))
            .expect("session metadata should persist"),
    )
    .expect("session metadata should be json");
    assert_eq!(session["outcome"], "success");

    let events_mode = fs::metadata(events_path)
        .expect("events metadata should exist")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(events_mode, 0o444);

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
    install_work_unit_seed_methodology(&fixture.methodology_dir());
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "audit-restrictive-modes-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
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
            work_unit: Some("76".to_string()),
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
    relabel_container_mount_source_if_possible(&host_mount);

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
            forge_type: "github".to_string(),
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
    install_work_unit_seed_methodology(&fixture.methodology_dir());
    let image = fixture.build_image();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "audit-failure-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
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
            work_unit: Some("76".to_string()),
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
            forge_type: "github".to_string(),
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
                forge_type: "github".to_string(),
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

#[test]
fn clones_ssh_repo_with_agent_scoped_mounted_identity() {
    if skip_if_podman_unavailable("clones_ssh_repo_with_agent_scoped_mounted_identity") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("ssh-clone-run");
    let ssh_server = SshGitServer::start(&fixture);
    let ssh_dir = fixture.create_ssh_client_dir(ssh_server.port());
    let image = fixture.build_image_with_ssh_client();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "ssh-clone-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
            mounts: vec![BindMount {
                source: ssh_dir,
                target: PathBuf::from("/home/ssh-clone-run/.ssh"),
                read_only: true,
            }],
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: vec![ResolvedEnvironmentVariable {
                name: "SESSION_TEST_BEHAVIOR".to_string(),
                value: "success-without-work-unit".to_string(),
            }],
        },
        SessionInvocation {
            repo_url: ssh_server.repo_url(),
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
fn ssh_repo_clone_cannot_use_another_agents_unmounted_identity() {
    if skip_if_podman_unavailable("ssh_repo_clone_cannot_use_another_agents_unmounted_identity") {
        return;
    }
    let _guard = podman_test_lock()
        .lock()
        .expect("podman test lock should be acquired");

    let fixture = SessionFixture::new("ssh-no-identity-run");
    let ssh_server = SshGitServer::start(&fixture);
    let image = fixture.build_image_with_ssh_client();

    let outcome = run_session_with_test_audit_root(
        &fixture.audit_root(),
        SessionSpec {
            daemon_instance_id: TEST_DAEMON_INSTANCE_ID.to_string(),
            agent_name: "ssh-no-identity-run".to_string(),
            base_image: image,
            methodology_dir: fixture.methodology_dir(),
            audit_root: fixture.audit_root(),
            forge_type: "github".to_string(),
            mounts: Vec::new(),
            agent_command: vec!["site-builder".to_string(), "exec".to_string()],
            environment: Vec::new(),
        },
        SessionInvocation {
            repo_url: ssh_server.repo_url(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
    )
    .expect("session should run");

    assert_ne!(outcome, SessionOutcome::Success { exit_code: 0 });
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

    fn bare_repo_dir(&self) -> PathBuf {
        self.root.join("repo-server/repo.git")
    }

    fn create_ssh_client_dir(&self, ssh_port: u16) -> PathBuf {
        let ssh_dir = self.root.join("ssh-client");
        fs::create_dir_all(&ssh_dir).expect("ssh client dir should be created");
        fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o755))
            .expect("ssh client dir should be traversable by the session user");
        write_file_with_mode(
            &ssh_dir.join("id_ed25519"),
            TEST_SSH_CLIENT_PRIVATE_KEY,
            0o600,
        );
        write_file_with_mode(
            &ssh_dir.join("known_hosts"),
            &format!("[host.containers.internal]:{ssh_port} {TEST_SSH_HOST_PUBLIC_KEY}\n"),
            0o644,
        );
        write_file_with_mode(
            &ssh_dir.join("config"),
            "Host *\n    IdentityFile ~/.ssh/id_ed25519\n    IdentitiesOnly yes\n    StrictHostKeyChecking yes\n    UserKnownHostsFile ~/.ssh/known_hosts\n",
            0o600,
        );
        relabel_container_mount_source_if_possible(&ssh_dir);
        ssh_dir
    }

    fn build_image(&self) -> String {
        self.build_image_with_agentd_work_unit_line(None)
    }

    fn build_image_with_ssh_client(&self) -> String {
        self.build_image_with_customizations(
            None,
            "RUN apt-get update \\\n    && apt-get install -y --no-install-recommends openssh-client \\\n    && rm -rf /var/lib/apt/lists/*\n",
        )
    }

    fn build_ssh_server_image(&self) -> String {
        let context_dir = self.root.join("ssh-server-image-context");
        fs::create_dir_all(&context_dir).expect("ssh server image context should be created");
        write_file_with_mode(
            &context_dir.join("ssh_host_ed25519_key"),
            TEST_SSH_HOST_PRIVATE_KEY,
            0o600,
        );
        write_file_with_mode(
            &context_dir.join("authorized_keys"),
            TEST_SSH_CLIENT_PUBLIC_KEY,
            0o644,
        );
        fs::write(context_dir.join("Containerfile"), SSH_SERVER_CONTAINERFILE)
            .expect("ssh server containerfile should be written");

        let tag = format!("agentd-runner-ssh-server-test:{}", self.agent_name);
        let status = Command::new("podman")
            .args(["build", "--tag", &tag, context_dir.to_str().unwrap()])
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .expect("podman ssh server build should start");

        assert!(status.success(), "podman ssh server build failed");
        tag
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

struct SshGitServer {
    container_name: String,
    port: u16,
}

impl SshGitServer {
    fn start(fixture: &SessionFixture) -> Self {
        let image = fixture.build_ssh_server_image();
        let container_name = format!("agentd-runner-ssh-server-{}", fixture.agent_name);
        let status = Command::new("podman")
            .args([
                "run",
                "--detach",
                "--name",
                &container_name,
                "--publish",
                "0.0.0.0::22",
                "--volume",
                &format!(
                    "{}:/srv/git/repo.git:ro,Z",
                    fixture.bare_repo_dir().display()
                ),
                &image,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .expect("podman run ssh server should start");
        assert!(status.success(), "podman ssh server run failed");

        let server = Self {
            container_name,
            port: podman_container_port(&fixture.agent_name, 22),
        };
        server.wait_until_ready();
        server
    }

    fn repo_url(&self) -> String {
        format!(
            "ssh://git@host.containers.internal:{}/srv/git/repo.git",
            self.port
        )
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = Command::new("podman")
                .args([
                    "exec",
                    &self.container_name,
                    "sh",
                    "-c",
                    "test -s /run/sshd.pid",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("podman exec ssh readiness probe should run");
            if status.success() {
                return;
            }

            assert!(
                Instant::now() < deadline,
                "timed out waiting for ssh git server {}",
                self.container_name
            );
            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for SshGitServer {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["rm", "--force", "--ignore", &self.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn podman_container_port(agent_name: &str, container_port: u16) -> u16 {
    let container_name = format!("agentd-runner-ssh-server-{agent_name}");
    let output = Command::new("podman")
        .args(["port", &container_name, &format!("{container_port}/tcp")])
        .output()
        .expect("podman port should run");
    assert!(
        output.status.success(),
        "podman port failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("podman port output should be utf-8");
    let endpoint = stdout
        .lines()
        .next()
        .expect("podman port should report an endpoint")
        .trim();
    endpoint
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("podman port output should end with a port: {endpoint}"))
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

fn write_file_with_mode(path: &Path, contents: &str, mode: u32) {
    fs::write(path, contents).unwrap_or_else(|error| {
        panic!("failed to write {}: {error}", path.display());
    });
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap_or_else(|error| {
        panic!("failed to set permissions on {}: {error}", path.display());
    });
}

fn relabel_container_mount_source_if_possible(path: &Path) {
    let Ok(status) = Command::new("chcon")
        .args(["-R", "-t", "container_file_t", path.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    else {
        return;
    };
    if status.success() || selinux_is_not_enforcing() {
        return;
    }

    panic!(
        "failed to label {} as container_file_t for Podman bind mount access",
        path.display()
    );
}

fn selinux_is_not_enforcing() -> bool {
    Command::new("getenforce")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_none_or(|state| state.trim() != "Enforcing")
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

const SSH_SERVER_CONTAINERFILE: &str = r#"
FROM docker.io/library/debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends git openssh-server \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /bin/sh git \
    && mkdir -p /run/sshd /home/git/.ssh \
    && git config --system --add safe.directory /srv/git/repo.git
COPY ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key
COPY authorized_keys /home/git/.ssh/authorized_keys
RUN chown root:root /etc/ssh/ssh_host_ed25519_key \
    && chmod 600 /etc/ssh/ssh_host_ed25519_key \
    && chown -R git:git /home/git/.ssh \
    && chmod 700 /home/git/.ssh \
    && chmod 600 /home/git/.ssh/authorized_keys
CMD ["/usr/sbin/sshd", "-D", "-e", "-p", "22", "-o", "HostKey=/etc/ssh/ssh_host_ed25519_key", "-o", "PasswordAuthentication=no", "-o", "PermitRootLogin=no", "-o", "PubkeyAuthentication=yes", "-o", "StrictModes=no"]
"#;

const TEST_SSH_CLIENT_PRIVATE_KEY: &str = r#"-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACChQZdJT2G6+ueG6I+nHXf6ZtsyYna9psMKOwB7qx0N1QAAAJhVmstGVZrL
RgAAAAtzc2gtZWQyNTUxOQAAACChQZdJT2G6+ueG6I+nHXf6ZtsyYna9psMKOwB7qx0N1Q
AAAEAE/pe0Mhtfy8QujE6l8Vyh7VHGxB7si8JkLhLMi+fp7aFBl0lPYbr654boj6cdd/pm
2zJidr2mwwo7AHurHQ3VAAAAEmFnZW50ZC10ZXN0LWNsaWVudAECAw==
-----END OPENSSH PRIVATE KEY-----
"#;

const TEST_SSH_CLIENT_PUBLIC_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKFBl0lPYbr654boj6cdd/pm2zJidr2mwwo7AHurHQ3V agentd-test-client\n";

const TEST_SSH_HOST_PRIVATE_KEY: &str = r#"-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACDfgtH3MwyDyxDt+nGAh5+ype2n1R9qlQlx6b1LZct4ugAAAJhmHI3OZhyN
zgAAAAtzc2gtZWQyNTUxOQAAACDfgtH3MwyDyxDt+nGAh5+ype2n1R9qlQlx6b1LZct4ug
AAAEAm3lPHtt2jzYTHKaXkmLdcuaj+Q5fIMlw04LocpTcmk9+C0fczDIPLEO36cYCHn7Kl
7afVH2qVCXHpvUtly3i6AAAAEGFnZW50ZC10ZXN0LWhvc3QBAgMEBQ==
-----END OPENSSH PRIVATE KEY-----
"#;

const TEST_SSH_HOST_PUBLIC_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN+C0fczDIPLEO36cYCHn7Kl7afVH2qVCXHpvUtly3i6 agentd-test-host";

const ENTRYPOINT_SH: &str = r#"#!/bin/sh
set -eu

echo "image entrypoint should not run" >&2
exit 97
"#;

const RUNA_STUB: &str = r#"#!/bin/sh
set -eu

transcript_events_file() {
    work_unit_component="${1:-_unscoped}"
    stage_component="${2:-stage}"
    if [ -n "${RUNA_TRANSCRIPT_RUN_ID:-}" ]; then
        run_id="${RUNA_TRANSCRIPT_RUN_ID}"
    else
        run_id="run-${stage_component}-1"
    fi
    printf '%s/deployments/%s/work-units/%s/runs/%s/events.jsonl' \
        "${RUNA_TRANSCRIPT_DIR:?}" \
        "${RUNA_TRANSCRIPT_DEPLOYMENT:?}" \
        "$work_unit_component" \
        "$run_id"
}

append_transcript_event() {
    event_file="$(transcript_events_file "$1" "$2")"
    mkdir -p "$(dirname "$event_file")"
    printf '%s\n' "$3" >> "$event_file"
}

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
                    if [ -n "${RUNA_TRANSCRIPT_DIR:-}" ]; then
                        append_transcript_event "$work_unit" "specify" '{"schema_version":2,"source":"runa","kind":"agent_input","content":"stub prompt"}'
                        append_transcript_event "$work_unit" "land" '{"schema_version":2,"source":"runa","kind":"agent_exit","success":true}'
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

transcript_events_file() {
    work_unit_component="${AGENTD_WORK_UNIT:-_unscoped}"
    if [ -n "${RUNA_TRANSCRIPT_RUN_ID:-}" ]; then
        run_id="${RUNA_TRANSCRIPT_RUN_ID}"
    else
        run_id="run-agent-command-1"
    fi
    printf '%s/deployments/%s/work-units/%s/runs/%s/events.jsonl' \
        "${RUNA_TRANSCRIPT_DIR:?}" \
        "${RUNA_TRANSCRIPT_DEPLOYMENT:?}" \
        "$work_unit_component" \
        "$run_id"
}

append_transcript_event() {
    event_file="$(transcript_events_file)"
    mkdir -p "$(dirname "$event_file")"
    printf '%s\n' "$1" >> "$event_file"
}

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

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "restrict-transcript-dir" ]; then
            chmod 000 "${RUNA_TRANSCRIPT_DIR}"
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "restrict-transcript-events" ]; then
            chmod 000 "$(transcript_events_file)"
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "assert-intent-input-present" ]; then
            [ -f "${HOME}/repo/.runa/workspace/intent/operator-input.json" ]
            grep -F '"statement":"Add a status page"' "${HOME}/repo/.runa/workspace/intent/operator-input.json"
            grep -F '"source":"operator"' "${HOME}/repo/.runa/workspace/intent/operator-input.json"
            if grep -F '"target"' "${HOME}/repo/.runa/workspace/intent/operator-input.json"; then
                echo "intent input unexpectedly contained target" >&2
                exit 90
            fi
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "assert-claim-input-present" ]; then
            [ -f "${HOME}/repo/.runa/workspace/claim/claim.json" ]
            grep -F '"summary":"Ship it"' "${HOME}/repo/.runa/workspace/claim/claim.json"
            exit 0
        fi

        if [ "${SESSION_TEST_BEHAVIOR:-}" = "execute-work-mode-cascade" ]; then
            [ "${AGENTD_WORK_UNIT:-}" = "76" ]
            [ -f "${HOME}/repo/.runa/workspace/work-unit/76.json" ]
            grep -F '"id":"76"' "${HOME}/repo/.runa/workspace/work-unit/76.json"
            mkdir -p \
                "${HOME}/repo/.runa/workspace/behavior-contract" \
                "${HOME}/repo/.runa/workspace/implementation-plan" \
                "${HOME}/repo/.runa/workspace/patch" \
                "${HOME}/repo/.runa/workspace/test-evidence" \
                "${HOME}/repo/.runa/workspace/documentation-record" \
                "${HOME}/repo/.runa/workspace/completion-record" \
                "${HOME}/repo/.runa/store/executions"
            printf '{"scenario":"Given a work-unit artifact, when work mode starts, then specify runs"}\n' > "${HOME}/repo/.runa/workspace/behavior-contract/specify.json"
            printf '{"decision":"execute the injected work-unit through work mode"}\n' > "${HOME}/repo/.runa/workspace/implementation-plan/plan.json"
            printf '{"status":"implemented"}\n' > "${HOME}/repo/.runa/workspace/patch/implement.json"
            printf '{"status":"verified"}\n' > "${HOME}/repo/.runa/workspace/test-evidence/verify.json"
            printf '{"status":"documented"}\n' > "${HOME}/repo/.runa/workspace/documentation-record/document.json"
            printf '{"status":"submitted"}\n' > "${HOME}/repo/.runa/workspace/completion-record/submit.json"
            printf '{"status":"landed"}\n' > "${HOME}/repo/.runa/workspace/completion-record/land.json"
            printf '{"events":[{"protocol":"specify","artifact":"behavior-contract","postcondition":"passed"},{"protocol":"plan","artifact":"implementation-plan","postcondition":"passed"},{"protocol":"implement","artifact":"patch","postcondition":"passed"},{"protocol":"verify","artifact":"test-evidence","postcondition":"passed"},{"protocol":"document","artifact":"documentation-record","postcondition":"passed"},{"protocol":"submit","artifact":"completion-record","postcondition":"passed"},{"protocol":"land","artifact":"completion-record","postcondition":"passed"}]}\n' > "${HOME}/repo/.runa/store/executions/0001.json"
            if [ -n "${RUNA_TRANSCRIPT_DIR:-}" ]; then
                append_transcript_event '{"schema_version":2,"source":"runa-mcp","kind":"tool_call","protocol":"take"}'
            fi
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
