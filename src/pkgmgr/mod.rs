//! Package manager abstraction layer.
//!
//! [`PackageManager`] exposes the operations ControlDeck needs (refresh the
//! cache, list updates / security fixes, install updates, search, install).
//! There is one implementation per distro family, each in its own file; the
//! running distro is detected from `/etc/os-release` ([`detect`]).
//!
//! Every implementation builds [`Cmd`]s (no shell) and runs them through a
//! [`Runner`]; output parsing lives in pure `parse_*` functions that are unit
//! tested with fixtures.

mod apk;
mod apt;
mod dnf;
mod emerge;
pub mod exec;
pub mod jobs;
mod pacman;
mod zypper;

use async_trait::async_trait;
use serde::Serialize;
use std::{fmt, sync::Arc};

pub use exec::{Cmd, CmdOutput, Runner, SystemRunner};

/// A pending update.
///
/// For managers that report security fixes as advisories rather than package
/// versions (zypper patches), `name` is the advisory name and `new_version`
/// is `None`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Update {
    pub name: String,
    pub current_version: Option<String>,
    pub new_version: Option<String>,
    pub repository: Option<String>,
    pub security: bool,
    /// Advisory identifier or description (CVE / GLSA / FEDORA-… / patch summary).
    pub advisory: Option<String>,
}

impl Update {
    pub(crate) fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            current_version: None,
            new_version: None,
            repository: None,
            security: false,
            advisory: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchResult {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
}

/// How reliably a manager can flag security updates.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecuritySupport {
    /// Built into the package manager.
    Full,
    /// Needs an optional tool (`arch-audit`, `glsa-check`); flags are absent without it.
    Partial,
    /// Not available; `security` is always false.
    Unavailable,
}

#[derive(Debug)]
pub enum PkgError {
    /// Operation not supported by this package manager.
    Unsupported(String),
    /// Invalid input (package names, query).
    Invalid(String),
    /// Command failed to run or exited with an error.
    Exec(String),
    /// A required executable is not installed.
    ToolMissing(String),
}

impl fmt::Display for PkgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PkgError::Unsupported(m) | PkgError::Invalid(m) | PkgError::Exec(m) => f.write_str(m),
            PkgError::ToolMissing(t) => write!(f, "required command not found: {t}"),
        }
    }
}

impl std::error::Error for PkgError {}

/// The operations every supported package manager provides.
#[async_trait]
pub trait PackageManager: Send + Sync {
    fn name(&self) -> &'static str;
    fn security_support(&self) -> SecuritySupport;
    /// Refresh the package metadata cache (e.g. `apt-get update`).
    async fn refresh(&self, r: &dyn Runner) -> Result<CmdOutput, PkgError>;
    /// Updates available from the *current* cache (run [`refresh`](Self::refresh) first).
    async fn list_updates(&self, r: &dyn Runner) -> Result<Vec<Update>, PkgError>;
    /// Install updates: all of them when `packages` is empty, otherwise only
    /// those named. `security_only` restricts to security fixes.
    async fn install_updates(
        &self,
        r: &dyn Runner,
        packages: &[String],
        security_only: bool,
    ) -> Result<CmdOutput, PkgError>;
    async fn search(&self, r: &dyn Runner, query: &str) -> Result<Vec<SearchResult>, PkgError>;
    async fn install(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError>;
    /// Remove installed packages.
    async fn remove(&self, r: &dyn Runner, packages: &[String]) -> Result<CmdOutput, PkgError>;
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerKind {
    Apt,
    Dnf,
    Zypper,
    Pacman,
    Emerge,
    Apk,
}

impl ManagerKind {
    pub fn build(self) -> Arc<dyn PackageManager> {
        match self {
            ManagerKind::Apt => Arc::new(apt::Apt),
            ManagerKind::Dnf => Arc::new(dnf::Dnf),
            ManagerKind::Zypper => Arc::new(zypper::Zypper),
            ManagerKind::Pacman => Arc::new(pacman::Pacman),
            ManagerKind::Emerge => Arc::new(emerge::Emerge),
            ManagerKind::Apk => Arc::new(apk::Apk),
        }
    }
}

/// Parses a manager name (used by the `DECK_PKG_MANAGER` override).
pub fn kind_from_name(s: &str) -> Option<ManagerKind> {
    match s.trim().to_ascii_lowercase().as_str() {
        "apt" | "apt-get" => Some(ManagerKind::Apt),
        "dnf" | "yum" | "rpm" => Some(ManagerKind::Dnf),
        "zypper" => Some(ManagerKind::Zypper),
        "pacman" => Some(ManagerKind::Pacman),
        "emerge" | "portage" => Some(ManagerKind::Emerge),
        "apk" => Some(ManagerKind::Apk),
        _ => None,
    }
}

/// Maps an os-release `ID` (or `ID_LIKE` entry) to a package manager.
pub fn kind_from_distro_id(id: &str) -> Option<ManagerKind> {
    match id {
        "debian" | "ubuntu" | "linuxmint" | "raspbian" | "pop" | "kali" | "elementary" | "devuan"
        | "neon" | "zorin" => Some(ManagerKind::Apt),
        "fedora" | "rhel" | "centos" | "rocky" | "almalinux" | "ol" | "amzn" | "nobara" => {
            Some(ManagerKind::Dnf)
        }
        "suse" | "opensuse" | "opensuse-leap" | "opensuse-tumbleweed" | "opensuse-slowroll"
        | "sles" | "sled" | "sles_sap" => Some(ManagerKind::Zypper),
        "arch" | "archarm" | "manjaro" | "endeavouros" | "cachyos" | "garuda" | "artix" => {
            Some(ManagerKind::Pacman)
        }
        "gentoo" | "funtoo" => Some(ManagerKind::Emerge),
        "alpine" | "postmarketos" => Some(ManagerKind::Apk),
        _ => None,
    }
}

fn os_release_value<'a>(content: &'a str, key: &str) -> Option<&'a str> {
    content.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches(|c| c == '"' || c == '\''))
    })
}

