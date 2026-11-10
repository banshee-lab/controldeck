mod collector;
mod db;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use collector::{Sample, SystemInfo};
use rusqlite::Connection;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The compiled SvelteKit static export, embedded at build time.
#[derive(RustEmbed)]
#[folder = "web/build/"]
struct Assets;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    latest: Arc<Mutex<Option<Sample>>>,
}

#[derive(Serialize)]
struct Current {
    sample: Sample,
    system: SystemInfo,
}

#[derive(Deserialize)]
struct HistoryQuery {
    minutes: Option<i64>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

async fn current(State(state): State<AppState>) -> Json<Current> {
    let now = now_secs();
    let system = collector::read_system_info(now);
    let latest = lock(&state.latest).clone();
    let sample = latest
        .unwrap_or_else(|| collector::make_sample(now, 0.0, &system.memory, &system.disk));
    Json(Current { sample, system })
}

async fn history(
    State(state): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Vec<Sample>>, StatusCode> {
    let minutes = q.minutes.unwrap_or(30).clamp(1, 7 * 24 * 60);
    let since = now_secs() - minutes * 60;
    let conn = lock(&state.db);
    db::history(&conn, since)
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn asset_response(path: &str, file: rust_embed::EmbeddedFile) -> Response {
    let cache = if path.starts_with("_app/immutable/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, mime_guess::from_path(path).first_or_octet_stream().to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
        ],
        file.data.into_owned(),
    )
        .into_response()
}

/// Serves embedded files; unknown extension-less paths fall back to
/// `index.html` so client-side (SPA) routing works.
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path == "api" || path.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(f) = Assets::get(path) {
        return asset_response(path, f);
    }
    let last = path.rsplit('/').next().unwrap_or(path);
    if last.contains('.') {
        return StatusCode::NOT_FOUND.into_response();
    }
    match Assets::get("index.html") {
        Some(f) => asset_response("index.html", f),
        None => (StatusCode::NOT_FOUND, "frontend not built (run `make build`)").into_response(),
    }
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/api/metrics/current", get(current))
        .route("/api/metrics/history", get(history))
        .fallback(static_handler)
        .with_state(state)
}

fn default_db_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("db").join("metrics.db")))
        .unwrap_or_else(|| PathBuf::from("db/metrics.db"))
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() {
    let db_path = std::env::var("CONTROLDECK_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_db_path());
    let addr = std::env::var("CONTROLDECK_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let interval = env_u64("CONTROLDECK_INTERVAL_SECS", 5).max(1);

    let conn = db::init(&db_path).unwrap_or_else(|e| panic!("cannot open {}: {e}", db_path.display()));
    let _ = db::cleanup(&conn, now_secs());

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        latest: Arc::new(Mutex::new(None)),
    };

    // Collector: sample CPU/memory/disk every `interval` seconds.
    let s = state.clone();
    tokio::spawn(async move {
        let mut prev = collector::read_cpu_times();
        let mut tick = tokio::time::interval(Duration::from_secs(interval));
        tick.tick().await; // first tick completes immediately
        loop {
            tick.tick().await;
            let cur = collector::read_cpu_times();
            let cpu = match (prev, cur) {
                (Some(p), Some(c)) => collector::cpu_percent(p, c),
                _ => 0.0,
            };
            prev = cur;
            let sample = collector::make_sample(
                now_secs(),
                cpu,
                &collector::read_memory(),
                &collector::read_disk(),
            );
            if let Err(e) = db::insert(&lock(&s.db), &sample) {
                eprintln!("insert failed: {e}");
            }
            *lock(&s.latest) = Some(sample);
        }
    });

    // Retention: purge records older than 7 days every hour.
    let s = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(e) = db::cleanup(&lock(&s.db), now_secs()) {
                eprintln!("cleanup failed: {e}");
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("controldeck listening on http://{addr} (db: {})", db_path.display());
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let conn = Connection::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        AppState {
            db: Arc::new(Mutex::new(conn)),
            latest: Arc::new(Mutex::new(None)),
        }
    }

    async fn get_json(state: AppState, uri: &str) -> (StatusCode, serde_json::Value) {
        let res = app(state)
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn current_returns_sample_and_system() {
        let (status, v) = get_json(test_state(), "/api/metrics/current").await;
        assert_eq!(status, StatusCode::OK);
        assert!(v["sample"]["cpu"].is_number());
        assert!(v["system"]["memory"]["total"].as_u64().unwrap() > 0);
        assert_eq!(v["system"]["load_avg"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn history_filters_by_window() {
        let state = test_state();
        let now = now_secs();
        {
            let c = lock(&state.db);
            let mk = |ts| Sample { ts, cpu: 1.0, memory: 2.0, swap: 3.0, disk: 4.0 };
            db::insert(&c, &mk(now - 10)).unwrap();
            db::insert(&c, &mk(now - 3 * 3600)).unwrap();
        }
        let (status, v) = get_json(state.clone(), "/api/metrics/history?minutes=30").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v.as_array().unwrap().len(), 1);
        let (_, v) = get_json(state, "/api/metrics/history?minutes=600").await;
        assert_eq!(v.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn unknown_api_path_is_404() {
        let (status, _) = get_json(test_state(), "/api/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
