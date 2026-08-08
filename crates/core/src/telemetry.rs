//! Observability: `tracing` with a stderr layer (for launchd capture) and a
//! daily-rolling JSON-ish file layer under the log directory.
//!
//! Both outputs are always on: launchd captures stderr into its own bootstrap
//! log, and the rolling file layer keeps `monitor.<date>.log` under the log
//! dir. The returned [`TelemetryGuard`] MUST be held for the whole process:
//! dropping it flushes and stops the non-blocking file writer.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

/// Holds the non-blocking file-writer guard alive for the process lifetime.
///
/// Dropping this flushes buffered log lines and shuts the writer thread down.
#[must_use = "dropping the guard stops file logging and loses buffered lines"]
pub struct TelemetryGuard {
    _file_guard: WorkerGuard,
}

/// Initialize tracing: stderr + daily-rolling file under `log_dir`.
///
/// Log files are named `monitor.<YYYY-MM-DD>.log` so the retention purge
/// (which matches the `log` extension) still finds them. Verbosity is driven
/// by `RUST_LOG`, defaulting to `info`.
pub fn init(log_dir: &Path) -> std::io::Result<TelemetryGuard> {
    std::fs::create_dir_all(log_dir)?;

    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("monitor")
        .filename_suffix("log")
        .build(log_dir)
        .map_err(std::io::Error::other)?;
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);

    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_timer(ChronoLocal::new("[%H:%M:%S]".into()))
        .with_target(false)
        .with_level(true)
        .with_ansi(false)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));

    let file_layer = fmt::layer()
        .with_writer(file_writer)
        .with_timer(ChronoLocal::new("[%H:%M:%S]".into()))
        .with_target(false)
        .with_level(true)
        .with_ansi(false)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .init();

    Ok(TelemetryGuard {
        _file_guard: file_guard,
    })
}
