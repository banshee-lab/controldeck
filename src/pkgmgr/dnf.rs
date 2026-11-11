//! dnf / rpm (Fedora, RHEL and derivatives).

use async_trait::async_trait;
use super::{
    Cmd, CmdOutput, PackageManager, PkgError, Runner, SearchResult, SecuritySupport, Update,
    require_ok,
};
use std::collections::HashMap;

pub struct Dnf;

const ARCHES: [&str; 9] =
    ["x86_64", "noarch", "aarch64", "i686", "i386", "ppc64le", "s390x", "armv7hl", "src"];

fn strip_arch(s: &str) -> &str {
    match s.rsplit_once('.') {
        Some((n, a)) if ARCHES.contains(&a) => n,
        _ => s,
    }
}

/// Parses `dnf -q check-update`: `name.arch  version  repo`. Stops at the
/// "Obsoleting Packages" section.
pub fn parse_check_update(out: &str) -> Vec<Update> {
    let mut v = Vec::new();
    for line in out.lines() {
        if line.starts_with("Obsoleting Packages") {
            break;
        }
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() != 3 || !ARCHES.iter().any(|a| t[0].ends_with(&format!(".{a}"))) {
            continue;
        }
        let mut u = Update::new(strip_arch(t[0]));
        u.new_version = Some(t[1].to_string());
        u.repository = Some(t[2].to_string());
        v.push(u);
    }
    v
}

/// Splits `name-version-release.arch` into the package name.
fn nevra_name(nevra: &str) -> Option<&str> {
    let no_arch = strip_arch(nevra);
    let (rest, _release) = no_arch.rsplit_once('-')?;
    let (name, _version) = rest.rsplit_once('-')?;
    Some(name)
}

fn looks_like_advisory(t: &str) -> bool {
    let mut parts = t.splitn(2, '-');
    let prefix = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");
    !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_uppercase())
        && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// Parses `dnf -q updateinfo list --security` (dnf4 and dnf5 column layouts):
/// returns package name -> advisory id.
pub fn parse_updateinfo(out: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in out.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        let advisory = t.iter().find(|x| looks_like_advisory(x));
        let nevra = t.iter().find(|x| ARCHES.iter().any(|a| x.ends_with(&format!(".{a}"))));
        if let (Some(id), Some(n)) = (advisory, nevra) {
            if let Some(name) = nevra_name(n) {
                m.entry(name.to_string()).or_insert_with(|| id.to_string());
            }
        }
    }
    m
}

/// Parses `dnf -q search`: `name.arch : summary` (header lines ignored).
pub fn parse_search(out: &str) -> Vec<SearchResult> {
    let mut seen = std::collections::HashSet::new();
    out.lines()
        .filter(|l| !l.starts_with('='))
        .filter_map(|l| {
            let (name, desc) = l.split_once(" : ")?;
            let name = strip_arch(name.trim()).to_string();
            seen.insert(name.clone()).then(|| SearchResult {
                name,
                version: None,
                description: Some(desc.trim().to_string()),
            })
        })
        .collect()
}

