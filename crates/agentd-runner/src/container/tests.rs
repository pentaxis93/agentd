use super::*;
use crate::audit::SessionAuditRecord;
use crate::lifecycle::{LifecycleFailureKind, log_lifecycle_failure};
use crate::resources::{PreparedBindMount, SessionResources};
use crate::test_support::{
    CommandBehavior, CommandOutcome, FakePodmanFixture, FakePodmanScenario, InspectBehavior,
    capture_tracing_events, exit_status, fake_podman_lock, test_session_spec,
};
use crate::{BindMount, ResolvedEnvironmentVariable, SessionInvocation};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

const VALID_REMOTE_REPO_URL: &str = "https://example.com/agentd.git";

fn test_audit_record() -> SessionAuditRecord {
    SessionAuditRecord {
        record_dir: PathBuf::from("/tmp/audit/site-builder/0123456789abcdef"),
        runa_dir: PathBuf::from("/tmp/audit/site-builder/0123456789abcdef/runa"),
        metadata_path: PathBuf::from(
            "/tmp/audit/site-builder/0123456789abcdef/agentd/session.json",
        ),
        session_id: "0123456789abcdef".to_string(),
        agent: "site-builder".to_string(),
        repo_url: VALID_REMOTE_REPO_URL.to_string(),
        work_unit: None,
        start_timestamp: "2026-04-16T00:00:00Z".to_string(),
    }
}

#[test]
fn create_container_args_include_shared_relabel_for_methodology_mount() {
    let args = build_create_container_args(
        &SessionResources {
            container_name: "agentd-agent-session".to_string(),
            methodology_staging_dir: PathBuf::from("/tmp/staging"),
            methodology_mount_source: PathBuf::from("/tmp/staging/methodology"),
            audit_record: test_audit_record(),
            audit_mount: PreparedBindMount {
                source: PathBuf::from("/tmp/staging/audit-runa"),
                target: PathBuf::from("/home/site-builder/.agentd/audit/runa"),
                read_only: false,
                relabel_shared: true,
            },
            invocation_input_mount: None,
            additional_mounts: Vec::new(),
            environment_secret_bindings: Vec::new(),
            repo_token_secret_binding: None,
        },
        &test_session_spec(),
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        None,
    );

    let mount_value = argument_value(&args.join(" "), "--mount")
        .expect("podman create should receive a methodology mount");

    assert!(
        mount_value.contains("relabel=shared"),
        "methodology bind mount should include shared SELinux relabeling: {mount_value}"
    );
}

#[test]
fn create_container_args_use_volume_for_shared_relabel_sources_containing_commas() {
    let args = build_create_container_args(
        &SessionResources {
            container_name: "agentd-agent-session".to_string(),
            methodology_staging_dir: PathBuf::from("/tmp/staging"),
            methodology_mount_source: PathBuf::from("/tmp/source,with,commas/methodology"),
            audit_record: test_audit_record(),
            audit_mount: PreparedBindMount {
                source: PathBuf::from("/tmp/audit,with,commas/runa"),
                target: PathBuf::from("/home/site-builder/.agentd/audit/runa"),
                read_only: false,
                relabel_shared: true,
            },
            invocation_input_mount: None,
            additional_mounts: Vec::new(),
            environment_secret_bindings: Vec::new(),
            repo_token_secret_binding: None,
        },
        &test_session_spec(),
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        None,
    );

    let volume_values = argument_values(&args, "--volume");
    assert_eq!(
        volume_values,
        vec![
            "/tmp/source,with,commas/methodology:/agentd/methodology:ro,z".to_string(),
            "/tmp/audit,with,commas/runa:/home/site-builder/.agentd/audit/runa:rw,z".to_string(),
        ]
    );
    assert!(
        argument_values(&args, "--mount").is_empty(),
        "comma-containing shared relabel sources should not use --mount: {args:?}"
    );
}

#[test]
fn create_container_args_force_root_user_and_entrypoint_before_image_argument() {
    let args = build_create_container_args(
        &SessionResources {
            container_name: "agentd-agent-session".to_string(),
            methodology_staging_dir: PathBuf::from("/tmp/staging"),
            methodology_mount_source: PathBuf::from("/tmp/staging/methodology"),
            audit_record: test_audit_record(),
            audit_mount: PreparedBindMount {
                source: PathBuf::from("/tmp/staging/audit-runa"),
                target: PathBuf::from("/home/site-builder/.agentd/audit/runa"),
                read_only: false,
                relabel_shared: true,
            },
            invocation_input_mount: None,
            additional_mounts: Vec::new(),
            environment_secret_bindings: Vec::new(),
            repo_token_secret_binding: None,
        },
        &test_session_spec(),
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        None,
    );

    let user_index = args
        .iter()
        .position(|arg| arg == "--user")
        .expect("podman create should receive --user");
    assert_eq!(args.get(user_index + 1).map(String::as_str), Some("0:0"));
    let entrypoint_index = args
        .iter()
        .position(|arg| arg == "--entrypoint")
        .expect("podman create should receive --entrypoint");
    assert_eq!(
        args.get(entrypoint_index + 1).map(String::as_str),
        Some("/bin/sh")
    );

    let image_index = args
        .iter()
        .position(|arg| arg == "image")
        .expect("podman create should include the base image");
    assert!(user_index < image_index && entrypoint_index < image_index);
}

#[test]
fn create_container_args_map_daemon_identity_to_session_user_id() {
    let args = build_create_container_args(
        &SessionResources {
            container_name: "agentd-agent-session".to_string(),
            methodology_staging_dir: PathBuf::from("/tmp/staging"),
            methodology_mount_source: PathBuf::from("/tmp/staging/methodology"),
            audit_record: test_audit_record(),
            audit_mount: PreparedBindMount {
                source: PathBuf::from("/tmp/staging/audit-runa"),
                target: PathBuf::from("/home/site-builder/.agentd/audit/runa"),
                read_only: false,
                relabel_shared: true,
            },
            invocation_input_mount: None,
            additional_mounts: Vec::new(),
            environment_secret_bindings: Vec::new(),
            repo_token_secret_binding: None,
        },
        &test_session_spec(),
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        None,
    );

    let userns_index = args
        .iter()
        .position(|arg| arg == "--userns")
        .expect("podman create should receive --userns");
    assert_eq!(
        args.get(userns_index + 1).map(String::as_str),
        Some("keep-id:uid=1000,gid=1000")
    );

    let image_index = args
        .iter()
        .position(|arg| arg == "image")
        .expect("podman create should include the base image");
    assert!(userns_index < image_index);
}

