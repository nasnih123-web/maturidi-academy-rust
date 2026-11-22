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

## Port progress — Wave 1 + Wave 2 + Wave 3 + Wave 4 native (2026-11-22)

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
- Wave 4 (done, verified green locally — `cargo fmt`, `clippy -D warnings`,
  `cargo test` incl. Postgres smoke): quran + hifz are answered natively in
  Rust; everything else still proxies. Kill switch per module:
  `RUST_ANYTHING_UNIT_<QURAN|HIFZ>=original` falls back to the Python
  origin. Details: ADR-031 – ADR-043.
- Wave 5 (done, verified green locally — `cargo fmt`, `clippy -D warnings`,
  `cargo test` incl. Postgres smoke): videos + voice are answered natively
  in Rust; everything else still proxies. Kill switch per module:
  `RUST_ANYTHING_UNIT_<VIDEOS|VOICE>=original` falls back to the Python
  origin. Details: ADR-044 – ADR-050. Total native routes: 232
  (Wave 2: 49, Wave 3: 81, Wave 4: 75 — quran 23 + hifz 52; Wave 5: 27 —
  videos 23 + voice 4; plus the Wave 1 front door `GET /health` +
  `GET /ready`).
  - quran: `GET /api/v1/quran/surahs`, `/surah/{number}`,
    `/ayah/{surah}/{ayah}`, `/recitation/reciters`,
    `/recitation/timing/generated`, `/recitation/timing`, `/providers`,
    `PUT /api/v1/quran/registry/custom-providers/{provider_id}/key`,
    `GET /api/v1/quran/munajat/status`, `POST|GET /api/v1/quran/notes`,
    `PUT|DELETE /api/v1/quran/notes/{note_id}`,
    `GET|PUT /api/v1/quran/reading-state`,
    `POST /api/v1/tasbih/sessions`,
    `POST /api/v1/tasbih/sessions/{session_id}/count`,
    `GET /api/v1/tasbih/today`, `GET /api/v1/adhan/times`, `/range`,
    `/options`, `/geocode`, `/reverse`
  - hifz: `GET /api/v1/quran/hifz/status`,
    `POST|GET /api/v1/quran/hifz/attempts`,
    `GET /api/v1/quran/hifz/attempts/resume`,
    `GET|DELETE /api/v1/quran/hifz/attempts/{attempt_id}`,
    `POST .../complete`, `PATCH .../position`, `POST .../pause`,
    `POST .../resume`, `GET .../mistakes`, `POST .../peek`,
    `POST .../dispute`, `DELETE /api/v1/quran/hifz/mistakes/{mistake_id}`,
    `GET|PUT /api/v1/quran/hifz/settings`,
    `GET|PUT|DELETE /api/v1/quran/hifz/listen-position`,
    `GET /api/v1/quran/hifz/stats`, `/review`,
    `GET /api/v1/quran/hifz/admin/attempts`,
    `DELETE /api/v1/quran/hifz/admin/attempts/{attempt_id}`,
    `POST /api/v1/quran/hifz/reviews`,
    `GET /api/v1/quran/hifz/reviews/due`,
    `POST|GET /api/v1/quran/hifz/plans`,
    `GET|PATCH|DELETE /api/v1/quran/hifz/plans/{plan_id}`,
    `PATCH /api/v1/quran/hifz/plans/{plan_id}/items/{item_id}`,
    `GET /api/v1/quran/hifz/dashboard`, `/activity`, `/suggested`,
    `POST|GET /api/v1/quran/hifz/tests`,
    `GET|DELETE /api/v1/quran/hifz/tests/{test_id}`,
    `POST .../complete`, `POST .../questions/{question_id}/skip`,
    `POST .../questions/{question_id}/peek`,
    `POST .../questions/{question_id}/events/{event_index}/remove`,
    `GET /api/v1/quran/hifz/mutashabihat`, `/mutashabihat/phrase`,
    `/mutashabihat/count`, `POST|GET /api/v1/quran/hifz/tasmi/sessions`,
    `GET|PATCH|DELETE /api/v1/quran/hifz/tasmi/sessions/{session_id}`,
    `GET /api/v1/quran/hifz/data/preview`,
    `POST /api/v1/quran/hifz/data/purge`
  - Intentionally NOT native (ADR-031): the ASR endpoints
    `POST /api/v1/quran/hifz/attempts/{id}/grade`, `grade-word`,
    `POST /api/v1/quran/hifz/voice-search`,
    `POST /api/v1/quran/hifz/tests/{id}/questions/{qid}/answer` stay
    proxied; the live stream `GET .../attempts/{id}/stream` is
    registered explicitly as 503 `hifz_live_unavailable`.
  - videos: `GET /api/v1/videos`, `/playlists`, `/playlists/{id}`,
    `/saved`, `GET|POST|DELETE /api/v1/videos/{id}/saved`,
    `GET /api/v1/videos/{id}`, `/subtitles/{lang}.vtt`,
    `GET|PUT /api/v1/videos/{id}/progress`,
    `GET|POST /api/v1/admin/videos/playlists`,
    `PATCH|DELETE /api/v1/admin/videos/playlists/{id}`,
    `POST /api/v1/admin/videos/playlists/{id}/items`,
    `DELETE .../items/{item_id}`,
    `PUT .../items/order`, `GET /api/v1/admin/videos/subtitles`,
    `POST /api/v1/admin/videos/{id}/subtitles/generate`,
    `GET|PUT /api/v1/admin/videos/{id}/subtitles/{lang}`,
    `GET /api/v1/admin/videos/candidates`
  - voice: `GET /api/v1/voice/tts-audio-by-ref`,
    `GET|PUT /api/v1/admin/voice/settings`,
    `GET /api/v1/admin/voice/usage`
  - Intentionally NOT native (ADR-046): `POST /api/v1/voice/transcribe`
    and `POST /api/v1/voice/ask` stay proxied (no STT in the Rust
    binary). Voice intents + TTS are ported as modules (ADR-047);
    audio lessons are Wave 6 LMS (ADR-048).
  - Tests: `rust/app/tests/wave5.rs` (15 tests, sqlite in-memory;
    `wave5_pg_smoke` replays the critical path on local Postgres when
    `WAVE2_PG_URL` is set) plus unit tests in `voice_intents.rs`
    (5), `voice_tts.rs` (6, incl. a mock cloud TTS endpoint),
    `subtitles.rs` (4), `video_links.rs` (2). Cases:
    `rust/app-cases/wave5/` (14 cases).
