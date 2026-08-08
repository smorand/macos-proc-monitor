//! Collection: per-process metrics sampled each second, written to partitioned Parquet.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{
    Float32Array, Float64Array, StringArray, UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use chrono::{Local, Utc};
use parquet::arrow::ArrowWriter;
use serde::Serialize;
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};
use tracing::{debug, error, info};
use tracing_subscriber::fmt::time::ChronoLocal;
use uzers::get_user_by_uid;

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
        Self { cache: HashMap::new() }
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
        c.args(["/usr/bin/lsof", "-p", &pid.to_string(), "-a", "-d", "cwd", "-Fn"]);
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
        Some(count.saturating_sub(1) as u32)
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
// Helpers: directory resolution
// ---------------------------------------------------------------------------

pub fn default_dir(env_var: &str, subdir: &str) -> PathBuf {
    if let Ok(v) = std::env::var(env_var) {
        return PathBuf::from(v);
    }
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".cache")
            .join("macos-proc-monitor")
            .join(subdir)
    } else {
        PathBuf::from("/var/db/macos-proc-monitor").join(subdir)
    }
}

// ---------------------------------------------------------------------------
// Purge files older than retention_days
// ---------------------------------------------------------------------------

fn purge_old_files(dir: &PathBuf, ext: &str, retention_days: u64) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(retention_days * 86400);

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
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
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX);
        if mtime < cutoff {
            if let Err(e) = fs::remove_file(&path) {
                eprintln!("[purge] cannot remove {:?}: {e}", path);
            } else {
                eprintln!("[purge] removed {:?}", path);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rotating log writer
// ---------------------------------------------------------------------------

struct RotatingLogWriter {
    dir: PathBuf,
    current_date: String,
    file: BufWriter<File>,
    retention_days: u64,
}

impl RotatingLogWriter {
    fn new(log_dir: &PathBuf, retention_days: u64) -> std::io::Result<Self> {
        fs::create_dir_all(log_dir)?;
        let current_date = Local::now().format("%Y-%m-%d").to_string();
        let path = log_dir.join(format!("monitor-{current_date}.log"));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            dir: log_dir.clone(),
            current_date,
            file: BufWriter::new(file),
            retention_days,
        })
    }

    fn rotate_if_needed(&mut self) {
        let today = Local::now().format("%Y-%m-%d").to_string();
        if today == self.current_date {
            return;
        }
        let _ = self.file.flush();
        let path = self.dir.join(format!("monitor-{today}.log"));
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                self.file = BufWriter::new(f);
                self.current_date = today;
                if self.retention_days > 0 {
                    purge_old_files(&self.dir, "log", self.retention_days);
                }
            }
            Err(e) => eprintln!("[log rotate] cannot open {path:?}: {e}"),
        }
    }
}

struct SharedLogWriter(std::sync::Arc<std::sync::Mutex<RotatingLogWriter>>);

impl std::io::Write for SharedLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        if let Ok(mut guard) = self.0.lock() {
            guard.rotate_if_needed();
            let _ = guard.file.write_all(buf);
            let _ = guard.file.flush();
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

static LOG_STATE: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<RotatingLogWriter>>> =
    std::sync::OnceLock::new();

struct StaticLogMakeWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for StaticLogMakeWriter {
    type Writer = SharedLogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        SharedLogWriter(LOG_STATE.get().expect("LOG_STATE not initialised").clone())
    }
}

pub fn init_logging(log_dir: &PathBuf, retention_days: u64) {
    let rotating = RotatingLogWriter::new(log_dir, retention_days)
        .expect("cannot open log file");
    LOG_STATE
        .set(std::sync::Arc::new(std::sync::Mutex::new(rotating)))
        .ok();

    tracing_subscriber::fmt()
        .with_writer(StaticLogMakeWriter)
        .with_timer(ChronoLocal::new("[%H:%M:%S]".into()))
        .with_target(false)
        .with_level(true)
        .with_ansi(false)
        .init();
}

// ---------------------------------------------------------------------------
// Parquet schema
// ---------------------------------------------------------------------------

fn parquet_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("ts",           DataType::Float64, false),
        Field::new("pid",          DataType::UInt32,  false),
        Field::new("ppid",         DataType::UInt32,  true),
        Field::new("name",         DataType::Utf8,    true),
        Field::new("user_name",    DataType::Utf8,    true),
        Field::new("status",       DataType::Utf8,    true),
        Field::new("cpu_percent",  DataType::Float32, true),
        Field::new("mem_rss",      DataType::UInt64,  false),
        Field::new("mem_vms",      DataType::UInt64,  false),
        Field::new("num_threads",  DataType::UInt64,  false),
        Field::new("create_time",  DataType::Float64, false),
        Field::new("cwd",          DataType::Utf8,    true),
        Field::new("num_fds",      DataType::UInt32,  true),
        Field::new("children",     DataType::Utf8,    true),
    ]))
}

// ---------------------------------------------------------------------------
// Write a batch of records to a new Parquet file
// ---------------------------------------------------------------------------

