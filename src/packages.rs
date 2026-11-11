//! HTTP API for package management (`/api/packages/*`).
//!
//! This is a thin gateway: the web process has no privileges and no package
//! manager code. Every route forwards to the `packages` plugin of the
//! privileged agent (`controldeck-pkgmgr`) over the agent socket, and the
//! plugin validates its input itself. The JSON contract is documented in
//! `docs/packages-api.md`.
//!
//! Read operations (`info`, `updates`, `search`) answer directly. Mutating
//! operations start a background job in the agent and return `202`; the job's
//! output is a replayable Server-Sent Events stream
//! (`/api/packages/jobs/{id}/stream`).

use crate::agent::ApiError;
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
use controldeck_proto::{AgentClient, RpcError};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::convert::Infallible;

const MODULE: &str = "packages";

type ApiResult = Result<Json<Value>, ApiError>;

async fn rpc(agent: &AgentClient, method: &str, params: Value) -> Result<Value, ApiError> {
    Ok(agent.call(MODULE, method, params).await?)
}

#[derive(Serialize)]
struct Started {
    job: Value,
    url: String,
    stream_url: String,
}

/// Calls a method that starts a job and answers `202` with links to it.
async fn start(agent: &AgentClient, method: &str, params: Value) -> Result<Response, ApiError> {
    let job = rpc(agent, method, params).await?;
    let id = job["id"].as_str().ok_or_else(|| RpcError::internal("agent returned a job without id"))?;
    let url = format!("/api/packages/jobs/{id}");
    let body = Started { stream_url: format!("{url}/stream"), url: url.clone(), job };
    Ok((StatusCode::ACCEPTED, [(header::LOCATION, url)], Json(body)).into_response())
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

async fn info(State(agent): State<AgentClient>) -> ApiResult {
    Ok(Json(rpc(&agent, "info", Value::Null).await?))
}

async fn list_updates(State(agent): State<AgentClient>, Query(q): Query<UpdatesQuery>) -> ApiResult {
    Ok(Json(rpc(&agent, "updates", json!({ "security": q.security })).await?))
}

async fn search(State(agent): State<AgentClient>, Query(q): Query<SearchQuery>) -> ApiResult {
    Ok(Json(rpc(&agent, "search", json!({ "q": q.q })).await?))
}

async fn refresh(State(agent): State<AgentClient>) -> Result<Response, ApiError> {
    start(&agent, "refresh", Value::Null).await
}

async fn install_updates(State(agent): State<AgentClient>, Json(body): Json<Value>) -> Result<Response, ApiError> {
    start(&agent, "install_updates", body).await
}

async fn install(State(agent): State<AgentClient>, Json(body): Json<Value>) -> Result<Response, ApiError> {
    start(&agent, "install", body).await
}

async fn remove(State(agent): State<AgentClient>, Json(body): Json<Value>) -> Result<Response, ApiError> {
    start(&agent, "remove", body).await
}

async fn list_jobs(State(agent): State<AgentClient>) -> ApiResult {
    Ok(Json(rpc(&agent, "jobs", Value::Null).await?))
}

async fn get_job(State(agent): State<AgentClient>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(rpc(&agent, "job", json!({ "id": id })).await?))
}

/// Server-Sent Events: `output` events (id = line sequence number) followed by
/// one `done` event, then the stream ends. Finished jobs replay and end. If the
/// agent connection drops mid-stream an `error` event is sent and the stream
/// ends; clients resume with `Last-Event-ID`.
async fn stream_job(
    State(agent): State<AgentClient>,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let after = q.after.or_else(|| headers.get("last-event-id")?.to_str().ok()?.trim().parse().ok());
    let upstream = agent.stream(MODULE, "job_stream", json!({ "id": id, "after": after })).await?;
    let events = upstream.map(|item| {
        let event = match item {
            Ok(ev) => {
                let mut sse = Event::default();
                if let Some(name) = ev.event {
                    sse = sse.event(name);
                }
                if let Some(id) = ev.id {
                    sse = sse.id(id);
                }
                sse.json_data(&ev.data)
            }
            Err(e) => Ok(Event::default().event("error").data(e.message)),
        };
        Ok::<Event, Infallible>(event.unwrap_or_else(|_| Event::default().comment("serialization error")))
    });
    Ok((
        // Tell reverse proxies (nginx) not to buffer the stream.
        [(HeaderName::from_static("x-accel-buffering"), HeaderValue::from_static("no"))],
        Sse::new(events).keep_alive(KeepAlive::default()),
    ))
}

