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

## ADR-014: DB layer — `Db::{Pg, Sqlite}` enum instead of `sqlx::any::AnyPool` (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Wave 2 needs `Uuid`, `DateTime<Utc>` and JSON values in queries.
`sqlx::any::Any` only implements those types for bool/int/float/string/blob
(`sqlx-core-0.8.6/src/any/types/`), so `AnyPool` cannot express the Wave 2
queries at all (49 type errors after the first compile pass).
Decision:
- One `Db` enum (`Pg(PgPool)` / `Sqlite(SqlitePool)`) with a single query
  API: `fetch_optional` / `fetch_one` / `fetch_all` / `execute`, taking one
  SQL string with `?` placeholders (rewritten to `$1..$n` for Postgres) and
  a slice of `Arg`s; rows come back as `DynRow` with the same `try_get`
  call on both backends.
- IDs and timestamps are always generated in Rust (`Uuid::new_v4`,
  `Utc::now`) and bound explicitly; no `RETURNING` clause anywhere.
- Expiry comparisons happen in Rust, never in SQL (the engines store
  timestamps differently).
- `Arg::Null` is typed (`NullText` / `NullUuid` / `NullTime` / `NullJson` /
  `NullBool` / `NullInt` / `NullFloat`); `Option<T>` maps `None` to
  `T::null_arg()`. A text-typed NULL is rejected by Postgres for jsonb /
  uuid / timestamptz columns, so an untyped NULL is never bound.
- `TEST_SCHEMA` column types match what the app binds and reads, not just
  the production names: id / FK columns are `UUID`, timestamps are
  `TIMESTAMPTZ`, JSON-bound columns are `JSONB` (sqlite tolerates the
  unknown type names). Two deliberate deviations from production:
  `domain_events.aggregate_id` and `audit_logs.entity_id` stay `TEXT`
  (production: `String(128)` / `String(512)` — the app binds text), and
  `platform_settings.value` stays `TEXT` (the app only ever handles the
  envelope as opaque text).
- Boolean SQL literals are always `TRUE` / `FALSE`, never `0` / `1`:
  Postgres boolean columns reject `revoked = 0`, sqlite accepts both.
- `Uuid` values are always bound as `Uuid`, never as text: on sqlite sqlx
  stores `Uuid` as a BLOB, so a text-bound id never matches a uuid-bound
  one (`WHERE id = ?` silently returns no rows) and `try_get::<Uuid>` fails
  on text-stored ids. All id columns, seeds and tests bind `Uuid`.
- `TEST_SCHEMA` (in `db.rs`) mirrors the production models with TEXT /
  INTEGER / BOOLEAN / TIMESTAMPTZ-neutral types so the same DDL applies to
  the sqlite test pool and to a local Postgres test database.
Consequences: the same Rust code runs against production Postgres and the
sqlite test pool; the `TRUE`/`FALSE` + `Arg::Null` + `Uuid`-bind rules are
port invariants for every later wave.

## ADR-015: Mail/SMS are stubs; avatar scan fails closed; S3 not ported (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The real Rust mailer/SMS sender, ClamAV client and S3 storage are
not ported in Wave 2.
Decision:
- `send_email` / `send_sms` only log and report success. The observable
  behaviour matches Python (tokens are stored, flows continue, resend
  works); real delivery comes with the full mailer in Wave 7.
- Avatar upload without the dev bypass fails closed with 503
  `antivirus_unavailable`, exactly like Python when no scanner is
  reachable; it passes only when `CLAMAV_ALLOW_INSECURE_DEV_BYPASS=true`
  and `APP_ENV=dev`.
- Only the `local` storage provider is implemented; any other
  `STORAGE_PROVIDER` is rejected with 503 `storage_not_configured`
  (avatar upload, avatar-by-ref, `put_object` / `read_bytes`).
Consequences: Wave 2 is behaviourally complete for tests, but no real email
/ SMS / virus scan / S3 happens until Wave 7.

