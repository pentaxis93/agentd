use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

pub(super) const DEFAULT_CONFIG_PATH: &str = "/etc/agentd/agentd.toml";
pub(super) const WISH_GREETING: &str = "Speak a wish: the state you want made true.";
pub(super) const WISH_STATEMENT_PROMPT: &str = "What do you wish to be true?";
pub(super) const WISH_TARGET_PROMPT: &str =
    "What is this wish aimed at? Leave blank if it has no target.";

const ROOT_ABOUT: &str = "Run the agentd service or submit agent sessions through it.";
const DAEMON_ABOUT: &str =
    "Start the foreground service that accepts and supervises agent sessions";
const DAEMON_LONG_ABOUT: &str = "Run the foreground agentd service that accepts manual and scheduled session requests over its control socket and supervises their execution.\n\nUse daemon to start the service. Use run for direct manual submission or wish for guided desired-state entry after the service is running.";
const RUN_ABOUT: &str = "Submit one manual session request, with optional invocation input";
const RUN_LONG_ABOUT: &str = "Submit one manual session request to an already-running agentd daemon.\n\nInvocation input may be omitted for an unseeded session or supplied as a work-unit reference, a prose intent statement, a complete artifact document, or a matching work-unit reference and work-unit artifact. Use run for direct manual submission; use wish for guided desired-state entry or daemon to start the service.";
const WISH_ABOUT: &str =
    "Elicit a desired state or select a work unit, then seed one agent session";
const WISH_LONG_ABOUT: &str = "Interactively elicit the state the operator wants made true and an optional target, validate the resulting canonical intent for the active methodology, and ask the running daemon to seed one agent session. With --work-unit, bypass prose elicitation and seed from the named tracker work unit.\n\nUse wish for guided desired-state entry. Use run for direct manual submission or daemon to start the service.";
const AGENT_HELP: &str = "Select an agent declared under this name in the daemon configuration";
const REPO_HELP: &str = "Use the selected agent's daemon-configured repository when omitted; otherwise clone a remote https://, http://, git://, ssh://, or user@host:path URL; local paths, credential-bearing URLs, queries, and fragments are rejected";
const SOCKET_PATH_HELP: &str = "Connect through this daemon control socket instead of resolving $XDG_RUNTIME_DIR/agentd/agentd.sock";
const PROGRESS_HELP: &str =
    "Choose how much live transcript activity to print while the session runs";

const ROOT_EXAMPLES: &[&[&str]] = &[&["agentd"], &["agentd", "--config", "<PATH>"]];
const DAEMON_EXAMPLES: &[&[&str]] = &[
    &["agentd", "daemon"],
    &["agentd", "daemon", "--config", "<PATH>"],
];
const RUN_EXAMPLES: &[&[&str]] = &[
    &["agentd", "run", "<AGENT>"],
    &["agentd", "run", "<AGENT>", "<REPO>"],
    &["agentd", "run", "<AGENT>", "--intent", "<STATEMENT>"],
    &["agentd", "run", "<AGENT>", "--work-unit", "<REFERENCE>"],
    &[
        "agentd",
        "run",
        "<AGENT>",
        "--artifact-type",
        "<TYPE>",
        "--artifact-file",
        "<ID>.json",
    ],
    &[
        "agentd",
        "run",
        "<AGENT>",
        "--work-unit",
        "<REFERENCE>",
        "--artifact-type",
        "work-unit",
        "--artifact-file",
        "<REFERENCE>.json",
    ],
];
const WISH_EXAMPLES: &[&[&str]] = &[
    &["agentd", "wish", "<AGENT>"],
    &["agentd", "wish", "<AGENT>", "<REPO>"],
    &["agentd", "wish", "<AGENT>", "--work-unit", "<REFERENCE>"],
];

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum ProgressLevel {
    /// Print compact live transcript activity.
    Summary,
    /// Print raw live transcript event detail.
    Full,
}

impl From<ProgressLevel> for agentd::LiveObservationLevel {
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
pub(super) struct Cli {
    #[arg(long, help = root_config_help())]
    pub(super) config: Option<PathBuf>,
    #[command(subcommand)]
    pub(super) command: Option<Command>,
}

#[derive(Args, Debug, Default)]
pub(super) struct DaemonArgs {
    #[arg(long, help = daemon_config_help())]
    pub(super) config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub(super) enum Command {
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
            help = "Seed from this tracker work-unit reference through runa; when combined with an artifact, its type must be work-unit and its file stem must equal this reference; conflicts with --intent"
        )]
        work_unit: Option<String>,
        #[arg(
            long,
            conflicts_with_all = ["work_unit", "artifact_file"],
            help = "Synthesize this prose statement into a canonical intent artifact; the active methodology must declare a compatible intent schema; conflicts with --work-unit and --artifact-file"
        )]
        intent: Option<String>,
        #[arg(
            long,
            requires = "artifact_type",
            conflicts_with = "intent",
            help = "Supply this complete UTF-8 JSON artifact document; its file stem becomes the artifact id and --artifact-type is required"
        )]
        artifact_file: Option<PathBuf>,
        #[arg(
            long,
            requires = "artifact_file",
            help = "Declare the active methodology's artifact type for --artifact-file; use work-unit when combining with --work-unit"
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
            help = "Bypass prose elicitation and seed from this existing tracker work-unit reference through runa"
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
        "Run the agentd service or submit agent sessions through it.\n\nWith no subcommand, agentd starts the foreground daemon using {DEFAULT_CONFIG_PATH}. Use daemon to make service startup explicit, run to submit one manual session request, or wish for guided desired-state entry."
    )
}

fn root_config_help() -> String {
    format!(
        "Load daemon configuration from this path when starting the service [default: {DEFAULT_CONFIG_PATH}]"
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    fn assert_complete_help(command: &clap::Command) {
        let about = command
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(
            !about.trim().is_empty(),
            "{} must state what it does and when to use it",
            command.get_name()
        );

        for argument in command.get_arguments() {
            let help = argument
                .get_help()
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                !help.trim().is_empty(),
                "{} input {} must state its purpose",
                command.get_name(),
                argument.get_id()
            );
        }

        for subcommand in command.get_subcommands() {
            assert_complete_help(subcommand);
        }
    }

    #[test]
    fn command_help_structurally_explains_every_surface() {
        assert_complete_help(&Cli::command());
    }

    #[test]
    fn canonical_help_examples_parse_and_preserve_work_mode_identity() {
        for example in [ROOT_EXAMPLES, DAEMON_EXAMPLES, RUN_EXAMPLES, WISH_EXAMPLES]
            .into_iter()
            .flatten()
        {
            Cli::try_parse_from(*example)
                .unwrap_or_else(|error| panic!("example {example:?} must parse: {error}"));
        }

        let combined = Cli::try_parse_from(
            *RUN_EXAMPLES
                .last()
                .expect("run examples should include combined work mode"),
        )
        .expect("combined work-mode example should parse");
        let Cli {
            command:
                Some(Command::Run {
                    work_unit: Some(work_unit),
                    artifact_file: Some(artifact_file),
                    artifact_type: Some(artifact_type),
                    ..
                }),
            ..
        } = combined
        else {
            panic!("last run example must be combined work mode");
        };

        assert_eq!(artifact_type, "work-unit");
        assert_eq!(
            artifact_file.file_stem().and_then(|stem| stem.to_str()),
            Some(work_unit.as_str()),
            "combined work-mode example must use one work-unit identity"
        );
    }
}
