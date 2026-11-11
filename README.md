# ControlDeck

Lightweight, web-based server management control panel for Linux. A Rust
backend ([Axum](https://github.com/tokio-rs/axum) + [rusqlite](https://github.com/rusqlite/rusqlite))
reads system vitals straight from `/proc` and `/sys`, stores history in SQLite
and serves an embedded SvelteKit + [Skeleton UI](https://www.skeleton.dev/) (v5)
dashboard from one port. Privileged work (package management) is done by a separate
root agent, so the web-facing process runs unprivileged; see
[docs/architecture.md](docs/architecture.md).

## Features

- Live gauges and 30-minute history charts: CPU, memory, virtual memory, disk
- System status: hostname, OS, time, uptime, kernel/CPU, processes, load
  averages (1/5/15 min), real and virtual memory, local disk space
- Mobile-first responsive UI, light/dark aware
- 7-day metric retention (purged on startup and hourly)
- Small, dependency-light; Linux only

## Quick start

Requirements: Rust >= 1.85 (edition 2024), Node.js (see `web/.node-version`), Yarn 1.x.

```bash
make build       # frontend (yarn) + release binaries (controldeck, controldeck-agent)
make run         # web server, http://127.0.0.1:9999
make run-agent   # privileged agent, in another terminal (sudo make run-agent for real changes)
make test        # cargo tests + svelte-check + vitest/playwright
```

Download prebuilt `controldeck-linux-x86_64` and `controldeck-agent-linux-x86_64` from the
[Releases](../../releases) page (published on every push to `main`).

## Install as a systemd service

Each release also ships `.deb` and `.rpm` packages. They install two services: the web
server `controldeck` (runs as the unprivileged `controldeck` user) and the privileged
`controldeck-agent` (root, reachable only by that user through a Unix socket). Both are
enabled and started.

```bash
sudo apt install ./controldeck_*.deb        # Debian / Ubuntu
sudo zypper install ./controldeck-*.rpm     # openSUSE (dnf install on Fedora/RHEL)

systemctl status controldeck controldeck-agent
sudo systemctl restart controldeck          # after editing controldeck.env
sudo systemctl restart controldeck-agent    # after editing agent.env
journalctl -u controldeck -u controldeck-agent -f
```

Metrics are stored in `/var/lib/controldeck/metrics.db`. Upgrading from the earlier
single-service version (dynamic user) may reset the metric history.

## Configuration (environment variables)

Web server (`/etc/controldeck/controldeck.env`):

| Variable | Default | Description |
|---|---|---|
| `DECK_ADDR` | `127.0.0.1:9999` | Listen address |
| `DECK_DB` | `<binary dir>/db/metrics.db` | SQLite file |
| `DECK_INTERVAL_SECS` | `5` | Sampling interval |
| `DECK_AGENT_SOCKET` | `/run/controldeck/agent.sock` | Socket of the privileged agent |

Agent (`/etc/controldeck/agent.env`, root-only):

| Variable | Default | Description |
|---|---|---|
| `DECK_AGENT_SOCKET` | `/run/controldeck/agent.sock` | Socket to listen on |
| `DECK_AGENT_USER` | `controldeck` | The only account allowed to connect |
| `DECK_AGENT_PEER_EXE` | `/usr/bin/controldeck` (packages) | The connecting process must run this executable |
| `DECK_PKG_MANAGER` | auto-detected | Force a package manager (`apt`, `dnf`, `zypper`, `pacman`, `emerge`, `apk`) |
| `DECK_PKG_READONLY` | unset | `1` disables package-modifying operations |

> **Security:** there is no authentication. It binds to localhost by default;
> put it behind a reverse proxy with auth before exposing it.

## API

- `GET /api/metrics/current` — latest sample plus full system status
- `GET /api/metrics/history?minutes=30` — samples (default 30, max 10080)
- `GET /api/agent/status` — plugins offered by the privileged agent (`503` if it is down)
- `/api/packages/*` — package management (refresh cache, list updates and security fixes,
  install updates, search, install, remove) for apt, dnf, zypper, pacman, emerge and apk.
  Mutating calls run as background jobs whose output streams live over SSE; see
  [docs/packages-api.md](docs/packages-api.md). They run in the root agent and can be
  disabled with `DECK_PKG_READONLY=1`.

## Layout

```
src/            public, unprivileged server (main.rs, db.rs, collector.rs, agent.rs, packages.rs gateway)
crates/proto    wire protocol + agent client
crates/agent-sdk plugin API, socket server, command/job helpers
crates/pkgmgr   "packages" plugin (apt, dnf, zypper, pacman, emerge, apk)
crates/agent    controldeck-agent binary (root)
web/       SvelteKit + Skeleton UI (static SPA export -> web/build)
db/             default SQLite location in development
.github/        CI (main/manual: test + release) and PR checks
```

## Development

Run `make run-agent`, `make run` (backend, :9999) and `cd web && yarn dev` (Vite proxies
`/api` to the backend). `build.rs` creates an empty `web/build` so
`cargo test` works without building the frontend.

## CI

- **CI & Release** (push to `main`, manual): tests both sides, builds the
  Linux x86_64 binary and publishes a GitHub release tagged `v0.1.<run_number>`.
- **PR Checks** (pull requests): tests both sides and builds the binary
  without publishing.

## License

GNU Affero General Public License v3.0 or later (AGPL-3.0-or-later). See [LICENSE](LICENSE).
