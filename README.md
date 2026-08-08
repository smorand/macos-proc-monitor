# macos-proc-monitor

Single daemon for macOS that does two things at once:

- **Collects** per-process metrics for every running process each second and writes them to hourly partitioned Parquet files. Every 60 seconds (configurable) it also collects `cwd` and `num_fds` via `lsof`.
- **Serves** a built-in web dashboard (Axum + DuckDB) that reads those Parquet files. No separate binary: the same process that collects also serves the UI at `http://127.0.0.1:9090`.

The collection loop runs on a dedicated blocking thread; the web server runs on the tokio runtime. Both share the same data directory.

## Architecture

Cargo workspace with two crates:

- `crates/core` (lib `procmon`, package `macos-proc-core`): collection (`collect.rs`), web dashboard (`web.rs`), typed config (`config.rs`, figment + XDG dirs), telemetry (`telemetry.rs`, tracing + rolling file logs), and error types (`error.rs`, thiserror). The dashboard HTML lives in `crates/core/static/index.html` (embedded via `include_str!`).
- `crates/macos-proc-monitor` (bin `macos-proc-monitor`): thin entry point (sync `main -> ExitCode`, async `run`), loads config, resolves data/log dirs, spawns the collection thread, then serves the web dashboard with graceful shutdown.

Only one binary is produced: `macos-proc-monitor`.

## Install

```bash
make install         # build release + install binary + sudoers + register/load launchd daemon (sudo), serves :9090
make uninstall       # unload daemon + remove binary + sudoers + plist (sudo)
```

Requires Rust (edition 2024, MSRV 1.87) and Cargo. The toolchain is pinned via `rust-toolchain.toml`. DuckDB is bundled (compiled from source on first build, expect a longer initial build).

### Development

```bash
make check     # full quality gate: format-check, clippy -D warnings, typecheck, security (audit + deny), coverage >= 80%, doc
make test      # run all tests (unit + integration + e2e)
make lint      # clippy with warnings denied
make format    # rustfmt
```

Run `make check` before every commit.

## Usage

```
macos-proc-monitor [OPTIONS]

Collection options:
  --interval <secs>       Sampling interval (default: 1.0)
  --slow-interval <n>     Seconds between cwd+fds collection (default: 60)
  --sudo                  Prefix lsof with sudo for privileged processes
  --no-slow               Never collect cwd/fds (faster, lower overhead)
  --pid <pid>             Monitor only this PID and its children
  --data-retention <d>    Delete data files older than d days (default: 7, 0 = keep forever)
  --log-retention <d>     Delete log files older than d days (default: 7, 0 = keep forever)
  --flush-interval <s>    Seconds between Parquet flushes (default: 30)

Web dashboard options:
  --port <port>           Dashboard TCP port (default: 9090)
  --bind <addr>           Dashboard bind address (default: 127.0.0.1)

  --config <path>         Optional TOML config file (default: XDG config dir)
  -h, --help              Print help
  -V, --version           Print version
```

Flags are optional: each falls back to the config file, then env vars
(`MACOS_PROC_MONITOR_*`), then built-in defaults. A flag passed on the command
line always wins.

The web dashboard is always served; there is no way to disable it. Open `http://127.0.0.1:9090` once the daemon is running.

### Examples

```bash
# Standard run — all processes, 1s interval, dashboard on http://127.0.0.1:9090
macos-proc-monitor

# High-frequency, no slow collection
macos-proc-monitor --no-slow --interval 0.5

# Monitor only PID 1234 and its children
macos-proc-monitor --pid 1234

# Serve the dashboard on a different port / interface
macos-proc-monitor --bind 0.0.0.0 --port 8080

# Privileged mode (collect cwd/fds for all processes including root-owned)
# Requires: %admin ALL=(ALL) NOPASSWD: /usr/bin/lsof  in /etc/sudoers
sudo macos-proc-monitor --sudo
```

## Configuration

Settings resolve in this order, later layers overriding earlier ones:

1. Built-in defaults.
2. TOML file: `--config <path>`, or `~/.config/macos-proc-monitor/config.toml` (a missing file is ignored). Keys match the field names, e.g. `port = 9090`, `interval = 1.0`, `no_slow = true`.
3. Environment variables prefixed `MACOS_PROC_MONITOR_` (e.g. `MACOS_PROC_MONITOR_PORT=8080`).
4. CLI flags.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `MACOS_PROC_MONITOR_FOLDER_LOG` | `~/.cache/macos-proc-monitor/logs/` | Directory for log files (overrides all) |
| `MACOS_PROC_MONITOR_FOLDER_DATA` | `~/.cache/macos-proc-monitor/data/` | Directory for Parquet data files (overrides all) |
| `MACOS_PROC_MONITOR_PORT` | `9090` | Dashboard TCP port |
| `MACOS_PROC_MONITOR_BIND` | `127.0.0.1` | Dashboard bind address |
| `RUST_LOG` | `info` | Log verbosity (tracing `EnvFilter` syntax) |