## ADR-016: Wave 2 verification findings — two port bugs fixed by tests (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The Wave 2 integration tests (`rust/app/tests/wave2.rs`) caught two
bugs in the ported code that no compile check could catch.
Decision (both fixed in Wave 2, with regression tests):
1. `fetch_token_row` (`auth_service.rs`) compared the RAW token against the
   `token_hash` column, but inserts store `hash_token(raw)` (SHA-256 hex).
   Verify-email, register-verify-email, set-password and password-reset
   could never find their tokens (always 400 `invalid_token`). The lookup
   now hashes its argument first.
2. `verify_embed_token` (`routes_api_keys.rs`) padded the base64url payload
   with `len % 4` `'='` chars; a payload with `len % 4 == 3` needs 1 pad
   char, not 3, so roughly a third of issued embed tokens failed
   verification. It now decodes with the NO_PAD engine directly.
Consequences: token and embed-token roundtrips are covered by tests; any
later change to token handling must keep those tests green.

## ADR-017: Wave 2 test strategy (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Tests must prove the native routes without touching the live API
or any production database, and must cover both SQL engines the `Db` enum
supports.
Decision:
- Primary suite: `rust/app/tests/wave2.rs` — fresh in-memory sqlite per
  test (`TEST_SCHEMA` + `init_test_schema`), real Axum router via
  `oneshot`, `ConnectInfo` injected for the peer-IP extractors. Covers
  meta, activity, api_keys (incl. the `validate_key` /
  `validate_embed_token` guards), users and auth end-to-end (register →
  login → refresh rotation + reuse detection → 2FA enrol/login/recovery →
  logout/logout-all → verify-email → password reset → WS ticket).
- Postgres smoke: `wave2_pg_smoke` in the same file, gated on
  `WAVE2_PG_URL`, replays register → login → refresh → activity →
  settings-public against a local Postgres test database (schema dropped
  and re-applied first). A local Postgres 16 cluster was installed for
  this on the dev VM (`maturidi_test` database).
- HTTP cases for `app record` / `app replay` live in
  `rust/app-cases/wave2/` (14 cases).
- TOTP anti-replay is real: a second code in the same 30 s step is
  `code_reused`, so tests that log in twice with TOTP sleep to the next
  step boundary (`sleep_until_next_totp_step`).
Consequences: `cargo test` is green on sqlite by default; the Postgres
smoke runs whenever `WAVE2_PG_URL` is set.

## ADR-018: Home search fatwa semantic boost is inactive (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's home search adds `0.3 * semantic` to fatwa hits only
when the resolved embedding provider's name is not `"hash"`.
Decision: In the Rust port the boost is inactive — no real embedding
provider is ported in Wave 3 (see ADR-019), and the Python default
provider is `"hash"`, which the Python code itself skips. The lexical
scoring, ordering and response shape are otherwise exact.
Consequences: Home search results match Python's behaviour with the
default provider. If a real provider is ported later, the boost can be
re-enabled behind the same condition.

## ADR-019: Hash-only embedding provider; Backend B full scan; `embedding_vec` always NULL (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python resolves the embedding provider from the
`ai.embedding_provider` setting (`"hash"` in dev/test,
`"local_model"` — sentence-transformers/torch — in production when
unset), and searches via pgvector (Backend A) or a full scan
(Backend B) on SQLite.
Decision:
- The Rust port implements the hash provider only (`hash-trigram-v1`,
  256-dim folding, `PROJECTION_ID="folding-v1"`), ported exactly from
  `embeddings.py` (`rust/app/src/embeddings.rs`).
- An unset `ai.embedding_provider` resolves to `"hash"` in **every**
  environment — a deliberate deviation from Python's production
  default `local_model`, which cannot run inside the Rust binary.
  Any other resolved name returns 500 `internal_error` (the same
  status Python produces when its provider fails to construct).
- Search always uses the Backend B full scan (no pgvector / pg_trgm in
  the Rust binary); scores are identical to Python's Backend B. On
  Postgres, Python's Backend A may return a subset in edge cases
  (candidate window `top_k*3`); the Rust full scan is a superset.
- `embedding_vec` is always NULL in Rust (no pgvector binding); the
  JSON `embedding` column carries the data.
Consequences: `external_api` / `local_model` providers are a
follow-up (Wave 7+). Verify, cluster matching, chunk creation and
add-to-rag all work with the hash provider.

