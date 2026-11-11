//! Client used by the unprivileged server to talk to the agent.
//!
//! Each call opens its own connection (one request per connection), so a slow
//! or stuck stream can never block other calls.

use crate::{Event, Frame, MAX_FRAME_BYTES, Mode, Request, RpcError, read_frame, write_frame};
use futures_util::Stream;
use serde_json::Value;
use std::{path::PathBuf, pin::Pin, time::Duration};
use tokio::{io::BufReader, net::UnixStream, time::timeout};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Upper bound for a unary call (long package operations are jobs, not calls).
const CALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Events of a stream; an `Err` item is terminal.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<Event, RpcError>> + Send>>;

#[derive(Debug, Clone)]
pub struct AgentClient {
    socket: PathBuf,
}

fn unavailable(what: &str, e: impl std::fmt::Display) -> RpcError {
    RpcError::unavailable(format!("privileged agent unavailable ({what}: {e})"))
}

impl AgentClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self { socket: socket.into() }
    }

    async fn open(&self, module: &str, method: &str, params: Value, mode: Mode) -> Result<BufReader<UnixStream>, RpcError> {
        let stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket))
            .await
            .map_err(|_| unavailable("connect", "timed out"))?
            .map_err(|e| unavailable("connect", e))?;
        let mut stream = BufReader::new(stream);
        let req = Request { module: module.into(), method: method.into(), params, mode };
        // The connection stays fully open (no half-close): the agent treats EOF
        // from us as "client went away" and stops streaming.
        write_frame(stream.get_mut(), &req).await.map_err(|e| unavailable("send", e))?;
        Ok(stream)
    }

    /// Calls `module.method` and returns its JSON result.
    pub async fn call(&self, module: &str, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut rd = self.open(module, method, params, Mode::Call).await?;
        let frame = timeout(CALL_TIMEOUT, read_frame::<_, Frame>(&mut rd, MAX_FRAME_BYTES))
            .await
            .map_err(|_| unavailable("receive", "timed out"))?
            .map_err(|e| unavailable("receive", e))?;
        match frame {
            Some(Frame::Result { result }) => Ok(result),
            Some(Frame::Error { error }) => Err(error),
            Some(_) => Err(RpcError::internal("unexpected frame from agent")),
            None => Err(unavailable("receive", "connection closed")),
        }
    }

    /// Starts `module.method` as a stream. Fails (with the agent's error)
    /// when the agent rejects the request; otherwise yields events until the
    /// agent ends the stream.
    pub async fn stream(&self, module: &str, method: &str, params: Value) -> Result<EventStream, RpcError> {
        let mut rd = self.open(module, method, params, Mode::Stream).await?;
        let first = timeout(CALL_TIMEOUT, read_frame::<_, Frame>(&mut rd, MAX_FRAME_BYTES))
            .await
            .map_err(|_| unavailable("receive", "timed out"))?
            .map_err(|e| unavailable("receive", e))?;
        match first {
            Some(Frame::Open) => {}
            Some(Frame::Error { error }) => return Err(error),
            Some(_) => return Err(RpcError::internal("unexpected frame from agent")),
            None => return Err(unavailable("receive", "connection closed")),
        }
        let s = futures_util::stream::unfold(Some(rd), |state| async move {
            let mut rd = state?;
            match read_frame::<_, Frame>(&mut rd, MAX_FRAME_BYTES).await {
                Ok(Some(Frame::Event(ev))) => Some((Ok(ev), Some(rd))),
                Ok(Some(Frame::End)) => None,
                Ok(Some(Frame::Error { error })) => Some((Err(error), None)),
                Ok(Some(_)) => Some((Err(RpcError::internal("unexpected frame from agent")), None)),
                Ok(None) => Some((Err(unavailable("stream", "connection closed")), None)),
                Err(e) => Some((Err(unavailable("stream", e)), None)),
            }
        });
        Ok(Box::pin(s))
    }
}
