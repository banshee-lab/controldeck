//! The `packages` agent plugin.
//!
//! Methods (all parameters/results are JSON; shapes are documented in
//! `docs/packages-api.md`):
//!
//! | method | kind | params |
//! |---|---|---|
//! | `info` | call | – |
//! | `updates` | call | `{security?: bool}` |
//! | `search` | call | `{q: string}` |
//! | `refresh` | call → job | – |
//! | `install` / `remove` | call → job | `{packages: [string]}` |
//! | `install_updates` | call → job | `{packages?: [string], security_only?: bool}` |
//! | `jobs` | call | – |
//! | `job` | call | `{id}` |
//! | `job_stream` | stream | `{id, after?}` |
//!
//! Mutating methods start a background job and return its `JobInfo`.
//! Everything received is validated here ([`validate_packages`],
//! [`validate_query`]): the caller is the (less trusted) web process.

use crate::{
    PackageManager, PkgError, SearchResult, SecuritySupport, Update, detect, validate_packages, validate_query,
};
use async_trait::async_trait;
use controldeck_agent_sdk::{
    EventStream, Plugin, params,
    exec::{CmdOutput, Runner, SystemRunner},
    jobs::{JobInfo, JobManager, OutputLine, Work},
    unknown_method,
};
use controldeck_proto::RpcError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub struct PackagesPlugin {
    distro: String,
    manager: Option<Arc<dyn PackageManager>>,
    runner: Arc<dyn Runner>,
    jobs: JobManager,
    /// Mutating methods disabled (`DECK_PKG_READONLY`).
    read_only: bool,
}

impl From<PkgError> for RpcError {
    fn from(e: PkgError) -> Self {
        match e {
            PkgError::Unsupported(m) => RpcError::unsupported(m),
            PkgError::Invalid(m) => RpcError::bad_request(m),
            PkgError::Exec(_) | PkgError::ToolMissing(_) => RpcError::internal(e.to_string()),
        }
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

#[derive(Deserialize, Default)]
struct UpdatesParams {
    #[serde(default)]
    security: bool,
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
}

#[derive(Deserialize, Default)]
struct InstallUpdatesParams {
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    security_only: bool,
}

#[derive(Deserialize)]
struct PackagesParams {
    #[serde(default)]
    packages: Vec<String>,
}

#[derive(Deserialize)]
struct JobParams {
    id: String,
    /// Resume after this output sequence number.
    after: Option<u64>,
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| RpcError::internal(format!("cannot encode result: {e}")))
}

fn non_empty_valid(packages: &[String]) -> Result<(), RpcError> {
    if packages.is_empty() {
        return Err(PkgError::Invalid("packages must not be empty".into()).into());
    }
    Ok(validate_packages(packages)?)
}

impl PackagesPlugin {
    /// Detects the distro and builds the matching manager. `DECK_PKG_MANAGER`
    /// overrides detection; `DECK_PKG_READONLY=1` disables mutations.
    pub fn from_env() -> Self {
        let d = detect();
        let read_only = std::env::var("DECK_PKG_READONLY").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
        Self::new(d.distro_id, d.kind.map(|k| k.build()), Arc::new(SystemRunner), read_only)
    }

    pub fn new(distro: String, manager: Option<Arc<dyn PackageManager>>, runner: Arc<dyn Runner>, read_only: bool) -> Self {
        Self { distro, manager, jobs: JobManager::new(runner.clone()), runner, read_only }
    }

    pub fn manager_name(&self) -> Option<&'static str> {
        self.manager.as_ref().map(|m| m.name())
    }

    fn manager(&self) -> Result<Arc<dyn PackageManager>, RpcError> {
        self.manager
            .clone()
            .ok_or_else(|| RpcError::unsupported(format!("no supported package manager for distro '{}'", self.distro)))
    }

