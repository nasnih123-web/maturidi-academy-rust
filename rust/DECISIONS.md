# DECISIONS — Maturidi Academy, Rust rewrite

Source of truth for the port. One ADR per decision, numbered in order, never
renumbered. A changed decision gets a new ADR that supersedes the old one.

## ADR-001: New repo, separate from the original
Status: accepted (2026-11-22, user confirmed)
Context: User asked for a new repo to hold the Rust code.
Decision: New public repo `nasnih123-web/maturidi-academy-rust`, created
2026-11-22 via the logged-in cloud browser, empty (no README / .gitignore / license).
The original repo `nasnih123-web/maturidi-academy` is never modified by this port.
Consequences: All Rust work lives here. Live Railway project is untouched.

## ADR-002: Source base = main + PR #193 + PR #194
Status: accepted (2026-11-22, user confirmed "build from the latest PR or merge")
Context: Main HEAD was `9154d17` (merge #192). Two user feature PRs were open and
not in main: #193 (player design 1, web only) and #194 (CRM subtitle cue editor,
backend + CRM, approved by Hossein). Main alone would miss them.
Decision: Base is main `9154d17` plus both PRs, merged cleanly with no conflicts
into local branch `rust-base` (`c414abb`, `5d1cb02`). #193 touches only web
(2 files); #194 touches API videos + CRM (8 files, 891 lines). They do not overlap.
Consequences: The Rust port is proved against the newest behaviour, including
the subtitle editor and the player change.

## ADR-003: Backend first, UI later
Status: accepted (2026-11-22, user confirmed)
Context: User: "first do the backend, then I decide about UI."
Decision: Phase 1 is the API server only (`apps/api`). Web, CRM, mobile and
desktop stay as they are and keep calling the same API.
Consequences: No UI work in Phase 1. UI rewrite (if any) is a separate decision.

## ADR-004: Live project untouched; any future deploy on a new Railway server
Status: accepted (2026-11-22, user confirmed)
Context: User: if he ever runs the Rust version, it will be on a new Railway
server, with nothing to do with the first project.
Decision: No change to the live Railway project, domains or DNS. Cutover only
with the user's explicit go-ahead.
Consequences: The Rust server is developed and tested in isolation.

## ADR-005: Method — Axum rewrite, NOT the PyO3 bridge
Status: accepted (2026-11-22, user: "all features must work exactly like the original, but in Rust; decide yourself where you hit a problem")
Context: `rust-anything analyze` (full report in `rust/PLAN-API.md`) says
FastAPI / Starlette have Rust fit **none** — "keep the app in Python" — and
25 of 30 units are Blocked for that reason. The PyO3 bridge ports libraries /
pure functions; it cannot turn a FastAPI server into a pure Rust server.
Decision: Rewrite the server in Rust with Axum + Tokio + SQLx +
Postgres + Serde. A Rust front door (`rust/app`) owns the entry point early
and proxies unported routes to the original Python server. Units move to Rust
one at a time, proved by `app record` / `app replay` at the HTTP edge plus
unit `record` / `diff` for pure functions. Lift only at fallback rate 0 with
all gates passing.
Consequences: This is a real rewrite, not a bridge. It is the only path that
ends in a standalone Rust server.

## ADR-006: Compatibility bar
Status: accepted (2026-11-22, user: "all features must work exactly like the original")
Context: Existing web / CRM / mobile clients must keep working.
Decision: HTTP routes, methods, status codes, response shapes,
DB schema and auth behaviour stay identical to the Python server.
Pydantic validation error text is the known exception: serde/garde errors
will differ in wording, so recorded cases must pin the status code and the
error shape, not the exact Pydantic message — unless the user demands
byte-identical error bodies.
Consequences: Clients need no change. Any intentional behaviour change needs
its own ADR (decided by Dash per ADR-010, recorded here).

## ADR-007: First wave
Status: accepted (2026-11-22, user confirmed waves + backend first)
Context: The tangled core is 49,484 lines in wave 0; starting there is the
riskiest first step.
Decision: Wave 1 is (1) the Axum front door + `app record` /
`app replay` harness, and (2) `/health` + `/ready` implemented natively in
Rust to prove the whole pipeline end to end, with all other routes proxied
to the original. Then the next smallest module by readiness. The tangled
core is taken as one large piece only after the pipeline is proved.
Consequences: Early proof that the method works, before committing to the
49k-line core.

