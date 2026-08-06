//! macos-proc-monitor — collects per-process metrics every second, writes JSONL.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use chrono::Local;
use clap::Parser;
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
    about = "Collect per-process metrics every second, write JSONL",
    long_about = None,
    version
)]
struct Args {
    /// Data output file (default: ~/.cache/macos-proc-monitor/data/procs-YYYY-MM-DD.jsonl)
    #[arg(long)]
    out: Option<PathBuf>,

    /// Log file (default: ~/.cache/macos-proc-monitor/logs/monitor-YYYY-MM-DD.log)
    #[arg(long)]
    log: Option<PathBuf>,

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
}

// ---------------------------------------------------------------------------
// JSONL record
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
    // Output lines: p<pid>\nn<path>
    // We want the line starting with 'n'
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
    // Count non-empty lines (first line is header)
    let count = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    // Subtract 1 for header row; if 0 lines, return None
    if count == 0 {
        None
    } else {
        Some(count.saturating_sub(1) as u32)
    }
}

// ---------------------------------------------------------------------------
// Helpers: build child PID map from process list
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
// Helpers: collect slow fields (cwd + num_fds) for all pids
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
// Helpers: log + data file paths (date-rotated)
// ---------------------------------------------------------------------------

fn default_dir(env_var: &str, subdir: &str) -> PathBuf {
    if let Ok(v) = std::env::var(env_var) {
        PathBuf::from(v)
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
            .join(".cache")
            .join("macos-proc-monitor")
            .join(subdir)
    }
}

fn dated_path(dir: &PathBuf, prefix: &str, ext: &str) -> PathBuf {
    let today = Local::now().format("%Y-%m-%d").to_string();
    dir.join(format!("{prefix}-{today}.{ext}"))
}

// ---------------------------------------------------------------------------
// File writer with date rotation
// ---------------------------------------------------------------------------

struct RotatingWriter {
    dir: PathBuf,
    prefix: String,
    ext: String,
    current_date: String,
    writer: BufWriter<File>,
    override_path: Option<PathBuf>,
}

impl RotatingWriter {
    fn open(dir: PathBuf, prefix: &str, ext: &str, override_path: Option<PathBuf>) -> std::io::Result<Self> {
        let path = if let Some(ref p) = override_path {
            p.clone()
        } else {
            dated_path(&dir, prefix, ext)
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let current_date = Local::now().format("%Y-%m-%d").to_string();
        Ok(Self {
            dir,
            prefix: prefix.to_string(),
            ext: ext.to_string(),
            current_date,
            writer: BufWriter::new(file),
            override_path,
        })
    }

    /// Rotate if date changed (no-op when override_path is set).
    fn maybe_rotate(&mut self) -> std::io::Result<()> {
        if self.override_path.is_some() {
            return Ok(());
        }
        let today = Local::now().format("%Y-%m-%d").to_string();
        if today != self.current_date {
            let _ = self.writer.flush();
            let path = dated_path(&self.dir, &self.prefix, &self.ext);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            self.writer = BufWriter::new(file);
            self.current_date = today;
        }
        Ok(())
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        self.maybe_rotate()?;
        writeln!(self.writer, "{}", line)?;
        self.writer.flush()
    }
}

// ---------------------------------------------------------------------------
// Logging setup: tracing to stderr + file simultaneously
// ---------------------------------------------------------------------------

struct DualWriter {
    file: std::sync::Mutex<BufWriter<File>>,
}

impl DualWriter {
    fn new(path: &PathBuf) -> std::io::Result<Self> {
        if let Some(p) = path.parent() { fs::create_dir_all(p)?; }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file: std::sync::Mutex::new(BufWriter::new(file)) })
    }
}