#[test]
fn create_container_args_pass_shell_flags_after_image_argument() {
    let spec = test_session_spec();
    let invocation = SessionInvocation {
        repo_url: VALID_REMOTE_REPO_URL.to_string(),
        repo_token: None,
        work_unit: None,
        input: None,
        timeout: None,
    };
    let args = build_create_container_args(
        &SessionResources {
            container_name: "agentd-agent-session".to_string(),
            methodology_staging_dir: PathBuf::from("/tmp/staging"),
            methodology_mount_source: PathBuf::from("/tmp/staging/methodology"),
            audit_record: test_audit_record(),
            audit_mount: PreparedBindMount {
                source: PathBuf::from("/tmp/staging/audit-runa"),
                target: PathBuf::from("/home/site-builder/.agentd/audit/runa"),
                read_only: false,
                relabel_shared: true,
            },
            invocation_input_mount: None,
            additional_mounts: Vec::new(),
            environment_secret_bindings: Vec::new(),
            repo_token_secret_binding: None,
        },
        &spec,
        &invocation,
        None,
    );
    let expected_script = build_container_script(&spec, &invocation, None);

    let image_index = args
        .iter()
        .position(|arg| arg == "image")
        .expect("podman create should include the base image");
    assert_eq!(args.get(image_index + 1).map(String::as_str), Some("-lc"));
    assert_eq!(
        args.get(image_index + 2).map(String::as_str),
        Some(expected_script.as_str())
    );
}

#[test]
fn create_container_args_include_configured_additional_mounts_after_methodology_mount() {
    let args = build_create_container_args(
        &SessionResources {
            container_name: "agentd-agent-session".to_string(),
            methodology_staging_dir: PathBuf::from("/tmp/staging"),
            methodology_mount_source: PathBuf::from("/tmp/staging/methodology"),
            audit_record: test_audit_record(),
            audit_mount: PreparedBindMount {
                source: PathBuf::from("/tmp/staging/audit-runa"),
                target: PathBuf::from("/home/site-builder/.agentd/audit/runa"),
                read_only: false,
                relabel_shared: true,
            },
            invocation_input_mount: None,
            additional_mounts: vec![
                PreparedBindMount {
                    source: PathBuf::from("/tmp/staging/mount-0"),
                    target: PathBuf::from("/home/site-builder/.claude"),
                    read_only: true,
                    relabel_shared: false,
                },
                PreparedBindMount {
                    source: PathBuf::from("/tmp/staging/mount-1"),
                    target: PathBuf::from("/home/site-builder/.runa"),
                    read_only: false,
                    relabel_shared: false,
                },
            ],
            environment_secret_bindings: Vec::new(),
            repo_token_secret_binding: None,
        },
        &test_session_spec(),
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        None,
    );

    let mount_values = argument_values(&args, "--mount");
    assert_eq!(mount_values.len(), 4);
    assert!(mount_values[0].contains("src=/tmp/staging/methodology"));
    assert!(mount_values[1].contains("src=/tmp/staging/audit-runa"));
    assert!(mount_values[1].contains("target=/home/site-builder/.agentd/audit/runa"));
    assert!(mount_values[1].contains("ro=false"));
    assert!(mount_values[1].contains("relabel=shared"));
    assert!(mount_values[2].contains("src=/tmp/staging/mount-0"));
    assert!(mount_values[2].contains("target=/home/site-builder/.claude"));
    assert!(mount_values[2].contains("ro=true"));
    assert!(
        !mount_values[2].contains("relabel="),
        "operator-declared mounts should not mutate host SELinux labels: {}",
        mount_values[2]
    );
    assert!(mount_values[3].contains("src=/tmp/staging/mount-1"));
    assert!(mount_values[3].contains("target=/home/site-builder/.runa"));
    assert!(mount_values[3].contains("ro=false"));
    assert!(
        !mount_values[3].contains("relabel="),
        "operator-declared mounts should not mutate host SELinux labels: {}",
        mount_values[3]
    );
}

#[test]
fn build_container_script_terminates_git_clone_options_before_repo_url() {
    let script = build_container_script(
        &test_session_spec(),
        &SessionInvocation {
            repo_url: "-repo.git".to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        None,
    );

    assert!(script.contains("git clone --no-hardlinks -- '-repo.git' '/home/site-builder/repo'"));
}

#[test]
fn build_container_script_disables_git_terminal_prompts() {
    let script = build_container_script(
        &test_session_spec(),
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        None,
    );

    assert!(script.contains("GIT_TERMINAL_PROMPT=0 git clone --no-hardlinks -- "));
}

#[test]
fn build_container_script_creates_home_workspace_initializes_runa_and_runs_agent_command() {
    let script = build_container_script(
        &crate::SessionSpec {
            agent_name: "myagent".to_string(),
            agent_command: vec![
                "site-builder".to_string(),
                "exec".to_string(),
                "--sandbox".to_string(),
                "workspace-write".to_string(),
            ],
            ..test_session_spec()
        },
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: Some("task-42".to_string()),
            input: None,
            timeout: None,
        },
        None,
    );

    assert!(script.contains(
        "groupadd --gid 1000 'myagent'\n\
         useradd --create-home --home-dir '/home/myagent' --shell /bin/sh --uid 1000 --gid 'myagent' 'myagent'"
    ));
    assert!(script.contains("\nmkdir -p '/home/myagent/.agentd/audit'\n"));
    assert!(script.contains(
        "\nfind '/home/myagent' -mindepth 0 \\( -path '/home/myagent/.agentd/audit/runa' -o -path '/home/myagent/repo' \\) -prune -o -exec chown 'myagent:myagent' {} +\n"
    ));
    assert!(script.contains(
        "git clone --no-hardlinks -- 'https://example.com/agentd.git' '/home/myagent/repo'"
    ));
    assert!(script.contains("\ncd '/home/myagent/repo'\n"));
    assert!(script.contains("\nchown -R 'myagent:myagent' '/home/myagent/repo'\n"));
    assert!(
        script.contains(
            "if [ -e '/home/myagent/repo/.runa' ] || [ -L '/home/myagent/repo/.runa' ]; then"
        ),
        "{script}"
    );
    assert!(
        script.contains("\nln -s '/home/myagent/.agentd/audit/runa' '/home/myagent/repo/.runa'\n")
    );
    assert!(!script.contains("\nchown 'myagent:myagent' '/home/myagent'\n"));
    assert!(!script.contains("\nchown -R 'myagent:myagent' '/home/myagent'\n"));
    assert!(script.contains("\nexport HOME='/home/myagent'\n"));
    assert!(script.contains(
        "\ngosu 'myagent:myagent' runa init --methodology '/agentd/methodology/manifest.toml'\n"
    ));
    assert!(!script.contains(".runa/config.toml"));
    assert!(script.contains(
        "\nexec gosu 'myagent:myagent' runa run --work-unit 'task-42' --agent-command -- 'site-builder' 'exec' '--sandbox' 'workspace-write'"
    ));
}

