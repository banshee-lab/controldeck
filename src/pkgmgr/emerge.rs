//! emerge / Portage (Gentoo).

use async_trait::async_trait;
use super::{
    Cmd, CmdOutput, PackageManager, PkgError, Runner, SearchResult, SecuritySupport, Update,
    require_ok, split_name_version,
};
use std::collections::HashMap;

pub struct Emerge;

fn strip_slot(s: &str) -> &str {
    s.split(':').next().unwrap_or(s)
}

/// Parses `emerge --pretend --update …` lines such as
/// `[ebuild     U  ] sys-libs/glibc-2.38-r10:2.2 [2.37-r7:2.2]`.
/// Only upgrades (`U`) are returned; new installs (`N`) and downgrades (`D`) are skipped.
pub fn parse_pretend(out: &str) -> Vec<Update> {
    out.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("[ebuild")?;
            let (flags, rest) = rest.split_once(']')?;
            if !flags.contains('U') || flags.contains('D') {
                return None;
            }
            let mut tok = rest.split_whitespace();
            let (name, new) = split_name_version(strip_slot(tok.next()?));
            let mut u = Update::new(&name);
            u.new_version = new;
            u.current_version = tok
                .find(|t| t.starts_with('['))
                .map(|t| strip_slot(t.trim_matches(|c| c == '[' || c == ']')).to_string());
            Some(u)
        })
        .collect()
}

/// Parses `glsa-check -l affected`: `202401-01 [N] Title (cat/pkg, cat/pkg2)`.
/// Returns package atom -> GLSA id.
pub fn parse_glsa(out: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for l in out.lines() {
        let id = match l.split_whitespace().next() {
            Some(i) if i.len() == 9 && i.as_bytes()[6] == b'-' && i.chars().filter(|c| c.is_ascii_digit()).count() == 8 => i,
            _ => continue,
        };
        let (Some(open), Some(close)) = (l.rfind('('), l.rfind(')')) else { continue };
        if open >= close {
            continue;
        }
        for atom in l[open + 1..close].split(|c: char| c == ',' || c.is_whitespace()) {
            if atom.contains('/') {
                m.entry(atom.to_string()).or_insert_with(|| id.to_string());
            }
        }
    }
    m
}

/// Parses `emerge --search`:
/// ```text
/// *  app-editors/vim
///       Latest version available: 9.1
///       Description:      Vim, an improved vi-style text editor
/// ```
pub fn parse_search(out: &str) -> Vec<SearchResult> {
    let mut v: Vec<SearchResult> = Vec::new();
    for l in out.lines() {
        if let Some(name) = l.strip_prefix("*  ") {
            v.push(SearchResult { name: name.trim().to_string(), version: None, description: None });
        } else if let Some(last) = v.last_mut() {
            let t = l.trim();
            if let Some(ver) = t.strip_prefix("Latest version available:") {
                last.version = Some(ver.trim().to_string());
            } else if let Some(d) = t.strip_prefix("Description:") {
                last.description = Some(d.trim().to_string());
            }
        }
    }
    v
}

