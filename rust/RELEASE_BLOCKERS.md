# Release Blockers — what still blocks a production cutover

Wave 9 PR-3 (ADR-163). Every incomplete feature / unverified
dependency that blocks cutting production traffic over to the Rust
backend. An entry leaves this list only when its **unblock
condition is proven with evidence**, not when its code lands.
Owners: PR-3 (this PR) or PR-4 (staging deployment). The list is
linked from `rust/MAP.md` and `README.md`.

## Open

### 1. Live mail/SMS credentials + a real delivery test — PR-4
- **What:** `RESEND_API_KEY` (or SMTP credentials) and `SMS_API_KEY`
  (Kavenegar) are not set anywhere, and no real email or SMS has ever
  been sent by the Rust binary. The transport is implemented and
  mock-tested (ADR-158), but a mock proves the request shape, not
  that Resend/Kavenegar accept the project's credentials.
- **Why it blocks:** registration, password reset and phone-code
  verification all depend on delivery. Without it, users cannot
  sign up or recover accounts.
- **Unblocks when:** the staging deploy sets the real keys and one
  verification email and one phone-code SMS are sent end-to-end from
  the Rust binary and confirmed received (inbox + handset), with the
  honest-failure path also shown (unset key → logged error, flow
  continues, resend works).

### 2. The 8 sidecar routes against the real Python sidecar — PR-4
- **What:** the forwarder is hardened and mock-tested (ADR-159,
  `rust/app-cases/wave9/sidecar-contract.md`), but no request has
  ever travelled Rust → real Python sidecar → Rust.
- **Why it blocks:** fatwa ask (incl. SSE streaming), the receipt
  PDF, the four hifz ASR routes and voice transcribe/ask are all
  user-facing; a shape mismatch, a quota difference or a timeout
  misestimate only shows up against the real sidecar.
- **Unblocks when:** in staging, each of the 8 routes is called
  against the real sidecar and returns the documented shape; the
  SSE stream arrives event-by-event; the voice quotas are consumed
  before processing; and stopping the sidecar yields the documented
  503 codes.

### 3. Real ARQ workers on all six queues — PR-4
- **What:** the queue machinery is proven at the ARQ level and the
  real media startup gate is proven to fail loud without its tools
  (ADR-160), but no real `python -m app.worker <queue>` process has
  run, on any queue.
- **Why it blocks:** transcription, OCR, subtitles, fatwa ingestion
  and telegram distribution all happen in workers. A queue that
  never processes is a feature that never happens.
- **Unblocks when:** in staging, one real job per queue is enqueued
  and observed completing in the worker logs; the six
  `railway.worker-*.toml` services start on the new Railway project;
  cron jobs fire on schedule; the dispatcher marks `domain_events`
  rows processed.

### 4. S3 bucket provisioning + live round-trip + local → S3 migration — PR-4
- **What:** the S3 provider is implemented (SigV4, boot validation)
  and proven against a mock server and the AWS signing vectors
  (ADR-162), but no bucket exists and no byte has ever been stored
  in S3 by this binary. The existing local files (`./storage` on the
  live project — including the receipt images that survived) have not
  been migrated.
- **Why it blocks:** uploads, receipts, library files and fonts all
  depend on durable storage; the 2026-11-19 loss happened exactly
  because storage was local and ephemeral.
- **Unblocks when:** the bucket is provisioned (R2 or MinIO — PR-4
  deployment decision), a live put → presigned GET → delete
  round-trip succeeds from the Rust binary, and the migration in
  `rust/app-cases/wave9/storage-migration.md` (`mc mirror` +
  SHA-256 sampling) has run against the real file set.

### 5. Railway Postgres `vector` / `pg_trgm` unverified — PR-4
- **What:** the extensions are proven on the CI service image
  (`pgvector/pgvector:pg16`) and on local Postgres, but not on
  Railway's managed Postgres.
- **Why it blocks:** embeddings search (`vector`) and trigram search
  (`pg_trgm`) are load-bearing (fatwa retrieval, library search);
  a managed Postgres without them fails the migration that creates
  them.
- **Unblocks when:** the staging Railway Postgres runs the Alembic
  migration that creates both extensions (or they are verified
  creatable there first).

### 6. Widget CORS wildcard must come from the deploy layer — PR-4
- **What:** the widget `*` CORS wildcard is a deploy-layer concern
  (ADR-121); the Rust binary only enforces `CORS_ALLOW_ORIGINS` for
  the `/ws/events` Origin check.
- **Why it blocks:** widgets embedded on third-party pages need the
  wildcard at the edge, or they break in production while working
  in dev.
- **Unblocks when:** the staging deploy configuration sets the
  wildcard and a widget loads cross-origin against it.

### 7. Sync `seq` MAX+1 race — PR-4 or later (ADR-113)
- **What:** `sync_changelog.seq` is assigned MAX+1, which races
  under concurrent writers. The Rust port carries the same race as
  Python (deliberate parity, ADR-113).
