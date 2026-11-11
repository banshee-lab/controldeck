//! apt (Debian, Ubuntu and derivatives).

use super::{
    Cmd, CmdOutput, PackageManager, PkgError, Runner, SearchResult, SecuritySupport, Update,
    require_ok, security_names,
};

pub struct Apt;

const NONINTERACTIVE: &str = "noninteractive";

fn apt_get(args: &[&str]) -> Cmd {
    Cmd::new("apt-get", args).privileged().env("DEBIAN_FRONTEND", NONINTERACTIVE)
}

const CONF_OPTS: [&str; 4] =
    ["-o", "Dpkg::Options::=--force-confdef", "-o", "Dpkg::Options::=--force-confold"];

/// Parses `apt list --upgradable`:
/// `curl/noble-updates,noble-security 8.5.0-2 amd64 [upgradable from: 8.4.0-1]`.
/// A package is a security update when one of its suites ends in `-security`
/// (also matches Ubuntu Pro `-infra-security` / `-apps-security`).
pub fn parse_upgradable(out: &str) -> Vec<Update> {
    out.lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once('/')?;
            if name.contains(char::is_whitespace) {
                return None;
            }
            let mut tok = rest.split_whitespace();
            let suites = tok.next()?;
            let new_version = tok.next()?;
            let current = rest.split_once("upgradable from:").map(|(_, v)| v.trim().trim_end_matches(']').to_string());
            let mut u = Update::new(name);
            u.new_version = Some(new_version.to_string());
            u.current_version = current;
            u.repository = Some(suites.to_string());
            u.security = suites.split(',').any(|s| s.ends_with("-security"));
            Some(u)
        })
        .collect()
}

/// Parses `apt-cache search`: `name - description`.
pub fn parse_search(out: &str) -> Vec<SearchResult> {
    out.lines()
        .filter_map(|l| {
            let (name, desc) = l.split_once(" - ")?;
            Some(SearchResult {
                name: name.trim().to_string(),
                version: None,
                description: Some(desc.trim().to_string()),
            })
        })
        .collect()
}

impl PackageManager for Apt {
    fn name(&self) -> &'static str {
        "apt"
    }

    fn security_support(&self) -> SecuritySupport {
        SecuritySupport::Full
    }

    fn refresh(&self, r: &dyn Runner) -> Result<CmdOutput, PkgError> {
        r.run(&apt_get(&["update"]).timeout_secs(300))
    }

    fn list_updates(&self, r: &dyn Runner) -> Result<Vec<Update>, PkgError> {
        let out = require_ok(r.run(&Cmd::new("apt", &["list", "--upgradable"]))?, "apt list --upgradable")?;
        Ok(parse_upgradable(&out.stdout))
    }

    fn install_updates(&self, r: &dyn Runner, packages: &[String], security_only: bool) -> Result<CmdOutput, PkgError> {
        let names: Vec<String> = if security_only {
            let updates = self.list_updates(r)?;
            let names = security_names(&updates, packages);
            if names.is_empty() {
                return Ok(CmdOutput { success: true, exit_code: Some(0), stdout: "no security updates to install\n".into(), ..Default::default() });
            }
            names
        } else {
            packages.to_vec()
        };
        let cmd = if names.is_empty() {
            apt_get(&["upgrade", "-y"]).with_args(CONF_OPTS.map(String::from))
        } else {
            apt_get(&["install", "-y", "--only-upgrade"])
                .with_args(CONF_OPTS.map(String::from))
                .with_args(names)
        };
        r.run(&cmd.timeout_secs(1800))
    }

    fn search(&self, r: &dyn Runner, query: &str) -> Result<Vec<SearchResult>, PkgError> {
        let out = require_ok(r.run(&Cmd::new("apt-cache", &["search", query]))?, "apt-cache search")?;
        Ok(parse_search(&out.stdout))
    }

    fn install(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError> {
        let cmd = apt_get(&["install", "-y"]).with_args(CONF_OPTS.map(String::from)).with_args(packages.to_vec());
        r.run(&cmd.timeout_secs(1800))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkgmgr::exec::fake::FakeRunner;

    const UPGRADABLE: &str = "Listing...\n\
curl/noble-updates,noble-security 8.5.0-2ubuntu10.6 amd64 [upgradable from: 8.5.0-2ubuntu10.5]\n\
vim/noble-updates 2:9.1.0016-1ubuntu7.5 amd64 [upgradable from: 2:9.1.0016-1ubuntu7.4]\n\
openssl/stable-security 3.0.15-1~deb12u1 amd64 [upgradable from: 3.0.14-1~deb12u2]\n";

    #[test]
    fn parses_upgradable() {
        let u = parse_upgradable(UPGRADABLE);
        assert_eq!(u.len(), 3);
        assert_eq!(u[0].name, "curl");
        assert_eq!(u[0].new_version.as_deref(), Some("8.5.0-2ubuntu10.6"));
        assert_eq!(u[0].current_version.as_deref(), Some("8.5.0-2ubuntu10.5"));
        assert_eq!(u[0].repository.as_deref(), Some("noble-updates,noble-security"));
        assert!(u[0].security);
        assert!(!u[1].security);
        assert!(u[2].security);
    }

    #[test]
    fn parses_search() {
        let r = parse_search("nginx - small, powerful, scalable web/proxy server\nnginx-common - common files - shared\nnoise line\n");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "nginx");
        assert_eq!(r[1].description.as_deref(), Some("common files - shared"));
    }

    #[test]
    fn install_builds_noninteractive_command() {
        let r = FakeRunner::new().on("apt-get install -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold curl git", "ok");
        let o = Apt.install(&r, &["curl".into(), "git".into()]).unwrap();
        assert!(o.success);
        let calls = r.calls.lock().unwrap();
        assert!(calls[0].privileged);
        assert!(calls[0].env.contains(&("DEBIAN_FRONTEND", "noninteractive")));
    }

    #[test]
    fn upgrade_all_and_selected() {
        let r = FakeRunner::new()
            .on("apt-get upgrade -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold", "")
            .on("apt-get install -y --only-upgrade -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold vim", "");
        Apt.install_updates(&r, &[], false).unwrap();
        Apt.install_updates(&r, &["vim".into()], false).unwrap();
        assert_eq!(r.executed().len(), 2);
    }

    #[test]
    fn security_only_installs_just_security_packages() {
        let r = FakeRunner::new()
            .on("apt list --upgradable", UPGRADABLE)
            .on("apt-get install -y --only-upgrade -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold curl openssl", "");
        assert!(Apt.install_updates(&r, &[], true).unwrap().success);
    }

    #[test]
    fn security_only_with_nothing_pending_is_a_noop() {
        let r = FakeRunner::new().on("apt list --upgradable", "Listing...\n");
        let o = Apt.install_updates(&r, &[], true).unwrap();
        assert!(o.success);
        assert_eq!(r.executed(), vec!["apt list --upgradable"]);
    }
}
