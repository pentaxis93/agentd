//! Composition root for the agentd daemon.
//!
//! Exposes the [`config`] module for TOML-based agent configuration. The
//! binary crate (`main.rs`) assembles runner and scheduler from the parsed
//! configuration and starts the daemon.

mod audit_root;
pub mod config;
pub mod daemon;
pub mod dispatch;
pub mod logging;
mod protocol;
pub mod runtime_paths;
mod scheduler;

pub use daemon::{
    ClientError, DaemonError, LiveObservationLevel, request_run, request_run_with_live_observation,
    run_daemon_until_shutdown,
};
pub use dispatch::{
    DispatchError, RunRequest, RunnerSessionExecutor, SessionExecutor, dispatch_run,
};
pub use logging::{
    LogFormat, LoggingError, ResolvedLoggingConfig, configure_tracing, resolve_logging_config,
};
pub use runtime_paths::{
    ClientSocketPathError, RuntimePathError, default_daemon_runtime_paths,
    resolve_client_socket_path,
};
