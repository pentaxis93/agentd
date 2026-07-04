//! Container creation, execution, and exit classification.
//!
//! Manages the podman container lifecycle: building the entrypoint script,
//! assembling `podman create` arguments, running the container in attached
//! mode, and classifying the exit result. The container runs as root (UID 0)
//! for privileged setup, then drops to an unprivileged agent user via `gosu`
//! before executing the command. Exit code 125 from `podman start
//! --attach` is ambiguous (podman infrastructure error vs. container process)
//! and requires container state inspection to disambiguate.

use crate::input::{INVOCATION_INPUT_MOUNT_PATH, ResolvedInvocationInput};
use crate::lifecycle::{LifecycleFailureKind, log_lifecycle_failure};
use crate::podman::{run_podman_command, run_podman_command_until};
use crate::resources::{SecretBinding, SessionResources, cleanup_podman_secrets};
use crate::session_paths::{
    session_home_dir, session_internal_audit_dir, session_internal_audit_runa_dir,
    session_repo_dir, session_repo_runa_dir, session_transcript_mount_dir,
};
use crate::transcript::{TRANSCRIPT_DEPLOYMENT_ENV, TRANSCRIPT_RUN_ID_ENV, TranscriptIdentity};
use crate::types::{BindMount, RunnerError, SessionInvocation, SessionOutcome, SessionSpec};
use crate::validation::{
    REPO_TOKEN_ENV, RepoUrlKind, TRANSCRIPT_DIR_ENV, TRANSCRIPT_REDACT_ENV, repo_url_kind,
    runner_managed_environment,
};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const ATTACHED_STDERR_TAIL_LIMIT: usize = 64 * 1024;
const ATTACHED_STDERR_TRUNCATION_NOTICE: &str = "[stderr truncated to last 65536 bytes]\n";
const SESSION_USER_ID: u32 = 1000;
const SESSION_GROUP_ID: u32 = 1000;
const METHODOLOGY_MOUNT_PATH: &str = "/agentd/methodology";
const METHODOLOGY_MANIFEST_PATH: &str = "/agentd/methodology/manifest.toml";
const PODMAN_INFRASTRUCTURE_ERROR_EXIT_CODE: i32 = 125;

pub(crate) fn create_container(
    resources: &SessionResources,
    spec: &SessionSpec,
    invocation: &SessionInvocation,
    resolved_input: Option<&ResolvedInvocationInput>,
) -> Result<(), RunnerError> {
    run_podman_command(build_create_container_args(
        resources,
        spec,
        invocation,
        resolved_input,
    ))
    .map(|_| ())
}

/// Run the created container attached until exit and classify the result.
///
/// Secret lifetime: once the container is observed `running`, the session's
/// podman secrets are deleted (`wait_for_container_exit`) — the values are
/// already inside the session environment, so the host-side secret store
/// holds them only for the startup window.
pub(crate) fn run_container_to_completion(
    container_name: &str,
    session_id: &str,
    secret_bindings: &[SecretBinding],
) -> Result<SessionOutcome, RunnerError> {
    let mut start = start_attached_container(container_name)?;
    let wait_result = wait_for_container_exit(
        &mut start.child,
        container_name,
        session_id,
        secret_bindings,
        None,
    );

    match wait_result {
        Ok(Some(status)) => {
            let (args, stderr) = finalize_attached_start(start)?;
            classify_attached_start_result(args, container_name, status, stderr)
        }
        Ok(None) => unreachable!("container wait without timeout should not return a timeout"),
        Err(error) => {
            cleanup_and_finalize_attached_start_after_wait_error(container_name, session_id, start);
            Err(error)
        }
    }
}

