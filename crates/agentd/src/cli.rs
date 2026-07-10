use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::LiveObservationLevel;

pub const WISH_GREETING: &str = "Speak a wish: the state you want made true.";
pub const WISH_STATEMENT_PROMPT: &str = "What do you wish to be true?";
pub const WISH_TARGET_PROMPT: &str = "What is this wish aimed at? Leave blank if it has no target.";
pub const DEFAULT_CONFIG_PATH: &str = "/etc/agentd/agentd.toml";

const ROOT_ABOUT: &str = "Run the agentd service or submit agent sessions through it.";
const DAEMON_ABOUT: &str = "Run the foreground service that accepts and supervises agent sessions.";
const DAEMON_LONG_ABOUT: &str = "Run the foreground service that accepts manual and scheduled session requests over its control socket and supervises their containerized agent sessions.\n\nStart daemon before using run or wish. Use run or wish to submit work to an already-running daemon.";
const RUN_ABOUT: &str = "Submit one manual session request with explicitly supplied input.";
const RUN_LONG_ABOUT: &str = "Submit one manual session request with explicitly supplied input to the running agentd daemon.\n\nUse run when the session input is already prepared. Use wish to elicit a desired state interactively, or daemon to start the service that accepts requests.";
const WISH_ABOUT: &str =
    "Elicit a desired state and seed one agent session through the running daemon.";
const WISH_LONG_ABOUT: &str = "Interactively elicit the state the operator wants made true and an optional target, or accept an existing tracker work-unit reference, then ask the running daemon to seed one agent session. Prose input is validated as a canonical intent against the active methodology's intent schema.\n\nUse wish for guided desired-state entry. Use run when invocation input is already prepared, or daemon to start the service that accepts requests.";
const AGENT_HELP: &str = "Select an agent declared under this name in the daemon configuration";
const REPO_HELP: &str = "Clone this Git repository for the session; when omitted, use the selected agent's daemon-configured repository";
const SOCKET_PATH_HELP: &str =
    "Send the request through this control socket instead of $XDG_RUNTIME_DIR/agentd/agentd.sock";
const PROGRESS_HELP: &str =
    "Choose how much live transcript activity to print while the session runs";

pub const ROOT_EXAMPLES: &[&[&str]] = &[&["agentd"], &["agentd", "--config", "<PATH>"]];
pub const DAEMON_EXAMPLES: &[&[&str]] = &[
    &["agentd", "daemon"],
    &["agentd", "daemon", "--config", "<PATH>"],
];
pub const RUN_EXAMPLES: &[&[&str]] = &[
    &["agentd", "run", "<AGENT>", "[REPO]"],
    &[
        "agentd",
        "run",
        "<AGENT>",
        "[REPO]",
        "--intent",
        "<STATEMENT>",
    ],
    &[
        "agentd",
        "run",
        "<AGENT>",
        "[REPO]",
        "--work-unit",
        "<REFERENCE>",
    ],
    &[
        "agentd",
        "run",
        "<AGENT>",
        "[REPO]",
        "--artifact-type",
        "<TYPE>",
        "--artifact-file",
        "<ID>.json",
    ],
    &[
        "agentd",
        "run",
        "<AGENT>",
        "[REPO]",
        "--work-unit",
        "<REFERENCE>",
        "--artifact-type",
        "work-unit",
        "--artifact-file",
        "<ID>.json",
    ],
];
pub const WISH_EXAMPLES: &[&[&str]] = &[
    &["agentd", "wish", "<AGENT>", "[REPO]"],
    &[
        "agentd",
        "wish",
        "<AGENT>",
        "[REPO]",
        "--work-unit",
        "<REFERENCE>",
    ],
];

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ProgressLevel {
    /// Print compact live transcript activity.
    Summary,
    /// Print raw live transcript event detail.
    Full,
}