- Wave 6 (done, verified green locally — `cargo fmt`, `clippy -D warnings`,
  `cargo test` incl. Postgres smoke): library + LMS + calculators are
  answered natively in Rust; everything else still proxies. Kill switch
  per module: `RUST_ANYTHING_UNIT_<LIBRARY|LMS|CALCULATORS>=original`
  falls back to the Python origin. Details: ADR-051 – ADR-075.
  Total native routes: 302 route+method combos (Wave 6 adds 69:
  library 16, LMS 37, calculators 16).
  - library: `GET /api/v1/library/items`, `/search`,
    `/items/{item_id}/stream`, `/items/{item_id}`,
    `/library/download-by-ref`, `GET|POST /api/v1/admin/library/items`,
    `GET|PUT|DELETE /admin/library/items/{item_id}`,
    `POST .../transition`, `GET .../versions`,
    `POST .../versions/{version}/revert`,
    `POST /api/v1/admin/library/import`, `.../reprocess`, `.../index-ai`
    (upload scanning: SHA-256 blocklist + EICAR + manual review, ADR-051;
    media pipeline stays a Python sidecar, ADR-053; index-ai native with
    the hash provider, ADR-054)
  - LMS: `GET /api/v1/lms/courses`, `/courses/{id}`, `/lessons/{id}`,
    `POST /lessons/{id}/progress`, `POST /quiz/submit`,
    `POST /quiz/{lesson_id}`, `GET /reports/me`, `/recommendations`,
    `/cohorts`, `POST /cohorts/{id}/enroll`, `GET /certificates/mine`,
    `/certificates/{code}/download`, `/certificates/download-by-ref`,
    `/certificates/{code}/verify`,
    `GET /api/v1/certificates/{code}/verify`, admin curricula/courses/
    lessons/cohorts/prerequisites/enrollments/progress/certificates
    (certificate PDF stays a Python sidecar, ADR-061; the record is
    issued natively and verifiable immediately)
  - calculators: `POST /api/v1/calculators/inheritance`,
    `POST|GET /inheritance/cases`, `POST /zakat`,
    `GET /zakat/nisab-rate`, `POST /zakat/extended`,
    `POST /inheritance/miras`, `POST /mosaref`,
    `GET|POST /api/v1/admin/calculators/rules`,
    `GET|PATCH /rules/{rule_id}`, `POST .../submit`, `.../review`,
    `GET .../history`, `GET /api/v1/admin/calculators/audit`
    (exact decimal/fraction arithmetic, ADR-065; fiqh tables ported
    verbatim from the Python engines, never invented, ADR-009;
    golden fixtures pin the exact engine output, ADR-075)
  - Tests: `rust/app/tests/wave6.rs` (40 tests: 14 library + 14 LMS +
    11 calculators sqlite tests + 1 combined `wave6_pg_smoke`
    replaying all three critical paths on local Postgres when
    `WAVE2_PG_URL` is set) plus 35 lib unit tests in
    `calc_decimal.rs` / `calc_engines.rs` / `calc_mosaref.rs`. Cases:
    `rust/app-cases/wave6/` (20 cases: lib-01–08, 01–06 lms, calc-01–06).
