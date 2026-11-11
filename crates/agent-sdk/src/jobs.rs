//! Background jobs for long-running, mutating plugin operations.
//!
//! A job runs independently of any client (a browser closing the page must
//! not kill an installation) and records every output line with a sequence
//! number, so clients can attach at any time, replay from the start, or resume
//! after a disconnect (`Last-Event-ID`). At most one job runs at a time per
//! [`JobManager`]; each plugin owns its manager.
//!
//! Plugin-specific descriptive fields (e.g. `manager`, `packages`) travel in
//! [`JobInfo::meta`] and are flattened into the job's JSON.

use crate::exec::{CmdOutput, LineStream, OutputSink, Runner, ExecError};
use async_trait::async_trait;
use controldeck_proto::Event;
use futures_util::Stream;
use serde::Serialize;
use serde_json::{Map, Value};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

/// Finished jobs kept in memory (the running job is never evicted).
const MAX_JOBS: usize = 50;
/// Output lines kept per job; later lines are dropped with a marker.
const MAX_LINES: usize = 20_000;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OutputLine {
    pub seq: u64,
    pub stream: LineStream,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct JobResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Set when the operation failed before/without a command result.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobInfo {
    pub id: String,
    pub operation: &'static str,
    /// Plugin-specific fields, flattened into the JSON object.
    #[serde(flatten)]
    pub meta: Map<String, Value>,
    pub status: JobStatus,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub duration_ms: Option<u64>,
    pub line_count: usize,
    pub truncated: bool,
    pub result: Option<JobResult>,
}

#[derive(Debug, Clone)]
pub enum JobEvent {
    Output(OutputLine),
    Done(JobInfo),
}

struct Inner {
    status: JobStatus,
    lines: Vec<OutputLine>,
    truncated: bool,
    finished_at: Option<i64>,
    duration_ms: Option<u64>,
    result: Option<JobResult>,
}

pub struct Job {
    id: String,
    operation: &'static str,
    meta: Map<String, Value>,
    started_at: i64,
    started: Instant,
    inner: Mutex<Inner>,
    /// Bumped on every change so streams can wait without polling.
    version: watch::Sender<u64>,
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Job {
    fn new(id: String, operation: &'static str, meta: Map<String, Value>) -> Self {
        Self {
            id,
            operation,
            meta,
            started_at: now_secs(),
            started: Instant::now(),
            inner: Mutex::new(Inner {
                status: JobStatus::Running,
                lines: Vec::new(),
                truncated: false,
                finished_at: None,
                duration_ms: None,
                result: None,
            }),
            version: watch::channel(0).0,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    fn notify(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    fn finish(&self, result: JobResult) {
        {
            let mut g = lock(&self.inner);
            g.status = if result.success { JobStatus::Succeeded } else { JobStatus::Failed };
            g.finished_at = Some(now_secs());
            g.duration_ms = Some(self.started.elapsed().as_millis() as u64);
            g.result = Some(result);
        }
        self.notify();
    }

    fn is_running(&self) -> bool {
        lock(&self.inner).status == JobStatus::Running
    }

    pub fn info(&self) -> JobInfo {
        let g = lock(&self.inner);
        JobInfo {
            id: self.id.clone(),
            operation: self.operation,
            meta: self.meta.clone(),
            status: g.status,
            started_at: self.started_at,
            finished_at: g.finished_at,
            duration_ms: g.duration_ms,
            line_count: g.lines.len(),
            truncated: g.truncated,
            result: g.result.clone(),
        }
    }

    /// All output so far.
    pub fn output(&self) -> Vec<OutputLine> {
        lock(&self.inner).lines.clone()
    }

    /// Lines from index `from` on, and whether the job has finished.
    fn snapshot(&self, from: usize) -> (Vec<OutputLine>, bool) {
        let g = lock(&self.inner);
        (g.lines.get(from..).map(<[_]>::to_vec).unwrap_or_default(), g.status != JobStatus::Running)
    }

    /// Output events followed by one `Done` event. Starts after sequence
    /// number `after` (exclusive) when given, otherwise from the beginning.
    /// Finished jobs replay everything and end.
    pub fn events(self: Arc<Self>, after: Option<u64>) -> impl Stream<Item = JobEvent> {
        struct State {
            job: Arc<Job>,
            rx: watch::Receiver<u64>,
            next: usize,
            queue: VecDeque<JobEvent>,
            done: bool,
        }
        let state = State {
            rx: self.version.subscribe(),
            next: after.map_or(0, |a| a as usize + 1),
            job: self,
            queue: VecDeque::new(),
            done: false,
        };
        futures_util::stream::unfold(state, |mut st| async move {
            loop {
                if let Some(ev) = st.queue.pop_front() {
                    return Some((ev, st));
                }
                if st.done {
                    return None;
                }
                // Mark the current version seen *before* reading, so a change
                // between the read and `changed()` is never missed.
                st.rx.borrow_and_update();
                let (batch, finished) = st.job.snapshot(st.next);
                let had_lines = !batch.is_empty();
                st.next += batch.len();
                st.queue.extend(batch.into_iter().map(JobEvent::Output));
                if finished && !had_lines {
                    st.queue.push_back(JobEvent::Done(st.job.info()));
                    st.done = true;
                } else if !had_lines && st.rx.changed().await.is_err() {
                    return None;
                }
            }
        })
    }
}

impl Job {
    /// [`events`](Job::events) in wire form: `output` events (id = line
    /// sequence number, data = the line) then one `done` event (data = the
    /// final [`JobInfo`]).
    pub fn rpc_events(self: Arc<Self>, after: Option<u64>) -> crate::EventStream {
        use futures_util::StreamExt;
        Box::pin(self.events(after).map(|ev| match ev {
            JobEvent::Output(line) => Event {
                event: Some("output".into()),
                id: Some(line.seq.to_string()),
                data: serde_json::to_value(&line).unwrap_or(Value::Null),
            },
            JobEvent::Done(info) => Event {
                event: Some("done".into()),
                id: None,
                data: serde_json::to_value(&info).unwrap_or(Value::Null),
            },
        }))
    }
}

impl OutputSink for Job {
    fn line(&self, stream: LineStream, text: &str) {
        {
            let mut g = lock(&self.inner);
            if g.lines.len() >= MAX_LINES {
                if g.truncated {
                    return;
                }
                g.truncated = true;
                let seq = g.lines.len() as u64;
                g.lines.push(OutputLine { seq, stream: LineStream::Stderr, text: "[output truncated]".into() });
            } else {
                let seq = g.lines.len() as u64;
                g.lines.push(OutputLine { seq, stream, text: text.to_string() });
            }
        }
        self.notify();
    }
}

/// Runner handed to the operation: forwards everything to the job's output.
struct Streamed {
    inner: Arc<dyn Runner>,
    job: Arc<Job>,
}

#[async_trait]
impl Runner for Streamed {
    async fn run_streaming(&self, cmd: &crate::exec::Cmd, _sink: &dyn OutputSink) -> Result<CmdOutput, ExecError> {
        self.inner.run_streaming(cmd, self.job.as_ref()).await
    }

    fn note(&self, text: &str) {
        self.job.line(LineStream::Stdout, text);
    }
}

/// The body of a job. Errors are reported as text in the job's output/result.
pub type Work = Pin<Box<dyn Future<Output = Result<CmdOutput, String>> + Send>>;

pub struct JobManager {
    runner: Arc<dyn Runner>,
    jobs: Mutex<VecDeque<Arc<Job>>>,
    counter: AtomicU64,
}

impl JobManager {
    pub fn new(runner: Arc<dyn Runner>) -> Self {
        Self { runner, jobs: Mutex::new(VecDeque::new()), counter: AtomicU64::new(0) }
    }

    /// Starts `work` as a background job. Fails with the id of the running job
    /// when another operation is already in progress.
    pub fn start(
        &self,
        operation: &'static str,
        meta: Map<String, Value>,
        work: impl FnOnce(Arc<dyn Runner>) -> Work + Send + 'static,
    ) -> Result<Arc<Job>, String> {
        let job = {
            let mut jobs = lock(&self.jobs);
            if let Some(running) = jobs.iter().find(|j| j.is_running()) {
                return Err(running.id.clone());
            }
            let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
            let job = Arc::new(Job::new(format!("{:x}-{n}", now_secs()), operation, meta));
            jobs.push_back(job.clone());
            while jobs.len() > MAX_JOBS {
                jobs.pop_front();
            }
            job
        };

        let runner: Arc<dyn Runner> = Arc::new(Streamed { inner: self.runner.clone(), job: job.clone() });
        let fut = work(runner);
        let j = job.clone();
        tokio::spawn(async move {
            // The operation runs in its own task so a panic marks the job failed
            // instead of leaving it "running" (which would block all mutations).
            let result = match tokio::spawn(fut).await {
                Ok(Ok(o)) => JobResult { success: o.success, exit_code: o.exit_code, timed_out: o.timed_out, error: None },
                Ok(Err(e)) => {
                    j.line(LineStream::Stderr, &format!("error: {e}"));
                    JobResult { success: false, exit_code: None, timed_out: false, error: Some(e.to_string()) }
                }
                Err(e) => {
                    let msg = format!("operation aborted: {e}");
                    j.line(LineStream::Stderr, &msg);
                    JobResult { success: false, exit_code: None, timed_out: false, error: Some(msg) }
                }
            };
            j.finish(result);
        });
        Ok(job)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        lock(&self.jobs).iter().find(|j| j.id == id).cloned()
    }

    /// Most recent first.
    pub fn list(&self) -> Vec<JobInfo> {
        lock(&self.jobs).iter().rev().map(|j| j.info()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fake::FakeRunner;
    use futures_util::StreamExt;

    fn manager(r: FakeRunner) -> JobManager {
        JobManager::new(Arc::new(r))
    }

    fn run_cmd(line: &'static str) -> impl FnOnce(Arc<dyn Runner>) -> Work + Send + 'static {
        move |r| {
            Box::pin(async move {
                let (prog, args) = line.split_once(' ').unwrap_or((line, ""));
                let args: Vec<&str> = args.split_whitespace().collect();
                let prog: &'static str = Box::leak(prog.to_string().into_boxed_str());
                r.run(&crate::exec::Cmd::new(prog, &args)).await.map_err(|e| e.to_string())
            })
        }
    }

    async fn collect(job: Arc<Job>, after: Option<u64>) -> Vec<JobEvent> {
        job.events(after).collect().await
    }

    #[tokio::test]
    async fn streams_output_then_done() {
        let m = manager(FakeRunner::new().on("apt-get update", "Hit:1 a\nGet:2 b"));
        let job = m.start("refresh", Map::new(), run_cmd("apt-get update")).unwrap();
        let ev = collect(job.clone(), None).await;
        let texts: Vec<String> = ev
            .iter()
            .filter_map(|e| if let JobEvent::Output(l) = e { Some(l.text.clone()) } else { None })
            .collect();
        assert_eq!(texts, ["apt-get update", "Hit:1 a", "Get:2 b"]);
        let Some(JobEvent::Done(info)) = ev.last() else { panic!("no done event") };
        assert_eq!(info.status, JobStatus::Succeeded);
        assert!(info.result.as_ref().unwrap().success);
        assert_eq!(info.line_count, 3);
    }

    #[tokio::test]
    async fn late_subscriber_replays_and_resumes() {
        let m = manager(FakeRunner::new().on("apt-get update", "one\ntwo"));
        let job = m.start("refresh", Map::new(), run_cmd("apt-get update")).unwrap();
        collect(job.clone(), None).await; // wait for completion
        let all = collect(job.clone(), None).await;
        assert_eq!(all.len(), 4); // command + 2 lines + done
        let resumed = collect(job.clone(), Some(1)).await; // after seq 1
        let seqs: Vec<u64> = resumed.iter().filter_map(|e| if let JobEvent::Output(l) = e { Some(l.seq) } else { None }).collect();
        assert_eq!(seqs, [2]);
        assert!(matches!(resumed.last(), Some(JobEvent::Done(_))));
    }

    #[tokio::test]
    async fn failed_command_marks_job_failed() {
        let out = CmdOutput { success: false, exit_code: Some(100), stderr: "E: lock".into(), ..Default::default() };
        let m = manager(FakeRunner::new().on_output("apt-get update", out));
        let job = m.start("refresh", Map::new(), run_cmd("apt-get update")).unwrap();
        let ev = collect(job, None).await;
        let Some(JobEvent::Done(info)) = ev.last() else { panic!() };
        assert_eq!(info.status, JobStatus::Failed);
        assert_eq!(info.result.as_ref().unwrap().exit_code, Some(100));
        assert!(ev.iter().any(|e| matches!(e, JobEvent::Output(l) if l.stream == LineStream::Stderr && l.text == "E: lock")));
    }

    #[tokio::test]
    async fn error_is_reported_in_stream() {
        let m = manager(FakeRunner::new()); // unexpected command -> Err
        let job = m.start("refresh", Map::new(), run_cmd("apt-get update")).unwrap();
        let ev = collect(job, None).await;
        let Some(JobEvent::Done(info)) = ev.last() else { panic!() };
        assert!(info.result.as_ref().unwrap().error.as_ref().unwrap().contains("unexpected command"));
    }

    #[tokio::test]
    async fn panic_does_not_wedge_the_manager() {
        let m = manager(FakeRunner::new().on("apt-get update", ""));
        let job = m.start("refresh", Map::new(), |_| Box::pin(async { panic!("boom") })).unwrap();
        let ev = collect(job, None).await;
        let Some(JobEvent::Done(info)) = ev.last() else { panic!() };
        assert_eq!(info.status, JobStatus::Failed);
        assert!(m.start("refresh", Map::new(), run_cmd("apt-get update")).is_ok());
    }

    #[tokio::test]
    async fn only_one_running_job() {
        let m = manager(FakeRunner::new());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let first = m
            .start("install", Map::new(), move |_| {
                Box::pin(async move {
                    let _ = rx.await;
                    Ok(CmdOutput::ok(""))
                })
            })
            .unwrap();
        let busy = m.start("refresh", Map::new(), run_cmd("apt-get update")).err().unwrap();
        assert_eq!(busy, first.id());
        tx.send(()).unwrap();
        collect(first, None).await;
        assert!(m.start("refresh", Map::new(), |_| Box::pin(async { Ok(CmdOutput::ok("")) })).is_ok());
    }

    #[tokio::test]
    async fn live_stream_receives_lines_as_they_arrive() {
        let m = manager(FakeRunner::new());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let job = m
            .start("install", Map::new(), move |r| {
                Box::pin(async move {
                    r.note("started");
                    let _ = rx.await;
                    r.note("finished");
                    Ok(CmdOutput::ok(""))
                })
            })
            .unwrap();
        let mut s = Box::pin(job.clone().events(None));
        let first = s.next().await.unwrap();
        assert!(matches!(first, JobEvent::Output(l) if l.text == "started"));
        tx.send(()).unwrap();
        let rest: Vec<JobEvent> = s.collect().await;
        assert!(matches!(&rest[0], JobEvent::Output(l) if l.text == "finished"));
        assert!(matches!(rest.last(), Some(JobEvent::Done(_))));
    }

    #[tokio::test]
    async fn keeps_only_recent_jobs_and_lists_newest_first() {
        let m = manager(FakeRunner::new());
        let mut last = String::new();
        for _ in 0..(MAX_JOBS + 5) {
            let job = m.start("refresh", Map::new(), |_| Box::pin(async { Ok(CmdOutput::ok("")) })).unwrap();
            collect(job.clone(), None).await;
            last = job.id().to_string();
        }
        let list = m.list();
        assert_eq!(list.len(), MAX_JOBS);
        assert_eq!(list[0].id, last);
        assert!(m.get(&last).is_some());
        assert!(m.get("nope").is_none());
    }

    #[test]
    fn output_is_truncated_with_marker() {
        let job = Job::new("t".into(), "refresh", Map::new());
        for i in 0..(MAX_LINES + 10) {
            job.line(LineStream::Stdout, &format!("l{i}"));
        }
        let info = job.info();
        assert!(info.truncated);
        assert_eq!(info.line_count, MAX_LINES + 1);
        assert_eq!(job.output().last().unwrap().text, "[output truncated]");
    }
}
