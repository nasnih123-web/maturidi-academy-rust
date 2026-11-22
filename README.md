# maturidi-academy-rust

Rust rewrite of the Maturidi Academy API server.

- **Original:** `nasnih123-web/maturidi-academy` — FastAPI (Python) + Next.js + CRM + mobile + desktop.
- **This repo:** backend API only, in Rust (Axum + Tokio + SQLx + Postgres). Web / CRM / mobile stay as they are for now and keep calling the same API.
- **Base:** original `main` at `9154d17` (merge #192) + PR #193 (player design 1) + PR #194 (CRM subtitle cue editor), merged cleanly with no conflicts.
- **Method:** [rust-anything](https://github.com/erkamyaman/rust-anything) 3.1.0 — record the original's behaviour, port one piece at a time, prove each piece with `app record` / `app replay` and `gate`. See `rust/MAP.md` and `rust/DECISIONS.md`.
- **Status:** Wave 8 (lift) done — the full API is pure Rust (471 route+method combos served: 463 native + 8 explicit sidecar-contract routes), the catch-all proxy is gone, and the whole app-cases replay passes locally. Nothing is deployed yet. The live project is untouched, and any future deploy would be on a new Railway server, fully separate (see `scripts/cutover.sh` + `scripts/cutover-README.md`).

## Important finding

`rust-anything analyze` says FastAPI / Starlette have **no Rust equivalent** and 25 of 30 API units are Blocked for that reason. The PyO3 bridge cannot turn a FastAPI server into a pure Rust server — so this port is an Axum rewrite with a Rust front door that proxies unported routes to the original, not a bridge. See ADR-005 in `rust/DECISIONS.md`.
