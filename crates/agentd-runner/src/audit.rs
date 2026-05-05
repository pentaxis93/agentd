use crate::{RunnerError, SessionInvocation, SessionOutcome, SessionSpec};
use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const METADATA_SCHEMA_VERSION: u32 = 2;
const ACTIVE_AUDIT_DIRECTORY_MODE: u32 = 0o755;
const SEALED_FILE_MODE: u32 = 0o444;
const SEALED_DIRECTORY_MODE: u32 = 0o555;
const TRANSCRIPT_SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
std::thread_local! {
    static FAIL_SYNC_PARENT_DIR_CALL_FOR_TESTS: Cell<usize> = const { Cell::new(0) };
    static SYNC_PARENT_DIR_CALL_COUNT_FOR_TESTS: Cell<usize> = const { Cell::new(0) };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionAuditRecord {
    pub(crate) record_dir: PathBuf,
    pub(crate) runa_dir: PathBuf,
    pub(crate) transcript_dir: PathBuf,
    pub(crate) metadata_path: PathBuf,
    pub(crate) session_id: String,
    pub(crate) agent: String,
    pub(crate) repo_url: String,
    pub(crate) work_unit: Option<String>,
    pub(crate) start_timestamp: String,
}

pub(crate) enum SessionAuditCompletion<'a> {
    Outcome(&'a SessionOutcome),
    Error,
}

#[derive(Debug, Serialize)]
struct SessionAuditMetadata<'a> {
    schema_version: u32,
    session_id: &'a str,
    agent: &'a str,
    repo_url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    work_unit: Option<&'a str>,
    start_timestamp: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_timestamp: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}

pub(crate) fn prepare_session_audit_record(
    session_id: &str,
    spec: &SessionSpec,
    invocation: &SessionInvocation,
) -> Result<SessionAuditRecord, RunnerError> {
    prepare_session_audit_record_at(&spec.audit_root, session_id, spec, invocation)
}

fn prepare_session_audit_record_at(
    host_root: &Path,
    session_id: &str,
    spec: &SessionSpec,
    invocation: &SessionInvocation,
) -> Result<SessionAuditRecord, RunnerError> {
    let agent_dir = host_root.join(&spec.agent_name);
    let record_dir = agent_dir.join(session_id);
    let runa_dir = record_dir.join("runa");
    let agentd_dir = record_dir.join("agentd");
    let transcript_dir = agentd_dir.join("transcript");
    let metadata_path = agentd_dir.join("session.json");

    fs::create_dir_all(&runa_dir)?;
    fs::create_dir_all(&transcript_dir)?;

    rollback_record_dir_on_error(&record_dir, || {
        set_active_audit_directory_permissions(&agent_dir)?;
        set_active_audit_directory_permissions(&record_dir)?;
        set_active_audit_directory_permissions(&agentd_dir)?;
        set_active_runa_permissions(&transcript_dir)?;
        set_active_runa_permissions(&runa_dir)?;

        let start_timestamp = current_timestamp()?;
        let record = SessionAuditRecord {
            record_dir: record_dir.clone(),
            runa_dir: runa_dir.clone(),
            transcript_dir: transcript_dir.clone(),
            metadata_path: metadata_path.clone(),
            session_id: session_id.to_string(),
            agent: spec.agent_name.clone(),
            repo_url: invocation.repo_url.clone(),
            work_unit: invocation.work_unit.clone(),
            start_timestamp,
        };

        write_session_audit_metadata(&record, None, None, None)?;
        Ok(record)
    })
}

pub(crate) fn finalize_session_audit_record(
    record: &SessionAuditRecord,
    completion: SessionAuditCompletion<'_>,
) -> Result<(), RunnerError> {
    prepare_audit_tree_for_traversal(&record.record_dir)?;
    preflight_validate_sealable_tree(&record.record_dir)?;
    let (outcome, exit_code) = match completion {
        SessionAuditCompletion::Outcome(outcome) => (Some(outcome.label()), outcome.exit_code()),
        SessionAuditCompletion::Error => (Some("error"), None),
    };
    let end_timestamp = current_timestamp()?;

    seal_session_audit_record(record)?;
    write_finalized_session_audit_metadata(record, &end_timestamp, outcome, exit_code)
}

