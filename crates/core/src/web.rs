//! Web dashboard: Axum server reading Parquet files via in-memory DuckDB.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::VERSION;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct AppState {
    data_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Query param structs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct WindowParams {
    #[serde(default = "default_window")]
    window: i64,
    user: Option<String>,
}

fn default_window() -> i64 {
    300
}

#[derive(Deserialize)]
struct TopParams {
    #[serde(default = "default_window")]
    window: i64,
    #[serde(default = "default_limit")]
    limit: i64,
    user: Option<String>,
}

fn default_limit() -> i64 {
    10
}

#[derive(Deserialize)]
struct TimelineParams {
    pid: i64,
    #[serde(default = "default_timeline_window")]
    window: i64,
}

fn default_timeline_window() -> i64 {
    3600
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SummaryResponse {
    active_procs: i64,
    total_cpu: f64,
    total_rss: i64,
}

#[derive(Serialize)]
struct TopEntry {
    name: String,
    user: Option<String>,
    avg_cpu: f64,
    peak_rss: i64,
    instances: i64,
}

#[derive(Serialize)]
struct TimelineEntry {
    ts: f64,
    cpu_percent: f64,
    mem_rss: i64,
}

#[derive(Serialize)]
struct ProcessEntry {
    pid: i64,
    name: String,
    user: Option<String>,
    last_seen: f64,
}

// ---------------------------------------------------------------------------
// Error helper
// ---------------------------------------------------------------------------

struct AppError(String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Log the full detail server-side; return a generic body (never leak internals).
        tracing::error!(error = %self.0, "api request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "internal server error" })),
        )
            .into_response()
    }
}

impl From<duckdb::Error> for AppError {
    fn from(e: duckdb::Error) -> Self {
        AppError(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Open in-memory DuckDB and create a view over all parquet files
// ---------------------------------------------------------------------------

fn open_db(data_dir: &Path) -> Result<Connection, duckdb::Error> {
    let conn = Connection::open_in_memory()?;
    // Glob matches all parquet files written by the monitor (YYYY-MM-DDTHH_NNNNNN.parquet)
    let pattern = data_dir.join("*.parquet").to_string_lossy().to_string();
    conn.execute_batch(&format!(
        "CREATE VIEW proc_metrics AS \
         SELECT * FROM read_parquet('{pattern}', union_by_name=true);",
    ))?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Compute cutoff timestamp (unix epoch as f64)
// ---------------------------------------------------------------------------

fn cutoff(window_secs: i64) -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    // Window seconds are small; f64 represents them exactly.
    #[allow(clippy::cast_precision_loss)]
    let window = window_secs as f64;
    now - window
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

/// Liveness: process is up. Never touches dependencies.
async fn health_live() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "version": VERSION }))
}

/// Readiness: process can serve queries (data dir reachable via DuckDB).
async fn health_ready(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match open_db(&state.data_dir) {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ready", "version": VERSION })),
        ),
        Err(e) => {
            tracing::error!(error = %e, "readiness check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "status": "unavailable", "version": VERSION })),
            )
        }
    }
}

fn user_clause(user: Option<&str>) -> String {
    match user {
        Some("__system") => r" AND user_name LIKE '\_%'".to_string(),
        Some(u) if !u.is_empty() => format!(" AND user_name = '{}'", u.replace('\'', "''")),
        _ => String::new(),
    }
}

async fn api_summary(
    State(state): State<Arc<AppState>>,
    Query(params): Query<WindowParams>,
) -> Result<Json<SummaryResponse>, AppError> {
    let conn = open_db(&state.data_dir)?;
    let uf = user_clause(params.user.as_deref());
    let sql = format!(
        "SELECT
            count(DISTINCT pid) as active_procs,
            coalesce(sum(cpu_percent), 0.0) as total_cpu,
            coalesce(sum(mem_rss), 0) as total_rss
         FROM (
             SELECT pid, avg(cpu_percent) as cpu_percent, max(mem_rss) as mem_rss
             FROM proc_metrics
             WHERE ts > {}{}
             GROUP BY pid
         )",
        cutoff(params.window),
        uf
    );
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt.query_row([], |r| {
        Ok(SummaryResponse {
            active_procs: r.get(0)?,
            total_cpu: r.get(1)?,
            total_rss: r.get(2)?,
        })
    })?;
    Ok(Json(row))
}

async fn api_top_cpu(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TopParams>,
) -> Result<Json<Vec<TopEntry>>, AppError> {
    let conn = open_db(&state.data_dir)?;
    let uf = user_clause(params.user.as_deref());
    let sql = format!(
        "SELECT name, user_name,
                round(avg(cpu_percent), 2) as avg_cpu,
                max(mem_rss) as peak_rss,
                count(DISTINCT pid) as instances
         FROM proc_metrics
         WHERE ts > {}{}
         GROUP BY name, user_name
         ORDER BY avg_cpu DESC
         LIMIT {}",
        cutoff(params.window),
        uf,
        params.limit
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(TopEntry {
            name: r.get(0)?,
            user: r.get(1)?,
            avg_cpu: r.get(2)?,
            peak_rss: r.get(3)?,
            instances: r.get(4)?,
        })
    })?;
    let entries: Result<Vec<_>, _> = rows.collect();
    Ok(Json(entries?))
}