#[test]
fn build_container_script_uses_mode_based_audit_mount_writability_without_chown() {
    let script = build_container_script(
        &crate::SessionSpec {
            agent_name: "myagent".to_string(),
            ..test_session_spec()
        },
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        None,
    );

    assert!(
        script.contains(
            "\nln -s '/home/myagent/.agentd/audit/runa' '/home/myagent/repo/.runa'\n\
             export HOME='/home/myagent'\n\
             unset AGENTD_WORK_UNIT\n\
             gosu 'myagent:myagent' runa init"
        ),
        "audit mount root should rely on host-side mode setup before runa init: {script}"
    );
    assert!(
        !script.contains("chown 'myagent:myagent' '/home/myagent/.agentd/audit/runa'"),
        "rootless session setup must not chown the host-backed audit mount: {script}"
    );
}

#[test]
fn build_container_script_materializes_invocation_input_without_audit_workspace_chown() {
    let script = build_container_script(
        &crate::SessionSpec {
            agent_name: "myagent".to_string(),
            ..test_session_spec()
        },
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        Some(&ResolvedInvocationInput {
            artifact_type: "request".to_string(),
            artifact_id: "operator-input".to_string(),
            document_json: "{}\n".to_string(),
        }),
    );

    assert!(
        script.contains(
            "\ngosu 'myagent:myagent' mkdir -p '/home/myagent/repo/.runa/workspace/request'\n\
             gosu 'myagent:myagent' cp '/agentd/invocation-input/document.json' '/home/myagent/repo/.runa/workspace/request/operator-input.json'\n"
        ),
        "invocation input should be written through the session user into the audit-backed workspace: {script}"
    );
    assert!(
        !script.contains("chown -R 'myagent:myagent' '/home/myagent/repo/.runa/workspace/request'"),
        "rootless session setup must not chown audit-backed workspace input: {script}"
    );
}

#[test]
fn build_container_script_uses_find_prune_to_reown_home_without_touching_mount_targets() {
    let script = build_container_script(
        &crate::SessionSpec {
            agent_name: "myagent".to_string(),
            mounts: vec![
                crate::BindMount {
                    source: PathBuf::from("/srv/claude"),
                    target: PathBuf::from("/home/myagent/.claude"),
                    read_only: true,
                },
                crate::BindMount {
                    source: PathBuf::from("/srv/claude-config"),
                    target: PathBuf::from("/home/myagent/.config/claude workspace"),
                    read_only: false,
                },
                crate::BindMount {
                    source: PathBuf::from("/srv/git-config"),
                    target: PathBuf::from("/home/myagent/.config/git (session)"),
                    read_only: false,
                },
                crate::BindMount {
                    source: PathBuf::from("/srv/deep-tree"),
                    target: PathBuf::from("/home/myagent/a/b/c"),
                    read_only: false,
                },
                crate::BindMount {
                    source: PathBuf::from("/srv/outside-home"),
                    target: PathBuf::from("/var/lib/shared"),
                    read_only: false,
                },
            ],
            ..test_session_spec()
        },
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        None,
    );

    assert!(
        script.contains(
            "\nfind '/home/myagent' -mindepth 0 \\( -path '/home/myagent/.claude' -o -path '/home/myagent/.config/claude workspace' -o -path '/home/myagent/.config/git (session)' -o -path '/home/myagent/a/b/c' -o -path '/home/myagent/.agentd/audit/runa' -o -path '/home/myagent/repo' \\) -prune -o -exec chown 'myagent:myagent' {} +\n"
        ),
        "home ownership should be repaired with one find traversal that prunes mounts and the repo: {script}"
    );
    assert_eq!(
        script.matches("-path '/home/myagent/repo'").count(),
        1,
        "the repo prune should be emitted exactly once"
    );
    assert!(
        !script.contains("-path '/var/lib/shared'"),
        "mounts outside HOME should not be part of the HOME ownership traversal: {script}"
    );
}

#[test]
fn build_container_script_does_not_emit_intermediate_home_chown_commands() {
    let script = build_container_script(
        &crate::SessionSpec {
            agent_name: "myagent".to_string(),
            mounts: vec![
                crate::BindMount {
                    source: PathBuf::from("/srv/claude"),
                    target: PathBuf::from("/home/myagent/.config/claude"),
                    read_only: true,
                },
                crate::BindMount {
                    source: PathBuf::from("/srv/git-config"),
                    target: PathBuf::from("/home/myagent/.config/git"),
                    read_only: false,
                },
            ],
            ..test_session_spec()
        },
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        None,
    );

    assert!(!script.contains("\nchown 'myagent:myagent' '/home/myagent'\n"));
    assert!(!script.contains("\nchown 'myagent:myagent' '/home/myagent/.config'\n"));
    assert!(!script.contains("\nchown 'myagent:myagent' '/home/myagent/.config/claude'\n"));
    assert!(!script.contains("\nchown 'myagent:myagent' '/home/myagent/.config/git'\n"));
    assert!(
        script.contains("\nfind '/home/myagent' -mindepth 0 "),
        "the find traversal should replace standalone home ownership chown lines: {script}"
    );
}

#[test]
fn build_container_script_unsets_work_unit_when_invocation_omits_it() {
    let script = build_container_script(
        &crate::SessionSpec {
            agent_name: "myagent".to_string(),
            ..test_session_spec()
        },
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        None,
    );

    assert!(script.contains("\nexport HOME='/home/myagent'\n"));
    assert!(script.contains("\nunset AGENTD_WORK_UNIT\n"));
    assert!(script.contains(
        "\nexec gosu 'myagent:myagent' runa run --agent-command -- 'site-builder' 'exec'"
    ));
}