## ADR-010: Dash decides blockers himself (user 2026-11-22 ~09:22)
Status: accepted
Context: User: "wherever you hit a problem, decide yourself" — the point is
that all features work exactly like the original, in Rust.
Decision: When a port hits a blocker (no crate, behaviour difference, missing
dependency), Dash chooses the fix himself — sidecar / external service, a
Rust crate covering the used subset, reimplementation, or keeping that piece
behind the proxy until its wave — and records the choice as an ADR here.
No per-blocker approval is needed.
Consequences: Work does not stop waiting for the user. The user reviews the
wave reports afterwards and says what needs changing.

## ADR-011: Waves, push and auto-merge (user 2026-11-22 ~09:22, Rust repo ONLY)
Status: accepted
Context: User: divide the whole project into waves, report each wave, start
the next; push whenever wanted, and when CI is all green merge without asking.
Decision: Work proceeds wave by wave (see MAP.md). After each wave: build →
verify (`cargo fmt`, `clippy -D warnings`, `cargo test`, replay) → push to
the new repo → CI must be ALL green → merge (merge commit, never
squash/rebase, never delete the branch) WITHOUT asking. If any CI job is
red, NO merge: read the log, fix, re-run. A wave report is sent to the user
in chat, then the next wave starts. The user checks afterwards and says
what needs changing.
Consequences: This auto-merge authorization covers ONLY
`nasnih123-web/maturidi-academy-rust`. The original repo keeps its strict
2026-11-20 boundary (no autonomous work, push/merge only for a fix the user
explicitly asked for).

## ADR-012: Wave plan
Status: accepted (2026-11-22)
Decision: Waves are:
- Wave 0 (done): plan + repo + base (main + #193 + #194) + analyze.
- Wave 1: front door (Axum) + `/health` + `/ready` natively + proxy for the rest + CI.
- Wave 2: auth + users + meta + activity + api_keys.
- Wave 3: content + home + articles + fatwa (read paths first).
- Wave 4: quran + hifz (read; scoring via sidecar — see ADR-008).
- Wave 5: videos + voice + STT (sidecars).
- Wave 6: library + LMS + calculators (fiqh HARD RULE, ADR-009).
- Wave 7: finance + admin + places + social + sync + telegram + widgets + workers.
- Wave 8: lift — remove proxy fallback, pure Rust, final replay, cutover script.
Consequences: Each wave ends with a report; the next starts without waiting
for the user, per ADR-011.

## ADR-008: Hard dependencies (OPEN — needs user decision per item)
Status: open (2026-11-22)
Context: These have no drop-in Rust crate (see PLAN-API.md D30–D69):
`faster-whisper` (STT), `sherpa-onnx` (Zipformer hifz ASR),
`sentence-transformers` (embeddings, torch), `telethon` /
`python-telegram-bot`, `reportlab` (PDF), `pgvector`, `arq` + Redis workers.
Decision: not yet made. Options per item: sidecar / external service,
ONNX Runtime in Rust, a Rust crate that covers the used subset, or drop.
Consequences: Until decided, units that depend on these cannot be lifted.
Hifz scoring and STT are the highest-risk items; they must not be faked —
ASR quality and tajwid rules are out of scope for a port to invent.

## ADR-009: Fiqh tables are never invented (HARD RULE, inherited)
Status: accepted (standing rule, P7 inheritance+zakat calculators)
Context: Zakat / inheritance / `مصرف الافعال` calculators use versioned rule
configs pending scholar review.
Decision: The Rust port copies the rule configs and their versioning exactly.
No new fiqh table, no changed threshold, no public launch without approval.
Consequences: The calculators module is ported faithfully, with its tests
and recorded cases, and nothing else.

## ADR-013: Wave 1 front-door details (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The Wave 1 task left three details unspecified: what the proxy
returns when the origin is unreachable, how non-Postgres `DATABASE_URL`
values are treated, and whether JSON key order must match Python.
Decision:
- Proxy to an unreachable origin returns 502 with
  `{"status": "bad_gateway", "env": <APP_ENV>}` (no panic, no hang).
- `DATABASE_URL` has no default; unset / empty / unknown-scheme values mean
  the DB is unavailable, never a panic. `sqlite://` URLs use a sqlite pool;
  the Python SQLAlchemy form `sqlite+aiosqlite://...` is normalized to
  `sqlite://...`, and `sqlite://:memory:` to the canonical `sqlite::memory:`.
- JSON is serialized with `serde_json` `preserve_order`, so `/health` and
  `/ready` emit fields in the same order as the Python server
  (`status`, `env`, `db`, `redis`), byte-for-byte, not alphabetically sorted.
Consequences: `/health` and `/ready` match the Python wire shape exactly;
the proxy fails closed with a clear 502 instead of hanging or crashing.
