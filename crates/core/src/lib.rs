//! macos-proc-core (`procmon`): metrics collection + web dashboard for macOS
//! per-process metrics.
//!
//! The collection loop ([`collect_loop`]) is blocking and runs on a dedicated
//! OS thread; the web server ([`serve_web`]) runs on the tokio runtime. Both
//! read/write the same Parquet data directory.

pub mod collect;
pub mod config;
pub mod error;
pub mod telemetry;
pub mod web;

pub use collect::{CollectConfig, collect_loop};
pub use config::{Config, ConfigOverrides, resolve_dir};
pub use error::CoreError;
pub use telemetry::{TelemetryGuard, init as init_telemetry};
pub use web::serve_web;

/// Crate version, exposed for `--version` and health output.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Await a shutdown signal: Ctrl-C, or SIGTERM on unix.
pub async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_non_empty() {
        assert!(!VERSION.is_empty());
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn shutdown_signal_is_pending_without_a_signal() {
        // With no Ctrl-C / SIGTERM, the future must not resolve. This exercises
        // the signal-handler setup and both select arms without raising a
        // real signal (which `unsafe`-free code cannot do here).
        let res =
            tokio::time::timeout(std::time::Duration::from_millis(150), shutdown_signal()).await;
        assert!(res.is_err(), "shutdown_signal resolved without any signal");
    }
}