#[test]
fn clone_command_passes_repo_token_to_git_via_environment_not_argv() {
    let root = unique_test_dir("agentd-runner-clone-auth");
    let bin_dir = root.join("bin");
    let log_dir = root.join("logs");
    fs::create_dir_all(&bin_dir).expect("fake git bin dir should be created");
    fs::create_dir_all(&log_dir).expect("fake git log dir should be created");
    install_fake_git(&bin_dir, &log_dir);

    let command = build_clone_command(
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: Some("test-token".to_string()),
            work_unit: None,
            input: None,
            timeout: None,
        },
        "/tmp/repo",
    );
    let original_path = env::var_os("PATH").expect("PATH should exist");
    let path =
        env::join_paths(std::iter::once(bin_dir.clone()).chain(env::split_paths(&original_path)))
            .expect("PATH should be extendable");

    let status = Command::new("sh")
        .args(["-c", &command])
        .env("PATH", path)
        .env(REPO_TOKEN_ENV, "test-token")
        .status()
        .expect("clone command should run");
    assert!(status.success(), "clone command should succeed");

    let git_argv =
        fs::read_to_string(log_dir.join("git-argv.log")).expect("fake git should record argv");
    let git_env =
        fs::read_to_string(log_dir.join("git-env.log")).expect("fake git should record env");

    assert!(git_argv.contains("clone --no-hardlinks --"));
    assert!(git_argv.contains(VALID_REMOTE_REPO_URL));
    assert!(git_argv.contains("/tmp/repo"));
    assert!(!git_argv.contains("test-token"));
    assert!(git_env.contains("GIT_CONFIG_COUNT=1"));
    assert!(git_env.contains("GIT_CONFIG_KEY_0=http.extraHeader"));
    assert!(git_env.contains("GIT_CONFIG_VALUE_0=Authorization: Bearer test-token"));
    assert!(git_env.contains("GIT_TERMINAL_PROMPT=0"));
    assert!(!git_env.contains(&format!("{REPO_TOKEN_ENV}=test-token")));
}

#[test]
fn clone_command_omits_git_auth_environment_when_repo_token_is_absent() {
    let root = unique_test_dir("agentd-runner-clone-public");
    let bin_dir = root.join("bin");
    let log_dir = root.join("logs");
    fs::create_dir_all(&bin_dir).expect("fake git bin dir should be created");
    fs::create_dir_all(&log_dir).expect("fake git log dir should be created");
    install_fake_git(&bin_dir, &log_dir);

    let command = build_clone_command(
        &SessionInvocation {
            repo_url: VALID_REMOTE_REPO_URL.to_string(),
            repo_token: None,
            work_unit: None,
            input: None,
            timeout: None,
        },
        "/tmp/repo",
    );
    let original_path = env::var_os("PATH").expect("PATH should exist");
    let path =
        env::join_paths(std::iter::once(bin_dir.clone()).chain(env::split_paths(&original_path)))
            .expect("PATH should be extendable");

    let status = Command::new("sh")
        .args(["-c", &command])
        .env("PATH", path)
        .status()
        .expect("clone command should run");
    assert!(status.success(), "clone command should succeed");

    let git_argv =
        fs::read_to_string(log_dir.join("git-argv.log")).expect("fake git should record argv");
    let git_env =
        fs::read_to_string(log_dir.join("git-env.log")).expect("fake git should record env");

    assert!(git_argv.contains("clone --no-hardlinks --"));
    assert!(!git_env.contains("GIT_CONFIG_COUNT=1"));
    assert!(!git_env.contains("GIT_CONFIG_KEY_0=http.extraHeader"));
    assert!(!git_env.contains("GIT_CONFIG_VALUE_0=Authorization: Bearer"));
    assert!(git_env.contains("GIT_TERMINAL_PROMPT=0"));
    assert!(!git_env.contains(REPO_TOKEN_ENV));
}

#[test]
fn shell_join_quotes_each_argument_for_direct_exec() {
    let joined = shell_join(&[
        "site-builder".to_string(),
        "exec".to_string(),
        "--prompt".to_string(),
        "hello world".to_string(),
    ]);

    assert_eq!(joined, "'site-builder' 'exec' '--prompt' 'hello world'");
}