pub(crate) fn finalize_session_transcript(record: &SessionAuditRecord) -> Result<(), RunnerError> {
    fs::create_dir_all(&record.transcript_dir)?;
    let events_path = record.transcript_dir.join("events.jsonl");
    let events = match fs::read_to_string(&events_path) {
        Ok(events) => events,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let coverage = transcript_coverage(&events);
    write_transcript_manifest(record, coverage)?;
    write_transcript_markdown(record, &events)?;
    Ok(())
}

fn write_session_audit_metadata(
    record: &SessionAuditRecord,
    end_timestamp: Option<&str>,
    outcome: Option<&str>,
    exit_code: Option<i32>,
) -> Result<(), RunnerError> {
    write_session_audit_metadata_with_mode(record, end_timestamp, outcome, exit_code, None)
}

fn write_finalized_session_audit_metadata(
    record: &SessionAuditRecord,
    end_timestamp: &str,
    outcome: Option<&str>,
    exit_code: Option<i32>,
) -> Result<(), RunnerError> {
    write_session_audit_metadata_with_mode(
        record,
        Some(end_timestamp),
        outcome,
        exit_code,
        Some(SEALED_FILE_MODE),
    )
}

fn write_session_audit_metadata_with_mode(
    record: &SessionAuditRecord,
    end_timestamp: Option<&str>,
    outcome: Option<&str>,
    exit_code: Option<i32>,
    file_mode: Option<u32>,
) -> Result<(), RunnerError> {
    let metadata = SessionAuditMetadata {
        schema_version: METADATA_SCHEMA_VERSION,
        session_id: &record.session_id,
        agent: &record.agent,
        repo_url: &record.repo_url,
        work_unit: record.work_unit.as_deref(),
        start_timestamp: &record.start_timestamp,
        end_timestamp,
        outcome,
        exit_code,
    };
    let mut payload = serde_json::to_vec_pretty(&metadata)
        .map_err(|error| RunnerError::Io(std::io::Error::other(error)))?;
    payload.push(b'\n');
    write_atomic(&record.metadata_path, &payload, file_mode)?;
    Ok(())
}

fn current_timestamp() -> Result<String, RunnerError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|error| RunnerError::Io(std::io::Error::other(error)))
}

#[derive(Debug, Serialize)]
struct TranscriptManifest<'a> {
    schema_version: u32,
    coverage: &'a str,
}

fn write_transcript_manifest(
    record: &SessionAuditRecord,
    coverage: &str,
) -> Result<(), RunnerError> {
    let manifest = TranscriptManifest {
        schema_version: TRANSCRIPT_SCHEMA_VERSION,
        coverage,
    };
    let mut payload = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| RunnerError::Io(std::io::Error::other(error)))?;
    payload.push(b'\n');
    fs::write(record.transcript_dir.join("manifest.json"), payload)?;
    Ok(())
}

fn write_transcript_markdown(record: &SessionAuditRecord, events: &str) -> Result<(), RunnerError> {
    let mut markdown = String::from("# Session Transcript\n\n");
    if events.trim().is_empty() {
        markdown.push_str("_No structured transcript events were emitted._\n");
    } else {
        for line in events.lines().filter(|line| !line.trim().is_empty()) {
            match serde_json::from_str::<Value>(line) {
                Ok(event) => {
                    let kind = event.get("kind").and_then(Value::as_str).unwrap_or("event");
                    markdown.push_str("## ");
                    markdown.push_str(kind);
                    markdown.push_str("\n\n");
                    if let Some(content) = event.get("content").and_then(Value::as_str) {
                        markdown.push_str("```text\n");
                        markdown.push_str(content);
                        if !content.ends_with('\n') {
                            markdown.push('\n');
                        }
                        markdown.push_str("```\n\n");
                    } else {
                        markdown.push_str("```json\n");
                        markdown.push_str(line);
                        markdown.push_str("\n```\n\n");
                    }
                }
                Err(_) => {
                    markdown.push_str("## unparsed_event\n\n```text\n");
                    markdown.push_str(line);
                    markdown.push_str("\n```\n\n");
                }
            }
        }
    }
    fs::write(record.transcript_dir.join("transcript.md"), markdown)?;
    Ok(())
}

