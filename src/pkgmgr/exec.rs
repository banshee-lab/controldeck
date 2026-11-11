//! Command execution for the package manager layer.
//!
//! Commands are described by [`Cmd`] (never a shell string, so there is no
//! quoting/injection surface) and executed through the [`Runner`] trait, which
//! lets the parsers and implementations be unit-tested with a fake runner.

use super::PkgError;
use serde::Serialize;
use async_trait::async_trait;
use std::{
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::process::Command;

/// Maximum bytes of stdout / stderr returned per command.
const MAX_OUTPUT: usize = 1024 * 1024;
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A command to run: program, arguments and execution policy.
#[derive(Debug, Clone)]
pub struct Cmd {
    pub program: &'static str,
    pub args: Vec<String>,
    /// Needs root: run directly when already root, otherwise via `sudo -n`.
    pub privileged: bool,
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
            privileged: false,
            timeout: Duration::from_secs(120),
            ok_codes: vec![0],
            env: Vec::new(),
        }
    }

    pub fn with_args<I: IntoIterator<Item = String>>(mut self, extra: I) -> Self {
        self.args.extend(extra);
        self
    }

    pub fn privileged(mut self) -> Self {
        self.privileged = true;
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
    #[cfg(test)]
    pub fn display(&self) -> String {
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
    #[cfg(test)]
    pub fn ok(stdout: &str) -> Self {
        Self { success: true, exit_code: Some(0), stdout: stdout.to_string(), ..Default::default() }
    }
}

#[async_trait]
pub trait Runner: Send + Sync {
    async fn run(&self, cmd: &Cmd) -> Result<CmdOutput, PkgError>;
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

fn envs(cmd: &Cmd) -> Vec<(String, String)> {
    let mut v = vec![
        ("LC_ALL".to_string(), "C".to_string()),
        ("LANG".to_string(), "C".to_string()),
        ("PATH".to_string(), PATH.to_string()),
    ];
    v.extend(cmd.env.iter().map(|(k, val)| (k.to_string(), val.to_string())));
    v
}

/// Final program + argv. Privileged commands run through `sudo -n` (never
/// prompting) unless already root; `sudo` resets the environment, so the
/// variables are re-applied with `env`.
pub fn build_argv(cmd: &Cmd, root: bool) -> (String, Vec<String>) {
    if cmd.privileged && !root {
        let mut args: Vec<String> = vec!["-n".into(), "--".into(), "env".into()];
        args.extend(envs(cmd).into_iter().map(|(k, v)| format!("{k}={v}")));
        args.push(cmd.program.to_string());
        args.extend(cmd.args.iter().cloned());
        ("sudo".to_string(), args)
    } else {
        (cmd.program.to_string(), cmd.args.clone())
    }
}

/// Runs commands on the local system.
pub struct SystemRunner;

#[async_trait]
impl Runner for SystemRunner {
    async fn run(&self, cmd: &Cmd) -> Result<CmdOutput, PkgError> {
        let (program, args) = build_argv(cmd, is_root());
        let mut c = Command::new(&program);
        c.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        for (k, v) in envs(cmd) {
            c.env(k, v);
        }

        let start = Instant::now();
        let child = c.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PkgError::ToolMissing(program.clone())
            } else {
                PkgError::Exec(format!("cannot run {program}: {e}"))
            }
        })?;
        let pid = child.id();

        match tokio::time::timeout(cmd.timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => {
                let exit_code = o.status.code();
                Ok(CmdOutput {
                    success: exit_code.is_some_and(|c| cmd.ok_codes.contains(&c)),
                    exit_code,
                    timed_out: false,
                    stdout: lossy_capped(&o.stdout),
                    stderr: lossy_capped(&o.stderr),
                    duration_ms: start.elapsed().as_millis() as u64,
                })
            }
            Ok(Err(e)) => Err(PkgError::Exec(format!("wait failed: {e}"))),
            Err(_) => {
                // The future (and the child) is dropped; kill_on_drop kills the
                // child, also take down its process group (e.g. sudo -> apt).
                if let Some(pid) = pid {
                    // SAFETY: plain syscall on the process group of our own child.
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
                Ok(CmdOutput {
                    success: false,
                    exit_code: None,
                    timed_out: true,
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                })
            }
        }
    }
}

fn lossy_capped(b: &[u8]) -> String {
    String::from_utf8_lossy(&b[..b.len().min(MAX_OUTPUT)]).into_owned()
}

/// Scripted runner for unit tests: responses are matched on the exact
/// `program arg arg…` string and every executed command is recorded.
#[cfg(test)]
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
            self.calls.lock().unwrap().iter().map(|c| c.display()).collect()
        }
    }

    #[async_trait]
    impl Runner for FakeRunner {
        async fn run(&self, cmd: &Cmd) -> Result<CmdOutput, PkgError> {
            self.calls.lock().unwrap().push(cmd.clone());
            match self.responses.get(&cmd.display()) {
                Some(Ok(o)) => Ok(o.clone()),
                Some(Err(_)) => Err(PkgError::ToolMissing(cmd.program.to_string())),
                None => Err(PkgError::Exec(format!("unexpected command: {}", cmd.display()))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileged_as_root_runs_directly() {
        let c = Cmd::new("apt-get", &["update"]).privileged();
        let (p, a) = build_argv(&c, true);
        assert_eq!(p, "apt-get");
        assert_eq!(a, vec!["update"]);
    }

    #[test]
    fn privileged_non_root_uses_sudo_with_env() {
        let c = Cmd::new("apt-get", &["update"]).privileged().env("DEBIAN_FRONTEND", "noninteractive");
        let (p, a) = build_argv(&c, false);
        assert_eq!(p, "sudo");
        assert_eq!(&a[..3], ["-n", "--", "env"]);
        assert!(a.contains(&"DEBIAN_FRONTEND=noninteractive".to_string()));
        assert!(a.contains(&"LC_ALL=C".to_string()));
        assert_eq!(&a[a.len() - 2..], ["apt-get", "update"]);
    }

    #[test]
    fn unprivileged_never_uses_sudo() {
        let c = Cmd::new("apt", &["list"]);
        assert_eq!(build_argv(&c, false).0, "apt");
    }

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
        assert!(matches!(e, PkgError::ToolMissing(_)));
    }
}