- **Why it blocks:** under production concurrency two changelog rows
  can share a `seq`, breaking sync ordering for the apps.
- **Unblocks when:** the sequence is assigned atomically (a Postgres
  sequence or an advisory lock) in both implementations, with a test
  that hammers it concurrently.

### 8. Admin rate limits not enforced — PR-4 or later (ADR-090)
- **What:** Python throttles some admin endpoints at 20–30/min; the
  Rust port does not enforce those limits.
- **Why it blocks:** admin endpoints without rate limits are an
  abuse vector in production (bulk export, broadcast, user listing).
- **Unblocks when:** the Python limits are ported and a test proves
  the 429 at the documented threshold.

### 9. Certificate PDF sidecar trigger unspecified — PR-4 (ADR-061)
- **What:** the certificate record is issued natively and the PDF
  download 404s until the Python sidecar renders
  `certificates/{code}.pdf` — but whether the sidecar is triggered
  by polling or by queue is still unspecified.
- **Why it blocks:** a certificate that is issued but never rendered
  is a broken promise to the learner (LMS completion flow).
- **Unblocks when:** the trigger is specified, implemented, and a
  certificate issued in staging renders its PDF end-to-end.

### 10. Subtitle generation sidecar trigger — PR-4 (ADR-045)
- **What:** subtitle generation stays a Python sidecar; the trigger
  returns 202 `enqueued: false` and the sidecar half is unwired.
- **Why it blocks:** videos without subtitles in fa/ar/en/ur/dari
  are an incomplete videos feature (the user's 2026-10-26 spec).
- **Unblocks when:** the trigger path is wired and a video's
  subtitles generate end-to-end in staging.

### 11. Dockerfile / railway.toml first Railway build — PR-4
- **What:** the Dockerfile builds in CI (PR-2 `docker` job, SBOM
  attached) but has never been built on Railway, and `railway.toml`
  has never been consumed by a Railway project.
- **Why it blocks:** CI proves the image builds; only Railway proves
  the project deploys from it (start command, healthcheck, port,
  env).
- **Unblocks when:** the staging Railway project builds from this
  Dockerfile and serves `/health` + `/ready`.

### 12. Backup / restore + rollback test — PR-4
- **What:** no backup/restore run and no rollback run exist for the
  Rust deployment.
- **Why it blocks:** a production cutover without a proven way back
  is not a cutover, it is a gamble.
- **Unblocks when:** a staging backup is restored to a fresh
  database and serves, and a rollback (Rust → Python sidecar, or
  previous image) is executed once.

### 13. Load test + FastAPI comparison — PR-4
- **What:** no load test and no benchmark against the FastAPI
  implementation exist for the Rust binary.
- **Why it blocks:** the Wave 9 completion criteria (user
  2026-11-22) require a benchmark and a comparison before any
  traffic moves.
- **Unblocks when:** a staging load test (representative endpoints,
  sustained concurrency) and a FastAPI-vs-Rust comparison report are
  delivered.

## Closed by PR-3 (recorded here so they are not re-raised)

- **sqlx audit ignores (ADR-152 → ADR-157):** sqlx 0.9.1 +
  libsqlite3-sys 0.40.0; `cargo audit` exits 0 with no ignores.
  Closed 2026-11-23.
- **Avatar ClamAV gate (ADR-051 → ADR-164):** avatars now use the
  shared ADR-051 scanner like every other upload; the dead
  `CLAMAV_ALLOW_INSECURE_DEV_BYPASS` flag is removed. Closed
  2026-11-23.
- **Mail/SMS stub (ADR-015 → ADR-158):** real Resend/SMTP/Kavenegar
  transport landed; only the live-key test (blocker #1) remains.
- **2 MB upload cap (ADR-082 → ADR-161):** handler caps restored
  as the real limits; a 3 MB upload is proven to reach its handler.
- **Sidecar forwarder gaps (ADR-134 → ADR-159):** timeouts, body
  caps, streaming, no-retries rule landed; only the real-sidecar
  pairing (blocker #2) remains.
- **Storage decision (ADR-015 → ADR-162):** S3-compatible decided,
  provider implemented, boot validation landed; only provisioning +
  migration (blocker #4) remain.
- **Workers machinery (ADR-125 → ADR-160):** ARQ-level proof done;
  only the real-worker runs (blocker #3) remain.

## Not blockers (recorded, no action)

- **chrono RUSTSEC-2026-0543 (unmaintained):** warning only,
  `cargo audit` exits 0; no fixed upgrade exists; migrating off
  chrono is out of scope (ADR-152/157).
- **512 MB library uploads buffer in memory:** Python's own
  behaviour, ported (ADR-161). A streaming-upload follow-up is a
  possible optimisation, not a cutover blocker.