fn transcript_coverage(events: &str) -> &'static str {
    if events.trim().is_empty() {
        return "outer_streams_only";
    }
    if events
        .lines()
        .any(|line| line.contains("\"source\":\"runa-mcp\""))
    {
        "full"
    } else {
        "missing_mcp_events"
    }
}

fn rollback_record_dir_on_error<T, F>(record_dir: &Path, init: F) -> Result<T, RunnerError>
where
    F: FnOnce() -> Result<T, RunnerError>,
{
    match init() {
        Ok(value) => Ok(value),
        Err(error) => {
            let _ = fs::remove_dir_all(record_dir);
            Err(error)
        }
    }
}

fn set_active_runa_permissions(path: &Path) -> Result<(), RunnerError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o777))?;
    Ok(())
}

fn set_active_audit_directory_permissions(path: &Path) -> Result<(), RunnerError> {
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(ACTIVE_AUDIT_DIRECTORY_MODE),
    )?;
    Ok(())
}

fn seal_session_audit_record(record: &SessionAuditRecord) -> Result<(), RunnerError> {
    seal_path_recursive(record, &record.record_dir)
}

fn prepare_audit_tree_for_traversal(path: &Path) -> Result<(), RunnerError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(());
    }

    let mode = metadata.permissions().mode();
    let traversal_mode = mode | SEALED_DIRECTORY_MODE;
    if traversal_mode != mode {
        fs::set_permissions(path, fs::Permissions::from_mode(traversal_mode))?;
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        prepare_audit_tree_for_traversal(&entry.path())?;
    }

    Ok(())
}

fn preflight_validate_sealable_tree(path: &Path) -> Result<(), RunnerError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            preflight_validate_sealable_tree(&entry.path())?;
        }
        return Ok(());
    }

    if metadata.nlink() > 1 {
        return Err(RunnerError::Io(std::io::Error::other(format!(
            "refusing to seal multi-linked audit entry {}",
            path.display()
        ))));
    }

    Ok(())
}

fn seal_path_recursive(record: &SessionAuditRecord, path: &Path) -> Result<(), RunnerError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            seal_path_recursive(record, &entry.path())?;
        }
    }

    if should_skip_sealing_path(record, path) {
        return Ok(());
    }

    seal_path(path, metadata.is_dir())
}

fn seal_path(path: &Path, is_dir: bool) -> Result<(), RunnerError> {
    let sealed_mode = if is_dir {
        SEALED_DIRECTORY_MODE
    } else {
        SEALED_FILE_MODE
    };
    fs::set_permissions(path, fs::Permissions::from_mode(sealed_mode))?;
    Ok(())
}

fn should_skip_sealing_path(record: &SessionAuditRecord, path: &Path) -> bool {
    path == record.record_dir
        || path == record.metadata_path
        || record
            .metadata_path
            .parent()
            .is_some_and(|metadata_dir| path == metadata_dir)
}

