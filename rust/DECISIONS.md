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

## ADR-031: ASR stays proxied; the live stream answers an explicit 503 (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Hifz scoring needs sherpa-onnx (Zipformer) or faster-whisper
(Whisper-quran) — native ASR models that cannot live inside the Rust
binary (ADR-008). The four HTTP scoring endpoints (`POST
/attempts/{id}/grade`, `grade-word`, `POST /voice-search`, `POST
/tests/{id}/questions/{qid}/answer`) and the live WebSocket stream
(`WS /attempts/{id}/stream`) all depend on them.
Decision: The four HTTP endpoints are NOT registered natively; they
fall through to the proxy fallback, so with a live Python origin they
keep working exactly as before, and with a dead origin they answer
502 `bad_gateway`. The WebSocket route IS registered, explicitly, as
503 `hifz_live_unavailable` — the HTTP proxy (reqwest) cannot relay a
WebSocket upgrade, so proxying it would silently break; an honest 503
is better than a hanging upgrade. `GET /quran/hifz/status` honestly
reports the selected engine as unavailable, with reason "موتور تشخیص
گفتار در نسخهٔ Rust پیکربندی نشده است" (invalid engine id →
available=false, reason "موتور انتخاب‌شده معتبر نیست"); map fields
false/unavailable. ASR quality is never faked: no grade endpoint is
native, so `HifzGradeWordOut.needs_review` is never emitted by Rust.
Consequences: Hifz grading, voice search and the live stream require
the Python origin until a sidecar exists (Wave 5/8). Everything else
in hifz (attempts, peek, settings, listen position, stats, review,
SRS, plans, dashboard, activity, suggested, tests except answer,
mutashabihat, tasmi, purge) is native.

## ADR-032: Test-passage sampling uses SplitMix64, not CPython's Mersenne Twister (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `build_passages` samples mushaf-line starts with
`random.Random(seed).sample(...)` (Mersenne Twister); the seed is the
first 8 bytes of the test UUID, little-endian.
Decision: Rust uses SplitMix64 with the same seed and the same wire
shape (Fisher–Yates partial shuffle with rejection sampling). Sampled
passages may therefore differ from Python's for the same test id; the
seed, question count, lengths and prompt rules are identical.
Consequences: Test questions are equivalent in distribution but not
byte-identical to Python's. If exact parity is ever required, a
Mersenne Twister implementation must be added deliberately.

## ADR-033: tasmi8 unknown variant / unknown rating → 500 internal_error (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `tasmi8_initial_state` and `tasmi_review_due_at`
raise plain `ValueError` for an unknown variant / unknown rating, with
no `APIError` conversion in the service — the request dies as a 500.
Decision: Rust mirrors this exactly: unknown `variant` on tasmi8
creation and unknown `rating` on `rate` return 500 `internal_error`.
Consequences: Faithful to Python, including the upstream wart; a
future product decision could map these to 422 instead.

## ADR-034: Hifz data-purge rate limit is in-process (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's purge limiter is 3/hour per auth session via Redis
when provisioned, else in-process.
Decision: Rust keeps a `Mutex<HashMap>` sliding 3600 s window, limit
3, keyed by session id falling back to user id — the same behaviour
as Python when Redis is absent. The limiter runs before validation,
so failed purge attempts consume quota, exactly like the Python
decorator.
Consequences: Quotas do not survive a restart and are not shared
across processes; that matches the no-Redis Python behaviour.

## ADR-035: `sync_changelog.seq` is MAX(seq)+1 (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `record_entity_change` (notes create/update/delete) needs a
`seq` for the sync journal. Production Postgres allocates it from a
real sequence via migrations; the test schema has no sequence.
Decision: Rust allocates `seq` as `MAX(seq)+1` in one statement.
Consequences: A concurrent-insert race could collide on the PK; this
is a known limitation to revisit in Wave 7 when sync is fully ported.

## ADR-036: No settings-version bump on custom-provider key write (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python bumps a settings version after `stage_setting`
because it caches settings; Rust reads settings from the DB on every
request.
Decision: `PUT /quran/registry/custom-providers/{id}/key` stores the
key via `stage_setting` and does not bump any version.
Consequences: Key changes take effect immediately in Rust, same
observable behaviour as Python after its bump.

## ADR-037: Pydantic-vs-service validation order in test creation (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `POST /api/v1/quran/hifz/tests` validates in two layers:
Pydantic schema ranges first, then service rules.
Decision: Rust keeps the order — schema-level ranges
(`question_count` 1–99, `time_limit_minutes` 1–60, surah 1–114) →
422 `validation_error`; service-level checks (length, stop_rule,
range) → domain codes `hifz_test_length` / `hifz_test_stop_rule` /
`hifz_test_range`, in that order.
Consequences: The same body gets the same code from Rust and Python.

## ADR-038: Peek `kind` semantics are inferred, not observed (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `kind="word"` = `<` (next word), `kind="ayah"` = `<<` (rest
of the ayah) is inferred from video iconography in the Python code;
it was never observed live.
Decision: Rust ports the Python semantics verbatim and records the
uncertainty here, as Python does in its own comments.
Consequences: If the user corrects the mapping, both codebases change
together; nothing in Rust depends on the guess beyond the port.

## ADR-039: Mutashabihat cache is computed once per process (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python keys its mutashabihat cache by the Tanzil file's
mtime; the Rust copy is embedded with `include_str!` and static.
Decision: Rust computes the phrase index once per process from the
embedded text (`OnceLock`); no mtime key is needed.
Consequences: The index never refreshes at runtime — it cannot,
because the data is compiled in. A data update requires a rebuild,
which is the honest equivalent of a file change + restart.

## ADR-040: Float rounding is Rust half-away-from-zero, not Python banker's (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: ADR-024 already records this class of divergence for Wave 3
(score rounding). Wave 4 has the same class: SM-2 ease is rounded to
4 decimals and adhan times use Python `round()` (banker's).
Decision: Rust uses its native `round()` (half away from zero) in
`sm2_next` and in the adhan `_fmt` port (`py_round` reimplements
banker's rounding for the adhan display path where the difference is
visible in "HH:MM" output; SM-2 ease keeps Rust rounding).
Consequences: SM-2 ease values can differ from Python in the last
decimal in exact-half cases only; adhan times match Python.

## ADR-041: Default provider editions are AlQuran Cloud — offline use needs bundled settings (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The built-in defaults are translations `en.sahih` /
`fa.makarem`, tafsir `ar.muyassar` and reciter audio `ar.alafasy`,
all served by AlQuran Cloud. A plain `GET /quran/surah/{n}` or
`/quran/ayah/{s}/{a}` therefore makes a live AlQuran Cloud round-trip
for translation/tafsir/audio, even though the text itself is
self-hosted — exactly like Python.
Decision: Rust mirrors this exactly; it does not silently fall back
to bundled editions. The Wave 4 tests set bundled defaults
(`quran.translations=["fa.rowwad"]`, `quran.tafsirs=["fa.mokhtasar"]`,
`quran.reciters=[]`) or pass `?translation=fa.rowwad&tafsir=fa.mokhtasar`
to stay offline. A call without bundled editions against an
unreachable AlQuran Cloud answers 503 `quran_provider_unavailable`
with `{"provider": "alquran_cloud"}`.
Consequences: The default path needs the network in both codebases;
offline operation is a settings choice, not a code change.

## ADR-042: The Tanzil text prefixes every surah's first ayah with the basmala (finding, 2026-11-22)
Status: accepted
Context: In the bundled Tanzil Uthmani text, every surah's first ayah
(except surah 9) begins with the basmala — e.g. `2|1` starts
"بِسْمِ ٱللَّهِ ٱلرَّحْمَـٰنِ ٱلرَّحِيمِ الٓمٓ" — and 27:30 contains it
too. The 4-gram "بسم الله الرحمن الرحيم" therefore occurs 114 times.
Decision: The mutashabihat group for that phrase reports 114
occurrences; this is data, not a bug, and must not be "fixed" to 2.
Consequences: The Wave 4 tests pin 114. Any future basmala handling
(grading, live stream) must remember the basmala is part of the ayah
text in this data file, unlike the printed mushaf where it is a
separate heading.

## ADR-043: TEST_SCHEMA `platform_settings.value` is JSONB, like production (bug found by Wave 4, 2026-11-22)
Status: accepted
Context: The Wave 4 Postgres smoke failed on the very first surah
call with 500 `internal_error`. The cause: `get_setting` reads
`platform_settings.value` as `serde_json::Value`, but TEST_SCHEMA
declared the column `TEXT` while production declares it `JSONB`
(`Mapped[dict] = mapped_column(JSONB)` in `models.py`). On sqlite
the typeless store hides the mismatch; on Postgres sqlx refuses to
decode a TEXT column as a JSON value. No earlier smoke caught it
because the Wave 2/3 smokes never read a platform setting on
Postgres (the calendar-theme setting read is a sqlite-only test).
Decision: TEST_SCHEMA now declares `platform_settings.value JSONB`,
matching production. `get_setting`/`stage_setting`/`encrypt_value`
are unchanged — they already treat the value as a JSON envelope.
Consequences: Settings round-trip on Postgres (verified by the Wave
4 smoke). The same latent class (TEXT column read as JSON, or vice
versa) should be checked whenever a new settings-backed feature is
ported; the smoke suite must exercise at least one settings read on
Postgres, which Wave 4 now does.

## ADR-044: Videos reuses a library-visibility subset, not a full library port (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The videos module is a view over library items (`library/service.py::_public_base_stmt`, `_may_access`, `_signed_url`): only `workflow_status == "published"` items, visibility rules (guest `public` only, logged-in `public`+`registered`, privileged `library.manage` or a scholar/admin/super_admin role sees all), hidden/unpublished answers 404, never 403. There is no create/upload route in the videos module — that is library admin, Wave 6.
Decision: Wave 5 ports that subset as `library_visibility.rs` (`is_privileged`, `public_where`, `may_access`, `signed_url`, `LibraryItem`, `library_item_from_row`) and reuses it from `routes_videos.rs`. The full library port (admin CRUD, uploads, download-by-ref) is Wave 6; the videos code must not grow its own copy of those rules.
Consequences: Visibility behaviour is identical between the videos list, watch, saved and progress routes because they share the one helper. Any Wave 6 change to the library rules must keep these call sites in sync.

## ADR-045: Subtitle generation stays a Python sidecar (faster-whisper + ffmpeg + LLM); the HTTP trigger only marks rows pending (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `videos/tasks.py::generate_subtitles` (ARQ `media` queue) needs faster-whisper (model `small` default, production `base`) + ffmpeg decode: a source pass transcribe (1500 s timeout), an `en` pass with `task="translate"` (900 s), and other languages via the fatwa LLM provider (unconfigured → row failed `translation_provider_not_configured`). Rust has no ARQ worker and no faster-whisper; embedding one would violate ADR-008 and faking subtitles would violate the never-fake rule.
Decision: `POST /api/v1/admin/videos/{item_id}/subtitles/generate` is native but only validates (404 → `not_a_video` → embed 422 `external_video` → no media 422 `no_media_file`), calls `reset_rows` (delete all rows, insert `pending` per target lang from `videos.subtitle_langs`), audits `videos.subtitles.generate`, and answers 202 with `enqueued: false` — exactly the Wave 3 ingest-trigger precedent. The VTT build/parse rules (`build_vtt` escaping, clamping, `parse_vtt` skipping invalid blocks) ARE ported faithfully in `subtitles.rs` because they are pure and testable.
Consequences: Subtitles are never generated by the Rust binary. Until a sidecar is wired, generated subtitles only appear when the Python worker runs against the same database.

## ADR-046: Voice transcribe/ask stay proxied — no STT in the Rust binary (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `POST /api/v1/voice/transcribe` and `POST /api/v1/voice/ask` exist only to run STT (faster-whisper sidecar per ADR-008, never in the Rust binary): quota first, then capped read, then `scan_upload_or_reject`, then audio validation, then STT. Registering them natively would answer an honest 503 where the Python origin transcribes for real — a functional regression for a working feature.
Decision: Neither route is registered natively. Both fall through to the proxy, exactly like the Wave 4 hifz ASR endpoints (ADR-031 precedent). Quota, scan and validation for these paths are enforced by the origin on the proxied path. The wave5 tests pin the fall-through (502 `bad_gateway` with a dead origin).
Consequences: Voice transcription keeps working through the origin while the Rust binary stays STT-free. A native ask becomes possible only once an STT sidecar is wired; the ported `voice_intents`/`voice_tts` modules (ADR-047) are the library it will call.

## ADR-047: Voice intents + TTS are ported as modules, not as a native ask route (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `voice/intents.py` (language detection, intent classification, ayah-reference extraction) and `voice/tts.py` (provider resolution, cloud synthesis, degrade-to-text) are pure/deterministic and portable — except that `answer_text`'s fatwa branch calls `fatwa_service.ask` (LLM), which stays proxied per ADR-026, so there is no honest native answer for that branch today.
Decision: `voice_intents.rs` and `voice_tts.rs` are ported faithfully (normalisation, word-boundary matching by hand because the Rust `regex` crate has no lookahead/lookbehind, surah/topic maps verbatim with insertion order preserved for tie-breaking, OpenAI-shape cloud TTS with 30 s timeout and honest degradation to text + notice `tts_unavailable`) and unit-tested, with a module-level `#![allow(dead_code)]` until a native route calls them. The `answer_text` pipeline itself is NOT ported.
Consequences: The deterministic voice pieces are ready for a future native `/voice/ask` once the STT sidecar exists (ADR-046). No route today claims to answer by voice.

## ADR-048: Audio lessons (teacher lessons, students, groups, certificates) are Wave 6 LMS, not voice (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The user's 2026-10-26 audio-lessons spec (teacher lessons, defining students, forming groups, issuing a verifiable PDF certificate for a course level) has no routes in `app/modules/voice/` at all. Those concepts live in LMS: courses, cohorts, enrollments, certificates (`SIGNED_PURPOSE_CERTIFICATE`).
Decision: Wave 5 ports only what the voice module contains (transcribe/ask proxy, TTS download, admin settings, usage log). Lessons/students/groups/certificates are Wave 6 per ADR-012.
Consequences: Nothing in the voice module is stretched to cover LMS. The Wave 6 port must cover the certificate signing/download path too.

## ADR-049: Voice settings are raw strings — most `voice.*` keys are not in Python's SETTING_DEFS (finding, 2026-11-22)
Status: accepted
Context: Only the three `voice.tts_cloud_*` keys are registered in `core/platform_settings.py` `SETTING_DEFS` (string/secret). The other six voice keys (`stt_provider`, `tts_provider`, `max_audio_seconds`, `max_audio_mb`, `tts_voice_fa`, `tts_voice_ar`) are NOT registered — so Python's `get_typed_setting` returns their raw decrypted strings.
Decision: Rust reads voice settings with `get_setting` (raw) and parses ints with Python `int()` semantics + default fallback (`voice_setting_raw` / `voice_int_setting` in `voice_tts.rs`). GET settings therefore returns a stored value as a string (e.g. `"60"`) but an unset key as its typed default (e.g. int `120`) — replicated as-is, including the oddity. PUT validates keys against the nine known keys, provider enums and int ranges 1–3600, in two passes (validate all, then stage) because Rust has no transaction and Python rolls the whole patch back on error; `bump_settings_version` increments by 1.
Consequences: The settings API is byte-compatible with Python including the string/int asymmetry. If Python ever registers the six keys, this ADR must be revisited.

## ADR-050: Upload scanning — no Wave 5 upload route goes native; ClamAV is NOT reintroduced (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The user decided 2026-11-19 to move away from ClamAV (replacement TBD: VirusTotal cloud, local YARA, or hash-check + manual admin review). The Wave 2 behaviour is fail-closed: 503 `antivirus_unavailable` unless `CLAMAV_ALLOW_INSECURE_DEV_BYPASS=true` and `APP_ENV=dev`.
Decision: No Wave 5 route needs a scan — videos has no upload route (library admin is Wave 6) and voice transcribe/ask are proxied (ADR-046) and scanned by the origin. The Wave 2 fail-closed gate is preserved wherever uploads go native; ClamAV is NOT reintroduced as a default; a replacement scanner is still TBD per the user's decision.
Consequences: No un-scanned upload path is created by Wave 5. Wave 6 (library uploads) must resolve the scanner question before going native.

## ADR-051: Library upload scanning — SHA-256 blocklist + EICAR signature + manual admin review (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The user decided 2026-11-19 to move away from ClamAV (ADR-050); Wave 6 must resolve the scanner question before library uploads go native. The candidate replacements were VirusTotal cloud, local YARA, or hash-check + manual admin review.
Decision: Every uploaded byte passes one gate, `scan_upload`, before any validation or storage write: (a) the SHA-256 of the bytes is compared against the admin-managed setting `library.blocked_hashes` (JSON array of hex digests, empty default, read via the encrypted settings store); (b) the byte stream is scanned for the standard EICAR test signature. A hit returns 422 `file_infected` with `details.signature` (`"blocked-hash"` or `"EICAR-STANDARD-ANTIVIRUS-TEST-FILE"`) and writes audit row `library.upload_rejected_infected`, exactly like Python. Clean bytes pass. All upload paths (create/update file, cover, fetched Aparat cover) go through this one gate, and every upload lands as `draft`, so the workflow review is the manual gate before anything is `published`. There is no bypass flag and no un-scanned upload path.
Consequences: ClamAV is NOT reintroduced. The blocklist is admin-managed through the existing settings surface (ADR-059). This is the hash-check + manual-review option from the user's 2026-11-19 decision.

## ADR-052: External video metadata — best-effort YouTube oEmbed + Aparat, never a fabricated title (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: For `kind=video` with an external link and no manual title, Python resolves the title from the video provider.
Decision: The title is resolved in order: YouTube oEmbed (`https://www.youtube.com/oembed?url=…&format=json`, 4 s timeout), then the Aparat JSON API (`https://www.aparat.com/etc/api/video/videohash/…`, 4 s), then og-scrape of the Aparat page. Any failure (network, non-2xx, unparseable body) yields `None`, never a fabricated title; if no title is resolvable the create fails 422 `title_required`. Canonical URLs are produced by the shared `video_links.rs` (YouTube watch URLs, Aparat watch URLs); a direct-file URL is accepted only for `https` URLs whose path ends `.mp4/.webm/.m4v/.mov`. A fetched Aparat cover goes through the same scan gate (ADR-051) and the 5 MB cover cap.
Consequences: No title is ever invented. Live-provider failures surface as an honest 422, not a guessed title.

## ADR-053: Library media pipeline stays a Python sidecar; Rust only marks rows pending (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: There is no Rust media worker (no faster-whisper, no OCR engine in the Rust binary, ADR-008).
Decision: `reprocess` (`transcribe`/`ocr`) and the upload/create paths only set `transcript_status`/`ocr_status` to `pending` and clear `processing_error`; the response reports `enqueued: false` (in the audit `new_values`, the response body being the item itself). This follows the Wave 3 ingest precedent (ADR-022): the Python ARQ worker does the work, Rust triggers honestly and never claims a job was queued when it was not.
Consequences: Transcripts/OCR text appear only when the Python worker runs against the same database. No media work is faked.

## ADR-054: index-ai is native with the hash embedding provider; chunk approval is governed by fatwa.approve (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `POST /api/v1/admin/library/items/{id}/index-ai` runs the ingestion pipeline: chunking, embedding, chunk rows, ingestion-run stats.
Decision: The pipeline is native: `chunk_text_paragraphs` ported verbatim from Python (500-word max, 50-word overlap, 20-word min; sentence split after `.!? ؟ 。` + whitespace, hand-rolled because Rust `regex` has no lookbehind), `ingestion_normalize` (whitespace-only, distinct from `embeddings.normalize_text`), hash embeddings only (`embedding_vec` is never named, ADR-030), chunks written to `fiqh_knowledge_chunks` with `source_type='ingested'`, `source_ref={type:library_item, id, version:str}`, plus an `ingestion_runs` row with stage stats. Only `approved`/`published` items are indexable (else 422 `item_not_indexable`); an item with no fatwa scope fails 422 `no_ai_scopes`. Without the `fatwa.approve` permission the chunks stay `draft` and `ai_indexed` stays `false`; with it they are approved and `ai_indexed` becomes `true`. Re-indexing inserts fresh chunks and does not delete old ones. Response shape: `{item_id, ai_indexed, chunks_created, chunks_approved, chunk_ids, scopes, detail}`.
Consequences: The RAG index path is fully native with the hash provider (ADR-019); real embedding providers remain a Wave 7+ follow-up.

## ADR-055: Admin library list keeps admin fields and nulls the signed URLs (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's admin library output differs from the public output in which fields it carries.
Decision: The admin list/detail shape (`LibraryItemAdminOut`) carries `transcript`/`ocr_text`/`processing_error` after `cover_url`, but `media_url`/`cover_url` are null in the admin list — exactly as Python's `_to_out(..., None, None)`. The public shape never exposes `transcript`/`ocr_text`/`processing_error`.
Consequences: Admin and public shapes match Python field-for-field, including the null URLs.

## ADR-056: Search score rounding — Rust half-away-from-zero vs Python banker's rounding (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Library search scores are rounded to 3 decimals.
Decision: Rust uses its native `round()` (half away from zero) rather than Python's `round(x, 3)` (banker's). The two differ only on exact-half values (e.g. x.xxx5); ranking and `matched_in` are unaffected.
Consequences: Accepted as an ADR-024/040-class edge difference, not worth a special-case port.

