//! zypper (openSUSE, SLE).

use async_trait::async_trait;
use super::{
    Cmd, CmdOutput, PackageManager, PkgError, Runner, SearchResult, SecuritySupport, Update,
    require_ok,
};
use std::collections::HashSet;

pub struct Zypper;

fn zypper(args: &[&str]) -> Cmd {
    let mut a = vec!["--non-interactive"];
    a.extend_from_slice(args);
    Cmd::new("zypper", &a)
}

fn parse_xml(out: &str) -> Result<roxmltree::Document<'_>, PkgError> {
    roxmltree::Document::parse(out).map_err(|e| PkgError::Exec(format!("cannot parse zypper XML output: {e}")))
}

fn source_alias(n: roxmltree::Node) -> Option<String> {
    n.children().find(|c| c.has_tag_name("source")).and_then(|s| s.attribute("alias")).map(String::from)
}

/// Parses `zypper --xmlout list-updates` (`<update kind="package" name edition edition-old …>`).
pub fn parse_list_updates(out: &str) -> Result<Vec<Update>, PkgError> {
    let doc = parse_xml(out)?;
    Ok(doc
        .descendants()
        .filter(|n| n.has_tag_name("update") && n.attribute("kind") == Some("package"))
        .filter_map(|n| {
            let mut u = Update::new(n.attribute("name")?);
            u.current_version = n.attribute("edition-old").map(String::from);
            u.new_version = n.attribute("edition").map(String::from);
            u.repository = source_alias(n);
            Some(u)
        })
        .collect())
}

/// Parses `zypper --xmlout list-patches --category security`. Security fixes are
/// patches, which cannot be reliably mapped to packages, so each patch is
/// returned as its own entry (`name` = patch name, `advisory` = summary).
pub fn parse_security_patches(out: &str) -> Result<Vec<Update>, PkgError> {
    let doc = parse_xml(out)?;
    Ok(doc
        .descendants()
        .filter(|n| n.has_tag_name("update") && n.attribute("kind") == Some("patch"))
        .filter(|n| n.attribute("category") == Some("security"))
        .filter_map(|n| {
            let mut u = Update::new(n.attribute("name")?);
            u.repository = source_alias(n);
            u.advisory = n.children().find(|c| c.has_tag_name("summary")).and_then(|s| s.text()).map(|t| t.trim().to_string());
            u.security = true;
            Some(u)
        })
        .collect())
}

/// Parses `zypper --xmlout search` (`<solvable kind="package" name summary …>`).
pub fn parse_search(out: &str) -> Result<Vec<SearchResult>, PkgError> {
    let doc = parse_xml(out)?;
    let mut seen = HashSet::new();
    Ok(doc
        .descendants()
        .filter(|n| n.has_tag_name("solvable") && n.attribute("kind").is_none_or(|k| k == "package"))
        .filter_map(|n| {
            let name = n.attribute("name")?.to_string();
            seen.insert(name.clone()).then(|| SearchResult {
                name,
                version: None,
                description: n.attribute("summary").map(String::from),
            })
        })
        .collect())
}

