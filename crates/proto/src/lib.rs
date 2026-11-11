//! Wire protocol between the unprivileged ControlDeck server and the
//! privileged agent.
//!
//! Transport: a Unix domain socket. Framing: one JSON document per line
//! (newline-delimited JSON). Every connection carries exactly **one request**
//! and its response, so there is no multiplexing state to get wrong:
//!
//! ```text
//! client -> {"module":"packages","method":"updates","params":{"security":true}}
//! agent  -> {"type":"result","result":{...}}                       (call)
//!
//! client -> {"module":"packages","method":"job_stream","params":{"id":"x"},"mode":"stream"}
//! agent  -> {"type":"open"}
//! agent  -> {"type":"event","event":"output","id":"0","data":{...}}  (repeated)
//! agent  -> {"type":"end"}                                          (stream)
//!
//! any failure -> {"type":"error","error":{"code":"bad_request","message":"..."}}
//! ```
//!
//! The protocol is deliberately generic: a *module* (plugin) owns its methods
//! and their JSON parameters, so adding a plugin never changes this crate.

mod client;
mod codec;

pub use client::{AgentClient, EventStream};
pub use codec::{MAX_FRAME_BYTES, read_frame, write_frame};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// Default location of the agent socket.
pub const DEFAULT_SOCKET: &str = "/run/controldeck/agent.sock";

/// How the agent should answer a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// One [`Frame::Result`] (or [`Frame::Error`]).
    #[default]
    Call,
    /// [`Frame::Open`], any number of [`Frame::Event`]s, then [`Frame::End`].
    Stream,
}

/// The single request of a connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Plugin name, e.g. `packages`.
    pub module: String,
    /// Method within the plugin, e.g. `refresh`.
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub mode: Mode,
}

/// One item of a stream (maps 1:1 onto a Server-Sent Event in the HTTP layer).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub data: Value,
}

/// A line sent by the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Frame {
    Result { result: Value },
    Open,
    Event(Event),
    End,
    Error { error: RpcError },
}

/// Machine-readable failure class; the HTTP layer maps these to status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    Forbidden,
    NotFound,
    Conflict,
    Unsupported,
    Internal,
    /// The agent could not be reached (client side only).
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
    /// Optional structured detail (e.g. `{"running_job": "…"}` on a conflict).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, m)
    }

    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Forbidden, m)
    }

    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, m)
    }

    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, m)
    }

    pub fn unsupported(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, m)
    }

    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, m)
    }

    pub fn unavailable(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, m)
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RpcError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_defaults() {
        let r: Request = serde_json::from_str(r#"{"module":"m","method":"x"}"#).unwrap();
        assert_eq!(r.mode, Mode::Call);
        assert!(r.params.is_null());
    }

    #[test]
    fn frames_use_a_type_tag() {
        let f = Frame::Event(Event { event: Some("output".into()), id: Some("3".into()), data: json!({"a": 1}) });
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v, json!({"type": "event", "event": "output", "id": "3", "data": {"a": 1}}));
        assert_eq!(serde_json::from_value::<Frame>(v).unwrap(), f);
        assert_eq!(serde_json::to_value(Frame::End).unwrap(), json!({"type": "end"}));
        let e = Frame::Error { error: RpcError::conflict("busy").with_data(json!({"running_job": "j"})) };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["error"]["code"], "conflict");
        assert_eq!(serde_json::from_value::<Frame>(v).unwrap(), e);
    }
}