/// [`run_container_to_completion`] with a caller-enforced deadline. On
/// timeout the container is force-removed and the outcome is
/// [`SessionOutcome::TimedOut`] — a caller-layer outcome that commons
/// EXIT-CODES.md deliberately leaves outside the shared exit-code
/// vocabulary. Cleanup failures degrade to kill + log, never to a hung
/// session.
pub(crate) fn run_container_with_timeout(
    container_name: &str,
    session_id: &str,
    secret_bindings: &[SecretBinding],
    timeout: Duration,
) -> Result<SessionOutcome, RunnerError> {
    let mut start = start_attached_container(container_name)?;
    let wait_result = wait_for_container_exit(
        &mut start.child,
        container_name,
        session_id,
        secret_bindings,
        Some(timeout),
    );

    match wait_result {
        Ok(Some(status)) => {
            let (args, stderr) = finalize_attached_start(start)?;
            classify_attached_start_result(args, container_name, status, stderr)
        }
        Ok(None) => match cleanup_container(container_name) {
            Ok(()) => {
                finalize_attached_start(start).map(|_| ())?;
                Ok(SessionOutcome::TimedOut)
            }
            Err(error) => {
                log_lifecycle_failure(
                    LifecycleFailureKind::Cleanup,
                    "session execution",
                    container_name,
                    session_id,
                    &error,
                );
                if let Err(kill_error) = start.child.kill() {
                    log_lifecycle_failure(
                        LifecycleFailureKind::AttachedStartKill,
                        "session execution",
                        container_name,
                        session_id,
                        &kill_error,
                    );
                }
                if let Err(finalize_error) = finalize_attached_start(start).map(|_| ()) {
                    log_lifecycle_failure(
                        LifecycleFailureKind::AttachedStartFinalization,
                        "session execution",
                        container_name,
                        session_id,
                        &finalize_error,
                    );
                }
                Ok(SessionOutcome::TimedOut)
            }
        },
        Err(error) => {
            cleanup_and_finalize_attached_start_after_wait_error(container_name, session_id, start);
            Err(error)
        }
    }
}

/// Force-remove the session container, ignoring absence so cleanup is
/// idempotent across retries and crash-recovery paths.
pub(crate) fn cleanup_container(container_name: &str) -> Result<(), RunnerError> {
    run_podman_command(vec![
        "rm".to_string(),
        "--force".to_string(),
        "--ignore".to_string(),
        container_name.to_string(),
    ])
    .map(|_| ())
}

