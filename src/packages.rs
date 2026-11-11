//! HTTP API for the package manager abstraction (`/api/packages/*`).
//!
//! Read operations (`updates`, `search`) answer directly. Mutating operations
//! (`refresh`, `install`, `remove`, `updates/install`) start a background job
//! and return `202`; the job's output is available as a replayable Server-Sent
//! Events stream (`/api/packages/jobs/{id}/stream`).

use crate::pkgmgr::{
    self, PackageManager, PkgError, Runner, SearchResult, SecuritySupport, SystemRunner, Update,
    jobs::{Job, JobEvent, JobInfo, JobManager, OutputLine, Work},
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, sync::Arc};

pub struct PackageService {
    distro: String,
    manager: Option<Arc<dyn PackageManager>>,
    runner: Arc<dyn Runner>,
    jobs: JobManager,
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
        Self { distro, manager, jobs: JobManager::new(runner.clone()), runner, read_only }
    }

    pub fn manager_name(&self) -> Option<&'static str> {
        self.manager.as_ref().map(|m| m.name())
    }

    fn manager(&self) -> Result<Arc<dyn PackageManager>, ApiError> {
        self.manager.clone().ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                format!("no supported package manager for distro '{}'", self.distro),
            )
        })
    }

    /// Starts a mutating operation as a background job and answers `202`.
    fn start_job(
        &self,
        operation: &'static str,
        packages: Vec<String>,
        work: impl FnOnce(Arc<dyn PackageManager>, Arc<dyn Runner>) -> Work + Send + 'static,
    ) -> Result<Response, ApiError> {
        if self.read_only {
            return Err(ApiError::new(StatusCode::FORBIDDEN, "package modifications are disabled (DECK_PKG_READONLY)".into()));
        }
        let m = self.manager()?;
        let name = m.name();
        let job = self.jobs.start(operation, name, packages, move |r| work(m, r)).map_err(|running| ApiError {
            status: StatusCode::CONFLICT,
            error: format!("another package operation is in progress (job {running})"),
            running_job: Some(running),
        })?;
        let url = format!("/api/packages/jobs/{}", job.id());
        let body = Started { stream_url: format!("{url}/stream"), url: url.clone(), job: job.info() };
        Ok((StatusCode::ACCEPTED, [(header::LOCATION, url)], Json(body)).into_response())
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    running_job: Option<String>,
}

struct ApiError {
    status: StatusCode,
    error: String,
    running_job: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, error: String) -> Self {
        Self { status, error, running_job: None }
    }
}

