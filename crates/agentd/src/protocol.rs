//! Daemon Unix-socket wire messages.
//!
//! Wire format and connection lifecycle are specified in
//! `docs/socket-protocol.md`: newline-delimited JSON, one request and one
//! response per connection. The protocol is intentionally internal and
//! unversioned in `v0.1.x` — daemon and CLI client must be the same build
//! (see README § Running a Session), which is why these types are
//! `pub(crate)` and carry no version field.

use agentd_runner::{InvocationInput, SessionOutcome};
use serde::{Deserialize, Serialize};

/// Client → daemon. Serialized externally as `{"type": "ping"}` or
/// `{"type": "run", ...}`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum RequestMessage {
    /// Liveness probe; the daemon answers [`ResponseMessage::Pong`].
    Ping,
    /// Trigger one session for a configured agent.
    Run {
        /// Exact `[[agents]].name` from `agentd.toml`.
        agent: String,
        /// Clone URL override; `None` uses the agent's configured `repo`.
        repo_url: Option<String>,
        /// Work-unit identifier for work-mode invocations.
        work_unit: Option<String>,
        /// Operator-supplied input materialized into the session workspace.
        input: Option<InvocationInput>,
    },
}

/// Daemon → client. Exactly one is written per accepted request, then the
/// connection closes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponseMessage {
    /// Answer to [`RequestMessage::Ping`].
    Pong,
    /// The session ran to a terminal outcome (which may be a failure
    /// outcome — transport success, not work success).
    SessionOutcome { outcome: OutcomeMessage },
    /// The request was rejected before a session outcome existed:
    /// malformed JSON, unknown agent, or dispatch failure.
    Error { message: String },
}

/// Terminal session outcome.
///
/// The `status` labels and `exit_code` values for `success` through
/// `infrastructure_failure` (codes 0–6) and the reserved external codes
/// (126, 127, 128+N) implement the shared session-outcome vocabulary —
/// canonical: [commons/EXIT-CODES.md](https://github.com/tesserine/commons/blob/main/EXIT-CODES.md).
/// Do not redefine their semantics here.
///
/// `timed_out` is the one agentd-layer addition: commons defines
/// caller-enforced timeout as a caller-layer outcome outside the shared
/// vocabulary, and agentd is that caller.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum OutcomeMessage {
    Success { exit_code: i32 },
    GenericFailure { exit_code: i32 },
    UsageError { exit_code: i32 },
    Blocked { exit_code: i32 },
    NothingReady { exit_code: i32 },
    WorkFailed { exit_code: i32 },
    InfrastructureFailure { exit_code: i32 },
    CommandNotExecutable { exit_code: i32 },
    CommandNotFound { exit_code: i32 },
    TerminatedBySignal { exit_code: i32, signal: i32 },
    TimedOut,
}

impl From<SessionOutcome> for OutcomeMessage {
    fn from(outcome: SessionOutcome) -> Self {
        match outcome {
            SessionOutcome::Success { exit_code } => Self::Success { exit_code },
            SessionOutcome::GenericFailure { exit_code } => Self::GenericFailure { exit_code },
            SessionOutcome::UsageError { exit_code } => Self::UsageError { exit_code },
            SessionOutcome::Blocked { exit_code } => Self::Blocked { exit_code },
            SessionOutcome::NothingReady { exit_code } => Self::NothingReady { exit_code },
            SessionOutcome::WorkFailed { exit_code } => Self::WorkFailed { exit_code },
            SessionOutcome::InfrastructureFailure { exit_code } => {
                Self::InfrastructureFailure { exit_code }
            }
            SessionOutcome::CommandNotExecutable { exit_code } => {
                Self::CommandNotExecutable { exit_code }
            }
            SessionOutcome::CommandNotFound { exit_code } => Self::CommandNotFound { exit_code },
            SessionOutcome::TerminatedBySignal { exit_code, signal } => {
                Self::TerminatedBySignal { exit_code, signal }
            }
            SessionOutcome::TimedOut => Self::TimedOut,
        }
    }
}

impl From<OutcomeMessage> for SessionOutcome {
    fn from(outcome: OutcomeMessage) -> Self {
        match outcome {
            OutcomeMessage::Success { exit_code } => Self::Success { exit_code },
            OutcomeMessage::GenericFailure { exit_code } => Self::GenericFailure { exit_code },
            OutcomeMessage::UsageError { exit_code } => Self::UsageError { exit_code },
            OutcomeMessage::Blocked { exit_code } => Self::Blocked { exit_code },
            OutcomeMessage::NothingReady { exit_code } => Self::NothingReady { exit_code },
            OutcomeMessage::WorkFailed { exit_code } => Self::WorkFailed { exit_code },
            OutcomeMessage::InfrastructureFailure { exit_code } => {
                Self::InfrastructureFailure { exit_code }
            }
            OutcomeMessage::CommandNotExecutable { exit_code } => {
                Self::CommandNotExecutable { exit_code }
            }
            OutcomeMessage::CommandNotFound { exit_code } => Self::CommandNotFound { exit_code },
            OutcomeMessage::TerminatedBySignal { exit_code, signal } => {
                Self::TerminatedBySignal { exit_code, signal }
            }
            OutcomeMessage::TimedOut => Self::TimedOut,
        }
    }
}