#[test]
fn wait_for_container_exit_checks_child_status_again_after_timeout_boundary() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(
        &FakePodmanScenario::new()
            .with_secret_rm(CommandBehavior::from_outcome(
                CommandOutcome::new().append_args_with_prefix("secret-commands.log", "rm"),
            ))
            .with_inspect(
                InspectBehavior::new()
                    .sleep_before(Duration::from_millis(200))
                    .status_fixed("running"),
            ),
    );

    let mut child = Command::new("sh")
        .args(["-c", "sleep 0.02"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("child process should start");

    let started_at = Instant::now();
    let result = fixture.run_with_fake_podman_env(|| {
        wait_for_container_exit(
            &mut child,
            "container",
            "session-123",
            &[SecretBinding {
                secret_name: "secret".to_string(),
                target_name: "GITHUB_TOKEN".to_string(),
            }],
            Some(Duration::from_millis(50)),
        )
    });
    let elapsed = started_at.elapsed();

    let status = result
        .expect("wait should succeed")
        .expect("completed child should win over timeout");
    assert_eq!(status.code(), Some(0));
    assert!(elapsed < Duration::from_millis(150));
    assert_eq!(fixture.secret_commands(), "");
}

#[test]
fn attached_start_classifies_exit_code_125_as_runner_error() {
    let error = classify_attached_start_result_with_inspector(
        vec![
            "start".to_string(),
            "--attach".to_string(),
            "container".to_string(),
        ],
        exit_status(125),
        "podman start failed".to_string(),
        || None,
    )
    .expect_err("podman infrastructure failures should surface as runner errors");

    match error {
        RunnerError::PodmanCommandFailed {
            args,
            status,
            stderr,
        } => {
            assert_eq!(
                args,
                vec![
                    "start".to_string(),
                    "--attach".to_string(),
                    "container".to_string()
                ]
            );
            assert_eq!(status.code(), Some(125));
            assert_eq!(stderr, "podman start failed");
        }
        other => panic!("expected PodmanCommandFailed, got {other:?}"),
    }
}

#[test]
fn attached_start_preserves_exit_code_125_when_inspection_reports_terminal_exit() {
    let outcome = classify_attached_start_result_with_inspector(
        vec![
            "start".to_string(),
            "--attach".to_string(),
            "container".to_string(),
        ],
        exit_status(125),
        String::new(),
        || Some(SessionOutcome::GenericFailure { exit_code: 125 }),
    )
    .expect("inspected terminal exit code should win over podman attach status");

    assert_eq!(outcome, SessionOutcome::GenericFailure { exit_code: 125 });
}

#[test]
fn attached_start_classifies_nonzero_exit_as_session_failure() {
    let outcome = classify_attached_start_result(
        vec![
            "start".to_string(),
            "--attach".to_string(),
            "container".to_string(),
        ],
        "container",
        exit_status(23),
        String::new(),
    )
    .expect("nonzero exit codes should remain session outcomes");

    assert_eq!(outcome, SessionOutcome::GenericFailure { exit_code: 23 });
}

#[test]
fn attached_start_classifies_blocked_exit_as_blocked() {
    let outcome = classify_attached_start_result(
        vec![
            "start".to_string(),
            "--attach".to_string(),
            "container".to_string(),
        ],
        "container",
        exit_status(3),
        String::new(),
    )
    .expect("blocked exits should be preserved as semantic outcomes");

    assert_eq!(outcome, SessionOutcome::Blocked { exit_code: 3 });
}

#[test]
fn attached_start_classifies_signal_exit_as_signal_termination() {
    let outcome = classify_attached_start_result(
        vec![
            "start".to_string(),
            "--attach".to_string(),
            "container".to_string(),
        ],
        "container",
        exit_status(130),
        String::new(),
    )
    .expect("signal-derived exits should be preserved as semantic outcomes");

    assert_eq!(
        outcome,
        SessionOutcome::TerminatedBySignal {
            exit_code: 130,
            signal: 2,
        }
    );
}

#[test]
fn attached_start_classifies_real_process_signals_as_signal_termination() {
    use std::os::unix::process::ExitStatusExt;

    let outcome = classify_attached_start_result(
        vec![
            "start".to_string(),
            "--attach".to_string(),
            "container".to_string(),
        ],
        "container",
        ExitStatusExt::from_raw(2),
        String::new(),
    )
    .expect("real signal terminations should be preserved as semantic outcomes");

    assert_eq!(
        outcome,
        SessionOutcome::TerminatedBySignal {
            exit_code: 130,
            signal: 2,
        }
    );
}

#[test]
fn attached_start_classifies_zero_exit_as_success() {
    let outcome = classify_attached_start_result(
        vec![
            "start".to_string(),
            "--attach".to_string(),
            "container".to_string(),
        ],
        "container",
        exit_status(0),
        String::new(),
    )
    .expect("successful attached starts should remain successful session outcomes");

    assert_eq!(outcome, SessionOutcome::Success { exit_code: 0 });
}

#[test]
fn attached_start_stderr_retains_only_bounded_tail() {
    let payload = "x".repeat((64 * 1024) + 128);
    let mut forwarded = Vec::new();

    let captured =
        forward_and_capture_stderr_to(std::io::Cursor::new(payload.as_bytes()), &mut forwarded)
            .expect("stderr forwarding should succeed");

    let expected_tail = "x".repeat(64 * 1024);
    assert!(captured.starts_with("[stderr truncated to last 65536 bytes]\n"));
    assert!(captured.ends_with(&expected_tail));
    assert_eq!(
        captured.len(),
        "[stderr truncated to last 65536 bytes]\n".len() + expected_tail.len()
    );
    assert_eq!(forwarded, payload.as_bytes());
}

#[test]
fn logs_cleanup_failures_with_cleanup_prefix() {
    let events = capture_tracing_events(|| {
        log_lifecycle_failure(
            LifecycleFailureKind::Cleanup,
            "session execution",
            "agentd-agent-session",
            "session-123",
            &RunnerError::InvalidBaseImage,
        );
    });

    let event = &events[0];
    assert_eq!(event["level"], "WARN");
    assert_eq!(event["fields"]["event"], "runner.lifecycle_failure");
    assert_eq!(event["fields"]["lifecycle_kind"], "cleanup");
    assert_eq!(event["fields"]["stage"], "session execution");
    assert_eq!(event["fields"]["container_name"], "agentd-agent-session");
    assert_eq!(event["fields"]["session_id"], "session-123");
    assert_eq!(event["fields"]["error"], "base_image must not be empty");
}

#[test]
fn logs_attached_start_finalization_failures_with_finalization_prefix() {
    let events = capture_tracing_events(|| {
        log_lifecycle_failure(
            LifecycleFailureKind::AttachedStartFinalization,
            "session execution",
            "agentd-agent-session",
            "session-123",
            &RunnerError::InvalidCommand,
        );
    });

    let event = &events[0];
    assert_eq!(
        event["fields"]["lifecycle_kind"],
        "attached_start_finalization"
    );
    assert_eq!(
        event["fields"]["error"],
        "command must contain at least one argument"
    );
}

#[test]
fn logs_attached_start_kill_failures_with_kill_prefix() {
    let events = capture_tracing_events(|| {
        let error = std::io::Error::other("kill failed");

        log_lifecycle_failure(
            LifecycleFailureKind::AttachedStartKill,
            "session execution",
            "agentd-agent-session",
            "session-123",
            &error,
        );
    });

    let event = &events[0];
    assert_eq!(event["fields"]["lifecycle_kind"], "attached_start_kill");
    assert_eq!(event["fields"]["error"], "kill failed");
}

#[test]
fn fake_podman_scenario_records_create_arguments_for_a_successful_session() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    let scenario = FakePodmanScenario::new();
    fixture.install(&scenario);

    let methodology_dir = fixture.create_methodology_dir("runner-methodology");
    let outcome = fixture.run_with_fake_podman(crate::SessionSpec {
        methodology_dir,
        environment: vec![ResolvedEnvironmentVariable {
            name: "GITHUB_TOKEN".to_string(),
            value: "test-token".to_string(),
        }],
        ..test_session_spec()
    });

    assert_eq!(
        outcome.expect("session should succeed with fake podman"),
        SessionOutcome::Success { exit_code: 0 }
    );
    assert!(fixture.create_args().contains("--name"));
}

