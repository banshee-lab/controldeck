//! Authentication of the process on the other end of the socket.
//!
//! The kernel reports the peer's uid/gid/pid for a Unix socket connection
//! (`SO_PEERCRED`); it cannot be spoofed by the peer. This is the *primary*
//! access control. The socket's file permissions are a second layer.

use std::{ffi::CString, io, path::PathBuf};

/// A system account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
}

/// Looks up a user by name (`getpwnam_r`, so NSS sources work too).
pub fn lookup_user(name: &str) -> io::Result<User> {
    let cname = CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid user name"))?;
    let mut buf = vec![0u8; 1024];
    loop {
        // SAFETY: zeroed passwd is a valid out-parameter for getpwnam_r.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: all pointers are valid for the duration of the call and the
        // buffer length matches the buffer.
        let rc = unsafe { libc::getpwnam_r(cname.as_ptr(), &mut pwd, buf.as_mut_ptr().cast(), buf.len(), &mut result) };
        match rc {
            0 if result.is_null() => {
                return Err(io::Error::new(io::ErrorKind::NotFound, format!("user '{name}' does not exist")));
            }
            0 => return Ok(User { uid: pwd.pw_uid, gid: pwd.pw_gid }),
            libc::ERANGE if buf.len() < 1 << 20 => buf.resize(buf.len() * 2, 0),
            e => return Err(io::Error::from_raw_os_error(e)),
        }
    }
}

/// Credentials of a connected peer.
#[derive(Debug, Clone, Copy)]
pub struct Peer {
    pub uid: u32,
    pub gid: u32,
    pub pid: Option<i32>,
}

/// Who may talk to the agent. **Fail-closed**: an empty `allowed_uids` denies
/// everybody.
#[derive(Debug, Clone, Default)]
pub struct PeerPolicy {
    pub allowed_uids: Vec<u32>,
    /// When set, the peer process must also be running this executable
    /// (resolved through `/proc/<pid>/exe`). This keeps other processes that
    /// happen to run under the same account out. Best-effort hardening: the
    /// pid is that of the connecting process at `connect` time.
    pub allowed_exe: Option<PathBuf>,
}

impl PeerPolicy {
    pub fn check(&self, peer: &Peer) -> Result<(), String> {
        if !self.allowed_uids.contains(&peer.uid) {
            return Err(format!("uid {} is not allowed", peer.uid));
        }
        let Some(want) = &self.allowed_exe else { return Ok(()) };
        let pid = peer.pid.ok_or("peer pid unavailable")?;
        let link = std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|e| format!("cannot resolve exe of pid {pid}: {e}"))?;
        // A binary replaced by a package upgrade shows up as "<path> (deleted)".
        let got = link.to_string_lossy();
        let got = got.strip_suffix(" (deleted)").unwrap_or(&got).to_string();
        let want = std::fs::canonicalize(want).unwrap_or_else(|_| want.clone());
        if std::path::Path::new(&got) == want {
            Ok(())
        } else {
            Err(format!("pid {pid} runs {got}, expected {}", want.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Peer {
        // SAFETY: trivial getters.
        unsafe { Peer { uid: libc::geteuid(), gid: libc::getegid(), pid: Some(std::process::id() as i32) } }
    }

    #[test]
    fn looks_up_root_and_rejects_unknown() {
        assert_eq!(lookup_user("root").unwrap(), User { uid: 0, gid: 0 });
        assert_eq!(lookup_user("no-such-user-xyz").unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(lookup_user("a\0b").is_err());
    }

    #[test]
    fn empty_policy_denies_everyone() {
        assert!(PeerPolicy::default().check(&me()).is_err());
    }

    #[test]
    fn uid_allowlist() {
        let p = PeerPolicy { allowed_uids: vec![me().uid], allowed_exe: None };
        assert!(p.check(&me()).is_ok());
        assert!(p.check(&Peer { uid: me().uid + 1, ..me() }).is_err());
    }

    #[test]
    fn exe_must_match() {
        let exe = std::env::current_exe().unwrap();
        let ok = PeerPolicy { allowed_uids: vec![me().uid], allowed_exe: Some(exe) };
        assert!(ok.check(&me()).is_ok());
        let bad = PeerPolicy { allowed_uids: vec![me().uid], allowed_exe: Some("/usr/bin/definitely-not-this".into()) };
        assert!(bad.check(&me()).is_err());
        assert!(ok.check(&Peer { pid: None, ..me() }).is_err());
    }
}
