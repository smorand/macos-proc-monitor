//! macos-proc-monitor — daemon that collects per-process metrics AND serves the web dashboard.
//!
//! The collection loop runs on a dedicated blocking thread; the Axum web server runs on the
//! tokio runtime. Both read/write the same Parquet data directory. A sync `main` renders errors
//! and returns an exit code; the async `run` holds the logic.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use procmon::{CollectConfig, Config, ConfigOverrides, collect_loop, init_telemetry, serve_web};
use tracing::info;

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
    /// Optional TOML config file (defaults to the XDG config dir).
    #[arg(long)]
    config: Option<PathBuf>,

    // --- Collection flags ---
    /// Sampling interval in seconds
    #[arg(long)]
    interval: Option<f64>,

    /// How often (in seconds) to collect cwd + num_fds
    #[arg(long)]
    slow_interval: Option<u64>,

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
    #[arg(long)]
    data_retention: Option<u64>,

    /// Delete log files older than this many days (0 = keep forever)
    #[arg(long)]
    log_retention: Option<u64>,

    /// How often (in seconds) to flush buffered records to Parquet
    #[arg(long)]
    flush_interval: Option<u64>,

    // --- Web flags ---
    /// TCP port for the web dashboard
    #[arg(long)]
    port: Option<u16>,

    /// Bind address for the web dashboard
    #[arg(long)]
    bind: Option<String>,
}

impl Args {
    /// Convert parsed flags into config overrides (only set flags override).
    fn overrides(&self) -> ConfigOverrides {
        ConfigOverrides {
            interval: self.interval,
            slow_interval: self.slow_interval,
            // Boolean flags: only override when the switch is present.
            sudo: self.sudo.then_some(true),
            no_slow: self.no_slow.then_some(true),
            pid: self.pid,
            data_retention: self.data_retention,
            log_retention: self.log_retention,
            flush_interval: self.flush_interval,
            port: self.port,
            bind: self.bind.clone(),
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    let cfg =
        Config::load(args.config.as_deref(), &args.overrides()).context("loading configuration")?;

    let data_dir = cfg.data_dir();
    let log_dir = cfg.log_dir();

    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;

    // Telemetry guard must live for the whole process (flushes file logs on drop).
    let _telemetry = init_telemetry(&log_dir).context("initializing telemetry")?;

    info!("macos-proc-monitor daemon starting (collector + web)");

    let collect_cfg = CollectConfig::from_config(&cfg, data_dir.clone(), log_dir.clone());

    // Collection loop is blocking; run it on a dedicated OS thread.
    std::thread::spawn(move || {
        collect_loop(collect_cfg);
    });

    // Web server runs on the tokio runtime and drives the process (with graceful shutdown).
    serve_web(cfg.bind.clone(), cfg.port, data_dir)
        .await
        .context("web server")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_only_set_present_flags() {
        let args = Args {
            config: None,
            interval: Some(2.0),
            slow_interval: None,
            sudo: true,
            no_slow: false,
            pid: Some(42),
            data_retention: None,
            log_retention: None,
            flush_interval: None,
            port: Some(8080),
            bind: None,
        };
        let o = args.overrides();
        assert_eq!(o.interval, Some(2.0));
        assert_eq!(o.slow_interval, None);
        assert_eq!(o.sudo, Some(true));
        // `no_slow` not passed => None, so it never overrides lower layers.
        assert_eq!(o.no_slow, None);
        assert_eq!(o.pid, Some(42));
        assert_eq!(o.port, Some(8080));
        assert_eq!(o.bind, None);
    }

    #[test]
    fn cli_parses_all_flags() {
        let args = Args::try_parse_from([
            "macos-proc-monitor",
            "--interval",
            "1.5",
            "--no-slow",
            "--port",
            "9091",
            "--bind",
            "0.0.0.0",
        ])
        .unwrap();
        assert_eq!(args.interval, Some(1.5));
        assert!(args.no_slow);
        assert_eq!(args.port, Some(9091));
        assert_eq!(args.bind.as_deref(), Some("0.0.0.0"));
    }
}
