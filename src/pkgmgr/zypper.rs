//! zypper (openSUSE, SLE).

use super::{
    Cmd, CmdOutput, PackageManager, PkgError, Runner, SearchResult, SecuritySupport, Update,
    parse_table, require_ok,
};
use std::collections::HashSet;

pub struct Zypper;

fn zypper(args: &[&str]) -> Cmd {
    let mut a = vec!["--non-interactive"];
    a.extend_from_slice(args);
    Cmd::new("zypper", &a)
}

/// Parses `zypper list-updates` (table: S | Repository | Name | Current Version | Available Version | Arch).
pub fn parse_list_updates(out: &str) -> Vec<Update> {
    parse_table(out)
        .into_iter()
        .filter_map(|row| {
            let mut u = Update::new(row.get("Name")?);
            u.current_version = row.get("Current Version").cloned();
            u.new_version = row.get("Available Version").cloned();
            u.repository = row.get("Repository").cloned();
            Some(u)
        })
        .collect()
}

/// Parses `zypper list-patches --category security`. Security fixes are
/// patches, which cannot be reliably mapped to packages, so each patch is
/// returned as its own entry (`name` = patch name, `advisory` = summary).
pub fn parse_security_patches(out: &str) -> Vec<Update> {
    parse_table(out)
        .into_iter()
        .filter_map(|row| {
            let mut u = Update::new(row.get("Name")?);
            u.repository = row.get("Repository").cloned();
            u.advisory = row.get("Summary").cloned();
            u.security = true;
            Some(u)
        })
        .collect()
}

/// Parses `zypper search` (table: S | Name | Summary | Type), packages only.
pub fn parse_search(out: &str) -> Vec<SearchResult> {
    let mut seen = HashSet::new();
    parse_table(out)
        .into_iter()
        .filter(|r| r.get("Type").is_none_or(|t| t == "package"))
        .filter_map(|r| {
            let name = r.get("Name")?.clone();
            seen.insert(name.clone()).then(|| SearchResult {
                name,
                version: None,
                description: r.get("Summary").cloned(),
            })
        })
        .collect()
}

impl PackageManager for Zypper {
    fn name(&self) -> &'static str {
        "zypper"
    }

    fn security_support(&self) -> SecuritySupport {
        SecuritySupport::Full
    }

    fn refresh(&self, r: &dyn Runner) -> Result<CmdOutput, PkgError> {
        r.run(&zypper(&["refresh"]).privileged().timeout_secs(300))
    }

    fn list_updates(&self, r: &dyn Runner) -> Result<Vec<Update>, PkgError> {
        let out = require_ok(r.run(&Cmd::new("zypper", &["-q", "--non-interactive", "list-updates"]).timeout_secs(300))?, "zypper list-updates")?;
        let mut updates = parse_list_updates(&out.stdout);
        let sec = require_ok(
            r.run(&Cmd::new("zypper", &["-q", "--non-interactive", "list-patches", "--category", "security"]).timeout_secs(300))?,
            "zypper list-patches",
        )?;
        updates.extend(parse_security_patches(&sec.stdout));
        Ok(updates)
    }

    fn install_updates(&self, r: &dyn Runner, packages: &[String], security_only: bool) -> Result<CmdOutput, PkgError> {
        let cmd = if security_only {
            if !packages.is_empty() {
                return Err(PkgError::Unsupported("zypper cannot restrict security patches to named packages".into()));
            }
            zypper(&["patch", "--category", "security", "--auto-agree-with-licenses"])
        } else {
            zypper(&["update", "--auto-agree-with-licenses"]).with_args(packages.to_vec())
        };
        r.run(&cmd.privileged().timeout_secs(1800))
    }

    fn search(&self, r: &dyn Runner, query: &str) -> Result<Vec<SearchResult>, PkgError> {
        let out = require_ok(r.run(&Cmd::new("zypper", &["-q", "--non-interactive", "search", query]).timeout_secs(300))?, "zypper search")?;
        Ok(parse_search(&out.stdout))
    }

    fn install(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError> {
        r.run(&zypper(&["install", "--auto-agree-with-licenses"]).with_args(packages.to_vec()).privileged().timeout_secs(1800))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkgmgr::exec::fake::FakeRunner;

    const LU: &str = "S | Repository | Name | Current Version | Available Version | Arch\n--+------------+------+-----------------+-------------------+-------\nv | Update     | bash | 5.2-1.1         | 5.2-2.1           | x86_64\nv | Update     | curl | 8.0-1.1         | 8.1-1.1           | x86_64\n";
    const LP: &str = "Repository | Name | Category | Severity | Interactive | Status | Since | Summary\n-----------+------+----------+----------+-------------+--------+-------+--------\nUpdate | openSUSE-SU-2024:1-1 | security | important | --- | needed | 2024-06-01 | Security update for curl\n";
    const SE: &str = "S | Name | Summary | Type\n--+------+---------+--------\n  | nginx | Web server | package\ni | nginx | Web server | package\n  | nginx | Web server | srcpackage\n  | nginx-extra | More | package\n";

    #[test]
    fn parses_updates() {
        let u = parse_list_updates(LU);
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].name, "bash");
        assert_eq!(u[0].current_version.as_deref(), Some("5.2-1.1"));
        assert_eq!(u[0].new_version.as_deref(), Some("5.2-2.1"));
        assert!(!u[0].security);
    }

    #[test]
    fn parses_security_patches() {
        let p = parse_security_patches(LP);
        assert_eq!(p.len(), 1);
        assert!(p[0].security);
        assert_eq!(p[0].name, "openSUSE-SU-2024:1-1");
        assert_eq!(p[0].advisory.as_deref(), Some("Security update for curl"));
    }

    #[test]
    fn parses_search_dedup_packages_only() {
        let r = parse_search(SE);
        assert_eq!(r.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["nginx", "nginx-extra"]);
    }

    #[test]
    fn list_combines_updates_and_patches() {
        let r = FakeRunner::new()
            .on("zypper -q --non-interactive list-updates", LU)
            .on("zypper -q --non-interactive list-patches --category security", LP);
        let u = Zypper.list_updates(&r).unwrap();
        assert_eq!(u.len(), 3);
        assert_eq!(u.iter().filter(|x| x.security).count(), 1);
    }

    #[test]
    fn commands() {
        let r = FakeRunner::new()
            .on("zypper --non-interactive refresh", "")
            .on("zypper --non-interactive update --auto-agree-with-licenses", "")
            .on("zypper --non-interactive update --auto-agree-with-licenses bash", "")
            .on("zypper --non-interactive patch --category security --auto-agree-with-licenses", "")
            .on("zypper --non-interactive install --auto-agree-with-licenses git", "");
        Zypper.refresh(&r).unwrap();
        Zypper.install_updates(&r, &[], false).unwrap();
        Zypper.install_updates(&r, &["bash".into()], false).unwrap();
        Zypper.install_updates(&r, &[], true).unwrap();
        Zypper.install(&r, &["git".into()]).unwrap();
        assert!(matches!(Zypper.install_updates(&r, &["bash".into()], true), Err(PkgError::Unsupported(_))));
    }
}