#[async_trait]
impl PackageManager for Zypper {
    fn name(&self) -> &'static str {
        "zypper"
    }

    fn security_support(&self) -> SecuritySupport {
        SecuritySupport::Full
    }

    async fn refresh(&self, r: &dyn Runner) -> Result<CmdOutput, PkgError> {
        r.run(&zypper(&["refresh"]).privileged().timeout_secs(300)).await
    }

    async fn list_updates(&self, r: &dyn Runner) -> Result<Vec<Update>, PkgError> {
        let out = require_ok(r.run(&Cmd::new("zypper", &["--xmlout", "--non-interactive", "list-updates"]).timeout_secs(300)).await?, "zypper list-updates")?;
        let mut updates = parse_list_updates(&out.stdout)?;
        let sec = require_ok(
            r.run(&Cmd::new("zypper", &["--xmlout", "--non-interactive", "list-patches", "--category", "security"]).timeout_secs(300)).await?,
            "zypper list-patches",
        )?;
        updates.extend(parse_security_patches(&sec.stdout)?);
        Ok(updates)
    }

    async fn install_updates(&self, r: &dyn Runner, packages: &[String], security_only: bool) -> Result<CmdOutput, PkgError> {
        let cmd = if security_only {
            if !packages.is_empty() {
                return Err(PkgError::Unsupported("zypper cannot restrict security patches to named packages".into()));
            }
            zypper(&["patch", "--category", "security", "--auto-agree-with-licenses"])
        } else {
            zypper(&["update", "--auto-agree-with-licenses"]).with_args(packages.to_vec())
        };
        r.run(&cmd.privileged().timeout_secs(1800)).await
    }

    async fn search(&self, r: &dyn Runner, query: &str) -> Result<Vec<SearchResult>, PkgError> {
        let out = require_ok(r.run(&Cmd::new("zypper", &["--xmlout", "--non-interactive", "search", query]).timeout_secs(300)).await?, "zypper search")?;
        parse_search(&out.stdout)
    }

    async fn install(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError> {
        r.run(&zypper(&["install", "--auto-agree-with-licenses"]).with_args(packages.to_vec()).privileged().timeout_secs(1800)).await
    }

    async fn remove(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError> {
        r.run(&zypper(&["remove"]).with_args(packages.to_vec()).privileged().timeout_secs(1800)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkgmgr::exec::fake::FakeRunner;

    const LU: &str = r#"<?xml version='1.0'?>
<stream><update-status version="0.6"><update-list>
<update name="bash" kind="package" edition="5.2-2.1" edition-old="5.2-1.1" arch="x86_64"><source url="http://x/update" alias="repo-update"/><description>Shell</description></update>
<update name="curl" kind="package" edition="8.1-1.1" edition-old="8.0-1.1" arch="x86_64"><source url="http://x/update" alias="repo-update"/></update>
</update-list></update-status></stream>"#;
    const LP: &str = r#"<?xml version='1.0'?>
<stream><update-status version="0.6"><update-list>
<update name="openSUSE-SU-2024:1-1" kind="patch" edition="1" category="security" severity="important"><summary>Security update for curl</summary><description>d</description><source url="u" alias="repo-update"/></update>
<update name="openSUSE-RU-2024:2-1" kind="patch" edition="1" category="recommended" severity="low"><summary>Bugfix</summary></update>
</update-list></update-status></stream>"#;
    const SE: &str = r#"<?xml version='1.0'?>
<stream><search-result version="0.0"><solvable-list>
<solvable status="not-installed" name="nginx" summary="Web server" kind="package"/>
<solvable status="installed" name="nginx" summary="Web server" kind="package"/>
<solvable status="not-installed" name="nginx" summary="Web server" kind="srcpackage"/>
<solvable status="not-installed" name="nginx-extra" summary="More" kind="package"/>
</solvable-list></search-result></stream>"#;

    #[test]
    fn parses_updates() {
        let u = parse_list_updates(LU).unwrap();
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].name, "bash");
        assert_eq!(u[0].current_version.as_deref(), Some("5.2-1.1"));
        assert_eq!(u[0].new_version.as_deref(), Some("5.2-2.1"));
        assert_eq!(u[0].repository.as_deref(), Some("repo-update"));
        assert!(!u[0].security);
    }

    #[test]
    fn rejects_malformed_xml() {
        assert!(parse_list_updates("not xml").is_err());
    }

    #[test]
    fn parses_security_patches() {
        let p = parse_security_patches(LP).unwrap();
        assert_eq!(p.len(), 1);
        assert!(p[0].security);
        assert_eq!(p[0].name, "openSUSE-SU-2024:1-1");
        assert_eq!(p[0].advisory.as_deref(), Some("Security update for curl"));
    }

    #[test]
    fn parses_search_dedup_packages_only() {
        let r = parse_search(SE).unwrap();
        assert_eq!(r.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["nginx", "nginx-extra"]);
    }

    #[tokio::test]
    async fn list_combines_updates_and_patches() {
        let r = FakeRunner::new()
            .on("zypper --xmlout --non-interactive list-updates", LU)
            .on("zypper --xmlout --non-interactive list-patches --category security", LP);
        let u = Zypper.list_updates(&r).await.unwrap();
        assert_eq!(u.len(), 3);
        assert_eq!(u.iter().filter(|x| x.security).count(), 1);
    }

    #[tokio::test]
    async fn commands() {
        let r = FakeRunner::new()
            .on("zypper --non-interactive refresh", "")
            .on("zypper --non-interactive update --auto-agree-with-licenses", "")
            .on("zypper --non-interactive update --auto-agree-with-licenses bash", "")
            .on("zypper --non-interactive patch --category security --auto-agree-with-licenses", "")
            .on("zypper --non-interactive install --auto-agree-with-licenses git", "");
        Zypper.refresh(&r).await.unwrap();
        Zypper.install_updates(&r, &[], false).await.unwrap();
        Zypper.install_updates(&r, &["bash".into()], false).await.unwrap();
        Zypper.install_updates(&r, &[], true).await.unwrap();
        Zypper.install(&r, &["git".into()]).await.unwrap();
        assert!(matches!(Zypper.install_updates(&r, &["bash".into()], true).await, Err(PkgError::Unsupported(_))));
    }

    #[tokio::test]
    async fn remove_command() {
        let r = FakeRunner::new().on("zypper --non-interactive remove git", "");
        assert!(Zypper.remove(&r, &["git".into()]).await.unwrap().success);
        assert!(r.calls.lock().unwrap()[0].privileged);
    }
}