- Wave 7 (done, verified green locally — `cargo fmt`, `clippy -D warnings`,
  `cargo test` incl. Postgres smokes): finance + admin + admin content
  center + social + sync + telegram + places + widgets are answered
  natively in Rust; everything else still proxies. Kill switch per
  module: `RUST_ANYTHING_UNIT_<FINANCE|ADMIN|ADMIN_CONTENT|SOCIAL|SYNC|
  TELEGRAM|PLACES|WIDGETS>=original` falls back to the Python origin.
  Details: ADR-076 – ADR-132. Total native routes: 440 route+method
  combos (Wave 7 adds 136: finance 23, admin 37, admin content 17,
  social 24, sync 7, telegram 15, places 9, widgets 4). Note: the
  pre-Wave-7 baseline recounts to 304, not 302 — the Wave 6 report's
  total was short by one in its own arithmetic, and hifz recounts to
  53 combos, not 52.
  - finance: `POST /api/v1/finance/donate`, `/donate/manual`,
    `GET /finance/callback`, `POST /finance/callback/idpay`,
    `POST /finance/webhook/{gateway}`,
    `GET /finance/donation-methods`, `GET /finance/campaigns`,
    `GET /finance/campaigns/{campaign_id}`,
    `POST /api/v1/finance/recurring`,
    `GET|POST /api/v1/admin/finance/campaigns`,
    `PUT|PATCH|DELETE /admin/finance/campaigns/{campaign_id}`,
    `GET /api/v1/admin/finance/donations`, `/ledger`, `/reconcile`,
    `/gateways`, `/manual-donations`,
    `/manual-donations/{proof_id}/proof-image`, `/fx-rate`,
    `POST .../manual-donations/{proof_id}/approve`, `.../reject`
    (ZarinPal + IDPay live over HTTPS, ADR-078; receipt PDF stays
    proxied, ADR-076; receipt images use the ADR-051 scan gate, ADR-077)
  - admin: `GET|PUT /api/v1/admin/settings`,
    `POST /api/v1/admin/settings/reencrypt`,
    `GET /api/v1/admin/events/dead-letter`,
    `POST .../dead-letter/{event_id}/retry`,
    `POST|DELETE /api/v1/admin/fonts` (`DELETE .../fonts/{locale}`),
    `GET /api/v1/admin/sync/devices`,
    `GET|POST /api/v1/admin/builds/releases`,
    `GET /api/v1/admin/builds/prereqs`,
    `POST /api/v1/admin/builds/rebuild`,
    `GET /api/v1/admin/builds/requests` (real GitHub
    `workflow_dispatch`, never a shell, ADR-087),
    `GET /api/v1/admin/connections/status`,
    `POST /api/v1/admin/connections/test/{service}`,
    `POST /api/v1/admin/notifications/broadcast`,
    `GET .../broadcasts`, `GET .../broadcasts/{broadcast_id}`,
    `GET .../delivery-stats`, `GET .../channels`,
    `POST .../preview`,
    `GET|POST /api/v1/admin/roles`,
    `GET /api/v1/admin/roles/permissions`,
    `PUT|DELETE /api/v1/admin/roles/{role_id}`,
    `GET /api/v1/admin/users/dashboard`, `/stats`, `/trash`,
    `GET /api/v1/admin/users/{user_id}/overview`,
    `PATCH .../roles`, `POST .../sessions/revoke`,
    `POST .../suspend`, `POST .../activate`,
    `DELETE /api/v1/admin/users/{user_id}`,
    `POST .../restore`, `DELETE .../permanent`
    (admin rate limits NOT enforced, ADR-090; roles-cohort broadcasts
    dedup with DISTINCT, ADR-091)
  - admin content center: `GET /api/v1/admin/content/adapters`,
    `/items`, `/items/{domain}/{content_type}/{item_id}`,
    `POST /api/v1/admin/content/bulk`,
    `GET|PUT /api/v1/admin/content/placements`,
    `PATCH|DELETE .../placements/{placement_id}`,
    `POST .../placements/reorder`,
    `GET|PUT /api/v1/admin/content/distribution`,
    `PATCH .../items/{domain}/{content_type}/{item_id}/distribution`,
    `GET /api/v1/admin/content/calendar`,
    `POST .../items/{domain}/{content_type}/{item_id}/transition`,
    `GET /api/v1/admin/content/audit`, `/ai-sources`,
    `POST .../ai-sources/{domain}/{content_type}/{item_id}/reindex`
    (11 adapters, static registry, ADR-098; reindex answers
    `enqueued: false`, ADR-103; fatwa draft transition emits no
    event, ADR-101)
  - social: `GET|POST /api/v1/social/groups`,
    `GET /api/v1/social/groups/{group_id}`,
    `POST .../groups/{group_id}/join`, `.../leave`,
    `GET /api/v1/social/feed`,
    `GET /api/v1/social/groups/{group_id}/posts`,
    `POST /api/v1/social/posts`, `GET /api/v1/social/posts/{post_id}`,
    `POST|DELETE .../posts/{post_id}/like`,
    `GET|POST .../posts/{post_id}/comments`,
    `POST|DELETE /api/v1/social/follows` (`DELETE .../{followed_id}`),
    `POST /api/v1/social/report`,
    `GET /api/v1/social/notifications`,
    `POST .../notifications/{notif_id}/read`,
    `GET .../notifications/unread-count`,
    `GET /api/v1/admin/social/reports`,
    `POST .../reports/{report_id}/action`,
    `GET|POST /api/v1/moderation/banned-words`,
    `DELETE .../banned-words/{word_id}`
    (mark-read persists — deliberate deviation, ADR-108; banned-word
    filter verbatim, ADR-109)
  - sync: `POST /api/v1/sync/push`, `GET /api/v1/sync/pull`,
    `/sync/cursor`, `/sync/devices`, `/sync/conflicts`,
    `POST /api/v1/sync/conflicts/{conflict_id}/resolve`,
    `GET /api/v1/sync/offline-pack`
    (idempotent push, max-wins progress, optimistic note conflicts,
    tombstones; `seq` MAX+1 race NOT closed, ADR-113)
  - telegram: `POST /api/v1/telegram/webhook`,
    `GET /telegram/status/public`,
    `POST /telegram/connect/request-code`,
    `POST /telegram/connect/confirm-code`,
    `GET /telegram/connect/status`,
    `POST /telegram/connect/disconnect`, `POST /telegram/import`,
    `GET|PUT /api/v1/telegram/distribute/policies`
    (`PUT .../policies/{content_type}`),
    `POST /telegram/distribute/publish`,
    `GET /telegram/distribute/log`,
    `POST .../retry/{published_id}`, `.../revoke/{published_id}`,
    `.../approve/{published_id}`, `.../reject/{published_id}`
    (Telethon connect honestly 503, ADR-114; sends degrade to `failed`,
    ADR-115; no ARQ enqueue, ADR-116)
  - places: `GET /api/v1/places`, `POST /api/v1/places/submissions`,
    `GET /api/v1/places/{place_id}`,
    `POST|GET /api/v1/admin/places`,
    `GET|PATCH|DELETE /admin/places/{place_id}`,
    `POST .../transition` (draft-only delete; `type` from the live
    `places.types` setting; `status` filter after the limit, ADR-119)
  - widgets: `GET /api/v1/widgets/prayer-times`, `/ayah-of-day`,
    `/latest-fatwa`, `/zakat-mini` (gate: `X-API-Key` or `?embed_token=`;
    CORS wildcard NOT ported — deploy layer, ADR-121; ayah cache
    in-process only, ADR-122)
  - workers: ARQ queues, dispatcher, `/ws/events`, `/metrics` and
    `/api/v1/health` all stay on the Python origin (ADR-125 – ADR-128);
    `projects/due_cron` noted as portable-in-principle, out of scope
    (ADR-129). The `projects` module (17 routes) is NOT in Wave 7 and
    stays proxied.
  - Merge fixes (ADR-132): the calculator engines
    (`calc_decimal`, `calc_engines`, `calc_mosaref`) are declared once
    at the crate root — `routes_widgets.rs` had `#[path]`-included a
    second copy, which broke `clippy -D warnings` with 37 duplicate-mod
    / dead-code errors; 14 further style lints were fixed in place.
    `TEST_SCHEMA` holds exactly one copy each of `social_posts` and
    `religious_places`; the Wave 3 fatwa catch-all event was fixed so a
    draft transition emits no domain event (ADR-101).
  - Tests: `rust/app/tests/wave7_finance.rs` (26: 25 sqlite +
    `wave7_finance_pg_smoke`), `wave7_admin.rs` (13: 12 sqlite +
    smoke), `wave7_admin_content.rs` (24: 23 sqlite + smoke),
    `wave7_social.rs` (20: 19 sqlite + smoke),
    `wave7_sync_telegram.rs` (17: 16 sqlite + smoke),
    `wave7_places_widgets.rs` (16: 15 sqlite + smoke) — 116 Wave 7
    tests, each binary replaying its critical path on local Postgres
    when `WAVE2_PG_URL` is set. Cases: `rust/app-cases/wave7/`
    (50 cases + README: fin-01–08, adm-01–10, acc-01–08, soc-01–08,
    syn-01–08, plw-01–08).