    /// Starts a mutating operation as a background job; returns its `JobInfo`.
    fn start_job(
        &self,
        operation: &'static str,
        packages: Vec<String>,
        work: impl FnOnce(Arc<dyn PackageManager>, Arc<dyn Runner>) -> Work + Send + 'static,
    ) -> Result<Value, RpcError> {
        if self.read_only {
            return Err(RpcError::forbidden("package modifications are disabled (DECK_PKG_READONLY)"));
        }
        let m = self.manager()?;
        let mut meta = Map::new();
        meta.insert("manager".into(), json!(m.name()));
        meta.insert("packages".into(), json!(packages));
        let job = self
            .jobs
            .start(operation, meta, move |r| work(m, r))
            .map_err(|running| RpcError::conflict(format!("another package operation is in progress (job {running})")).with_data(json!({ "running_job": running })))?;
        to_value(&job.info())
    }

    fn find_job(&self, id: &str) -> Result<Arc<controldeck_agent_sdk::jobs::Job>, RpcError> {
        self.jobs.get(id).ok_or_else(|| RpcError::not_found(format!("job {id} not found")))
    }

    async fn updates(&self, p: UpdatesParams) -> Result<Value, RpcError> {
        let m = self.manager()?;
        let mut updates = m.list_updates(self.runner.as_ref()).await?;
        if p.security {
            updates.retain(|u| u.security);
        }
        to_value(&UpdatesResponse { manager: m.name(), security_support: m.security_support(), count: updates.len(), updates })
    }

    async fn search(&self, p: SearchParams) -> Result<Value, RpcError> {
        validate_query(&p.q)?;
        let m = self.manager()?;
        let query = p.q.trim().to_string();
        let results = m.search(self.runner.as_ref(), &query).await?;
        to_value(&SearchResponse { manager: m.name(), query, count: results.len(), results })
    }
}