pub fn router<S: Clone + Send + Sync + 'static>(agent: AgentClient) -> Router<S> {
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
        .with_state(agent)
}

/// End-to-end tests: HTTP -> this gateway -> Unix socket -> the real agent
/// server and `packages` plugin (with a scripted command runner).
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::{body::Body, http::Request};
    use controldeck_agent_sdk::{
        Registry,
        exec::{Cmd, CmdOutput, ExecError, OutputSink, Runner, fake::FakeRunner},
        peer::PeerPolicy,
        server,
    };
    use controldeck_pkgmgr::{ManagerKind, PackagesPlugin};
    use http_body_util::BodyExt;
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        },
    };
    use tokio::sync::{Notify, oneshot};
    use tower::ServiceExt;

    const UPGRADABLE: &str = "Listing...\ncurl/noble-security 8.6 amd64 [upgradable from: 8.5]\nvim/noble-updates 9.2 amd64 [upgradable from: 9.1]\n";
    const CONF: &str = "-o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold";
    static N: AtomicU32 = AtomicU32::new(0);

    /// Holds a running agent server (stopped on drop) and the gateway router.
    struct Harness {
        router: Router,
        socket: PathBuf,
        _stop: oneshot::Sender<()>,
    }

    fn harness_with(runner: Arc<dyn Runner>, kind: Option<ManagerKind>, read_only: bool) -> Harness {
        let socket = std::env::temp_dir().join(format!("deck-gw-{}-{}.sock", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let listener = server::bind(&socket, None).unwrap();
        let mut registry = Registry::new();
        registry.register(PackagesPlugin::new("ubuntu".into(), kind.map(|k| k.build()), runner, read_only));
        // SAFETY: trivial getter.
        let policy = PeerPolicy { allowed_uids: vec![unsafe { libc::geteuid() }], allowed_exe: None };
        let (stop, rx) = oneshot::channel::<()>();
        tokio::spawn(server::serve(listener, Arc::new(registry), policy, async move {
            let _ = rx.await;
        }));
        Harness { router: router::<()>(AgentClient::new(&socket)).merge(crate::agent::router(AgentClient::new(&socket))), socket, _stop: stop }
    }

    fn harness(runner: FakeRunner, kind: Option<ManagerKind>, read_only: bool) -> Harness {
        harness_with(Arc::new(runner), kind, read_only)
    }

    fn apt(runner: FakeRunner) -> Harness {
        harness(runner, Some(ManagerKind::Apt), false)
    }

    async fn raw(h: &Harness, method: &str, uri: &str, body: Option<&str>, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
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
        let res = h.router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn call(h: &Harness, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, Value) {
        let (st, _, text) = raw(h, method, uri, body, &[]).await;
        (st, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Starts a mutating job, waits for it via the stream endpoint (which ends
    /// when the job is done) and returns the final job detail.
    async fn run_job(h: &Harness, uri: &str, body: Option<&str>) -> Value {
        let (st, v) = call(h, "POST", uri, body).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        raw(h, "GET", v["stream_url"].as_str().unwrap(), None, &[]).await;
        let (st, detail) = call(h, "GET", v["url"].as_str().unwrap(), None).await;
        assert_eq!(st, StatusCode::OK);
        detail
    }

    fn texts(d: &Value) -> Vec<&str> {
        d["output"].as_array().unwrap().iter().map(|l| l["text"].as_str().unwrap()).collect()
    }

    #[tokio::test]
    async fn info_reports_manager() {
        let (st, v) = call(&apt(FakeRunner::new()), "GET", "/api/packages/info", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["manager"], "apt");
        assert_eq!(v["distro"], "ubuntu");
        assert_eq!(v["security_support"], "full");
        assert_eq!(v["read_only"], false);
    }

    #[tokio::test]
    async fn unsupported_distro_is_501() {
        let h = harness(FakeRunner::new(), None, false);
        let (st, v) = call(&h, "GET", "/api/packages/info", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["manager"].is_null());
        let (st, v) = call(&h, "GET", "/api/packages/updates", None).await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED);
        assert!(v["error"].as_str().unwrap().contains("ubuntu"));
        let (st, _) = call(&h, "POST", "/api/packages/refresh", None).await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn lists_and_filters_updates() {
        let h = apt(FakeRunner::new().on("apt list --upgradable", UPGRADABLE));
        let (st, v) = call(&h, "GET", "/api/packages/updates", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["count"], 2);
        let (_, v) = call(&h, "GET", "/api/packages/updates?security=true", None).await;
        assert_eq!(v["count"], 1);
        assert_eq!(v["updates"][0]["name"], "curl");
        assert_eq!(v["updates"][0]["security"], true);
    }

    #[tokio::test]
    async fn search_validates_and_returns_results() {
        let h = apt(FakeRunner::new().on("apt-cache search nginx", "nginx - web server\n"));
        let (st, v) = call(&h, "GET", "/api/packages/search?q=nginx", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["results"][0]["name"], "nginx");
        let (st, _) = call(&h, "GET", "/api/packages/search?q=--help", None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = call(&h, "GET", "/api/packages/search", None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mutations_return_202_with_job_links() {
        let h = apt(FakeRunner::new().on("apt-get update", "Done"));
        let (st, headers, text) = raw(&h, "POST", "/api/packages/refresh", None, &[]).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let v: Value = serde_json::from_str(&text).unwrap();
        let url = v["url"].as_str().unwrap();
        assert!(url.starts_with("/api/packages/jobs/"));
        assert_eq!(v["stream_url"], format!("{url}/stream"));
        assert_eq!(headers[header::LOCATION], url);
        assert_eq!(v["job"]["operation"], "refresh");
        assert_eq!(v["job"]["manager"], "apt");
    }

    #[tokio::test]
    async fn refresh_job_output_and_result() {
        let h = apt(FakeRunner::new().on("apt-get update", "Hit:1 a\nGet:2 b"));
        let d = run_job(&h, "/api/packages/refresh", None).await;
        assert_eq!(d["status"], "succeeded");
        assert_eq!(d["result"]["success"], true);
        assert_eq!(texts(&d), ["apt-get update", "Hit:1 a", "Get:2 b"]);
        assert_eq!(d["output"][0]["stream"], "command");
        assert_eq!(d["output"][1]["stream"], "stdout");
    }

    #[tokio::test]
    async fn install_and_remove_jobs() {
        let r = FakeRunner::new().on(&format!("apt-get install -y {CONF} git"), "installed").on("apt-get remove -y git", "removed");
        let h = apt(r);
        let d = run_job(&h, "/api/packages/install", Some(r#"{"packages":["git"]}"#)).await;
        assert_eq!(d["operation"], "install");
        assert_eq!(d["packages"][0], "git");
        assert_eq!(d["output"][1]["text"], "installed");
        let d = run_job(&h, "/api/packages/remove", Some(r#"{"packages":["git"]}"#)).await;
        assert_eq!(d["operation"], "remove");
        assert_eq!(d["status"], "succeeded");
    }

    #[tokio::test]
    async fn security_only_update_install() {
        let r = FakeRunner::new().on("apt list --upgradable", UPGRADABLE).on(&format!("apt-get install -y --only-upgrade {CONF} curl"), "ok");
        let d = run_job(&apt(r), "/api/packages/updates/install", Some(r#"{"security_only":true}"#)).await;
        assert_eq!(d["status"], "succeeded");
        assert_eq!(d["operation"], "install-updates");
    }

    #[tokio::test]
    async fn install_rejects_bad_input_in_the_agent() {
        let h = apt(FakeRunner::new());
        for uri in ["/api/packages/install", "/api/packages/remove"] {
            for body in [r#"{"packages":[]}"#, r#"{"packages":["-y"]}"#, r#"{"packages":["a;b"]}"#] {
                let (st, _) = call(&h, "POST", uri, Some(body)).await;
                assert_eq!(st, StatusCode::BAD_REQUEST, "{uri} {body}");
            }
        }
    }

    #[tokio::test]
    async fn unsupported_operation_is_a_failed_job_not_a_crash() {
        let h = harness(FakeRunner::new(), Some(ManagerKind::Apk), false);
        let d = run_job(&h, "/api/packages/updates/install", Some(r#"{"security_only":true}"#)).await;
        assert_eq!(d["status"], "failed");
        assert!(d["result"]["error"].as_str().unwrap().contains("security"));
    }

    #[tokio::test]
    async fn stream_is_sse_with_ids_and_done_event() {
        let h = apt(FakeRunner::new().on("apt-get update", "one\ntwo"));
        let (_, v) = call(&h, "POST", "/api/packages/refresh", None).await;
        let url = v["stream_url"].as_str().unwrap();
        let (st, headers, text) = raw(&h, "GET", url, None, &[]).await;
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
            let (_, _, t) = raw(&h, "GET", &uri, None, &hdr).await;
            assert!(!t.contains(r#""text":"one""#), "{t}");
            assert!(t.contains(r#""text":"two""#) && t.contains("event: done"), "{t}");
        }
    }

    #[tokio::test]
    async fn job_endpoints_404_and_list() {
        let h = apt(FakeRunner::new().on("apt-get update", ""));
        for uri in ["/api/packages/jobs/nope", "/api/packages/jobs/nope/stream"] {
            let (st, _) = call(&h, "GET", uri, None).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{uri}");
        }
        let (_, v) = call(&h, "GET", "/api/packages/jobs", None).await;
        assert_eq!(v["count"], 0);
        run_job(&h, "/api/packages/refresh", None).await;
        let (_, v) = call(&h, "GET", "/api/packages/jobs", None).await;
        assert_eq!(v["count"], 1);
        assert!(v["jobs"][0].get("output").is_none());
    }

    #[tokio::test]
    async fn read_only_blocks_mutations_only() {
        let h = harness(FakeRunner::new().on("apt list --upgradable", "Listing...\n"), Some(ManagerKind::Apt), true);
        for (uri, body) in [
            ("/api/packages/refresh", None),
            ("/api/packages/install", Some(r#"{"packages":["git"]}"#)),
            ("/api/packages/remove", Some(r#"{"packages":["git"]}"#)),
            ("/api/packages/updates/install", Some("{}")),
        ] {
            let (st, _) = call(&h, "POST", uri, body).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{uri}");
        }
        let (st, _) = call(&h, "GET", "/api/packages/updates", None).await;
        assert_eq!(st, StatusCode::OK);
    }

    /// Blocks every command until released, to keep a job running.
    struct Blocking(Arc<Notify>);

    #[async_trait]
    impl Runner for Blocking {
        async fn run_streaming(&self, _cmd: &Cmd, _sink: &dyn OutputSink) -> Result<CmdOutput, ExecError> {
            self.0.notified().await;
            Ok(CmdOutput::ok(""))
        }
    }

    #[tokio::test]
    async fn concurrent_mutation_is_409_with_running_job() {
        let gate = Arc::new(Notify::new());
        let h = harness_with(Arc::new(Blocking(gate.clone())), Some(ManagerKind::Apt), false);
        let (st, first) = call(&h, "POST", "/api/packages/refresh", None).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let (st, v) = call(&h, "POST", "/api/packages/refresh", None).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(v["running_job"], first["job"]["id"]);
        gate.notify_one();
        raw(&h, "GET", first["stream_url"].as_str().unwrap(), None, &[]).await;
        gate.notify_one();
        let (st, _) = call(&h, "POST", "/api/packages/refresh", None).await;
        assert_eq!(st, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn failed_command_is_a_failed_job() {
        let out = CmdOutput { success: false, exit_code: Some(100), stderr: "E: Unable to lock".into(), ..Default::default() };
        let h = apt(FakeRunner::new().on_output("apt-get update", out));
        let d = run_job(&h, "/api/packages/refresh", None).await;
        assert_eq!(d["status"], "failed");
        assert_eq!(d["result"]["exit_code"], 100);
        assert!(d["output"].as_array().unwrap().iter().any(|l| l["stream"] == "stderr" && l["text"] == "E: Unable to lock"));
    }

    #[tokio::test]
    async fn agent_status_lists_plugins() {
        let (st, v) = call(&apt(FakeRunner::new()), "GET", "/api/agent/status", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["connected"], true);
        assert_eq!(v["modules"][0]["name"], "packages");
    }

    #[tokio::test]
    async fn agent_down_is_503_but_the_server_keeps_working() {
        let h = apt(FakeRunner::new());
        let down = Harness { router: router::<()>(AgentClient::new("/nonexistent/agent.sock")).merge(crate::agent::router(AgentClient::new("/nonexistent/agent.sock"))), socket: h.socket.clone(), _stop: oneshot::channel().0 };
        for (method, uri) in [("GET", "/api/packages/info"), ("GET", "/api/packages/updates"), ("POST", "/api/packages/refresh"), ("GET", "/api/packages/jobs/x/stream"), ("GET", "/api/agent/status")] {
            let (st, v) = call(&down, method, uri, None).await;
            assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert!(v["error"].as_str().unwrap().contains("agent unavailable"));
        }
    }
}