/// Generates the shell script passed as the container entrypoint via
/// `/bin/sh -lc`.
///
/// Privilege model: the script starts as root (UID 0) for privileged setup —
/// creating the agent's unix user, recursively re-owning pre-existing home
/// content while preserving host-backed mount targets, and, for HTTP(S)
/// repository URLs, running the clone itself as root: the repo token is
/// captured into a shell variable, `AGENTD_REPO_TOKEN` is unset immediately,
/// and the value is passed only as a one-shot `http.extraHeader` for that
/// single `git clone` (`build_clone_command`). SSH clones instead run as the
/// agent via `gosu` with `HOME` set so OpenSSH reads the mounted identity.
/// After a root clone, repository ownership is transferred to the agent
/// user. The final command is `exec gosu <agent> …`, so the drop to the
/// unprivileged session user is permanent for the agent workload — no root
/// process remains to return to. `set -eu` at the top aborts on any setup
/// failure rather than continuing with a broken workspace.
fn build_container_script(
    spec: &SessionSpec,
    invocation: &SessionInvocation,
    transcript_identity: &TranscriptIdentity,
    resolved_input: Option<&ResolvedInvocationInput>,
) -> String {
    let username = &spec.agent_name;
    let home_dir_path = session_home_dir(username);
    let home_dir = home_dir_path.display().to_string();
    let internal_audit_dir_path = session_internal_audit_dir(username);
    let internal_audit_dir = internal_audit_dir_path.display().to_string();
    let internal_audit_runa_dir_path = session_internal_audit_runa_dir(username);
    let internal_audit_runa_dir = internal_audit_runa_dir_path.display().to_string();
    let repo_dir_path = session_repo_dir(username);
    let repo_dir = repo_dir_path.display().to_string();
    let repo_runa_dir = session_repo_runa_dir(username).display().to_string();
    let user_group = format!("{username}:{username}");
    let mut script = String::from("set -eu\ngroupadd --gid ");
    script.push_str(&SESSION_GROUP_ID.to_string());
    script.push(' ');
    script.push_str(&shell_quote(username));
    script.push_str("\nuseradd --create-home --home-dir ");
    script.push_str(&shell_quote(&home_dir));
    script.push_str(" --shell /bin/sh --uid ");
    script.push_str(&SESSION_USER_ID.to_string());
    script.push_str(" --gid ");
    script.push_str(&shell_quote(username));
    script.push(' ');
    script.push_str(&shell_quote(username));
    script.push('\n');
    script.push_str("mkdir -p ");
    script.push_str(&shell_quote(&internal_audit_dir));
    script.push('\n');
    script.push_str(&build_home_ownership_command(
        &home_dir_path,
        &repo_dir_path,
        &spec.mounts,
        &internal_audit_runa_dir_path,
        &user_group,
    ));
    script.push_str("\nrm -rf ");
    script.push_str(&shell_quote(&repo_dir));
    script.push('\n');
    script.push_str(&build_clone_command(
        invocation,
        &repo_dir,
        &home_dir,
        &user_group,
    ));
    script.push_str("\ncd ");
    script.push_str(&shell_quote(&repo_dir));
    script.push_str("\nchown -R ");
    script.push_str(&shell_quote(&user_group));
    script.push(' ');
    script.push_str(&shell_quote(&repo_dir));
    script.push_str("\nif [ -e ");
    script.push_str(&shell_quote(&repo_runa_dir));
    script.push_str(" ] || [ -L ");
    script.push_str(&shell_quote(&repo_runa_dir));
    script.push_str(" ]; then\n");
    script.push_str("echo ");
    script.push_str(&shell_quote(
        "repo root .runa is reserved by agentd for persistent audit state",
    ));
    script.push_str(" >&2\nexit 6\nfi\nln -s ");
    script.push_str(&shell_quote(&internal_audit_runa_dir));
    script.push(' ');
    script.push_str(&shell_quote(&repo_runa_dir));
    script.push_str("\nexport HOME=");
    script.push_str(&shell_quote(&home_dir));
    if let Some(work_unit) = &invocation.work_unit {
        script.push_str("\nexport AGENTD_WORK_UNIT=");
        script.push_str(&shell_quote(work_unit));
    } else {
        script.push_str("\nunset AGENTD_WORK_UNIT");
    }
    script.push_str("\nexport ");
    script.push_str(TRANSCRIPT_DIR_ENV);
    script.push('=');
    script.push_str(&shell_quote(
        &session_transcript_mount_dir().display().to_string(),
    ));
    script.push_str("\nexport ");
    script.push_str(TRANSCRIPT_DEPLOYMENT_ENV);
    script.push('=');
    script.push_str(&shell_quote(transcript_identity.deployment()));
    script.push_str("\nexport ");
    script.push_str(TRANSCRIPT_RUN_ID_ENV);
    script.push('=');
    script.push_str(&shell_quote(transcript_identity.run_id()));
    let redact_env = transcript_redact_environment(spec, invocation);
    script.push_str("\nexport ");
    script.push_str(TRANSCRIPT_REDACT_ENV);
    script.push('=');
    script.push_str(&shell_quote(&redact_env));
    script.push_str("\ngosu ");
    script.push_str(&shell_quote(&user_group));
    script.push_str(" runa init --methodology ");
    script.push_str(&shell_quote(METHODOLOGY_MANIFEST_PATH));
    if let Some(resolved_input) = resolved_input {
        let workspace_dir = format!("{repo_runa_dir}/workspace/{}", resolved_input.artifact_type);
        let artifact_path = format!("{workspace_dir}/{}.json", resolved_input.artifact_id);
        let staged_document_path = format!("{INVOCATION_INPUT_MOUNT_PATH}/document.json");
        script.push_str("\ngosu ");
        script.push_str(&shell_quote(&user_group));
        script.push_str(" mkdir -p ");
        script.push_str(&shell_quote(&workspace_dir));
        script.push_str("\ngosu ");
        script.push_str(&shell_quote(&user_group));
        script.push_str(" cp ");
        script.push_str(&shell_quote(&staged_document_path));
        script.push(' ');
        script.push_str(&shell_quote(&artifact_path));
    }
    script.push_str("\nexec gosu ");
    script.push_str(&shell_quote(&user_group));
    script.push_str(" runa run");
    if let Some(work_unit) = &invocation.work_unit {
        script.push_str(" --work-unit ");
        script.push_str(&shell_quote(work_unit));
    }
    script.push_str(" --agent-command -- ");
    script.push_str(&shell_join(&spec.agent_command));

    script
}

