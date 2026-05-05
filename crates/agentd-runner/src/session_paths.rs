use std::path::PathBuf;

const HOME_ROOT_DIR: &str = "/home";
const REPO_DIR_NAME: &str = "repo";
const INTERNAL_AGENTD_DIR_NAME: &str = ".agentd";
const INTERNAL_AUDIT_DIR_NAME: &str = "audit";
const INTERNAL_AUDIT_RUNA_DIR_NAME: &str = "runa";
const TRANSCRIPT_MOUNT_PATH: &str = "/agentd/transcript";
const REPO_RUNA_DIR_NAME: &str = ".runa";

pub(crate) fn session_home_dir(agent_name: &str) -> PathBuf {
    PathBuf::from(HOME_ROOT_DIR).join(agent_name)
}

pub(crate) fn session_repo_dir(agent_name: &str) -> PathBuf {
    session_home_dir(agent_name).join(REPO_DIR_NAME)
}

pub(crate) fn session_internal_agentd_dir(agent_name: &str) -> PathBuf {
    session_home_dir(agent_name).join(INTERNAL_AGENTD_DIR_NAME)
}

pub(crate) fn session_internal_audit_dir(agent_name: &str) -> PathBuf {
    session_internal_agentd_dir(agent_name).join(INTERNAL_AUDIT_DIR_NAME)
}

pub(crate) fn session_internal_audit_runa_dir(agent_name: &str) -> PathBuf {
    session_internal_audit_dir(agent_name).join(INTERNAL_AUDIT_RUNA_DIR_NAME)
}

pub(crate) fn session_repo_runa_dir(agent_name: &str) -> PathBuf {
    session_repo_dir(agent_name).join(REPO_RUNA_DIR_NAME)
}

pub(crate) fn session_transcript_mount_dir() -> PathBuf {
    PathBuf::from(TRANSCRIPT_MOUNT_PATH)
}