## ADR-020: Path UUIDs parse to 422; chunk/audit UUID cursors fail with 500 (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: FastAPI path params typed `UUID` reject malformed values with
422; Wave 2 routes used axum's `Path<Uuid>`, which answers 400.
Python's chunk/audit list handlers call `UUID(cursor)` unguarded, so
an invalid cursor reaches the generic handler as a 500.
Decision:
- Wave 3 routes take `Path<String>` and parse manually
  (`validate::parse_path_uuid`) → 422 `validation_error`, faithful to
  FastAPI.
- Chunk/audit `after` cursors parse with `Uuid::from_str`; failure →
  500 `internal_error`, faithful to Python's unguarded call.
Consequences: Known inconsistency with Wave 2's `Path<Uuid>` (400) —
to be aligned in a later wave, not in Wave 3.

## ADR-021: `PageUpdate.slug_immutable` is unreachable — verified, not ported (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `cms_service.update_item` raises 422 `slug_immutable` when
`"slug" in data` for pages, but `PageUpdate` (cms_schemas.py:117-122)
has no `slug` field and no `model_config` / `extra='forbid'` is set
anywhere in the content schemas, so Pydantic's default
`extra='ignore'` drops a client-sent `slug` before the check.
Decision: The Rust port ignores unknown body fields identically and
never emits `slug_immutable`.
Consequences: A client sending `slug` on a page PATCH gets a normal
200 in both Python and Rust.

## ADR-022: Ingest trigger always reports `enqueued=false` (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `trigger_ingestion` best-effort enqueues the ARQ job
`ingest_knowledge_run` on queue `"ai"` (Redis); on any exception the
run stays `queued` with `enqueued=false`.
Decision: The Rust port never enqueues (no ARQ client in Wave 3) and
always stores `stats={"run_config": {...}, "enqueued": false}` with
`status="queued"`, emitting `ingestion.run_triggered`. This is a
legitimate Python code path (Redis down), not an error.
Consequences: Ingestion runs created via the Rust API stay `queued`
until the ARQ worker path is ported (Wave 7 worker work).

## ADR-023: Trusted-source check error-class mapping is approximate (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python returns the httpx exception class name on a failed
check (e.g. `ConnectError`, `TimeoutException`); Rust uses reqwest.
Decision: Map reqwest errors approximately — connect failure →
`ConnectError`, timeout → `TimeoutException`, anything else →
`RequestError` — with the same 200 response shape
`{ok: false, latency_ms, error}` and no DB write.
Consequences: The `error` string may differ from Python's for exotic
failures (e.g. TLS errors); the shape and status are identical.

## ADR-024: SQL/validation fidelity notes (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Small places where the two engines or the two validation
stacks differ.
Decision:
- ILIKE: sqlite has no `ILIKE`; Rust uses
  `LOWER(CAST(col AS TEXT)) LIKE ?` with a pre-lowered `%q%` pattern
  on both backends. `%`/`_` in `q` stay wildcards, exactly like
  Python (which does no escaping).
- Length caps where Python would 500 via the DB varchar constraint:
  page slug max 160, nav url max 1024, banner placement max 64,
  `topic_category` max 64, `madhab_scope` max 16 → 422 in Rust.
- Score rounding: Rust `(x * 10000.0).round() / 10000.0` rounds half
  away from zero; Python `round(x, 4)` uses banker's rounding — a tiny
  edge difference on exact .00005 boundaries.
Consequences: No behavioural difference in normal use; the 422s are
strictly more helpful than Python's 500s.

## ADR-025: Telegram hooks in submission transitions are skipped (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python calls `notify_review_request` (on `in_review`) and
`auto_publish_if_policy` (on `published`) after committing a
submission transition, inside a try/except that swallows all errors.
Decision: The Rust port performs the transition, its side effects
(`updated_at`, `reviewed_by/at`, `published_at`, `version` bump,
domain event) and nothing else. Telegram distribution belongs to
Wave 7.
Consequences: No Telegram notification is sent from the Rust API for
submission transitions; the transition itself is identical.