## ADR-057: Multipart create parses metadata before enforcing file-size errors (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The order of validation checks in multipart create/update matters for which error a bad request gets.
Decision: The `metadata` JSON field is parsed (422 `invalid_metadata` on bad JSON/kind) before file bytes are size-checked, matching the Python router's order. File bytes are buffered in memory like Python's `read_upload_capped`; a provisional streaming cap of 512 MB (file) / 5 MB (cover) applies, then the per-kind limit returns 413 `file_too_large` (`max_bytes`, `kind` from the validate path). A cover oversize at read time is also `file_too_large` with `max_bytes: 5242880` per Python's `read_capped_upload` defaults — `cover_too_large` is unreachable via the router.
Consequences: The same bad request gets the same error code from Rust and Python.

## ADR-058: download-by-ref with a missing file returns 500 internal_error (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python raises `FileNotFoundError` when a signed key resolves to a library item whose file is absent from storage, and the generic handler turns it into a 500.
Decision: Rust mirrors this exactly: missing file → 500 `internal_error`. Unknown keys stay 404 `key_not_found`; bad/expired signatures stay 403 `invalid_signature`; missing params stay 422.
Consequences: Faithful to Python, including the upstream wart; a future product decision could map the missing-file case to 404 instead.

## ADR-059: Blocked upload hashes live in the encrypted setting library.blocked_hashes (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The SHA-256 blocklist (ADR-051) needs a store admins can manage.
Decision: The blocklist is a JSON array of lowercase hex SHA-256 digests stored in the encrypted platform setting `library.blocked_hashes` (empty default = no hashes blocked), managed by admins through the existing settings surface. A match is a hard reject (422 `file_infected`, `signature: "blocked-hash"`), not a quarantine — there is no quarantine store in the Rust app.
Consequences: Blocking a hash is a settings write; unblocking is removing it from the array.

## ADR-060: religious_place_ref and oversized integers fail 422 instead of 500 (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Where Python would 500 on a DB constraint (overlong text / out-of-range integer), Rust validates up front.
Decision: `religious_place_ref` ≤ 128 chars; `duration_seconds`/`file_size_bytes` must fit i32 — else 422. Same ADR-024 class as the other explicit 422s in this module.
Consequences: The 422s are strictly more helpful than Python's 500s; normal requests behave identically.

## ADR-061: Certificate PDF stays a Python sidecar; the certificate record is issued natively (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `lms/certificates.py::render_certificate_pdf` needs reportlab + arabic-reshaper + python-bidi + system Arabic/Latin fonts (ADR-008). No Rust PDF crate covers the trilingual (fa/ar/en in one page), reshaped, bidi-correct layout, and faking a PDF would violate the never-fake rule.
Decision: On course completion the Rust API issues the certificate record natively — `code` (`MA-{year}-{8 uppercase hex}`, 5 attempts then 500 `cert_code_failed`), `user_id`, `course_id`, `issued_at`, `locale`, plus the `lms.course_completed` and `lms.certificate_issued` domain events — but `pdf_storage_ref` stays NULL. A Python sidecar (the existing `render_certificate_pdf`, run as a worker against the same database) picks up certificates with `pdf_storage_ref IS NULL`, renders the PDF, stores it under `certificates/{code}.pdf`, and fills in `pdf_storage_ref`. The download routes (`GET /certificates/{code}/download`, `GET /certificates/download-by-ref` with purpose `lms-certificate`) are native and serve only bytes actually stored under the certificate's `pdf_storage_ref`; while the sidecar has not rendered the PDF they answer 404 `certificate_not_found`, exactly like Python when `pdf_storage_ref` is empty. Verify (`/certificates/{code}/verify`) and `mine` work immediately, since they never need the PDF.
Consequences: Certificates are verifiable from the moment of issue; the PDF appears once the sidecar renders it. No PDF bytes are ever fabricated by the Rust binary. The sidecar's exact trigger (poll vs. queue) is Wave 7/8 work.

## ADR-062: list_cohorts ignores the after cursor (faithful mirror of the Python quirk) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `list_cohorts` accepts `after` but never applies it to the query — it fetches `limit+1` rows and still mints `next_cursor = _encode_cursor(0, last.id)`. The courses list, by contrast, applies its cursor correctly.
Decision: Rust mirrors this exactly: `GET /lms/cohorts` accepts `after`, ignores it, returns the first page plus a cursor that, if followed, returns the same first page. Fixing the quirk would change pagination behaviour vs. Python and is a product decision, not a port decision.
Consequences: Cohort pagination behaves identically (including the quirk) in both codebases; the Wave 6 tests pin the behaviour. If the user wants real cohort cursors, both codebases change together.

## ADR-063: LMS LocalizedText is a loose dict[str, str], not strict fa/ar/en (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `lms/schemas.py` defines `LocalizedText = dict[str, str]` — any keys, string values only — unlike the strict `{fa, ar, en}` `LocalizedText` used by home/content (enforced by `validate::req_localized`).
Decision: LMS titles/descriptions are validated with a loose validator (`req_text_map` / `opt_text_map`): must be a JSON object whose values are all strings; no key is required. Values are stored verbatim as JSONB.
Consequences: `{"fa": "..."}` alone is valid for a course title, as in Python; the strict validator must not be applied to LMS fields.

## ADR-064: Duplicate prerequisite / enrollment races are detected by pre-check, not by catching IntegrityError (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python maps the DB `IntegrityError` on duplicate prerequisite / concurrent enrollment to 409 (`prerequisite_exists` / `already_enrolled`). The Rust `Db` layer converts any failed statement into a plain 500 `ApiError`, so the handler cannot see the underlying UNIQUE violation.
Decision: Handlers detect the duplicate with a SELECT before INSERT (prerequisite pair; enrollment already checks the existing row for the active case). A genuine concurrent race between the check and the insert would still surface as a 500 instead of Python's 409 — a known, untested edge — rather than inventing error plumbing in the shared `Db` layer for one wave.
Consequences: All non-race duplicate paths return the exact Python 409s (covered by tests); the race window is documented here for Wave 7 to revisit if `Db` ever surfaces constraint errors.

