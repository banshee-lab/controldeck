//! `controldeck-agent`: the privileged half of ControlDeck.
//!
//! Runs as root and does everything that needs privileges, on behalf of the
//! unprivileged `controldeck` web server, over a Unix socket that only that
//! server can use (see `controldeck-agent-sdk`). Functionality is provided by
//! *plugins* compiled into this binary, one Cargo feature each; to add one:
//!
//! 1. create a crate implementing `controldeck_agent_sdk::Plugin`,
//! 2. add it as an optional dependency plus a feature in `Cargo.toml`,
//! 3. register it in [`registry`] below.
//!
//! Configuration (environment, see `packaging/agent.env`):
//!
//! | variable | default | meaning |
//! |---|---|---|
//! | `DECK_AGENT_SOCKET` | `/run/controldeck/agent.sock` | socket path |
//! | `DECK_AGENT_USER` | `controldeck` | the only account allowed to connect |
//! | `DECK_AGENT_PEER_EXE` | unset | if set, the peer must also run this executable |

use controldeck_agent_sdk::{
    Registry,
    peer::{PeerPolicy, lookup_user},
    proto::DEFAULT_SOCKET,
    server,
};
use std::{path::PathBuf, process::ExitCode, sync::Arc};

/// Every compiled-in plugin is registered here.
fn registry() -> Registry {
    let mut r = Registry::new();
    #[cfg(feature = "packages")]
    r.register(controldeck_pkgmgr::PackagesPlugin::from_env());
    r
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("cannot install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn run() -> Result<(), String> {
    let socket: PathBuf = std::env::var("DECK_AGENT_SOCKET").map(PathBuf::from).unwrap_or_else(|_| DEFAULT_SOCKET.into());
    let user_name = std::env::var("DECK_AGENT_USER").unwrap_or_else(|_| "controldeck".into());
    let peer_exe = std::env::var("DECK_AGENT_PEER_EXE").ok().filter(|s| !s.is_empty()).map(PathBuf::from);

    // Fail closed: without a valid, non-root client account nobody may connect.
    let user = lookup_user(&user_name).map_err(|e| format!("DECK_AGENT_USER '{user_name}': {e}"))?;
    if user.uid == 0 {
        return Err("DECK_AGENT_USER must not be root: the web server must run unprivileged".into());
    }
    let root = is_root();
    if !root {
        eprintln!("agent: WARNING: not running as root; privileged operations will fail (development mode)");
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let registry = registry();
        if registry.names().is_empty() {
            return Err("no plugins compiled in".to_string());
        }
        // Socket group = the web server's group (only when we can chown).
        let listener = server::bind(&socket, root.then_some(user.gid)).map_err(|e| format!("cannot create {}: {e}", socket.display()))?;
        let policy = PeerPolicy { allowed_uids: vec![user.uid], allowed_exe: peer_exe.clone() };
        println!(
            "controldeck-agent listening on {} (user: {user_name}, plugins: {}{})",
            socket.display(),
            registry.names().join(", "),
            peer_exe.map(|p| format!(", peer exe: {}", p.display())).unwrap_or_default()
        );
        server::serve(listener, Arc::new(registry), policy, shutdown_signal()).await;
        let _ = std::fs::remove_file(&socket);
        Ok(())
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("controldeck-agent: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use controldeck_agent_sdk::proto::AgentClient;
    use serde_json::Value;

    #[tokio::test]
    async fn default_build_serves_the_packages_plugin() {
        let path = std::env::temp_dir().join(format!("deck-agent-test-{}.sock", std::process::id()));
        let listener = server::bind(&path, None).unwrap();
        // SAFETY: trivial getter.
        let policy = PeerPolicy { allowed_uids: vec![unsafe { libc::geteuid() }], allowed_exe: None };
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(server::serve(listener, Arc::new(registry()), policy, async move {
            let _ = rx.await;
        }));
        let c = AgentClient::new(&path);
        let v = c.call("agent", "modules", Value::Null).await.unwrap();
        let names: Vec<&str> = v["modules"].as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["packages"]);
        assert_eq!(c.call("packages", "info", Value::Null).await.unwrap()["read_only"].is_boolean(), true);
        let _ = stop.send(());
    }
}