fn build_home_ownership_command(
    home_dir: &Path,
    repo_dir: &Path,
    mounts: &[BindMount],
    internal_audit_runa_dir: &Path,
    user_group: &str,
) -> String {
    let mut prune_targets = mounts
        .iter()
        .filter_map(|mount| home_descendant_mount_target(home_dir, &mount.target))
        .collect::<Vec<_>>();
    prune_targets.push(internal_audit_runa_dir.display().to_string());
    prune_targets.push(repo_dir.display().to_string());

    let mut command = String::from("find ");
    command.push_str(&shell_quote(&home_dir.display().to_string()));
    // `find -path`, `-prune`, `-o`, and `-exec ... +` are POSIX. `-mindepth`
    // is a widely available extension that we rely on as part of the base
    // image contract so the home directory entry itself is re-owned.
    command.push_str(" -mindepth 0 \\( ");
    for (index, prune_target) in prune_targets.iter().enumerate() {
        if index > 0 {
            command.push_str(" -o ");
        }
        command.push_str("-path ");
        command.push_str(&shell_quote(prune_target));
    }
    command.push_str(" \\) -prune -o -exec chown ");
    command.push_str(&shell_quote(user_group));
    command.push_str(" {} +");
    command
}

fn home_descendant_mount_target(home_dir: &Path, mount_target: &Path) -> Option<String> {
    let relative_target = mount_target.strip_prefix(home_dir).ok()?;
    if relative_target.as_os_str().is_empty() {
        return None;
    }

    Some(mount_target.display().to_string())
}

fn build_clone_command(
    invocation: &SessionInvocation,
    repo_dir: &str,
    home_dir: &str,
    user_group: &str,
) -> String {
    let mut command = String::new();

    if repo_url_kind(&invocation.repo_url) == RepoUrlKind::Ssh {
        command.push_str("GIT_TERMINAL_PROMPT=0 HOME=");
        command.push_str(&shell_quote(home_dir));
        command.push_str(" gosu ");
        command.push_str(&shell_quote(user_group));
        command.push_str(" git clone --no-hardlinks -- ");
        command.push_str(&shell_quote(&invocation.repo_url));
        command.push(' ');
        command.push_str(&shell_quote(repo_dir));
        return command;
    }

    if invocation.repo_token.is_some() {
        command.push_str("repo_token=${");
        command.push_str(REPO_TOKEN_ENV);
        command.push_str("-}\nunset ");
        command.push_str(REPO_TOKEN_ENV);
        command.push_str("\nif [ -n \"$repo_token\" ]; then\n");
        command.push_str("GIT_CONFIG_COUNT=1 ");
        command.push_str("GIT_CONFIG_KEY_0=http.extraHeader ");
        command.push_str(
            "GIT_CONFIG_VALUE_0=\"$(printf 'Authorization: Bearer %s' \"$repo_token\")\" ",
        );
        command.push_str("GIT_TERMINAL_PROMPT=0 git clone --no-hardlinks -- ");
        command.push_str(&shell_quote(&invocation.repo_url));
        command.push(' ');
        command.push_str(&shell_quote(repo_dir));
        command.push_str("\nelse\n");
        command.push_str("GIT_TERMINAL_PROMPT=0 git clone --no-hardlinks -- ");
        command.push_str(&shell_quote(&invocation.repo_url));
        command.push(' ');
        command.push_str(&shell_quote(repo_dir));
        command.push_str("\nfi\nunset repo_token");
        return command;
    }

    command.push_str("GIT_TERMINAL_PROMPT=0 git clone --no-hardlinks -- ");
    command.push_str(&shell_quote(&invocation.repo_url));
    command.push(' ');
    command.push_str(&shell_quote(repo_dir));
    command
}

fn transcript_redact_environment(spec: &SessionSpec, invocation: &SessionInvocation) -> String {
    let mut names = spec
        .environment
        .iter()
        .map(|variable| variable.name.as_str())
        .collect::<Vec<_>>();
    if invocation
        .repo_token
        .as_deref()
        .is_some_and(|token| !token.is_empty())
    {
        names.push(REPO_TOKEN_ENV);
    }
    names.join(",")
}

