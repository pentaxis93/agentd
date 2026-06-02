use crate::types::{RunnerError, SessionInvocation, SessionOutcome, SessionSpec};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tracing_subscriber::fmt::MakeWriter;

const VALID_REMOTE_REPO_URL: &str = "https://example.com/agentd.git";

pub(crate) fn test_session_spec() -> SessionSpec {
    SessionSpec {
        daemon_instance_id: "1a2b3c4d".to_string(),
        agent_name: "site-builder".to_string(),
        base_image: "image".to_string(),
        methodology_dir: PathBuf::from("/tmp/methodology"),
        audit_root: std::env::temp_dir().join("agentd-runner-test-audit-root"),
        forge: "github".to_string(),
        mounts: Vec::new(),
        agent_command: vec!["site-builder".to_string(), "exec".to_string()],
        environment: Vec::new(),
    }
}

pub(crate) fn fake_podman_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) fn capture_tracing_events(run: impl FnOnce()) -> Vec<Value> {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .without_time()
        .with_writer(SharedBuffer::new(buffer.clone()))
        .finish();

    tracing::subscriber::with_default(subscriber, run);

    let output = String::from_utf8(
        buffer
            .lock()
            .expect("trace buffer should be lockable")
            .clone(),
    )
    .expect("trace output should be valid UTF-8");

    output
        .lines()
        .map(|line| serde_json::from_str(line).expect("trace line should be valid JSON"))
        .collect()
}

pub(crate) fn fake_podman_ps_json(records: &[(&[&str], &str, &str)]) -> String {
    serde_json::to_string(
        &records
            .iter()
            .map(|(names, state, status)| {
                json!({
                    "Names": names,
                    "State": state,
                    "Status": status,
                })
            })
            .collect::<Vec<_>>(),
    )
    .expect("fake podman ps payload should serialize")
}

#[derive(Clone)]
struct SharedBuffer {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl SharedBuffer {
    fn new(inner: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { inner }
    }
}

impl<'a> MakeWriter<'a> for SharedBuffer {
    type Writer = SharedBufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedBufferWriter {
            inner: self.inner.clone(),
        }
    }
}

