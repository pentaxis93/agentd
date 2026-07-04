//! Session audit records: preparation, transcript finalization, and sealing.
//!
//! Every session leaves a persistent record under
//! `<audit_root>/<agent>/<session_id>/` (layout and format:
//! `docs/audit-record.md`; security tradeoffs: `ARCHITECTURE.md`
//! § "Host audit records"). The record moves through exactly one lifecycle:
//!
//! 1. [`prepare_session_audit_record`] — create the record tree with
//!    *active* permissions and publish initial `session.json` metadata.
//!    Any preparation failure rolls the whole record directory back so a
//!    half-built record is never observable.
//! 2. The session runs; runa writes `runa/` state and transcript events
//!    arrive in `agentd/transcript/` through the session-user identity.
//! 3. [`finalize_session_transcript`] — render runa's nested event files into
//!    `transcript.md` and record a coverage verdict in `manifest.json`.
//!    A rendering failure is itself recorded (`finalization_failed`) so
//!    the manifest never silently overstates coverage.
//! 4. [`finalize_session_audit_record`] — seal the tree read-only and only
//!    then publish the finalized metadata. A record with a populated
//!    `outcome` is therefore always a sealed record.
//!
//! Sealing is the security boundary between "session may still write" and
//! "record is evidence". The invariants the sealing path enforces:
//!
//! - **Symlink entries are skipped, never sealed.** Every walk inspects
//!   with `symlink_metadata` and skips symlinks; the sealing chmods are
//!   plain `fs::set_permissions` calls made only on paths that inspection
//!   found non-symlink. This check-then-chmod sequence is safe because
//!   finalization runs after the session container has exited — no live
//!   writer can swap a path between the check and the chmod. (Transcript
//!   permission repair additionally uses `fchmodat(AT_SYMLINK_NOFOLLOW)`;
//!   see `set_owner_permissions_no_follow`.)
//! - **Multi-linked files are refused, not sealed.** A regular file with
//!   `nlink > 1` aborts finalization (`preflight_validate_sealable_tree`):
//!   chmod acts on the inode, so sealing a hardlink would mutate a file
//!   that also lives outside the record.
//! - **Metadata is published by atomic replace.** `session.json` is written
//!   to a temp file, fsynced, then renamed into place (`write_atomic`);
//!   readers see the old or the new metadata, never a partial write.
//! - **The daemon seals with its own filesystem authority.** No Podman or
//!   user-namespace re-entry happens during finalization; the startup
//!   audit-root probe verified this authority before any dispatch.

use crate::transcript::{TranscriptEventFile, TranscriptEventSource, TranscriptIdentity};
use crate::{RunnerError, SessionInvocation, SessionOutcome, SessionSpec};
use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use std::cell::Cell;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// `session.json` schema version; bump on any breaking metadata change and
/// update `docs/audit-record.md` in the same change.
const METADATA_SCHEMA_VERSION: u32 = 2;
/// Directories of an *active* (unsealed) record. World-traversable so the
/// session-user identity can reach the `runa/` and transcript subtrees.
const ACTIVE_AUDIT_DIRECTORY_MODE: u32 = 0o755;
/// Sealed regular files: world-readable, writable by no one. Deliberate
/// single-tenant tradeoff — see ARCHITECTURE.md § "Host audit records".
const SEALED_FILE_MODE: u32 = 0o444;
/// Sealed directories: traversal without write, same tradeoff as files.
const SEALED_DIRECTORY_MODE: u32 = 0o555;
/// `manifest.json` schema version for the transcript coverage verdict.
const TRANSCRIPT_MANIFEST_SCHEMA_VERSION: u32 = 1;
const EVENTS_ARTIFACT: &str = "events.jsonl";
const MANIFEST_ARTIFACT: &str = "manifest.json";
const MARKDOWN_ARTIFACT: &str = "transcript.md";

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
    pub(crate) transcript_identity: TranscriptIdentity,
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

#[derive(Debug)]
struct TranscriptFinalizationFailure {
    artifact: &'static str,
    error: RunnerError,
}

impl fmt::Display for TranscriptFinalizationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

/// Create the session's audit record tree with active permissions and
/// publish the initial `session.json` (no `end_timestamp`/`outcome` yet).
///
/// On any failure the record directory is removed (`rollback_record_dir_on_error`)
/// so a half-prepared record is never left for an operator to mistake for a
/// real session.
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
            transcript_identity: TranscriptIdentity::new(
                &spec.forge_type,
                &invocation.repo_url,
                session_id,
            ),
            start_timestamp,
        };

        write_session_audit_metadata(&record, None, None, None)?;
        Ok(record)
    })
}

/// Seal the record read-only, then publish finalized metadata — in that
/// order, so a `session.json` carrying an `outcome` is proof the tree was
/// already sealed.
///
/// Ordering within: restore daemon traversal over session-written
/// directories, refuse the tree if any regular file is multi-linked, seal
/// everything except the metadata path (which must stay replaceable for the
/// final atomic write), then atomically publish `session.json` already in
/// sealed mode.
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

/// Render the transcript artifacts and record a coverage verdict.
///
/// Coverage is honest by construction: a failure while producing
/// `events.jsonl` or `transcript.md` is itself written into
/// `manifest.json` as `finalization_failed` (with the error), so the
/// manifest never claims more transcript than the record holds.
pub(crate) fn finalize_session_transcript(record: &SessionAuditRecord) -> Result<(), RunnerError> {
    prepare_transcript_tree_for_finalization(&record.transcript_dir)?;
    match finalize_session_transcript_artifacts(record) {
        Ok(()) => Ok(()),
        Err(failure) => {
            let failure_message = failure.to_string();
            if failure.artifact != MANIFEST_ARTIFACT {
                write_transcript_manifest(
                    record,
                    "finalization_failed",
                    Vec::new(),
                    Some(&failure_message),
                )
                .map_err(|manifest_failure| manifest_failure.error)?;
            }
            Err(failure.error)
        }
    }
}

