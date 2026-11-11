//! The agent's Unix-socket server.

use crate::{Registry, peer::{Peer, PeerPolicy}};
use controldeck_proto::{Frame, Mode, Request, RpcError, read_frame, write_frame};
use futures_util::StreamExt;
use std::{
    ffi::CString,
    io,
    os::unix::{ffi::OsStrExt, fs::{FileTypeExt, PermissionsExt}},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    time::timeout,
};

/// Requests are small; anything bigger is a bug or an attack.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: usize = 32;

/// Creates the listening socket.
///
/// The socket is created under `umask 0177` (root-only), then handed to
/// `group` with mode `0660`; there is no moment at which it is reachable by
/// anyone else. With `group = None` it stays `0600` (same user only; used for
/// development and tests). An existing *socket* at `path` is replaced (stale
/// leftover); anything else there is an error. Must be called inside a Tokio
/// runtime.
pub fn bind(path: &Path, group: Option<u32>) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists and is not a socket", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // SAFETY: umask is a plain syscall; no async code runs between the two calls.
    let old = unsafe { libc::umask(0o177) };
    let bound = UnixListener::bind(path);
    // SAFETY: restores the previous mask.
    unsafe { libc::umask(old) };
    let listener = bound?;

    if let Some(gid) = group {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
        // SAFETY: valid C string; uid u32::MAX (-1) leaves the owner unchanged.
        let rc = unsafe { libc::chown(c.as_ptr(), u32::MAX, gid) };
        let res = if rc == 0 { std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660)) } else { Err(io::Error::last_os_error()) };
        if let Err(e) = res {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
    }
    Ok(listener)
}

