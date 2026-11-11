//! SDK for the privileged ControlDeck agent.
//!
//! * [`Plugin`] / [`Registry`]: the extension point. A plugin is an ordinary
//!   Rust module compiled into the agent binary (one Cargo feature each) that
//!   exposes narrow, validated operations over the wire protocol in
//!   [`controldeck_proto`].
//! * [`server`]: the Unix-socket listener. It creates the socket with
//!   restrictive permissions and authenticates every connection through the
//!   kernel's `SO_PEERCRED` ([`peer`]) before reading a single byte.
//! * [`exec`] and [`jobs`]: shared building blocks for plugins that run
//!   commands (no shell, timeouts, capped output) and long-running operations
//!   with replayable output streams.

pub mod exec;
pub mod jobs;
pub mod peer;
mod registry;
pub mod server;

pub use controldeck_proto as proto;
pub use registry::{BUILTIN, EventStream, Plugin, Registry, params, unknown_method};