fn finalize_session_transcript_artifacts(
    record: &SessionAuditRecord,
) -> Result<(), TranscriptFinalizationFailure> {
    let source = TranscriptEventSource::from_record(record);
    let event_paths = source
        .discover_event_files()
        .map_err(|error| runner_artifact_failure(EVENTS_ARTIFACT, error))?;
    let summary = transcript_summary(&source, &event_paths)?;
    write_transcript_markdown(record, &source, &event_paths)?;
    write_transcript_manifest(
        record,
        summary.coverage(),
        summary.event_schema_versions(),
        None,
    )?;
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
    event_schema_versions: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finalization_error: Option<&'a str>,
}

fn write_transcript_manifest_payload(
    record: &SessionAuditRecord,
    manifest: TranscriptManifest<'_>,
) -> Result<(), TranscriptFinalizationFailure> {
    let mut payload = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| artifact_failure(MANIFEST_ARTIFACT, std::io::Error::other(error)))?;
    payload.push(b'\n');
    write_new_transcript_artifact(
        &record.transcript_dir.join(MANIFEST_ARTIFACT),
        MANIFEST_ARTIFACT,
        &payload,
    )?;
    Ok(())
}

fn write_transcript_manifest(
    record: &SessionAuditRecord,
    coverage: &str,
    event_schema_versions: Vec<u64>,
    finalization_error: Option<&str>,
) -> Result<(), TranscriptFinalizationFailure> {
    let manifest = TranscriptManifest {
        schema_version: TRANSCRIPT_MANIFEST_SCHEMA_VERSION,
        coverage,
        event_schema_versions,
        finalization_error,
    };
    write_transcript_manifest_payload(record, manifest)
}

fn write_transcript_markdown(
    record: &SessionAuditRecord,
    source: &TranscriptEventSource,
    event_paths: &[TranscriptEventFile],
) -> Result<(), TranscriptFinalizationFailure> {
    let mut markdown = create_new_transcript_artifact(
        &record.transcript_dir.join(MARKDOWN_ARTIFACT),
        MARKDOWN_ARTIFACT,
    )?;
    markdown
        .write_all(b"# Session Transcript\n\n")
        .map_err(|error| artifact_failure(MARKDOWN_ARTIFACT, error))?;

    let mut line = String::new();
    let mut wrote_event = false;
    for event_file in event_paths {
        let events = open_transcript_events(source, event_file)?;
        let mut reader = BufReader::new(events);
        loop {
            line.clear();
            let bytes_read = reader
                .read_line(&mut line)
                .map_err(|error| artifact_failure(EVENTS_ARTIFACT, error))?;
            if bytes_read == 0 {
                break;
            }

            let line = line.trim_end_matches(['\r', '\n']);
            if line.trim().is_empty() {
                continue;
            }

            wrote_event = true;
            match serde_json::from_str::<Value>(line) {
                Ok(event) => {
                    let kind = event.get("kind").and_then(Value::as_str).unwrap_or("event");
                    writeln!(markdown, "## {kind}\n")
                        .map_err(|error| artifact_failure(MARKDOWN_ARTIFACT, error))?;
                    if let Some(content) = event.get("content").and_then(Value::as_str) {
                        write_fenced_code_block(&mut markdown, "text", content)?;
                    } else {
                        write_fenced_code_block(&mut markdown, "json", line)?;
                    }
                }
                Err(_) => {
                    markdown
                        .write_all(b"## unparsed_event\n\n")
                        .map_err(|error| artifact_failure(MARKDOWN_ARTIFACT, error))?;
                    write_fenced_code_block(&mut markdown, "text", line)?;
                }
            }
        }
    }

    if !wrote_event {
        markdown
            .write_all(b"_No structured transcript events were emitted._\n")
            .map_err(|error| artifact_failure(MARKDOWN_ARTIFACT, error))?;
    }

    Ok(())
}

fn open_transcript_events(
    source: &TranscriptEventSource,
    event_file: &TranscriptEventFile,
) -> Result<File, TranscriptFinalizationFailure> {
    source
        .open_event_file(event_file)
        .map_err(|error| runner_artifact_failure(EVENTS_ARTIFACT, error))
}

fn write_new_transcript_artifact(
    path: &Path,
    artifact: &'static str,
    payload: &[u8],
) -> Result<(), TranscriptFinalizationFailure> {
    let mut file = create_new_transcript_artifact(path, artifact)?;
    file.write_all(payload)
        .map_err(|error| artifact_failure(artifact, error))?;
    Ok(())
}

fn create_new_transcript_artifact(
    path: &Path,
    artifact: &'static str,
) -> Result<File, TranscriptFinalizationFailure> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                unsafe_artifact_failure(artifact, "already exists")
            } else {
                artifact_failure(artifact, error)
            }
        })
}

fn artifact_failure(
    artifact: &'static str,
    error: std::io::Error,
) -> TranscriptFinalizationFailure {
    TranscriptFinalizationFailure {
        artifact,
        error: RunnerError::Io(error),
    }
}

fn runner_artifact_failure(
    artifact: &'static str,
    error: RunnerError,
) -> TranscriptFinalizationFailure {
    TranscriptFinalizationFailure { artifact, error }
}