#[test]
fn run_session_does_not_pass_resolved_environment_values_via_podman_create_arguments() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(&FakePodmanScenario::new());

    let methodology_dir = fixture.create_methodology_dir("runner-methodology");
    let outcome = fixture.run_with_fake_podman(crate::SessionSpec {
        methodology_dir,
        environment: vec![ResolvedEnvironmentVariable {
            name: "GITHUB_TOKEN".to_string(),
            value: "test-token".to_string(),
        }],
        ..test_session_spec()
    });

    assert_eq!(
        outcome.expect("session should succeed with fake podman"),
        SessionOutcome::Success { exit_code: 0 }
    );

    let create_args = fixture.create_args();
    assert!(!create_args.contains("GITHUB_TOKEN=test-token"));
    assert!(create_args.contains("--secret"));

    let secret_args = fixture.secret_commands();
    assert!(secret_args.contains("create"));
    assert_eq!(fixture.read_log("secret-value.log"), "test-token");
}

#[test]
fn run_session_does_not_pass_repo_token_via_podman_create_arguments() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(&FakePodmanScenario::new());

    let methodology_dir = fixture.create_methodology_dir("runner-methodology");
    let outcome = fixture.run_with_fake_podman_env(|| {
        crate::run_session(
            crate::SessionSpec {
                methodology_dir,
                ..test_session_spec()
            },
            SessionInvocation {
                repo_url: VALID_REMOTE_REPO_URL.to_string(),
                repo_token: Some("repo-secret-token".to_string()),
                work_unit: None,
                input: None,
                timeout: None,
            },
        )
    });

    assert_eq!(
        outcome.expect("session should succeed with repo token"),
        SessionOutcome::Success { exit_code: 0 }
    );

    let create_args = fixture.create_args();
    assert!(!create_args.contains("repo-secret-token"));
    assert!(create_args.contains("--secret"));
    assert!(create_args.contains(&format!("target={REPO_TOKEN_ENV}")));

    let secret_args = fixture.secret_commands();
    assert!(secret_args.contains("create"));
    assert_eq!(fixture.read_log("secret-value.log"), "repo-secret-token");
}

#[test]
fn run_session_injects_empty_environment_values_via_direct_env_args() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(
        &FakePodmanScenario::new().with_secret_create(CommandBehavior::from_outcome(
            CommandOutcome::new()
                .append_args_with_prefix("secret-commands.log", "create")
                .capture_stdin_to("secret-value.log")
                .track_secret_create()
                .reject_empty_stdin("secret data must be larger than 0", 96),
        )),
    );

    let methodology_dir = fixture.create_methodology_dir("runner-methodology");
    let outcome = fixture.run_with_fake_podman(crate::SessionSpec {
        methodology_dir,
        environment: vec![
            ResolvedEnvironmentVariable {
                name: "EMPTY_VALUE".to_string(),
                value: String::new(),
            },
            ResolvedEnvironmentVariable {
                name: "GITHUB_TOKEN".to_string(),
                value: "test-token".to_string(),
            },
        ],
        ..test_session_spec()
    });

    assert_eq!(
        outcome.expect("session should succeed with mixed empty and non-empty environment"),
        SessionOutcome::Success { exit_code: 0 }
    );

    let create_args = fixture.create_args();
    assert!(create_args.contains("--env EMPTY_VALUE="));
    assert!(!create_args.contains("target=EMPTY_VALUE"));
    assert!(!create_args.contains("GITHUB_TOKEN=test-token"));
    assert!(create_args.contains("--secret"));
    assert_eq!(
        fixture
            .secret_commands()
            .lines()
            .filter(|line| line.starts_with("create "))
            .count(),
        1
    );
}

#[test]
fn run_session_reuses_one_session_identifier_for_container_stage_and_secret_names() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(&FakePodmanScenario::new());
    let agent_name = "myagent";

    let methodology_dir = fixture.create_methodology_dir("runner-methodology");
    let additional_mount_source = fixture.create_methodology_dir("runner-additional-mount");
    let outcome = fixture.run_with_fake_podman(crate::SessionSpec {
        agent_name: agent_name.to_string(),
        methodology_dir,
        mounts: vec![BindMount {
            source: additional_mount_source,
            target: PathBuf::from("/home/myagent/.claude"),
            read_only: true,
        }],
        environment: vec![ResolvedEnvironmentVariable {
            name: "GITHUB_TOKEN".to_string(),
            value: "test-token".to_string(),
        }],
        ..test_session_spec()
    });

    assert_eq!(
        outcome.expect("session should succeed with fake podman"),
        SessionOutcome::Success { exit_code: 0 }
    );

    let create_args = fixture.create_args();
    let container_name =
        argument_value(&create_args, "--name").expect("podman create should receive a name");
    let create_arg_parts = create_args
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mount_value = argument_values(&create_arg_parts, "--mount")
        .into_iter()
        .find(|value| value.contains("/mount-0"))
        .expect("podman create should receive a staged additional mount");
    let mount_source = mount_src_value(&mount_value).expect("mount should include src");
    let stage_dir_name = Path::new(&mount_source)
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .expect("mount source should live under the runner staging directory");
    let secret_commands = fixture.secret_commands();
    let secret_name = secret_commands
        .split_whitespace()
        .nth(1)
        .expect("secret create should include a secret name");

    let daemon_instance_id = test_session_spec().daemon_instance_id;
    let container_prefix = format!("agentd-{daemon_instance_id}-{agent_name}-");
    let session_id = container_name
        .strip_prefix(&container_prefix)
        .expect("container name should include daemon and agent prefix");
    let stage_suffix = stage_dir_name
        .strip_prefix("agentd-session-stage-")
        .expect("staging dir should include session stage prefix");

    assert_eq!(stage_suffix, session_id);
    assert_eq!(
        secret_name,
        format!("agentd-{daemon_instance_id}-{session_id}-0")
    );
    assert_eq!(daemon_instance_id.len(), 8);
    assert_eq!(session_id.len(), 16);
}

