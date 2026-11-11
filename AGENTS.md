# AGENTS.md

Guidance for AI coding agents working on ControlDeck.

## Overview
Rust (edition 2024) Cargo workspace. The **public** binary `controldeck` (repo root) runs
unprivileged: Axum + rusqlite, embeds a SvelteKit (Svelte 5, Tailwind 4, Skeleton UI v5)
static SPA via `rust-embed`, one port. Everything that needs root lives in the separate
**`controldeck-agent`** (`crates/agent`, plugins compiled in), reached over a Unix socket
that only the `controldeck` user can use. Read `docs/architecture.md` first. Linux-only
(reads `/proc`, `/sys`).

## Commands
- `make build` — `yarn build` in `web/`, then `cargo build --release --workspace`
- `make test` — `cargo test --workspace` + `yarn run check` + `yarn test`
- `make run` / `make run-agent` — web server / privileged agent (dev socket in `./run`)
- Frontend dev: `cd web && yarn dev` (proxies `/api` to :9999)
- Use **Yarn 1** (never npm to install) in `web/`; commit `yarn.lock`. Run scripts as
  `yarn run check` (plain `yarn check` is a Yarn built-in). Do not edit the generated
  `web/.pnp.*` files.
- Format with `yarn format` (Prettier: tabs, single quotes, width 100).

## Structure
- `src/collector.rs` — pure `parse_*` functions (take `&str`, unit-tested) plus
  thin `read_*` wrappers over `/proc`. Add new metrics as parser + test first.
- `src/db.rs` — schema, insert, history, 7-day `cleanup`.
- `src/main.rs` — router, embedded static handler with SPA fallback, background
  collector and retention tasks, env config.
- `src/agent.rs` — gateway plumbing: `RpcError` -> HTTP status mapping, `GET /api/agent/status`.
- `src/packages.rs` — `/api/packages/*` **gateway**: thin routes that forward JSON to the
  agent's `packages` plugin (mutations return 202 + job; output via SSE at
  `/jobs/{id}/stream`). No package logic or validation lives here. Its tests run
  end-to-end against a real agent server on a temp socket with a `FakeRunner`. Docs live in
  `docs/packages-api.md`; keep them in sync with routes and JSON shapes.
- `crates/proto` — wire protocol (newline-delimited JSON, one request per connection),
  `AgentClient`. Generic; never add plugin-specific types here.
- `crates/agent-sdk` — `Plugin` trait + `Registry`, `server` (socket creation, `SO_PEERCRED`
  authentication via `peer`), `exec` (`Cmd` = argv, never a shell; `Runner`; `SystemRunner`;
  `FakeRunner` behind the `test-util` feature), `jobs` (background jobs: sequenced output,
  replayable/resumable stream, one running job per manager, panic-safe).
- `crates/pkgmgr` — the `packages` plugin (apt, dnf, zypper, pacman, emerge, apk):
  - `lib.rs` — async `PackageManager` trait, distro detection (`os-release` `ID`/`ID_LIKE`,
    `DECK_PKG_MANAGER` override), input validation, shared helpers.
  - `plugin.rs` — `PackagesPlugin`: JSON methods (`info`, `updates`, `search`, `refresh`,
    `install`, `remove`, `install_updates`, `jobs`, `job`, `job_stream`).
  - one file per manager. Pure `parse_*` functions (take `&str`, fixture-tested) plus a thin
    async impl that builds `Cmd`s. Prefer machine-readable output (e.g. zypper `--xmlout`).
  - Adding a manager: new file implementing the trait, register in `ManagerKind` and
    `kind_from_distro_id`, add fixture tests, update the docs table.
  - Every mutating operation must go through `PackagesPlugin::start_job` so its output is
    streamable; never a blocking request. Never build commands from unvalidated input
    (use `validate_packages` / `validate_query`). The agent is root: there is no `sudo`.
- `crates/agent` — `controldeck-agent` binary: config from env, `registry()` registers
  compiled-in plugins (one Cargo feature each). Adding a plugin: see
  `docs/architecture.md` ("Writing a plugin").
- **Security rules.** The web process is untrusted from the agent's point of view: plugins
  validate all input and expose typed operations, never "run this command". Keep the
  peer check fail-closed; never widen socket permissions; never make the web service run
  as root or share its account with other software.
- `web/src/lib/` — `api.ts` (types mirror backend JSON, snake_case),
  `format.ts`, `chart.ts` (pure, tested), `Gauge.svelte`, `LineChart.svelte`.
- `web/src/routes/+page.svelte` — dashboard (polls every 5 s).

## Conventions
- Keep the JSON contract in sync: Rust structs in `collector.rs` <-> `api.ts`.
- Frontend uses Svelte 5 runes (`$state`, `$derived`, `$props`), no stores.
- Skeleton v5 components come from `@skeletonlabs/skeleton-svelte` (e.g.
  `Progress.Circle`); style with Skeleton/Tailwind classes. Check
  https://www.skeleton.dev/docs/svelte before using an API.
- SvelteKit 3: config lives in `web/vite.config.ts` (no `svelte.config.js`); runes mode
  is forced. Use `#lib/...` subpath imports (declared in `package.json` `imports`) and
  include file extensions for `.ts` imports.
- Tests: `*.svelte.spec.ts` run in a Playwright browser (client project), other
  `*.spec/test.ts` in node (server project); `*.e2e.ts` run with Playwright.
- Do not add auth-less network exposure by default; keep the localhost bind.
- Dependencies: add a crate when it is genuinely needed, actively maintained and well
  tested; do not hand-roll what a maintained crate (or `std`/tokio) already does. Prefer a
  small, focused crate over a framework. Justify each new dependency in the PR description
  and keep feature flags minimal. Current additions: `async-trait` (async trait objects for
  the package layer), `roxmltree` (parsing `zypper --xmlout`), tokio `process`.

## Testing rules
- Every parser/helper gets a unit test. Package-manager code is tested with `FakeRunner`
  (exact command lines) and fixtures; verify against a real manager where one is available. Backend API tests use an in-memory
  SQLite and `tower::ServiceExt::oneshot`. Server/protocol tests use temp Unix sockets.
- Run `make test` before committing. `web/build` is gitignored; `build.rs`
  creates it empty so `cargo test` works without a frontend build.

## Packaging
- `packaging/` holds `controldeck.service`, `controldeck-agent.service`, `controldeck.env`,
  `agent.env` and deb maintainer scripts. deb/rpm metadata lives in the root `Cargo.toml`
  (`[package.metadata.deb]`, `[package.metadata.generate-rpm]`); OBS uses `debian/` and
  `packaging/rpm/controldeck.spec`. Keep all four in sync (both binaries, both units, both
  env files, `controldeck` user creation).
- The web service runs as the static `controldeck` user (created by the package scripts) +
  `StateDirectory`; the DB path is set via `DECK_DB`. The agent unit is intentionally not
  sandboxed (package managers need the system; `NoNewPrivileges` breaks SELinux transitions)
  and uses `KillMode=process` so a running package operation survives an agent restart.
- Keep sandboxing compatible with metrics collection (`ProtectHome=read-only` is fine,
  it was implied by the former `DynamicUser`; `ProtectHome=yes`/`tmpfs` would distort disk stats;
  no `ProtectProc`, which would hide processes).

## CI
- `.github/workflows/ci.yml`: main push + manual; tests, builds, publishes release
  (both binaries + deb + rpm + SHA256SUMS; package version is set to the `0.1.<run>` tag).
- `.github/workflows/pr.yml`: PRs; tests and builds, never publishes.
