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
- Prefer no new dependencies; avoid third-party system-metrics crates.

## Testing rules
- Every parser/helper gets a unit test. Backend API tests use an in-memory
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