impl From<ProgressLevel> for LiveObservationLevel {
    fn from(level: ProgressLevel) -> Self {
        match level {
            ProgressLevel::Summary => Self::Summary,
            ProgressLevel::Full => Self::Full,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "agentd",
    version,
    propagate_version = true,
    about = ROOT_ABOUT,
    long_about = root_long_about(),
    after_help = examples_after_help(ROOT_EXAMPLES)
)]
pub struct Cli {
    #[arg(long, help = root_config_help())]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Args, Debug, Default)]
pub struct DaemonArgs {
    #[arg(long, help = daemon_config_help())]
    pub config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(
        about = DAEMON_ABOUT,
        long_about = DAEMON_LONG_ABOUT,
        after_help = examples_after_help(DAEMON_EXAMPLES)
    )]
    Daemon(DaemonArgs),
    #[command(
        display_name = "agentd",
        about = RUN_ABOUT,
        long_about = RUN_LONG_ABOUT,
        after_help = run_after_help()
    )]
    Run {
        #[arg(help = AGENT_HELP)]
        agent: String,
        #[arg(help = REPO_HELP)]
        repo: Option<String>,
        #[arg(long, help = SOCKET_PATH_HELP)]
        socket_path: Option<PathBuf>,
        #[arg(
            long,
            value_enum,
            default_value_t = ProgressLevel::Summary,
            help = PROGRESS_HELP
        )]
        progress: ProgressLevel,
        #[arg(
            long,
            conflicts_with = "intent",
            help = "Seed the session from this tracker work-unit reference through runa; conflicts with --intent"
        )]
        work_unit: Option<String>,
        #[arg(
            long,
            conflicts_with_all = ["work_unit", "artifact_file"],
            help = "Synthesize this prose statement into a canonical intent artifact; the active methodology must declare a compatible intent schema"
        )]
        intent: Option<String>,
        #[arg(
            long,
            requires = "artifact_type",
            conflicts_with = "intent",
            help = "Supply this complete JSON artifact document as invocation input; its file stem becomes the artifact ID and --artifact-type is required"
        )]
        artifact_file: Option<PathBuf>,
        #[arg(
            long,
            requires = "artifact_file",
            help = "Declare the active methodology's artifact type for --artifact-file; --artifact-file is required"
        )]
        artifact_type: Option<String>,
    },
    #[command(
        display_name = "agentd",
        about = WISH_ABOUT,
        long_about = WISH_LONG_ABOUT,
        after_help = wish_after_help()
    )]
    Wish {
        #[arg(help = AGENT_HELP)]
        agent: String,
        #[arg(help = REPO_HELP)]
        repo: Option<String>,
        #[arg(long, help = SOCKET_PATH_HELP)]
        socket_path: Option<PathBuf>,
        #[arg(
            long,
            help = "Seed from this existing tracker work-unit reference instead of eliciting prose; runa resolves it before scoped work begins"
        )]
        work_unit: Option<String>,
        #[arg(
            long,
            value_enum,
            default_value_t = ProgressLevel::Summary,
            help = PROGRESS_HELP
        )]
        progress: ProgressLevel,
    },
}

fn root_long_about() -> String {
    format!(
        "Run the agentd service or submit agent sessions through it.\n\nWith no subcommand, agentd starts the foreground daemon using {DEFAULT_CONFIG_PATH}. Use daemon to make service startup explicit, run to submit prepared session input, or wish to elicit a desired state interactively."
    )
}

fn root_config_help() -> String {
    format!(
        "Load daemon configuration from this path when agentd starts in daemon mode [default: {DEFAULT_CONFIG_PATH}]"
    )
}

fn daemon_config_help() -> String {
    format!("Load daemon configuration from this path [default: {DEFAULT_CONFIG_PATH}]")
}

fn examples_after_help(examples: &[&[&str]]) -> String {
    let examples = examples
        .iter()
        .map(|example| format!("  {}", example.join(" ")))
        .collect::<Vec<_>>()
        .join("\n");
    format!("Examples:\n{examples}")
}

fn run_after_help() -> String {
    format!(
        "Live observation:\n  agentd run streams compact followable transcript activity by default while the session executes. Use --progress full for raw transcript event detail.\n\n{}",
        examples_after_help(RUN_EXAMPLES)
    )
}

fn wish_after_help() -> String {
    format!(
        "Live observation:\n  agentd wish streams compact followable transcript activity by default while the session executes. Use --progress full for raw transcript event detail.\n\nPrompts:\n  {WISH_GREETING}\n  {WISH_STATEMENT_PROMPT}\n  {WISH_TARGET_PROMPT}\n\n{}",
        examples_after_help(WISH_EXAMPLES)
    )
}