fn unsafe_artifact_failure(artifact: &'static str, reason: &str) -> TranscriptFinalizationFailure {
    artifact_failure(
        artifact,
        std::io::Error::other(format!("unsafe transcript artifact: {artifact} {reason}")),
    )
}

fn write_fenced_code_block(
    markdown: &mut File,
    language: &str,
    content: &str,
) -> Result<(), TranscriptFinalizationFailure> {
    let fence = "`".repeat(max_consecutive_backticks(content).saturating_add(1).max(3));
    writeln!(markdown, "{fence}{language}")
        .map_err(|error| artifact_failure(MARKDOWN_ARTIFACT, error))?;
    markdown
        .write_all(content.as_bytes())
        .map_err(|error| artifact_failure(MARKDOWN_ARTIFACT, error))?;
    if !content.ends_with('\n') {
        markdown
            .write_all(b"\n")
            .map_err(|error| artifact_failure(MARKDOWN_ARTIFACT, error))?;
    }
    writeln!(markdown, "{fence}\n").map_err(|error| artifact_failure(MARKDOWN_ARTIFACT, error))?;
    Ok(())
}

/// Longest backtick run in untrusted transcript content — the rendered
/// markdown fence must be strictly longer so session output cannot break
/// out of its code block and forge transcript structure.
fn max_consecutive_backticks(content: &str) -> usize {
    let mut current = 0;
    let mut max = 0;
    for character in content.chars() {
        if character == '`' {
            current += 1;
            max = max.max(current);
        } else {
            current = 0;
        }
    }
    max
}

#[derive(Debug, Default)]
struct TranscriptSummary {
    saw_event: bool,
    saw_mcp_event: bool,
    event_schema_versions: BTreeSet<u64>,
}

impl TranscriptSummary {
    fn coverage(&self) -> &'static str {
        if self.saw_mcp_event {
            "full"
        } else if self.saw_event {
            "missing_mcp_events"
        } else {
            "no_events"
        }
    }

    fn event_schema_versions(&self) -> Vec<u64> {
        self.event_schema_versions.iter().copied().collect()
    }
}

fn transcript_summary(
    source: &TranscriptEventSource,
    event_paths: &[TranscriptEventFile],
) -> Result<TranscriptSummary, TranscriptFinalizationFailure> {
    let mut summary = TranscriptSummary::default();
    for event_file in event_paths {
        let events = open_transcript_events(source, event_file)?;
        summarize_transcript_events(events, &mut summary)?;
    }
    Ok(summary)
}

fn summarize_transcript_events(
    events: File,
    summary: &mut TranscriptSummary,
) -> Result<(), TranscriptFinalizationFailure> {
    let mut reader = BufReader::new(events);
    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|error| artifact_failure(EVENTS_ARTIFACT, error))?;
        if bytes_read == 0 {
            break;
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        summary.saw_event = true;
        if let Ok(event) = serde_json::from_str::<Value>(line) {
            if event.get("source").and_then(Value::as_str) == Some("runa-mcp") {
                summary.saw_mcp_event = true;
            }
            if let Some(schema_version) = event.get("schema_version").and_then(Value::as_u64) {
                summary.event_schema_versions.insert(schema_version);
            }
        }
    }

    Ok(())
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

fn prepare_transcript_tree_for_finalization(path: &Path) -> Result<(), RunnerError> {
    ensure_transcript_directory(path)?;
    prepare_audit_tree_for_traversal(path)?;
    preflight_validate_sealable_tree(path)?;
    repair_transcript_path_permissions(path)
}

fn ensure_transcript_directory(path: &Path) -> Result<(), RunnerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(RunnerError::Io(std::io::Error::other(format!(
                    "unsafe transcript directory: {} is not a directory",
                    path.display()
                ))));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
        }
        Err(error) => return Err(RunnerError::Io(error)),
    }

    Ok(())
}

/// Re-grant owner access (`u+rwx` dirs, `u+rw` files) over the transcript
/// tree without following symlinks, so finalization can read
/// session-written events whatever modes the session left behind.
fn repair_transcript_path_permissions(path: &Path) -> Result<(), RunnerError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    if metadata.is_dir() {
        set_owner_permissions_no_follow(path, metadata.permissions().mode() | 0o700)?;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            repair_transcript_path_permissions(&entry.path())?;
        }
    } else if metadata.is_file() {
        set_owner_permissions_no_follow(path, metadata.permissions().mode() | 0o600)?;
    }

    Ok(())
}

/// chmod that never dereferences a final-component symlink
/// (`fchmodat(AT_SYMLINK_NOFOLLOW)`): the audit tree contains
/// session-written content, and following a planted link would let a
/// session aim the daemon's chmod authority outside the record.
fn set_owner_permissions_no_follow(path: &Path, mode: u32) -> Result<(), RunnerError> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|error| {
        RunnerError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path contains interior nul byte: {error}"),
        ))
    })?;
    let result = unsafe {
        libc::fchmodat(
            libc::AT_FDCWD,
            path.as_ptr(),
            mode as libc::mode_t,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(RunnerError::Io(std::io::Error::last_os_error()))
    }
}

fn seal_session_audit_record(record: &SessionAuditRecord) -> Result<(), RunnerError> {
    seal_path_recursive(record, &record.record_dir)
}

/// Restore daemon read+traverse permission over session-written
/// directories before validation and sealing walk them. The session user
/// may have dropped directory modes; without this the walk could not even
/// enumerate the tree it must seal.
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

