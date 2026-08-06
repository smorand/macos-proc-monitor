//! macos-proc-monitor — collects per-process metrics every second, writes to DuckDB.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use chrono::Local;
use clap::Parser;
use duckdb::{params, Connection};
use serde::Serialize;
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};
use tracing::{debug, error, info};
use tracing_subscriber::fmt::time::ChronoLocal;
use uzers::get_user_by_uid;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "macos-proc-monitor",
    about = "Collect per-process metrics every second, write to DuckDB",
    long_about = None,
    version
)]
struct Args {
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
}

// ---------------------------------------------------------------------------
// Record struct (used for building, not serialized to JSON for writing)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
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

fn default_dir(env_var: &str, subdir: &str) -> PathBuf {
    if let Ok(v) = std::env::var(env_var) {
        return PathBuf::from(v);
    }
    // Under launchd, HOME may be unset — fall back to /var/db/macos-proc-monitor
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
// Purge log files older than retention_days
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

fn init_logging(log_dir: &PathBuf, retention_days: u64) {
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
// Filter: if --pid is set, collect only that pid + descendants
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
// Main loop
// ---------------------------------------------------------------------------

fn main() {
    let args = Args::parse();

    let log_dir = default_dir("MACOS_PROC_MONITOR_FOLDER_LOG", "logs");
    let data_dir = default_dir("MACOS_PROC_MONITOR_FOLDER_DATA", "data");

    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!("Cannot create log dir {:?}: {e}", log_dir);
    }
    if let Err(e) = fs::create_dir_all(&data_dir) {
        eprintln!("Cannot create data dir {:?}: {e}", data_dir);
    }

    init_logging(&log_dir, args.log_retention);

    // --- open DuckDB ---
    let db_path = data_dir.join("procs.duckdb");
    let mut conn = Connection::open(&db_path).expect("cannot open DuckDB");

    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS proc_metrics (
            ts           DOUBLE NOT NULL,
            pid          UINTEGER NOT NULL,
            ppid         UINTEGER,
            name         VARCHAR,
            user_name    VARCHAR,
            status       VARCHAR,
            cpu_percent  FLOAT,
            mem_rss      UBIGINT,
            mem_vms      UBIGINT,
            num_threads  UBIGINT,
            create_time  DOUBLE,
            cwd          VARCHAR,
            num_fds      UINTEGER,
            children     VARCHAR
        );
    ").expect("cannot create table");

    info!("macos-proc-monitor starting");
    info!("log  -> {}/monitor-<date>.log", log_dir.display());
    info!("data -> {}", db_path.display());
    info!(
        "interval={}s  slow-interval={}s  sudo={}  no-slow={}",
        args.interval, args.slow_interval, args.sudo, args.no_slow
    );
    info!(
        "retention: data={}d  logs={}d",
        if args.data_retention == 0 { "inf".to_string() } else { args.data_retention.to_string() },
        if args.log_retention == 0 { "inf".to_string() } else { args.log_retention.to_string() },
    );

    let interval = Duration::from_secs_f64(args.interval.max(0.1));
    let slow_every = Duration::from_secs(args.slow_interval.max(1));

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

    // For daily retention purge
    let mut last_retention_date = Local::now().format("%Y-%m-%d").to_string();

    loop {
        let tick_start = Instant::now();
        let ts = unix_now();

        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::everything());

        let children_map = build_children_map(&sys);

        let do_slow = !args.no_slow && tick_start.duration_since(last_slow) >= slow_every;
        if do_slow {
            last_slow = tick_start;
        }

        let all_pids: Vec<u32> = sys.processes().keys().map(|p| p.as_u32()).collect();
        let target_pids: Vec<u32> = if let Some(root) = args.pid {
            reachable_pids(root, &children_map)
        } else {
            all_pids
        };

        // build records
        let mut records: Vec<ProcRecord> = Vec::with_capacity(target_pids.len());
        for pid in &target_pids {
            let spid = Pid::from_u32(*pid);
            let proc_ = match sys.process(spid) {
                Some(p) => p,
                None => continue,
            };

            if do_slow {
                let slow_fields = collect_slow_for_pid(*pid, args.sudo);
                slow_cache.insert(*pid, slow_fields);
            }

            let slow_ref = slow_cache.get(pid);
            let children = children_map.get(pid).cloned().unwrap_or_default();
            let record = build_record(*pid, proc_, ts, &mut user_cache, &children, slow_ref);
            records.push(record);
        }

        // write batch to DuckDB in one transaction
        let written = records.len();
        {
            let tx = conn.transaction().unwrap();
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO proc_metrics VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
                ).unwrap();
                for record in &records {
                    let children_str = serde_json::to_string(&record.children).unwrap_or_else(|_| "[]".into());
                    if let Err(e) = stmt.execute(params![
                        record.ts,
                        record.pid,
                        record.ppid,
                        record.name,
                        record.user,
                        record.status,
                        record.cpu_percent,
                        record.mem_rss,
                        record.mem_vms,
                        record.num_threads,
                        record.create_time,
                        record.cwd,
                        record.num_fds,
                        children_str,
                    ]) {
                        debug!("DuckDB insert error pid={}: {e}", record.pid);
                    }
                }
            }
            if let Err(e) = tx.commit() {
                error!("DuckDB commit error: {e}");
            }
        }

        // purge stale slow cache entries
        slow_cache.retain(|pid, _| sys.process(Pid::from_u32(*pid)).is_some());

        // daily retention purge
        if args.data_retention > 0 {
            let today = Local::now().format("%Y-%m-%d").to_string();
            if today != last_retention_date {
                last_retention_date = today;
                let sql = format!("DELETE FROM proc_metrics WHERE ts < extract(epoch FROM current_timestamp)::DOUBLE - {}::DOUBLE", args.data_retention as i64 * 86400);
                if let Err(e) = conn.execute(&sql, []) {
                    error!("DuckDB retention purge error: {e}");
                } else {
                    info!("DuckDB retention purge: removed rows older than {}d", args.data_retention);
                }
            }
        }

        let elapsed = tick_start.elapsed();
        info!(
            "{} procs | {}ms | slow={}",
            written,
            elapsed.as_millis(),
            if do_slow { "yes" } else { "no" }
        );

        let elapsed = tick_start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}
