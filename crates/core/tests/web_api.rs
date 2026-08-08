//! End-to-end tests for the web dashboard API: spawn the real Axum router on
//! an ephemeral port against a temp data dir containing a real Parquet file,
//! and drive it with a real reqwest client.

use std::sync::Arc;

use arrow::array::{Float32Array, Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use procmon::web;

/// Schema mirroring the collector's Parquet output.
fn schema() -> Arc<Schema> {
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

/// Write one Parquet file with two sample rows into `dir`.
fn write_sample(dir: &std::path::Path, now: f64) {
    let schema = schema();
    let path = dir.join("2025-01-01T00_000000.parquet");
    let file = std::fs::File::create(&path).unwrap();
    let mut writer = ArrowWriter::try_new(file, schema.clone(), None).unwrap();

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Float64Array::from(vec![now, now])),
            Arc::new(UInt32Array::from(vec![100u32, 200u32])),
            Arc::new(UInt32Array::from(vec![Some(1u32), Some(1u32)])),
            Arc::new(StringArray::from(vec![Some("alpha"), Some("beta")])),
            Arc::new(StringArray::from(vec![
                Some("sebastien"),
                Some("_spotlight"),
            ])),
            Arc::new(StringArray::from(vec![Some("running"), Some("running")])),
            Arc::new(Float32Array::from(vec![Some(10.0f32), Some(20.0f32)])),
            Arc::new(UInt64Array::from(vec![1000u64, 2000u64])),
            Arc::new(UInt64Array::from(vec![5000u64, 6000u64])),
            Arc::new(UInt64Array::from(vec![2u64, 3u64])),
            Arc::new(Float64Array::from(vec![now - 100.0, now - 200.0])),
            Arc::new(StringArray::from(vec![Some("/tmp"), None])),
            Arc::new(UInt32Array::from(vec![Some(3u32), None])),
            Arc::new(StringArray::from(vec![Some("[]"), Some("[]")])),
        ],
    )
    .unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn now_secs() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

async fn spawn(dir: std::path::PathBuf) -> String {
    let app = web::router(dir);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn health_live_returns_version() {
    let tmp = tempfile::tempdir().unwrap();
    let base = spawn(tmp.path().to_path_buf()).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/health/live"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], procmon::VERSION);
}

#[tokio::test]
async fn summary_aggregates_parquet_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let now = now_secs();
    write_sample(tmp.path(), now);
    let base = spawn(tmp.path().to_path_buf()).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/summary?window=3600"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["active_procs"], 2);
    // total_cpu = avg per pid summed = 10 + 20 = 30
    assert!((body["total_cpu"].as_f64().unwrap() - 30.0).abs() < 0.001);
    // total_rss = max per pid summed = 1000 + 2000 = 3000
    assert_eq!(body["total_rss"], 3000);
}

#[tokio::test]
async fn versioned_and_legacy_api_paths_both_work() {
    let tmp = tempfile::tempdir().unwrap();
    let now = now_secs();
    write_sample(tmp.path(), now);
    let base = spawn(tmp.path().to_path_buf()).await;
    let client = reqwest::Client::new();

    for path in ["/api/summary", "/api/v1/summary"] {
        let resp = client
            .get(format!("{base}{path}?window=3600"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "path {path}");
    }
}

#[tokio::test]
async fn top_cpu_and_users_and_processes() {
    let tmp = tempfile::tempdir().unwrap();
    let now = now_secs();
    write_sample(tmp.path(), now);
    let base = spawn(tmp.path().to_path_buf()).await;
    let client = reqwest::Client::new();

    // top by cpu: beta (20) before alpha (10)
    let top: serde_json::Value = client
        .get(format!("{base}/api/top?window=3600&limit=10"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(top[0]["name"], "beta");

    // users list contains both users
    let users: Vec<String> = client
        .get(format!("{base}/api/users?window=3600"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(users.iter().any(|u| u == "sebastien"));
    assert!(users.iter().any(|u| u == "_spotlight"));

    // __system filter keeps only underscore users
    let sys: serde_json::Value = client
        .get(format!("{base}/api/processes?window=3600&user=__system"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = sys.as_array().unwrap();
    assert!(
        arr.iter()
            .all(|p| p["user"].as_str().unwrap().starts_with('_'))
    );
}

#[tokio::test]
async fn timeline_returns_points_for_pid() {
    let tmp = tempfile::tempdir().unwrap();
    let now = now_secs();
    write_sample(tmp.path(), now);
    let base = spawn(tmp.path().to_path_buf()).await;
    let client = reqwest::Client::new();

    let tl: serde_json::Value = client
        .get(format!("{base}/api/timeline?pid=100&window=3600"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = tl.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert!((arr[0]["cpu_percent"].as_f64().unwrap() - 10.0).abs() < 0.001);
}

#[tokio::test]
async fn top_mem_orders_by_peak_rss() {
    let tmp = tempfile::tempdir().unwrap();
    let now = now_secs();
    write_sample(tmp.path(), now);
    let base = spawn(tmp.path().to_path_buf()).await;
    let client = reqwest::Client::new();

    let top: serde_json::Value = client
        .get(format!("{base}/api/top-mem?window=3600&limit=10"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // beta has the larger peak_rss (2000) so it comes first.
    assert_eq!(top[0]["name"], "beta");
    assert_eq!(top[0]["peak_rss"], 2000);
}

#[tokio::test]
async fn empty_data_dir_yields_errors() {
    // No parquet files: DuckDB view creation fails, so data endpoints 500 and
    // readiness reports unavailable. Liveness stays OK.
    let tmp = tempfile::tempdir().unwrap();
    let base = spawn(tmp.path().to_path_buf()).await;
    let client = reqwest::Client::new();

    let live = client
        .get(format!("{base}/health/live"))
        .send()
        .await
        .unwrap();
    assert_eq!(live.status(), reqwest::StatusCode::OK);

    let ready = client
        .get(format!("{base}/health/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);

    let summary = client
        .get(format!("{base}/api/summary?window=60"))
        .send()
        .await
        .unwrap();
    assert_eq!(summary.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    let body: serde_json::Value = summary.json().await.unwrap();
    // 5xx body is sanitized: never leaks the internal error string.
    assert_eq!(body["error"], "internal server error");
}