fn build_create_container_args(
    resources: &SessionResources,
    spec: &SessionSpec,
    invocation: &SessionInvocation,
    resolved_input: Option<&ResolvedInvocationInput>,
) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        "--name".to_string(),
        resources.container_name.clone(),
        "--userns".to_string(),
        format!("keep-id:uid={SESSION_USER_ID},gid={SESSION_GROUP_ID}"),
    ];

    push_bind_mount_arg(
        &mut args,
        resources.methodology_mount_source.as_path(),
        Path::new(METHODOLOGY_MOUNT_PATH),
        true,
        true,
    );

    push_prepared_bind_mount_arg(&mut args, &resources.audit_mount);
    push_prepared_bind_mount_arg(&mut args, &resources.transcript_mount);

    if let Some(mount) = &resources.invocation_input_mount {
        push_prepared_bind_mount_arg(&mut args, mount);
    }

    for mount in &resources.additional_mounts {
        push_prepared_bind_mount_arg(&mut args, mount);
    }

    let mut secret_bindings = resources.environment_secret_bindings.iter();

    for variable in &spec.environment {
        if variable.value.is_empty() {
            args.push("--env".to_string());
            args.push(format!("{}=", variable.name));
            continue;
        }

        let binding = secret_bindings
            .next()
            .expect("non-empty environment values should have matching secret bindings");
        debug_assert_eq!(binding.target_name, variable.name);

        args.push("--secret".to_string());
        args.push(format!(
            "{},type=env,target={}",
            binding.secret_name, binding.target_name
        ));
    }
    debug_assert!(
        secret_bindings.next().is_none(),
        "all secret bindings should be consumed when building create args"
    );

    if let Some(binding) = &resources.repo_token_secret_binding {
        args.push("--secret".to_string());
        args.push(format!(
            "{},type=env,target={}",
            binding.secret_name, binding.target_name
        ));
    }

    for (name, value) in runner_managed_environment(spec) {
        args.push("--env".to_string());
        args.push(format!("{name}={value}"));
    }

    args.push("--user".to_string());
    args.push("0:0".to_string());
    args.push("--entrypoint".to_string());
    args.push("/bin/sh".to_string());
    args.push(spec.base_image.clone());
    args.push("-lc".to_string());
    args.push(build_container_script(
        spec,
        invocation,
        &resources.audit_record.transcript_identity,
        resolved_input,
    ));

    args
}

fn push_prepared_bind_mount_arg(
    args: &mut Vec<String>,
    mount: &crate::resources::PreparedBindMount,
) {
    push_bind_mount_arg(
        args,
        &mount.source,
        &mount.target,
        mount.read_only,
        mount.relabel_shared,
    );
}

fn push_bind_mount_arg(
    args: &mut Vec<String>,
    source: &Path,
    target: &Path,
    read_only: bool,
    relabel_shared: bool,
) {
    if relabel_shared && source.to_string_lossy().contains(',') {
        args.push("--volume".to_string());
        args.push(format!(
            "{}:{}:{},z",
            source.display(),
            target.display(),
            if read_only { "ro" } else { "rw" }
        ));
        return;
    }

    args.push("--mount".to_string());
    let mut mount_value = format!(
        "type=bind,src={},target={},ro={}",
        source.display(),
        target.display(),
        read_only
    );
    if relabel_shared {
        mount_value.push_str(",relabel=shared");
    }
    args.push(mount_value);
}

fn cleanup_and_finalize_attached_start_after_wait_error(
    container_name: &str,
    session_id: &str,
    start: AttachedPodmanStart,
) {
    if let Err(error) = cleanup_container(container_name) {
        log_lifecycle_failure(
            LifecycleFailureKind::Cleanup,
            "session execution",
            container_name,
            session_id,
            &error,
        );
    }

    if let Err(error) = finalize_attached_start(start).map(|_| ()) {
        log_lifecycle_failure(
            LifecycleFailureKind::AttachedStartFinalization,
            "session execution",
            container_name,
            session_id,
            &error,
        );
    }
}

