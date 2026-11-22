# MAP — Maturidi Academy, Rust rewrite (backend first)

Goal: a standalone Rust API server that does what the Python FastAPI server does,
with the original kept only as reference. Live project untouched; future deploy (if any)
on a NEW Railway server, fully separate.

## Source base (verified 2026-11-22)

- Original repo: `nasnih123-web/maturidi-academy`, branch `main`
- Main HEAD: `9154d17` — Merge PR #192 (2026-11-22)
- Plus, merged cleanly (no conflicts) into local branch `rust-base`:
  - PR #193 `658fcfc` — feat(player): adaptive aspect frame + full-bleed mobile layout (design 1)
    — 2 files, web only (`globals.css`, `ProVideoPlayer.tsx`), 23 insertions
  - PR #194 `e22b71d` — feat(videos): CRM subtitle cue editor — manual timing/text fixes
    — 8 files, 891 insertions, backend `apps/api/app/modules/videos/*` + CRM
  - Combined base commits: `c414abb` (merge #193), `5d1cb02` (merge #194)
- #193 and #194 are independent single-commit branches off main; neither contains the other.
- New repo: `nasnih123-web/maturidi-academy-rust` (public, created 2026-11-22 via cloud browser, empty)

## Scope (confirmed by user 2026-11-22)

- Phase 1: backend API only. Web / CRM / mobile UI stay in React/Next.js and keep calling the same API.
- UI rewrite decision comes later, by the user.
- Compatibility: HTTP routes, status codes, response shapes, DB schema and auth stay identical,
  so existing clients work unchanged. (To be confirmed as ADR — see DECISIONS.md.)
- Cutover: only with the user's explicit go-ahead, on a new Railway server. Never touch live.

## Analyze result (rust-anything 3.1.0, `analyze apps/api --whole-app --lang python`)

Full report: `rust/PLAN-API.md` (535 lines).

- 1 package, 270 source files (67,350 lines), 128 test files, 30 units.
- Ready: 2, Caution: 3, Blocked: 25, Keep: 0.
- Shape: server. Entry points: `app/main.py` (FastAPI), `app/core/dispatcher.py`,
  `app/worker.py`, `app/seed.py`, plus CLI scripts.
- Tangled core, wave 0, one piece: 49,484 lines —
  `app/core/`, `app/models/`, `app/modules/admin/`, `auth/`, `content/`, `fatwa/`,
  `home/`, `library/`, `places/`, `quran/`, `social/`, `sync/`, `telegram/`,
  `users/`, `videos/`, `voice/`.
- Modules (21): activity, admin, api_keys, articles, auth, calculators, content,
  fatwa, finance, home, library, lms, meta, places, projects, quran, social,
  sync, telegram, users, videos, voice, widgets.
- Backend size: `apps/api` 399 Python files, 123,542 lines total (incl. tests);
  `apps/web` 208M, `apps/crm` 2.9M (for reference only, out of scope in Phase 1).

## Critical finding — the bridge path does not fit a FastAPI server

rust-anything's own analyze says, for all 25 server units:

- `fastapi` / `starlette`: Rust fit **none** — "Web framework: keep the app in Python."
- `uvicorn`: "a server is an application rewrite, not a port."

The PyO3 unit-by-unit bridge (steps 1–5 of the skill) is for libraries / pure
functions, not for a FastAPI server. Porting this backend that way would keep
FastAPI as the server and only move small functions — that is NOT what the user
asked for.

The honest path for a pure Rust server is:

1. **Axum front door** (`rust/app`): owns the entry point, proxies every route
   not ported yet to the original Python server. `app record` / `app replay`
   prove behaviour at the HTTP edge.
2. **Rewrite handlers in Rust, module by module**, with `sqlx` + Postgres,
   `tokio`, `serde` — mirroring routes, validation and errors, proved by
   recorded cases (`app record` → `app replay`, plus unit `record`/`diff` where
   a pure function is ported).
3. **Lift** only when fallback rate is 0 and every unit passes `gate`.

## Hard dependencies (no Rust equivalent — need a decision each)

- `faster-whisper` (STT) — no Rust crate; options: call whisper.cpp / external STT service, or keep as a sidecar.
- `sherpa-onnx` (Zipformer ASR, hifz scoring) — ONNX Runtime in Rust, or sidecar.
- `sentence-transformers` (embeddings, brings torch) — ONNX Runtime / external embedding service, or sidecar.
- `telethon` / `python-telegram-bot` — Rust Telegram crates exist but are not drop-in; bot behaviour must be re-proved.
- `reportlab` (PDF) — Rust PDF crates are partial; certificates / PDFs need a decision.
- `pgvector` — `pgvector` Rust support via sqlx + pgvector crate; check.
- `arq` + Redis workers, `argon2`, `pyotp`, `hijri-converter`, `arabic-reshaper` / `bidi`,
  `minio`, `opentelemetry`, `sentry-sdk` — each needs a crate choice (see PLAN-API.md D30–D69).

## Open questions (for the user — see DECISIONS.md)

- Confirm Axum rewrite (path B) vs. PyO3 bridge-only (path A, not a pure Rust server).
- Confirm compatibility bar (identical routes / DB / auth).
- Confirm first wave: front door + one small read-only route to prove the pipeline,
  or a different first module.
- Confirm how to treat the hard dependencies above (sidecar / external service / reimplement / drop).
- Confirm equivalence bar for floats, error text, validation errors (Pydantic errors will differ in Rust).

## Port progress — Wave 1 + Wave 2 + Wave 3 native (2026-11-22)

- Wave 1 (done, verified green locally): Axum front door, `GET /health`,
  `GET /ready`, proxy fallback to `ORIGIN_URL`. See ADR-007 / ADR-013.
- Wave 2 (done, verified green locally — `cargo fmt`, `clippy -D warnings`,
  `cargo test` incl. Postgres smoke): meta, activity, api_keys, users, auth
  are answered natively in Rust; everything else still proxies. Kill switch
  per module: `RUST_ANYTHING_UNIT_<META|ACTIVITY|API_KEYS|USERS|AUTH>=original`
  falls back to the Python origin. Details: ADR-014 – ADR-017.
- Wave 3 (done, verified green locally — `cargo fmt`, `clippy -D warnings`,
  `cargo test` incl. Postgres smoke): home, content, articles, fatwa are
  answered natively in Rust; everything else still proxies. Kill switch per
  module: `RUST_ANYTHING_UNIT_<HOME|CONTENT|ARTICLES|FATWA>=original` falls
  back to the Python origin. Details: ADR-018 – ADR-030.
  - home: `GET /api/v1/home/sections`, `GET /api/v1/search`,
    `PUT /api/v1/admin/home/sections/{key}/data`
  - content: `GET /api/v1/content/items`,
    `GET /api/v1/content/items/{content_type}/{item_id}/preview`,
    `GET /api/v1/content/policies/{content_type}/{item_id}`,
    `GET|POST /api/v1/content/{resource}`,
    `GET|PATCH|DELETE /api/v1/content/{resource}/{item_id}`,
    `POST /api/v1/content/{resource}/{item_id}/transition`,
    `GET /api/v1/content/public/nav`, `/banners`, `/announcements`,
    `/pages`, `/pages/slug/{slug}`, `/events`, `/quotes`, `/calendar/theme`
  - articles: `GET /api/v1/articles`, `/categories`, `/{slug}/meta`,
    `/{slug}`, `POST /{slug}/share`, `GET|POST /api/v1/admin/articles`,
    `PUT /{article_id}`, `PATCH /{article_id}/status`,
    `GET|POST /api/v1/admin/articles/categories`,
    `PUT|DELETE /{category_id}`
  - fatwa: `GET /api/v1/fatwa/categories`, `/topics`, `/topics/categories`,
    `/topics/{id}`, `POST /topics/{id}/share`, `GET /compare/{id}`,
    `POST /quiz/generate`, `/quiz/submit`, `POST /api/v1/verify/check`,
    `GET|POST /api/v1/admin/fatwa/chunks`,
    `PATCH /api/v1/admin/fatwa/chunks/{id}/status`,
    `POST /api/v1/admin/fatwa/ingest`, `GET /ingest/{run_id}`,
    `POST|GET /api/v1/admin/fatwa/topics`, `PATCH /topics/{id}/status`,
    `PUT /topics/{id}`, `POST /topics/{id}/positions`,
    `POST /api/v1/admin/fatwa/positions`,
    `PATCH /positions/{id}/status`,
    `POST /api/v1/admin/fatwa/quiz/questions`,
    `GET|POST /api/v1/admin/fatwa/categories`, `PUT|DELETE /{id}`,
    `GET|POST /api/v1/admin/fatwa/trusted-sources`,
    `PATCH|DELETE /{id}`, `POST /{id}/check`,
    `GET /api/v1/admin/fatwa/audit`,
    `POST|GET /api/v1/fatwa/submissions`, `GET /{id}`,
    `POST /{id}/submit|approve|publish|reject`, `POST /{id}/add-to-rag`,
    `POST /api/v1/fatwa/questions`, `GET /api/v1/fatwa/clusters`,
    `GET /{id}`, `POST /{id}/answer`,
    `GET /api/v1/fatwas/saved`, `GET|POST|DELETE /{id}/saved`
  - `POST /api/v1/fatwa/ask` is intentionally NOT native (assistant/mufti,
    ADR-026) and stays proxied.
  - Tests: `rust/app/tests/wave3.rs` (20 tests, sqlite in-memory;
    `wave3_pg_smoke` replays the critical path on local Postgres when
    `WAVE2_PG_URL` is set). Cases: `rust/app-cases/wave3/` (14 cases).
  - Not yet ported in Wave 3 (stubs / later waves): ARQ ingest worker
    (ingest trigger always `enqueued=false`, ADR-022), real embedding
    providers (hash-only, ADR-019), Telegram hooks (ADR-025).
  - meta: `GET /api/v1/themes/active`, `GET /api/v1/settings/public`,
    `GET /api/v1/i18n/detect`
  - activity: `POST /api/v1/activity/events`
  - api_keys: `POST|GET /api/v1/api-keys`, `GET|PATCH /api/v1/api-keys/{id}`,
    `POST /api/v1/api-keys/{id}/revoke`, `GET /api/v1/api-keys/{id}/usage`,
    `GET /api/v1/api-keys/{id}/embed-snippet`
  - users: `GET|PATCH|DELETE /api/v1/users/me`, `POST /api/v1/users/me/avatar`,
    `GET /api/v1/users/me/sessions`, `DELETE /api/v1/users/me/sessions/{id}`,
    `GET /api/v1/users/me/consent`, `POST /api/v1/users/me/export`,
    `GET /api/v1/admin/users`, `POST /api/v1/users/me/devices`,
    `GET /api/v1/notifications/web-push/vapid-key`,
    `POST|DELETE /api/v1/users/me/web-push/subscriptions`,
    `GET /api/v1/users/me/notifications`,
    `POST /api/v1/users/me/notifications/{id}/read`,
    `GET /api/v1/users/avatar-by-ref`
  - auth: `POST /api/v1/auth/register`, `login`, `refresh`, `logout`,
    `logout-all`, `verify-email`, `register/verify-email`,
    `register/phone-code/request`, `register/phone-code/verify`,
    `register/set-password`, `register/resend`, `verify-phone/request`,
    `verify-phone`, `password-reset/request`, `password-reset/confirm`,
    `2fa/setup`, `2fa/enable`, `2fa/recover`, `2fa/status`, `2fa/disable`,
    `2fa/recovery/regenerate`, `ws-ticket`
  - Tests: `rust/app/tests/wave2.rs` (26 tests, sqlite in-memory;
    `wave2_pg_smoke` replays the critical path on local Postgres when
    `WAVE2_PG_URL` is set). Cases: `rust/app-cases/wave2/` (14 cases).
  - Not yet ported in Wave 2 (stubs / Wave 7): real mail/SMS delivery,
    ClamAV scan, S3 storage (see ADR-015).

## Units (status)

All `open` until the user confirms the plan. No unit claimed yet.

- Wave 0 (tangled core, one piece, 49,484 lines): see above.
- Wave 1: `alembic/` (43, ready), `app/modules/activity/` (69), `api_keys/` (789),
  `articles/` (690), `calculators/` (3,678 — fiqh-sensitive, HARD RULE applies),
  `lms/` (2,196), `meta/` (133), `projects/` (982), `scripts/` (753).
- Wave 2: `finance/` (2,493), `widgets/` (325).
- Wave 3: `app/` (1,817 — entry / wiring).

## Out of scope (Phase 1)

- Web (`apps/web`), CRM (`apps/crm`), mobile (`apps/mobile`), desktop (`apps/desktop`).
- Live Railway project, domains, DNS.
- Any change to the original repo.
