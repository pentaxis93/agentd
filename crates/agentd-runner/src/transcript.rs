//! Shared transcript identity and path resolution for runa event records.
//!
//! Runa emits events below
//! `deployments/<deployment>/work-units/<work-unit>/runs/<run-id>/events.jsonl`.
//! agentd owns the deployment and run identifiers it injects into runa, then
//! reads only paths addressable from those injected values.

use crate::audit::SessionAuditRecord;
use crate::types::RunnerError;
use sha2::{Digest, Sha256};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TranscriptEventFile {
    path: PathBuf,
    work_unit_component: OsString,
}

impl TranscriptEventFile {
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
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

    pub(crate) fn discover_event_files(&self) -> Result<Vec<TranscriptEventFile>, RunnerError> {
        let base_dir = open_base_dir(&self.transcript_dir)?;
        let Some(deployments_dir) = open_optional_dir_at(&base_dir, OsStr::new(DEPLOYMENTS_DIR))?
        else {
            return Ok(Vec::new());
        };
        let encoded_deployment = encode_path_component(self.identity.deployment());
        let Some(deployment_dir) =
            open_optional_dir_at(&deployments_dir, OsStr::new(&encoded_deployment))?
        else {
            return Ok(Vec::new());
        };
        let Some(work_units_dir) =
            open_optional_dir_at(&deployment_dir, OsStr::new(WORK_UNITS_DIR))?
        else {
            return Ok(Vec::new());
        };

        let encoded_run_id = OsString::from(encode_path_component(self.identity.run_id()));
        let work_units_path = self.work_units_dir();
        let mut event_files = Vec::new();
        for work_unit_component in read_dir_names(&work_units_dir)? {
            let Some(metadata) = metadata_at(&work_units_dir, &work_unit_component)? else {
                continue;
            };
            if !is_directory(metadata.st_mode) {
                continue;
            }

            let Some(work_unit_dir) = open_optional_dir_at(&work_units_dir, &work_unit_component)?
            else {
                continue;
            };
            let Some(runs_dir) = open_optional_dir_at(&work_unit_dir, OsStr::new(RUNS_DIR))? else {
                continue;
            };
            let Some(run_dir) = open_optional_dir_at(&runs_dir, &encoded_run_id)? else {
                continue;
            };
            if entry_exists_at(&run_dir, OsStr::new(EVENTS_FILE))? {
                let path = work_units_path
                    .join(&work_unit_component)
                    .join(RUNS_DIR)
                    .join(&encoded_run_id)
                    .join(EVENTS_FILE);
                event_files.push(TranscriptEventFile {
                    path,
                    work_unit_component,
                });
            }
        }
        event_files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(event_files)
    }

    pub(crate) fn open_event_file(
        &self,
        event_file: &TranscriptEventFile,
    ) -> Result<File, RunnerError> {
        let base_dir = open_base_dir(&self.transcript_dir)?;
        let deployments_dir = open_required_dir_at(&base_dir, OsStr::new(DEPLOYMENTS_DIR))?;
        let encoded_deployment = encode_path_component(self.identity.deployment());
        let deployment_dir =
            open_required_dir_at(&deployments_dir, OsStr::new(&encoded_deployment))?;
        let work_units_dir = open_required_dir_at(&deployment_dir, OsStr::new(WORK_UNITS_DIR))?;
        let work_unit_dir = open_required_dir_at(&work_units_dir, &event_file.work_unit_component)?;
        let runs_dir = open_required_dir_at(&work_unit_dir, OsStr::new(RUNS_DIR))?;
        let encoded_run_id = OsString::from(encode_path_component(self.identity.run_id()));
        let run_dir = open_required_dir_at(&runs_dir, &encoded_run_id)?;
        open_regular_file_at(&run_dir, OsStr::new(EVENTS_FILE))
    }

    fn work_units_dir(&self) -> PathBuf {
        self.transcript_dir
            .join(DEPLOYMENTS_DIR)
            .join(encode_path_component(self.identity.deployment()))
            .join(WORK_UNITS_DIR)
    }
}

