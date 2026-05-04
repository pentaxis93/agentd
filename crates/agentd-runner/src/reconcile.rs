use std::collections::BTreeSet;
use std::io::{Error as IoError, ErrorKind};

use crate::naming::{
    PODMAN_RESOURCE_PREFIX, is_daemon_instance_id, parse_container_name, parse_secret_name,
};
use crate::podman::run_podman_command;
use crate::resources::cleanup_podman_secret_names;
use crate::{RunnerError, StartupReconciliationReport};
use serde::Deserialize;

/// Removes stale runner-managed Podman resources before the daemon begins
/// accepting new work.
pub fn reconcile_startup_resources(
    daemon_instance_id: &str,
) -> Result<StartupReconciliationReport, RunnerError> {
    if !is_daemon_instance_id(daemon_instance_id) {
        return Err(RunnerError::InvalidDaemonInstanceId);
    }

    let containers = list_agentd_containers()?;

    let removed_container_names = containers
        .iter()
        .filter(|container| container.daemon_instance_id == daemon_instance_id)
        .filter(|container| container.startup_reconciliation == StartupReconciliation::Remove)
        .map(|container| container.name.clone())
        .collect::<Vec<_>>();
    remove_containers(&removed_container_names)?;

    let live_session_ids = containers
        .iter()
        .filter(|container| container.daemon_instance_id == daemon_instance_id)
        .filter(|container| container.startup_reconciliation == StartupReconciliation::Preserve)
        .map(|container| container.session_id.as_str())
        .collect::<BTreeSet<_>>();

    let removed_secret_names = list_agentd_secret_names()?
        .into_iter()
        .filter(|name| {
            parse_secret_name(name)
                .filter(|parts| parts.daemon_instance_id == daemon_instance_id)
                .map(|parts| !live_session_ids.contains(parts.session_id))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    remove_secrets(&removed_secret_names)?;

    Ok(StartupReconciliationReport {
        removed_container_names,
        removed_secret_names,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerRecord {
    daemon_instance_id: String,
    name: String,
    session_id: String,
    startup_reconciliation: StartupReconciliation,
}

#[derive(Debug, Deserialize)]
struct PodmanPsRecord {
    #[serde(rename = "Names")]
    names: Vec<String>,
    #[serde(rename = "State")]
    state: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupReconciliation {
    Remove,
    Preserve,
}

fn list_agentd_containers() -> Result<Vec<ContainerRecord>, RunnerError> {
    let output = run_podman_command(vec![
        "ps".to_string(),
        "-a".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ])?;

    let records: Vec<PodmanPsRecord> = serde_json::from_str(&output).map_err(|error| {
        RunnerError::Io(IoError::new(
            ErrorKind::InvalidData,
            format!("invalid podman ps --format json output: {error}"),
        ))
    })?;

    Ok(records
        .into_iter()
        .filter_map(parse_container_record)
        .collect())
}

fn parse_container_record(record: PodmanPsRecord) -> Option<ContainerRecord> {
    let name = record
        .names
        .into_iter()
        .find(|name| parse_container_name(name).is_some())?;
    let parts = parse_container_name(&name)?;
    let daemon_instance_id = parts.daemon_instance_id.to_string();
    let session_id = parts.session_id.to_string();

    Some(ContainerRecord {
        daemon_instance_id,
        startup_reconciliation: classify_startup_reconciliation(record.state.trim()),
        name,
        session_id,
    })
}

fn classify_startup_reconciliation(state: &str) -> StartupReconciliation {
    match state {
        "exited" | "dead" | "stopped" | "created" | "initialized" => StartupReconciliation::Remove,
        _ => StartupReconciliation::Preserve,
    }
}

fn list_agentd_secret_names() -> Result<Vec<String>, RunnerError> {
    Ok(run_podman_command(vec![
        "secret".to_string(),
        "ls".to_string(),
        "--format".to_string(),
        "{{.Name}}".to_string(),
    ])?
    .lines()
    .map(str::trim)
    .filter(|line| line.starts_with(PODMAN_RESOURCE_PREFIX))
    .map(ToString::to_string)
    .collect())
}

fn remove_containers(container_names: &[String]) -> Result<(), RunnerError> {
    if container_names.is_empty() {
        return Ok(());
    }

    let mut args = vec![
        "rm".to_string(),
        "--force".to_string(),
        "--ignore".to_string(),
    ];
    args.extend(container_names.iter().cloned());
    run_podman_command(args).map(|_| ())
}

fn remove_secrets(secret_names: &[String]) -> Result<(), RunnerError> {
    cleanup_podman_secret_names(secret_names)
}