impl std::io::Write for &DualWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Write to stderr
        let _ = std::io::stderr().write_all(buf);
        // Write to file
        if let Ok(mut guard) = self.file.lock() {
            let _ = guard.write_all(buf);
            let _ = guard.flush();
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

// tracing needs MakeWriter; we use a static reference trick
static LOG_WRITER: std::sync::OnceLock<DualWriter> = std::sync::OnceLock::new();

struct StaticDualMakeWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for StaticDualMakeWriter {
    type Writer = &'a DualWriter;
    fn make_writer(&'a self) -> Self::Writer {
        LOG_WRITER.get().expect("LOG_WRITER not initialised")
    }
}

fn init_logging(log_path: &PathBuf) {
    let writer = DualWriter::new(log_path).expect("cannot open log file");
    LOG_WRITER.set(writer).ok();

    tracing_subscriber::fmt()
        .with_writer(StaticDualMakeWriter)
        .with_timer(ChronoLocal::new("[%H:%M:%S]".into()))
        .with_target(false)
        .with_level(true)
        .with_ansi(false)
        .init();
}

// ---------------------------------------------------------------------------
// Build a ProcRecord from sysinfo data + optional slow fields
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

    // --- resolve paths ---
    let log_dir = default_dir("MACOS_PROC_MONITOR_FOLDER_LOG", "logs");
    let data_dir = default_dir("MACOS_PROC_MONITOR_FOLDER_DATA", "data");

    let log_path = args.log.clone().unwrap_or_else(|| dated_path(&log_dir, "monitor", "log"));
    let data_path_override = args.out.clone();

    // ensure dirs exist
    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!("Cannot create log dir {:?}: {e}", log_dir);
    }
    if let Err(e) = fs::create_dir_all(&data_dir) {
        eprintln!("Cannot create data dir {:?}: {e}", data_dir);
    }

    init_logging(&log_path);

    info!("macos-proc-monitor starting");
    info!("log  → {:?}", log_path);
    info!(
        "data → {:?}",
        data_path_override
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("{}/procs-<date>.jsonl", data_dir.display()))
    );
    info!(
        "interval={}s  slow-interval={}s  sudo={}  no-slow={}",
        args.interval, args.slow_interval, args.sudo, args.no_slow
    );

    let interval = Duration::from_secs_f64(args.interval.max(0.1));
    let slow_every = Duration::from_secs(args.slow_interval.max(1));

    // --- open data writer ---
    let mut data_writer = RotatingWriter::open(
        data_dir.clone(),
        "procs",
        "jsonl",
        data_path_override,
    )
    .expect("Cannot open data file");

    // --- sysinfo ---
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
        .unwrap_or_else(Instant::now); // trigger immediately on first tick

    // Slow-field cache: pid → SlowFields (reused across ticks)
    let mut slow_cache: HashMap<u32, SlowFields> = HashMap::new();

    loop {
        let tick_start = Instant::now();
        let ts = unix_now();

        // refresh processes
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::everything());

        let children_map = build_children_map(&sys);

        // should we do a slow collection this tick?
        let do_slow = !args.no_slow && tick_start.duration_since(last_slow) >= slow_every;
        if do_slow {
            last_slow = tick_start;
        }

        // determine which pids to emit
        let all_pids: Vec<u32> = sys.processes().keys().map(|p| p.as_u32()).collect();
        let target_pids: Vec<u32> = if let Some(root) = args.pid {
            reachable_pids(root, &children_map)
        } else {
            all_pids
        };

        let mut written = 0usize;

        for pid in &target_pids {
            let spid = Pid::from_u32(*pid);
            let proc_ = match sys.process(spid) {
                Some(p) => p,
                None => continue,
            };

            // slow collection
            if do_slow {
                let slow_fields = collect_slow_for_pid(*pid, args.sudo);
                slow_cache.insert(*pid, slow_fields);
            }

            // purge pids that no longer exist from cache (keep memory tidy)
            // (done below after the loop)

            let slow_ref = slow_cache.get(pid);
            let children = children_map.get(pid).cloned().unwrap_or_default();

            let record = build_record(*pid, proc_, ts, &mut user_cache, &children, slow_ref);

            match serde_json::to_string(&record) {
                Ok(line) => {
                    if let Err(e) = data_writer.write_line(&line) {
                        error!("Write error: {e}");
                    }
                    written += 1;
                }
                Err(e) => {
                    debug!("Serialise pid={pid} error: {e}");
                }
            }
        }

        // purge stale slow cache entries
        slow_cache.retain(|pid, _| sys.process(Pid::from_u32(*pid)).is_some());

        let elapsed = tick_start.elapsed();
        info!(
            "{} procs | {}ms | slow={}",
            written,
            elapsed.as_millis(),
            if do_slow { "yes" } else { "no" }
        );

        // rotate log path if needed (date changed)
        // Note: tracing's file handle is fixed; for true log rotation we'd need
        // more complex plumbing. Data file rotates correctly via RotatingWriter.
        if let Err(e) = data_writer.maybe_rotate() {
            error!("Data rotation error: {e}");
        }

        // sleep for remainder of interval
        let elapsed = tick_start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}