## ADR-026: `POST /api/v1/fatwa/ask` stays proxied (assistant/mufti separate) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The ask route (incl. its SSE variant) is the AI assistant
surface: sessions, messages, LLM provider, streaming. The task scope
says the assistant/mufti is separate — do not touch it.
Decision: `POST /api/v1/fatwa/ask` is NOT registered in Rust; it falls
through to the proxy. `fatwa_sessions` / `fatwa_messages` tables exist
in TEST_SCHEMA but no Wave 3 route reads or writes them.
Consequences: Ask traffic keeps working through the Python server
until the assistant is ported (Wave 5+).

## ADR-027: Quiz quota — in-process windows, daily count compared in Rust (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `check_quota` (ask + quiz/generate) uses in-process
sliding windows (per-minute, keyed `user:<id>` / `ip:<ip>`, per
endpoint) and a DB count of `ai_usage_log` rows since UTC midnight
for authenticated users; guests use an in-process 24 h window. The
per-minute slot is consumed even when the daily check then fails.
Decision: Rust mirrors this with process-local statics
(`QUOTA_MINUTE` / `QUOTA_DAILY`) and a DB count whose timestamps are
compared in Rust, never in SQL (Wave 2 guidance on the two engines'
timestamp storage). Settings `ai.rate_per_minute` (default 5) and
`ai.daily_quota_per_user` (default 20) are read via
`get_typed_setting` with Python's `int()` truncation semantics.
Consequences: Quota state is per-process (not shared across
replicas), exactly like Python.

## ADR-028: Postgres type strictness — INTEGER columns read as i32, timestamps bound as DateTime (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The Wave 3 Postgres smoke test caught two failures that
sqlite silently accepts. (1) sqlx on Postgres cannot decode an INT4
(`INTEGER`) column as i64 — `row_to_dict` reading `version` as i64
made every CMS create 500 on Postgres while passing on sqlite.
(2) A `text` parameter is rejected for a TIMESTAMPTZ column
(Postgres error 42804); the test helper bound RFC3339 strings for
`created_at`, which sqlite accepted.
Decision: Every read of an `INTEGER` column uses i32 (`version`,
`hijri_month/day`, `sort_order`, `priority`, `correct_option`,
`view_count`/`share_count`, `surah`/`ayah`, `donations.amount`
which is read as i32 then cast to f64); `COUNT(*)` reads stay i64
because Postgres returns BIGINT. Every timestamp bind uses
`DateTime<Utc>` (`Arg::Dt`), never an RFC3339 string. The same fix
was applied to the latent Wave 2 sites (`routes_users.rs` ayah notes
+ donations amount) since they share the bug class.
Consequences: The full suite (incl. both Postgres smoke tests) is
green on real Postgres. New code must read INTEGER as i32; a future
lint or review check should catch i64 reads of INTEGER columns.

## ADR-029: No HTTP route approves a quiz question in Python (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `quiz/generate` requires quiz questions with
`workflow_status` in `('approved', 'published')`, but there is no
HTTP route that moves a quiz question out of `draft` (only
`POST /api/v1/admin/fatwa/quiz/questions`, which creates it as
`draft`). Verified by grepping `fatwa/router.py` and `service.py`.
Decision: Rust mirrors Python exactly — no quiz-question status
route is added. Over pure HTTP, `quiz/generate` therefore answers
404 `no_quiz_questions` until a question is approved out-of-band
(direct DB), the same as Python. The Wave 3 tests seed the approved
state directly in the DB and then exercise generate/submit over HTTP.
Consequences: Quiz is ported faithfully, including the upstream gap;
if the user wants a quiz-approval endpoint, that is a product
decision for a later wave, not a silent addition here.

## ADR-030: `embedding_vec` is never named in Rust INSERTs (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Per ADR-019 `embedding_vec` is always NULL in Rust and
`TEST_SCHEMA` deliberately omits the column (no pgvector in sqlite).
Both chunk INSERTs still listed `embedding_vec` (with a NULL literal),
which 500s on sqlite ("no such column") while working on Postgres.
Decision: Rust INSERTs never name `embedding_vec`; the column keeps
its production default (NULL) on Postgres. The JSON `embedding`
column is still written.
Consequences: Chunk creation and add-to-rag work identically on
sqlite and Postgres. Any future vector work must reintroduce the
column deliberately, not by accident.
