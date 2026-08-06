# macos-proc-monitor

Collects per-process metrics for every running process every second and writes one JSONL line per process per tick to a rolling daily file. Every 60 seconds (configurable) it also collects `cwd` and `num_fds` via `lsof`.

## Install

```bash
make install        # builds release binary, copies to ~/.local/bin
```

Requires Rust 1.75+ and Cargo. Binaries land in `~/.local/bin/macos-proc-monitor`.

## Usage

```
macos-proc-monitor [OPTIONS]

Options:
  --out <path>            Override data output file
  --log <path>            Override log file
  --interval <secs>       Sampling interval (default: 1.0)
  --slow-interval <n>     Seconds between cwd+fds collection (default: 60)
  --sudo                  Prefix lsof with sudo for privileged processes
  --no-slow               Never collect cwd/fds (faster, lower overhead)
  --pid <pid>             Monitor only this PID and its children
  -h, --help              Print help
  -V, --version           Print version
```

### Examples

```bash
# Standard run — all processes, 1s interval
macos-proc-monitor

# High-frequency, no slow collection
macos-proc-monitor --no-slow --interval 0.5

# Monitor only PID 1234 and its children
macos-proc-monitor --pid 1234

# Write to a specific file
macos-proc-monitor --out /tmp/procs.jsonl --no-slow

# Privileged mode (collect cwd/fds for all processes including root-owned)
# Requires: %admin ALL=(ALL) NOPASSWD: /usr/bin/lsof  in /etc/sudoers
sudo macos-proc-monitor --sudo
```

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `MACOS_PROC_MONITOR_FOLDER_LOG` | `~/.cache/macos-proc-monitor/logs/` | Directory for log files |
| `MACOS_PROC_MONITOR_FOLDER_DATA` | `~/.cache/macos-proc-monitor/data/` | Directory for data files |

Both directories are created automatically on first run.

Data files are named `procs-YYYY-MM-DD.jsonl` and rotate at midnight.  
Log files are named `monitor-YYYY-MM-DD.log`.

## JSONL schema

Each line is a JSON object:

```json
{
  "ts":           1234567890.123,
  "pid":          123,
  "ppid":         1,
  "name":         "node",
  "user":         "sebastien",
  "status":       "running",
  "cpu_percent":  12.5,
  "mem_rss":      589744,
  "mem_vms":      445167008,
  "num_threads":  24,
  "create_time":  1234567800.0,
  "cwd":          "/Users/sebastien/projects/foo",
  "num_fds":      42,
  "children":     [456, 789]
}
```

- `cwd` and `num_fds` are `null` on ticks where slow collection did not run, or when `lsof` fails (permission denied, process exited).
- `cpu_percent` is 0.0 on the first tick (sysinfo needs two samples).
- `children` lists direct child PIDs derived from the already-collected process list.

## DuckDB query examples

```sql
-- Install DuckDB: brew install duckdb

-- Top 10 CPU hogs in the last 5 minutes
SELECT name, pid, avg(cpu_percent) AS avg_cpu, max(mem_rss)/1e6 AS max_rss_mb
FROM read_ndjson_auto('~/.cache/macos-proc-monitor/data/procs-*.jsonl')
WHERE ts > epoch(now()) - 300
GROUP BY name, pid
ORDER BY avg_cpu DESC
LIMIT 10;

-- Memory growth over time for a specific PID
SELECT to_timestamp(ts)::TIMESTAMP AS t, mem_rss/1e6 AS rss_mb
FROM read_ndjson_auto('~/.cache/macos-proc-monitor/data/procs-*.jsonl')
WHERE pid = 1234
ORDER BY ts;

-- Processes with most open file descriptors
SELECT name, pid, max(num_fds) AS max_fds
FROM read_ndjson_auto('~/.cache/macos-proc-monitor/data/procs-*.jsonl')
WHERE num_fds IS NOT NULL
GROUP BY name, pid
ORDER BY max_fds DESC
LIMIT 20;
```

## Sudo mode

For full `cwd` and `num_fds` coverage on processes owned by other users or root, run with `--sudo`. This requires passwordless sudo for `/usr/bin/lsof`. Add to `/etc/sudoers` via `sudo visudo`:

```
%admin ALL=(ALL) NOPASSWD: /usr/bin/lsof
```

## Performance

With `--no-slow`, each tick costs one `sysinfo` refresh plus one JSON write per process. On a typical macOS system with ~500 processes at 1s interval, expect < 20ms per tick and ~ 50 MB/h of JSONL output.

With slow collection enabled (default every 60s), each slow tick spawns `lsof` twice per visible process. This is CPU-intensive for the slow tick but has no impact on normal ticks.
