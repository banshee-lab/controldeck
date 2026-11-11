# Architecture: public server and privileged agent

ControlDeck is split into two processes so that the part exposed to the network never
holds privileges.

```
 browser ──HTTP──▶ controldeck            (user: controldeck, unprivileged)
                     │  static SPA, /api/*, metrics collector, SQLite
                     │  Unix socket  /run/controldeck/agent.sock
                     ▼
                  controldeck-agent       (user: root)
                     plugins: packages, …   ← anything that needs root
```

| Crate | Role |
|---|---|
| `controldeck` (repo root) | Public binary. Serves the embedded SPA and the HTTP API, collects metrics (`/proc`, `/sys`: no privileges needed). Privileged API routes are thin gateways that forward to the agent. |
| `crates/proto` | Wire protocol and `AgentClient`. Generic: knows nothing about any plugin. |
| `crates/agent-sdk` | `Plugin` trait, plugin `Registry`, the Unix-socket server with peer authentication, plus shared building blocks (`exec`: commands without a shell; `jobs`: background jobs with replayable output). |
| `crates/pkgmgr` | The `packages` plugin (apt, dnf, zypper, pacman, emerge, apk). |
| `crates/agent` | The `controldeck-agent` binary: reads config, registers the compiled-in plugins, serves. |

## Why a Unix domain socket

* Local only: no network surface, nothing to bind or firewall.
* The kernel authenticates the peer: `SO_PEERCRED` yields the connecting process's
  uid/gid/pid and cannot be forged by it.
* Access control comes for free from file permissions, and a stream protocol carries
  long-running output without extra dependencies.

Heavier options (gRPC, D-Bus + polkit, HTTP over a socket) add dependencies and moving
parts without improving the security properties.

## How the agent is locked down

Defence in depth, strictest check first:

1. **File permissions.** The agent creates the socket under `umask 0177`, then hands it
   to the web server's group: `root:controldeck`, mode `0660`. There is never a moment at
   which another account can connect. Processes of other users get `EACCES`.
2. **Peer credentials (primary control).** Before reading a single byte, the agent checks
   the connecting uid against `DECK_AGENT_USER`. It is fail-closed: if the account cannot
   be resolved, or is `root`, the agent refuses to start. Rejected connections are dropped
   and logged.
3. **Peer executable (optional, enabled in the packages).** With `DECK_AGENT_PEER_EXE`
   the process must also be running that binary (`/proc/<pid>/exe`), which keeps other
   processes under the same account out. A binary replaced during an upgrade
   (`… (deleted)`) is still accepted. This is best-effort hardening: the pid is the
   connecting process at `connect` time.
4. **Narrow, typed API.** There is no "run this command". A plugin exposes specific
   operations and validates every parameter itself, treating the web process as
   untrusted (it is the part that faces the network). Commands are argv arrays, never
   shell strings, with a fixed environment.
5. **Limits.** One request per connection, 1 MiB request limit, 10 s to send it, at most
   32 concurrent connections.

Operational rules: nothing else may run as the `controldeck` user, and the socket
directory must not be made world-writable. The agent itself is intentionally not
sandboxed by systemd (package managers need the whole system), so the boundary is the
socket plus the plugin's validation.

## Wire protocol (`controldeck-proto`)

Newline-delimited JSON over the socket, **one request per connection**.

```text
→ {"module":"packages","method":"updates","params":{"security":true}}
← {"type":"result","result":{…}}

→ {"module":"packages","method":"job_stream","params":{"id":"…","after":4},"mode":"stream"}
← {"type":"open"}
← {"type":"event","event":"output","id":"5","data":{…}}      (repeated)
← {"type":"end"}

errors: ← {"type":"error","error":{"code":"conflict","message":"…","data":{"running_job":"…"}}}
```

Error codes (`bad_request`, `forbidden`, `not_found`, `conflict`, `unsupported`,
`internal`; `unavailable` is client-side) map to HTTP 400/403/404/409/501/500/503.
The built-in module `agent` answers `ping` and `modules`; the web server exposes the
latter as `GET /api/agent/status`.

## Writing a plugin

A plugin is an ordinary Rust crate compiled into the agent.

1. Create `crates/<name>` depending on `controldeck-agent-sdk` and implement `Plugin`:

   ```rust
   use controldeck_agent_sdk::{Plugin, params, unknown_method};
   use controldeck_proto::RpcError;
   use serde_json::{Value, json};

   pub struct Hello;

   #[async_trait::async_trait]
   impl Plugin for Hello {
       fn name(&self) -> &'static str { "hello" }

       async fn call(&self, method: &str, p: Value) -> Result<Value, RpcError> {
           match method {
               "whoami" => Ok(json!({ "uid": unsafe { libc::geteuid() } })),
               m => Err(unknown_method("hello", m)),
           }
       }
       // Optionally implement `stream` for event streams.
   }
   ```

2. In `crates/agent/Cargo.toml` add it as an optional dependency and a feature
   (`hello = ["dep:controldeck-hello"]`, then add it to `default`).
3. Register it in `registry()` in `crates/agent/src/main.rs` behind
   `#[cfg(feature = "hello")]`.
4. Add the HTTP gateway in the public crate: a module like `src/packages.rs` that maps
   routes to `AgentClient::call("hello", …)`. The gateway forwards JSON; the plugin owns
   validation.

Guidelines: validate everything; expose operations, not commands; run long work through
`jobs::JobManager` so output survives a restart of the web process; test with
`exec::fake::FakeRunner` (enable the SDK's `test-util` feature in `dev-dependencies`).

## Development

```bash
make run-agent         # terminal 1: agent as you (package changes will fail)
sudo make run-agent    # …or as root, trusting the user who ran sudo
make run               # terminal 2: web server using ./run/agent.sock
```
