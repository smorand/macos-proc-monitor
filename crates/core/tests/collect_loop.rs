//! Integration test: run the real collection loop briefly against a temp dir
//! and confirm it writes Parquet output. The loop is infinite by design, so it
//! runs on a detached thread and the test polls for output with a timeout.

use std::time::{Duration, Instant};

use procmon::{CollectConfig, Config};

#[test]
fn collect_loop_writes_parquet() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();

    let cfg = Config {
        // Fast, cheap: sample often, flush quickly, never touch lsof.
        interval: 0.2,
        flush_interval: 1,
        no_slow: true,
        data_retention: 0,
        log_retention: 0,
        ..Config::default()
    };
    let collect_cfg = CollectConfig::from_config(&cfg, data_dir.clone(), log_dir);

    // The loop never returns; run it detached and poll for output.
    std::thread::spawn(move || procmon::collect_loop(collect_cfg));

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut found = false;
    while Instant::now() < deadline {
        let has_parquet = std::fs::read_dir(&data_dir).is_ok_and(|rd| {
            rd.filter_map(Result::ok).any(|e| {
                e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| x == "parquet")
            })
        });
        if has_parquet {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    assert!(found, "collect_loop should write at least one parquet file");
}