/// Serves requests until `shutdown` completes.
///
/// Every connection is authenticated with [`PeerPolicy`] *before* any data is
/// read; rejected peers are disconnected silently (and logged).
pub async fn serve(listener: UnixListener, registry: Arc<Registry>, policy: PeerPolicy, shutdown: impl std::future::Future<Output = ()>) {
    let policy = Arc::new(policy);
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tokio::pin!(shutdown);
    loop {
        let stream = tokio::select! {
            _ = &mut shutdown => break,
            r = listener.accept() => match r {
                Ok((s, _)) => s,
                Err(e) => {
                    eprintln!("agent: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let peer = match stream.peer_cred() {
            Ok(c) => Peer { uid: c.uid(), gid: c.gid(), pid: c.pid() },
            Err(e) => {
                eprintln!("agent: rejected connection: cannot read peer credentials: {e}");
                continue;
            }
        };
        if let Err(why) = policy.check(&peer) {
            eprintln!("agent: rejected connection: {why}");
            continue;
        }
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            eprintln!("agent: too many concurrent connections; dropping one");
            continue;
        };
        let registry = registry.clone();
        tokio::spawn(async move {
            handle(stream, &registry).await;
            drop(permit);
        });
    }
}

async fn handle(stream: UnixStream, registry: &Registry) {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    let req: Request = match timeout(REQUEST_TIMEOUT, read_frame(&mut rd, MAX_REQUEST_BYTES)).await {
        Ok(Ok(Some(r))) => r,
        Ok(Ok(None)) | Err(_) => return,
        Ok(Err(e)) => {
            let _ = write_frame(&mut wr, &Frame::Error { error: RpcError::bad_request(e.to_string()) }).await;
            return;
        }
    };

    match req.mode {
        Mode::Call => {
            let frame = match registry.call(&req.module, &req.method, req.params).await {
                Ok(result) => Frame::Result { result },
                Err(error) => Frame::Error { error },
            };
            if let Err(e) = write_frame(&mut wr, &frame).await {
                // A result bigger than the frame limit would otherwise leave the caller hanging.
                if e.kind() != io::ErrorKind::BrokenPipe {
                    eprintln!("agent: cannot send response for {}.{}: {e}", req.module, req.method);
                }
            }
        }
        Mode::Stream => {
            let mut events = match registry.stream(&req.module, &req.method, req.params).await {
                Ok(s) => s,
                Err(error) => {
                    let _ = write_frame(&mut wr, &Frame::Error { error }).await;
                    return;
                }
            };
            if write_frame(&mut wr, &Frame::Open).await.is_err() {
                return;
            }
            let mut probe = [0u8; 64];
            loop {
                tokio::select! {
                    ev = events.next() => match ev {
                        Some(ev) => {
                            if write_frame(&mut wr, &Frame::Event(ev)).await.is_err() {
                                return;
                            }
                        }
                        None => {
                            let _ = write_frame(&mut wr, &Frame::End).await;
                            return;
                        }
                    },
                    // The client never sends anything after its request: any
                    // read result (EOF, error or data) means it is gone or
                    // misbehaving, so stop streaming.
                    _ = rd.read(&mut probe) => return,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventStream, Plugin, unknown_method};
    use controldeck_proto::{AgentClient, ErrorCode, Event, MAX_FRAME_BYTES};
    use serde_json::{Value, json};
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicBool, AtomicU32, Ordering},
    };
    use tokio::io::AsyncWriteExt;

    static N: AtomicU32 = AtomicU32::new(0);
    static DROPPED: AtomicBool = AtomicBool::new(false);

    struct DropFlag;
    impl Drop for DropFlag {
        fn drop(&mut self) {
            DROPPED.store(true, Ordering::SeqCst);
        }
    }

    struct Test;

    #[async_trait::async_trait]
    impl Plugin for Test {
        fn name(&self) -> &'static str {
            "test"
        }
        async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
            match method {
                "echo" => Ok(params),
                "fail" => Err(RpcError::conflict("busy").with_data(json!({"running_job": "j1"}))),
                m => Err(unknown_method("test", m)),
            }
        }
        async fn stream(&self, method: &str, _params: Value) -> Result<EventStream, RpcError> {
            match method {
                "three" => Ok(Box::pin(futures_util::stream::iter((0..3).map(|i| Event {
                    event: Some("n".into()),
                    id: Some(i.to_string()),
                    data: json!(i),
                })))),
                "forever" => {
                    let flag = DropFlag;
                    Ok(Box::pin(futures_util::stream::unfold(flag, |f| async move {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Some((Event { event: None, id: None, data: json!("tick") }, f))
                    })))
                }
                m => Err(unknown_method("test", m)),
            }
        }
    }

    fn me() -> u32 {
        // SAFETY: trivial getter.
        unsafe { libc::geteuid() }
    }

    fn sock() -> PathBuf {
        std::env::temp_dir().join(format!("deck-{}-{}.sock", std::process::id(), N.fetch_add(1, Ordering::SeqCst)))
    }

    fn start(policy: PeerPolicy) -> (PathBuf, tokio::sync::oneshot::Sender<()>) {
        let path = sock();
        let listener = bind(&path, None).unwrap();
        let mut reg = Registry::new();
        reg.register(Test);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(serve(listener, Arc::new(reg), policy, async move {
            let _ = rx.await;
        }));
        (path, tx)
    }

    fn allow_me() -> PeerPolicy {
        PeerPolicy { allowed_uids: vec![me()], allowed_exe: None }
    }

    #[tokio::test]
    async fn call_and_errors_roundtrip() {
        let (path, _stop) = start(allow_me());
        let c = AgentClient::new(&path);
        assert_eq!(c.call("test", "echo", json!({"a": 1})).await.unwrap(), json!({"a": 1}));
        let e = c.call("test", "fail", Value::Null).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Conflict);
        assert_eq!(e.data.unwrap()["running_job"], "j1");
        assert_eq!(c.call("test", "nope", Value::Null).await.unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(c.call("nope", "x", Value::Null).await.unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(c.call("agent", "ping", Value::Null).await.unwrap(), json!({"pong": true}));
    }

    #[tokio::test]
    async fn streams_events_then_ends_and_reports_open_errors() {
        let (path, _stop) = start(allow_me());
        let c = AgentClient::new(&path);
        let got: Vec<_> = c.stream("test", "three", Value::Null).await.unwrap().collect().await;
        let ids: Vec<_> = got.iter().map(|e| e.as_ref().unwrap().id.clone().unwrap()).collect();
        assert_eq!(ids, ["0", "1", "2"]);
        // Errors are reported by `stream()` itself, before any event.
        assert_eq!(c.stream("test", "nope", Value::Null).await.err().unwrap().code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn stream_stops_when_the_client_disconnects() {
        let (path, _stop) = start(allow_me());
        let c = AgentClient::new(&path);
        let mut s = c.stream("test", "forever", Value::Null).await.unwrap();
        assert!(s.next().await.unwrap().is_ok());
        drop(s);
        for _ in 0..100 {
            if DROPPED.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("server kept streaming to a closed connection");
    }

    #[tokio::test]
    async fn peers_outside_the_policy_get_nothing() {
        let (path, _stop) = start(PeerPolicy { allowed_uids: vec![me() + 1], allowed_exe: None });
        let e = AgentClient::new(&path).call("test", "echo", json!(1)).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Unavailable);
        // Fail closed: an empty allowlist denies everybody.
        let (path, _stop) = start(PeerPolicy::default());
        assert!(AgentClient::new(&path).call("test", "echo", json!(1)).await.is_err());
    }

    #[tokio::test]
    async fn exe_policy_is_enforced() {
        let exe = std::env::current_exe().unwrap();
        let (path, _s1) = start(PeerPolicy { allowed_uids: vec![me()], allowed_exe: Some(exe) });
        assert!(AgentClient::new(&path).call("test", "echo", json!(1)).await.is_ok());
        let (path, _s2) = start(PeerPolicy { allowed_uids: vec![me()], allowed_exe: Some("/bin/sh".into()) });
        assert!(AgentClient::new(&path).call("test", "echo", json!(1)).await.is_err());
    }

    #[tokio::test]
    async fn socket_is_private_and_stale_sockets_are_replaced() {
        let path = sock();
        let first = bind(&path, None).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        drop(first); // leaves a stale socket file behind
        assert!(path.exists());
        bind(&path, None).unwrap();
        // A regular file is never clobbered.
        let file = sock();
        std::fs::write(&file, "x").unwrap();
        assert_eq!(bind(&file, None).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x");
    }

    #[tokio::test]
    async fn garbage_and_oversized_requests_are_rejected() {
        let (path, _stop) = start(allow_me());
        let mut s = UnixStream::connect(&path).await.unwrap();
        s.write_all(b"this is not json\n").await.unwrap();
        let mut rd = BufReader::new(s);
        let f: Frame = read_frame(&mut rd, MAX_FRAME_BYTES).await.unwrap().unwrap();
        assert!(matches!(f, Frame::Error { error } if error.code == ErrorCode::BadRequest));

        let mut s = UnixStream::connect(&path).await.unwrap();
        let big = format!("{{\"module\":\"test\",\"method\":\"echo\",\"params\":\"{}\"}}\n", "x".repeat(MAX_REQUEST_BYTES + 10));
        let _ = s.write_all(big.as_bytes()).await;
        let mut rd = BufReader::new(s);
        let f: Option<Frame> = read_frame(&mut rd, MAX_FRAME_BYTES).await.unwrap_or(None);
        assert!(matches!(f, None | Some(Frame::Error { .. })));
    }
}