fn finalize_attached_start(
    mut start: AttachedPodmanStart,
) -> Result<(Vec<String>, String), RunnerError> {
    start.child.wait()?;
    let stderr = finish_captured_stderr(start.stderr_thread)?;
    Ok((start.args, stderr))
}

// Polls the attached `podman start` child process for exit, optionally
// enforcing a timeout deadline. While waiting, inspects the container status
// to detect the "running" state. Once running is confirmed, releases the
// backing podman secrets so credential material is removed from the host
// secret store while the container continues using its in-memory environment
// copy. Returns `Some(ExitStatus)` on natural completion or `None` on timeout.
fn wait_for_container_exit(
    child: &mut Child,
    container_name: &str,
    session_id: &str,
    secret_bindings: &[SecretBinding],
    timeout: Option<Duration>,
) -> Result<Option<ExitStatus>, RunnerError> {
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let mut secrets_released = secret_bindings.is_empty();

    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            return Ok(None);
        }

        if !secrets_released {
            let running = match deadline {
                Some(deadline) => match inspect_container_status_until(container_name, deadline)? {
                    Some(status) => status == "running",
                    None => {
                        if let Some(status) = child.try_wait()? {
                            return Ok(Some(status));
                        }
                        return Ok(None);
                    }
                },
                None => inspect_container_status(container_name)? == "running",
            };

            if running {
                match cleanup_podman_secrets(secret_bindings) {
                    Ok(()) => {}
                    Err(error) => log_lifecycle_failure(
                        LifecycleFailureKind::Cleanup,
                        "secret release",
                        container_name,
                        session_id,
                        &error,
                    ),
                }
                secrets_released = true;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

struct AttachedPodmanStart {
    args: Vec<String>,
    child: Child,
    stderr_thread: thread::JoinHandle<std::io::Result<String>>,
}

fn start_attached_container(container_name: &str) -> Result<AttachedPodmanStart, RunnerError> {
    let args = vec![
        "start".to_string(),
        "--attach".to_string(),
        container_name.to_string(),
    ];
    let mut child = Command::new("podman")
        .args(&args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = child
        .stderr
        .take()
        .expect("podman stderr should be piped when capturing attached startup errors");

    Ok(AttachedPodmanStart {
        args,
        child,
        stderr_thread: thread::spawn(move || forward_and_capture_stderr(stderr)),
    })
}

// Interprets the exit status of `podman start --attach`. Exit code 125 is
// ambiguous: podman uses it for infrastructure errors, but the container
// process itself may have exited 125. When exit code 125 is observed, inspects
// the container's terminal state via `podman inspect`. If the container reached
// a terminal state (exited/stopped), uses the container's own exit code as the
// session outcome. Otherwise surfaces a `PodmanCommandFailed` error.
fn classify_attached_start_result(
    args: Vec<String>,
    container_name: &str,
    status: ExitStatus,
    stderr: String,
) -> Result<SessionOutcome, RunnerError> {
    classify_attached_start_result_with_inspector(args, status, stderr, || {
        inspect_terminal_container_outcome(container_name)
    })
}

fn classify_attached_start_result_with_inspector<F>(
    args: Vec<String>,
    status: ExitStatus,
    stderr: String,
    inspect_terminal_outcome: F,
) -> Result<SessionOutcome, RunnerError>
where
    F: FnOnce() -> Option<SessionOutcome>,
{
    if status.code() == Some(PODMAN_INFRASTRUCTURE_ERROR_EXIT_CODE) {
        if let Some(outcome) = inspect_terminal_outcome() {
            return Ok(outcome);
        }

        return Err(RunnerError::PodmanCommandFailed {
            args,
            status,
            stderr,
        });
    }

    Ok(container_status_to_outcome(status))
}

fn container_status_to_outcome(status: ExitStatus) -> SessionOutcome {
    if let Some(signal) = status.signal() {
        return SessionOutcome::TerminatedBySignal {
            exit_code: 128 + signal,
            signal,
        };
    }

    SessionOutcome::from_exit_code(status.code().unwrap_or(1))
}

fn inspect_terminal_container_outcome(container_name: &str) -> Option<SessionOutcome> {
    let output = run_podman_command(vec![
        "inspect".to_string(),
        "--type".to_string(),
        "container".to_string(),
        "--format".to_string(),
        "{{.State.Status}} {{.State.ExitCode}}".to_string(),
        container_name.to_string(),
    ])
    .ok()?;
    let (status, exit_code) = parse_container_state(&output)?;

    if matches!(status, "exited" | "stopped") {
        return Some(exit_code_to_outcome(exit_code));
    }

    None
}

fn parse_container_state(output: &str) -> Option<(&str, i32)> {
    let mut parts = output.split_whitespace();
    let status = parts.next()?;
    let exit_code = parts.next()?.parse().ok()?;
    Some((status, exit_code))
}

fn exit_code_to_outcome(exit_code: i32) -> SessionOutcome {
    SessionOutcome::from_exit_code(exit_code)
}

fn inspect_container_status(container_name: &str) -> Result<String, RunnerError> {
    run_podman_command(vec![
        "inspect".to_string(),
        "--type".to_string(),
        "container".to_string(),
        "--format".to_string(),
        "{{.State.Status}}".to_string(),
        container_name.to_string(),
    ])
    .map(|output| output.trim().to_string())
}

fn inspect_container_status_until(
    container_name: &str,
    deadline: Instant,
) -> Result<Option<String>, RunnerError> {
    run_podman_command_until(
        vec![
            "inspect".to_string(),
            "--type".to_string(),
            "container".to_string(),
            "--format".to_string(),
            "{{.State.Status}}".to_string(),
            container_name.to_string(),
        ],
        deadline,
    )
    .map(|output| output.map(|output| output.trim().to_string()))
}

fn finish_captured_stderr(
    stderr_thread: thread::JoinHandle<std::io::Result<String>>,
) -> Result<String, RunnerError> {
    stderr_thread
        .join()
        .map_err(|panic_payload| {
            let message = match panic_payload.downcast::<String>() {
                Ok(message) => *message,
                Err(panic_payload) => match panic_payload.downcast::<&'static str>() {
                    Ok(message) => (*message).to_string(),
                    Err(_) => "unknown panic".to_string(),
                },
            };

            RunnerError::Io(std::io::Error::other(format!(
                "stderr forwarding thread panicked: {message}"
            )))
        })?
        .map_err(RunnerError::Io)
}

fn forward_and_capture_stderr<T>(mut stderr: T) -> std::io::Result<String>
where
    T: Read,
{
    let host_stderr = std::io::stderr();
    forward_and_capture_stderr_to(&mut stderr, host_stderr)
}

fn forward_and_capture_stderr_to<T, U>(mut stderr: T, mut host_stderr: U) -> std::io::Result<String>
where
    T: Read,
    U: Write,
{
    let mut collected = StderrTailBuffer::new(ATTACHED_STDERR_TAIL_LIMIT);
    let mut buffer = [0_u8; 4096];

    loop {
        let bytes_read = stderr.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }

        let chunk = &buffer[..bytes_read];
        host_stderr.write_all(chunk)?;
        host_stderr.flush()?;
        collected.push(chunk);
    }

    Ok(collected.into_string())
}

// Ring buffer that retains only the last `limit` bytes of stderr output.
// Attached container runs can produce unbounded stderr; this caps memory usage
// while preserving the most recent (and most diagnostic) output for inclusion
// in error messages.
struct StderrTailBuffer {
    bytes: VecDeque<u8>,
    limit: usize,
    truncated: bool,
}

impl StderrTailBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(limit),
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= self.limit {
            self.bytes.clear();
            self.bytes
                .extend(chunk[chunk.len() - self.limit..].iter().copied());
            self.truncated = true;
            return;
        }

        let overflow = self
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(self.limit);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.truncated = true;
        }

        self.bytes.extend(chunk.iter().copied());
    }

    fn into_string(self) -> String {
        let stderr =
            String::from_utf8_lossy(&self.bytes.into_iter().collect::<Vec<_>>()).into_owned();
        if self.truncated {
            return format!("{ATTACHED_STDERR_TRUNCATION_NOTICE}{stderr}");
        }

        stderr
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    let mut quoted = String::from("'");
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\"'\"'");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

fn shell_join(values: &[String]) -> String {
    values
        .iter()
        .map(|value| shell_quote(value))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;
