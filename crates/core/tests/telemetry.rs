//! Integration test: telemetry init creates a rolling log file and the guard
//! flushes on drop. Runs in its own process so the global subscriber is fresh.

#[test]
fn init_creates_log_dir_and_file() {
    let tmp = tempfile::tempdir().unwrap();
    let log_dir = tmp.path().join("logs");

    let guard = procmon::init_telemetry(&log_dir).expect("telemetry init");
    tracing::info!("hello from telemetry test");

    // Dropping the guard flushes the non-blocking writer.
    drop(guard);

    assert!(log_dir.is_dir(), "log dir should be created");
    let entries: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x == "log")
        })
        .collect();
    assert!(
        !entries.is_empty(),
        "a monitor.<date>.log file should exist"
    );
}