async fn api_top_mem(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TopParams>,
) -> Result<Json<Vec<TopEntry>>, AppError> {
    let conn = open_db(&state.data_dir)?;
    let uf = user_clause(params.user.as_deref());
    let sql = format!(
        "SELECT name, user_name,
                round(avg(cpu_percent), 2) as avg_cpu,
                max(mem_rss) as peak_rss,
                count(DISTINCT pid) as instances
         FROM proc_metrics
         WHERE ts > {}{}
         GROUP BY name, user_name
         ORDER BY peak_rss DESC
         LIMIT {}",
        cutoff(params.window),
        uf,
        params.limit
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(TopEntry {
            name: r.get(0)?,
            user: r.get(1)?,
            avg_cpu: r.get(2)?,
            peak_rss: r.get(3)?,
            instances: r.get(4)?,
        })
    })?;
    let entries: Result<Vec<_>, _> = rows.collect();
    Ok(Json(entries?))
}

async fn api_timeline(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TimelineParams>,
) -> Result<Json<Vec<TimelineEntry>>, AppError> {
    let conn = open_db(&state.data_dir)?;
    let sql = format!(
        "SELECT ts, round(cpu_percent, 2) as cpu_percent, mem_rss
         FROM proc_metrics
         WHERE pid = {} AND ts > {}
         ORDER BY ts",
        params.pid,
        cutoff(params.window)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(TimelineEntry {
            ts: r.get(0)?,
            cpu_percent: r.get(1)?,
            mem_rss: r.get(2)?,
        })
    })?;
    let entries: Result<Vec<_>, _> = rows.collect();
    Ok(Json(entries?))
}

async fn api_users(
    State(state): State<Arc<AppState>>,
    Query(params): Query<WindowParams>,
) -> Result<Json<Vec<String>>, AppError> {
    let conn = open_db(&state.data_dir)?;
    let sql = format!(
        "SELECT DISTINCT coalesce(user_name, '') as user_name
         FROM proc_metrics
         WHERE ts > {}
         ORDER BY user_name",
        cutoff(params.window)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let entries: Result<Vec<_>, _> = rows.collect();
    Ok(Json(entries?))
}

async fn api_processes(
    State(state): State<Arc<AppState>>,
    Query(params): Query<WindowParams>,
) -> Result<Json<Vec<ProcessEntry>>, AppError> {
    let conn = open_db(&state.data_dir)?;
    let uf = user_clause(params.user.as_deref());
    let sql = format!(
        "SELECT DISTINCT pid, name, user_name, max(ts) as last_seen
         FROM proc_metrics
         WHERE ts > {}{}
         GROUP BY pid, name, user_name
         ORDER BY name",
        cutoff(params.window),
        uf
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(ProcessEntry {
            pid: r.get(0)?,
            name: r.get(1)?,
            user: r.get(2)?,
            last_seen: r.get(3)?,
        })
    })?;
    let entries: Result<Vec<_>, _> = rows.collect();
    Ok(Json(entries?))
}

// ---------------------------------------------------------------------------
// Router construction (shared by the server and e2e tests)
// ---------------------------------------------------------------------------

/// Build the API router. Data queries are served under both `/api/v1/*`
/// (versioned, per the standard) and the legacy `/api/*` paths the embedded
/// dashboard calls.
pub fn router(data_dir: PathBuf) -> Router {
    let state = Arc::new(AppState { data_dir });

    let api = Router::new()
        .route("/summary", get(api_summary))
        .route("/top", get(api_top_cpu))
        .route("/top-mem", get(api_top_mem))
        .route("/timeline", get(api_timeline))
        .route("/processes", get(api_processes))
        .route("/users", get(api_users));

    Router::new()
        .route("/", get(index))
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .route("/health", get(health_live))
        .nest("/api/v1", api.clone())
        .nest("/api", api)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Serve the web dashboard (async — run on the tokio runtime)
// ---------------------------------------------------------------------------

pub async fn serve_web(bind: String, port: u16, data_dir: PathBuf) -> anyhow::Result<()> {
    info!("Web dashboard data dir: {}", data_dir.display());
    info!("Web dashboard listening on {}:{}", bind, port);

    let app = router(data_dir);

    let addr: std::net::SocketAddr = format!("{bind}:{port}")
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address {bind}:{port}: {e}"))?;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(crate::shutdown_signal())
        .await?;
    Ok(())
}
