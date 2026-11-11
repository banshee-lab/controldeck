//! apk (Alpine).

use async_trait::async_trait;
use super::{
    Cmd, CmdOutput, run, PackageManager, PkgError, Runner, SearchResult, SecuritySupport, Update,
    require_ok, split_name_version,
};
use std::collections::HashSet;

pub struct Apk;

/// Parses `apk version -l '<'`: `busybox-1.36-r0  <  1.36-r1` (header skipped).
pub fn parse_version_list(out: &str) -> Vec<Update> {
    out.lines()
        .filter_map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            if t.len() < 3 || t[1] != "<" {
                return None;
            }
            let (name, current) = split_name_version(t[0]);
            let mut u = Update::new(&name);
            u.current_version = current;
            u.new_version = Some(t[2].to_string());
            Some(u)
        })
        .collect()
}

/// Parses `apk search -v`: `name-1.0-r0 - description`.
pub fn parse_search(out: &str) -> Vec<SearchResult> {
    let mut seen = HashSet::new();
    out.lines()
        .filter_map(|l| {
            let (nv, desc) = match l.split_once(" - ") {
                Some((a, b)) => (a, Some(b.trim().to_string())),
                None => (l.trim(), None),
            };
            if nv.is_empty() {
                return None;
            }
            let (name, version) = split_name_version(nv);
            seen.insert(name.clone()).then(|| SearchResult { name, version, description: desc })
        })
        .collect()
}

#[async_trait]
impl PackageManager for Apk {
    fn name(&self) -> &'static str {
        "apk"
    }

    /// Alpine ships no advisory data in apk itself.
    fn security_support(&self) -> SecuritySupport {
        SecuritySupport::Unavailable
    }

    async fn refresh(&self, r: &dyn Runner) -> Result<CmdOutput, PkgError> {
        run(r, &Cmd::new("apk", &["update"]).timeout_secs(300)).await
    }

    async fn list_updates(&self, r: &dyn Runner) -> Result<Vec<Update>, PkgError> {
        let out = require_ok(run(r, &Cmd::new("apk", &["version", "-l", "<"])).await?, "apk version")?;
        Ok(parse_version_list(&out.stdout))
    }

    async fn install_updates(&self, r: &dyn Runner, packages: &[String], security_only: bool) -> Result<CmdOutput, PkgError> {
        if security_only {
            return Err(PkgError::Unsupported("apk cannot identify security updates".into()));
        }
        let cmd = if packages.is_empty() {
            Cmd::new("apk", &["upgrade"])
        } else {
            Cmd::new("apk", &["add", "-u"]).with_args(packages.to_vec())
        };
        run(r, &cmd.timeout_secs(1800)).await
    }

    async fn search(&self, r: &dyn Runner, query: &str) -> Result<Vec<SearchResult>, PkgError> {
        let out = require_ok(run(r, &Cmd::new("apk", &["search", "-v", "-d", query])).await?, "apk search")?;
        Ok(parse_search(&out.stdout))
    }

    async fn install(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError> {
        run(r, &Cmd::new("apk", &["add"]).with_args(packages.to_vec()).timeout_secs(1800)).await
    }

    async fn remove(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError> {
        run(r, &Cmd::new("apk", &["del"]).with_args(packages.to_vec()).timeout_secs(1800)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use controldeck_agent_sdk::exec::fake::FakeRunner;

    #[test]
    fn parses_version_list() {
        let u = parse_version_list("Installed:                                Available:\nbusybox-1.36.1-r0                      <  1.36.1-r1\npy3-foo-1.2-r0 < 1.3-r0\n");
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].name, "busybox");
        assert_eq!(u[0].current_version.as_deref(), Some("1.36.1-r0"));
        assert_eq!(u[0].new_version.as_deref(), Some("1.36.1-r1"));
        assert_eq!(u[1].name, "py3-foo");
        assert!(!u[1].security);
    }

    #[test]
    fn parses_search_dedup() {
        let r = parse_search("nginx-1.26.0-r0 - HTTP and reverse proxy server\nnginx-1.24.0-r0 - HTTP and reverse proxy server\nnginx-doc-1.26.0-r0 - docs\n");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].version.as_deref(), Some("1.26.0-r0"));
        assert_eq!(r[1].name, "nginx-doc");
    }

    #[tokio::test]
    async fn commands() {
        let r = FakeRunner::new()
            .on("apk update", "")
            .on("apk upgrade", "")
            .on("apk add -u curl", "")
            .on("apk add git", "");
        Apk.refresh(&r).await.unwrap();
        Apk.install_updates(&r, &[], false).await.unwrap();
        Apk.install_updates(&r, &["curl".into()], false).await.unwrap();
        Apk.install(&r, &["git".into()]).await.unwrap();
        assert!(matches!(Apk.install_updates(&r, &[], true).await, Err(PkgError::Unsupported(_))));
        assert_eq!(Apk.security_support(), SecuritySupport::Unavailable);
    }

    #[tokio::test]
    async fn remove_command() {
        let r = FakeRunner::new().on("apk del git", "");
        assert!(Apk.remove(&r, &["git".into()]).await.unwrap().success);
    }
}
