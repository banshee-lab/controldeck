//! HTTP API for the package manager abstraction (`/api/packages/*`).

use crate::pkgmgr::{self, CmdOutput, PackageManager, PkgError, Runner, SearchResult, SecuritySupport, SystemRunner, Update};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{Mutex, MutexGuard};

pub struct PackageService {
    distro: String,
    manager: Option<Arc<dyn PackageManager>>,
    runner: Arc<dyn Runner>,
    /// Serialises mutating operations (package managers hold their own locks).
    op_lock: Mutex<()>,
    /// Mutating endpoints disabled (`DECK_PKG_READONLY`).
    read_only: bool,
}

impl PackageService {
    /// Detects the distro and builds the matching manager.
    pub fn detect() -> Self {
        let d = pkgmgr::detect();
        let read_only = std::env::var("DECK_PKG_READONLY").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
        Self::new(d.distro_id, d.kind.map(|k| k.build()), Arc::new(SystemRunner), read_only)
    }

    pub fn new(distro: String, manager: Option<Arc<dyn PackageManager>>, runner: Arc<dyn Runner>, read_only: bool) -> Self {
        Self { distro, manager, runner, op_lock: Mutex::new(()), read_only }
    }

    pub fn manager_name(&self) -> Option<&'static str> {
        self.manager.as_ref().map(|m| m.name())
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

struct ApiError(StatusCode, String);

impl From<PkgError> for ApiError {
    fn from(e: PkgError) -> Self {
        let status = match e {
            PkgError::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
            PkgError::Invalid(_) => StatusCode::BAD_REQUEST,
            PkgError::Exec(_) | PkgError::ToolMissing(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

impl PackageService {
    fn manager(&self) -> Result<Arc<dyn PackageManager>, ApiError> {
        self.manager.clone().ok_or_else(|| {
            ApiError(
                StatusCode::NOT_IMPLEMENTED,
                format!("no supported package manager for distro '{}'", self.distro),
            )
        })
    }

    /// Gate for mutating operations: rejects in read-only mode and when another
    /// mutation is running. Hold the guard for the duration of the operation.
    fn begin_mutation(&self) -> Result<MutexGuard<'_, ()>, ApiError> {
        if self.read_only {
            return Err(ApiError(StatusCode::FORBIDDEN, "package modifications are disabled (DECK_PKG_READONLY)".into()));
        }
        self.op_lock
            .try_lock()
            .map_err(|_| ApiError(StatusCode::CONFLICT, "another package operation is in progress".into()))
    }
}

#[derive(Serialize)]
struct Info {
    distro: String,
    manager: Option<&'static str>,
    security_support: Option<SecuritySupport>,
    read_only: bool,
}

#[derive(Serialize)]
struct OpResponse {
    manager: &'static str,
    #[serde(flatten)]
    output: CmdOutput,
}

#[derive(Serialize)]
struct UpdatesResponse {
    manager: &'static str,
    security_support: SecuritySupport,
    count: usize,
    updates: Vec<Update>,
}

#[derive(Serialize)]
struct SearchResponse {
    manager: &'static str,
    query: String,
    count: usize,
    results: Vec<SearchResult>,
}

#[derive(Deserialize)]
struct UpdatesQuery {
    #[serde(default)]
    security: bool,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

#[derive(Deserialize)]
struct InstallUpdatesBody {
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    security_only: bool,
}

#[derive(Deserialize)]
struct InstallBody {
    packages: Vec<String>,
}

async fn info(State(svc): State<Arc<PackageService>>) -> Json<Info> {
    Json(Info {
        distro: svc.distro.clone(),
        manager: svc.manager_name(),
        security_support: svc.manager.as_ref().map(|m| m.security_support()),
        read_only: svc.read_only,
    })
}

async fn refresh(State(svc): State<Arc<PackageService>>) -> ApiResult<OpResponse> {
    let m = svc.manager()?;
    let _guard = svc.begin_mutation()?;
    let output = m.refresh(svc.runner.as_ref()).await?;
    Ok(Json(OpResponse { manager: m.name(), output }))
}

async fn list_updates(State(svc): State<Arc<PackageService>>, Query(q): Query<UpdatesQuery>) -> ApiResult<UpdatesResponse> {
    let m = svc.manager()?;
    let mut updates = m.list_updates(svc.runner.as_ref()).await?;
    if q.security {
        updates.retain(|u| u.security);
    }
    Ok(Json(UpdatesResponse { manager: m.name(), security_support: m.security_support(), count: updates.len(), updates }))
}

async fn install_updates(State(svc): State<Arc<PackageService>>, Json(body): Json<InstallUpdatesBody>) -> ApiResult<OpResponse> {
    pkgmgr::validate_packages(&body.packages)?;
    let m = svc.manager()?;
    let _guard = svc.begin_mutation()?;
    let output = m.install_updates(svc.runner.as_ref(), &body.packages, body.security_only).await?;
    Ok(Json(OpResponse { manager: m.name(), output }))
}

async fn search(State(svc): State<Arc<PackageService>>, Query(q): Query<SearchQuery>) -> ApiResult<SearchResponse> {
    pkgmgr::validate_query(&q.q)?;
    let m = svc.manager()?;
    let query = q.q.trim().to_string();
    let results = m.search(svc.runner.as_ref(), &query).await?;
    Ok(Json(SearchResponse { manager: m.name(), query, count: results.len(), results }))
}

async fn install(State(svc): State<Arc<PackageService>>, Json(body): Json<InstallBody>) -> ApiResult<OpResponse> {
    if body.packages.is_empty() {
        return Err(PkgError::Invalid("packages must not be empty".into()).into());
    }
    pkgmgr::validate_packages(&body.packages)?;
    let m = svc.manager()?;
    let _guard = svc.begin_mutation()?;
    let output = m.install(svc.runner.as_ref(), &body.packages).await?;
    Ok(Json(OpResponse { manager: m.name(), output }))
}

pub fn router<S: Clone + Send + Sync + 'static>(svc: Arc<PackageService>) -> Router<S> {
    Router::new()
        .route("/api/packages/info", get(info))
        .route("/api/packages/refresh", post(refresh))
        .route("/api/packages/updates", get(list_updates))
        .route("/api/packages/updates/install", post(install_updates))
        .route("/api/packages/search", get(search))
        .route("/api/packages/install", post(install))
        .with_state(svc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkgmgr::{ManagerKind, exec::fake::FakeRunner};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const UPGRADABLE: &str = "Listing...\ncurl/noble-security 8.6 amd64 [upgradable from: 8.5]\nvim/noble-updates 9.2 amd64 [upgradable from: 9.1]\n";
    const CONF: &str = "-o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold";

    fn svc(runner: FakeRunner, kind: Option<ManagerKind>, read_only: bool) -> Arc<PackageService> {
        Arc::new(PackageService::new("ubuntu".into(), kind.map(|k| k.build()), Arc::new(runner), read_only))
    }

    async fn call(s: Arc<PackageService>, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = router::<()>(s).oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn info_reports_manager() {
        let (st, v) = call(svc(FakeRunner::new(), Some(ManagerKind::Apt), false), "GET", "/api/packages/info", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["manager"], "apt");
        assert_eq!(v["distro"], "ubuntu");
        assert_eq!(v["security_support"], "full");
        assert_eq!(v["read_only"], false);
    }

    #[tokio::test]
    async fn unsupported_distro_is_501() {
        let s = svc(FakeRunner::new(), None, false);
        let (st, v) = call(s.clone(), "GET", "/api/packages/info", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["manager"].is_null());
        let (st, v) = call(s, "GET", "/api/packages/updates", None).await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED);
        assert!(v["error"].as_str().unwrap().contains("ubuntu"));
    }

    #[tokio::test]
    async fn lists_and_filters_updates() {
        let s = svc(FakeRunner::new().on("apt list --upgradable", UPGRADABLE), Some(ManagerKind::Apt), false);
        let (st, v) = call(s.clone(), "GET", "/api/packages/updates", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["count"], 2);
        let (_, v) = call(s, "GET", "/api/packages/updates?security=true", None).await;
        assert_eq!(v["count"], 1);
        assert_eq!(v["updates"][0]["name"], "curl");
        assert_eq!(v["updates"][0]["security"], true);
    }

    #[tokio::test]
    async fn search_validates_and_returns_results() {
        let s = svc(FakeRunner::new().on("apt-cache search nginx", "nginx - web server\n"), Some(ManagerKind::Apt), false);
        let (st, v) = call(s.clone(), "GET", "/api/packages/search?q=nginx", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["results"][0]["name"], "nginx");
        let (st, _) = call(s.clone(), "GET", "/api/packages/search?q=--help", None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = call(s, "GET", "/api/packages/search", None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn refresh_and_install() {
        let r = FakeRunner::new().on("apt-get update", "Done").on(&format!("apt-get install -y {CONF} git"), "installed");
        let s = svc(r, Some(ManagerKind::Apt), false);
        let (st, v) = call(s.clone(), "POST", "/api/packages/refresh", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["success"], true);
        assert_eq!(v["manager"], "apt");
        assert_eq!(v["stdout"], "Done");
        let (st, v) = call(s, "POST", "/api/packages/install", Some(r#"{"packages":["git"]}"#)).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["stdout"], "installed");
    }

    #[tokio::test]
    async fn install_rejects_bad_input() {
        let s = svc(FakeRunner::new(), Some(ManagerKind::Apt), false);
        for body in [r#"{"packages":[]}"#, r#"{"packages":["-y"]}"#, r#"{"packages":["a;b"]}"#] {
            let (st, _) = call(s.clone(), "POST", "/api/packages/install", Some(body)).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        }
    }

    #[tokio::test]
    async fn security_only_update_install() {
        let r = FakeRunner::new()
            .on("apt list --upgradable", UPGRADABLE)
            .on(&format!("apt-get install -y --only-upgrade {CONF} curl"), "ok");
        let (st, v) = call(svc(r, Some(ManagerKind::Apt), false), "POST", "/api/packages/updates/install", Some(r#"{"security_only":true}"#)).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["success"], true);
    }

    #[tokio::test]
    async fn unsupported_operation_is_501() {
        let s = svc(FakeRunner::new(), Some(ManagerKind::Apk), false);
        let (st, _) = call(s, "POST", "/api/packages/updates/install", Some(r#"{"security_only":true}"#)).await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn read_only_blocks_mutations_only() {
        let s = svc(FakeRunner::new().on("apt list --upgradable", "Listing...\n"), Some(ManagerKind::Apt), true);
        for (uri, body) in [("/api/packages/refresh", None), ("/api/packages/install", Some(r#"{"packages":["git"]}"#)), ("/api/packages/updates/install", Some("{}"))] {
            let (st, _) = call(s.clone(), "POST", uri, body).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{uri}");
        }
        let (st, _) = call(s, "GET", "/api/packages/updates", None).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn concurrent_mutation_is_409() {
        let s = svc(FakeRunner::new().on("apt-get update", ""), Some(ManagerKind::Apt), false);
        let _held = s.op_lock.lock().await;
        let (st, _) = call(s.clone(), "POST", "/api/packages/refresh", None).await;
        assert_eq!(st, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn failed_command_is_reported_not_hidden() {
        let out = CmdOutput { success: false, exit_code: Some(100), stderr: "E: Unable to lock".into(), ..Default::default() };
        let s = svc(FakeRunner::new().on_output("apt-get update", out), Some(ManagerKind::Apt), false);
        let (st, v) = call(s, "POST", "/api/packages/refresh", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["success"], false);
        assert_eq!(v["exit_code"], 100);
    }
}
