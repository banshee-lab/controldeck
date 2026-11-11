# AGENTS.md

Guidance for AI coding agents working on ControlDeck.

## Overview
Rust (edition 2024) backend with Axum + rusqlite that embeds a SvelteKit
(Svelte 5, Tailwind 4, Skeleton UI v5) static SPA via `rust-embed`. One binary,
one port. Linux-only (reads `/proc`, `/sys`).

## Commands
- `make build` — `yarn build` in `web/`, then `cargo build --release`
- `make test` — `cargo test` + `yarn run check` + `yarn test`
- `make run` — run the release binary
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
- `src/pkgmgr/` — package manager abstraction (apt, dnf, zypper, pacman, emerge, apk):
  - `mod.rs` — async `PackageManager` trait, distro detection (`os-release` `ID`/`ID_LIKE`,
    `DECK_PKG_MANAGER` override), input validation, shared helpers.
  - `exec.rs` — `Cmd` (argv, never a shell), `Runner` trait, `SystemRunner` (tokio process,
    timeout, `sudo -n` when not root) and a `FakeRunner` for tests.
  - one file per manager. Pure `parse_*` functions (take `&str`, fixture-tested) plus a thin
    async impl that builds `Cmd`s. Prefer machine-readable output (e.g. zypper `--xmlout`).
  - `jobs.rs` — background jobs for mutating operations: output recorded with sequence
    numbers, replayable/resumable event stream, one running job at a time, panic-safe.
    Operations never write to HTTP responses directly; they emit lines through the runner.
  - `src/packages.rs` — `/api/packages/*` routes (mutations return 202 + job; output via
    SSE at `/jobs/{id}/stream`). Docs live in `docs/packages-api.md`; keep
    them in sync with routes and JSON shapes.
  - Adding a manager: new file implementing the trait, register in `ManagerKind` and
    `kind_from_distro_id`, add fixture tests, update the docs table.
  - Every mutating operation (refresh, install, remove, upgrade) must go through
    `PackageService::start_job` so its output is streamable. Add new operations as a trait
    method + job, never as a blocking request.
  - Mutating endpoints (refresh/install/remove) need root; they run via `sudo -n`, are serialised,
    and are disabled by `DECK_PKG_READONLY=1`. Never build commands from unvalidated input
    (use `validate_packages` / `validate_query`).
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
  SQLite and `tower::ServiceExt::oneshot`.
- Run `make test` before committing. `web/build` is gitignored; `build.rs`
  creates it empty so `cargo test` works without a frontend build.

## Packaging
- `packaging/` holds `controldeck.service`, `controldeck.env` and deb maintainer scripts.
  deb/rpm metadata lives in `Cargo.toml` (`[package.metadata.deb]`, `[package.metadata.generate-rpm]`).
- The service uses `DynamicUser` + `StateDirectory`; the DB path is set via `DECK_DB`.
- Keep sandboxing compatible with metrics collection (e.g. no `ProtectHome`, which would
  distort disk stats; no `ProtectProc`, which would hide processes).

## CI
- `.github/workflows/ci.yml`: main push + manual; tests, builds, publishes release
  (binary + deb + rpm + SHA256SUMS; package version is set to the `0.1.<run>` tag).
- `.github/workflows/pr.yml`: PRs; tests and builds, never publishes.