#[async_trait]
impl PackageManager for Dnf {
    fn name(&self) -> &'static str {
        "dnf"
    }

    fn security_support(&self) -> SecuritySupport {
        SecuritySupport::Full
    }

    async fn refresh(&self, r: &dyn Runner) -> Result<CmdOutput, PkgError> {
        r.run(&Cmd::new("dnf", &["-y", "makecache", "--refresh"]).privileged().timeout_secs(300)).await
    }

    async fn list_updates(&self, r: &dyn Runner) -> Result<Vec<Update>, PkgError> {
        // check-update exits 100 when updates are available.
        let out = require_ok(
            r.run(&Cmd::new("dnf", &["-q", "check-update"]).ok_codes(&[0, 100]).timeout_secs(300)).await?,
            "dnf check-update",
        )?;
        let mut updates = parse_check_update(&out.stdout);
        if updates.is_empty() {
            return Ok(updates);
        }
        let info = require_ok(
            r.run(&Cmd::new("dnf", &["-q", "updateinfo", "list", "--security"]).timeout_secs(300)).await?,
            "dnf updateinfo",
        )?;
        let sec = parse_updateinfo(&info.stdout);
        for u in &mut updates {
            if let Some(id) = sec.get(&u.name) {
                u.security = true;
                u.advisory = Some(id.clone());
            }
        }
        Ok(updates)
    }

    async fn install_updates(&self, r: &dyn Runner, packages: &[String], security_only: bool) -> Result<CmdOutput, PkgError> {
        let mut args = vec!["-y", "upgrade"];
        if security_only {
            args.push("--security");
        }
        r.run(&Cmd::new("dnf", &args).with_args(packages.to_vec()).privileged().timeout_secs(1800)).await
    }

    async fn search(&self, r: &dyn Runner, query: &str) -> Result<Vec<SearchResult>, PkgError> {
        let out = require_ok(r.run(&Cmd::new("dnf", &["-q", "search", query]).timeout_secs(300)).await?, "dnf search")?;
        Ok(parse_search(&out.stdout))
    }

    async fn install(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError> {
        r.run(&Cmd::new("dnf", &["-y", "install"]).with_args(packages.to_vec()).privileged().timeout_secs(1800)).await
    }

    async fn remove(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError> {
        r.run(&Cmd::new("dnf", &["-y", "remove"]).with_args(packages.to_vec()).privileged().timeout_secs(1800)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkgmgr::exec::fake::FakeRunner;

    const CHECK: &str = "\nopenssl-libs.x86_64     1:3.2.2-1.fc40     updates\nvim-minimal.x86_64      2:9.1.100-1.fc40   updates\n\nObsoleting Packages\nfoo.x86_64  1.0-1  updates\n    bar.x86_64  0.9-1  @fedora\n";
    const INFO_DNF4: &str = "FEDORA-2024-abc123 Important/Sec. openssl-libs-1:3.2.2-1.fc40.x86_64\nFEDORA-2024-def456 Low/Sec.       unrelated-1.0-1.fc40.noarch\n";
    const INFO_DNF5: &str = "Name                Type     Severity  Package                       Issued\nFEDORA-2024-abc123  security Important openssl-libs-1:3.2.2-1.fc40.x86_64 2024-06-01\n";

    #[test]
    fn parses_check_update() {
        let u = parse_check_update(CHECK);
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].name, "openssl-libs");
        assert_eq!(u[0].new_version.as_deref(), Some("1:3.2.2-1.fc40"));
        assert_eq!(u[1].repository.as_deref(), Some("updates"));
    }

    #[test]
    fn parses_updateinfo_both_layouts() {
        for fixture in [INFO_DNF4, INFO_DNF5] {
            let m = parse_updateinfo(fixture);
            assert_eq!(m.get("openssl-libs").map(String::as_str), Some("FEDORA-2024-abc123"));
        }
    }

    #[tokio::test]
    async fn list_updates_flags_security() {
        let r = FakeRunner::new()
            .on("dnf -q check-update", CHECK)
            .on("dnf -q updateinfo list --security", INFO_DNF4);
        let u = Dnf.list_updates(&r).await.unwrap();
        assert!(u[0].security);
        assert_eq!(u[0].advisory.as_deref(), Some("FEDORA-2024-abc123"));
        assert!(!u[1].security);
    }

    #[tokio::test]
    async fn no_updates_skips_updateinfo() {
        let r = FakeRunner::new().on("dnf -q check-update", "");
        assert!(Dnf.list_updates(&r).await.unwrap().is_empty());
        assert_eq!(r.executed().len(), 1);
    }

    #[tokio::test]
    async fn check_update_accepts_exit_100() {
        let r = FakeRunner::new().on_output(
            "dnf -q check-update",
            CmdOutput { success: true, exit_code: Some(100), stdout: String::new(), ..Default::default() },
        );
        assert!(Dnf.list_updates(&r).await.is_ok());
        assert_eq!(r.calls.lock().unwrap()[0].ok_codes, vec![0, 100]);
    }

    #[test]
    fn parses_search_dedup() {
        let r = parse_search("========== Name & Summary Matched: nginx ==========\nnginx.x86_64 : A high performance web server\nnginx.aarch64 : A high performance web server\nnginx-mod-http.x86_64 : HTTP module\n");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "nginx");
    }

    #[tokio::test]
    async fn upgrade_commands() {
        let r = FakeRunner::new()
            .on("dnf -y upgrade", "")
            .on("dnf -y upgrade --security", "")
            .on("dnf -y upgrade vim", "")
            .on("dnf -y install git", "");
        Dnf.install_updates(&r, &[], false).await.unwrap();
        Dnf.install_updates(&r, &[], true).await.unwrap();
        Dnf.install_updates(&r, &["vim".into()], false).await.unwrap();
        Dnf.install(&r, &["git".into()]).await.unwrap();
        assert!(r.calls.lock().unwrap().iter().all(|c| c.privileged));
    }

    #[tokio::test]
    async fn remove_command() {
        let r = FakeRunner::new().on("dnf -y remove git", "");
        assert!(Dnf.remove(&r, &["git".into()]).await.unwrap().success);
        assert!(r.calls.lock().unwrap()[0].privileged);
    }
}