impl From<PkgError> for ApiError {
    fn from(e: PkgError) -> Self {
        let status = match e {
            PkgError::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
            PkgError::Invalid(_) => StatusCode::BAD_REQUEST,
            PkgError::Exec(_) | PkgError::ToolMissing(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError::new(status, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(ErrorBody { error: self.error, running_job: self.running_job })).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

#[derive(Serialize)]
struct Info {
    distro: String,
    manager: Option<&'static str>,
    security_support: Option<SecuritySupport>,
    read_only: bool,
}

#[derive(Serialize)]
struct Started {
    job: JobInfo,
    url: String,
    stream_url: String,
}

#[derive(Serialize)]
struct JobsResponse {
    count: usize,
    jobs: Vec<JobInfo>,
}

#[derive(Serialize)]
struct JobDetail {
    #[serde(flatten)]
    job: JobInfo,
    output: Vec<OutputLine>,
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
struct StreamQuery {
    /// Resume after this sequence number (same as the `Last-Event-ID` header).
    after: Option<u64>,
}

#[derive(Deserialize)]
struct InstallUpdatesBody {
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    security_only: bool,
}

#[derive(Deserialize)]
struct PackagesBody {
    packages: Vec<String>,
}

fn non_empty_valid(packages: &[String]) -> Result<(), ApiError> {
    if packages.is_empty() {
        return Err(PkgError::Invalid("packages must not be empty".into()).into());
    }
    Ok(pkgmgr::validate_packages(packages)?)
}

async fn info(State(svc): State<Arc<PackageService>>) -> Json<Info> {
    Json(Info {
        distro: svc.distro.clone(),
        manager: svc.manager_name(),
        security_support: svc.manager.as_ref().map(|m| m.security_support()),
        read_only: svc.read_only,
    })
}

async fn refresh(State(svc): State<Arc<PackageService>>) -> Result<Response, ApiError> {
    svc.start_job("refresh", vec![], |m, r| Box::pin(async move { m.refresh(r.as_ref()).await }))
}

async fn list_updates(State(svc): State<Arc<PackageService>>, Query(q): Query<UpdatesQuery>) -> ApiResult<UpdatesResponse> {
    let m = svc.manager()?;
    let mut updates = m.list_updates(svc.runner.as_ref()).await?;
    if q.security {
        updates.retain(|u| u.security);
    }
    Ok(Json(UpdatesResponse { manager: m.name(), security_support: m.security_support(), count: updates.len(), updates }))
}

async fn install_updates(State(svc): State<Arc<PackageService>>, Json(body): Json<InstallUpdatesBody>) -> Result<Response, ApiError> {
    pkgmgr::validate_packages(&body.packages)?;
    let packages = body.packages.clone();
    svc.start_job("install-updates", packages, move |m, r| {
        Box::pin(async move { m.install_updates(r.as_ref(), &body.packages, body.security_only).await })
    })
}

async fn search(State(svc): State<Arc<PackageService>>, Query(q): Query<SearchQuery>) -> ApiResult<SearchResponse> {
    pkgmgr::validate_query(&q.q)?;
    let m = svc.manager()?;
    let query = q.q.trim().to_string();
    let results = m.search(svc.runner.as_ref(), &query).await?;
    Ok(Json(SearchResponse { manager: m.name(), query, count: results.len(), results }))
}

async fn install(State(svc): State<Arc<PackageService>>, Json(body): Json<PackagesBody>) -> Result<Response, ApiError> {
    non_empty_valid(&body.packages)?;
    let packages = body.packages.clone();
    svc.start_job("install", packages, move |m, r| Box::pin(async move { m.install(r.as_ref(), &body.packages).await }))
}

async fn remove(State(svc): State<Arc<PackageService>>, Json(body): Json<PackagesBody>) -> Result<Response, ApiError> {
    non_empty_valid(&body.packages)?;
    let packages = body.packages.clone();
    svc.start_job("remove", packages, move |m, r| Box::pin(async move { m.remove(r.as_ref(), &body.packages).await }))
}

fn find_job(svc: &PackageService, id: &str) -> Result<Arc<Job>, ApiError> {
    svc.jobs.get(id).ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("job {id} not found")))
}

async fn list_jobs(State(svc): State<Arc<PackageService>>) -> Json<JobsResponse> {
    let jobs = svc.jobs.list();
    Json(JobsResponse { count: jobs.len(), jobs })
}

async fn get_job(State(svc): State<Arc<PackageService>>, Path(id): Path<String>) -> ApiResult<JobDetail> {
    let job = find_job(&svc, &id)?;
    Ok(Json(JobDetail { job: job.info(), output: job.output() }))
}

/// Server-Sent Events: `output` events (id = line sequence number) followed by
/// one `done` event, then the stream ends. Finished jobs replay and end.
async fn stream_job(
    State(svc): State<Arc<PackageService>>,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let job = find_job(&svc, &id)?;
    let after = q.after.or_else(|| headers.get("last-event-id")?.to_str().ok()?.trim().parse().ok());
    let events = job.events(after).map(|ev| {
        let event = match ev {
            JobEvent::Output(line) => Event::default().event("output").id(line.seq.to_string()).json_data(&line),
            JobEvent::Done(info) => Event::default().event("done").json_data(&info),
        };
        Ok::<Event, Infallible>(event.unwrap_or_else(|_| Event::default().comment("serialization error")))
    });
    Ok((
        // Tell reverse proxies (nginx) not to buffer the stream.
        [(HeaderName::from_static("x-accel-buffering"), HeaderValue::from_static("no"))],
        Sse::new(events).keep_alive(KeepAlive::default()),
    ))
}

pub fn router<S: Clone + Send + Sync + 'static>(svc: Arc<PackageService>) -> Router<S> {
    Router::new()
        .route("/api/packages/info", get(info))
        .route("/api/packages/refresh", post(refresh))
        .route("/api/packages/updates", get(list_updates))
        .route("/api/packages/updates/install", post(install_updates))
        .route("/api/packages/search", get(search))
        .route("/api/packages/install", post(install))
        .route("/api/packages/remove", post(remove))
        .route("/api/packages/jobs", get(list_jobs))
        .route("/api/packages/jobs/{id}", get(get_job))
        .route("/api/packages/jobs/{id}/stream", get(stream_job))
        .with_state(svc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkgmgr::{CmdOutput, ManagerKind, exec::fake::FakeRunner};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const UPGRADABLE: &str = "Listing...\ncurl/noble-security 8.6 amd64 [upgradable from: 8.5]\nvim/noble-updates 9.2 amd64 [upgradable from: 9.1]\n";
    const CONF: &str = "-o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold";

    fn svc(runner: FakeRunner, kind: Option<ManagerKind>, read_only: bool) -> Arc<PackageService> {
        Arc::new(PackageService::new("ubuntu".into(), kind.map(|k| k.build()), Arc::new(runner), read_only))
    }

    async fn raw(s: Arc<PackageService>, method: &str, uri: &str, body: Option<&str>, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = router::<()>(s).oneshot(req.body(body).unwrap()).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn call(s: Arc<PackageService>, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, serde_json::Value) {
        let (st, _, text) = raw(s, method, uri, body, &[]).await;
        (st, serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    /// Starts a mutating job, waits for it via the stream endpoint (which ends
    /// when the job is done) and returns the final job detail.
    async fn run_job(s: Arc<PackageService>, uri: &str, body: Option<&str>) -> serde_json::Value {
        let (st, v) = call(s.clone(), "POST", uri, body).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let stream_url = v["stream_url"].as_str().unwrap().to_string();
        raw(s.clone(), "GET", &stream_url, None, &[]).await;
        let (st, detail) = call(s, "GET", v["url"].as_str().unwrap(), None).await;
        assert_eq!(st, StatusCode::OK);
        detail
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
        let (st, v) = call(s.clone(), "GET", "/api/packages/updates", None).await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED);
        assert!(v["error"].as_str().unwrap().contains("ubuntu"));
        let (st, _) = call(s, "POST", "/api/packages/refresh", None).await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED);
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
    async fn mutations_return_202_with_job_links() {
        let s = svc(FakeRunner::new().on("apt-get update", "Done"), Some(ManagerKind::Apt), false);
        let (st, headers, text) = raw(s, "POST", "/api/packages/refresh", None, &[]).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let url = v["url"].as_str().unwrap();
        assert!(url.starts_with("/api/packages/jobs/"));
        assert_eq!(v["stream_url"], format!("{url}/stream"));
        assert_eq!(headers[header::LOCATION], url);
        assert_eq!(v["job"]["operation"], "refresh");
        assert_eq!(v["job"]["manager"], "apt");
    }

    #[tokio::test]
    async fn refresh_job_output_and_result() {
        let s = svc(FakeRunner::new().on("apt-get update", "Hit:1 a\nGet:2 b"), Some(ManagerKind::Apt), false);
        let d = run_job(s, "/api/packages/refresh", None).await;
        assert_eq!(d["status"], "succeeded");
        assert_eq!(d["result"]["success"], true);
        let lines: Vec<&str> = d["output"].as_array().unwrap().iter().map(|l| l["text"].as_str().unwrap()).collect();
        assert_eq!(lines, ["apt-get update", "Hit:1 a", "Get:2 b"]);
        assert_eq!(d["output"][0]["stream"], "command");
        assert_eq!(d["output"][1]["stream"], "stdout");
    }

    #[tokio::test]
    async fn install_and_remove_jobs() {
        let r = FakeRunner::new()
            .on(&format!("apt-get install -y {CONF} git"), "installed")
            .on("apt-get remove -y git", "removed");
        let s = svc(r, Some(ManagerKind::Apt), false);
        let d = run_job(s.clone(), "/api/packages/install", Some(r#"{"packages":["git"]}"#)).await;
        assert_eq!(d["operation"], "install");
        assert_eq!(d["packages"][0], "git");
        assert_eq!(d["output"][1]["text"], "installed");
        let d = run_job(s, "/api/packages/remove", Some(r#"{"packages":["git"]}"#)).await;
        assert_eq!(d["operation"], "remove");
        assert_eq!(d["status"], "succeeded");
    }

    #[tokio::test]
    async fn security_only_update_install() {
        let r = FakeRunner::new()
            .on("apt list --upgradable", UPGRADABLE)
            .on(&format!("apt-get install -y --only-upgrade {CONF} curl"), "ok");
        let d = run_job(svc(r, Some(ManagerKind::Apt), false), "/api/packages/updates/install", Some(r#"{"security_only":true}"#)).await;
        assert_eq!(d["status"], "succeeded");
        assert_eq!(d["operation"], "install-updates");
    }

    #[tokio::test]
    async fn nothing_to_do_is_visible_in_output() {
        let r = FakeRunner::new().on("apt list --upgradable", "Listing...\n");
        let d = run_job(svc(r, Some(ManagerKind::Apt), false), "/api/packages/updates/install", Some(r#"{"security_only":true}"#)).await;
        let texts: Vec<&str> = d["output"].as_array().unwrap().iter().map(|l| l["text"].as_str().unwrap()).collect();
        assert!(texts.contains(&"no security updates to install"));
    }

    #[tokio::test]
    async fn install_rejects_bad_input() {
        let s = svc(FakeRunner::new(), Some(ManagerKind::Apt), false);
        for uri in ["/api/packages/install", "/api/packages/remove"] {
            for body in [r#"{"packages":[]}"#, r#"{"packages":["-y"]}"#, r#"{"packages":["a;b"]}"#] {
                let (st, _) = call(s.clone(), "POST", uri, Some(body)).await;
                assert_eq!(st, StatusCode::BAD_REQUEST, "{uri} {body}");
            }
        }
    }

    #[tokio::test]
    async fn unsupported_operation_is_a_failed_job_not_a_crash() {
        let s = svc(FakeRunner::new(), Some(ManagerKind::Apk), false);
        let d = run_job(s, "/api/packages/updates/install", Some(r#"{"security_only":true}"#)).await;
        assert_eq!(d["status"], "failed");
        assert!(d["result"]["error"].as_str().unwrap().contains("security"));
    }

    #[tokio::test]
    async fn stream_is_sse_with_ids_and_done_event() {
        let s = svc(FakeRunner::new().on("apt-get update", "one\ntwo"), Some(ManagerKind::Apt), false);
        let (_, v) = call(s.clone(), "POST", "/api/packages/refresh", None).await;
        let url = v["stream_url"].as_str().unwrap();
        let (st, headers, text) = raw(s.clone(), "GET", url, None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert!(headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/event-stream"));
        assert_eq!(headers["x-accel-buffering"], "no");
        assert!(text.contains("event: output\n"), "{text}");
        assert!(text.contains("id: 0\n") && text.contains("id: 2\n"), "{text}");
        assert!(text.contains(r#""text":"one""#), "{text}");
        assert!(text.trim_end().ends_with('}') && text.contains("event: done\n"), "{text}");
        assert!(text.contains(r#""status":"succeeded""#), "{text}");

        // Resume: via header and via query, only lines after seq 1.
        for (uri, hdr) in [(url.to_string(), vec![("last-event-id", "1")]), (format!("{url}?after=1"), vec![])] {
            let (_, _, t) = raw(s.clone(), "GET", &uri, None, &hdr).await;
            assert!(!t.contains(r#""text":"one""#), "{t}");
            assert!(t.contains(r#""text":"two""#) && t.contains("event: done"), "{t}");
        }
    }

    #[tokio::test]
    async fn job_endpoints_404_and_list() {
        let s = svc(FakeRunner::new().on("apt-get update", ""), Some(ManagerKind::Apt), false);
        for uri in ["/api/packages/jobs/nope", "/api/packages/jobs/nope/stream"] {
            let (st, _) = call(s.clone(), "GET", uri, None).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{uri}");
        }
        let (_, v) = call(s.clone(), "GET", "/api/packages/jobs", None).await;
        assert_eq!(v["count"], 0);
        run_job(s.clone(), "/api/packages/refresh", None).await;
        let (_, v) = call(s, "GET", "/api/packages/jobs", None).await;
        assert_eq!(v["count"], 1);
        assert!(v["jobs"][0].get("output").is_none());
    }

    #[tokio::test]
    async fn read_only_blocks_mutations_only() {
        let s = svc(FakeRunner::new().on("apt list --upgradable", "Listing...\n"), Some(ManagerKind::Apt), true);
        for (uri, body) in [
            ("/api/packages/refresh", None),
            ("/api/packages/install", Some(r#"{"packages":["git"]}"#)),
            ("/api/packages/remove", Some(r#"{"packages":["git"]}"#)),
            ("/api/packages/updates/install", Some("{}")),
        ] {
            let (st, _) = call(s.clone(), "POST", uri, body).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{uri}");
        }
        let (st, _) = call(s, "GET", "/api/packages/updates", None).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn concurrent_mutation_is_409_with_running_job() {
        let s = svc(FakeRunner::new().on("apt-get update", ""), Some(ManagerKind::Apt), false);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let running = s.jobs.start("install", "apt", vec![], move |_| {
            Box::pin(async move {
                let _ = rx.await;
                Ok(CmdOutput::ok(""))
            })
        }).unwrap();
        let (st, v) = call(s.clone(), "POST", "/api/packages/refresh", None).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(v["running_job"], running.id());
        tx.send(()).unwrap();
        raw(s.clone(), "GET", &format!("/api/packages/jobs/{}/stream", running.id()), None, &[]).await;
        let (st, _) = call(s, "POST", "/api/packages/refresh", None).await;
        assert_eq!(st, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn failed_command_is_a_failed_job() {
        let out = CmdOutput { success: false, exit_code: Some(100), stderr: "E: Unable to lock".into(), ..Default::default() };
        let s = svc(FakeRunner::new().on_output("apt-get update", out), Some(ManagerKind::Apt), false);
        let d = run_job(s, "/api/packages/refresh", None).await;
        assert_eq!(d["status"], "failed");
        assert_eq!(d["result"]["exit_code"], 100);
        assert!(d["output"].as_array().unwrap().iter().any(|l| l["stream"] == "stderr" && l["text"] == "E: Unable to lock"));
    }
}