- Wave 8 (lift, done, verified green locally — `cargo fmt`, `cargo clippy -D warnings`,
  `cargo test` incl. Postgres smokes): the catch-all proxy fallback is gone. Anything no route matches is a native 404 in Python's error envelope (`http_error` / `Not Found`, ADR-133); a wrong method on a known path is a native 405 in the same envelope. `ORIGIN_URL` is deleted from `Config` (ADR-133). Kill switches are retired: `RUST_ANYTHING_UNIT_<NAME>=original` only logs one warning per module and the routes stay native (ADR-133). New native surfaces (ADR-135 – ADR-140): `GET /metrics` (real counters via a middleware layer + outbox gauges from `domain_events`, `text/plain; version=0.0.4`); `GET /api/v1/health` (`media_tools` honestly all-unavailable); `GET /openapi.json` (minimal spec generated at startup from the router's `Debug` form, cached process-wide); `GET /static/{*path}` (uploaded fonts from `{storage_dir}/static`); `GET /ws/events` (native Axum WebSocket: ticket/JWT handshake, Origin check, subscriptions, heartbeat 4400, resume replay, outbox poller fan-out). The 8 sidecar-contract routes forward explicitly to `SIDECAR_URL` and answer honest 503s with route-specific codes when it is unset/unreachable (ADR-134): `POST /api/v1/fatwa/ask` (`fatwa_ask_unavailable`), `GET /api/v1/finance/receipts/{id}` (`receipt_pdf_unavailable`), the four hifz ASR routes (`hifz_asr_unavailable`), `POST /api/v1/voice/transcribe` + `.../ask` (`voice_stt_unavailable`). `/docs` is not served and 404s, matching production Python (dev-only). `GET /health` and `GET /ready` unchanged; the hifz live-stream `GET → 503 hifz_live_unavailable` stub stays (ADR-031; the `routes_hifz.rs` citations that said ADR-032 are fixed, ADR-147).
  - projects: `routes_projects.rs` — all 17 Python endpoints native (18 routes incl. the due-soon-cycle endpoint, ADR-141/142): project CRUD with cursor pagination (`base64url("<ISO ts>|<uuid>")`, 400 `bad_cursor`) and live `task_count`; task CRUD + kanban `move` with the `TASK_TRANSITIONS` guard (422 `invalid_status_transition` with `{from, to, allowed}`); reverse lookup `GET /api/v1/projects/tasks/by-entity` (registered before `/tasks/{task_id}`); staff CRUD (409 `staff_exists`, one row per user); staff-reader gate (authenticated user with `projects.manage` or an active `staff_members` row, else 403 `forbidden`); audit rows + domain events (`project.created`, `task.created`, `task.assigned`, `task.status_changed`, `task.due_soon`); assign/due-soon notifications honouring `notification_preferences` (`in_app` writes a `notifications` row; email/SMS count as delivered when an address/phone exists, per the stub mailer — ADR-143); due-soon cycle as native `run_task_due_soon_cycle(now)` + `POST /api/v1/admin/projects/due-soon-cycle` (ADR-142; 24h window, 20h anti-spam, unassigned rows still stamped). `TEST_SCHEMA` gains `projects`, `tasks` (+ `ix_tasks_related_entity`), `staff_members`.
  - Route accounting: Python has 468 endpoints (467 HTTP decorators + the hifz WS stream) plus `/docs` (dev-only), `/openapi.json` (auto) and the `/static` mount. Rust serves **471** route+method combos: **463 native** (440 pre-lift + 5 lift-new + 18 projects) + **8 sidecar-contract**. The 6 generic CMS `{resource}` routes cover all 36 concrete Python CMS routes, and the due-soon-cycle endpoint is a deliberate Wave 8 addition with no Python HTTP equivalent. The pre-lift native count is **440**, re-verified two ways (source parse and live-router spec); the 439 figure was a miscount (a `//` comment hid the video-subtitles route from a naive parse) and is refuted in ADR-147.
  - Replay (ADR-138): `rust/app/tests/wave8.rs` replays all 127 app-cases from waves 2–7 against the pure-Rust build (fresh sqlite DB per case, auth provisioned by label, mechanical placeholder substitution, per-file seeding hooks) — final replay 2026-11-22: **122 passed, 0 failed, 5 skipped** (wave2 11/0/3, wave3 14/0/0, wave4 15/0/0, wave5 14/0/0, wave6 18/0/2, wave7 50/0/0); the 5 skipped are documentation-only by design (wave2/08, 09, 10 need tokens from a prior step; wave6/lib-05 needs a minted download signature; wave6/lib-06 has a prose multipart body). The first run had 3 failures, all case-file drift verified against Python and corrected to the Python behaviour (wave2/01 nested `places.globe` + no vapid key; lib-04 no `total`; syn-08 bare-array response); PG smoke green.
  - Cutover: `scripts/cutover.sh` (read-only preflight: live-project guard, env, Postgres, Redis, schema, bundled data, sidecar UP/DOWN table; exit 0 GO / 2 DEGRADED / 1 NO-GO; `--dry-run`) + `scripts/cutover-README.md` (Persian runbook: deploy order, env-var table, smoke checklist, rollback). Script only — never executed against a live server. Open items from the cutover worker: no Dockerfile / `railway.toml` in the Rust repo yet; whether Railway Postgres provides `vector`/`pg_trgm`; the mail/SMS delivery path on the Rust deployment (mailer is a stub); widget CORS wildcard must come from the deploy layer.
  - Tests: `rust/app/tests/wave8_projects.rs` (18: 17 sqlite + `wave8_projects_pg_smoke`), `rust/app/tests/wave8_lift.rs` (16: 15 sqlite + `wave8_pg_smoke`), `rust/app/tests/wave8.rs` (2: `replay_all_app_cases` — 122 passed / 0 failed / 5 skipped over the 127 wave2–wave7 cases — + `wave8_pg_smoke`), `rust/app/tests/front_door.rs` (11, rewritten: proxy tests deleted, 404/405/metrics/health/openapi/sidecar-503 added). Cases: `rust/app-cases/wave8/` (`proj-01`–`proj-13`, `lift-01`–`lift-12` + README).
  - Tests: `rust/app/tests/wave4.rs` (24 tests, sqlite in-memory;
    `wave4_pg_smoke` replays the critical path on local Postgres when
    `WAVE2_PG_URL` is set). Cases: `rust/app-cases/wave4/` (15 cases).
  - Data embedded from the project's own licensed files
    (`rust/app/data/quran/`): Tanzil Uthmani text, bundled translations
    (fa.rowwad, fa.taji) + tafsirs (fa.mokhtasar, fa.saadi), Torbat Jam
    adhan table, generated recitation timings (ar.husary surahs 1, 67),
    mushaf lines (for test passages). Surah 2 timings are in-progress
    in Python and deliberately not served.
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