Directory resolution: the `_FOLDER_*` env vars win; otherwise the XDG cache dir
(`~/.cache/macos-proc-monitor/{data,logs}`); otherwise `/var/db/macos-proc-monitor/{data,logs}`
(the launchd daemon runs as root and sets the env vars explicitly).

Both directories are created automatically on first run.

Data files are named `YYYY-MM-DDTHH_NNNNNN.parquet` (one hour partition, sequential flushes).
Log files are named `monitor.YYYY-MM-DD.log` and roll daily (tracing-appender).

## Parquet schema

Each row is one process at one tick:

| Column | Type | Notes |
|---|---|---|
| `ts` | f64 | Unix epoch seconds |
| `pid` | u32 | |
| `ppid` | u32 (nullable) | |
| `name` | utf8 | |
| `user_name` | utf8 (nullable) | Resolved from uid |
| `status` | utf8 | |
| `cpu_percent` | f32 (nullable) | 0.0 on the first tick (needs two samples) |
| `mem_rss` | u64 | Resident set size (bytes) |
| `mem_vms` | u64 | Virtual memory size (bytes) |
| `num_threads` | u64 | |
| `create_time` | f64 | Process start time |
| `cwd` | utf8 (nullable) | Only on slow ticks; null otherwise or if lsof fails |
| `num_fds` | u32 (nullable) | Same as `cwd` |
| `children` | utf8 (nullable) | JSON array of direct child PIDs |

## Web dashboard API

The daemon exposes these JSON endpoints (all read from the Parquet data via an in-memory DuckDB):

| Route | Description |
|---|---|
| `GET /` | Dashboard HTML |
| `GET /health/live` | Liveness (always OK) + version |
| `GET /health/ready` | Readiness: 200 if the data dir is queryable, else 503 |
| `GET /health` | Alias for `/health/live` |
| `GET /api/summary?window=<s>&user=<u>` | Active procs, total CPU, total RSS over the window |
| `GET /api/top?window=<s>&limit=<n>&user=<u>` | Top processes by average CPU |
| `GET /api/top-mem?window=<s>&limit=<n>&user=<u>` | Top processes by peak RSS |
| `GET /api/timeline?pid=<p>&window=<s>` | CPU/RSS timeline for one PID |
| `GET /api/processes?window=<s>&user=<u>` | Distinct processes seen in the window |
| `GET /api/users?window=<s>` | Distinct users seen in the window |

Data endpoints are also served under the versioned prefix `/api/v1/*` (e.g.
`/api/v1/summary`); the embedded dashboard uses the legacy `/api/*` paths.
5xx responses return a sanitized `{"error":"internal server error"}` body; the
full error is logged server-side.

`user=__system` filters to system users (names starting with `_`).

## Ad-hoc DuckDB queries

```sql
-- brew install duckdb

-- Top 10 CPU hogs in the last 5 minutes
SELECT name, pid, avg(cpu_percent) AS avg_cpu, max(mem_rss)/1e6 AS max_rss_mb
FROM read_parquet('~/.cache/macos-proc-monitor/data/*.parquet', union_by_name=true)
WHERE ts > epoch(now()) - 300
GROUP BY name, pid
ORDER BY avg_cpu DESC
LIMIT 10;

-- Memory growth over time for a specific PID
SELECT to_timestamp(ts)::TIMESTAMP AS t, mem_rss/1e6 AS rss_mb
FROM read_parquet('~/.cache/macos-proc-monitor/data/*.parquet', union_by_name=true)
WHERE pid = 1234
ORDER BY ts;
```

## Daemon (launchd)

`make install` installs `launchd/com.smorand.macos-proc-monitor.plist`, which runs the daemon as root with:

- data → `/var/db/macos-proc-monitor/data`, logs → `/var/db/macos-proc-monitor/logs`
- `--data-retention 7 --log-retention 7 --port 9090 --bind 127.0.0.1`
- `RunAtLoad` + `KeepAlive` (starts at boot, auto-restarts on crash)

Manage it with `make daemon-start`, `make daemon-stop`, `make daemon-status`, `make uninstall`.

## Sudo mode

For full `cwd` and `num_fds` coverage on processes owned by other users or root, run with `--sudo`. This requires passwordless sudo for `/usr/bin/lsof`. Add via `sudo visudo`:

```
%admin ALL=(ALL) NOPASSWD: /usr/bin/lsof
```

The launchd daemon already runs as root, so it does not need `--sudo`.

## Performance

With `--no-slow`, each tick costs one `sysinfo` refresh plus buffering. On a typical macOS system with ~500-600 processes at 1s interval, expect < 30ms per tick. Parquet is far more compact than JSONL.

With slow collection enabled (default every 60s), each slow tick spawns `lsof` twice per visible process. That slow tick is CPU-intensive but has no impact on normal ticks. The web dashboard opens a fresh in-memory DuckDB per request, so query cost scales with the number of Parquet files in the window.
