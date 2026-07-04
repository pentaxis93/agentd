//! Shared transcript identity and path resolution for runa event records.
//!
//! Runa emits events below
//! `deployments/<deployment>/work-units/<work-unit>/runs/<run-id>/events.jsonl`.
//! agentd owns the deployment and run identifiers it injects into runa, then
//! reads only paths addressable from those injected values.

use crate::audit::SessionAuditRecord;
use crate::types::RunnerError;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

pub(crate) const TRANSCRIPT_DEPLOYMENT_ENV: &str = "RUNA_TRANSCRIPT_DEPLOYMENT";
pub(crate) const TRANSCRIPT_RUN_ID_ENV: &str = "RUNA_TRANSCRIPT_RUN_ID";

const DEPLOYMENTS_DIR: &str = "deployments";
const WORK_UNITS_DIR: &str = "work-units";
const RUNS_DIR: &str = "runs";
const EVENTS_FILE: &str = "events.jsonl";
#[cfg(test)]
const UNSCOPED_WORK_UNIT: &str = "_unscoped";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptIdentity {
    deployment: String,
    run_id: String,
}

impl TranscriptIdentity {
    pub(crate) fn new(forge_type: &str, repo_url: &str, session_id: &str) -> Self {
        Self {
            deployment: deployment_identity(forge_type, repo_url),
            run_id: session_id.to_string(),
        }
    }

    pub(crate) fn deployment(&self) -> &str {
        &self.deployment
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptEventSource {
    transcript_dir: PathBuf,
    identity: TranscriptIdentity,
}

impl TranscriptEventSource {
    pub(crate) fn from_record(record: &SessionAuditRecord) -> Self {
        Self {
            transcript_dir: record.transcript_dir.clone(),
            identity: record.transcript_identity.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn event_file_path_for_work_unit(&self, work_unit: &str) -> PathBuf {
        self.work_units_dir()
            .join(encode_path_component(work_unit))
            .join(RUNS_DIR)
            .join(encode_path_component(self.identity.run_id()))
            .join(EVENTS_FILE)
    }

    pub(crate) fn discover_event_files(&self) -> Result<Vec<PathBuf>, RunnerError> {
        let work_units_dir = self.work_units_dir();
        let entries = match fs::read_dir(&work_units_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(RunnerError::Io(error)),
        };

        let encoded_run_id = encode_path_component(self.identity.run_id());
        let mut event_files = Vec::new();
        for entry in entries {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            let event_path = entry
                .path()
                .join(RUNS_DIR)
                .join(&encoded_run_id)
                .join(EVENTS_FILE);
            match fs::symlink_metadata(&event_path) {
                Ok(_) => event_files.push(event_path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(RunnerError::Io(error)),
            }
        }
        event_files.sort();
        Ok(event_files)
    }

    fn work_units_dir(&self) -> PathBuf {
        self.transcript_dir
            .join(DEPLOYMENTS_DIR)
            .join(encode_path_component(self.identity.deployment()))
            .join(WORK_UNITS_DIR)
    }
}

#[cfg(test)]
pub(crate) fn scoped_work_unit_component(work_unit: Option<&str>) -> &str {
    work_unit.unwrap_or(UNSCOPED_WORK_UNIT)
}

pub(crate) fn encode_path_component(component: &str) -> String {
    if component.is_empty() {
        return "_empty".to_string();
    }

    let all_dots = component.bytes().all(|byte| byte == b'.');
    let mut encoded = String::new();
    for byte in component.bytes() {
        if !all_dots && is_safe_path_component_byte(byte) {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(nibble_to_hex(byte >> 4));
            encoded.push(nibble_to_hex(byte & 0x0f));
        }
    }
    encoded
}

fn deployment_identity(forge_type: &str, repo_url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentd transcript deployment identity v1\0");
    hasher.update(forge_type.as_bytes());
    hasher.update(b"\0");
    hasher.update(repo_url.as_bytes());
    let digest = hasher.finalize();

    let mut identity = String::from("agentd-");
    for byte in digest {
        identity.push(nibble_to_hex(byte >> 4));
        identity.push(nibble_to_hex(byte & 0x0f));
    }
    identity
}

fn is_safe_path_component_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

fn nibble_to_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + (nibble - 10)) as char,
        _ => unreachable!("nibble must be four bits"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_component_matches_runa_for_safe_empty_dotted_and_unsafe_segments() {
        assert_eq!(encode_path_component("safe-AZaz09_."), "safe-AZaz09_.");
        assert_eq!(encode_path_component(""), "_empty");
        assert_eq!(encode_path_component("."), "%2E");
        assert_eq!(encode_path_component("..."), "%2E%2E%2E");
        assert_eq!(
            encode_path_component("take/stage#1 space"),
            "take%2Fstage%231%20space"
        );
    }

    #[test]
    fn transcript_identity_is_stable_for_project_and_uses_session_id_as_run_id() {
        let first = TranscriptIdentity::new("github", "https://example.com/agentd.git", "abc123");
        let second = TranscriptIdentity::new("github", "https://example.com/agentd.git", "abc123");
        let other_repo =
            TranscriptIdentity::new("github", "https://example.com/other.git", "abc123");

        assert_eq!(first, second);
        assert_ne!(first.deployment(), other_repo.deployment());
        assert_eq!(first.run_id(), "abc123");
        assert!(first.deployment().starts_with("agentd-"));
    }

    #[test]
    fn unscoped_work_unit_component_matches_runa_fallback_component() {
        assert_eq!(scoped_work_unit_component(None), "_unscoped");
        assert_eq!(scoped_work_unit_component(Some("issue-162")), "issue-162");
    }
}
