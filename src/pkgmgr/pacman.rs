//! pacman (Arch and derivatives).

use super::{
    Cmd, CmdOutput, PackageManager, PkgError, Runner, SearchResult, SecuritySupport, Update,
    require_ok,
};
use std::collections::HashMap;

pub struct Pacman;

/// Parses `pacman -Qu`: `name 1.0-1 -> 1.1-1`.
pub fn parse_qu(out: &str) -> Vec<Update> {
    out.lines()
        .filter_map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            if t.len() < 4 || t[2] != "->" {
                return None;
            }
            let mut u = Update::new(t[0]);
            u.current_version = Some(t[1].to_string());
            u.new_version = Some(t[3].to_string());
            Some(u)
        })
        .collect()
}

/// Parses `arch-audit` output (`Package X is affected by CVE-…, CVE-…. High risk!`):
/// package name -> CVE list.
pub fn parse_arch_audit(out: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for l in out.lines() {
        let Some(rest) = l.strip_prefix("Package ") else { continue };
        let Some((name, tail)) = rest.split_once(" is affected by ") else { continue };
        let cves: Vec<&str> = tail
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(|t| t.trim_end_matches(['.', '!']))
            .filter(|t| t.starts_with("CVE-"))
            .collect();
        m.insert(name.to_string(), cves.join(","));
    }
    m
}

/// Parses `pacman -Ss`: `repo/name version [flags]` followed by an indented description.
pub fn parse_search(out: &str) -> Vec<SearchResult> {
    let mut v: Vec<SearchResult> = Vec::new();
    for l in out.lines() {
        if l.starts_with(char::is_whitespace) {
            if let Some(last) = v.last_mut() {
                last.description = Some(l.trim().to_string());
            }
        } else if let Some((id, rest)) = l.split_once(' ') {
            let name = id.rsplit_once('/').map_or(id, |(_, n)| n);
            v.push(SearchResult {
                name: name.to_string(),
                version: rest.split_whitespace().next().map(String::from),
                description: None,
            });
        }
    }
    v
}

impl PackageManager for Pacman {
    fn name(&self) -> &'static str {
        "pacman"
    }

    fn security_support(&self) -> SecuritySupport {
        SecuritySupport::Partial
    }

    fn refresh(&self, r: &dyn Runner) -> Result<CmdOutput, PkgError> {
        r.run(&Cmd::new("pacman", &["-Sy"]).privileged().timeout_secs(300))
    }

    fn list_updates(&self, r: &dyn Runner) -> Result<Vec<Update>, PkgError> {
        // Exit 1 means "no updates".
        let out = require_ok(r.run(&Cmd::new("pacman", &["-Qu"]).ok_codes(&[0, 1]))?, "pacman -Qu")?;
        let mut updates = parse_qu(&out.stdout);
        // Optional: arch-audit (exit 1 when vulnerable packages exist).
        match r.run(&Cmd::new("arch-audit", &["-u"]).ok_codes(&[0, 1]).timeout_secs(120)) {
            Ok(a) if a.success => {
                let vuln = parse_arch_audit(&a.stdout);
                for u in &mut updates {
                    if let Some(cves) = vuln.get(&u.name) {
                        u.security = true;
                        u.advisory = Some(cves.clone());
                    }
                }
            }
            Ok(_) | Err(PkgError::ToolMissing(_)) => {}
            Err(e) => return Err(e),
        }
        Ok(updates)
    }

    fn install_updates(&self, r: &dyn Runner, packages: &[String], security_only: bool) -> Result<CmdOutput, PkgError> {
        if security_only || !packages.is_empty() {
            return Err(PkgError::Unsupported(
                "Arch does not support partial upgrades; only a full system upgrade is allowed".into(),
            ));
        }
        r.run(&Cmd::new("pacman", &["-Su", "--noconfirm"]).privileged().timeout_secs(1800))
    }

    fn search(&self, r: &dyn Runner, query: &str) -> Result<Vec<SearchResult>, PkgError> {
        // Exit 1 means "no match".
        let out = require_ok(r.run(&Cmd::new("pacman", &["-Ss", query]).ok_codes(&[0, 1]))?, "pacman -Ss")?;
        Ok(parse_search(&out.stdout))
    }

    fn install(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError> {
        r.run(&Cmd::new("pacman", &["-S", "--noconfirm", "--needed"]).with_args(packages.to_vec()).privileged().timeout_secs(1800))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkgmgr::exec::fake::FakeRunner;

    const AUDIT: &str = "Package openssl is affected by CVE-2024-1, CVE-2024-2. High risk!\nPackage zlib is affected by CVE-2024-3. Low risk!\n";

    #[test]
    fn parses_qu() {
        let u = parse_qu("linux 6.6.1.arch1-1 -> 6.6.2.arch1-1\nopenssl 3.2.0-1 -> 3.2.1-1 [ignored]\nnoise\n");
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].name, "linux");
        assert_eq!(u[0].new_version.as_deref(), Some("6.6.2.arch1-1"));
        assert_eq!(u[1].current_version.as_deref(), Some("3.2.0-1"));
    }

    #[test]
    fn parses_arch_audit() {
        let m = parse_arch_audit(AUDIT);
        assert_eq!(m["openssl"], "CVE-2024-1,CVE-2024-2");
        assert_eq!(m["zlib"], "CVE-2024-3");
    }

    #[test]
    fn parses_search() {
        let r = parse_search("extra/nginx 1.26.0-1 [installed]\n    Lightweight HTTP server\ncore/nginx-mod 1.0-1\n    Module\n");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "nginx");
        assert_eq!(r[0].version.as_deref(), Some("1.26.0-1"));
        assert_eq!(r[0].description.as_deref(), Some("Lightweight HTTP server"));
    }

    #[test]
    fn list_flags_security_with_arch_audit() {
        let r = FakeRunner::new().on("pacman -Qu", "openssl 3.2.0-1 -> 3.2.1-1\nvim 9.0-1 -> 9.1-1\n").on("arch-audit -u", AUDIT);
        let u = Pacman.list_updates(&r).unwrap();
        assert!(u[0].security);
        assert!(!u[1].security);
    }

    #[test]
    fn list_works_without_arch_audit() {
        let r = FakeRunner::new().on("pacman -Qu", "vim 9.0-1 -> 9.1-1\n").missing("arch-audit -u");
        let u = Pacman.list_updates(&r).unwrap();
        assert_eq!(u.len(), 1);
        assert!(!u[0].security);
    }

    #[test]
    fn partial_upgrades_rejected() {
        let r = FakeRunner::new().on("pacman -Su --noconfirm", "");
        assert!(matches!(Pacman.install_updates(&r, &["vim".into()], false), Err(PkgError::Unsupported(_))));
        assert!(matches!(Pacman.install_updates(&r, &[], true), Err(PkgError::Unsupported(_))));
        assert!(Pacman.install_updates(&r, &[], false).unwrap().success);
    }
}