fn write_parquet(path: &PathBuf, records: &[ProcRecord]) -> Result<(), Box<dyn std::error::Error>> {
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
            Arc::new(Float64Array::from(records.iter().map(|r| r.ts).collect::<Vec<_>>())),
            Arc::new(UInt32Array::from(records.iter().map(|r| r.pid).collect::<Vec<_>>())),
            Arc::new(UInt32Array::from(records.iter().map(|r| r.ppid).collect::<Vec<_>>())),
            Arc::new(StringArray::from(
                records.iter().map(|r| Some(r.name.as_str())).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records.iter().map(|r| r.user.as_deref()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records.iter().map(|r| Some(r.status.as_str())).collect::<Vec<_>>(),
            )),
            Arc::new(Float32Array::from(
                records.iter().map(|r| Some(r.cpu_percent)).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(records.iter().map(|r| r.mem_rss).collect::<Vec<_>>())),
            Arc::new(UInt64Array::from(records.iter().map(|r| r.mem_vms).collect::<Vec<_>>())),
            Arc::new(UInt64Array::from(records.iter().map(|r| r.num_threads).collect::<Vec<_>>())),
            Arc::new(Float64Array::from(records.iter().map(|r| r.create_time).collect::<Vec<_>>())),
            Arc::new(StringArray::from(
                records.iter().map(|r| r.cwd.as_deref()).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                records.iter().map(|r| r.num_fds).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                children_strs.iter().map(|s| Some(s.as_str())).collect::<Vec<_>>(),
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
    proc_: &Process,
    ts: f64,
    user_cache: &mut UserCache,
    children: &[u32],
    slow: Option<&SlowFields>,
) -> ProcRecord {
    let ppid = proc_.parent().map(|p| p.as_u32());
    let uid = proc_.user_id().map(|u| **u);
    let user = uid.and_then(|u| user_cache.lookup(u));
    let status = format!("{:?}", proc_.status()).to_lowercase();
    let (cwd, num_fds) = match slow {
        Some(s) => (s.cwd.clone(), s.num_fds),
        None => (None, None),
    };

    ProcRecord {
        ts,
        pid,
        ppid,
        name: proc_.name().to_string_lossy().into_owned(),
        user,
        status,
        cpu_percent: proc_.cpu_usage(),
        mem_rss: proc_.memory(),
        mem_vms: proc_.virtual_memory(),
        num_threads: proc_.tasks().map(|t| t.len() as u64 + 1).unwrap_or(1),
        create_time: proc_.start_time() as f64,
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

pub fn collect_loop(cfg: CollectConfig) {
    let data_dir = cfg.data_dir.clone();

    info!("macos-proc-monitor collection starting");
    info!("log  -> {}/monitor-<date>.log", cfg.log_dir.display());
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
        if cfg.data_retention == 0 { "inf".to_string() } else { cfg.data_retention.to_string() },
        if cfg.log_retention == 0 { "inf".to_string() } else { cfg.log_retention.to_string() },
    );

    let interval     = Duration::from_secs_f64(cfg.interval.max(0.1));
    let slow_every   = Duration::from_secs(cfg.slow_interval.max(1));
    let flush_every  = Duration::from_secs(cfg.flush_interval.max(1));

    let refresh_kind = RefreshKind::nothing().with_processes(
        ProcessRefreshKind::everything()
            .with_cpu()
            .with_memory()
            .with_user(UpdateKind::Always),
    );

    let mut sys = System::new_with_specifics(refresh_kind);
    let mut user_cache = UserCache::new();
    let mut last_slow  = Instant::now()
        .checked_sub(slow_every)
        .unwrap_or_else(Instant::now);
    let mut slow_cache: HashMap<u32, SlowFields> = HashMap::new();

    // Parquet buffer
    static FLUSH_COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut buffer: Vec<ProcRecord> = Vec::new();
    let mut last_flush     = Instant::now().checked_sub(flush_every).unwrap_or_else(Instant::now);
    let mut current_hour_str = current_hour();

    // For daily retention purge
    let mut last_retention_date = Local::now().format("%Y-%m-%d").to_string();

    loop {
        let tick_start = Instant::now();
        let ts = unix_now();

        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::everything());

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
            let proc_ = match sys.process(spid) {
                Some(p) => p,
                None => continue,
            };

            if do_slow {
                let slow_fields = collect_slow_for_pid(*pid, cfg.sudo);
                slow_cache.insert(*pid, slow_fields);
            }

            let slow_ref = slow_cache.get(pid);
            let children = children_map.get(pid).cloned().unwrap_or_default();
            let record = build_record(*pid, proc_, ts, &mut user_cache, &children, slow_ref);
            buffer.push(record);
            tick_count += 1;
        }

        // Purge stale slow cache entries
        slow_cache.retain(|pid, _| sys.process(Pid::from_u32(*pid)).is_some());

        // Flush to Parquet if the interval elapsed or the hour changed
        let new_hour  = current_hour();
        let hour_changed = new_hour != current_hour_str;
        let time_to_flush = tick_start.duration_since(last_flush) >= flush_every;

        if (time_to_flush || hour_changed) && !buffer.is_empty() {
            // Flush records accumulated so far (they belong to current_hour_str)
            let flush_hour = current_hour_str.clone();
            let seq = FLUSH_COUNTER.fetch_add(1, Ordering::Relaxed);
            let filename = format!("{}_{:06}.parquet", flush_hour, seq);
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

        // Daily retention purge of parquet files
        if cfg.data_retention > 0 {
            let today = Local::now().format("%Y-%m-%d").to_string();
            if today != last_retention_date {
                last_retention_date = today;
                purge_old_files(&data_dir, "parquet", cfg.data_retention);
                info!("Parquet retention purge: removed files older than {}d", cfg.data_retention);
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

        let elapsed = tick_start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}