fn write_atomic(path: &Path, payload: &[u8], file_mode: Option<u32>) -> Result<(), RunnerError> {
    let temp_path = path.with_extension("json.tmp");
    let parent = path.parent().ok_or_else(|| {
        RunnerError::Io(std::io::Error::other(
            "audit metadata path must have a parent directory",
        ))
    })?;
    let write_result = (|| -> Result<(), RunnerError> {
        let mut temp_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)?;
        temp_file.write_all(payload)?;
        if let Some(file_mode) = file_mode {
            temp_file.set_permissions(fs::Permissions::from_mode(file_mode))?;
        }
        temp_file.sync_all()?;
        drop(temp_file);

        // The atomic rename publishes the finalized metadata. A later parent
        // directory sync failure is a durability warning, not a correctness
        // failure.
        fs::rename(&temp_path, path)?;
        if let Err(error) = sync_parent_dir(parent) {
            tracing::warn!(
                event = "runner.audit_warning",
                warning_kind = "post_rename_parent_sync",
                metadata_path = %path.display(),
                parent_dir = %parent.display(),
                error = %error,
                "parent directory sync failed after atomic audit metadata publish"
            );
        }
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    write_result
}

fn sync_parent_dir(path: &Path) -> Result<(), RunnerError> {
    #[cfg(test)]
    {
        let call_count = SYNC_PARENT_DIR_CALL_COUNT_FOR_TESTS.with(|call_count| {
            let next_call = call_count.get() + 1;
            call_count.set(next_call);
            next_call
        });
        let failure_call = FAIL_SYNC_PARENT_DIR_CALL_FOR_TESTS.with(Cell::get);
        if failure_call != 0 && call_count == failure_call {
            return Err(RunnerError::Io(std::io::Error::other(
                "injected parent directory sync failure",
            )));
        }
    }

    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn with_sync_parent_dir_failure_for_tests<T>(run: impl FnOnce() -> T) -> T {
    with_sync_parent_dir_failure_on_call_for_tests(1, run)
}

#[cfg(test)]
pub(crate) fn with_sync_parent_dir_failure_on_call_for_tests<T>(
    call_index: usize,
    run: impl FnOnce() -> T,
) -> T {
    FAIL_SYNC_PARENT_DIR_CALL_FOR_TESTS.with(|failure_call| {
        assert!(
            failure_call.get() == 0,
            "sync_parent_dir failure injection should not be nested"
        );
        failure_call.set(call_index);
    });
    SYNC_PARENT_DIR_CALL_COUNT_FOR_TESTS.with(|call_count| call_count.set(0));

    struct ResetGuard;

    impl Drop for ResetGuard {
        fn drop(&mut self) {
            FAIL_SYNC_PARENT_DIR_CALL_FOR_TESTS.with(|failure_call| failure_call.set(0));
            SYNC_PARENT_DIR_CALL_COUNT_FOR_TESTS.with(|call_count| call_count.set(0));
        }
    }

    let _guard = ResetGuard;
    run()
}

#[cfg(test)]
mod tests {
    use super::{
        SessionAuditCompletion, current_timestamp, prepare_session_audit_record_at,
        rollback_record_dir_on_error,
    };
    use crate::test_support::{capture_tracing_events, test_session_spec};
    use crate::{RunnerError, SessionInvocation, SessionOutcome};
    use serde_json::Value;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

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

    #[test]
    fn prepare_session_audit_record_writes_initial_metadata_without_end_or_outcome() {
        let root = unique_test_dir("agentd-audit-initial");
        let record = prepare_session_audit_record_at(
            &root,
            "0123456789abcdef",
            &test_session_spec(),
            &SessionInvocation {
                repo_url: "https://example.com/agentd.git".to_string(),
                repo_token: None,
                work_unit: Some("issue-76".to_string()),
                input: None,
                timeout: None,
            },
        )
        .expect("audit record should be created");

        let payload = fs::read_to_string(record.metadata_path)
            .expect("initial session metadata should be readable");
        let json: Value = serde_json::from_str(&payload).expect("metadata should be valid json");

        assert_eq!(json["schema_version"], 2);
        assert_eq!(json["session_id"], "0123456789abcdef");
        assert_eq!(json["agent"], "site-builder");
        assert_eq!(json["repo_url"], "https://example.com/agentd.git");
        assert_eq!(json["work_unit"], "issue-76");
        assert!(json.get("end_timestamp").is_none());
        assert!(json.get("outcome").is_none());
        assert!(json.get("exit_code").is_none());

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn rollback_record_dir_on_error_returns_value_without_removing_record_dir_when_init_succeeds() {
        let root = unique_test_dir("agentd-audit-rollback-ok");
        let record_dir = root.join("record");
        fs::create_dir_all(&record_dir).expect("record dir should be created");

        let value = rollback_record_dir_on_error(&record_dir, || Ok::<_, RunnerError>(42))
            .expect("rollback wrapper should return success values");

        assert_eq!(value, 42);
        assert!(
            record_dir.exists(),
            "successful initialization should keep the record dir"
        );

        fs::remove_dir_all(&root).expect("temporary audit root should be removed");
    }

    #[test]
    fn rollback_record_dir_on_error_removes_record_dir_and_returns_original_error() {
        let root = unique_test_dir("agentd-audit-rollback-error");
        let record_dir = root.join("record");
        fs::create_dir_all(&record_dir).expect("record dir should be created");
        fs::write(record_dir.join("partial"), "stale\n").expect("partial state should be created");

        let error = rollback_record_dir_on_error(&record_dir, || {
            Err::<(), _>(RunnerError::Io(std::io::Error::other(
                "initial metadata write failed",
            )))
        })
        .expect_err("rollback wrapper should return the original initialization error");

        match error {
            RunnerError::Io(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::Other);
                assert_eq!(error.to_string(), "initial metadata write failed");
            }
            other => panic!("expected original io error, got {other:?}"),
        }
        assert!(
            !record_dir.exists(),
            "failed initialization should remove the record dir"
        );

        fs::remove_dir_all(&root).expect("temporary audit root should be removed");
    }

    #[test]
    fn rollback_record_dir_on_error_ignores_cleanup_failure_and_returns_original_error() {
        let root = unique_test_dir("agentd-audit-rollback-best-effort");
        let record_dir = root.join("record");
        fs::create_dir_all(&record_dir).expect("record dir should be created");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555))
            .expect("record dir parent should become read-only");

        let error = rollback_record_dir_on_error(&record_dir, || {
            Err::<(), _>(RunnerError::Io(std::io::Error::other(
                "initial metadata write failed",
            )))
        })
        .expect_err("rollback wrapper should return the original initialization error");

        match error {
            RunnerError::Io(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::Other);
                assert_eq!(error.to_string(), "initial metadata write failed");
            }
            other => panic!("expected original io error, got {other:?}"),
        }
        assert!(
            record_dir.exists(),
            "best-effort rollback should not replace the original error when cleanup fails"
        );

        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))
            .expect("record dir parent should become writable for cleanup");
        fs::remove_dir_all(&root).expect("temporary audit root should be removed");
    }

    #[test]
    fn prepare_session_audit_record_removes_record_dir_when_initial_metadata_write_fails() {
        let root = unique_test_dir("agentd-audit-initial-write-failure");
        let record_dir = root.join("site-builder").join("write-failure");
        fs::create_dir_all(record_dir.join("agentd/session.json"))
            .expect("conflicting metadata path directory should be created");

        let error = prepare_session_audit_record_at(
            &root,
            "write-failure",
            &test_session_spec(),
            &SessionInvocation {
                repo_url: "https://example.com/agentd.git".to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            },
        )
        .expect_err("initial metadata write should fail when session.json is a directory");

        assert!(
            matches!(error, RunnerError::Io(_)),
            "expected metadata write failure, got {error:?}"
        );
        assert!(
            !record_dir.exists(),
            "metadata write failure should remove the partially-created record dir"
        );

        if root.exists() {
            fs::remove_dir_all(&root).expect("temporary audit root should be removed");
        }
    }

    #[test]
    fn prepare_session_audit_record_normalizes_runner_managed_audit_dirs_to_host_traversable_modes()
    {
        let root = unique_test_dir("agentd-audit-dir-normalization");
        let agent_root = root.join("site-builder");
        let record_dir = agent_root.join("dir-normalization");
        let agentd_dir = record_dir.join("agentd");

        fs::create_dir_all(&agentd_dir).expect("audit dir tree should be created");
        fs::set_permissions(&agent_root, fs::Permissions::from_mode(0o700))
            .expect("agent dir should become private");
        fs::set_permissions(&record_dir, fs::Permissions::from_mode(0o700))
            .expect("record dir should become private");
        fs::set_permissions(&agentd_dir, fs::Permissions::from_mode(0o700))
            .expect("metadata dir should become private");

        let record = prepare_session_audit_record_at(
            &root,
            "dir-normalization",
            &test_session_spec(),
            &SessionInvocation {
                repo_url: "https://example.com/agentd.git".to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            },
        )
        .expect("audit record should be created");

        let agent_mode = fs::metadata(&agent_root)
            .expect("agent dir metadata should exist")
            .permissions()
            .mode();
        let record_mode = fs::metadata(&record.record_dir)
            .expect("record dir metadata should exist")
            .permissions()
            .mode();
        let agentd_mode = fs::metadata(record.metadata_path.parent().expect("metadata dir"))
            .expect("metadata dir metadata should exist")
            .permissions()
            .mode();
        let runa_mode = fs::metadata(&record.runa_dir)
            .expect("runa dir metadata should exist")
            .permissions()
            .mode();

        assert_eq!(agent_mode & 0o777, 0o755);
        assert_eq!(record_mode & 0o777, 0o755);
        assert_eq!(agentd_mode & 0o777, 0o755);
        assert_eq!(runa_mode & 0o777, 0o777);

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn write_atomic_treats_post_rename_parent_sync_failure_as_a_warning() {
        let root = unique_test_dir("agentd-audit-post-rename-sync");
        let metadata_path = root.join("agentd").join("session.json");
        let payload = br#"{"outcome":"success"}"#;

        fs::create_dir_all(metadata_path.parent().expect("metadata parent"))
            .expect("metadata parent dir should be created");

        let result = std::cell::RefCell::new(None);
        let events = capture_tracing_events(|| {
            let write_result = super::with_sync_parent_dir_failure_for_tests(|| {
                super::write_atomic(&metadata_path, payload, Some(super::SEALED_FILE_MODE))
            });
            result.replace(Some(write_result));
        });

        result
            .into_inner()
            .expect("write result should be captured")
            .expect("post-rename parent sync failure should not propagate");

        assert_eq!(
            fs::read(&metadata_path).expect("metadata path should contain finalized payload"),
            payload
        );
        assert!(
            !metadata_path.with_extension("json.tmp").exists(),
            "temporary metadata file should not remain after atomic replace"
        );
        assert!(
            events.iter().any(|event| {
                event["fields"]["event"] == "runner.audit_warning"
                    && event["fields"]["warning_kind"] == "post_rename_parent_sync"
            }),
            "post-rename parent sync failure should emit a durability warning"
        );

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_audit_record_writes_outcome_and_seals_record() {
        let root = unique_test_dir("agentd-audit-final");
        let record = prepare_session_audit_record_at(
            &root,
            "fedcba9876543210",
            &test_session_spec(),
            &SessionInvocation {
                repo_url: "https://example.com/agentd.git".to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            },
        )
        .expect("audit record should be created");
        fs::write(record.runa_dir.join("artifact.txt"), "persisted\n")
            .expect("runa artifact should be writable before sealing");
        let nested_dir = record.runa_dir.join("nested");
        fs::create_dir_all(&nested_dir).expect("nested runa dir should be created");
        fs::write(nested_dir.join("nested-artifact.txt"), "nested\n")
            .expect("nested runa artifact should be writable before sealing");

        super::finalize_session_audit_record(
            &record,
            SessionAuditCompletion::Outcome(&SessionOutcome::WorkFailed { exit_code: 5 }),
        )
        .expect("audit record should finalize");

        let payload = fs::read_to_string(&record.metadata_path)
            .expect("final session metadata should be readable");
        let json: Value = serde_json::from_str(&payload).expect("metadata should be valid json");

        assert_eq!(json["outcome"], "work_failed");
        assert_eq!(json["exit_code"], 5);
        assert!(json["end_timestamp"].is_string());

        let runa_mode = fs::metadata(&record.runa_dir)
            .expect("runa dir metadata should exist")
            .permissions()
            .mode();
        let metadata_mode = fs::metadata(&record.metadata_path)
            .expect("metadata file should exist")
            .permissions()
            .mode();
        let artifact_mode = fs::metadata(record.runa_dir.join("artifact.txt"))
            .expect("runa artifact metadata should exist")
            .permissions()
            .mode();
        let nested_dir_mode = fs::metadata(&nested_dir)
            .expect("nested runa dir metadata should exist")
            .permissions()
            .mode();
        let nested_artifact_mode = fs::metadata(nested_dir.join("nested-artifact.txt"))
            .expect("nested runa artifact metadata should exist")
            .permissions()
            .mode();
        let metadata_dir_mode = fs::metadata(
            record
                .metadata_path
                .parent()
                .expect("metadata path should have parent"),
        )
        .expect("metadata dir metadata should exist")
        .permissions()
        .mode();
        let record_dir_mode = fs::metadata(&record.record_dir)
            .expect("record dir metadata should exist")
            .permissions()
            .mode();
        assert_eq!(runa_mode & 0o777, 0o555);
        assert_eq!(artifact_mode & 0o777, 0o444);
        assert_eq!(nested_dir_mode & 0o777, 0o555);
        assert_eq!(nested_artifact_mode & 0o777, 0o444);
        assert_eq!(metadata_mode & 0o777, 0o444);
        assert_eq!(metadata_dir_mode & 0o777, 0o755);
        assert_eq!(record_dir_mode & 0o777, 0o755);

        make_tree_writable(&root);

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_audit_record_prepares_restrictive_directories_for_traversal() {
        for initial_mode in [0o000, 0o111, 0o444, 0o700] {
            let root = unique_test_dir(&format!("agentd-audit-restrictive-dir-{initial_mode:o}"));
            let record = prepare_session_audit_record_at(
                &root,
                "1234567890abcdef",
                &test_session_spec(),
                &SessionInvocation {
                    repo_url: "https://example.com/agentd.git".to_string(),
                    repo_token: None,
                    work_unit: None,
                    input: None,
                    timeout: None,
                },
            )
            .expect("audit record should be created");
            let restricted_dir = record.runa_dir.join(format!("mode-{initial_mode:o}"));
            let artifact_path = restricted_dir.join("artifact.txt");
            fs::create_dir_all(&restricted_dir).expect("restricted audit dir should be created");
            fs::write(&artifact_path, "persisted\n").expect("audit artifact should be writable");
            fs::set_permissions(&restricted_dir, fs::Permissions::from_mode(initial_mode))
                .expect("restricted dir should enter session-written mode");

            let finalize_result = super::finalize_session_audit_record(
                &record,
                SessionAuditCompletion::Outcome(&SessionOutcome::Success { exit_code: 0 }),
            );
            if finalize_result.is_err() {
                fs::set_permissions(&restricted_dir, fs::Permissions::from_mode(0o755))
                    .expect("restricted dir should become traversable for cleanup");
                make_tree_writable(&root);
            }
            finalize_result.expect("audit record should finalize from restrictive dir mode");

            let payload = fs::read_to_string(&record.metadata_path)
                .expect("final session metadata should be readable");
            let json: Value =
                serde_json::from_str(&payload).expect("metadata should be valid json");
            assert_eq!(json["outcome"], "success");
            assert!(json["end_timestamp"].is_string());

            let metadata_dir = record
                .metadata_path
                .parent()
                .expect("metadata path should have a parent");
            assert_eq!(
                fs::metadata(&restricted_dir)
                    .expect("restricted dir metadata should exist")
                    .permissions()
                    .mode()
                    & 0o777,
                0o555
            );
            assert_eq!(
                fs::metadata(&artifact_path)
                    .expect("audit artifact metadata should exist")
                    .permissions()
                    .mode()
                    & 0o777,
                0o444
            );
            assert_eq!(
                fs::metadata(metadata_dir)
                    .expect("metadata dir metadata should exist")
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
            assert_eq!(
                fs::metadata(&record.record_dir)
                    .expect("record dir metadata should exist")
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );

            make_tree_writable(&root);
            fs::remove_dir_all(root).expect("temporary audit root should be removed");
        }
    }

    #[test]
    fn finalize_session_audit_record_skips_symlinks_when_sealing() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = unique_test_dir("agentd-audit-symlink");
        let outside_target = root.join("outside-target.txt");
        let record = prepare_session_audit_record_at(
            &root,
            "1111222233334444",
            &test_session_spec(),
            &SessionInvocation {
                repo_url: "https://example.com/agentd.git".to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            },
        )
        .expect("audit record should be created");

        fs::write(&outside_target, "outside\n").expect("outside target should be writable");
        fs::set_permissions(&outside_target, fs::Permissions::from_mode(0o666))
            .expect("outside target mode should be writable");
        symlink(&outside_target, record.runa_dir.join("escaped-link"))
            .expect("symlink should be created");

        super::finalize_session_audit_record(
            &record,
            SessionAuditCompletion::Outcome(&SessionOutcome::Success { exit_code: 0 }),
        )
        .expect("audit record should finalize");

        let outside_mode = fs::metadata(&outside_target)
            .expect("outside target metadata should exist")
            .permissions()
            .mode();
        assert_eq!(outside_mode & 0o777, 0o666);

        make_tree_writable(&root);
        fs::remove_file(&outside_target).expect("outside target should be removed");
        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_audit_record_refuses_hard_linked_entries_before_metadata_rewrite() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_test_dir("agentd-audit-hard-link");
        let outside_target = root.join("outside-target.txt");
        let record = prepare_session_audit_record_at(
            &root,
            "9999000011112222",
            &test_session_spec(),
            &SessionInvocation {
                repo_url: "https://example.com/agentd.git".to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            },
        )
        .expect("audit record should be created");

        fs::write(&outside_target, "outside\n").expect("outside target should be writable");
        fs::set_permissions(&outside_target, fs::Permissions::from_mode(0o666))
            .expect("outside target mode should be writable");
        fs::hard_link(&outside_target, record.runa_dir.join("escaped-hard-link"))
            .expect("hard link should be created");

        let error = super::finalize_session_audit_record(
            &record,
            SessionAuditCompletion::Outcome(&SessionOutcome::Success { exit_code: 0 }),
        )
        .expect_err("hard-linked audit entries should be rejected before sealing");
        assert!(
            matches!(error, crate::RunnerError::Io(_)),
            "expected io error for unsafe hard-linked entry, got {error:?}"
        );

        let payload = fs::read_to_string(&record.metadata_path)
            .expect("initial session metadata should remain readable");
        let json: Value = serde_json::from_str(&payload).expect("metadata should be valid json");
        assert!(
            json.get("end_timestamp").is_none(),
            "hard-link refusal must leave end_timestamp incomplete"
        );
        assert!(
            json.get("outcome").is_none(),
            "hard-link refusal must leave outcome incomplete"
        );

        let outside_mode = fs::metadata(&outside_target)
            .expect("outside target metadata should exist")
            .permissions()
            .mode();
        assert_eq!(outside_mode & 0o777, 0o666);

        make_tree_writable(&root);
        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn write_session_audit_metadata_replaces_file_without_leaving_temp_file() {
        let root = unique_test_dir("agentd-audit-atomic-write");
        let record = prepare_session_audit_record_at(
            &root,
            "abcdabcdabcdabcd",
            &test_session_spec(),
            &SessionInvocation {
                repo_url: "https://example.com/agentd.git".to_string(),
                repo_token: None,
                work_unit: None,
                input: None,
                timeout: None,
            },
        )
        .expect("audit record should be created");

        super::finalize_session_audit_record(
            &record,
            SessionAuditCompletion::Outcome(&SessionOutcome::Success { exit_code: 0 }),
        )
        .expect("audit record should finalize");

        let payload = fs::read_to_string(&record.metadata_path)
            .expect("final session metadata should be readable");
        let json: Value = serde_json::from_str(&payload).expect("metadata should be valid json");
        assert_eq!(json["outcome"], "success");
        assert!(
            !record.metadata_path.with_extension("json.tmp").exists(),
            "temporary metadata file should not remain after atomic replace"
        );

        make_tree_writable(&root);

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn current_timestamp_emits_rfc3339_utc_values() {
        let timestamp = current_timestamp().expect("timestamp should format");
        assert!(timestamp.ends_with('Z'));
        assert!(timestamp.contains('T'));
    }
}
