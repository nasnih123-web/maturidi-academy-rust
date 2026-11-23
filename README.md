# maturidi-academy-rust

Rust rewrite of the Maturidi Academy API server.

- **Original:** `nasnih123-web/maturidi-academy` — FastAPI (Python) + Next.js + CRM + mobile + desktop.
- **This repo:** backend API only, in Rust (Axum + Tokio + SQLx + Postgres). Web / CRM / mobile stay as they are for now and keep calling the same API.
- **Base (locked):** original `main` at `9154d17` (merge #192) + PR #193 (player design 1) + PR #194 (CRM subtitle cue editor), merged cleanly with no conflicts into local branch `rust-base` in the original repo. **Locked migration base SHA: `5d1cb02bf7665ad04cf13e32360759de47443ff8`** (merge of PR #194; parents `c414abb` + `e22b71d`; verified 2026-11-22). The base does not move: any later original-repo change is out of scope unless a new decision re-locks it.
- **Method:** [rust-anything](https://github.com/erkamyaman/rust-anything) 3.1.0 — record the original's behaviour, port one piece at a time, prove each piece with `app record` / `app replay` and `gate`. See `rust/MAP.md` and `rust/DECISIONS.md`.
- **Status:** Wave 8 (lift) done — the full API is pure Rust (471 route+method combos served: 463 native + 8 explicit sidecar-contract routes), the catch-all proxy is gone, and the whole app-cases replay passes locally. Nothing is deployed yet. The live project is untouched, and any future deploy would be on a new Railway server, fully separate (see `scripts/cutover.sh` + `scripts/cutover-README.md`).
- **Wave 9 (Production Readiness, in progress):** PR-1 architecture + cutover reconciliation (merged as PR #1), PR-2 CI + build verification (merged as PR #2), PR-3 production blockers (in progress), PR-4 staging deploy on a NEW Railway project. Standing rules for Wave 9: **UIs (web, CRM, mobile, desktop) stay TypeScript — no Rust UI rewrite**; the live project, its domains/DNS and its database are never touched; every change goes in an independent PR with tests and docs; a PR is never declared complete on SQLite tests alone. What still blocks cutover is registered in **`rust/RELEASE_BLOCKERS.md`** (ADR-163) — an entry leaves that list only when its unblock condition is proven.

## CI

`.github/workflows/ci.yml` has four jobs, all merge gates (a failing
run blocks the PR — branch protection requires the checks `rust`,
`redis`, `docker` and `alembic` on `main`; set 2026-11-23, see
`rust/MAP.md`, Wave 9 PR-2):

- **`rust`** — fmt, clippy, `cargo build --release --locked`,
  `cargo test` (SQLite), the PostgreSQL smokes run sequentially (one
  binary at a time, `WAVE2_PG_URL` set only there), and `cargo audit` (fails on any RUSTSEC advisory — the two ADR-152 exceptions were closed by PR-3, ADR-157: sqlx 0.9.1 + libsqlite3-sys 0.40.0, audit clean with no ignores). Service:
  `pgvector/pgvector:pg16`. No Redis service here, on purpose
  (ADR-154): a live Redis at localhost:6379 is the test configs'
  default `redis_url`, and every binary would share the auth
  rate-limit counter and 429 itself.
- **`redis`** — `cargo test --test wave9_pr2` against the real Redis
  service (`redis:7-alpine`, `REDIS_URL` set only in this job).
- **`docker`** — first-ever build of the repo `Dockerfile` (no push),
  then a syft SBOM (SPDX JSON) uploaded as artifact `sbom`.
- **`alembic`** — checks out the original repo at the locked base SHA
  (read-only), runs `scripts/alembic-check.sh`: single Alembic head,
  `alembic upgrade head` on a real pgvector Postgres, schema
  verification, and the Rust binary's `/ready` against the migrated
  schema. The same script runs locally.

Local verification mirrors CI: `cargo fmt --all -- --check`,
`cargo clippy --all-targets -- -D warnings`, then the test suite one
binary at a time with `WAVE2_PG_URL` set (the smokes share one
database), plus `bash scripts/alembic-check.sh` for the migration
check. There is no Docker daemon in the dev VM, so the `docker` job
can only be verified by CI.

## Important finding

`rust-anything analyze` says FastAPI / Starlette have **no Rust equivalent** and 25 of 30 API units are Blocked for that reason. The PyO3 bridge cannot turn a FastAPI server into a pure Rust server — so this port is an Axum rewrite. Waves 1–7 kept a Rust front door that proxied unported routes to the original; **Wave 8 removed that proxy entirely** (ADR-133): there are no unported routes left — every route is native, an explicit sidecar-contract route (ADR-134), or dev-only. See ADR-005 in `rust/DECISIONS.md`.