struct SharedBufferWriter {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl Write for SharedBufferWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner
            .lock()
            .expect("trace buffer should be lockable")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FakePodmanScenario {
    create: CommandBehavior,
    ps: CommandBehavior,
    start: CommandBehavior,
    remove: CommandBehavior,
    secret_create: CommandBehavior,
    secret_list: CommandBehavior,
    secret_remove: CommandBehavior,
    inspect: InspectBehavior,
}

impl FakePodmanScenario {
    pub(crate) fn new() -> Self {
        Self {
            create: CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .write_args_to("create-args.log")
                    .set_container_state("created"),
            ),
            ps: CommandBehavior::from_outcome(CommandOutcome::new()),
            start: CommandBehavior::from_outcome(
                CommandOutcome::new().set_container_state("running"),
            ),
            remove: CommandBehavior::from_outcome(CommandOutcome::new()),
            secret_create: CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .append_args_with_prefix("secret-commands.log", "create")
                    .capture_stdin_to("secret-value.log")
                    .track_secret_create(),
            ),
            secret_list: CommandBehavior::from_outcome(
                CommandOutcome::new().list_tracked_secrets(),
            ),
            secret_remove: CommandBehavior::from_outcome(
                CommandOutcome::new()
                    .append_args_with_prefix("secret-commands.log", "rm")
                    .track_secret_remove(),
            ),
            inspect: InspectBehavior::new(),
        }
    }

    pub(crate) fn with_create(mut self, behavior: CommandBehavior) -> Self {
        self.create = behavior;
        self
    }

    pub(crate) fn with_ps(mut self, behavior: CommandBehavior) -> Self {
        self.ps = behavior;
        self
    }

    pub(crate) fn with_start(mut self, behavior: CommandBehavior) -> Self {
        self.start = behavior;
        self
    }

    pub(crate) fn with_rm(mut self, behavior: CommandBehavior) -> Self {
        self.remove = behavior;
        self
    }

    pub(crate) fn with_secret_create(mut self, behavior: CommandBehavior) -> Self {
        self.secret_create = behavior;
        self
    }

    pub(crate) fn with_secret_ls(mut self, behavior: CommandBehavior) -> Self {
        self.secret_list = behavior;
        self
    }

    pub(crate) fn with_secret_rm(mut self, behavior: CommandBehavior) -> Self {
        self.secret_remove = behavior;
        self
    }

    pub(crate) fn with_inspect(mut self, behavior: InspectBehavior) -> Self {
        self.inspect = behavior;
        self
    }

    fn render_script(&self) -> String {
        let mut script = String::from(
            "#!/bin/sh\n\
             set -eu\n\
             \n\
             log_root=\"${AGENTD_FAKE_PODMAN_LOG_DIR:?}\"\n\
             command_name=\"$1\"\n\
             shift\n\
             \n\
             next_count() {\n\
                 key=\"$1\"\n\
                 count_file=\"$log_root/$key.count\"\n\
                 count=0\n\
                 if [ -f \"$count_file\" ]; then\n\
                     count=\"$(cat \"$count_file\")\"\n\
                 fi\n\
                 count=$((count + 1))\n\
                 printf '%s' \"$count\" > \"$count_file\"\n\
                 printf '%s' \"$count\"\n\
             }\n\
             \n\
             read_state() {\n\
                 cat \"$log_root/container-state\" 2>/dev/null || printf 'created'\n\
             }\n\
             \n\
             case \"$command_name\" in\n",
        );

        script.push_str(&render_command_branch("create", &self.create));
        script.push_str(&render_command_branch("ps", &self.ps));
        script.push_str(
            "    secret)\n\
                 subcommand=\"$1\"\n\
                 shift\n\
                 case \"$subcommand\" in\n",
        );
        script.push_str(&render_command_branch("secret-create", &self.secret_create));
        script.push_str(&render_command_branch("secret-ls", &self.secret_list));
        script.push_str(&render_command_branch("secret-rm", &self.secret_remove));
        script.push_str(
            "            *)\n\
                 echo \"unexpected podman secret subcommand: $subcommand\" >&2\n\
                 exit 98\n\
                 ;;\n\
                 esac\n\
                 ;;\n",
        );
        script.push_str(&render_command_branch("start", &self.start));
        script.push_str(&render_command_branch("rm", &self.remove));
        script.push_str(&render_inspect_branch(&self.inspect));
        script.push_str(
            "    *)\n\
                 echo \"unexpected podman command: $command_name\" >&2\n\
                 exit 99\n\
                 ;;\n\
             esac\n",
        );

        script
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CommandBehavior {
    outcomes: Vec<CommandOutcome>,
}

impl CommandBehavior {
    pub(crate) fn from_outcome(outcome: CommandOutcome) -> Self {
        Self {
            outcomes: vec![outcome],
        }
    }

    pub(crate) fn sequence(outcomes: impl Into<Vec<CommandOutcome>>) -> Self {
        let outcomes = outcomes.into();
        assert!(
            !outcomes.is_empty(),
            "command behavior must have at least one outcome"
        );
        Self { outcomes }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CommandOutcome {
    write_args_file: Option<String>,
    append_args: Option<(String, String)>,
    stdin_file: Option<String>,
    reject_empty_stdin: Option<(String, i32)>,
    reject_args_containing: Vec<(String, String, i32)>,
    track_secret_create: bool,
    track_secret_remove: bool,
    list_tracked_secrets: bool,
    container_state: Option<String>,
    exec_sleep_ms: Option<u64>,
    record_pid_file: Option<String>,
    touch_files: Vec<String>,
    wait_for_file: Option<WaitForFile>,
    stdout: Option<String>,
    stderr: Option<String>,
    exit_code: i32,
}

impl CommandOutcome {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn write_args_to(mut self, file: &str) -> Self {
        self.write_args_file = Some(file.to_string());
        self
    }

    pub(crate) fn append_args_with_prefix(mut self, file: &str, prefix: &str) -> Self {
        self.append_args = Some((file.to_string(), prefix.to_string()));
        self
    }

    pub(crate) fn capture_stdin_to(mut self, file: &str) -> Self {
        self.stdin_file = Some(file.to_string());
        self
    }

    pub(crate) fn reject_empty_stdin(mut self, message: &str, exit_code: i32) -> Self {
        self.reject_empty_stdin = Some((message.to_string(), exit_code));
        self
    }

    pub(crate) fn reject_args_containing(
        mut self,
        needle: &str,
        message: &str,
        exit_code: i32,
    ) -> Self {
        self.reject_args_containing
            .push((needle.to_string(), message.to_string(), exit_code));
        self
    }

    pub(crate) fn track_secret_create(mut self) -> Self {
        self.track_secret_create = true;
        self
    }

    pub(crate) fn track_secret_remove(mut self) -> Self {
        self.track_secret_remove = true;
        self
    }

    pub(crate) fn list_tracked_secrets(mut self) -> Self {
        self.list_tracked_secrets = true;
        self
    }

    pub(crate) fn set_container_state(mut self, state: &str) -> Self {
        self.container_state = Some(state.to_string());
        self
    }

    pub(crate) fn exec_sleep(mut self, duration: Duration) -> Self {
        self.exec_sleep_ms = Some(duration.as_millis() as u64);
        self
    }

    pub(crate) fn record_pid_to(mut self, file: &str) -> Self {
        self.record_pid_file = Some(file.to_string());
        self
    }

    pub(crate) fn touch_file(mut self, file: &str) -> Self {
        self.touch_files.push(file.to_string());
        self
    }

    pub(crate) fn wait_for_file(
        mut self,
        file: &str,
        duration: Duration,
        timeout_message: &str,
        timeout_exit_code: i32,
    ) -> Self {
        self.wait_for_file = Some(WaitForFile {
            file: file.to_string(),
            timeout_secs: duration.as_secs(),
            timeout_message: timeout_message.to_string(),
            timeout_exit_code,
        });
        self
    }

    pub(crate) fn stdout(mut self, value: &str) -> Self {
        self.stdout = Some(value.to_string());
        self
    }

    pub(crate) fn stderr(mut self, value: &str) -> Self {
        self.stderr = Some(value.to_string());
        self
    }

    pub(crate) fn exit_code(mut self, code: i32) -> Self {
        self.exit_code = code;
        self
    }
}

#[derive(Clone, Debug)]
struct WaitForFile {
    file: String,
    timeout_secs: u64,
    timeout_message: String,
    timeout_exit_code: i32,
}

#[derive(Clone, Debug)]
pub(crate) struct InspectBehavior {
    sleep_before_ms: Option<u64>,
    failure: Option<(String, i32)>,
    status_output: InspectOutput,
    status_exit_output: InspectOutput,
}

impl InspectBehavior {
    pub(crate) fn new() -> Self {
        Self {
            sleep_before_ms: None,
            failure: None,
            status_output: InspectOutput::FromState,
            status_exit_output: InspectOutput::FromStateWithExit(0),
        }
    }

    pub(crate) fn sleep_before(mut self, duration: Duration) -> Self {
        self.sleep_before_ms = Some(duration.as_millis() as u64);
        self
    }

    pub(crate) fn fail(mut self, message: &str, exit_code: i32) -> Self {
        self.failure = Some((message.to_string(), exit_code));
        self
    }

    pub(crate) fn status_fixed(mut self, value: &str) -> Self {
        self.status_output = InspectOutput::Fixed(value.to_string());
        self
    }

    pub(crate) fn status_exit_fixed(mut self, value: &str) -> Self {
        self.status_exit_output = InspectOutput::Fixed(value.to_string());
        self
    }
}

#[derive(Clone, Debug)]
enum InspectOutput {
    Fixed(String),
    FromState,
    FromStateWithExit(i32),
}

pub(crate) struct FakePodmanFixture {
    root: PathBuf,
    log_dir: PathBuf,
    bin_dir: PathBuf,
}

impl FakePodmanFixture {
    pub(crate) fn new() -> Self {
        let root = unique_temp_dir("agentd-runner-fake-podman");
        let log_dir = root.join("logs");
        let bin_dir = root.join("bin");
        fs::create_dir_all(&log_dir).expect("log dir should be created");
        fs::create_dir_all(&bin_dir).expect("bin dir should be created");

        Self {
            root,
            log_dir,
            bin_dir,
        }
    }

    pub(crate) fn install(&self, scenario: &FakePodmanScenario) {
        let script_path = self.bin_dir.join("podman");
        fs::write(&script_path, scenario.render_script())
            .expect("fake podman script should be written");

        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&script_path)
            .expect("fake podman script metadata should be available")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions)
            .expect("fake podman script should be executable");
    }

    pub(crate) fn create_methodology_dir(&self, name: &str) -> PathBuf {
        let methodology_dir = self.root.join(name);
        fs::create_dir_all(&methodology_dir).expect("methodology dir should be created");
        fs::write(methodology_dir.join("manifest.toml"), "name = \"test\"\n")
            .expect("methodology manifest should be written");
        methodology_dir
    }

    pub(crate) fn run_with_fake_podman(
        &self,
        spec: SessionSpec,
    ) -> Result<SessionOutcome, RunnerError> {
        self.run_with_fake_podman_env(|| {
            crate::run_session(
                spec,
                SessionInvocation {
                    repo_url: VALID_REMOTE_REPO_URL.to_string(),
                    repo_token: None,
                    work_unit: None,
                    input: None,
                    timeout: None,
                },
            )
        })
    }

    pub(crate) fn run_with_fake_podman_env<T>(&self, run: impl FnOnce() -> T) -> T {
        let original_path = env::var_os("PATH").expect("PATH should exist for fake podman tests");
        let fake_path = env::join_paths(
            std::iter::once(self.bin_dir.clone()).chain(env::split_paths(&original_path)),
        )
        .expect("fake PATH should be constructed");

        unsafe {
            env::set_var("PATH", &fake_path);
            env::set_var("AGENTD_FAKE_PODMAN_LOG_DIR", &self.log_dir);
        }

        let result = run();

        unsafe {
            env::set_var("PATH", original_path);
            env::remove_var("AGENTD_FAKE_PODMAN_LOG_DIR");
        }

        result
    }

    pub(crate) fn read_log(&self, name: &str) -> String {
        fs::read_to_string(self.log_dir.join(name)).unwrap_or_default()
    }

    pub(crate) fn create_args(&self) -> String {
        self.read_log("create-args.log")
    }

    pub(crate) fn secret_commands(&self) -> String {
        self.read_log("secret-commands.log")
    }

    pub(crate) fn start_pid(&self) -> u32 {
        self.start_pid_from("start.pid")
    }

    pub(crate) fn start_pid_from(&self, file: &str) -> u32 {
        self.read_log(file)
            .trim()
            .parse::<u32>()
            .expect("fake podman script should record its pid")
    }
}

impl Drop for FakePodmanFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn render_command_branch(name: &str, behavior: &CommandBehavior) -> String {
    let mut branch = String::new();
    let match_name = name
        .rsplit_once('-')
        .map(|(_, suffix)| suffix)
        .unwrap_or(name);
    branch.push_str(&format!("    {match_name})\n"));
    branch.push_str(&format!(
        "        count=\"$(next_count {})\"\n",
        sh_quote(name)
    ));
    if behavior.outcomes.len() == 1 {
        branch.push_str(&render_command_outcome(&behavior.outcomes[0]));
    } else {
        branch.push_str("        case \"$count\" in\n");
        for (index, outcome) in behavior.outcomes.iter().enumerate() {
            let selector = index + 1;
            branch.push_str(&format!("            {selector})\n"));
            for line in render_command_outcome(outcome).lines() {
                branch.push_str("                ");
                branch.push_str(line);
                branch.push('\n');
            }
            branch.push_str("                ;;\n");
        }
        branch.push_str("            *)\n");
        for line in render_command_outcome(
            behavior
                .outcomes
                .last()
                .expect("multi-outcome behavior should have a last outcome"),
        )
        .lines()
        {
            branch.push_str("                ");
            branch.push_str(line);
            branch.push('\n');
        }
        branch.push_str("                ;;\n");
        branch.push_str("        esac\n");
    }
    branch.push_str("        ;;\n");
    branch
}

fn render_command_outcome(outcome: &CommandOutcome) -> String {
    let mut body = String::new();

    if let Some(file) = &outcome.record_pid_file {
        body.push_str(&format!("printf '%s\\n' \"$$\" > \"$log_root/{}\"\n", file));
    }
    if let Some(file) = &outcome.write_args_file {
        body.push_str(&format!("printf '%s\\n' \"$*\" > \"$log_root/{}\"\n", file));
    }
    if let Some((file, prefix)) = &outcome.append_args {
        body.push_str(&format!(
            "printf '{} %s\\n' \"$*\" >> \"$log_root/{}\"\n",
            prefix, file
        ));
    }
    if let Some(file) = &outcome.stdin_file {
        body.push_str(&format!("cat > \"$log_root/{}\"\n", file));
        if let Some((message, exit_code)) = &outcome.reject_empty_stdin {
            body.push_str(&format!(
                "if [ ! -s \"$log_root/{}\" ]; then\n    echo {} >&2\n    exit {}\nfi\n",
                file,
                sh_quote(message),
                exit_code
            ));
        }
    }
    for (needle, message, exit_code) in &outcome.reject_args_containing {
        body.push_str(&format!(
            "for arg in \"$@\"; do\n    if [ \"$arg\" = {} ]; then\n        echo {} >&2\n        exit {}\n    fi\ndone\n",
            sh_quote(needle),
            sh_quote(message),
            exit_code
        ));
    }
    if outcome.track_secret_create {
        body.push_str("printf '%s\\n' \"$1\" >> \"$log_root/secrets.log\"\n");
    }
    if outcome.track_secret_remove {
        body.push_str(
            "for secret_name in \"$@\"; do\n    if [ -f \"$log_root/secrets.log\" ]; then\n        grep -Fvx -- \"$secret_name\" \"$log_root/secrets.log\" > \"$log_root/secrets.next\" || true\n        mv \"$log_root/secrets.next\" \"$log_root/secrets.log\"\n    fi\ndone\n",
        );
    }
    if outcome.list_tracked_secrets {
        body.push_str("cat \"$log_root/secrets.log\" 2>/dev/null || true\n");
    }
    if let Some(state) = &outcome.container_state {
        body.push_str(&format!(
            "printf '{}' > \"$log_root/container-state\"\n",
            state
        ));
    }
    for file in &outcome.touch_files {
        body.push_str(&format!(": > \"$log_root/{}\"\n", file));
    }
    if let Some(wait) = &outcome.wait_for_file {
        body.push_str(&format!(
            "deadline=$(( $(date +%s) + {} ))\nwhile [ ! -f \"$log_root/{}\" ]; do\n    if [ \"$(date +%s)\" -ge \"$deadline\" ]; then\n        echo {} >&2\n        exit {}\n    fi\n    sleep 0.05\ndone\n",
            wait.timeout_secs,
            wait.file,
            sh_quote(&wait.timeout_message),
            wait.timeout_exit_code
        ));
    }
    if let Some(stderr) = &outcome.stderr {
        body.push_str(&format!("echo {} >&2\n", sh_quote(stderr)));
    }
    if let Some(stdout) = &outcome.stdout {
        body.push_str(&format!("printf '{}\\n'\n", stdout));
    }
    if let Some(ms) = outcome.exec_sleep_ms {
        body.push_str(&format!("exec sleep {}\n", seconds_literal(ms)));
    } else {
        body.push_str(&format!("exit {}\n", outcome.exit_code));
    }

    body
}

fn render_inspect_branch(behavior: &InspectBehavior) -> String {
    let mut branch = String::from(
        "    inspect)\n\
             format_value=\"\"\n\
             while [ \"$#\" -gt 0 ]; do\n\
                 case \"$1\" in\n\
                     --type)\n\
                         shift 2\n\
                         ;;\n\
                     --format)\n\
                         format_value=\"$2\"\n\
                         shift 2\n\
                         ;;\n\
                     *)\n\
                         shift\n\
                         ;;\n\
                 esac\n\
             done\n",
    );
    if let Some(ms) = behavior.sleep_before_ms {
        branch.push_str(&format!("        sleep {}\n", seconds_literal(ms)));
    }
    if let Some((message, exit_code)) = &behavior.failure {
        branch.push_str(&format!(
            "        echo {} >&2\n        exit {}\n",
            sh_quote(message),
            exit_code
        ));
    } else {
        branch.push_str("        state=\"$(read_state)\"\n");
        branch.push_str(
            "        case \"$format_value\" in\n\
                 \"{{.State.Status}}\")\n",
        );
        branch.push_str(&format!(
            "            {}\n            ;;\n",
            render_inspect_output(&behavior.status_output)
        ));
        branch.push_str("        \"{{.State.Status}} {{.State.ExitCode}}\")\n");
        branch.push_str(&format!(
            "            {}\n            ;;\n",
            render_inspect_output(&behavior.status_exit_output)
        ));
        branch.push_str(
            "        *)\n\
                 echo \"unexpected podman inspect format: $format_value\" >&2\n\
                 exit 97\n\
                 ;;\n\
                 esac\n",
        );
    }
    branch.push_str("        ;;\n");
    branch
}

fn render_inspect_output(output: &InspectOutput) -> String {
    match output {
        InspectOutput::Fixed(value) => format!("printf '{}\\n'", value),
        InspectOutput::FromState => "printf '%s\\n' \"$state\"".to_string(),
        InspectOutput::FromStateWithExit(code) => {
            format!("printf '%s {}\\n' \"$state\"", code)
        }
    }
}

fn seconds_literal(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    let remainder = milliseconds % 1000;
    if remainder == 0 {
        seconds.to_string()
    } else {
        format!("{seconds}.{remainder:03}")
    }
}

fn sh_quote(value: &str) -> String {
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

pub(crate) fn unique_temp_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after the unix epoch")
            .as_nanos()
    ))
}

pub(crate) fn exit_status(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    ExitStatusExt::from_raw(code << 8)
}
pub(crate) fn assert_process_is_reaped(pid: u32) {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps should run");

    if !output.status.success() {
        return;
    }

    let status = String::from_utf8(output.stdout).expect("ps output should be utf-8");
    assert!(
        !status.trim().starts_with('Z'),
        "expected process {pid} to be reaped, got state {:?}",
        status.trim()
    );
}