#[test]
fn run_session_releases_session_secrets_after_container_reaches_running_state() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(
        &FakePodmanScenario::new()
            .with_start(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .set_container_state("running")
                    .wait_for_file(
                        "secret-removed",
                        Duration::from_secs(3),
                        "secret was not removed while container was running",
                        42,
                    ),
            ))
            .with_secret_rm(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .append_args_with_prefix("secret-commands.log", "rm")
                    .touch_file("secret-removed"),
            )),
    );

    let methodology_dir = fixture.create_methodology_dir("runner-methodology");
    let outcome = fixture.run_with_fake_podman(crate::SessionSpec {
        methodology_dir,
        environment: vec![ResolvedEnvironmentVariable {
            name: "GITHUB_TOKEN".to_string(),
            value: "test-token".to_string(),
        }],
        ..test_session_spec()
    });

    assert_eq!(
        outcome.expect("session should succeed with fake podman"),
        SessionOutcome::Success { exit_code: 0 }
    );
    assert!(fixture.secret_commands().contains("create"));
    assert!(fixture.secret_commands().contains("rm"));
}

#[test]
fn run_session_cleans_up_session_secrets_on_bookworm_podman_floor() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(
        &FakePodmanScenario::new().with_secret_rm(CommandBehavior::from_outcome(
            CommandOutcome::new()
                .append_args_with_prefix("secret-commands.log", "rm")
                .reject_args_containing(
                    "--ignore",
                    "Error: unknown flag: --ignore\nSee 'podman secret rm --help'",
                    125,
                )
                .track_secret_remove(),
        )),
    );

    let methodology_dir = fixture.create_methodology_dir("runner-methodology");
    let outcome = fixture.run_with_fake_podman(crate::SessionSpec {
        methodology_dir,
        environment: vec![ResolvedEnvironmentVariable {
            name: "GITHUB_TOKEN".to_string(),
            value: "test-token".to_string(),
        }],
        ..test_session_spec()
    });

    assert_eq!(
        outcome.expect("session should clean up secrets on the supported podman floor"),
        SessionOutcome::Success { exit_code: 0 }
    );
    assert!(!fixture.secret_commands().contains("--ignore"));
    assert_eq!(
        fixture
            .secret_commands()
            .lines()
            .filter(|line| line.starts_with("rm "))
            .count(),
        1
    );
}

#[test]
fn run_session_continues_when_secret_release_fails_after_container_reaches_running_state() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(
        &FakePodmanScenario::new()
            .with_start(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .set_container_state("running")
                    .wait_for_file(
                        "secret-rm-attempted",
                        Duration::from_secs(3),
                        "secret release was not attempted while container was running",
                        42,
                    ),
            ))
            .with_secret_rm(CommandBehavior::sequence(vec![
                CommandOutcome::new()
                    .append_args_with_prefix("secret-commands.log", "rm")
                    .touch_file("secret-rm-attempted")
                    .stderr("secret cleanup failed after container reached running")
                    .exit_code(29),
                CommandOutcome::new()
                    .append_args_with_prefix("secret-commands.log", "rm")
                    .touch_file("secret-removed"),
            ])),
    );

    let methodology_dir = fixture.create_methodology_dir("runner-methodology");
    let outcome = fixture.run_with_fake_podman(crate::SessionSpec {
        methodology_dir,
        environment: vec![ResolvedEnvironmentVariable {
            name: "GITHUB_TOKEN".to_string(),
            value: "test-token".to_string(),
        }],
        ..test_session_spec()
    });

    assert_eq!(
        outcome.expect("session should still succeed when secret release fails"),
        SessionOutcome::Success { exit_code: 0 }
    );
    assert_eq!(
        fixture
            .secret_commands()
            .lines()
            .filter(|line| line.starts_with("rm "))
            .count(),
        2
    );
}

#[test]
fn wait_for_container_exit_returns_timeout_when_secret_release_fails() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(
        &FakePodmanScenario::new()
            .with_secret_ls(CommandBehavior::from_outcome(
                CommandOutcome::new().stdout("secret"),
            ))
            .with_secret_rm(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .append_args_with_prefix("secret-commands.log", "rm")
                    .stderr("secret cleanup failed while attached start was still running")
                    .exit_code(29),
            ))
            .with_inspect(InspectBehavior::new().status_fixed("running")),
    );

    let mut child = Command::new("sh")
        .args(["-c", "sleep 1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("child process should start");

    let started_at = Instant::now();
    let outcome = fixture
        .run_with_fake_podman_env(|| {
            wait_for_container_exit(
                &mut child,
                "container",
                "session-123",
                &[SecretBinding {
                    secret_name: "secret".to_string(),
                    target_name: "GITHUB_TOKEN".to_string(),
                }],
                Some(Duration::from_millis(500)),
            )
        })
        .expect("secret release failure should not surface as a runner error");
    let elapsed = started_at.elapsed();

    assert_eq!(outcome, None);
    assert!(elapsed < Duration::from_millis(900));
    assert_eq!(
        fixture
            .secret_commands()
            .lines()
            .filter(|line| line.starts_with("rm "))
            .count(),
        1
    );

    let _ = child.kill();
    child.wait().expect("child process should be reaped");
}

#[test]
fn wait_for_container_exit_returns_timeout_promptly_when_inspect_stalls_past_deadline() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(
        &FakePodmanScenario::new()
            .with_secret_rm(CommandBehavior::from_outcome(
                CommandOutcome::new().append_args_with_prefix("secret-commands.log", "rm"),
            ))
            .with_inspect(
                InspectBehavior::new()
                    .sleep_before(Duration::from_millis(200))
                    .status_fixed("running"),
            ),
    );

    let mut child = Command::new("sh")
        .args(["-c", "sleep 0.3"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("child process should start");

    let started_at = Instant::now();
    let outcome = fixture
        .run_with_fake_podman_env(|| {
            wait_for_container_exit(
                &mut child,
                "container",
                "session-123",
                &[SecretBinding {
                    secret_name: "secret".to_string(),
                    target_name: "GITHUB_TOKEN".to_string(),
                }],
                Some(Duration::from_millis(50)),
            )
        })
        .expect("inspect timeout should not surface as a runner error");
    let elapsed = started_at.elapsed();

    assert_eq!(outcome, None);
    assert!(elapsed < Duration::from_millis(150));
    assert_eq!(fixture.secret_commands(), "");

    let _ = child.kill();
    child.wait().expect("child process should be reaped");
}

#[test]
fn run_container_to_completion_reaps_attached_child_when_wait_for_container_exit_errors() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(
        &FakePodmanScenario::new()
            .with_start(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .record_pid_to("start.pid")
                    .wait_for_file(
                        "rm-called",
                        Duration::from_secs(3),
                        "timed out waiting for forced cleanup",
                        42,
                    ),
            ))
            .with_rm(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .write_args_to("rm-commands.log")
                    .touch_file("rm-called"),
            ))
            .with_inspect(
                InspectBehavior::new()
                    .fail("inspect failed while attached start was still running", 41),
            ),
    );

    let started_at = Instant::now();
    let error = fixture
        .run_with_fake_podman_env(|| {
            run_container_to_completion(
                "container",
                "session-123",
                &[SecretBinding {
                    secret_name: "secret".to_string(),
                    target_name: "GITHUB_TOKEN".to_string(),
                }],
            )
        })
        .expect_err("inspect failure should surface as a runner error");
    let elapsed = started_at.elapsed();

    match error {
        RunnerError::PodmanCommandFailed { args, status, .. } => {
            assert_eq!(
                args,
                vec![
                    "inspect".to_string(),
                    "--type".to_string(),
                    "container".to_string(),
                    "--format".to_string(),
                    "{{.State.Status}}".to_string(),
                    "container".to_string(),
                ]
            );
            assert_eq!(status.code(), Some(41));
        }
        other => panic!("expected PodmanCommandFailed, got {other:?}"),
    }

    assert!(elapsed < Duration::from_millis(750));
    assert_eq!(
        fixture.read_log("rm-commands.log"),
        "--force --ignore container\n"
    );
    let pid = fixture.start_pid();
    crate::test_support::assert_process_is_reaped(pid);
}