fn job_work<F, Fut>(f: F) -> impl FnOnce(Arc<dyn PackageManager>, Arc<dyn Runner>) -> Work + Send + 'static
where
    F: FnOnce(Arc<dyn PackageManager>, Arc<dyn Runner>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<CmdOutput, PkgError>> + Send + 'static,
{
    move |m, r| Box::pin(async move { f(m, r).await.map_err(|e| e.to_string()) })
}

#[async_trait]
impl Plugin for PackagesPlugin {
    fn name(&self) -> &'static str {
        "packages"
    }

    fn description(&self) -> &'static str {
        "Package management (apt, dnf, zypper, pacman, emerge, apk)"
    }

    async fn call(&self, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            "info" => to_value(&Info {
                distro: self.distro.clone(),
                manager: self.manager_name(),
                security_support: self.manager.as_ref().map(|m| m.security_support()),
                read_only: self.read_only,
            }),
            "updates" => self.updates(params(p)?).await,
            "search" => self.search(params(p)?).await,
            "refresh" => self.start_job("refresh", vec![], job_work(|m, r| async move { m.refresh(r.as_ref()).await })),
            "install_updates" => {
                let body: InstallUpdatesParams = params(p)?;
                validate_packages(&body.packages)?;
                let packages = body.packages.clone();
                self.start_job(
                    "install-updates",
                    packages,
                    job_work(move |m, r| async move { m.install_updates(r.as_ref(), &body.packages, body.security_only).await }),
                )
            }
            "install" => {
                let body: PackagesParams = params(p)?;
                non_empty_valid(&body.packages)?;
                let packages = body.packages.clone();
                self.start_job("install", packages, job_work(move |m, r| async move { m.install(r.as_ref(), &body.packages).await }))
            }
            "remove" => {
                let body: PackagesParams = params(p)?;
                non_empty_valid(&body.packages)?;
                let packages = body.packages.clone();
                self.start_job("remove", packages, job_work(move |m, r| async move { m.remove(r.as_ref(), &body.packages).await }))
            }
            "jobs" => {
                let jobs = self.jobs.list();
                to_value(&JobsResponse { count: jobs.len(), jobs })
            }
            "job" => {
                let p: JobParams = params(p)?;
                let job = self.find_job(&p.id)?;
                to_value(&JobDetail { job: job.info(), output: job.output() })
            }
            m => Err(unknown_method("packages", m)),
        }
    }

    /// `output` events (id = line sequence number) followed by one `done`
    /// event, then the stream ends. Finished jobs replay and end.
    async fn stream(&self, method: &str, p: Value) -> Result<EventStream, RpcError> {
        match method {
            "job_stream" => {
                let p: JobParams = params(p)?;
                Ok(self.find_job(&p.id)?.rpc_events(p.after))
            }
            m => Err(unknown_method("packages", m)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManagerKind;
    use controldeck_agent_sdk::exec::fake::FakeRunner;
    use controldeck_proto::ErrorCode;
    use futures_util::StreamExt;

    const UPGRADABLE: &str = "Listing...\ncurl/noble-security 8.6 amd64 [upgradable from: 8.5]\nvim/noble-updates 9.2 amd64 [upgradable from: 9.1]\n";
    const CONF: &str = "-o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold";

    fn svc(runner: FakeRunner, kind: Option<ManagerKind>, read_only: bool) -> PackagesPlugin {
        PackagesPlugin::new("ubuntu".into(), kind.map(|k| k.build()), Arc::new(runner), read_only)
    }

    fn apt(runner: FakeRunner) -> PackagesPlugin {
        svc(runner, Some(ManagerKind::Apt), false)
    }

    async fn collect(p: &PackagesPlugin, id: &str, after: Option<u64>) -> Vec<controldeck_proto::Event> {
        p.stream("job_stream", json!({ "id": id, "after": after })).await.unwrap().collect().await
    }

    /// Starts a job, waits for it through its stream (which ends when the job
    /// is done) and returns the final job detail.
    async fn run_job(p: &PackagesPlugin, method: &str, body: Value) -> Value {
        let started = p.call(method, body).await.unwrap();
        let id = started["id"].as_str().unwrap().to_string();
        collect(p, &id, None).await;
        p.call("job", json!({ "id": id })).await.unwrap()
    }

    fn texts(d: &Value) -> Vec<&str> {
        d["output"].as_array().unwrap().iter().map(|l| l["text"].as_str().unwrap()).collect()
    }

    #[tokio::test]
    async fn info_reports_manager() {
        let v = apt(FakeRunner::new()).call("info", Value::Null).await.unwrap();
        assert_eq!(v["manager"], "apt");
        assert_eq!(v["distro"], "ubuntu");
        assert_eq!(v["security_support"], "full");
        assert_eq!(v["read_only"], false);
    }

    #[tokio::test]
    async fn unsupported_distro_is_not_implemented() {
        let p = svc(FakeRunner::new(), None, false);
        assert!(p.call("info", Value::Null).await.unwrap()["manager"].is_null());
        let e = p.call("updates", Value::Null).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Unsupported);
        assert!(e.message.contains("ubuntu"));
        assert_eq!(p.call("refresh", Value::Null).await.unwrap_err().code, ErrorCode::Unsupported);
    }

    #[tokio::test]
    async fn lists_and_filters_updates() {
        let p = apt(FakeRunner::new().on("apt list --upgradable", UPGRADABLE));
        let v = p.call("updates", Value::Null).await.unwrap();
        assert_eq!(v["count"], 2);
        let v = p.call("updates", json!({"security": true})).await.unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["updates"][0]["name"], "curl");
        assert_eq!(v["updates"][0]["security"], true);
    }

    #[tokio::test]
    async fn search_validates_and_returns_results() {
        let p = apt(FakeRunner::new().on("apt-cache search nginx", "nginx - web server\n"));
        let v = p.call("search", json!({"q": "nginx"})).await.unwrap();
        assert_eq!(v["results"][0]["name"], "nginx");
        assert_eq!(p.call("search", json!({"q": "--help"})).await.unwrap_err().code, ErrorCode::BadRequest);
        assert_eq!(p.call("search", Value::Null).await.unwrap_err().code, ErrorCode::BadRequest);
    }

    #[tokio::test]
    async fn mutations_return_job_info() {
        let p = apt(FakeRunner::new().on("apt-get update", "Done"));
        let v = p.call("refresh", Value::Null).await.unwrap();
        assert!(v["id"].as_str().is_some());
        assert_eq!(v["operation"], "refresh");
        assert_eq!(v["manager"], "apt"); // plugin metadata is flattened into the job
        assert_eq!(v["packages"], json!([]));
    }

    #[tokio::test]
    async fn refresh_job_output_and_result() {
        let p = apt(FakeRunner::new().on("apt-get update", "Hit:1 a\nGet:2 b"));
        let d = run_job(&p, "refresh", Value::Null).await;
        assert_eq!(d["status"], "succeeded");
        assert_eq!(d["result"]["success"], true);
        assert_eq!(texts(&d), ["apt-get update", "Hit:1 a", "Get:2 b"]);
        assert_eq!(d["output"][0]["stream"], "command");
        assert_eq!(d["output"][1]["stream"], "stdout");
    }

    #[tokio::test]
    async fn install_and_remove_jobs() {
        let r = FakeRunner::new().on(&format!("apt-get install -y {CONF} git"), "installed").on("apt-get remove -y git", "removed");
        let p = apt(r);
        let d = run_job(&p, "install", json!({"packages": ["git"]})).await;
        assert_eq!(d["operation"], "install");
        assert_eq!(d["packages"][0], "git");
        assert_eq!(d["output"][1]["text"], "installed");
        let d = run_job(&p, "remove", json!({"packages": ["git"]})).await;
        assert_eq!(d["operation"], "remove");
        assert_eq!(d["status"], "succeeded");
    }

    #[tokio::test]
    async fn security_only_update_install() {
        let r = FakeRunner::new().on("apt list --upgradable", UPGRADABLE).on(&format!("apt-get install -y --only-upgrade {CONF} curl"), "ok");
        let d = run_job(&apt(r), "install_updates", json!({"security_only": true})).await;
        assert_eq!(d["status"], "succeeded");
        assert_eq!(d["operation"], "install-updates");
    }

    #[tokio::test]
    async fn nothing_to_do_is_visible_in_output() {
        let r = FakeRunner::new().on("apt list --upgradable", "Listing...\n");
        let d = run_job(&apt(r), "install_updates", json!({"security_only": true})).await;
        assert!(texts(&d).contains(&"no security updates to install"));
    }

    #[tokio::test]
    async fn install_rejects_bad_input() {
        let p = apt(FakeRunner::new());
        for method in ["install", "remove"] {
            for body in [json!({"packages": []}), json!({"packages": ["-y"]}), json!({"packages": ["a;b"]}), json!({"packages": ["$(x)"]}), json!({})] {
                let e = p.call(method, body.clone()).await.unwrap_err();
                assert_eq!(e.code, ErrorCode::BadRequest, "{method} {body}");
            }
        }
        // Wrong types are a client error too, not a crash.
        assert_eq!(p.call("install", json!({"packages": "git"})).await.unwrap_err().code, ErrorCode::BadRequest);
        assert_eq!(p.call("install_updates", json!({"packages": ["-y"]})).await.unwrap_err().code, ErrorCode::BadRequest);
    }

    #[tokio::test]
    async fn unsupported_operation_is_a_failed_job_not_a_crash() {
        let p = svc(FakeRunner::new(), Some(ManagerKind::Apk), false);
        let d = run_job(&p, "install_updates", json!({"security_only": true})).await;
        assert_eq!(d["status"], "failed");
        assert!(d["result"]["error"].as_str().unwrap().contains("security"));
    }

    #[tokio::test]
    async fn stream_has_ids_a_done_event_and_resumes() {
        let p = apt(FakeRunner::new().on("apt-get update", "one\ntwo"));
        let id = p.call("refresh", Value::Null).await.unwrap()["id"].as_str().unwrap().to_string();
        let ev = collect(&p, &id, None).await;
        assert!(ev[..3].iter().all(|e| e.event.as_deref() == Some("output")));
        let ids: Vec<_> = ev[..3].iter().map(|e| e.id.clone().unwrap()).collect();
        assert_eq!(ids, ["0", "1", "2"]);
        assert_eq!(ev[1].data["text"], "one");
        let done = ev.last().unwrap();
        assert_eq!(done.event.as_deref(), Some("done"));
        assert_eq!(done.data["status"], "succeeded");
        // Resume after sequence 1: only line 2 and the done event.
        let ev = collect(&p, &id, Some(1)).await;
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].data["text"], "two");
        assert_eq!(ev[1].event.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn unknown_jobs_are_not_found_and_listing_has_no_output() {
        let p = apt(FakeRunner::new().on("apt-get update", ""));
        assert_eq!(p.call("job", json!({"id": "nope"})).await.unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(p.stream("job_stream", json!({"id": "nope"})).await.err().unwrap().code, ErrorCode::NotFound);
        assert_eq!(p.call("jobs", Value::Null).await.unwrap()["count"], 0);
        run_job(&p, "refresh", Value::Null).await;
        let v = p.call("jobs", Value::Null).await.unwrap();
        assert_eq!(v["count"], 1);
        assert!(v["jobs"][0].get("output").is_none());
    }

    #[tokio::test]
    async fn read_only_blocks_mutations_only() {
        let p = svc(FakeRunner::new().on("apt list --upgradable", "Listing...\n"), Some(ManagerKind::Apt), true);
        for (m, body) in [("refresh", json!({})), ("install", json!({"packages": ["git"]})), ("remove", json!({"packages": ["git"]})), ("install_updates", json!({}))] {
            assert_eq!(p.call(m, body).await.unwrap_err().code, ErrorCode::Forbidden, "{m}");
        }
        assert!(p.call("updates", Value::Null).await.is_ok());
        assert_eq!(p.call("info", Value::Null).await.unwrap()["read_only"], true);
    }

    #[tokio::test]
    async fn concurrent_mutation_is_a_conflict_naming_the_running_job() {
        let p = apt(FakeRunner::new().on("apt-get update", ""));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let mut meta = Map::new();
        meta.insert("manager".into(), json!("apt"));
        let running = p
            .jobs
            .start("install", meta, move |_| {
                Box::pin(async move {
                    let _ = rx.await;
                    Ok(CmdOutput::ok(""))
                })
            })
            .unwrap();
        let e = p.call("refresh", Value::Null).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Conflict);
        assert_eq!(e.data.unwrap()["running_job"], running.id());
        tx.send(()).unwrap();
        collect(&p, running.id(), None).await;
        assert!(p.call("refresh", Value::Null).await.is_ok());
    }

    #[tokio::test]
    async fn failed_command_is_a_failed_job() {
        let out = CmdOutput { success: false, exit_code: Some(100), stderr: "E: Unable to lock".into(), ..Default::default() };
        let d = run_job(&apt(FakeRunner::new().on_output("apt-get update", out)), "refresh", Value::Null).await;
        assert_eq!(d["status"], "failed");
        assert_eq!(d["result"]["exit_code"], 100);
        assert!(d["output"].as_array().unwrap().iter().any(|l| l["stream"] == "stderr" && l["text"] == "E: Unable to lock"));
    }

    #[tokio::test]
    async fn unknown_methods_are_not_found() {
        let p = apt(FakeRunner::new());
        assert_eq!(p.call("rm_rf", Value::Null).await.unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(p.stream("refresh", Value::Null).await.err().unwrap().code, ErrorCode::NotFound);
    }
}