fn open_base_dir(path: &Path) -> Result<File, RunnerError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| match error.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => {
                unsafe_transcript_path_error(&path.display().to_string(), "is not a directory")
            }
            _ => RunnerError::Io(error),
        })
}

fn open_required_dir_at(parent: &File, component: &OsStr) -> Result<File, RunnerError> {
    open_optional_dir_at(parent, component)?.ok_or_else(|| {
        RunnerError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "transcript directory component not found: {}",
                component.to_string_lossy()
            ),
        ))
    })
}

fn open_optional_dir_at(parent: &File, component: &OsStr) -> Result<Option<File>, RunnerError> {
    let component = cstring_component(component)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        return Ok(Some(unsafe { File::from_raw_fd(fd) }));
    }

    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOENT) => Ok(None),
        Some(libc::ELOOP) | Some(libc::ENOTDIR) => Err(unsafe_transcript_path_error(
            &component.to_string_lossy(),
            "is not a directory",
        )),
        _ => Err(RunnerError::Io(error)),
    }
}

fn read_dir_names(dir: &File) -> Result<Vec<OsString>, RunnerError> {
    let proc_fd_path = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
    let mut names = Vec::new();
    for entry in fs::read_dir(proc_fd_path)? {
        names.push(entry?.file_name());
    }
    Ok(names)
}

fn metadata_at(parent: &File, component: &OsStr) -> Result<Option<libc::stat>, RunnerError> {
    let component = cstring_component(component)?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            component.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        return Ok(Some(unsafe { metadata.assume_init() }));
    }

    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOENT) => Ok(None),
        _ => Err(RunnerError::Io(error)),
    }
}

fn entry_exists_at(parent: &File, component: &OsStr) -> Result<bool, RunnerError> {
    metadata_at(parent, component).map(|metadata| metadata.is_some())
}

fn open_regular_file_at(parent: &File, component: &OsStr) -> Result<File, RunnerError> {
    let metadata = metadata_at(parent, component)?.ok_or_else(|| {
        RunnerError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "transcript event file not found: {}",
                component.to_string_lossy()
            ),
        ))
    })?;
    if !is_regular_file(metadata.st_mode) {
        return Err(unsafe_transcript_artifact_error(
            EVENTS_FILE,
            "is not a regular file",
        ));
    }

    let component = cstring_component(component)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            component.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::ELOOP) => {
                unsafe_transcript_artifact_error(EVENTS_FILE, "is not a regular file")
            }
            _ => RunnerError::Io(error),
        });
    }

    let file = unsafe { File::from_raw_fd(fd) };
    let mut open_metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe { libc::fstat(file.as_raw_fd(), open_metadata.as_mut_ptr()) };
    if result != 0 {
        return Err(RunnerError::Io(io::Error::last_os_error()));
    }
    let open_metadata = unsafe { open_metadata.assume_init() };
    if !is_regular_file(open_metadata.st_mode)
        || open_metadata.st_dev != metadata.st_dev
        || open_metadata.st_ino != metadata.st_ino
    {
        return Err(unsafe_transcript_artifact_error(
            EVENTS_FILE,
            "is not a regular file",
        ));
    }

    Ok(file)
}

fn cstring_component(component: &OsStr) -> Result<CString, RunnerError> {
    if component.as_bytes().contains(&b'/') {
        return Err(unsafe_transcript_path_error(
            &component.to_string_lossy(),
            "contains a path separator",
        ));
    }
    CString::new(component.as_bytes()).map_err(|error| {
        RunnerError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path component contains interior nul byte: {error}"),
        ))
    })
}

fn is_directory(mode: libc::mode_t) -> bool {
    (mode & libc::S_IFMT) == libc::S_IFDIR
}

fn is_regular_file(mode: libc::mode_t) -> bool {
    (mode & libc::S_IFMT) == libc::S_IFREG
}

fn unsafe_transcript_path_error(component: &str, reason: &str) -> RunnerError {
    RunnerError::Io(io::Error::other(format!(
        "unsafe transcript path: {component} {reason}"
    )))
}

fn unsafe_transcript_artifact_error(artifact: &str, reason: &str) -> RunnerError {
    RunnerError::Io(io::Error::other(format!(
        "unsafe transcript artifact: {artifact} {reason}"
    )))
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
