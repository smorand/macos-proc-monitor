//! Configuration: typed settings resolved from defaults, an optional TOML file,
//! environment variables, and CLI overrides (figment layered).
//!
//! Directory resolution keeps a strict contract the launchd daemon relies on:
//! the `MACOS_PROC_MONITOR_FOLDER_DATA` / `MACOS_PROC_MONITOR_FOLDER_LOG` env
//! vars override everything (the root plist sets them to `/var/db/...`),
//! otherwise the XDG cache dir (`~/.cache/macos-proc-monitor/{data,logs}`) is
//! used, and if no home directory can be resolved we fall back to
//! `/var/db/macos-proc-monitor/{data,logs}`.

use std::path::{Path, PathBuf};

use etcetera::{BaseStrategy, choose_base_strategy};
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// Application name, used for XDG directory resolution and env prefixes.
pub const APP_NAME: &str = "macos-proc-monitor";

/// Env var that overrides the data directory (set by the launchd plist).
pub const ENV_FOLDER_DATA: &str = "MACOS_PROC_MONITOR_FOLDER_DATA";
/// Env var that overrides the log directory (set by the launchd plist).
pub const ENV_FOLDER_LOG: &str = "MACOS_PROC_MONITOR_FOLDER_LOG";

/// Fully resolved daemon configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Sampling interval in seconds.
    pub interval: f64,
    /// How often (in seconds) to collect cwd + num_fds.
    pub slow_interval: u64,
    /// Prefix lsof calls with sudo.
    pub sudo: bool,
    /// Never collect cwd / num_fds.
    pub no_slow: bool,
    /// Monitor only this PID and its children (None = all processes).
    pub pid: Option<u32>,
    /// Delete data rows older than this many days (0 = keep forever).
    pub data_retention: u64,
    /// Delete log files older than this many days (0 = keep forever).
    pub log_retention: u64,
    /// How often (in seconds) to flush buffered records to Parquet.
    pub flush_interval: u64,
    /// TCP port for the web dashboard.
    pub port: u16,
    /// Bind address for the web dashboard.
    pub bind: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval: 1.0,
            slow_interval: 60,
            sudo: false,
            no_slow: false,
            pid: None,
            data_retention: 7,
            log_retention: 7,
            flush_interval: 30,
            port: 9090,
            bind: "127.0.0.1".to_string(),
        }
    }
}

/// CLI overrides: only fields set on the command line override lower layers.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConfigOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slow_interval: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sudo: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_slow: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_retention: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_retention: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flush_interval: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
}

impl Config {
    /// XDG config directory: `~/.config/macos-proc-monitor`.
    pub fn config_dir() -> Result<PathBuf, CoreError> {
        let strategy = choose_base_strategy().map_err(|e| CoreError::Dirs(e.to_string()))?;
        Ok(strategy.config_dir().join(APP_NAME))
    }

    /// Load configuration: defaults < TOML file < env (`MACOS_PROC_MONITOR_`) < CLI overrides.
    ///
    /// A missing TOML file is a no-op.
    pub fn load(
        config_path: Option<&Path>,
        overrides: &ConfigOverrides,
    ) -> Result<Self, CoreError> {
        let path = match config_path {
            Some(p) => p.to_path_buf(),
            None => Self::config_dir()?.join("config.toml"),
        };
        Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("MACOS_PROC_MONITOR_"))
            .merge(Serialized::defaults(overrides))
            .extract()
            .map_err(CoreError::from)
    }

    /// Resolve the data directory, honoring the daemon contract.
    pub fn data_dir(&self) -> PathBuf {
        resolve_dir(ENV_FOLDER_DATA, "data")
    }

    /// Resolve the log directory, honoring the daemon contract.
    pub fn log_dir(&self) -> PathBuf {
        resolve_dir(ENV_FOLDER_LOG, "logs")
    }
}

/// Resolve a runtime directory: env override, then XDG cache, then `/var/db`.
///
/// The env override is the contract the launchd plist relies on (it sets the
/// vars to `/var/db/...` when running as root). Without an override we use the
/// XDG cache dir; if even that cannot be resolved (no home directory) we fall
/// back to `/var/db/macos-proc-monitor/<subdir>`.
pub fn resolve_dir(env_var: &str, subdir: &str) -> PathBuf {
    if let Ok(v) = std::env::var(env_var) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    match choose_base_strategy() {
        Ok(strategy) => strategy.cache_dir().join(APP_NAME).join(subdir),
        Err(_) => PathBuf::from("/var/db/macos-proc-monitor").join(subdir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_values() {
        let cfg = Config::default();
        assert!((cfg.interval - 1.0).abs() < f64::EPSILON);
        assert_eq!(cfg.port, 9090);
        assert_eq!(cfg.bind, "127.0.0.1");
        assert_eq!(cfg.data_retention, 7);
    }

    #[test]
    fn overrides_win_over_defaults() {
        let overrides = ConfigOverrides {
            port: Some(8080),
            no_slow: Some(true),
            ..Default::default()
        };
        let cfg = Config::load(Some(Path::new("/nonexistent.toml")), &overrides).unwrap();
        assert_eq!(cfg.port, 8080);
        assert!(cfg.no_slow);
        // Untouched field keeps its default.
        assert_eq!(cfg.data_retention, 7);
    }

    #[test]
    fn resolve_dir_honors_env_override() {
        // HOME is set in every normal test environment, so this exercises the
        // env-override branch without mutating any process env var (which is
        // `unsafe` under edition 2024 and forbidden here).
        let dir = resolve_dir("HOME", "data");
        let home = std::env::var("HOME").expect("HOME set in test env");
        assert_eq!(dir, PathBuf::from(home));
    }

    #[test]
    fn resolve_dir_falls_back_to_cache_subdir() {
        let dir = resolve_dir("MACOS_PROC_MONITOR_UNSET_VAR_XYZ", "logs");
        assert!(dir.ends_with("macos-proc-monitor/logs"));
    }

    #[test]
    fn load_reads_values_from_toml_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "interval = 5.0\nport = 7777\nbind = \"0.0.0.0\"\nno_slow = true\n",
        )
        .unwrap();

        let cfg = Config::load(Some(&path), &ConfigOverrides::default()).unwrap();
        assert!((cfg.interval - 5.0).abs() < f64::EPSILON);
        assert_eq!(cfg.port, 7777);
        assert_eq!(cfg.bind, "0.0.0.0");
        assert!(cfg.no_slow);
        // Field absent from the file keeps its default.
        assert_eq!(cfg.flush_interval, 30);
    }

    #[test]
    fn cli_overrides_beat_the_toml_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = 7777\n").unwrap();

        let overrides = ConfigOverrides {
            port: Some(9999),
            ..Default::default()
        };
        let cfg = Config::load(Some(&path), &overrides).unwrap();
        assert_eq!(cfg.port, 9999);
    }

    #[test]
    fn config_dir_ends_with_app_name() {
        let dir = Config::config_dir().unwrap();
        assert!(dir.ends_with(APP_NAME));
    }

    #[test]
    fn data_and_log_dirs_resolve() {
        let cfg = Config::default();
        assert!(cfg.data_dir().ends_with("data") || cfg.data_dir().is_absolute());
        assert!(cfg.log_dir().ends_with("logs") || cfg.log_dir().is_absolute());
    }
}
