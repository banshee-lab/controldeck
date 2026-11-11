# ControlDeck

Lightweight, web-based server management control panel for Linux. A Rust
backend ([Axum](https://github.com/tokio-rs/axum) + [rusqlite](https://github.com/rusqlite/rusqlite))
reads system vitals straight from `/proc` and `/sys`, stores history in SQLite
and serves an embedded SvelteKit + [Skeleton UI](https://www.skeleton.dev/) (v5)
dashboard — all from **one binary on one port**.

## Features

- Live gauges and 30-minute history charts: CPU, memory, virtual memory, disk
- System status: hostname, OS, time, uptime, kernel/CPU, processes, load
  averages (1/5/15 min), real and virtual memory, local disk space
- Mobile-first responsive UI, light/dark aware
- 7-day metric retention (purged on startup and hourly)
- No agents or heavy dependencies; Linux only

## Quick start

Requirements: Rust >= 1.85 (edition 2024), Node.js (see `web/.node-version`), Yarn 1.x.

```bash
make build   # frontend (yarn) + release binary
make run     # http://127.0.0.1:9999
make test    # cargo tests + svelte-check + vitest/playwright
```

Download a prebuilt `controldeck-linux-x86_64` from the
[Releases](../../releases) page (published on every push to `main`).

## Install as a systemd service

Each release also ships `.deb` and `.rpm` packages. They install the binary to
`/usr/bin`, the unit to `controldeck.service`, a config file at
`/etc/controldeck/controldeck.env`, and enable and start the service.

```bash
sudo apt install ./controldeck_*.deb        # Debian / Ubuntu
sudo zypper install ./controldeck-*.rpm     # openSUSE (dnf install on Fedora/RHEL)

systemctl status controldeck
sudo systemctl restart controldeck          # after editing the env file
journalctl -u controldeck -f
```

The service runs as a dynamic unprivileged user and stores metrics in
`/var/lib/controldeck/metrics.db`.

## Configuration (environment variables)

| Variable | Default | Description |
|---|---|---|
| `DECK_ADDR` | `127.0.0.1:9999` | Listen address |
| `DECK_DB` | `<binary dir>/db/metrics.db` | SQLite file |
| `DECK_INTERVAL_SECS` | `5` | Sampling interval |
| `DECK_PKG_MANAGER` | auto-detected | Force a package manager (`apt`, `dnf`, `zypper`, `pacman`, `emerge`, `apk`) |
| `DECK_PKG_READONLY` | unset | `1` disables package-modifying endpoints |

> **Security:** there is no authentication. It binds to localhost by default;
> put it behind a reverse proxy with auth before exposing it.

## API

- `GET /api/metrics/current` — latest sample plus full system status
- `GET /api/metrics/history?minutes=30` — samples (default 30, max 10080)
- `/api/packages/*` — package management (refresh cache, list updates and security fixes,
  install updates, search, install, remove) for apt, dnf, zypper, pacman, emerge and apk.
  Mutating calls run as background jobs whose output streams live over SSE; see
  [docs/packages-api.md](docs/packages-api.md). Mutating calls need root/sudo, and can be
  disabled with `DECK_PKG_READONLY=1`.

## Layout

```
src/            Rust backend (main.rs, db.rs, collector.rs, packages.rs, pkgmgr/)
web/       SvelteKit + Skeleton UI (static SPA export -> web/build)
db/             default SQLite location in development
.github/        CI (main/manual: test + release) and PR checks
```

## Development

Run `cargo run` (backend, :9999) and `cd web && yarn dev` (Vite proxies
`/api` to the backend). `build.rs` creates an empty `web/build` so
`cargo test` works without building the frontend.

## CI

- **CI & Release** (push to `main`, manual): tests both sides, builds the
  Linux x86_64 binary and publishes a GitHub release tagged `v0.1.<run_number>`.
- **PR Checks** (pull requests): tests both sides and builds the binary
  without publishing.

## License

GNU Affero General Public License v3.0 or later (AGPL-3.0-or-later). See [LICENSE](LICENSE).