#[async_trait]
impl PackageManager for Emerge {
    fn name(&self) -> &'static str {
        "emerge"
    }

    /// Needs `glsa-check` (app-portage/gentoolkit).
    fn security_support(&self) -> SecuritySupport {
        SecuritySupport::Partial
    }

    async fn refresh(&self, r: &dyn Runner) -> Result<CmdOutput, PkgError> {
        r.run(&Cmd::new("emerge", &["--sync", "--quiet"]).privileged().timeout_secs(1800)).await
    }

    async fn list_updates(&self, r: &dyn Runner) -> Result<Vec<Update>, PkgError> {
        let out = require_ok(
            r.run(&Cmd::new("emerge", &["--pretend", "--update", "--deep", "--newuse", "--color", "n", "--nospinner", "@world"]).timeout_secs(600)).await?,
            "emerge --pretend",
        )?;
        let mut updates = parse_pretend(&out.stdout);
        match r.run(&Cmd::new("glsa-check", &["-l", "affected"]).timeout_secs(300)).await {
            Ok(g) if g.success => {
                let glsa = parse_glsa(&g.stdout);
                // Updates carry bare names; GLSA atoms are category/package.
                // Resolve names from the pretend output which includes the category.
                for u in &mut updates {
                    if let Some(id) = glsa.get(&u.name) {
                        u.security = true;
                        u.advisory = Some(id.clone());
                    }
                }
            }
            Ok(_) | Err(PkgError::ToolMissing(_)) => {}
            Err(e) => return Err(e),
        }
        Ok(updates)
    }

    async fn install_updates(&self, r: &dyn Runner, packages: &[String], security_only: bool) -> Result<CmdOutput, PkgError> {
        if security_only {
            return Err(PkgError::Unsupported("emerge cannot restrict an update to security fixes".into()));
        }
        let cmd = if packages.is_empty() {
            Cmd::new("emerge", &["--update", "--deep", "--newuse", "--color", "n", "--nospinner", "@world"])
        } else {
            Cmd::new("emerge", &["--update", "--newuse", "--color", "n", "--nospinner"]).with_args(packages.to_vec())
        };
        r.run(&cmd.privileged().timeout_secs(7200)).await
    }

    async fn search(&self, r: &dyn Runner, query: &str) -> Result<Vec<SearchResult>, PkgError> {
        let out = require_ok(r.run(&Cmd::new("emerge", &["--search", "--color", "n", "--nospinner", query]).timeout_secs(300)).await?, "emerge --search")?;
        Ok(parse_search(&out.stdout))
    }

    async fn install(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError> {
        r.run(&Cmd::new("emerge", &["--noreplace", "--color", "n", "--nospinner"]).with_args(packages.to_vec()).privileged().timeout_secs(7200)).await
    }

    async fn remove(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError> {
        r.run(&Cmd::new("emerge", &["--unmerge", "--color", "n", "--nospinner"]).with_args(packages.to_vec()).privileged().timeout_secs(1800)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkgmgr::exec::fake::FakeRunner;

    const PRETEND: &str = "\nCalculating dependencies... done!\n[ebuild     U  ] sys-libs/glibc-2.38-r10:2.2 [2.37-r7:2.2]\n[ebuild  N     ] dev-libs/newdep-1.0\n[ebuild     UD ] app-misc/down-1.0 [2.0]\n[ebuild     U  ] dev-libs/openssl-3.0.13 [3.0.12-r1] USE=\"asm\"\n";
    const GLSA: &str = "Checking GLSAs\n202401-01 [N] OpenSSL: Multiple Vulnerabilities (dev-libs/openssl)\n202402-03 [A] Foo: bar (net-misc/foo, net-misc/baz)\n";

    #[test]
    fn parses_pretend() {
        let u = parse_pretend(PRETEND);
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].name, "sys-libs/glibc");
        assert_eq!(u[0].new_version.as_deref(), Some("2.38-r10"));
        assert_eq!(u[0].current_version.as_deref(), Some("2.37-r7"));
        assert_eq!(u[1].current_version.as_deref(), Some("3.0.12-r1"));
    }

    #[test]
    fn parses_glsa() {
        let m = parse_glsa(GLSA);
        assert_eq!(m["dev-libs/openssl"], "202401-01");
        assert_eq!(m["net-misc/baz"], "202402-03");
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn parses_search() {
        let r = parse_search("[ Results for search key : vim ]\nSearching...\n\n*  app-editors/vim\n      Latest version available: 9.1.0\n      Homepage:      https://vim.org\n      Description:   Vim, an improved vi-style text editor\n\n*  app-editors/gvim\n      Latest version available: 9.1.0\n");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "app-editors/vim");
        assert_eq!(r[0].version.as_deref(), Some("9.1.0"));
        assert_eq!(r[0].description.as_deref(), Some("Vim, an improved vi-style text editor"));
    }

    #[tokio::test]
    async fn list_flags_glsa_matches() {
        let r = FakeRunner::new()
            .on("emerge --pretend --update --deep --newuse --color n --nospinner @world", PRETEND)
            .on("glsa-check -l affected", GLSA);
        let u = Emerge.list_updates(&r).await.unwrap();
        assert!(!u[0].security);
        assert!(u[1].security);
        assert_eq!(u[1].advisory.as_deref(), Some("202401-01"));
    }

    #[tokio::test]
    async fn list_works_without_glsa_check() {
        let r = FakeRunner::new()
            .on("emerge --pretend --update --deep --newuse --color n --nospinner @world", PRETEND)
            .missing("glsa-check -l affected");
        assert_eq!(Emerge.list_updates(&r).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn commands() {
        let r = FakeRunner::new()
            .on("emerge --sync --quiet", "")
            .on("emerge --update --deep --newuse --color n --nospinner @world", "")
            .on("emerge --update --newuse --color n --nospinner app-editors/vim", "")
            .on("emerge --noreplace --color n --nospinner app-editors/vim", "");
        Emerge.refresh(&r).await.unwrap();
        Emerge.install_updates(&r, &[], false).await.unwrap();
        Emerge.install_updates(&r, &["app-editors/vim".into()], false).await.unwrap();
        Emerge.install(&r, &["app-editors/vim".into()]).await.unwrap();
        assert!(matches!(Emerge.install_updates(&r, &[], true).await, Err(PkgError::Unsupported(_))));
    }

    #[tokio::test]
    async fn remove_command() {
        let r = FakeRunner::new().on("emerge --unmerge --color n --nospinner git", "");
        assert!(Emerge.remove(&r, &["git".into()]).await.unwrap().success);
        assert!(r.calls.lock().unwrap()[0].privileged);
    }
}