/// Refuse to seal a tree containing any multi-linked regular file.
///
/// `chmod` acts on the inode: sealing a hardlink planted by the session
/// would flip permissions on a file that also exists outside the record.
/// Refusing (rather than skipping) makes the tampering attempt loud — the
/// session finalizes as an error instead of producing a quietly partial
/// seal.
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

/// Paths excluded from sealing: the record root, `session.json`, and its
/// parent directory. They must remain daemon-writable until the final
/// atomic metadata publish; the metadata file itself is written directly
/// in sealed mode.
fn should_skip_sealing_path(record: &SessionAuditRecord, path: &Path) -> bool {
    path == record.record_dir
        || path == record.metadata_path
        || record
            .metadata_path
            .parent()
            .is_some_and(|metadata_dir| path == metadata_dir)
}

/// Publish a payload by temp-file write + fsync + rename within the same
/// directory. Readers observe the previous content or the complete new
/// content, never a torn write; the temp file is removed on failure.
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
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

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
                work_unit: Some("76".to_string()),
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
        assert_eq!(json["work_unit"], "76");
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
    fn finalize_session_transcript_reports_no_events_without_creating_flat_events_jsonl() {
        let root = unique_test_dir("agentd-audit-empty-transcript");
        let record = prepare_session_audit_record_at(
            &root,
            "empty-transcript",
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

        super::finalize_session_transcript(&record).expect("transcript should finalize");

        assert!(
            !record.transcript_dir.join("events.jsonl").exists(),
            "agentd should not create a flat transcript event stream"
        );

        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(record.transcript_dir.join("manifest.json"))
                .expect("transcript manifest should exist"),
        )
        .expect("manifest should be json");
        assert_eq!(manifest["coverage"], "no_events");
        assert_eq!(manifest["event_schema_versions"], serde_json::json!([]));

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn transcript_event_source_discovers_stage_file_created_after_first_scan() {
        let root = unique_test_dir("agentd-audit-nested-growth");
        let record = prepare_session_audit_record_at(
            &root,
            "nested-growth",
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
        let source = crate::transcript::TranscriptEventSource::from_record(&record);
        let first_path = source.event_file_path_for_work_unit("survey");
        fs::create_dir_all(first_path.parent().expect("event path parent"))
            .expect("first nested event dir should be created");
        fs::write(
            &first_path,
            "{\"schema_version\":2,\"source\":\"runa\",\"kind\":\"agent_input\",\"content\":\"survey\"}\n",
        )
        .expect("first nested event file should be written");

        let discovered = source
            .discover_event_files()
            .expect("first discovery should succeed");
        let discovered_paths: Vec<_> = discovered.iter().map(|event| event.path()).collect();
        assert_eq!(discovered_paths, vec![first_path.as_path()]);

        let second_path = source.event_file_path_for_work_unit("take/stage#1");
        fs::create_dir_all(second_path.parent().expect("event path parent"))
            .expect("second nested event dir should be created");
        fs::write(
            &second_path,
            "{\"schema_version\":2,\"source\":\"runa-mcp\",\"kind\":\"tool_call\",\"content\":\"take\"}\n",
        )
        .expect("second nested event file should be written");

        let discovered = source
            .discover_event_files()
            .expect("second discovery should succeed");
        let discovered_paths: Vec<_> = discovered.iter().map(|event| event.path()).collect();
        assert_eq!(
            discovered_paths.len(),
            2,
            "source must re-scan for new stages"
        );
        assert!(
            discovered_paths.contains(&first_path.as_path()),
            "{discovered:?}"
        );
        assert!(
            discovered_paths.contains(&second_path.as_path()),
            "{discovered:?}"
        );

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_transcript_reads_nested_runa_events_not_flat_events_jsonl() {
        let root = unique_test_dir("agentd-audit-nested-transcript");
        let record = prepare_session_audit_record_at(
            &root,
            "nested-transcript",
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
        fs::write(
            record.transcript_dir.join("events.jsonl"),
            "{\"schema_version\":1,\"source\":\"runa\",\"kind\":\"agent_input\",\"content\":\"flat\"}\n",
        )
        .expect("flat event file should be written");
        write_nested_events(
            &record,
            "survey",
            "{\"schema_version\":2,\"source\":\"runa\",\"kind\":\"agent_input\",\"content\":\"nested survey\"}\n",
        );
        write_nested_events(
            &record,
            "take/stage#1",
            "{\"schema_version\":2,\"source\":\"runa-mcp\",\"kind\":\"tool_call\",\"content\":\"nested take\"}\n",
        );

        super::finalize_session_transcript(&record).expect("transcript should finalize");

        let markdown = fs::read_to_string(record.transcript_dir.join("transcript.md"))
            .expect("transcript markdown should exist");
        assert!(markdown.contains("nested survey"), "{markdown}");
        assert!(markdown.contains("nested take"), "{markdown}");
        assert!(!markdown.contains("flat"), "{markdown}");

        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(record.transcript_dir.join("manifest.json"))
                .expect("transcript manifest should exist"),
        )
        .expect("manifest should be json");
        assert_eq!(manifest["coverage"], "full");
        assert_eq!(manifest["event_schema_versions"], serde_json::json!([2]));

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_transcript_refuses_symlinked_nested_transcript_ancestors() {
        use std::os::unix::fs::symlink;

        for ancestor in SymlinkAncestor::refused_cases() {
            let root = unique_test_dir(&format!(
                "agentd-audit-symlinked-transcript-{}",
                ancestor.name()
            ));
            let record = prepare_session_audit_record_at(
                &root,
                &format!("symlinked-{}", ancestor.name()),
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
            let outside_root = root.join("outside-transcript");
            let outside_events = create_outside_nested_events(&record, &outside_root);
            fs::set_permissions(&outside_events, fs::Permissions::from_mode(0o600))
                .expect("outside events should start with controlled mode");
            let before_mode = fs::metadata(&outside_events)
                .expect("outside events metadata should exist")
                .permissions()
                .mode()
                & 0o777;

            let link_path = ancestor.link_path(&record);
            if let Some(parent) = link_path.parent() {
                fs::create_dir_all(parent).expect("symlink parent should be created");
            }
            symlink(ancestor.outside_target(&record, &outside_root), &link_path)
                .expect("symlinked transcript ancestor should be created");

            let result = super::finalize_session_transcript(&record);

            let after_mode = fs::metadata(&outside_events)
                .expect("outside events metadata should exist")
                .permissions()
                .mode()
                & 0o777;
            let markdown =
                fs::read_to_string(record.transcript_dir.join("transcript.md")).unwrap_or_default();
            let manifest =
                fs::read_to_string(record.transcript_dir.join("manifest.json")).unwrap_or_default();

            assert!(
                result.is_err(),
                "known symlinked ancestor {} should be refused",
                ancestor.name()
            );
            assert_eq!(
                before_mode,
                after_mode,
                "outside target must not be chmoded through {}",
                ancestor.name()
            );
            assert!(
                !markdown.contains("outside secret"),
                "outside events must not be rendered through {}: {markdown}",
                ancestor.name()
            );
            assert!(
                !manifest.contains("full"),
                "outside events must not contribute coverage through {}: {manifest}",
                ancestor.name()
            );

            fs::remove_file(link_path).expect("symlink should be removable");
            make_tree_writable(&root);
            fs::remove_dir_all(root).expect("temporary audit root should be removed");
        }
    }

    #[test]
    fn transcript_event_source_skips_symlinked_work_unit_entries() {
        use std::os::unix::fs::symlink;

        let root = unique_test_dir("agentd-audit-symlinked-work-unit-entry");
        let record = prepare_session_audit_record_at(
            &root,
            "symlinked-work-unit-entry",
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
        let outside_root = root.join("outside-transcript");
        let outside_events = create_outside_nested_events(&record, &outside_root);
        let source = crate::transcript::TranscriptEventSource::from_record(&record);
        let work_unit_link = source.event_file_path_for_work_unit("survey");
        let work_unit_link = work_unit_link
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("work-unit path should have an encoded work-unit ancestor")
            .to_path_buf();
        fs::create_dir_all(work_unit_link.parent().expect("work-unit parent"))
            .expect("work-units directory should be created");
        symlink(
            outside_events
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .expect("outside events should have a work-unit ancestor"),
            &work_unit_link,
        )
        .expect("symlinked work-unit entry should be created");

        super::finalize_session_transcript(&record).expect("transcript should finalize safely");

        let markdown = fs::read_to_string(record.transcript_dir.join("transcript.md"))
            .expect("transcript markdown should exist");
        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(record.transcript_dir.join("manifest.json"))
                .expect("manifest should exist"),
        )
        .expect("manifest should be json");
        assert!(!markdown.contains("outside secret"), "{markdown}");
        assert_eq!(manifest["coverage"], "no_events");

        fs::remove_file(work_unit_link).expect("work-unit symlink should be removable");
        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[derive(Debug, Clone, Copy)]
    enum SymlinkAncestor {
        Deployments,
        Deployment,
        WorkUnits,
        Runs,
        RunId,
    }

    impl SymlinkAncestor {
        fn refused_cases() -> [Self; 5] {
            [
                Self::Deployments,
                Self::Deployment,
                Self::WorkUnits,
                Self::Runs,
                Self::RunId,
            ]
        }

        fn name(self) -> &'static str {
            match self {
                Self::Deployments => "deployments",
                Self::Deployment => "deployment",
                Self::WorkUnits => "work-units",
                Self::Runs => "runs",
                Self::RunId => "run-id",
            }
        }

        fn link_path(self, record: &super::SessionAuditRecord) -> PathBuf {
            let source = crate::transcript::TranscriptEventSource::from_record(record);
            let event_path = source.event_file_path_for_work_unit("survey");
            match self {
                Self::Deployments => record.transcript_dir.join("deployments"),
                Self::Deployment => event_path
                    .ancestors()
                    .nth(5)
                    .expect("event path should have deployment ancestor")
                    .to_path_buf(),
                Self::WorkUnits => event_path
                    .ancestors()
                    .nth(4)
                    .expect("event path should have work-units ancestor")
                    .to_path_buf(),
                Self::Runs => event_path
                    .ancestors()
                    .nth(2)
                    .expect("event path should have runs ancestor")
                    .to_path_buf(),
                Self::RunId => event_path
                    .parent()
                    .expect("event path should have run-id parent")
                    .to_path_buf(),
            }
        }

        fn outside_target(
            self,
            record: &super::SessionAuditRecord,
            outside_root: &Path,
        ) -> PathBuf {
            let source = crate::transcript::TranscriptEventSource::from_record(record);
            let outside_events = outside_root.join(
                source
                    .event_file_path_for_work_unit("survey")
                    .strip_prefix(&record.transcript_dir)
                    .expect("event path should live under transcript dir"),
            );
            match self {
                Self::Deployments => outside_root.join("deployments"),
                Self::Deployment => outside_events
                    .ancestors()
                    .nth(5)
                    .expect("outside event path should have deployment ancestor")
                    .to_path_buf(),
                Self::WorkUnits => outside_events
                    .ancestors()
                    .nth(4)
                    .expect("outside event path should have work-units ancestor")
                    .to_path_buf(),
                Self::Runs => outside_events
                    .ancestors()
                    .nth(2)
                    .expect("outside event path should have runs ancestor")
                    .to_path_buf(),
                Self::RunId => outside_events
                    .parent()
                    .expect("outside event path should have run-id parent")
                    .to_path_buf(),
            }
        }
    }

    fn create_outside_nested_events(
        record: &super::SessionAuditRecord,
        outside_root: &Path,
    ) -> PathBuf {
        let source = crate::transcript::TranscriptEventSource::from_record(record);
        let outside_events = outside_root.join(
            source
                .event_file_path_for_work_unit("survey")
                .strip_prefix(&record.transcript_dir)
                .expect("event path should live under transcript dir"),
        );
        fs::create_dir_all(outside_events.parent().expect("outside events parent"))
            .expect("outside nested event directory should be created");
        fs::write(
            &outside_events,
            "{\"schema_version\":2,\"source\":\"runa-mcp\",\"kind\":\"tool_call\",\"content\":\"outside secret\"}\n",
        )
        .expect("outside events should be written");
        outside_events
    }

    fn write_nested_events(record: &super::SessionAuditRecord, work_unit: &str, events: &str) {
        let source = crate::transcript::TranscriptEventSource::from_record(record);
        let path = source.event_file_path_for_work_unit(work_unit);
        fs::create_dir_all(path.parent().expect("event path parent"))
            .expect("nested event directory should be created");
        fs::write(path, events).expect("nested events should be written");
    }

    #[test]
    fn finalize_session_transcript_repairs_restrictive_transcript_directory_before_rendering() {
        let root = unique_test_dir("agentd-audit-restrictive-transcript-dir");
        let record = prepare_session_audit_record_at(
            &root,
            "restrictive-transcript-dir",
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
        write_nested_events(
            &record,
            "survey",
            "{\"schema_version\":1,\"source\":\"runa\",\"kind\":\"agent_input\",\"content\":\"hello\"}\n",
        );
        fs::set_permissions(&record.transcript_dir, fs::Permissions::from_mode(0o000))
            .expect("transcript dir should become restrictive");

        let finalize_result = super::finalize_session_transcript(&record);
        if finalize_result.is_err() {
            let _ = fs::set_permissions(&record.transcript_dir, fs::Permissions::from_mode(0o755));
        }
        finalize_result.expect("transcript should finalize from restrictive directory mode");

        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(record.transcript_dir.join("manifest.json"))
                .expect("transcript manifest should exist"),
        )
        .expect("manifest should be json");
        assert_eq!(manifest["coverage"], "missing_mcp_events");
        let markdown = fs::read_to_string(record.transcript_dir.join("transcript.md"))
            .expect("transcript markdown should exist");
        assert!(markdown.contains("agent_input"), "{markdown}");

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_transcript_repairs_restrictive_events_jsonl_before_reading() {
        let root = unique_test_dir("agentd-audit-restrictive-transcript-events");
        let record = prepare_session_audit_record_at(
            &root,
            "restrictive-transcript-events",
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
        let events_path = crate::transcript::TranscriptEventSource::from_record(&record)
            .event_file_path_for_work_unit("survey");
        fs::create_dir_all(events_path.parent().expect("event path parent"))
            .expect("nested event directory should be created");
        fs::write(
            &events_path,
            "{\"schema_version\":1,\"source\":\"runa-mcp\",\"kind\":\"tool_call\"}\n",
        )
        .expect("events jsonl should be created");
        fs::set_permissions(&events_path, fs::Permissions::from_mode(0o000))
            .expect("events jsonl should become restrictive");

        let finalize_result = super::finalize_session_transcript(&record);
        if finalize_result.is_err() {
            let _ = fs::set_permissions(&events_path, fs::Permissions::from_mode(0o644));
        }
        finalize_result.expect("transcript should finalize from restrictive events mode");

        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(record.transcript_dir.join("manifest.json"))
                .expect("transcript manifest should exist"),
        )
        .expect("manifest should be json");
        assert_eq!(manifest["coverage"], "full");

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_transcript_refuses_hard_linked_events_jsonl_without_chmoding_target() {
        let root = unique_test_dir("agentd-audit-transcript-hard-link");
        let record = prepare_session_audit_record_at(
            &root,
            "transcript-hard-link",
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
        let runa_target = record.runa_dir.join("events-source.jsonl");
        fs::write(
            &runa_target,
            "{\"schema_version\":1,\"source\":\"runa\",\"kind\":\"agent_input\"}\n",
        )
        .expect("runa target should be created");
        fs::set_permissions(&runa_target, fs::Permissions::from_mode(0o000))
            .expect("runa target should start unreadable");
        let events_path = crate::transcript::TranscriptEventSource::from_record(&record)
            .event_file_path_for_work_unit("survey");
        fs::create_dir_all(events_path.parent().expect("event path parent"))
            .expect("nested event directory should be created");
        fs::hard_link(&runa_target, &events_path)
            .expect("hard-linked transcript events should be created");

        let before_mode = fs::metadata(&runa_target)
            .expect("runa target metadata should exist")
            .permissions()
            .mode()
            & 0o777;

        let result = super::finalize_session_transcript(&record);

        let after_mode = fs::metadata(&runa_target)
            .expect("runa target metadata should exist")
            .permissions()
            .mode()
            & 0o777;
        if after_mode == 0o000 {
            let _ = fs::set_permissions(&runa_target, fs::Permissions::from_mode(0o600));
        }
        let error =
            result.expect_err("hard-linked transcript events should fail before permission repair");
        assert!(
            error
                .to_string()
                .contains("refusing to seal multi-linked audit entry"),
            "error should come from hard-link preflight: {error}"
        );
        assert_eq!(before_mode, 0o000);
        assert_eq!(after_mode, 0o000);

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_transcript_does_not_chmod_symlink_targets_while_repairing_modes() {
        use std::os::unix::fs::symlink;

        let root = unique_test_dir("agentd-audit-transcript-repair-symlink");
        let record = prepare_session_audit_record_at(
            &root,
            "repair-symlink",
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
        let outside_target = root.join("outside-target.txt");
        fs::write(&outside_target, "outside\n").expect("outside target should be created");
        fs::set_permissions(&outside_target, fs::Permissions::from_mode(0o600))
            .expect("outside target should start restrictive");
        symlink(
            &outside_target,
            record.transcript_dir.join("unrelated-link"),
        )
        .expect("transcript symlink should be created");

        super::finalize_session_transcript(&record).expect("transcript should finalize");

        let outside_mode = fs::metadata(&outside_target)
            .expect("outside target metadata should exist")
            .permissions()
            .mode();
        assert_eq!(outside_mode & 0o777, 0o600);

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_transcript_rejects_symlinked_events_jsonl_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = unique_test_dir("agentd-audit-symlinked-transcript-events");
        let record = prepare_session_audit_record_at(
            &root,
            "symlinked-events",
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
        let outside_target = root.join("outside-events.jsonl");
        fs::write(&outside_target, "outside secret\n").expect("outside target should be created");
        let events_path = crate::transcript::TranscriptEventSource::from_record(&record)
            .event_file_path_for_work_unit("survey");
        fs::create_dir_all(events_path.parent().expect("event path parent"))
            .expect("nested event directory should be created");
        symlink(&outside_target, &events_path)
            .expect("symlinked events artifact should be created");

        let error = super::finalize_session_transcript(&record)
            .expect_err("symlinked transcript events should fail finalization");

        assert!(
            error.to_string().contains("events.jsonl"),
            "error should name the unsafe artifact: {error}"
        );
        assert_eq!(
            fs::read_to_string(&outside_target).expect("outside target should remain readable"),
            "outside secret\n"
        );
        assert_eq!(
            fs::read_to_string(record.transcript_dir.join("manifest.json"))
                .expect("failure manifest should be written"),
            "{\n  \"schema_version\": 1,\n  \"coverage\": \"finalization_failed\",\n  \"event_schema_versions\": [],\n  \"finalization_error\": \"unsafe transcript artifact: events.jsonl is not a regular file\"\n}\n"
        );

        fs::remove_file(events_path).expect("symlink should be removable");
        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_transcript_rejects_fifo_events_jsonl_without_hanging() {
        let root = unique_test_dir("agentd-audit-fifo-transcript-events");
        let record = prepare_session_audit_record_at(
            &root,
            "fifo-events",
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
        let fifo_path = crate::transcript::TranscriptEventSource::from_record(&record)
            .event_file_path_for_work_unit("survey");
        fs::create_dir_all(fifo_path.parent().expect("event path parent"))
            .expect("nested event directory should be created");
        let status = Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("mkfifo should run");
        assert!(status.success(), "mkfifo should create events fifo");

        let finalize_record = record.clone();
        let handle = thread::spawn(move || super::finalize_session_transcript(&finalize_record));
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline && !handle.is_finished() {
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            handle.is_finished(),
            "transcript finalization must not hang on fifo events"
        );
        let error = handle
            .join()
            .expect("finalization thread should not panic")
            .expect_err("fifo transcript events should fail finalization");
        assert!(
            error.to_string().contains("events.jsonl"),
            "error should name the unsafe artifact: {error}"
        );

        fs::remove_file(&fifo_path).expect("fifo should be removable");
        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_transcript_rejects_preexisting_transcript_markdown_without_overwriting_it()
    {
        let root = unique_test_dir("agentd-audit-preexisting-transcript-markdown");
        let record = prepare_session_audit_record_at(
            &root,
            "preexisting-markdown",
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
        write_nested_events(
            &record,
            "survey",
            "{\"schema_version\":1,\"source\":\"runa\",\"kind\":\"agent_input\"}\n",
        );
        fs::write(record.transcript_dir.join("transcript.md"), "preexisting\n")
            .expect("preexisting markdown should be created");

        let error = super::finalize_session_transcript(&record)
            .expect_err("preexisting markdown should fail finalization");

        assert!(
            error.to_string().contains("transcript.md"),
            "error should name the preexisting artifact: {error}"
        );
        assert_eq!(
            fs::read_to_string(record.transcript_dir.join("transcript.md"))
                .expect("preexisting markdown should remain readable"),
            "preexisting\n"
        );
        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(record.transcript_dir.join("manifest.json"))
                .expect("failure manifest should be written"),
        )
        .expect("failure manifest should be json");
        assert_eq!(manifest["coverage"], "finalization_failed");
        assert!(
            manifest["finalization_error"]
                .as_str()
                .expect("failure error should be a string")
                .contains("transcript.md")
        );

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn finalize_session_transcript_rejects_preexisting_manifest_without_overwriting_it() {
        let root = unique_test_dir("agentd-audit-preexisting-transcript-manifest");
        let record = prepare_session_audit_record_at(
            &root,
            "preexisting-manifest",
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
        write_nested_events(
            &record,
            "survey",
            "{\"schema_version\":1,\"source\":\"runa\",\"kind\":\"agent_input\"}\n",
        );
        fs::write(record.transcript_dir.join("manifest.json"), "preexisting\n")
            .expect("preexisting manifest should be created");

        let error = super::finalize_session_transcript(&record)
            .expect_err("preexisting manifest should fail finalization");

        assert!(
            error.to_string().contains("manifest.json"),
            "error should name the preexisting artifact: {error}"
        );
        assert_eq!(
            fs::read_to_string(record.transcript_dir.join("manifest.json"))
                .expect("preexisting manifest should remain readable"),
            "preexisting\n"
        );

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn write_transcript_markdown_uses_a_wrapper_fence_longer_than_content_fences() {
        let root = unique_test_dir("agentd-audit-fenced-transcript");
        let record = prepare_session_audit_record_at(
            &root,
            "fenced-transcript",
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
        let content = "example\n```python\nprint('hello')\n```\n```json\n{\"ok\":true}\n```\n```toml\nname = \"agentd\"\n```";
        let event = serde_json::json!({
            "schema_version": 1,
            "source": "runa",
            "kind": "agent_output",
            "content": content,
        });
        write_nested_events(&record, "survey", &format!("{event}\n"));

        super::finalize_session_transcript(&record).expect("transcript should finalize");

        let markdown = fs::read_to_string(record.transcript_dir.join("transcript.md"))
            .expect("human-readable transcript should exist");
        let expected_block = format!("````text\n{content}\n````\n\n");
        assert!(
            markdown.contains(&expected_block),
            "outer fence should remain open around fenced content:\n{markdown}"
        );

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    #[test]
    fn transcript_coverage_uses_parsed_event_sources() {
        assert_eq!(classify_transcript_fixture(""), "no_events");
        assert_eq!(classify_transcript_fixture("\n  \n\t\n"), "no_events");
        assert_eq!(
            classify_transcript_fixture(
                r#"{"schema_version":1,"source":"runa","kind":"agent_input"}"#
            ),
            "missing_mcp_events"
        );
        assert_eq!(
            classify_transcript_fixture(
                r#"{"schema_version":1,"source": "runa-mcp","kind":"tool_call"}"#
            ),
            "full"
        );
        assert_eq!(
            classify_transcript_fixture(
                r#"{"schema_version":1,"source":"runa","kind":"agent_output","content":"{\"source\":\"runa-mcp\"}"}"#
            ),
            "missing_mcp_events"
        );
    }

    fn classify_transcript_fixture(events: &str) -> &'static str {
        let root = unique_test_dir("agentd-audit-coverage-fixture");
        fs::create_dir_all(&root).expect("coverage fixture dir should be created");
        let events_path = root.join("events.jsonl");
        fs::write(&events_path, events).expect("coverage fixture should be written");
        let events_file = fs::File::open(&events_path).expect("coverage fixture should open");
        let mut summary = super::TranscriptSummary::default();
        super::summarize_transcript_events(events_file, &mut summary)
            .expect("coverage classification should run");
        let coverage = summary.coverage();
        fs::remove_dir_all(root).expect("coverage fixture dir should be removed");
        coverage
    }

    #[test]
    fn finalize_session_transcript_streams_large_events_without_large_rss_growth() {
        const CHILD_ENV: &str = "AGENTD_LARGE_TRANSCRIPT_STREAMING_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            let current_exe = std::env::current_exe().expect("current test binary should exist");
            let output = Command::new(current_exe)
                .arg("--exact")
                .arg("audit::tests::finalize_session_transcript_streams_large_events_without_large_rss_growth")
                .arg("--nocapture")
                .env(CHILD_ENV, "1")
                .output()
                .expect("child test process should run");
            assert!(
                output.status.success(),
                "child streaming regression failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let root = unique_test_dir("agentd-audit-large-transcript");
        let record = prepare_session_audit_record_at(
            &root,
            "large-transcript",
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
        let events_path = crate::transcript::TranscriptEventSource::from_record(&record)
            .event_file_path_for_work_unit("survey");
        fs::create_dir_all(events_path.parent().expect("event path parent"))
            .expect("nested event directory should be created");
        let mut events_file =
            fs::File::create(&events_path).expect("large events should be created");
        let content = "x".repeat(512);
        for index in 0..90_000 {
            writeln!(
                events_file,
                "{{\"schema_version\":1,\"source\":\"runa\",\"kind\":\"agent_output\",\"content\":\"{content}-{index}\"}}"
            )
            .expect("large event should be written");
        }
        drop(events_file);

        let before_kib =
            current_rss_high_water_kib().expect("linux rss high-water should be readable");
        super::finalize_session_transcript(&record).expect("large transcript should finalize");
        let after_kib =
            current_rss_high_water_kib().expect("linux rss high-water should be readable");
        let growth_kib = after_kib.saturating_sub(before_kib);
        assert!(
            growth_kib < 32 * 1024,
            "transcript finalization grew RSS high-water by {growth_kib} KiB"
        );

        fs::remove_dir_all(root).expect("temporary audit root should be removed");
    }

    fn current_rss_high_water_kib() -> Option<u64> {
        let status = fs::read_to_string("/proc/self/status").ok()?;
        status.lines().find_map(|line| {
            let value = line.strip_prefix("VmHWM:")?;
            value.split_whitespace().next()?.parse().ok()
        })
    }

    #[test]
    fn current_timestamp_emits_rfc3339_utc_values() {
        let timestamp = current_timestamp().expect("timestamp should format");
        assert!(timestamp.ends_with('Z'));
        assert!(timestamp.contains('T'));
    }
}
