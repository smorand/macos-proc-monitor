//! macos-proc-monitor — daemon that collects per-process metrics AND serves the web dashboard.
//!
//! The collection loop runs on a dedicated blocking thread; the Axum web server runs on the
//! tokio runtime. Both read/write the same Parquet data directory.

use std::fs;

use clap::Parser;
use procmon::{collect_loop, default_dir, init_logging, serve_web, CollectConfig};
use tracing::{error, info};

// ---------------------------------------------------------------------------
// CLI (merged: collection flags + web flags)
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "macos-proc-monitor",
    about = "Collect per-process metrics every second (Parquet) and serve the web dashboard",
    long_about = None,
    version
)]
struct Args {
    // --- Collection flags ---
    /// Sampling interval in seconds
    #[arg(long, default_value = "1.0")]
    interval: f64,

    /// How often (in seconds) to collect cwd + num_fds
    #[arg(long, default_value = "60")]
    slow_interval: u64,

    /// Prefix lsof calls with sudo (requires passwordless sudo for /usr/bin/lsof)
    #[arg(long)]
    sudo: bool,

    /// Never collect cwd / num_fds
    #[arg(long)]
    no_slow: bool,

    /// Monitor only this PID and its children (omit for all processes)
    #[arg(long)]
    pid: Option<u32>,

    /// Delete data rows older than this many days (0 = keep forever)
    #[arg(long, default_value = "7")]
    data_retention: u64,

    /// Delete log files older than this many days (0 = keep forever)
    #[arg(long, default_value = "7")]
    log_retention: u64,

    /// How often (in seconds) to flush buffered records to Parquet
    #[arg(long, default_value = "30")]
    flush_interval: u64,

    // --- Web flags ---
    /// TCP port for the web dashboard
    #[arg(long, default_value = "9090", env = "ANALYTICS_PORT")]
    port: u16,

    /// Bind address for the web dashboard
    #[arg(long, default_value = "127.0.0.1", env = "ANALYTICS_BIND")]
    bind: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let log_dir  = default_dir("MACOS_PROC_MONITOR_FOLDER_LOG", "logs");
    let data_dir = default_dir("MACOS_PROC_MONITOR_FOLDER_DATA", "data");

    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!("Cannot create log dir {:?}: {e}", log_dir);
    }
    if let Err(e) = fs::create_dir_all(&data_dir) {
        eprintln!("Cannot create data dir {:?}: {e}", data_dir);
    }

    init_logging(&log_dir, args.log_retention);

    info!("macos-proc-monitor daemon starting (collector + web)");

    let cfg = CollectConfig {
        interval: args.interval,
        slow_interval: args.slow_interval,
        sudo: args.sudo,
        no_slow: args.no_slow,
        pid: args.pid,
        data_retention: args.data_retention,
        log_retention: args.log_retention,
        flush_interval: args.flush_interval,
        data_dir: data_dir.clone(),
        log_dir: log_dir.clone(),
    };

    // Collection loop is blocking; run it on a dedicated OS thread.
    std::thread::spawn(move || {
        collect_loop(cfg);
    });

    // Web server runs on the tokio runtime and drives the process.
    if let Err(e) = serve_web(args.bind, args.port, data_dir).await {
        error!("web server error: {e}");
        return Err(e);
    }

    Ok(())
}
