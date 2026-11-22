# maturidi-academy-rust

Rust rewrite of the Maturidi Academy API server.

- **Original:** `nasnih123-web/maturidi-academy` — FastAPI (Python) + Next.js + CRM + mobile + desktop.
- **This repo:** backend API only, in Rust (Axum + Tokio + SQLx + Postgres). Web / CRM / mobile stay as they are for now and keep calling the same API.
- **Base (locked):** original `main` at `9154d17` (merge #192) + PR #193 (player design 1) + PR #194 (CRM subtitle cue editor), merged cleanly with no conflicts into local branch `rust-base` in the original repo. **Locked migration base SHA: `5d1cb02bf7665ad04cf13e32360759de47443ff8`** (merge of PR #194; parents `c414abb` + `e22b71d`; verified 2026-11-22). The base does not move: any later original-repo change is out of scope unless a new decision re-locks it.
- **Method:** [rust-anything](https://github.com/erkamyaman/rust-anything) 3.1.0 — record the original's behaviour, port one piece at a time, prove each piece with `app record` / `app replay` and `gate`. See `rust/MAP.md` and `rust/DECISIONS.md`.
- **Status:** Wave 8 (lift) done — the full API is pure Rust (471 route+method combos served: 463 native + 8 explicit sidecar-contract routes), the catch-all proxy is gone, and the whole app-cases replay passes locally. Nothing is deployed yet. The live project is untouched, and any future deploy would be on a new Railway server, fully separate (see `scripts/cutover.sh` + `scripts/cutover-README.md`).
- **Wave 9 (Production Readiness, in progress):** PR-1 architecture + cutover reconciliation, then PR-2 CI + build verification, PR-3 production blockers, PR-4 staging deploy on a NEW Railway project. Standing rules for Wave 9: **UIs (web, CRM, mobile, desktop) stay TypeScript — no Rust UI rewrite**; the live project, its domains/DNS and its database are never touched; every change goes in an independent PR with tests and docs; a PR is never declared complete on SQLite tests alone.

## Important finding

`rust-anything analyze` says FastAPI / Starlette have **no Rust equivalent** and 25 of 30 API units are Blocked for that reason. The PyO3 bridge cannot turn a FastAPI server into a pure Rust server — so this port is an Axum rewrite. Waves 1–7 kept a Rust front door that proxied unported routes to the original; **Wave 8 removed that proxy entirely** (ADR-133): there are no unported routes left — every route is native, an explicit sidecar-contract route (ADR-134), or dev-only. See ADR-005 in `rust/DECISIONS.md`.
