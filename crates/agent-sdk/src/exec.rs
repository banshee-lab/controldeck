//! Command execution for plugins.
//!
//! Commands are described by [`Cmd`] (never a shell string, so there is no
//! quoting/injection surface) and executed through the [`Runner`] trait, which
//! lets plugins be unit-tested with a fake runner. The agent runs as root, so
//! every command runs with the agent's privileges: there is no `sudo`.

use async_trait::async_trait;
use serde::Serialize;
use std::{
    fmt,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
};

/// Maximum bytes of stdout / stderr returned per command.
const MAX_OUTPUT: usize = 1024 * 1024;
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Failure to run a command at all (as opposed to the command failing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    /// The executable is not installed.
    ToolMissing(String),
    /// The command could not be spawned or awaited.
    Exec(String),
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::ToolMissing(t) => write!(f, "required command not found: {t}"),
            ExecError::Exec(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ExecError {}

/// A command to run: program, arguments and execution policy.
#[derive(Debug, Clone)]
pub struct Cmd {
    pub program: &'static str,
    pub args: Vec<String>,
    pub timeout: Duration,
    /// Exit codes treated as success (e.g. `dnf check-update` returns 100).
    pub ok_codes: Vec<i32>,
    pub env: Vec<(&'static str, &'static str)>,
}

impl Cmd {
    pub fn new(program: &'static str, args: &[&str]) -> Self {
        Self {
            program,
            args: args.iter().map(|s| s.to_string()).collect(),
            timeout: Duration::from_secs(120),
            ok_codes: vec![0],
            env: Vec::new(),
        }
    }

    pub fn with_args<I: IntoIterator<Item = String>>(mut self, extra: I) -> Self {
        self.args.extend(extra);
        self
    }

    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    pub fn ok_codes(mut self, codes: &[i32]) -> Self {
        self.ok_codes = codes.to_vec();
        self
    }

    pub fn env(mut self, key: &'static str, value: &'static str) -> Self {
        self.env.push((key, value));
        self
    }

    /// Human-readable form, e.g. `apt-get install -y curl`.
    pub fn display_string(&self) -> String {
        std::iter::once(self.program.to_string())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Result of a finished command.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CmdOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

impl CmdOutput {
    pub fn ok(stdout: &str) -> Self {
        Self { success: true, exit_code: Some(0), stdout: stdout.to_string(), ..Default::default() }
    }
}

/// Which stream an output line came from.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LineStream {
    /// The command line being executed (emitted once per command).
    Command,
    Stdout,
    Stderr,
}

/// Receives command output line by line while a command runs.
pub trait OutputSink: Send + Sync {
    fn line(&self, stream: LineStream, text: &str);
}

pub struct NoSink;

impl OutputSink for NoSink {
    fn line(&self, _stream: LineStream, _text: &str) {}
}

#[async_trait]
pub trait Runner: Send + Sync {
    /// Runs `cmd`, forwarding output lines to `sink` as they are produced.
    async fn run_streaming(&self, cmd: &Cmd, sink: &dyn OutputSink) -> Result<CmdOutput, ExecError>;

    async fn run(&self, cmd: &Cmd) -> Result<CmdOutput, ExecError> {
        self.run_streaming(cmd, &NoSink).await
    }

    /// Informational line from an operation that ran no command (shown in streamed output).
    fn note(&self, _text: &str) {}
}

/// Clean, fixed environment: nothing from the agent's own environment (which
/// may carry configuration) leaks into spawned commands.
fn envs(cmd: &Cmd) -> Vec<(String, String)> {
    let mut v = vec![
        ("LC_ALL".to_string(), "C".to_string()),
        ("LANG".to_string(), "C".to_string()),
        ("PATH".to_string(), PATH.to_string()),
        ("HOME".to_string(), "/root".to_string()),
        ("TERM".to_string(), "dumb".to_string()),
    ];
    v.extend(cmd.env.iter().map(|(k, val)| (k.to_string(), val.to_string())));
    v
}

/// Runs commands on the local system.
pub struct SystemRunner;

/// Reads `r` to the end, forwarding each line (carriage-return separated
/// progress updates count as separate lines) and collecting a capped copy.
async fn pump<R: AsyncRead + Unpin>(r: R, stream: LineStream, sink: &dyn OutputSink, collected: &mut String) {
    let mut rd = BufReader::new(r);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match rd.read_until(b'\n', &mut raw).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&raw);
                for part in text.split(['\n', '\r']).filter(|p| !p.is_empty()) {
                    sink.line(stream, part);
                    if collected.len() < MAX_OUTPUT {
                        collected.push_str(part);
                        collected.push('\n');
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Runner for SystemRunner {
    async fn run_streaming(&self, cmd: &Cmd, sink: &dyn OutputSink) -> Result<CmdOutput, ExecError> {
        let program = cmd.program;
        let mut c = Command::new(program);
        c.args(&cmd.args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        for (k, v) in envs(cmd) {
            c.env(k, v);
        }

        let start = Instant::now();
        let mut child = c.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ExecError::ToolMissing(program.to_string())
            } else {
                ExecError::Exec(format!("cannot run {program}: {e}"))
            }
        })?;
        sink.line(LineStream::Command, &cmd.display_string());
        let pid = child.id();
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let (mut out, mut err) = (String::new(), String::new());
        let waited = tokio::time::timeout(cmd.timeout, async {
            let (_, _, status) = tokio::join!(
                pump(stdout, LineStream::Stdout, sink, &mut out),
                pump(stderr, LineStream::Stderr, sink, &mut err),
                child.wait()
            );
            status
        })
        .await;

        let duration_ms = start.elapsed().as_millis() as u64;
        match waited {
            Ok(Ok(status)) => {
                let exit_code = status.code();
                Ok(CmdOutput {
                    success: exit_code.is_some_and(|c| cmd.ok_codes.contains(&c)),
                    exit_code,
                    timed_out: false,
                    stdout: out,
                    stderr: err,
                    duration_ms,
                })
            }
            Ok(Err(e)) => Err(ExecError::Exec(format!("wait failed: {e}"))),
            Err(_) => {
                // Kill the whole process group (e.g. sudo -> apt), then the child.
                if let Some(pid) = pid {
                    // SAFETY: plain syscall on the process group of our own child.
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
                let _ = child.start_kill();
                sink.line(LineStream::Stderr, "timed out; command killed");
                Ok(CmdOutput { success: false, exit_code: None, timed_out: true, stdout: out, stderr: err, duration_ms })
            }
        }
    }
}

/// Scripted runner for unit tests: responses are matched on the exact
/// `program arg arg…` string and every executed command is recorded.
#[cfg(any(test, feature = "test-util"))]
pub mod fake {
    use super::*;
    use std::{collections::HashMap, sync::Mutex};

    #[derive(Default)]
    pub struct FakeRunner {
        responses: HashMap<String, Result<CmdOutput, String>>,
        pub calls: Mutex<Vec<Cmd>>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn on(mut self, cmdline: &str, stdout: &str) -> Self {
            self.responses.insert(cmdline.to_string(), Ok(CmdOutput::ok(stdout)));
            self
        }

        pub fn on_output(mut self, cmdline: &str, out: CmdOutput) -> Self {
            self.responses.insert(cmdline.to_string(), Ok(out));
            self
        }

        pub fn missing(mut self, cmdline: &str) -> Self {
            self.responses.insert(cmdline.to_string(), Err("missing".to_string()));
            self
        }

        pub fn executed(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|c| c.display_string()).collect()
        }
    }

    #[async_trait]
    impl Runner for FakeRunner {
        async fn run_streaming(&self, cmd: &Cmd, sink: &dyn OutputSink) -> Result<CmdOutput, ExecError> {
            self.calls.lock().unwrap().push(cmd.clone());
            sink.line(LineStream::Command, &cmd.display_string());
            match self.responses.get(&cmd.display_string()) {
                Some(Ok(o)) => {
                    o.stdout.lines().for_each(|l| sink.line(LineStream::Stdout, l));
                    o.stderr.lines().for_each(|l| sink.line(LineStream::Stderr, l));
                    Ok(o.clone())
                }
                Some(Err(_)) => Err(ExecError::ToolMissing(cmd.program.to_string())),
                None => Err(ExecError::Exec(format!("unexpected command: {}", cmd.display_string()))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn system_runner_captures_output_and_codes() {
        let r = SystemRunner;
        let o = r.run(&Cmd::new("sh", &["-c", "echo out; echo err >&2; exit 3"]).ok_codes(&[3])).await.unwrap();
        assert!(o.success);
        assert_eq!(o.exit_code, Some(3));
        assert_eq!(o.stdout.trim(), "out");
        assert_eq!(o.stderr.trim(), "err");
        let o = r.run(&Cmd::new("sh", &["-c", "exit 1"])).await.unwrap();
        assert!(!o.success);
    }

    #[tokio::test]
    async fn system_runner_times_out() {
        let o = SystemRunner.run(&Cmd::new("sleep", &["5"]).timeout_secs(1)).await.unwrap();
        assert!(o.timed_out);
        assert!(!o.success);
    }

    #[tokio::test]
    async fn missing_binary_is_tool_missing() {
        let e = SystemRunner.run(&Cmd::new("definitely-not-a-real-binary", &[])).await.unwrap_err();
        assert!(matches!(e, ExecError::ToolMissing(_)));
    }

    #[derive(Default)]
    struct Collect(std::sync::Mutex<Vec<(LineStream, String)>>);

    impl OutputSink for Collect {
        fn line(&self, stream: LineStream, text: &str) {
            self.0.lock().unwrap().push((stream, text.to_string()));
        }
    }

    #[tokio::test]
    async fn system_runner_streams_lines_as_they_are_produced() {
        let sink = Collect::default();
        let script = "printf 'a\\nb\\r50%%\\r100%%\\n'; echo oops >&2";
        let o = SystemRunner.run_streaming(&Cmd::new("sh", &["-c", script]), &sink).await.unwrap();
        assert!(o.success);
        let lines = sink.0.lock().unwrap().clone();
        assert_eq!(lines[0], (LineStream::Command, format!("sh -c {script}")));
        let out: Vec<&str> = lines.iter().filter(|(s, _)| *s == LineStream::Stdout).map(|(_, t)| t.as_str()).collect();
        assert_eq!(out, ["a", "b", "50%", "100%"]);
        assert!(lines.contains(&(LineStream::Stderr, "oops".to_string())));
        assert_eq!(o.stdout, "a\nb\n50%\n100%\n");
    }

    #[tokio::test]
    async fn timeout_keeps_partial_output() {
        let sink = Collect::default();
        let o = SystemRunner
            .run_streaming(&Cmd::new("sh", &["-c", "echo started; sleep 5"]).timeout_secs(1), &sink)
            .await
            .unwrap();
        assert!(o.timed_out);
        assert_eq!(o.stdout, "started\n");
        assert!(sink.0.lock().unwrap().iter().any(|(_, t)| t.contains("timed out")));
    }
}