#[test]
fn run_container_with_timeout_returns_timed_out_promptly_when_cleanup_container_fails_after_timeout()
 {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(
        &FakePodmanScenario::new()
            .with_start(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .record_pid_to("start.pid")
                    .exec_sleep(Duration::from_millis(300)),
            ))
            .with_rm(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .write_args_to("rm-commands.log")
                    .stderr("rm failed after timeout")
                    .exit_code(47),
            ))
            .with_inspect(InspectBehavior::new().status_fixed("created")),
    );

    let started_at = Instant::now();
    let outcome = fixture
        .run_with_fake_podman_env(|| {
            run_container_with_timeout("container", "session-123", &[], Duration::from_millis(50))
        })
        .expect("timeout cleanup failure should still return TimedOut promptly");
    let elapsed = started_at.elapsed();

    assert_eq!(outcome, SessionOutcome::TimedOut);
    assert!(elapsed < Duration::from_millis(250));
    assert_eq!(
        fixture.read_log("rm-commands.log"),
        "--force --ignore container\n"
    );
    crate::test_support::assert_process_is_reaped(fixture.start_pid());
}

#[test]
fn run_session_returns_run_error_when_cleanup_after_run_also_fails() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(
        &FakePodmanScenario::new()
            .with_start(CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .set_container_state("running")
                    .stderr("attached start failed")
                    .exit_code(125),
            ))
            .with_secret_rm(CommandBehavior::sequence(vec![
                CommandOutcome::new().append_args_with_prefix("secret-commands.log", "rm"),
                CommandOutcome::new()
                    .append_args_with_prefix("secret-commands.log", "rm")
                    .stderr("secret cleanup failed after run failure")
                    .exit_code(29),
            ]))
            .with_inspect(
                InspectBehavior::new()
                    .status_fixed("running")
                    .status_exit_fixed("running 0"),
            ),
    );

    let methodology_dir = fixture.create_methodology_dir("runner-methodology");
    let error = fixture
        .run_with_fake_podman_env(|| {
            crate::run_session(
                crate::SessionSpec {
                    methodology_dir,
                    environment: vec![ResolvedEnvironmentVariable {
                        name: "GITHUB_TOKEN".to_string(),
                        value: "test-token".to_string(),
                    }],
                    ..test_session_spec()
                },
                SessionInvocation {
                    repo_url: VALID_REMOTE_REPO_URL.to_string(),
                    repo_token: None,
                    work_unit: None,
                    input: None,
                    timeout: None,
                },
            )
        })
        .expect_err("run failure should remain the returned error");

    match error {
        RunnerError::PodmanCommandFailed { args, status, .. } => {
            assert_eq!(args.first().map(String::as_str), Some("start"));
            assert_eq!(status.code(), Some(125));
        }
        other => panic!("expected PodmanCommandFailed, got {other:?}"),
    }
}

#[test]
fn fake_podman_scenario_allows_create_stdout_without_breaking_success() {
    let _guard = fake_podman_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = FakePodmanFixture::new();
    fixture.install(
        &FakePodmanScenario::new().with_create(CommandBehavior::from_outcome(
            CommandOutcome::new()
                .write_args_to("create-args.log")
                .set_container_state("created")
                .stdout("created"),
        )),
    );

    let methodology_dir = fixture.create_methodology_dir("runner-methodology");
    let outcome = fixture.run_with_fake_podman(crate::SessionSpec {
        methodology_dir,
        environment: vec![ResolvedEnvironmentVariable {
            name: "GITHUB_TOKEN".to_string(),
            value: "test-token".to_string(),
        }],
        ..test_session_spec()
    });

    assert_eq!(
        outcome.expect("session should still succeed"),
        SessionOutcome::Success { exit_code: 0 }
    );
}

fn argument_value(command_line: &str, flag: &str) -> Option<String> {
    let mut parts = command_line.split_whitespace();
    while let Some(part) = parts.next() {
        if part == flag {
            return parts.next().map(str::to_string);
        }
    }

    None
}

fn argument_values(args: &[String], flag: &str) -> Vec<String> {
    args.windows(2)
        .filter_map(|window| {
            if window[0] == flag {
                Some(window[1].clone())
            } else {
                None
            }
        })
        .collect()
}

fn mount_src_value(mount: &str) -> Option<String> {
    mount
        .split(',')
        .find_map(|component| component.strip_prefix("src=").map(str::to_string))
}

fn unique_test_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system time should be after the unix epoch")
            .as_nanos()
    ))
}

fn install_fake_git(bin_dir: &Path, log_dir: &Path) {
    let script_path = bin_dir.join("git");
    fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" > {argv}\nenv | sort > {env}\nexit 0\n",
            argv = shell_quote(&log_dir.join("git-argv.log").display().to_string()),
            env = shell_quote(&log_dir.join("git-env.log").display().to_string()),
        ),
    )
    .expect("fake git script should be written");

    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(&script_path)
        .expect("fake git script metadata should be available")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("fake git script should be executable");
}