/// Returns `(distro id, manager)` from the contents of an os-release file:
/// `ID` first, then each `ID_LIKE` entry.
pub fn detect_from_os_release(content: &str) -> (String, Option<ManagerKind>) {
    let id = os_release_value(content, "ID").unwrap_or("unknown").to_ascii_lowercase();
    let kind = kind_from_distro_id(&id).or_else(|| {
        os_release_value(content, "ID_LIKE")
            .unwrap_or("")
            .split_whitespace()
            .find_map(|l| kind_from_distro_id(&l.to_ascii_lowercase()))
    });
    (id, kind)
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub distro_id: String,
    pub kind: Option<ManagerKind>,
}

/// Detects the running distro's package manager. `DECK_PKG_MANAGER` overrides
/// detection (useful for derivatives with an unknown `ID`).
pub fn detect() -> Detection {
    let content = ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let (distro_id, mut kind) = detect_from_os_release(&content);
    if let Some(k) = std::env::var("DECK_PKG_MANAGER").ok().and_then(|v| kind_from_name(&v)) {
        kind = Some(k);
    }
    Detection { distro_id, kind }
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

const MAX_PACKAGES: usize = 100;
const MAX_NAME_LEN: usize = 128;

/// Package names are passed as single argv entries (no shell), but are still
/// restricted so they can never be parsed as an option and stay sane.
pub fn validate_packages(packages: &[String]) -> Result<(), PkgError> {
    if packages.len() > MAX_PACKAGES {
        return Err(PkgError::Invalid(format!("too many packages (max {MAX_PACKAGES})")));
    }
    for p in packages {
        let ok = !p.is_empty()
            && p.len() <= MAX_NAME_LEN
            && !p.starts_with('-')
            && p.chars().all(|c| c.is_ascii_alphanumeric() || "+._:@/=~-".contains(c));
        if !ok {
            return Err(PkgError::Invalid(format!("invalid package name: {p:?}")));
        }
    }
    Ok(())
}

pub fn validate_query(q: &str) -> Result<(), PkgError> {
    let q = q.trim();
    if q.is_empty() || q.len() > MAX_NAME_LEN || q.starts_with('-') || q.chars().any(|c| c.is_control()) {
        return Err(PkgError::Invalid("invalid search query".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers shared by the implementations
// ---------------------------------------------------------------------------

/// Turns a non-successful command into an error with its first stderr line.
pub(crate) fn require_ok(out: CmdOutput, what: &str) -> Result<CmdOutput, PkgError> {
    if out.success {
        return Ok(out);
    }
    let why = if out.timed_out {
        "timed out".to_string()
    } else {
        let line = out.stderr.lines().chain(out.stdout.lines()).find(|l| !l.trim().is_empty());
        format!(
            "exit {}{}",
            out.exit_code.map_or("?".to_string(), |c| c.to_string()),
            line.map(|l| format!(": {}", l.trim())).unwrap_or_default()
        )
    };
    Err(PkgError::Exec(format!("{what} failed ({why})")))
}

/// Package names of security updates, optionally limited to `only`.
pub(crate) fn security_names(updates: &[Update], only: &[String]) -> Vec<String> {
    updates
        .iter()
        .filter(|u| u.security && (only.is_empty() || only.contains(&u.name)))
        .map(|u| u.name.clone())
        .collect()
}

/// Splits `name-1.2.3-r1` into `("name", "1.2.3-r1")`: the version starts at
/// the last `-` that is followed by a digit (apk and Portage naming).
pub(crate) fn split_name_version(s: &str) -> (String, Option<String>) {
    let b = s.as_bytes();
    let idx = (1..b.len()).rev().find(|&i| b[i - 1] == b'-' && b[i].is_ascii_digit());
    match idx {
        Some(i) => (s[..i - 1].to_string(), Some(s[i..].to_string())),
        None => (s.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_id() {
        let cases = [
            ("ID=ubuntu\nID_LIKE=debian", ManagerKind::Apt),
            ("ID=debian", ManagerKind::Apt),
            ("ID=fedora", ManagerKind::Dnf),
            ("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"", ManagerKind::Dnf),
            ("ID=\"opensuse-leap\"\nID_LIKE=\"suse opensuse\"", ManagerKind::Zypper),
            ("ID=opensuse-tumbleweed", ManagerKind::Zypper),
            ("ID=arch", ManagerKind::Pacman),
            ("ID=manjaro\nID_LIKE=arch", ManagerKind::Pacman),
            ("ID=gentoo", ManagerKind::Emerge),
            ("ID=alpine", ManagerKind::Apk),
        ];
        for (content, want) in cases {
            assert_eq!(detect_from_os_release(content).1, Some(want), "{content}");
        }
    }

    #[test]
    fn falls_back_to_id_like() {
        let (id, kind) = detect_from_os_release("ID=somederivative\nID_LIKE=\"ubuntu debian\"");
        assert_eq!(id, "somederivative");
        assert_eq!(kind, Some(ManagerKind::Apt));
    }

    #[test]
    fn unknown_distro_is_none() {
        assert_eq!(detect_from_os_release("ID=plan9").1, None);
        assert_eq!(detect_from_os_release("").1, None);
    }

    #[test]
    fn manager_names_roundtrip() {
        for (n, k) in [
            ("apt", ManagerKind::Apt),
            ("DNF", ManagerKind::Dnf),
            ("zypper", ManagerKind::Zypper),
            ("pacman", ManagerKind::Pacman),
            ("emerge", ManagerKind::Emerge),
            ("apk", ManagerKind::Apk),
        ] {
            let kind = kind_from_name(n).unwrap();
            assert_eq!(kind, k);
            assert_eq!(k.build().name(), n.to_ascii_lowercase());
        }
        assert!(kind_from_name("brew").is_none());
    }

    #[test]
    fn validates_package_names() {
        let ok = |v: &[&str]| validate_packages(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(ok(&["curl", "libstdc++6", "dev-libs/openssl", "python3.12", "pkg=1.2-3", "gtk+3"]).is_ok());
        for bad in ["", "-y", "--root=/x", "a b", "a;rm", "$(x)", "a\nb", "a|b", "`x`"] {
            assert!(ok(&[bad]).is_err(), "{bad:?} should be rejected");
        }
        let many: Vec<String> = (0..101).map(|i| format!("p{i}")).collect();
        assert!(validate_packages(&many).is_err());
    }

    #[test]
    fn validates_query() {
        assert!(validate_query("web server").is_ok());
        assert!(validate_query("").is_err());
        assert!(validate_query("  ").is_err());
        assert!(validate_query("--help").is_err());
        assert!(validate_query("a\0b").is_err());
    }

    #[test]
    fn splits_name_and_version() {
        assert_eq!(split_name_version("busybox-1.36.1-r0"), ("busybox".into(), Some("1.36.1-r0".into())));
        assert_eq!(split_name_version("py3-foo-1.2-r3"), ("py3-foo".into(), Some("1.2-r3".into())));
        assert_eq!(split_name_version("gtk+3.0-3.24.1-r0"), ("gtk+3.0".into(), Some("3.24.1-r0".into())));
        assert_eq!(split_name_version("glibc-2.38-r10"), ("glibc".into(), Some("2.38-r10".into())));
        assert_eq!(split_name_version("nover"), ("nover".into(), None));
    }

    #[test]
    fn require_ok_reports_first_stderr_line() {
        let out = CmdOutput { success: false, exit_code: Some(100), stderr: "\nE: boom\nmore".into(), ..Default::default() };
        let e = require_ok(out, "apt-get update").unwrap_err().to_string();
        assert_eq!(e, "apt-get update failed (exit 100: E: boom)");
    }
}
