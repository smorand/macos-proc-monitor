//! Collection: per-process metrics sampled each second, written to partitioned Parquet.

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arrow::array::{Float32Array, Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use chrono::{Local, Utc};
use parquet::arrow::ArrowWriter;
use serde::Serialize;
use sysinfo::{
    Pid, Process, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind,
};
use tracing::{debug, error, info};
use uzers::get_user_by_uid;

use crate::config::Config;

/// Monotonic sequence counter for Parquet flush filenames.
static FLUSH_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Collection configuration
// ---------------------------------------------------------------------------

/// Configuration for the collection loop, built by the binary from CLI flags.
#[derive(Debug, Clone)]
pub struct CollectConfig {
    /// Sampling interval in seconds
    pub interval: f64,
    /// How often (in seconds) to collect cwd + num_fds
    pub slow_interval: u64,
    /// Prefix lsof calls with sudo
    pub sudo: bool,
    /// Never collect cwd / num_fds
    pub no_slow: bool,
    /// Monitor only this PID and its children (None = all processes)
    pub pid: Option<u32>,
    /// Delete data rows older than this many days (0 = keep forever)
    pub data_retention: u64,
    /// Delete log files older than this many days (0 = keep forever)
    pub log_retention: u64,
    /// How often (in seconds) to flush buffered records to Parquet
    pub flush_interval: u64,
    /// Directory for Parquet output
    pub data_dir: PathBuf,
    /// Directory for log files
    pub log_dir: PathBuf,
}

impl CollectConfig {
    /// Build a collection config from the resolved [`Config`] plus the already
    /// resolved data and log directories.
    pub fn from_config(cfg: &Config, data_dir: PathBuf, log_dir: PathBuf) -> Self {
        Self {
            interval: cfg.interval,
            slow_interval: cfg.slow_interval,
            sudo: cfg.sudo,
            no_slow: cfg.no_slow,
            pid: cfg.pid,
            data_retention: cfg.data_retention,
            log_retention: cfg.log_retention,
            flush_interval: cfg.flush_interval,
            data_dir,
            log_dir,
        }
    }
}

// ---------------------------------------------------------------------------
// Record struct
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct ProcRecord {
    ts: f64,
    pid: u32,
    ppid: Option<u32>,
    name: String,
    user: Option<String>,
    status: String,
    cpu_percent: f32,
    mem_rss: u64,
    mem_vms: u64,
    num_threads: u64,
    create_time: f64,
    cwd: Option<String>,
    num_fds: Option<u32>,
    children: Vec<u32>,
}

// ---------------------------------------------------------------------------
// Helpers: timestamp
// ---------------------------------------------------------------------------

fn unix_now() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Return current UTC hour string: "YYYY-MM-DDTHH"
fn current_hour() -> String {
    Utc::now().format("%Y-%m-%dT%H").to_string()
}

// ---------------------------------------------------------------------------
// Helpers: uid → username cache
// ---------------------------------------------------------------------------

struct UserCache {
    cache: HashMap<u32, Option<String>>,
}

impl UserCache {
    fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    fn lookup(&mut self, uid: u32) -> Option<String> {
        self.cache
            .entry(uid)
            .or_insert_with(|| {
                get_user_by_uid(uid).map(|u| u.name().to_string_lossy().into_owned())
            })
            .clone()
    }
}

// ---------------------------------------------------------------------------
// Helpers: lsof — cwd
// ---------------------------------------------------------------------------

fn lsof_cwd(pid: u32, use_sudo: bool) -> Option<String> {
    let mut cmd = if use_sudo {
        let mut c = Command::new("sudo");
        c.args([
            "/usr/bin/lsof",
            "-p",
            &pid.to_string(),
            "-a",
            "-d",
            "cwd",
            "-Fn",
        ]);
        c
    } else {
        let mut c = Command::new("/usr/bin/lsof");
        c.args(["-p", &pid.to_string(), "-a", "-d", "cwd", "-Fn"]);
        c
    };

    let out = cmd.output().ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(path) = line.strip_prefix('n') {
            if !path.is_empty() && path != "/" {
                return Some(path.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Helpers: lsof — num_fds
// ---------------------------------------------------------------------------

fn lsof_num_fds(pid: u32, use_sudo: bool) -> Option<u32> {
    let mut cmd = if use_sudo {
        let mut c = Command::new("sudo");
        c.args(["/usr/bin/lsof", "-p", &pid.to_string()]);
        c
    } else {
        let mut c = Command::new("/usr/bin/lsof");
        c.args(["-p", &pid.to_string()]);
        c
    };

    let out = cmd.output().ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        return None;
    }
    let count = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    if count == 0 {
        None
    } else {
        u32::try_from(count.saturating_sub(1)).ok()
    }
}

// ---------------------------------------------------------------------------
// Helpers: build child PID map
// ---------------------------------------------------------------------------

fn build_children_map(sys: &System) -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, proc_) in sys.processes() {
        if let Some(parent) = proc_.parent() {
            map.entry(parent.as_u32()).or_default().push(pid.as_u32());
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Helpers: slow fields (cwd + num_fds)
// ---------------------------------------------------------------------------

struct SlowFields {
    cwd: Option<String>,
    num_fds: Option<u32>,
}

fn collect_slow_for_pid(pid: u32, use_sudo: bool) -> SlowFields {
    let cwd = lsof_cwd(pid, use_sudo);
    let num_fds = lsof_num_fds(pid, use_sudo);
    SlowFields { cwd, num_fds }
}

// ---------------------------------------------------------------------------
// Purge files older than retention_days
// ---------------------------------------------------------------------------

fn purge_old_files(dir: &Path, ext: &str, retention_days: u64) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(retention_days * 86400);

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(u64::MAX, |d| d.as_secs());
        if mtime < cutoff {
            if let Err(e) = fs::remove_file(&path) {
                error!("[purge] cannot remove {}: {e}", path.display());
            } else {
                info!("[purge] removed {}", path.display());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parquet schema
// ---------------------------------------------------------------------------

fn parquet_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("ts", DataType::Float64, false),
        Field::new("pid", DataType::UInt32, false),
        Field::new("ppid", DataType::UInt32, true),
        Field::new("name", DataType::Utf8, true),
        Field::new("user_name", DataType::Utf8, true),
        Field::new("status", DataType::Utf8, true),
        Field::new("cpu_percent", DataType::Float32, true),
        Field::new("mem_rss", DataType::UInt64, false),
        Field::new("mem_vms", DataType::UInt64, false),
        Field::new("num_threads", DataType::UInt64, false),
        Field::new("create_time", DataType::Float64, false),
        Field::new("cwd", DataType::Utf8, true),
        Field::new("num_fds", DataType::UInt32, true),
        Field::new("children", DataType::Utf8, true),
    ]))
}

// ---------------------------------------------------------------------------
// Write a batch of records to a new Parquet file
// ---------------------------------------------------------------------------

fn write_parquet(path: &Path, records: &[ProcRecord]) -> Result<(), Box<dyn std::error::Error>> {
    if records.is_empty() {
        return Ok(());
    }
    let schema = parquet_schema();
    let file = File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema.clone(), None)?;

    let children_strs: Vec<String> = records
        .iter()
        .map(|r| serde_json::to_string(&r.children).unwrap_or_else(|_| "[]".into()))
        .collect();

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Float64Array::from(
                records.iter().map(|r| r.ts).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                records.iter().map(|r| r.pid).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                records.iter().map(|r| r.ppid).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records
                    .iter()
                    .map(|r| Some(r.name.as_str()))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records
                    .iter()
                    .map(|r| r.user.as_deref())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records
                    .iter()
                    .map(|r| Some(r.status.as_str()))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float32Array::from(
                records
                    .iter()
                    .map(|r| Some(r.cpu_percent))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                records.iter().map(|r| r.mem_rss).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                records.iter().map(|r| r.mem_vms).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                records.iter().map(|r| r.num_threads).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                records.iter().map(|r| r.create_time).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records.iter().map(|r| r.cwd.as_deref()).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                records.iter().map(|r| r.num_fds).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                children_strs
                    .iter()
                    .map(|s| Some(s.as_str()))
                    .collect::<Vec<_>>(),
            )),
        ],
    )?;

    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Build a ProcRecord from sysinfo data
// ---------------------------------------------------------------------------

fn build_record(
    pid: u32,
    process: &Process,
    ts: f64,
    user_cache: &mut UserCache,
    children: &[u32],
    slow: Option<&SlowFields>,
) -> ProcRecord {
    #![allow(clippy::similar_names)] // pid / ppid are domain terms
    let ppid = process.parent().map(Pid::as_u32);
    let uid = process.user_id().map(|u| **u);
    let user = uid.and_then(|u| user_cache.lookup(u));
    let status = format!("{:?}", process.status()).to_lowercase();
    let (cwd, num_fds) = match slow {
        Some(s) => (s.cwd.clone(), s.num_fds),
        None => (None, None),
    };

    ProcRecord {
        ts,
        pid,
        ppid,
        name: process.name().to_string_lossy().into_owned(),
        user,
        status,
        cpu_percent: process.cpu_usage(),
        mem_rss: process.memory(),
        mem_vms: process.virtual_memory(),
        num_threads: process.tasks().map_or(1, |t| t.len() as u64 + 1),
        // Unix-epoch seconds fit exactly in an f64 mantissa; no meaningful loss.
        #[allow(clippy::cast_precision_loss)]
        create_time: process.start_time() as f64,
        cwd,
        num_fds,
        children: children.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Filter: if pid is set, collect only that pid + descendants
// ---------------------------------------------------------------------------

fn reachable_pids(root: u32, children_map: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut result = vec![root];
    let mut queue = vec![root];
    while let Some(p) = queue.pop() {
        if let Some(kids) = children_map.get(&p) {
            for &k in kids {
                result.push(k);
                queue.push(k);
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Collection loop (blocking — run on a dedicated thread)
// ---------------------------------------------------------------------------

#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
pub fn collect_loop(cfg: CollectConfig) {
    let data_dir = cfg.data_dir.clone();

    info!("macos-proc-monitor collection starting");
    info!("log  -> {}/monitor.<date>.log", cfg.log_dir.display());
    info!(
        "data -> {}/ (parquet, flush every {}s)",
        data_dir.display(),
        cfg.flush_interval
    );
    info!(
        "interval={}s  slow-interval={}s  sudo={}  no-slow={}",
        cfg.interval, cfg.slow_interval, cfg.sudo, cfg.no_slow
    );
    info!(
        "retention: data={}d  logs={}d",
        if cfg.data_retention == 0 {
            "inf".to_string()
        } else {
            cfg.data_retention.to_string()
        },
        if cfg.log_retention == 0 {
            "inf".to_string()
        } else {
            cfg.log_retention.to_string()
        },
    );

    let interval = Duration::from_secs_f64(cfg.interval.max(0.1));
    let slow_every = Duration::from_secs(cfg.slow_interval.max(1));
    let flush_every = Duration::from_secs(cfg.flush_interval.max(1));

    let refresh_kind = RefreshKind::nothing().with_processes(
        ProcessRefreshKind::everything()
            .with_cpu()
            .with_memory()
            .with_user(UpdateKind::Always),
    );

    let mut sys = System::new_with_specifics(refresh_kind);
    let mut user_cache = UserCache::new();
    let mut last_slow = Instant::now()
        .checked_sub(slow_every)
        .unwrap_or_else(Instant::now);
    let mut slow_cache: HashMap<u32, SlowFields> = HashMap::new();

    // Parquet buffer
    let mut buffer: Vec<ProcRecord> = Vec::new();
    let mut last_flush = Instant::now()
        .checked_sub(flush_every)
        .unwrap_or_else(Instant::now);
    let mut current_hour_str = current_hour();

    // For daily retention purge
    let mut last_retention_date = Local::now().format("%Y-%m-%d").to_string();

    loop {
        let tick_start = Instant::now();
        let ts = unix_now();

        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::everything(),
        );

        let children_map = build_children_map(&sys);

        let do_slow = !cfg.no_slow && tick_start.duration_since(last_slow) >= slow_every;
        if do_slow {
            last_slow = tick_start;
        }

        let all_pids: Vec<u32> = sys.processes().keys().map(|p| p.as_u32()).collect();
        let target_pids: Vec<u32> = if let Some(root) = cfg.pid {
            reachable_pids(root, &children_map)
        } else {
            all_pids
        };

        // Build records and push into buffer
        let mut tick_count = 0usize;
        for pid in &target_pids {
            let spid = Pid::from_u32(*pid);
            let Some(process) = sys.process(spid) else {
                continue;
            };

            if do_slow {
                let slow_fields = collect_slow_for_pid(*pid, cfg.sudo);
                slow_cache.insert(*pid, slow_fields);
            }

            let slow_ref = slow_cache.get(pid);
            let children = children_map.get(pid).cloned().unwrap_or_default();
            let record = build_record(*pid, process, ts, &mut user_cache, &children, slow_ref);
            buffer.push(record);
            tick_count += 1;
        }

        // Purge stale slow cache entries
        slow_cache.retain(|pid, _| sys.process(Pid::from_u32(*pid)).is_some());

        // Flush to Parquet if the interval elapsed or the hour changed
        let new_hour = current_hour();
        let hour_changed = new_hour != current_hour_str;
        let time_to_flush = tick_start.duration_since(last_flush) >= flush_every;

        if (time_to_flush || hour_changed) && !buffer.is_empty() {
            // Flush records accumulated so far (they belong to current_hour_str)
            let flush_hour = current_hour_str.clone();
            let seq = FLUSH_COUNTER.fetch_add(1, Ordering::Relaxed);
            let filename = format!("{flush_hour}_{seq:06}.parquet");
            let path = data_dir.join(&filename);

            match write_parquet(&path, &buffer) {
                Ok(()) => {
                    debug!("flushed {} records -> {}", buffer.len(), filename);
                }
                Err(e) => {
                    error!("Parquet write error {filename}: {e}");
                }
            }

            buffer.clear();
            last_flush = tick_start;
        }

        if hour_changed {
            current_hour_str = new_hour;
        }

        // Daily retention purge (parquet data + rolling log files).
        {
            let today = Local::now().format("%Y-%m-%d").to_string();
            if today != last_retention_date {
                last_retention_date = today;
                if cfg.data_retention > 0 {
                    purge_old_files(&data_dir, "parquet", cfg.data_retention);
                    info!(
                        "Parquet retention purge: removed files older than {}d",
                        cfg.data_retention
                    );
                }
                if cfg.log_retention > 0 {
                    purge_old_files(&cfg.log_dir, "log", cfg.log_retention);
                    info!(
                        "Log retention purge: removed files older than {}d",
                        cfg.log_retention
                    );
                }
            }
        }

        let elapsed = tick_start.elapsed();
        info!(
            "{} procs | {}ms | slow={} | buffered={}",
            tick_count,
            elapsed.as_millis(),
            if do_slow { "yes" } else { "no" },
            buffer.len(),
        );

        let remaining = interval.saturating_sub(elapsed);
        if !remaining.is_zero() {
            std::thread::sleep(remaining);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckdb::Connection;

    #[allow(clippy::similar_names)] // pid / ppid are domain terms
    fn sample_record(pid: u32, ppid: Option<u32>, name: &str, children: Vec<u32>) -> ProcRecord {
        ProcRecord {
            ts: 1_700_000_000.0,
            pid,
            ppid,
            name: name.to_string(),
            user: Some("tester".to_string()),
            status: "running".to_string(),
            cpu_percent: 12.5,
            mem_rss: 1024,
            mem_vms: 4096,
            num_threads: 3,
            create_time: 1_699_999_000.0,
            cwd: Some("/tmp".to_string()),
            num_fds: Some(7),
            children,
        }
    }

    #[test]
    fn unix_now_is_positive_and_recent() {
        let now = unix_now();
        assert!(now > 1_700_000_000.0, "expected a plausible unix timestamp");
    }

    #[test]
    fn current_hour_has_expected_shape() {
        let h = current_hour();
        // "YYYY-MM-DDTHH" == 13 chars.
        assert_eq!(h.len(), 13, "got {h}");
        assert_eq!(&h[4..5], "-");
        assert_eq!(&h[10..11], "T");
    }

    #[test]
    fn user_cache_caches_lookups() {
        let mut cache = UserCache::new();
        // uid 0 is root on every unix; both calls must agree.
        let a = cache.lookup(0);
        let b = cache.lookup(0);
        assert_eq!(a, b);
    }

    #[test]
    fn reachable_pids_walks_descendants() {
        let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
        map.insert(1, vec![2, 3]);
        map.insert(2, vec![4]);
        let mut got = reachable_pids(1, &map);
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3, 4]);
    }

    #[test]
    fn reachable_pids_leaf_is_just_itself() {
        let map: HashMap<u32, Vec<u32>> = HashMap::new();
        assert_eq!(reachable_pids(42, &map), vec![42]);
    }

    #[test]
    fn parquet_schema_has_all_columns() {
        let schema = parquet_schema();
        assert_eq!(schema.fields().len(), 14);
        assert_eq!(schema.field(0).name(), "ts");
        assert_eq!(schema.field(13).name(), "children");
    }

    #[test]
    fn write_parquet_empty_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.parquet");
        write_parquet(&path, &[]).unwrap();
        assert!(!path.exists(), "no file should be written for empty input");
    }

    #[test]
    fn write_parquet_roundtrips_via_duckdb() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("2025-01-01T00_000000.parquet");
        let records = vec![
            sample_record(100, Some(1), "alpha", vec![200]),
            sample_record(200, Some(100), "beta", vec![]),
        ];
        write_parquet(&path, &records).unwrap();
        assert!(path.exists());

        let conn = Connection::open_in_memory().unwrap();
        let pattern = path.to_string_lossy().to_string();
        let sql = format!(
            "SELECT pid, name, user_name, num_fds, children FROM read_parquet('{pattern}') ORDER BY pid"
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        let rows: Vec<(u32, String, String, Option<u32>, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<u32>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 100);
        assert_eq!(rows[0].1, "alpha");
        assert_eq!(rows[0].2, "tester");
        assert_eq!(rows[0].3, Some(7));
        assert_eq!(rows[0].4, "[200]");
        assert_eq!(rows[1].0, 200);
        assert_eq!(rows[1].4, "[]");
    }

    #[test]
    fn purge_old_files_removes_only_stale_matching_ext() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let old = dir.join("old.parquet");
        let fresh = dir.join("fresh.parquet");
        let other = dir.join("keep.log");
        fs::write(&old, b"x").unwrap();
        fs::write(&fresh, b"x").unwrap();
        fs::write(&other, b"x").unwrap();

        // Backdate `old` well beyond the retention window.
        let ten_days_ago =
            filetime::FileTime::from_unix_time(Utc::now().timestamp() - 10 * 86400, 0);
        filetime::set_file_mtime(&old, ten_days_ago).unwrap();

        purge_old_files(dir, "parquet", 7);

        assert!(!old.exists(), "stale parquet should be removed");
        assert!(fresh.exists(), "fresh parquet should remain");
        assert!(other.exists(), "non-matching extension should remain");
    }

    #[test]
    fn collect_config_from_config_maps_fields() {
        let cfg = Config {
            interval: 2.0,
            no_slow: true,
            data_retention: 3,
            ..Config::default()
        };
        let cc = CollectConfig::from_config(&cfg, PathBuf::from("/tmp/d"), PathBuf::from("/tmp/l"));
        assert!((cc.interval - 2.0).abs() < f64::EPSILON);
        assert!(cc.no_slow);
        assert_eq!(cc.data_retention, 3);
        assert_eq!(cc.data_dir, PathBuf::from("/tmp/d"));
        assert_eq!(cc.log_dir, PathBuf::from("/tmp/l"));
    }
}