## ADR-065: Exact decimal/fraction arithmetic without a decimal crate (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `Cargo.toml` is outside the Wave 6 scope and no decimal crate is in the dependencies, but the calculator engines need exact decimal arithmetic (Python `Decimal`).
Decision: `Dec` (mantissa i128 × power of 10, preserving the input exponent so `str()` matches Python's `str(Decimal)` exactly, including trailing zeros and scientific notation) and `Frac` (exact normalised fraction) are implemented in `calc_decimal.rs`. Division reproduces Python's default context (28 significant digits, ROUND_HALF_EVEN with the 29th digit + sticky); `quantize` uses ROUND_HALF_UP (ties away from zero); `floor_div` is `Decimal.__floordiv__`. There is no `f64` anywhere in the engines, so the ADR-024/040 rounding divergence does not occur in this module.
Consequences: Calculator arithmetic is exact and byte-identical to Python's `Decimal` behaviour on the tested paths; two early implementation bugs (swapped `quantize` branches, `floor_div` sign order for negatives) were caught and fixed by unit tests in the same session.

## ADR-066: NUMERIC columns via CAST AS TEXT reads and CAST(? AS NUMERIC) writes (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Following ADR-014/ADR-028, `NUMERIC(20,4)` and `NUMERIC(20,6)` columns cannot be decoded as `String` by sqlx-postgres over the binary protocol.
Decision: NUMERIC columns are read in SELECT with `CAST(col AS TEXT)` and written in INSERT with `CAST(? AS NUMERIC(20,4|6))`. Values are formatted in the app with `Dec` (estate 4dp, prices 6dp) so sqlite and Postgres behave identically.
Consequences: NUMERIC round-trips exactly on both backends (pinned by the Postgres smoke).

## ADR-067: get_active_rule filters in Rust, not SQL (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `get_active_rule` must pick the active approved rule config for a madhhab.
Decision: It reads all approved rows in version-descending order and filters in Rust: the effective window (`effective_from <= now < effective_to|∞`), madhhab match (inheritance: exact; zakat: `madhab IS NULL` when the input is None), and the presence of the required key in `rule_data` — following the ADR-014 guidance for expiry comparisons.
Consequences: The same rule is active in Rust and Python for the same database state.

## ADR-068: Zakat price settings read raw via get_setting (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The keys `zakat.price_source_url` and `zakat.price_ttl_seconds` are not registered in Python's `SETTING_DEFS`, so Python's `get_typed_setting` returns them raw.
Decision: Rust reads them with `get_setting` (not `get_typed_setting`) so the URL is not parsed as JSON. TTL default 21600 seconds with a 60-second floor, matching `prices.py`.
Consequences: Price settings behave identically, including the raw-string quirk (ADR-049 class).

## ADR-069: Estate and prices formatted to 4dp/6dp in the app (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: sqlite's NUMERIC affinity drops trailing zeros (`"1200.0000"` → `1200`) while Postgres `NUMERIC(20,4)` keeps them, so the same value reads back differently on the two backends.
Decision: `estate_value` is formatted with `fmt_fixed(4)` before INSERT and nisab prices with `fmt_fixed(6)`, so sqlite tests show exactly the Postgres behaviour. This formatting is Python's `str()` on the quantised `Decimal`, not new rounding.
Consequences: Golden values match on both backends.

## ADR-070: INTEGER columns read as i32 on Postgres (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `version` in `rule_to_out`, `get_active_rule` and `rule_update` was first read as i64; it worked on sqlite but the Postgres smoke failed with 500, because sqlx only decodes a Postgres INTEGER column as i32 (ADR-028).
Decision: All three places read `try_get::<i32>(...) as i64`. The smoke went green.
Consequences: Same ADR-028 class as the Wave 3/4 fixes; the rule applies to every INTEGER column in every wave.

## ADR-071: Inheritance-cases cursor pagination ported faithfully, UUID-order filter with time-order sort (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `service.list_inheritance_cases` sorts by `ORDER BY created_at DESC` but filters the cursor with `id < UUID(cursor)`; the second page is therefore UUID-ordered, not time-ordered, and may come back empty or duplicated.
Decision: Rust reproduces this exactly (compatibility beats "fixing" it). The integration test asserts only the first page + cursor presence + the full list, not the second page's contents.
Consequences: Pagination behaves identically (including the quirk) in both codebases; fixing it is a product decision for both codebases together.

## ADR-072: kind is optional on the rule list, required on item routes (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `GET /api/v1/admin/calculators/rules` without `kind` merges both kinds and returns them by `created_at` descending (stable sort); an invalid `kind` → 422 `bad_rule_kind`. The item routes (`get/update/submit/review/history`) require `kind` (FastAPI 422 if absent) and return 404 `rule_not_found` for the wrong kind.
Decision: Rust mirrors both behaviours exactly.
Consequences: The same request gets the same status from Rust and Python.

## ADR-073: Engine error codes unreachable via HTTP (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `invalid_gender` (miras), `invalid_transitivity` and `invalid_harf_jar` (mosaref) exist in the engines but can never surface over HTTP, because Pydantic in Python (and the equivalent validation in Rust) rejects those inputs first as 422 `validation_error`.
Decision: The codes exist only in the engine unit tests, not in any HTTP path.
Consequences: No HTTP test asserts them; the engine tests pin them.

## ADR-074: Mosaref ported natively, not proxied (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The 1398-line mosaref engine is mechanical string construction; the Wave 6 scope includes it, and proxy-out is only for routes that cannot be ported faithfully.
Decision: The engine is ported natively in full: the `HARF_JAR_FORMS` tables and all renderers verbatim. One port bug was caught by a unit test in the same session: in `_build_naqis` (Python line 393) the present-tense prefix vowel of the sound form is always `َ`; Rust first used `pres_ain` and produced `يُدْعُو` instead of `يَدْعُو` — fixed.
Consequences: Mosaref needs no sidecar and no proxy; its golden fixtures pin the output.

## ADR-075: Golden-fixture values vs exact engine output (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: In the zakat fixture, `zakat_coins: "0.5523"` is the display value of the source page's float; the engine's exact output is `"0.5522727272"`, and the test `test_zakat_money_is_decimal_exact` pins that. The fixture key `extra_money_at_source_price` is the source page's field name; the engine (Python and Rust) names the field `extra_money`, and the golden test performs that mapping.
Decision: These are not divergences and must not be "fixed".
Consequences: The golden tests pin the exact engine values; display rounding stays a presentation concern.

## ADR-076: Finance receipt PDF stays proxied (FIN-01) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `GET /api/v1/finance/receipts/{id}` renders a trilingual receipt PDF with reportlab + arabic-reshaper + python-bidi + system fonts (ADR-008 / ADR-061 class).
Decision: The route stays proxied so the Python origin keeps rendering it for real, instead of answering an honest 503 for a working feature (ADR-046 precedent).
Consequences: No native receipt-PDF route exists; the proxy fallback must stay for this path until a Rust renderer exists.

## ADR-077: Receipt images go through the Wave 6 scan gate (FIN-02) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's finance module itself never scanned receipt images. The Rust port applies the Wave 6 scan gate (ADR-051: SHA-256 blocklist + EICAR + pending/review workflow) to receipt uploads — a deliberate, task-directed hardening, not a Python behaviour.
Decision: A scan hit is 422 `file_infected` with an audit row `finance.upload_rejected_infected`.
Consequences: Receipt uploads are stricter than Python. Merge check (resolved 2026-11-22): the reported signature name `"Eicar-Test-Signature"` matches Python `core/antivirus.py` (`"stream: Eicar-Test-Signature FOUND"`); ADR-051's `EICAR-STANDARD-ANTIVIRUS-TEST-FILE` is the byte-pattern fragment inside the EICAR test file, not the reported name. There is no discrepancy and no code change.

## ADR-078: ZarinPal + IDPay live over HTTPS (FIN-03) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Online donation intents need the real gateways; a sandbox or stub would silently take no money.
Decision: ZarinPal and IDPay request + verify run live over HTTPS against the providers, with the same intent/callback flow as Python. No sandbox mode exists in the Rust binary.
Consequences: Gateway credentials must be configured for donations to work; an unconfigured gateway answers 503 `gateway_unconfigured`.

## ADR-079: FX rates via Wallex/CoinGecko with exact string-decimal math (FIN-04) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: FX conversion touches real money; float arithmetic would drift from Python's exact decimal results.
Decision: Rates come from Wallex/CoinGecko over HTTPS and all conversion math uses exact string-decimal arithmetic (no f64 anywhere in the path), mirroring Python.
Consequences: FX results match Python to the last digit; a provider outage surfaces as an honest error, never a stale invented rate.

## ADR-080: Reconciliation honestly 503s when it cannot run (FIN-05) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Reconciliation needs live gateway statements; there is no local equivalent to compute it from.
Decision: When reconciliation cannot run (no gateway access / no statements), the endpoint answers an honest 503 instead of returning a fabricated reconciled state.
Consequences: Operators see the outage instead of a false "all matched" report.

## ADR-081: `donations.payment_intent_id` added; amounts stay INTEGER (FIN-06) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Donation intents need a join key to the gateway intent; Python's `donations` table has no `payment_intent_id` column in the test schema.
Decision: TEST_SCHEMA gains `ALTER TABLE donations ADD COLUMN payment_intent_id UUID`. `donations.amount` stays INTEGER per ADR-028 (i32 on Postgres, read as i32).
Consequences: Intent lookup is a single indexed column; no amount-precision change anywhere.

## ADR-082: Multipart 2 MB default disabled for `POST /donate/manual` only (FIN-07) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Axum's `Multipart` extractor defaults to a 2 MB body limit; a manual donation with a receipt image can legitimately exceed it.
Decision: `DefaultBodyLimit::disable()` is layered on `POST /api/v1/finance/donate/manual` only. The same latent 2 MB cap affects Wave 6 library uploads and is a follow-up, not touched here.
Consequences: Large receipt images upload; every other route keeps the default limit.

## ADR-083: Idempotency-key replay and `_finalize_success` re-entrancy (FIN-08) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: A retried callback or a double-submitted intent must not create a second donation.
Decision: Ported exactly — replaying an intent's idempotency key returns the stored result, and `_finalize_success` is re-entrant: one donation + one ledger row per intent, campaign credit in IRR.
Consequences: Double-charging by retry is impossible in Rust exactly as in Python.

## ADR-084: Manual-approve FX conversion — manual rate wins (FIN-09) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Approving a manual (card/crypto/PayPal) donation converts its amount to IRR; Python prefers the rate the admin typed over the live rate.
Decision: The manual rate takes precedence over the live rate, with the `manual:<intent_id>` fallback reference kept verbatim.
Consequences: Admin-entered rates are honoured exactly as in Python; the fallback reference is stable across both stacks.

## ADR-085: Recurring donations are registration-only (FIN-10) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Periodic charging is the ARQ worker's job in Python; there is no charging loop in the Rust binary.
Decision: The Rust API registers recurring donations natively; it never charges them. Charging stays a Python sidecar concern.
Consequences: A recurring donation registered in Rust is charged by the Python worker exactly as before; no charge is ever faked.

## ADR-086: Gateway statuses and donation methods are native reads (FIN-11) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Gateway status and the donation-methods list are settings/DB reads; no provider ping is needed to answer them.
Decision: Both are served natively from settings and the database. No liveness ping to a gateway is performed or faked.
Consequences: The endpoints answer even when a gateway is down; they report configuration, not liveness.

## ADR-087: Admin `/rebuild` is a real GitHub `workflow_dispatch`, never a shell (ADM-01) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The builds rebuild endpoint must trigger a real rebuild; shelling out from the API binary would be a command-injection surface and would not work in the Rust deploy.
Decision: Rebuild dispatches a real GitHub Actions `workflow_dispatch` over HTTPS. No shell is ever spawned.
Consequences: A rebuild needs a configured GitHub token; without one the endpoint answers 503 instead of pretending to rebuild.

## ADR-088: Release download URLs for `artifact_key` rows cannot be presigned (ADM-02) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The builds bucket is MinIO/S3, which the Rust binary does not own (ADR-015: no S3 in the Rust binary).
Decision: Release creation is native, but download URLs for `artifact_key` rows are not presigned in Rust; the field is left for the Python origin.
Consequences: Release rows are complete except for a presigned URL; no URL is ever fabricated.

## ADR-089: AI/Telegram connectivity tests report honestly (ADM-03) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: A connectivity test that cannot reach the provider must not claim success.
Decision: The AI and Telegram connectivity tests report what they can actually verify; when the provider cannot be reached they say so instead of faking a ping.
Consequences: A green test means a real check passed; a red test is a real outage, not a stub.

## ADR-090: Admin rate limits are NOT enforced in Rust (ADM-04) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python throttles some admin endpoints at 20–30/min; no prior wave wires route-level rate limits in the Rust binary, so there is no shared limiter to reuse.
Decision: Admin rate limits are not enforced in Wave 7. This is a known gap, recorded as follow-up work, not a silent behaviour change.
Consequences: Admin endpoints in Rust accept bursts Python would throttle; the gap must be closed by a shared limiter, not per-route hacks.

## ADR-091: Roles-cohort broadcasts dedup with `DISTINCT` (ADM-05) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's roles-cohort broadcast counts a user once per role, so a user with two roles is double-counted in the recipient total.
Decision: Rust dedups with `DISTINCT` — one recipient row per user. This is a deliberate deviation from Python's double-count bug.
Consequences: Broadcast recipient counts in Rust are correct; Python's count is higher by the number of multi-role users.

## ADR-092: TEST_SCHEMA gaps fixed for roles/permissions/settings (ADM-06) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The admin routes read `roles.description`, `permissions.description` and `platform_settings.updated_at`, which TEST_SCHEMA did not define — the routes 500'd on a fresh test database.
Decision: The three columns were added to TEST_SCHEMA (schema fix only, no behaviour change).
Consequences: Admin tests pass on a fresh schema; production schema is unchanged (the columns already exist there).

## ADR-093: `super_admin` guards on role/permission CRUD (ADM-07) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Role and permission changes are the highest-privilege admin action; Python guards them behind `super_admin`.
Decision: The guard is ported exactly — non-super-admin callers get 403 on role/permission create, update and delete.
Consequences: No privilege escalation through the Rust admin API.

## ADR-094: Custom font uploads use the ADR-051 scan gate (ADM-08) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Admin font upload accepts arbitrary files; an unchecked upload is the same attack surface the Wave 6 library gate closes.
Decision: Font uploads run the ADR-051 scanner (SHA-256 blocklist + EICAR + manual admin review) exactly like library uploads.
Consequences: A blocked-hash or EICAR font is rejected with 422 `file_infected` and an audit row, never stored.

## ADR-095: Dead-letter retry marks pending, never enqueues (ADM-09) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Retrying a dead-letter event needs the ARQ dispatcher, which stays Python (PLW-08).
Decision: The retry endpoint is native for listing and for marking the event pending again; it never claims a job was enqueued (`enqueued: false`).
Consequences: A retried event waits for the Python dispatcher exactly as in Python; no retry is ever faked.

## ADR-096: Users-admin lifecycle is fully native (ADM-10) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The users dashboard/stats/trash, the 360° overview, role replacement, session revocation, suspend/activate, soft delete, restore and permanent delete are all pure DB/state operations.
Decision: All are ported natively with Python's exact guards (including the super_admin checks on role replacement and permanent delete).
Consequences: User administration works identically on both stacks; no user row is ever mutated by a path Python would forbid.

## ADR-097: Platform settings GET/PUT and re-encrypt are native (ADM-11) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Settings are the control plane for every other module; a proxied settings write would split the source of truth.
Decision: Settings read/write and the settings re-encrypt endpoint are native, using the same encrypted-setting helpers as Waves 2–6.
Consequences: Settings written through Rust are visible to Python immediately (same table, same encryption).

## ADR-098: Content-center adapters are a static registry of 11 (ACC-01) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python registers its eleven content adapters at import time (content ×5, fatwa ×2, library, LMS, places, social); each maps one (domain, content_type) pair onto its own table with the shared §9 workflow machine. No generic content table exists, in Python or here.
Decision: Rust uses the same static registry — identical list, identical pairs, no dynamic registration.
Consequences: An unknown (domain, content_type) 404s exactly as in Python; adding a domain means adding an adapter in both stacks.

## ADR-099: `GET /items` merges, sorts and cursor-pages like Python (ACC-02) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The center inbox merges per-adapter lists; the merge order and cursor format are observable behaviour.
Decision: Ported exactly — merge, sort by (-created_at, domain, content_type, id), page with the `{"t","d","ct","id"}` cursor, same as `list_items`.
Consequences: Paging through the inbox is identical on both stacks, including the cursor's opaque shape.

## ADR-100: Adapter transitions reuse the domain transition + center audit (ACC-03) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: A center transition must run the domain's own transition (CMS, fatwa submissions, library, places) — with its own domain event and audit — and the center must add its own audit on top.
Decision: Ported exactly: domain transition first (its event + audit), then the center's `content.transition` / `content.bulk.transition` audit.
Consequences: The audit trail for a center transition is identical to Python's, row for row.

## ADR-101: Fatwa draft transition emits NO domain event (ACC-04, resolved at merge) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The Wave 7 content-center path emits no domain event for a draft transition, but Wave 3 `routes_fatwa.rs` emitted `fatwa.submission_updated` for it. One of the two had to be wrong.
Decision: The center is right. Python's `_TRANSITION_EVENTS` (in `submissions_service.py`) has entries only for `in_review`, `approved`, `published`, `rejected` and `archived` — no `draft` entry — so `service.transition` emits nothing for a draft target, and both Python paths (submissions router and content-center adapter) call that same function. At the Wave 7 merge, `routes_fatwa.rs` was fixed to match: the event is now emitted only for the five mapped targets, and a draft (or any other unmapped) target emits no event.
Consequences: Both Rust paths now match Python exactly; the `fatwa.submission_updated` event type no longer exists anywhere in the Rust binary.

## ADR-102: Telegram distribution hooks are skipped in the center (ACC-05) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's center transitions notify Telegram (ADR-025); the Rust binary has no bot runtime.
Decision: Transitions never notify Telegram from Rust. The transition itself is unaffected.
Consequences: A transition made through Rust does not fan out to Telegram until the Python worker sees the domain event.

## ADR-103: AI reindex trigger marks queued with `enqueued: false` (ACC-06) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Reindexing is an ARQ job (ADR-022 precedent); there is no indexer in the Rust binary.
Decision: The trigger only marks the ingestion run `queued` and answers `enqueued: false` honestly.
Consequences: No reindex is ever faked; the Python worker picks the run up exactly as in Python.

## ADR-104: `GET /distribution` never 404s on a missing item (ACC-07) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `get_distribution` 404s only on an unknown adapter, never on a missing item.
Decision: Ported exactly — a missing item returns its distribution state (empty), an unknown adapter 404s.
Consequences: Clients cannot distinguish "no distribution" from "missing item" through this endpoint, exactly as in Python.

## ADR-105: Bulk vs single transition audit actions differ (ACC-08) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python logs `content.bulk.transition` for bulk transitions and `content.transition` for single ones; the action name is how operators tell them apart.
Decision: Ported exactly — the audit action follows the call shape, not the domain.
Consequences: Audit queries by action name return the same rows on both stacks.

## ADR-106: Placement reorder and distribution policy are native (ACC-09) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Reordering placements and setting a distribution policy are pure DB writes with validation; no worker is involved.
Decision: Both are ported natively with Python's exact validation (empty `ordered_ids` → 422, >200 → 422, unknown placement id → 404).
Consequences: Homepage placement order set through Rust is live immediately, same as through Python.

## ADR-107: No generic content table (ACC-10) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The center is one inbox over many tables; a generic table would be a new source of truth that Python does not have.
Decision: No generic content table exists in Rust either. Every adapter reads its own domain table.
Consequences: The center can never show a row that no domain owns; schema stays one-to-one with Python.

## ADR-108: Social mark-read persists — deliberate deviation (SOC-01) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's social `mark_notification_read` only flushes and never commits, so the read is silently lost; the sibling users endpoint (`/users/me/notifications/{id}/read`, native since Wave 2) commits, and the social `unread-count` endpoint only makes sense if reads stick.
Decision: `POST /social/notifications/{id}/read` persists the read with a real `UPDATE`. (The `routes_social.rs` module doc cites this as SOC-02; the worker's draft list calls it SOC-01 — same decision, numbering only.)
Consequences: Unread counts in Rust are correct; in Python they never decrease through this endpoint.

## ADR-109: Banned-word filter ported verbatim (SOC-02) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The filter decides what users may post; any difference in normalisation would admit or reject different text.
Decision: Ported verbatim — runs before insert on posts and comments (422 `content_policy_violation`, never echoing the matched word), with Python's `_normalize_for_match` (diacritics stripped, Arabic Yeh/Kaf folded to Farsi, whitespace collapsed).
Consequences: The same text is accepted or rejected on both stacks.

## ADR-110: Social cursor pagination reuses the shared helpers (SOC-03) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `_encode_cursor` / `_decode_cursor` produce base64url `JSON([iso, id])`; a different cursor format would break clients paging across the two stacks.
Decision: Social cursors reuse `validate::{encode_cursor, decode_cursor}` — the same format as Python.
Consequences: A cursor minted by Python pages correctly in Rust and vice versa.

## ADR-111: Social permission gates match the Python router (SOC-04) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Group creation, the report queue, report actions and banned-word management are all permission-gated in Python.
Decision: Ported exactly — `create_group` requires `social.moderate`, the report queue and report actions require `social.moderate`, banned words require `moderation.words`.
Consequences: No social moderation action is reachable in Rust that Python would forbid.

## ADR-112: Social feed, follows, likes, reports are native (SOC-05) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Groups, feed/posts, comments, likes, follows, reports + moderation queue and the notifications surface are all pure DB/state operations.
Decision: All are ported natively with Python's exact shapes and guards.
Consequences: The social surface works identically on both stacks; no social row is ever written by a path Python would forbid.

## ADR-113: `sync_changelog.seq` race is NOT closed (SYN-01) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `sync_changelog.seq` is allocated as `MAX(seq)+1` by the shared `sync::record_entity_change` helper (ADR-035); two concurrent writers can allocate the same seq.
Decision: The race is not closed in Wave 7. The fix (a sequence or an atomic allocation) is specified as follow-up work; the helper keeps the `MAX+1` allocation exactly as Python does.
Consequences: Under concurrent sync writers the Rust binary has the same race Python has — no worse, no better — until the follow-up lands.

## ADR-114: Telethon connect answers an honest 503 (SYN-02) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The Telethon MTProto client (connect code request/confirm) does not exist in the Rust binary.
Decision: Code request/confirm validate everything locally, then answer an honest 503 `telegram_unavailable`.
Consequences: Telegram account linking through Rust fails loudly instead of hanging or faking a code.

## ADR-115: Bot sends degrade honestly to `failed`; distribution gate fails open (SYN-03) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The python-telegram-bot runtime (sending, deleting, answering callback queries) does not exist in Rust; the Wave 7 admin control-centre distribution gate (`_telegram_allowed`) is not ported either.
Decision: Sends fail exactly the way Python fails them with no token (`failed` / `bot unavailable or send failed`), never with a fabricated message id. The distribution gate fails open, exactly like Python on a gate error.
Consequences: No Telegram message is ever claimed sent when it was not; a gate error does not block distribution in either stack.

## ADR-116: No ARQ enqueue from the Telegram routes (SYN-04) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `/distribute/publish`, `/distribute/retry` and `/import` need the ARQ worker, which stays Python (PLW-08).
Decision: Rows and audits are written exactly like Python, but nothing is ever enqueued (`enqueued: false`, `error: "worker unavailable — retry later"`) — the honest Redis-down path Python itself takes.
Consequences: A publish requested through Rust waits for the Python worker; no job is ever faked.

## ADR-117: Telegram webhook secret verified before anything else (SYN-05) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The webhook is a public URL; an unverified update must not reach any handler.
Decision: The secret is verified exactly like Python (`X-Telegram-Bot-Api-Secret-Token` against the `telegram.webhook_secret` setting, constant-time compare) before anything else happens. Non-`tg:` updates are ignored with `{"ok": true, "ignored": true}`; `tg:` callbacks run the full one-shot / expiry / HMAC checks and the fatwa-submission transition through the authorised path.
Consequences: Forged updates are rejected at the door in Rust exactly as in Python.

## ADR-118: Places `type` length is 422, not Python's 500 (PLW-01) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python declares `type` as `String(32)`; an over-long value passes Pydantic and fails at the DB, surfacing as a 500.
Decision: The Rust port validates `len ≤ 32` up front and returns 422 `validation_error`. The cap value (32) is copied verbatim from the Python model.
Consequences: A client error is no longer a 500. The observable difference is deliberate and strictly better.

## ADR-119: Admin places list applies the `status` filter after the limit (PLW-02) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's admin list fetches one page (`limit`, default 100) and filters by `status` in memory, so `total` is the filtered count of that page, not of the whole table.
Decision: Rust mirrors this exactly, including the consequence that a filtered page can be short or empty while more matching rows exist beyond the limit.
Consequences: Deliberate compatibility, not a bug; do not "fix" without a Python-side change.

## ADR-120: Places `LocalizedText` is lenient by default (PLW-03) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Missing `fa`/`ar`/`en` keys default to `""` (Pydantic defaults), present non-string/null values are 422, unknown keys are dropped.
Decision: The Rust port mirrors the Python places schemas exactly. This is stricter than Wave 3's `validate::req_localized` in one respect (non-string present values are rejected).
Consequences: Place names/descriptions validate identically on both stacks.

## ADR-121: Widget CORS wildcard is NOT ported; it belongs to the deploy layer (PLW-04) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `WidgetCorsMiddleware` sets `Access-Control-Allow-Origin: *` (no credentials) on `/api/v1/widgets/*` and answers OPTIONS with 204. This is middleware behaviour, not a route, and the Rust binary has no CORS middleware of its own.
Decision: The wildcard must be provided by the reverse proxy / deploy layer in front of the Rust binary. The embed-token gate itself is fully ported (gate = `X-API-Key` or `?embed_token=`, never `?api_key=`).
Consequences: Until the deploy layer provides it, cross-origin widget embeds will be blocked by browsers even though the endpoints answer.

## ADR-122: ayah-of-day cache is in-process only; no Redis tier (PLW-05) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python caches the ayah bundle in Redis (shared across workers, survives restarts).
Decision: Rust uses a process-local `Mutex<HashMap>` cache with the same TTL rules (success: seconds to UTC midnight clamped to [300, 21600]; negative: 60 s).
Consequences: In a multi-replica deploy each replica warms its own cache and a restart clears it; the endpoint stays correct, just colder. A shared cache tier is a deploy concern, not a route concern.

## ADR-123: ayah-of-day provider failure is an honest 502 (PLW-06) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The ayah text always comes from the Quran provider registry, never hardcoded.
Decision: Any failure of the provider registry while building the ayah bundle returns 502 `quran_provider_error`, mirroring Python (which catches generic `Exception` from the bundle function).
Consequences: No fallback edition and no hardcoded text are ever served.

## ADR-124: zakat-mini persists a calculation row; no active rule is an honest 503 (PLW-07) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `GET /zakat-mini` runs the real Wave 6 engine and persists a `zakat_calculations` row (user NULL) exactly like Python's `calculate_zakat`.
Decision: If no active zakat rule exists (with `asset_categories`), it returns 503 `zakat_rule_not_configured` — not the calculator's 422 — because the endpoint is unavailable by configuration, not by bad input. Test fixtures use a synthetic rule (rate 0.1); no fiqh content was invented (ADR-009).
Consequences: The widget is honest about misconfiguration; the persisted row matches Python's.

## ADR-125: ARQ queues and the dispatcher stay Python; Rust only writes outbox rows (PLW-08) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The six ARQ queues (critical, ai, media, telegram, notifications, analytics; Redis-backed, `app/worker.py` + `app/core/queues.py`) and the dispatcher (`app/core/dispatcher.py`: outbox poll, atomic claim ≤200, claim TTL 120 s, exponential backoff capped 300 s, dead-letter after 5 retries, Redis pub/sub bus) are a separate process concern with Redis semantics.
Decision: They are not ported. Rust writes `domain_events` outbox rows exactly as before and never claims or publishes them. Precedent: jobs stay Python sidecars; Rust only marks rows pending and answers `enqueued:false` honestly (ADR-022, ADR-045, ADR-053, ADR-061).
Consequences: Porting the loop into the Axum binary would change failure modes (claim TTL, redelivery) without a Python-side equivalent to test against.

## ADR-126: Realtime WebSocket `/ws/events` stays proxied (PLW-09) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The endpoint (`app/core/realtime.py`) authenticates at handshake (preferred one-time `?ticket=` from `POST /api/v1/auth/ws-ticket`, 60 s, atomic redeem, session-bound; fallback `Authorization: Bearer <JWT>` header only — `?token=` is rejected), rejects foreign Origins, enforces channel subscriptions (exact / `prefix.*` / `*`), requires `realtime.admin` for `admin.*`, delivers `user`-aggregate events only to the owner or `users.manage` holders, heartbeats with ping/pong (timeout → close 4400), and supports resume replay from the DB via a two-part cursor.
Decision: It is not registered natively in Axum. Honest note: the Rust binary's HTTP proxy (reqwest) cannot relay a WebSocket upgrade, so "stays proxied" currently means it works via the Python origin directly; a WS-aware proxy (or a native Axum WS implementation) is Wave 8 work, not Wave 7.
Consequences: Live realtime clients must reach the Python origin directly until Wave 8.

## ADR-127: `/metrics` is not ported; it stays proxied (PLW-10) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python exposes in-memory Prometheus exposition (`text/plain; version=0.0.4`): `http_requests_total{method,path,status}` (paths normalised, UUID/numeric segments → `{id}`), `http_request_duration_ms_avg`, `process_uptime_seconds`, `outbox_pending_events`, `outbox_oldest_pending_age_seconds`, `outbox_dead_letter_events`.
Decision: Not ported. It would require a metrics registry the Rust binary does not have, and its counters (notably the outbox gauges) are only meaningful alongside the Python dispatcher.
Consequences: Metrics stay on the Python origin.

## ADR-128: `/api/v1/health` stays proxied (PLW-11) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `/api/v1/health` returns `{status:"ok", env, api:"v1", media_tools: <media toolchain status>}`. Rust has only `/health` + `/ready` natively (Wave 1).
Decision: The endpoint stays on the Python origin. The `media_tools` field reports the Python media toolchain, which the Rust binary does not own.
Consequences: `/api/v1/health` answers from Python until the media toolchain has a Rust-side equivalent.

## ADR-129: `projects/due_cron` is portable in principle but out of scope; it stays proxied (PLW-12) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: A due-date notification cron (DB due-check + notification rows) could be written as pure Rust with no ARQ dependency.
Decision: It is explicitly not built in Wave 7 because the projects module itself is not in Wave 7 scope and stays proxied; building the cron alone would create a second writer to project notification state with no native reader. Revisit when projects is ported.
Consequences: Project due notifications keep coming from Python only.

## ADR-130: `key_from_row` reads INTEGER api-key columns as i32 (PLW-13) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `ApiKeyRow.rate_limit_per_min / quota_monthly / used_this_month` are `i64`, but the `api_keys` columns are `INTEGER` (int4). sqlx cannot decode Postgres int4 as i64, so `validate_key` (and every admin api-keys read path using `key_from_row`) returned 500 on Postgres while passing on sqlite. Root cause was never caught because Wave 2's PG smoke never exercised `validate_key` against Postgres; the widgets gate is the first caller that does.
Decision: Fixed in Wave 7 by reading the three columns as `i32` and widening to `i64` inside `key_from_row` (ADR-028: INTEGER columns read as i32).
Consequences: The widgets gate and the admin api-keys reads work on Postgres; no behaviour change on sqlite.

## ADR-131: No `;` inside `--` comments in TEST_SCHEMA (PLW-14) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `init_test_schema` applies the schema by splitting on `';'`. A semicolon inside a `--` comment therefore splits a statement in two and breaks schema setup for every test binary in the crate (this happened with the finance `BigInteger; …` comment).
Decision: TEST_SCHEMA comments must not contain `;`; prefer `–` or a line break. Longer term, the splitter should be comment-aware, but that is a harness change, not this wave's.
Consequences: The finance comment was fixed to `BigInteger - …`; any future `;` in a TEST_SCHEMA comment breaks all tests the same way.

## ADR-132: Calculator engines live at the crate root; Wave 7 clippy merge fixes (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: At the Wave 7 merge, `cargo clippy --all-targets -- -D warnings` failed with 51 errors although every worker had reported clean: 37 came from `calc_decimal.rs` / `calc_engines.rs` being loaded as a module twice — once inside `routes_calculators.rs` and once via `#[path]` inside `routes_widgets.rs` (clippy `duplicate_mod`; the second copy's items all read as dead code) — and 14 were style lints in the Wave 7 route and test files (`type_complexity`, `too_many_arguments`, `needless_late_init`, `while_let_loop`, `sort_by_key`, `redundant_guards`, `collapsible_match`, `manual_is_multiple_of`, `len_zero`, `doc_lazy_continuation`, needless borrows/closures).
Decision: `calc_decimal`, `calc_engines` and `calc_mosaref` are declared once at the crate root (`lib.rs`); `routes_calculators.rs` and `routes_widgets.rs` use them via `crate::calc_*` and load no copy of their own. The 14 style lints were fixed in place with behaviour-preserving rewrites (type aliases for complex types, a `ScanAudit` struct for the 8-argument receipt scanner, a `PlacementArgs` struct for the test helper, match guards for the collapsible `if`s). The `TEST_SCHEMA` dedup (one copy each of `social_posts` and `religious_places`) and the finance-comment `;` fix (ADR-131) were verified in the merged tree.
Consequences: `cargo clippy --all-targets -- -D warnings` is green again on the merged tree. Any future module that needs the calculator engines must `use crate::calc_*` — never `#[path]`-include the files a second time.

## ADR-133: Catch-all proxy removed; unknown paths are a native 404; ORIGIN_URL deleted; kill switches retired (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Every unported surface used to fall through to `router.fallback(proxy)` (reqwest forwarding to `ORIGIN_URL`, unreachable → 502 `bad_gateway`, ADR-013). With the Wave 8 lift every Python route is either native, an explicit sidecar contract (ADR-134), or dev-only, so no request ever needs to leave the Rust binary.
Decision:
- The proxy fallback is deleted (`proxy` and `bad_gateway` removed). The fallback is a native 404 returning exactly Python's envelope: `{"error": {"code": "http_error", "message": "Not Found", "details": {}}, "request_id": "<uuid>"}` — Python's own 404 is not FastAPI's default `{"detail": "Not Found"}`: `app/core/errors.py` routes `StarletteHTTPException` through the `http_error` handler, and a fresh `request_id` is generated per response, as Python does when no request-id middleware set it. A known path with the wrong method is a native 405 in the same envelope (`"Method Not Allowed"`, via `method_not_allowed_fallback`; axum's default is an empty 405 body).
- `Config.origin_url` (env `ORIGIN_URL`, default `http://127.0.0.1:8000`) is deleted: the field, its env read, and its `Default` are gone. `ORIGIN_URL` in the environment is now ignored. Every test file's `test_config()` that set `origin_url: "http://127.0.0.1:9"` had the line removed.
- Kill switches retire: `RUST_ANYTHING_UNIT_<NAME>=original` used to un-register a module's native routes so they proxied to Python. With no origin there is nothing to fall back to, so the env vars are still read in `build_router`, but if any module is set to `original`, one warning per module per process is logged (`kill switch RUST_ANYTHING_UNIT_<NAME>=original retired at lift; ignoring, routes stay native`) and the routes stay native. `unit_native()` now always returns true.
Consequences: No request ever leaves the Rust binary toward an origin again. Clients that relied on proxying unported paths now get 404s; the only unported Python surfaces are the 17 projects/staff routes (ported natively in the same wave, ADR-141) and dev-only `/docs` (ADR-139). Operators must remove the kill-switch vars from deploy configs; nothing breaks if they do not.

## ADR-134: Sidecar contract — 8 routes, explicit per-route forwarding (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Eight routes need a toolchain the Rust binary must not own (ADR-026 fatwa assistant, ADR-031 hifz ASR, ADR-046 voice STT, ADR-076 receipt PDF). The old answer was the catch-all proxy; the lift needs these reachable without reopening a catch-all.
Decision: One new config, `SIDECAR_URL` (env, no default; unset = sidecar unavailable). Exactly these 8 routes are registered natively with handlers that forward method/path/query/headers/body to `{SIDECAR_URL}<same path>` and return status/headers/body verbatim (the old proxy forwarding logic, renamed `forward_to_sidecar`, applied only here): `POST /api/v1/fatwa/ask`; `GET /api/v1/finance/receipts/{donation_id}`; `POST /api/v1/quran/hifz/attempts/{attempt_id}/grade`, `…/grade-word`, `POST /api/v1/quran/hifz/tests/{test_id}/questions/{question_id}/answer`, `POST /api/v1/quran/hifz/voice-search`; `POST /api/v1/voice/transcribe`, `POST /api/v1/voice/ask`. When `SIDECAR_URL` is unset or unreachable, each answers an honest 503 in the error envelope with a route-specific code: `fatwa_ask_unavailable`, `receipt_pdf_unavailable`, `hifz_asr_unavailable`, `voice_stt_unavailable`. Auth is enforced by the sidecar itself (headers forward verbatim, as the old proxy did); with no sidecar the 503 precedes any auth check. Forwarding is verified live against a mock sidecar (`sidecar_forwards_to_live_sidecar`: status, query, header and body survive verbatim).
Consequences: No other path forwards anywhere. A missing/unreachable sidecar degrades to honest 503s instead of 502s or faked successes.

## ADR-135: `/ws/events` is native in Axum (supersedes ADR-126) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `app/core/realtime.py`: handshake auth (preferred one-time `?ticket=` from `POST /api/v1/auth/ws-ticket`, 60 s, atomic redeem, session-bound; fallback `Authorization: Bearer <JWT>` header only — `?token=` rejected), foreign-Origin rejection, channel subscriptions (exact / `prefix.*` / `*`), `realtime.admin` required for `admin.*`, `user`-aggregate events only to the owner or `users.manage` holders, ping/pong heartbeat (timeout → close 4400), resume replay from the DB via the two-part cursor `(created_at, event_id)`, and `broadcast_event` fan-out fed by the dispatcher bus.
Decision: Implemented natively with axum's `WebSocketUpgrade` (axum `"ws"` feature; no new crate). Auth, Origin check, subscription rules and the privacy rule are verbatim ports (ticket redeem is the same conditional `UPDATE … WHERE used_at IS NULL` + session/user checks; JWT path enforces `scope == full` and a live, unrevoked, owner-matching session; `?token=` is never read). Rejections are delivered as a WebSocket close frame 4401 after accepting, exactly like Python (`websocket.close(code=4401)`), not as HTTP 403. Delivery is a poller spawned once per `AppState` in `build_router` (guarded so the test harness's repeated `build_router` calls never fan an event out twice): it starts its watermark at the newest outbox row, polls `domain_events` every `DISPATCHER_POLL_INTERVAL` (default 1.0 s, Python's default), and broadcasts new rows on a shared channel; each connection filters by its own subscriptions + privacy. Resume replay fetches the newest 1000 rows ordered by `(created_at, event_id)` and replays those after the reference id (unknown reference → nothing, like Python). Heartbeat uses `WS_HEARTBEAT_INTERVAL` (25 s) / `WS_HEARTBEAT_TIMEOUT` (60 s); a client that never pongs is closed with 4400.
Deviations, all deliberate and tested: (1) replay is bounded to the newest 1000 rows (Python replays unbounded); (2) ping is sent after `interval` of connection inactivity, so a flood of live events can delay a ping — but the pong-timeout check also runs at the top of every loop iteration, so a never-ponging client is still closed on time; (3) no periodic mid-stream session revalidation — Python's `session_still_valid` exists in `realtime.py` but is not called by its `websocket_endpoint` loop (only by `hifz_router`), so there is nothing to mirror. The WS tests use a real TCP server and a hand-rolled minimal WS client (no new test-only crate).
Consequences: Live realtime clients no longer need the Python origin. `emit_event` is the only outbox writer, so polling the outbox is the whole delivery path; the Python dispatcher bus is not involved. No stub was shipped — native parity was feasible and is implemented.

## ADR-136: `/metrics` is native (supersedes ADR-127) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python exposes in-memory Prometheus exposition (`text/plain; version=0.0.4`): `http_requests_total{method,path,status}` (paths normalised, UUID/numeric segments → `{id}`), `http_request_duration_ms_avg`, `process_uptime_seconds`, `outbox_pending_events`, `outbox_oldest_pending_age_seconds`, `outbox_dead_letter_events`.
Decision: Ported. A middleware layer on the whole router (fallback 404s included, like Python's `AccessLogMiddleware`) counts every request by method + normalised path + status and accumulates duration; `normalize_path` mirrors Python exactly (UUID regex, then all-digit segments → `{id}` — implemented segment-wise because the Rust `regex` crate has no look-around for Python's `/\d+(?=/|$)`). Outbox gauges are queried from `domain_events` per request, mirroring `dispatcher._refresh_outbox_metrics` (pending = `processed_at IS NULL AND failed_at IS NULL`, dead = `failed_at IS NOT NULL`, oldest age from the oldest pending row); no DB or any query error → all zeros, so `/metrics` never 500s. Float gauges use Python's `str(round(x, n))` formatting. The exposition text matches Python's line-for-line in shape.
Consequences: Metrics no longer need the Python origin. Counters are per-process (as in Python); the `/metrics` request itself is counted after its response is generated, exactly like Python.

## ADR-137: `/api/v1/health` is native with an honest `media_tools` report (supersedes ADR-128) (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `health_v1` returns `{status: "ok", env, api: "v1", media_tools: media_tools_status()}`, where the media report is `{binaries: {ffmpeg, ffprobe, tesseract, pdftoppm} (path or null), tesseract_langs (list or null), faster_whisper: bool, stt_engine: "faster-whisper", stt_model (default "small"), missing: [...], ready: bool}`.
Decision: Ported with the shape mirrored exactly and values reported honestly: the Rust binary owns no media toolchain, so every binary is `null`, `tesseract_langs` is `null`, `faster_whisper` is `false`, `missing` lists all five tools and `ready` is `false`. `stt_model` follows `FASTER_WHISPER_MODEL` (default `"small"`), mirroring Python's default — it names the configured model, it does not claim the model is present.
Consequences: `/api/v1/health` no longer needs the origin, and no consumer can mistake this binary for having ffmpeg/tesseract/faster-whisper.

## ADR-138: Executable replay of the wave2..wave7 HTTP cases (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `rust/app-cases/wave2..wave7/` hold 127 JSON HTTP cases written as documentation fixtures for a planned `app record` / `app replay` harness (ADR-007/ADR-011) that was never built.
Decision: Build the harness as `rust/app/tests/wave8.rs`. `replay_all_app_cases` walks the case files in order and replays each against the pure-Rust router (`build_router`, fresh in-memory sqlite `TEST_SCHEMA` per case); `wave8_pg_smoke` (gated on `WAVE2_PG_URL`) replays the lift-critical subset on Postgres (unknown-path 404, `/metrics`, `/api/v1/health`, `/openapi.json`, fatwa-ask sidecar 503, `GET /api/v1/projects` empty-list / staff-reader 403). Wave 8 cases (`proj-*`, `lift-*`) are excluded — covered by `wave8_projects.rs` / `wave8_lift.rs`.
Tiers: (a) runs as-is on a fresh DB; (b) runs once a user/role/row/setting or mechanical `<uuid>` substitution is supplied — the per-file `seed` hook does that (rows, a real API-key id, a registered user, a newer stored reading state, quran settings, telegram policy/secret); (c) documentation-only — the SKIP list, printed in the summary, never silent: wave2/08, wave2/09, wave2/10 (need tokens minted by a prior step), wave6/lib-05 (signature must be minted first), wave6/lib-06 (prose multipart body). Any other unrunnable case FAILS the test; the skip list is not extended without a reason.
Auth: seeded user + HS256 token with the `jwt::create_access_token` claims shape; permissions granted through a fresh non-privileged `editor` role (never `admin`/`super_admin`, so no TOTP enrolment). The case label is a hint, not the authority — the runner grants what each handler's `require_permission` actually checks.
Check semantics: `expected_status` must match exactly; then `expected` is subset-matched (every key must exist; objects recurse; numbers compare numerically; non-empty arrays compare element-wise up to the expected length; empty arrays compare exactly). Placeholders: `<uuid>` any valid UUID, `<cursor>` any non-empty string, `<password>`/`<jwt>`/`<opaque>`/`<cert-code>`/`<mk_...>` any non-empty string, `<N items>` an array of exactly N, `<string|null>` string or null; prose `"<...>"` means the key exists with a non-empty value of its own type. A `note` key is case-file annotation and is skipped; an `absent` prose value asserts the key must not exist. `expected_content_type`, when present, must prefix-match. Overrides: wave2/12 html must contain the base URL and `اوقات شرعی` and must not contain `mk_`; wave4/08 `last_read.surah` must be 2 after the stale PUT.
Consequences: First full replay 2026-11-22: 119 passed, 3 failed, 5 skipped. The 3 failures were case-file expectation drift verified against the original Python API (wave2/01 flat `places.globe.*` keys + phantom `push.vapid_public_key` — Python returns a nested `places.globe` object and no vapid key; lib-04 phantom `total` — Python's `search_items` returns `{query, hits}` only; syn-08 `{"items": …}` wrapper over a bare-array response) — Rust matched Python in each, so the three case files were corrected to the Python behaviour (not weakened) and the replay re-run: **122 passed, 0 failed, 5 skipped** (wave2 11/0/3, wave3 14/0/0, wave4 15/0/0, wave5 14/0/0, wave6 18/0/2, wave7 50/0/0); PG smoke green. Case files are updated only when the code is proven right against Python; the runner never weakens a check to pass.

## ADR-139: `/openapi.json` is generated from the router; `/docs` stays unserved (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python serves `/docs` only in dev (`docs_url="/docs" if settings.is_dev else None` in `main.py`), so in production `/docs` 404s. `openapi_url` is never overridden, so FastAPI's default `/openapi.json` is served in every environment.
Decision: `/openapi.json` serves a minimal spec (`openapi 3.0.3`, title `Maturidi Academy API`, version `0.2.0`, paths + methods) **generated at startup from the built router**, not hand-written: `format!("{:?}", router)` on axum 0.8 lists every route as `RouteId(N): MethodRouter(…)` plus `Node { paths: {RouteId(N): "/path"} }`, and the spec is parsed out of that (fallback excluded — it is registered after generation). The spec is cached process-wide, which is sound because the registered routes no longer depend on env or config. `/docs` is not served, so it 404s — matching production Python.
Consequences: The spec always lists exactly the registered routes (453 combos, asserted in tests). It depends on axum 0.8's `Router` `Debug` shape; if a future axum upgrade changes that shape, parsing yields an empty-paths spec instead of failing the boot, and the `openapi_json_lists_registered_routes` test will catch it. The spec is minimal (no schemas, no per-route metadata) — a deliberate scope limit.

## ADR-140: `/static` serves uploaded custom fonts from the local static dir (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python mounts `StaticFiles` on `/static` rooted at `apps/api/static` (`STATIC_DIR` in `admin/service.py`); admin font uploads land in `STATIC_DIR/fonts/<locale>/` and are returned as `/static/fonts/<locale>/<name>`. In Rust, `upload_font` writes the same key (`static/fonts/...`) via `put_object`, i.e. under `{storage_dir}/static/`.
Decision: `GET /static/{*path}` serves files from `{storage_dir}/static/` with an explicit extension→content-type map (woff2/woff/ttf/otf/eot + common web types, else `application/octet-stream`); a missing file or any `..`/`.`/empty segment is a 404 in Python's envelope. Implemented as a small handler, not `ServeDir` (no `tower-http "fs"` feature needed).
Consequences: Fonts uploaded through the Rust admin flow are served by the Rust binary from the same layout Python used. Only the local provider is served; with a non-local `STORAGE_PROVIDER` the files are not on local disk and reads 404.

## ADR-141: `projects` module ported natively in Wave 8 (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The `projects` module (Phase 16 PMS: projects, tasks, staff) was the last unported module family; ADR-129 had deferred it (and its due-cron) as out of scope for Wave 7. The Wave 8 lift ports it natively.
Decision: All 17 Python endpoints are ported in `routes_projects.rs` with identical routes/methods/statuses/response shapes/DB schema/auth: cursor pagination (`base64url("<ISO ts>|<uuid>")`, 400 `bad_cursor`), kanban `move` with `TASK_TRANSITIONS`, staff-reader gate, audit rows and domain events exactly as Python. DB tables `projects`, `tasks` (+ `ix_tasks_related_entity`), `staff_members` added to `TEST_SCHEMA`.
Consequences: `projects` no longer falls through to the proxy; the due-cron is portable now (ADR-142).

## ADR-142: Due-soon cycle exposed as `POST /api/v1/admin/projects/due-soon-cycle` (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python has no HTTP route for the due-soon cycle — it is an ARQ cron on the notifications queue, every 6h at minute 0 (`due_cron.py`). The native port `run_task_due_soon_cycle(db, now)` keeps the injectable-`now` shape for testability, but a pure function is not triggerable in the lifted (proxy-free) deployment.
Decision: Expose the cycle as `POST /api/v1/admin/projects/due-soon-cycle`, requiring `projects.manage`, returning `{scanned, notified}`. This is a deliberate Wave 8 addition, not a Python parity route; the 6h ARQ schedule is an operator concern, not part of this endpoint.
Consequences: 18 native routes for the module, not 17. The ARQ cron registration stays Python-side until the worker story is decided; nothing calls this endpoint automatically.

## ADR-143: Project notifications deliver through the stub mailer, honestly (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `_notify_task_event` calls real senders (`CHANNEL_SENDERS`) that return `(ok, err)`. The Rust mailer is still the Wave 2 stub (`mail.rs`, ADR-015): `send_email`/`send_sms` log and return `()`, so there is no delivery signal to honour.
Decision: Mirror the admin broadcast's established Rust behaviour: email counts as delivered when the user has an email address, SMS when they have a phone number; a missing address/phone is an honest non-delivery; push is never configured. `in_app` always writes the `notifications` row. `notified` in the due cycle counts only rows where at least one channel delivered under these rules.
Consequences: In tests (no `SMTP_HOST`/`SMS_API_KEY`), only `in_app` delivers — which is what the tests assert. When a real mailer lands, this helper must switch to its `(ok, err)` signal; the `delivered` semantics in `task.due_soon` events will then become real.

## ADR-144: Staff-reader gate skips the privileged-2FA enrolment check; writes use full `require_permission` (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `_staff_reader` runs on plain `get_current_user` — no 2FA-enrolment gate — while Rust's `require_permission` adds the full-scope + privileged-2FA check. The two gates cannot share one helper.
Decision: Reads use `authenticate` + a direct permission lookup (same SQL as `require_permission`) + the active-`staff_members` fallback, with no 2FA-enrolment check. Writes use the full `require_permission`, like every other module.
Consequences: A privileged user who has not enrolled TOTP can read projects (as in Python) but cannot write. If a later decision gates reads on 2FA too, only `staff_reader_ctx` changes.

## ADR-145: Due-soon cycle has no explicit transaction in Rust (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `run_task_due_soon_cycle` stamps every candidate and commits once at the end. The Rust `Db` has no transaction API; statements apply one by one, like every other module.
Decision: Port the cycle with per-statement application in candidate order (stamp, then notify + emit per assigned row). The observable end state is identical to Python's single commit.
Consequences: A mid-cycle crash leaves a partial stamp instead of rolling back; the next run re-scans only unstamped rows, so the cycle is still safe to re-run. A future transaction API should wrap this function first.

## ADR-146: Unused `project_title` argument dropped (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Python's `_notify_task_event` takes a `project_title` argument that no title or body ever uses (both kinds use only the task title).
Decision: The native `notify_task_event` takes no project title. Nothing is carried over, because there is nothing to carry.
Consequences: None observable. If a future notification template uses the project title, the caller must thread it through then.

## ADR-147: `routes_hifz.rs` cited the wrong ADR for the stream 503; the native count is 440, not 439 (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: `routes_hifz.rs:12` (and the same citation at :3861) credited the live-stream 503 rationale to ADR-032, but ADR-032 is "test-passage sampling uses SplitMix64"; the stream-503 decision is ADR-031 ("ASR stays proxied; the live stream answers an explicit 503").
Decision: Both citations corrected to ADR-031. Separately, the native route count was re-verified two independent ways (comment-aware source parse and the live router's `Debug` → OpenAPI spec): pre-lift native is **440**, not 439 — the 439 figure came from a naive parse that misses `GET /api/v1/videos/{item_id}/subtitles/{lang_vtt}` because a `//` comment sits between `.route(` and its path string; there are no duplicate (method, path) registrations. MAP keeps 440; both tests assert 453 total served combos.
Consequences: The two hifz comments now point at the right ADR. The 439 claim is recorded here as refuted, with the counting method, so it is not re-applied.

## ADR-148: Wave 9 PR-1 reconciliation corrections (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Wave 9 PR-1 re-read README.md, MAP.md, DECISIONS.md, PLAN-API.md, `scripts/cutover.sh`, `scripts/cutover-README.md`, `Dockerfile`, `railway.toml`, `lib.rs` and `routes_lift.rs` against the real Wave 8 tree and re-verified every count from the code (paren-depth parse of `lib.rs` `.route(` calls; alembic revision graph on the locked `rust-base` checkout; `TEST_SCHEMA` in `db.rs`; the original repo's `app/core/queues.py`, `app/worker.py`, `app/core/dispatcher.py` and `railway.worker-*.toml`). The old ADR texts below are NOT rewritten (this file is append-only); the corrections live here and in the fixed live docs.
Decision:
- Route counts: the Wave 5 total in MAP was **232 with hifz 52 — wrong; it is 233 with hifz 53** (per-module recount from `lib.rs`: Wave 2: 49, Wave 3: 81, Wave 4: 76 = quran 23 + hifz 53, Wave 5: 27, Wave 6: 69, Wave 7: 136, front door: 2 → 440 pre-lift; + 5 lift-new + 18 projects = 463 native; + 8 sidecar-contract = **471** served combos, matching the `front_door` test's OpenAPI assertion). Wave 6's 302 and the pre-Wave-7 304 were already right. MAP is fixed; the per-module breakdown is in MAP's Wave 9 section.
- ADR-139's consequence "(453 combos, asserted in tests)" and ADR-147's "both tests assert 453 total served combos" are **stale: the total is 471** (453 was the count before the 18 projects routes were wired in). The tests assert 471.
- `TEST_SCHEMA` holds **119 tables, not 116** (distinct `CREATE TABLE` names in `db.rs`). MAP §4 and the cutover script comment are fixed to 119.
- The cutover script's `CORE_TABLES` listed `fatwa_topics` — **no such table exists**. Fatwa topics live in `fiqh_topics` (`apps/api/app/models/models.py:715`; also in `TEST_SCHEMA`). The script now checks `fiqh_topics`.
- Supersession, made explicit: the "stays proxied" decisions in **ADR-026 (fatwa ask), ADR-031 (hifz ASR), ADR-046 (voice STT) and ADR-076 (receipt PDF) are superseded by ADR-134** (sidecar-contract: explicit forwarding to `SIDECAR_URL`, honest 503s). ADR-126/127/128 were already marked superseded by ADR-135/136/137. The old ADR texts stay as the historical record of what was decided when.
- `CORS_ALLOW_ORIGINS` **is read by the Rust binary** (`Config::from_env`, `lib.rs`; used by the `/ws/events` Origin check in `routes_lift.rs` `origin_allowed`). The cutover README §5.3 claim that it "is not read" was wrong and is fixed; what is still deploy-layer is only the widget `Access-Control-Allow-Origin: *` wildcard (ADR-121).
- Alembic head re-verified on the locked base: **one linear chain, 50 revisions, single head `f2a3b4c5d6e7`** (`f2a3b4c5d6e7_user_saved_fatwas.py`), root `bd898906c7e2`, no branch points — the cutover script's "head f2a3b4c5d6e7, 50 revisions" holds. (An early naive parse briefly suggested two heads; that was a regex artifact — 12-hex-only matching plus matching `down_revision` lines — and is refuted here so it is not re-applied.)
- The six ARQ queues and the dispatcher are documented from the original code in `scripts/cutover-README.md` §6 (PR-1): critical / ai / media / telegram / notifications / analytics, with their registered tasks and crons; only ai, media and telegram have registered task functions (critical, notifications and analytics have cron jobs only). The dispatcher is in-process inside the Python API lifespan, not a separate service; the Rust WS poller is read-only against `domain_events` and does not mark rows processed.
Consequences: Live docs (README, MAP, cutover script + runbook, code comments, wave case READMEs) now match Wave 8. DECISIONS.md history is untouched; this ADR is the pointer for every correction above.

## ADR-149: CI services — pgvector Postgres + Redis, sequential PG smokes (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: Before PR-2, CI ran one job (`rust`) with no services: `cargo test` ran SQLite-only because the PG smokes skip without `WAVE2_PG_URL`, and no test ever touched a real Redis — so the "Postgres + Redis green" claim was never actually verified anywhere except a developer's local machine. PR-2 must close that gap without weakening any test.
Decision:
- Postgres service: `pgvector/pgvector:pg16` — the same image the original `docker-compose.yml` uses. Verified from the upstream Dockerfile (pgvector/pgvector, master): the image is `FROM postgres:$PG_MAJOR-$DEBIAN_CODENAME` plus the vector extension build, so it is stock postgres:16 with `vector` added; `pg_trgm` ships in the postgres base image (contrib). The migration e2b3c4d5f6a7's `CREATE EXTENSION vector` + `pg_trgm` therefore works on the service, and the new `wave9_pr2_pg_smoke` proves it (create + a `vector(3)` cosine-distance round-trip + a trigram similarity check) instead of trusting the image tag.
- Redis service: `redis:7-alpine`, the same image the original compose uses. Wired as `REDIS_URL=redis://127.0.0.1:6379/0` at job level, so the wave9_pr2 Redis tests (PING, SET/GET, `RateLimiter` against real Redis, /ready `redis: "ok"`) run in the plain `cargo test` step. The tests use per-process unique keys, so they are safe under parallel test binaries.
- Postgres is deliberately NOT wired at job level. The PG smokes share one database and each drops/re-creates the schema, so they must run one binary at a time; `cargo test` runs binaries in parallel. CI therefore runs a separate sequential step that sets `WAVE2_PG_URL` and runs, per binary, `cargo test --test <bin> -- --test-threads=1 pg_smoke` (exactly one smoke test per binary matches that filter). This mirrors the local verification procedure (sequential per binary) instead of inventing a second one.
- `cargo build --release --locked` is a step in the `rust` job: the same command the Dockerfile runs, so a release-only breakage (e.g. a feature-gated dependency) fails CI before the Docker job even starts.
- `RateLimiter` is re-exported (`pub use ratelimit::RateLimiter` in `lib.rs`) so integration tests can exercise its Redis path directly. It is the only component besides /ready that talks to Redis, and its fail-closed/fallback behaviour is otherwise unobservable from tests.
Consequences: CI now runs the PG smokes and Redis tests for real on every push/PR. Local runs are unchanged: without `REDIS_URL`/`WAVE2_PG_URL` the new tests skip with a clear message (never a fake pass), and the sequential loop in CI is the same loop the local procedure uses. The two services add ~1 minute of container startup to the `rust` job.

## ADR-150: Docker build, cargo audit and SBOM in CI (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The repo `Dockerfile` (multi-stage rust:1.100-slim-bookworm → debian:bookworm-slim, `cargo build --release -p maturidi-api-rs --locked`) and `railway.toml` existed since Wave 8 but had never been built anywhere, and there was no vulnerability scan or SBOM. This VM has no Docker daemon, so CI is the first verification of the Docker build — that boundary is stated in the workflow, in MAP.md and here, not hidden.
Decision:
- New `docker` job: `docker/setup-buildx-action@v3` + `docker/build-push-action@v6` with `context: .`, `push: false`, `load: true`, tag `maturidi-api-rs:ci`. It builds exactly what the repo Dockerfile builds — no second Dockerfile, no push, no registry credentials involved.
- SBOM: `syft` (via `anchore/sbom-action/download-syft@v0`) scans the built image and writes SPDX JSON (`sbom.spdx.json`), uploaded with `actions/upload-artifact@v4` as artifact `sbom`. syft was chosen over cargo-sbom because it covers the whole image (Debian OS packages + the Rust binary), while cargo-sbom only covers the Cargo dependency tree; the Cargo tree is already fully described by the committed `Cargo.lock`.
- `cargo audit` runs in the `rust` job (`cargo install cargo-audit --locked`, then `cargo audit`) with default behaviour: any RUSTSEC advisory affecting this tree fails the run. No `--ignore` flags. An exception requires a new ADR naming the RUSTSEC id, the affected dependency, and why the advisory does not apply (or the planned fix date). None are granted now; the PR-2 run itself is the first proof the tree is clean.
Consequences: A Dockerfile breakage, a vulnerable dependency, or a missing SBOM all fail CI and block the merge. The SBOM artifact is kept per run (default 90-day retention) so a release can point at the exact image contents it shipped.

## ADR-151: Alembic check against the locked base in CI (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: The Rust repo holds no Alembic files, yet the production schema is defined by the original repo's Alembic chain (locked base 5d1cb02, single head f2a3b4c5d6e7, 50 revisions). PR-2 must prove in CI that the locked chain actually migrates a real Postgres and that the Rust binary serves /ready against the migrated schema — without ever touching the live database and without pretending a SQLite replay is the same check.
Decision:
- New `alembic` job with a `pgvector/pgvector:pg16` service (`POSTGRES_DB: alembic_check`). It checks out the original repo at the locked SHA into `original/` (read-only) and runs `scripts/alembic-check.sh`, which is the whole mechanism — CI adds no hidden steps, so the check is reproducible locally with one command.
- The script asserts, in order, with `set -euo pipefail` and no swallowed failures: (1) the checkout's HEAD equals `LOCKED_BASE_SHA` exactly; (2) a venv with the minimal pinned set from the original `apps/api/requirements.txt` (`alembic==1.20.0`, `sqlalchemy[asyncio]==2.1.5`, `asyncpg==0.32.0`, `pydantic==2.14.1`, `pydantic-settings==2.16.1`, `pgvector==0.5.1` — the only packages `alembic/env.py`'s import chain needs: `app.models` imports `pgvector.sqlalchemy`, `app.core.config` needs `pydantic-settings`); (3) `alembic heads` prints exactly one line, `f2a3b4c5d6e7 (head)`; (4) `alembic upgrade head` succeeds; (5) the migrated schema verifies: `alembic_version = f2a3b4c5d6e7`, extensions `vector` + `pg_trgm` present, and exactly 120 public tables (119 app tables — the same 119 as the Rust `TEST_SCHEMA`, ADR-148 — plus `alembic_version`); (6) the Rust release binary, built with `cargo build --release -p maturidi-api-rs --locked`, serves /ready 200 with `db: "ok"` against the migrated schema, with `redis: "unavailable"` (no Redis in this job, proving Redis never fails readiness).
- The full chain was verified locally before being written into CI (venv + `alembic upgrade head` against local Postgres with the pgvector package: single head, upgrade clean, version row, both extensions, 120 tables).
Consequences: Any drift in the locked base (a moved SHA, a second head, a migration that needs something the service image lacks) fails CI loudly instead of being discovered at staging deploy time. The check stays honest about its limits: it proves the migrations run and the binary reads the migrated schema — it does not prove the Rust `TEST_SCHEMA` equals the Alembic schema table-for-table (that comparison is a PR-4 staging concern, not claimed here).

## ADR-152: First `cargo audit` — two vulnerabilities, one fixed by nothing in scope, two named exceptions (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: PR-2 ran `cargo audit` (cargo-audit 0.23.0, 1561 advisories, 298 crate dependencies) on the Wave 8 tree for the first time. It found two vulnerabilities and one warning. This ADR records what was found, what was tried, and the exceptions — it amends ADR-150's "None are granted now" line; ADR-150 itself is not rewritten.
Decision:
- **RUSTSEC-2023-0071 (rsa 0.9.10, Marvin Attack, medium 5.9; fix: >= 0.10.0).** `rsa` reaches the tree only through `sqlx-mysql` (sqlx's MySQL driver). Fix attempts, all done and all failing, in order: (1) `cargo update -p sqlx` — 0.8.6 is already the latest 0.8.x, nothing changes; (2) `cargo update -p libsqlite3-sys --precise` is not applicable here; (3) dropping the `any` feature from the sqlx declaration (the code never uses `AnyPool` — db.rs says so) plus `default-features = false` — the fresh lockfile was byte-identical, because sqlx-macros itself re-enables `any`; (4) also dropping `macros` — same result, fresh lockfile identical again; (5) a minimal probe crate with the same sqlx features reproduces the lockfile entries, and bisecting its features shows the base set keeps `sqlx-macros` while adding any one of `chrono`, `uuid`, `json`, `migrate` — all load-bearing for this codebase (DateTime/Uuid/JSON columns, migrations) — pulls in `sqlx-mysql` → `rsa`. Conclusion: with sqlx 0.8.6, `rsa` 0.9.10 is unavoidable for any feature set this code needs. The `Cargo.toml`/`Cargo.lock` changes from attempts (3)–(4) were reverted — they changed nothing and must not ship as a pretend fix.
- **RUSTSEC-2026-0459 (libsqlite3-sys 0.30.1, C SQLite CVE-2026-119375, medium 6.8; fix: >= 0.39.0).** `cargo update -p libsqlite3-sys --precise 0.39.0` fails: sqlx-sqlite 0.8.6 requires `libsqlite3-sys ^0.30`, and 0.8.6 is the latest 0.8.x. Same conclusion as above: no patch upgrade exists inside sqlx 0.8. Exposure is limited — the bundled SQLite is only exercised by tests and the `sqlite://` probe path; production uses Postgres and all app SQL is parameterized — but the vulnerable C library is still compiled into the shipped binary, so this is a real exception, not a non-issue.
- **RUSTSEC-2026-0543 (chrono 0.4.45, "unmaintained") is a warning, not a vulnerability.** `cargo audit` exits 0 on warnings alone ("1 allowed warning found") and no fixed upgrade exists; the ecosystem-wide fix (migrating off chrono) is out of scope. Recorded here, no `--ignore` needed, no exception granted.
- **Exceptions granted (exactly two):** CI runs `cargo audit --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2026-0459`. Any other advisory fails the run, as ADR-150 requires. These two ignores are time-boxed, not permanent: evaluating a sqlx 0.8 → 0.9 upgrade (0.9.1 exists on crates.io; whether it moves libsqlite3-sys >= 0.39 and rsa >= 0.10 must be checked there, not assumed) is a PR-3 open item. If that upgrade lands, the ignores are removed in the same PR; if it is rejected, a new ADR must re-justify keeping them.
Consequences: The audit gate is green with two named, justified exceptions instead of red-with-no-plan or green-by-pretence. The Cargo.toml sqlx line and Cargo.lock are unchanged from Wave 8 (the trimming attempts are documented here as refuted, so they are not re-applied).

## ADR-153: REDIS_URL must not be set at CI job level — the shared auth rate-limit counter 429s the suite (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: ADR-149 wired `REDIS_URL` at the `rust` job level so the wave9_pr2 Redis tests would run in the plain `cargo test` step. That design was wrong, and local verification proved it: with `REDIS_URL` pointing at a real Redis, the full suite failed across the board — every auth-dependent test (register, login, 2FA, API keys, users, activity) returned 429 `rate_limited` instead of its expected status. The auth endpoints rate-limit through the Redis-backed limiter keyed by client IP, and every test uses the same test IP (`127.0.0.1:43111`), so all binaries shared one Redis counter. With a dead Redis (the pre-PR-2 state) the limiter falls back to a per-AppState in-memory window, which is why the suite was green before.
Decision:
- `REDIS_URL` is set only for a dedicated step, `cargo test --test wave9_pr2`, which runs the wave9_pr2 binary (all six tests: PING, SET/GET, `RateLimiter`, /ready live/dead) against the real Redis service. Those tests use per-process unique keys and their own limiter instances, so they are safe in isolation.
- The plain `cargo test` step runs with no `REDIS_URL` (the binary's default points at a dead server), exactly as before PR-2 — no behaviour change for the 17 pre-existing binaries.
- This corrects ADR-149's "Wired as REDIS_URL at job level" paragraph; ADR-149 itself is not rewritten. The workflow comment carries the same warning so the mistake is not re-introduced by a later edit.
Consequences: CI runs the Redis tests for real without breaking the rest of the suite. Locally, the same split applies: run the suite without `REDIS_URL`, then `REDIS_URL=redis://127.0.0.1:6379/0 cargo test --test wave9_pr2` for the Redis coverage. Any future test that needs a live Redis must follow this pattern (dedicated step, unique keys) and never set the variable at job level.

## ADR-154: Redis tests need their own CI job — a Redis service in the `rust` job breaks the whole suite through the default URL (decided by Dash per ADR-010, 2026-11-22)
Status: accepted
Context: ADR-153 moved `REDIS_URL` from job level to a dedicated `cargo test --test wave9_pr2` step inside the `rust` job, after proving that a live Redis makes the auth tests 429 (shared rate-limit counter, same test IP). That fix was necessary but not sufficient: the second full-suite run still failed the same way with no `REDIS_URL` set at all. Root cause, found by inspection: most test configs use `Config::default()`, whose `redis_url` is hardcoded to `redis://localhost:6379/0` (lib.rs:188); only front_door, wave8_lift and wave9_pr1 override it to the dead port 6399. So any live Redis on localhost:6379 — the service, or a locally running server (my own `redis-server` on 6379, started during PR-2 verification) — is reached by every test binary through the default URL, whether or not `REDIS_URL` is set. The Postgres service does not have this problem: the default `database_url` is `None`, and nothing connects to localhost:5432 unless `WAVE2_PG_URL` is set (only in the sequential smoke step).
Decision:
- The `redis` service and the wave9_pr2 run move into a job of their own (`redis`), which runs only `cargo test --test wave9_pr2` with `REDIS_URL=redis://127.0.0.1:6379/0`. Its PG smoke skips there (no `WAVE2_PG_URL`) and runs in the `rust` job's sequential step instead.
- The `rust` job has no Redis service and never sets `REDIS_URL`. Its plain `cargo test` step then behaves exactly as before PR-2 (dead default Redis → in-memory limiter fallback).
- This corrects ADR-153's placement (the dedicated-step idea is kept, moved to its own job); ADR-153's diagnosis of the 429 mechanism stays correct and is not rewritten. The alternative — changing `Config::default()`'s redis_url to the dead port — was rejected: the default is shared with production-adjacent code paths and the test-only override already exists in three test files; isolating the service in CI is the smaller, safer change. A future cleanup could align all test configs on the dead-port override, but it is not needed for this gate.
Consequences: CI runs the Redis tests for real without breaking the rest of the suite. Locally the same split applies: stop any Redis on 6379 before the plain suite, then run `REDIS_URL=redis://127.0.0.1:6379/0 cargo test --test wave9_pr2` with the server up. The four merge gates are now `rust`, `redis`, `docker`, `alembic` (branch protection must require all four — manual follow-up, see MAP.md).

## ADR-155: Locked-base identity is proven by tree hash, not commit SHA (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: PR-2's `alembic` job checked out the original repo at the locked-base merge commit `5d1cb02` by SHA, and CI failed instantly at that step: the commit does not exist on GitHub. It was created locally by merging PRs #193/#194 into `rust-base` and never pushed — pushing anything to the original repo is forbidden by the Wave 9 boundary. The two PR heads (`658fcfc`, `e22b71d`) do exist on GitHub, and neither PR touches `apps/api/alembic`, so the migration chain on main `9154d17` is identical to the locked base's chain — but the check must still prove it is testing the locked base, not a moving target.
Decision:
- CI checks out the original repo at main `9154d17` (read-only, full history), fetches `refs/pull/193/head` and `refs/pull/194/head`, and merges both with `--no-ff` — the same operations that produced the locked base locally. Nothing is pushed anywhere.
- `scripts/alembic-check.sh` proves the checkout is the locked base by TREE hash (`LOCKED_TREE=73422149c202d8b4e0a4a1c0bcc8dba3f79239f0`), not commit SHA: a merge of the same parents is content-identical, so its tree hash must match. The script's old `LOCKED_BASE_SHA` assertion is replaced, not kept alongside.
- This corrects ADR-151's checkout mechanism; ADR-151's assertions (single head `f2a3b4c5d6e7`, 50 revisions, 120 tables, `/ready` against the migrated schema) are unchanged. Verified locally: the script passes against the `rust-base` checkout (same tree) and the CI merge reproduces that tree.
Consequences: the `alembic` gate is reproducible in CI without ever writing to the original repo. If either PR head moves on GitHub, the merge reproduces a different tree and the gate fails honestly instead of silently testing the wrong base.

## ADR-156: Two CI workflow bugs found by the gates themselves (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: PR-2's first CI runs failed before any test ran, twice, for workflow-file reasons — not code reasons. (1) The `rust` job had an `env:` key whose body was only comments, which YAML parses as `env: null`; GitHub Actions rejects a null `env` mapping, so the whole workflow failed instantly with zero jobs. (2) Both Postgres services declared `POSTGRES_PASSWORD: <redacted>` — a literal placeholder, not a password — while every client URL used password `postgres`, so the `alembic` job (and the `rust` job's PG smokes) failed with `password authentication failed for user "postgres"`.
Decision:
- Never leave an `env:` (or any mapping) key with only comments in a workflow file: either give it real entries or delete the key and keep the explanation as a plain comment. Validate with `python3 -c "yaml.safe_load(...)"` and check that no job's `env` is `None`.
- Never write a placeholder as a credential value: CI test databases use the same throwaway credential as the local test setup (`postgres` / `postgres`), stated plainly in the workflow. The services are ephemeral and hold no real data.
- Both bugs were found by CI itself (the gates doing their job) and fixed in the PR-2 branch before merge; neither was papered over locally.
Consequences: the four jobs now actually run. Any future workflow edit gets a YAML-parse check plus a grep for placeholder values before pushing.
## ADR-157: sqlx 0.8 → 0.9 — both audit vulnerabilities fixed, ignores removed (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: ADR-152 granted two named `cargo audit` exceptions because no patch upgrade existed inside sqlx 0.8: RUSTSEC-2023-0071 (`rsa` 0.9.10 via sqlx-mysql) and RUSTSEC-2026-0459 (`libsqlite3-sys` 0.30.1, C SQLite CVE-2026-119375, fix >= 0.39.0). The ADR-152 follow-up — evaluate sqlx 0.9 — is this PR's item 8, and its condition was explicit: land the upgrade only if it compiles, the full suite passes, and `cargo audit` is clean; then remove both `--ignore` flags.
Decision:
- `sqlx` 0.8.6 → **0.9.1** (`rust/app/Cargo.toml`). The 0.9 feature split renames the old `runtime-tokio-rustls` into `runtime-tokio` + `tls-rustls-ring`; the declaration now reads `["any", "postgres", "sqlite", "runtime-tokio", "tls-rustls-ring", "chrono", "uuid", "json"]`.
- `libsqlite3-sys` 0.30.1 → **0.40.0** (`cargo update -p libsqlite3-sys --precise 0.40.0`). sqlx-sqlite 0.9.1 allows `>=0.30.1, <0.41.0`, so the bump resolves inside sqlx 0.9 where 0.8.6's `^0.30` forbade it. (The crates.io index line that suggested sqlx 0.9 "moves libsqlite3-sys to ^0.37" was misleading: the resolved tree still had 0.30.1 until the explicit bump — the index is not the resolver.)
- `rsa` is **gone from the tree**: sqlx 0.9 makes MySQL RSA auth opt-in (`mysql-rsa` feature, not enabled here), so RUSTSEC-2023-0071 has no affected crate left to flag.
- `cargo audit` (cargo-audit 0.23.0) now exits **0 with no ignores**: the only remaining finding is the chrono RUSTSEC-2026-0543 warning (unmaintained, no fixed upgrade), which exits 0 on its own and needs no ignore — as ADR-152 already recorded. **Both `--ignore` flags are removed from `.github/workflows/ci.yml`** (plain `cargo audit`); ADR-150's "no ignore without a new ADR" rule is satisfied by this ADR, and ADR-152's two exceptions are closed, not carried.
- The upgrade's one breaking change is sqlx 0.9's `SqlSafeStr` gate: `query_with` no longer accepts a borrowed non-`'static` `&str` or a `&String`. Four call sites in `db.rs` were affected. The fix is a single audited choke point: `to_pg_placeholders` now returns `sqlx::AssertSqlSafe<String>` (it only renumbers `?` → `$N` and never interpolates a value), and the SQLite branches wrap the borrowed SQL in a new `safe_sql()` helper carrying the audit note. The audit itself was done, not assumed: every SQL string reaching `Db::fetch_*`/`execute` was grepped — all are hardcoded literals or built from code constants (`?`-placeholder counts, enum-derived table names, hardcoded `SET` column lists) with all values as bound arguments; no caller interpolates user input into SQL. Call sites audited: `library_visibility.rs` (IN-placeholders), `routes_admin.rs` (cohort IN-placeholders; `count_by_user` table/column from four hardcoded call sites), `routes_admin_content.rs` (`Adapter::table()` enum strings; hardcoded `SET` lists), `auth_service.rs` (`fetch_token_row` table from the literal `"password_reset_tokens"` only). Any future dynamically-built SQL must keep that invariant — the comment on `safe_sql` says so.
Consequences: the audit gate is green with zero exceptions for the first time since it was introduced. The full suite (339 tests, 18 binaries, sequential, PG smokes) passes on sqlx 0.9 — the upgrade is behaviour-safe by that evidence, not by the changelog. Future sqlx upgrades must re-run the same bar (compile + full suite + clean audit) and must not reintroduce an ignore without a new ADR.

## ADR-158: Real email + SMS — Resend / SMTP / Kavenegar, honest failure, no stub left (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: Since Wave 2 (ADR-015) `mail.rs` was a stub: `send_email`/`send_sms` logged and returned success without sending anything. Every caller treated that as delivery — worst in the admin broadcast path, which recorded `(true, None)` (delivered) and counted `total_sent` for messages never sent, and in the SMS connection-test endpoint, which answered `{"ok": true}` unconditionally. Python's own dev fallbacks (`LogSmsProvider`, the email dev-mode log-and-return-True) are the same fake-success shape. PR-3 replaces the stub with real sending; the task's rule is absolute: when unset/unreachable, fail honestly — a real error, never a fake success.
Decision:
- Transports, same order and semantics as Python (`app/core/email.py`, `app/core/sms.py`): **email** — Resend HTTPS API (`POST https://api.resend.com/emails`, `Authorization: Bearer <key>`, payload `from`/`to: [to]`/`subject`/`text`[/`html`]) when `RESEND_API_KEY` is set (preferred: Railway blocks SMTP ports); else SMTP (STARTTLS, 15 s, login when `SMTP_USER` is set) via the `lettre` crate when `SMTP_HOST` is set; else `Err(MailError::NotConfigured)`. **SMS** — Kavenegar (`POST https://api.kavenegar.com/v1/{api_key}/sms/send.json`, form `receptor`/`sender`/`message`, 15 s) when `SMS_API_KEY` is set; HTTP 200 with a non-200 `return.status` in the JSON body is a delivery failure (Python's fake-success guard, ported); else `Err(NotConfigured)`. No dev-mode log-and-pretend: the deliberate divergence from Python's dev fallback is recorded here — a dev deploy without credentials now logs an honest error instead of claiming delivery, because a fake success in the delivery path is exactly what this PR removes.
- `send_email`/`send_sms` now return `Result<(), MailError>` and take the `Config` (credentials come only from env: `RESEND_API_KEY`, `SMTP_HOST`/`SMTP_PORT`/`SMTP_USER`/`SMTP_PASSWORD`/`SMTP_FROM`, `SMS_API_KEY`/`SMS_SENDER`; `SMS_PROVIDER` selects the provider, only `kavenegar` is implemented — anything else is `NotConfigured` with a clear message, not a silent log). Errors never carry a key or a message body: a Resend failure reports only the HTTP status (`resend delivery failed: http=N`), a Kavenegar failure only the HTTP status and the API status code. Bodies are redacted before any log line (`redact_secrets`, ported from Python: `token=` values and 6-digit codes masked).
- Call-site behaviour, Python parity: auth flows (verification email, password reset, phone codes) stay best-effort — the token/code is already stored, a delivery failure is logged and the flow continues, the user can ask for a resend. The admin broadcast path now records the **real** per-channel outcome (`sent` vs `failed` + error, `skipped` with `{ch}_not_configured` when the channel is unconfigured) instead of marking every attempted send delivered. The projects task-notify path only counts a channel delivered on `Ok`. The SMS connection-test endpoint returns `{"ok": false, "error": ...}` on a real failure instead of unconditional `ok: true`. `channel_configured("email")` is fixed: it checked only `SMTP_HOST`, so a Resend-only deploy reported email as unconfigured while Python would send fine — it is now `RESEND_API_KEY` set OR `SMTP_HOST` set.
- Real bodies, not empty strings: the verification and reset call sites passed `("", "")` for text/html. They now render the real email from `mail_templates.rs` (ported from `app/core/mail_templates.py`): fa/ar/en copy tables for both messages, locale by `resolve_email_locale` (phone country IR → fa, Arab countries → ar, else the stored locale when fa/ar/en, else en; the dial-code table is the minimal IR + 22 Arab prefixes — every other number maps to en, the same outcome as Python's full table), action URL `/{lang}/verify-email?token=…` / `/{lang}/reset-password?token=…` built from `app_base_url` with Python's `public_url` validation (http/https, hostname present, no userinfo — an invalid base logs and skips the send), logo `public_url("/logo.jpg")`, name greeting. The branded HTML shell is ported with the same structure and brand colours.
- Verification boundary (what tests prove vs staging): the mock-transport tests (`wave9_pr3.rs`, in-process mock Resend/Kavenegar servers) prove request shape, header/payload correctness, the non-2xx and bad-`return.status` error paths, and honest failure when unset — with **no real network call**. No live Resend or Kavenegar key exists in this environment, so a real delivery is **not** verified here; staging (PR-4) must send one real verification email and one real SMS from the deployed config and confirm receipt. That item is on the Release Blockers list until it happens.
Consequences: mail/SMS is no longer the cutover gap it was since Wave 2. Any deploy without credentials now fails loudly at the point of sending (and, for the admin paths, in the recorded delivery status) instead of silently dropping messages the UI claims were sent.

## ADR-159: Sidecar contract — per-route timeouts, body caps, streaming, no retries (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: Wave 8's forwarder (ADR-134) forwarded method/path/query/headers/body verbatim with per-route 503 codes, but three gaps were found in the PR-3 survey: (1) no per-route timeout — only the shared reqwest client timeout of 30 s, too short for an LLM answer or a 15-minute ASR transcription and the same for a PDF as for audio; (2) no body cap — `to_bytes(body, usize::MAX)` is unbounded, and the axum router's 2 MB default body limit was silently capping the audio routes below their 32 MB contract (see ADR-161); (3) `upstream.bytes().await` buffered the whole upstream response, so `POST /api/v1/fatwa/ask?stream=true` could not stream a single SSE event until generation finished.
Decision:
- Per-route timeouts, bounding the whole upstream call including streaming: fatwa ask 300 s, the four hifz ASR routes 300 s, voice transcribe/ask 180 s, receipt PDF 60 s. The figures are generous on purpose (LLM generation and faster-whisper on up to ~15 min of audio are slow); they bound a hung sidecar, they do not rush a slow one. `SIDECAR_TIMEOUT_SECS` (env, 0 = per-route defaults) overrides all of them at once — an operations knob and the knob the tests use to prove the timeout path without waiting minutes. A timeout is a 503 with the route's code.
- Per-route body caps, enforced by the forwarder before any byte goes upstream: fatwa ask 1 MB (a 4000-char question is ~16 KB; 60× headroom) → 413 `request_too_large`; hifz/voice audio 32 MB (Python's `MAX_AUDIO_BYTES`) → 413 `hifz_audio_too_large` / 422 `audio_too_large` (the Python-equivalent codes, not a generic 413). The voice cap is the `voice.max_audio_mb` setting in Python; the forwarder cannot read that setting, so it enforces the same 32 MB ceiling and lets the sidecar apply the tighter configured cap itself.
- Streaming upstream bodies (`Body::from_stream` over the reqwest byte stream): no code path buffers a whole upstream body any more. The SSE test proves the first event arrives while the mock sidecar is still holding the stream open.
- No retries, now a stated rule rather than an accident: none of these POSTs is idempotent (a grade consumes an attempt, transcribe/ask consume AI quota, a fatwa ask starts an LLM run), so a retry could double-charge or double-record. One attempt; on failure the honest 503.
- The full contract — request/response shapes, timeouts, caps, error mapping, auth (enforced by the sidecar; headers forwarded verbatim), and what the Python sidecar must serve — is documented in `rust/app-cases/wave9/sidecar-contract.md`. The mock-sidecar tests in `wave9_pr3.rs` prove forward-on-success for all 8 routes, honest 503 when unset/unreachable/timeout, the body caps, and SSE streaming. The real sidecar pairing is a PR-4 staging proof (Release Blockers).
Consequences: the forwarder is now a contract with stated limits instead of an unbounded pipe. If staging shows real p99 latencies above these timeouts, the numbers move in the contract and this ADR, not silently in code.

## ADR-160: Python workers — what PR-3 verified locally, and the staging boundary (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: The Rust binary never runs a worker and never enqueues an ARQ job (ADR-125). The workers live in the original repo (`python -m app.worker <queue>`, six ARQ queues, `railway.worker-*.toml` prepared but not deployed). PR-3 had to verify queue processing "as far as locally possible" without claiming what was not run.
Decision:
- Verified locally, with a real Redis 7.0.15 (local `redis-server`, port 6380 — deliberately not 6379, so the running Rust suite's rate limiter was untouched) and real ARQ 0.28.0 (the exact pinned version): a worker with the REAL media-queue settings (`queue_name="media"`, `max_jobs=4`, `max_tries=3`, `job_timeout=1800`) and a job function named `transcribe_media` was started with `python -m arq`; a separate process enqueued with the REAL call shape (`create_pool(RedisSettings.from_dsn(...))` + `enqueue_job(..., _queue_name="media")`, the same call `library/service.py:379` makes). The worker picked the job up in 0.20 s, ran it, and the enqueuer received the result. The job body was a stand-in (it recorded the call and returned the payload); the proof is that ARQ + Redis deliver a job to a worker and return its result end-to-end. The first attempt, without `_queue_name`, timed out — the job landed on arq's default queue and the media worker never saw it — which itself proves the queue routing is real.
- The real `app.core.queues` module loads (pydantic + pydantic-settings only) and `worker_settings_for()` returns exactly the six queues and settings PR-1 documented.
- The real media startup gate runs and fails loud on this VM: `media_tools_status()` reports ffmpeg/ffprobe/pdftoppm present, **tesseract and faster-whisper missing**, `ready: False`; `assert_media_tools_ready()` raises `RuntimeError: media tools missing: tesseract, faster-whisper`. So the real media worker cannot start here, and this PR does not pretend otherwise.
- NOT verified, and owned by PR-4 staging: the real `python -m app.worker <queue>` processes (importing `app.worker` here stops at the full app dependency tree — fastapi and the whole `requirements.txt`, including faster-whisper/torch — only the deployed image has all of it), the real job functions on all six queues, the six `railway.worker-*.toml` services starting on the new Railway project, cron jobs firing, and the dispatcher marking `domain_events` rows processed. Full detail + the staging checklist: `rust/app-cases/wave9/workers-verification.md`.
Consequences: "workers verified" in PR-3 means the queue machinery, not the real workers. The Release Blockers list carries the real-worker proof as a PR-4 item.

## ADR-161: Upload limits — the 2 MB router default was silently capping every upload (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: axum applies a 2 MB default body limit to every route unless the route disables it. Only `POST /api/v1/finance/donate/manual` had done so (ADR-082). Every other upload route — library file/cover (handler caps 512 MB / 5 MB, ADR-057), avatar (5 MB), admin fonts (5 MB), and the four hifz/voice sidecar audio routes (32 MB contract) — was therefore capped at 2 MB by the router, and the handler caps were unreachable above 2 MB. A 3 MB library file or a 3 MB voice clip would 413 at the router with no handler-level error code, even though Python accepts them. The 2 MB cap was a Wave 2/6 accident, not a decided limit: no ADR ever set 2 MB as a product limit.
Decision:
- The router default is disabled (`DefaultBodyLimit::disable()`) on exactly the upload routes whose handler caps exceed 2 MB — library create/update, avatar upload, font upload, and the four hifz/voice sidecar routes (whose forwarder caps govern) — so the **handler caps are the real limits again**. This restores the ported Python limits; it does not raise any of them. Routes without a handler cap keep the 2 MB default.
- Per-type production limits (all ported from Python, unchanged): library files 512 MB / covers 5 MB (`file_too_large` 413 after metadata parsing), receipt images 5 MB (`receipt_too_large` 413), avatars 5 MB, admin fonts 5 MB, hifz/voice audio 32 MB (`hifz_audio_too_large` 413 / `audio_too_large` 422), fatwa ask JSON 1 MB (`request_too_large` 413, ADR-159). Over-cap is always an explicit error with the type's code, never a silent truncation and never a router-default 413.
- The scan gate is unchanged: ADR-051 (SHA-256 blocklist + EICAR + every upload lands as `draft` + manual admin review, no ClamAV) still runs in the handlers before `put_object`, on both storage providers. Scan hardening beyond that (e.g. a cloud scanner) is a PR-4 decision, not a PR-3 silent change — see the Release Blockers list.
- Proven by test: a 3 MB PNG-magic body to the avatar route now passes the router and reaches the handler (it is stored — the sniff only checks magic bytes), where the same body 413'd before this change. The sidecar 3 MB audio test proves the same for the forwarder.
Consequences: uploads above 2 MB work as Python accepts them. The largest accepted body is now 512 MB (library video) — the handlers buffer it (Python's `read_capped_upload` does the same), so a 512 MB upload transiently holds 512 MB in memory; that is Python's own behaviour, ported, and the value is on the Release Blockers list for a streaming-upload follow-up, not silently lowered here.

## ADR-162: Durable storage is S3-compatible — the local provider is dev/test only (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: The Rust `storage.rs` was local-disk only (ADR-015): `STORAGE_PROVIDER != local` → 503 `storage_not_configured`. The product's own history decides this question: the 2026-11-19 migration lost the bytes of 4 manual-donation receipt images because both APIs stored on local ephemeral disk and the old container was redeployed after the uploads. The original repo's storage policy (`docs/ops/storage.md`, Track 2 / §12) already requires `s3` in production, with fail-fast boot validation and a documented local → S3 migration. PR-3 ports that decision to Rust instead of re-deciding it.
Decision:
- **Production: `STORAGE_PROVIDER=s3`** (any S3-compatible endpoint). **Development/test: `local`** (disk under `STORAGE_LOCAL_DIR`). A Railway volume was considered and rejected: it is bound to one service, does not survive a project rebuild the way an external bucket does, and cannot be read by the sidecar/workers as separate services. The concrete endpoint for the new Railway project (Cloudflare R2 is the natural S3-compatible choice — no egress fees, and the project already uses Cloudflare; a self-hosted MinIO is the alternative) is a PR-4 deployment decision; this PR provisions no bucket and names no live one.
- Implementation: `storage.rs` dispatches the same four operations (`put_object`, `read_bytes`, `delete_object`, `presigned_url`) to local (unchanged HMAC behaviour) or S3 (SigV4 over reqwest — PUT/GET/DELETE with header signing; presigned GET URLs with query signing, computed locally with no network call, like the MinIO client). Config uses Python's env names: `S3_ENDPOINT` (default `http://localhost:9000`), `S3_ENDPOINT_PUBLIC` ("" = same as endpoint), `S3_ACCESS_KEY` (`minioadmin`), `S3_SECRET_KEY` (`minioadmin`), `S3_BUCKET` (`maturidi-media`), `S3_REGION` (`us-east-1`); `minio` is Python's alias for `s3`. Objects stay private: no public bucket policy, access only via short-lived presigned URLs (default 300 s). The internal endpoint never leaks to browsers — presigned URLs are signed with the public endpoint.
- Boot validation (`validate_storage_config`, called from `main.rs`), ported from Python's `_validate_s3_settings` + `s3_production_problems` + `assert_production_secrets_ok`: with `STORAGE_PROVIDER=s3` and `APP_ENV=production` the binary refuses to boot on a missing/placeholder/weak secret (`CHANGE_ME*`, known defaults, secret < 32 chars), an empty/placeholder access key, a demo endpoint (`localhost`/`example.*`) or demo bucket, an effective presigned host that is browser-unreachable (`minio`, `localhost`, `127.*`, `*.local`, RFC1918), or a `STORAGE_SIGNING_SECRET` that is missing, short, or equal to `JWT_SECRET`/`ENCRYPTION_KEY`. In dev/test these are warnings, not boot failures. A bad S3 config is never a silent fallback to local.
- `read_bytes` works on both providers (server-side authenticated reads, used by admin/transcribe paths). The local HMAC `download-by-ref` endpoint keeps working for local objects; with S3, clients use the presigned URL directly and that endpoint 403s for S3 keys (the original's behaviour, ported). `/static/{*path}` (uploaded fonts) now reads through the storage abstraction instead of only local disk. The scan gate (ADR-051) runs before `put_object` on both providers.
- Migration (procedure, PR-4): backup (DB + `STORAGE_LOCAL_DIR`) → create bucket → `mc mirror --overwrite <STORAGE_LOCAL_DIR> s3/<S3_BUCKET>/` with keys mirrored exactly (no added prefix, so they match the DB keys) → verify by random SHA-256 sampling → switch `STORAGE_PROVIDER=s3` and redeploy. Full detail: `rust/app-cases/wave9/storage-migration.md`. Never switch the provider on a database full of local keys without running this migration — the keys would resolve to an empty bucket.
- Verification boundary: the SigV4 signing is proven against the published AWS test vectors and the S3 wire shape (PUT/GET/DELETE + presigned URL) against a mock HTTP server asserting the `Authorization` header — no live S3 exists here. A live round-trip against the provisioned bucket (put → presigned GET → delete, plus the local → S3 migration run) is a PR-4 staging proof, on the Release Blockers list.
Consequences: the storage decision is made and the abstraction is real — no fake backend, no silent local fallback. The binary still boots and serves with `local` until PR-4 provisions the bucket.

## ADR-163: Release Blockers list — every incomplete feature that still blocks cutover (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: Wave 9's standing rule is "never report an incomplete feature as complete". PR-3 closes several gaps (mail/SMS transport, sidecar contract, storage decision, upload limits, sqlx audit) but each of them has a staging-only remainder, and several known items were never in PR-3's scope. Without a single list, those remainders scatter across ADRs and get lost before cutover.
Decision:
- `rust/RELEASE_BLOCKERS.md` is the single registry. Every entry names what it is, why it blocks, what would unblock it, and which PR owns it. It is linked from `rust/MAP.md` and `README.md`, and it is the checklist PR-4 works through — an entry leaves the list only when its unblock condition is proven, not when its code lands.
- Entries at PR-3 close: live mail/SMS keys + real delivery test (PR-4); the 8 sidecar routes against the real Python sidecar (PR-4); real ARQ workers on all six queues (PR-4); S3 bucket provisioning + live round-trip + local → S3 migration (PR-4); Railway Postgres `vector`/`pg_trgm` unverified (PR-4); widget CORS wildcard from the deploy layer (PR-4); sync `seq` MAX+1 race (ADR-113, PR-4 or later); admin rate limits unenforced (ADR-090, PR-4 or later); certificate PDF sidecar trigger unspecified (ADR-061, PR-4); subtitle sidecar trigger (ADR-045, PR-4); Dockerfile/railway.toml first Railway build (PR-4 — built in CI, never on Railway); backup/restore + rollback test (PR-4); load test + FastAPI comparison (PR-4). Closed by this PR and recorded as closed, not as blockers: the sqlx audit ignores (ADR-157), the 2 MB upload cap (ADR-161), the mail/SMS stub (ADR-158 — transport done; only the live-key test remains open).
Consequences: cutover readiness is now a checklist with owners, not a feeling. PR-4's job is to empty this list with evidence.

## ADR-164: Avatar uploads get the ADR-051 scanner — the last ClamAV gate is gone (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: The PR-3 upload review found one upload path the Wave 6 scanner never reached: `POST /api/v1/users/me/avatar` still carried the Wave 2 ClamAV gate — 503 `antivirus_unavailable` unless `CLAMAV_ALLOW_INSECURE_DEV_BYPASS=true` and `APP_ENV=dev`. Python's avatar upload uses the same ClamAV gate (`scan_upload_or_reject` → clamd), so the Rust gate was parity with Python — but parity with a scanner the project decided to abandon (user 2026-11-19) and the Rust deployment cannot run (the Dockerfile ships no clamd, and no ClamAV daemon exists in the deployment). In practice the gate meant avatar upload could never succeed in the Rust deployment at all, while library, receipt and font uploads all used the ADR-051 scanner. The three ADR-051 implementations (library, finance receipts, admin fonts) were also three separate copies of the same logic, differing only in their audit action/entity.
Decision:
- The avatar handler now runs the ADR-051 scanner first (EICAR signature + SHA-256 against the admin-managed `library.blocked_hashes` setting), then the magic-byte sniff — the same order as Python (scan → sniff). A hit is 422 `file_infected` with audit row `user.avatar_upload_rejected_infected` on the user entity; clean bytes proceed to storage as before. Avatars have no `draft` state, so the manual-review half of ADR-051 does not apply to them — the scan is the gate, and the image is public immediately on upload, as Python does.
- The scanner is extracted into one shared module (`upload_scan.rs`): `scan_upload(db, config, actor_id, actor_type, action, entity_type, data, filename, ip, user_agent)`. Library, finance and admin fonts are rewired to it with their exact existing audit actions/entities (`library.upload_rejected_infected` / `library_item`, `finance.upload_rejected_infected` / `manual_donation_proof`, `admin.font_upload_rejected_infected` / `platform_setting`) — behaviour unchanged, proven by their existing `file_infected` tests. The three duplicated copies are deleted.
- `CLAMAV_ALLOW_INSECURE_DEV_BYPASS` is removed from `Config` (field, default, env read, doc comment): with the last ClamAV gate gone, the flag has no consumer and keeping it would suggest a bypass that no longer exists. The historical ADRs that mention it (ADR-051 and the Wave 2 scan note) are records of their time and are not rewritten.
- Proven by test: a 3 MB avatar upload now reaches its handler and stores (ADR-161's proof, which also required this fix — the test would 503 before it), and an EICAR body is 422 `file_infected`.
Consequences: every upload path in the binary now scans the same way, with no ClamAV anywhere. A stronger scanner (cloud/YARA) remains a PR-4 decision (Release Blockers), not a silent PR-3 change.

## ADR-165: Harden the live-WS resume test against interleaved poller frames (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: CI run #101 on PR #3 failed the `rust` job twice in the same untouched test file (`rust/app/tests/wave8_lift.rs`), on two different tests — attempt #1 `wave8_pg_smoke` ("live event on pg", the event never arrived in the 5 s window) and attempt #2 `ws_resume_replay_from_query_and_live_message` (the frame after `resume` was a live `{"type":"event"}` instead of `{"type":"resumed"}`). Both are the same root cause: the realtime outbox poller (`spawn_realtime_poller`, 1 s tick) broadcasts rows newer than its spawn-time watermark to the WS hub, and a tick can land between the client's subscribe and the server's ack/replay/resume. The tests read exactly one next frame and asserted it was the wanted one, so any interleaved live frame failed them. Locally both tests pass repeatedly (the exact CI pg_smoke loop green, the resume test 3/3 in isolation before the fix), and PR-3 touches neither the poller nor the WS server — the flakiness is in the tests' frame-reading assumptions, not in the binary.
Decision:
- `ws_resume_replay_from_query_and_live_message` now waits for the frame it expects instead of reading a single next frame: new helpers `wait_frame_type` (waits for a frame with a given `"type"`, e.g. `subscribed` / `resumed`) and `wait_event_id` (waits for an `"event"` frame with a given `event_id`), both in the same shape as the existing `wait_event` — they skip interleaved live `event` frames and answer server `{"type":"ping"}` frames with `pong`, within the same 5 s budget as before. The 5 s budget is unchanged: the drain removes the ordering race, and the ack/replay/resumed frames are all direct responses to client messages (no poller tick in their path), so the bound only needs to cover server processing.
- The assertions are not weakened: the ack is still asserted `subscribed`; both replayed events are still asserted in order with the right `event_id`s (a live copy of an event arriving before its replay, or a replayed duplicate arriving after the live copy, both satisfy the wait — the duplicate is skipped by the next wait); the resume still asserts `resumed` with `last_event_id` equal to the newest event.
- `wave8_pg_smoke` (attempt #1) is deliberately NOT changed here: its failure was a 5 s window expiring with no event at all, a different symptom of the same loaded-runner timing, and it passed on the attempt #2 re-run and in every local replication. If it flakes again, the fix is its own ADR, not a speculative timeout raise bundled into this one.
Consequences: the resume test no longer depends on the poller tick landing outside its reads. The poller/WS server behaviour is untouched — this is a test-hardening fix only, on the PR-3 branch, so the PR still merges only when all four CI jobs are green.

## ADR-166: Raise wave8_pg_smoke's live-event wait from 5 s to 15 s (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: `wave8_pg_smoke` (`rust/app/tests/wave8_lift.rs`, the Postgres live-WS smoke: register, ticket, subscribe to `user.*`, insert a `user.login` event, wait for the live frame) failed in CI three times with the same panic — `wave8_pg_smoke:1007: "live event on pg"`, i.e. `wait_event` returned `None`, no matching live frame within 5 s:
- CI run #101 attempt #1, old HEAD 477f850 (the `rust` job's first failure on PR #3);
- push run 45181799404 attempts #1 and #2, new HEAD 80f6056 (after the ADR-165 fix commits).
In between it passed: run #101 attempt #2 (re-run), the pull_request run 45181810487 on the same HEAD 80f6056 (all four jobs green), and every local replication. The ADR-165 test (`ws_resume_replay_from_query_and_live_message`) PASSED in the failing push run, the SQLite stage was fully green (16/16 in `wave8_lift`), and `wave8.rs`'s own `wave8_pg_smoke` passed in the same PG stage — only `wave8_lift`'s PG execution failed. ADR-165 explicitly deferred this test ("if it flakes again, the fix is its own ADR, not a speculative timeout raise bundled into this one"); it flaked twice more, so this ADR is that fix, and a third blind re-run was refused.
Decision:
- The live-event wait in `wave8_pg_smoke` only is raised from 5 s to 15 s. Nothing else in the test changes: the `subscribed` ack read stays 5 s (it passed in every failing run — no evidence it is at risk), the assertions are unchanged (the test still verifies live WS delivery of a freshly inserted event on Postgres), and no `src/` file changes — the poller and WS server are untouched.
- Evidence for 15 s (measured, not guessed): with temporary instrumentation, the live event arrived 42.9 / 44.9 / 44.5 / 47.0 / 45.7 ms after the insert over 5 local runs (mean ~45 ms, max 47 ms) — the 5 s budget was already ~110x the local latency, so the CI failures are stalls of the poller tick + PG query + broadcast path on a loaded shared runner, not slow delivery. 15 s is 3x the failing budget, ~330x the measured local latency, and 15 poller ticks of slack; the worst case it costs CI is +10 s on one test in a ~14 min rust job. The exact stall magnitude is not visible in the CI log (no timing detail beyond the panic), so 15 s is a headroom choice, not a fitted one — deliberately not 30 s or 60 s, where a genuinely stuck delivery would just fail slower.
- Watermark-tie hazard checked and ruled out (read from `spawn_realtime_poller`, `routes_lift.rs`): the watermark is the `(created_at, event_id)` of the newest row at spawn, and a row is broadcast only when its key is strictly newer. The `user.login` event is inserted seconds after the poller spawns (the register/ticket/serve/subscribe round-trips in between), so its `created_at` is strictly newer — a tie would need two inserts in the same Postgres microsecond, which this test's spacing excludes. No src/ bug was found; none was fixed.
Consequences: the smoke no longer fails on a 5–15 s runner stall. If it ever fails at 15 s, the stall is not a 5 s budget problem and the fix is not a bigger number — it is a real investigation of the poller/PG/WS path. Test-only fix on the PR-3 branch; the PR still merges only when all four CI jobs are green on both the push and the pull_request runs.

## ADR-167: `wave8_pg_smoke` CI failure at the 15 s wait — diagnose, don't raise the budget (decided by Dash per ADR-010, 2026-11-23)
Status: accepted
Context: `wave8_pg_smoke` (`rust/app/tests/wave8_lift.rs`, the Postgres live-WS smoke: register, ticket, subscribe to `user.*`, insert a `user.login` event, wait for the live frame) failed in CI for the 5th time — push run 45204668807 and pull_request run 45204683344 on HEAD a77db2c, both runs, `"live event on pg"` at line 1012 — even with ADR-166's 15 s wait. The other three CI jobs (redis, docker, alembic) are green on both runs. The same code passes locally every time, including 6/6 under full CPU saturation (event arrives in 36–95 ms). Five failures in seven CI runs: run #101 attempt #1 (477f850, 5 s), push run 45181799404 attempts #1 and #2 (80f6056, 5 s), push 45204668807 + PR 45204683344 (a77db2c, 15 s); passes in between were run #101 attempt #2 and PR run 45181810487.
- A diagnosis pass (2026-11-23) could not reproduce the failure locally and ruled out: watermark tie (microsecond `TIMESTAMPTZ`, 76 ms spacing between `user.logged_in` and the test's `user.login`, verified from a row dump), heartbeat timeout (the 2 s `ws_heartbeat_timeout` cannot fire in the 1–3 ms insert window; an earlier stall would fail at the `subscribed` ack instead), CPU load (6/6 pass under saturation), unparseable frames (every server send is JSON), wrong DB/channel/visibility (same pool for insert and poller; `channel_matches("user.*", "user.login")` and `event_visible` pass; the SQLite stage of the same test is green in CI). A calibration experiment with a dead poller (`dispatcher_poll_interval: 30.0`) fails with the exact CI signature, proving the poller is the sole delivery path.
- The CI log (push run 45204668807, rust job) shows the `wave8_lift` binary running only `pg_smoke` finished in 19.95 s, with the panic at `wave8_lift.rs:1012:10` and no other test output. 19.95 s is consistent with a ~5 s baseline plus the full 15 s wait (genuine timeout — the poller never delivered), but it is NOT proof: an early stream close with a ~20 s baseline on a heavily loaded runner cannot be excluded from that number alone.
- The decisive gap: `wait_event` collapsed timeout / stream-close / unparseable-frame into one `None`, so the failure mode in CI was unobserved — and `next_frame`'s `None` conflated a read timeout with EOF on the socket, so even "stream ended" could not be told apart from "nothing arrived".
Decision:
- `wait_event` (and, for consistency, `wait_frame_type` / `wait_event_id`) now reports which failure path fired and after how long: `TIMEOUT` on deadline expiry, `STREAM ENDED` with the close code on a close frame or EOF, `UNPARSEABLE` with the parse error and the first 200 chars of the frame. The plumbing is two small enums — `FrameOutcome` (frame / timeout / EOF) and `TextOutcome` (text / closed / timeout) — with `next_frame` reimplemented on top of `next_frame_outcome` with identical behaviour (WS-level ping auto-reply preserved). `wave8_pg_smoke` also logs its own wait elapsed time at the `expect("live event on pg")` site, so the next CI log shows the wait's duration directly.
- Budgets unchanged (15 s live-event wait, 5 s ack), assertions unchanged, no retries, no masking of the loss. This does not fix the flake; it converts the next CI occurrence from a mystery into a measurement.
- After the next CI occurrence, the poller-side vs WS-close-side investigation is chosen from the measured reason: a `TIMEOUT after ~15 s` means the poller genuinely never delivered in CI and the investigation moves to `spawn_realtime_poller` / the PG pool (e.g. logging `fetch_recent_events` errors instead of swallowing them — a `src/` change, justified only then); a `STREAM ENDED after ≪15 s` means the socket closed early and the investigation traces which close frame and why.
- The wait budget is never raised again as a fix for this test: if the poller genuinely never delivers in CI, the defect is in the poller/PG path and must be fixed there.
Consequences: the next red run on this test will say exactly what happened and after how long, instead of a bare `"live event on pg"`. Test-only change on the PR-3 branch; the PR still merges only when all four CI jobs are green on both the push and the pull_request runs.
