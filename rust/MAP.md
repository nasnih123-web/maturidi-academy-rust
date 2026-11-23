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
- **Locked migration base: `rust-base` HEAD `5d1cb02bf7665ad04cf13e32360759de47443ff8`** (merge of PR #194; parents `c414abb` + `e22b71d`; verified 2026-11-22, re-verified in Wave 9 PR-1). The base does not move.
- New repo: `nasnih123-web/maturidi-academy-rust` (public, created 2026-11-22 via cloud browser, empty)

## Scope (confirmed by user 2026-11-22)

- Phase 1: backend API only. Web / CRM / mobile UI stay in React/Next.js and keep calling the same API.
- **UI decision (made in Wave 9, user 2026-11-22): the UIs stay TypeScript — no Rust UI rewrite, ever, under this plan.** Web, CRM, mobile and desktop remain as they are and keep calling the same API.
- Compatibility: HTTP routes, status codes, response shapes, DB schema and auth stay identical,
  so existing clients work unchanged. (ADR-006.)
- Cutover: only with the user's explicit go-ahead, on a new Railway server. Never touch live.
- **Wave 9 standing rules:** every change goes in an independent PR with tests and docs; a PR is never declared complete on SQLite tests alone; the live project, its domains/DNS and its database are never touched; no incomplete feature is reported as complete.

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

## Hard dependencies (decided per item — Wave 9 PR-1)

Each item below was decided during the waves; the ADR is the record.
Nothing here is still open as a porting decision (production gaps are
Wave 9 PR-3 work, not porting decisions).

- `faster-whisper` (STT) — Python sidecar via the sidecar contract
  (ADR-046 → ADR-134); subtitle generation stays a Python sidecar
  (ADR-045).
- `sherpa-onnx` (Zipformer ASR, hifz scoring) — Python sidecar via the
  sidecar contract (ADR-031 → ADR-134); the live stream stays an
  explicit 503 (ADR-031).
- `sentence-transformers` (embeddings) — hash-only in Rust (ADR-019);
  `embedding_vec` stays NULL; pgvector stays a schema-only concern.
- `telethon` / `python-telegram-bot` — connect honestly 503, sends
  degrade to `failed` (ADR-114/115/116).
- `reportlab` (PDF) — certificate PDF stays a Python sidecar (ADR-061);
  receipt PDF is a sidecar-contract route (ADR-076 → ADR-134).
- `pgvector` — schema extension only; Rust never queries it (ADR-019).
- `arq` + Redis workers — stay Python (ADR-125); Rust only writes
  outbox rows. `argon2`, `pyotp`, `hijri-converter`,
  `arabic-reshaper` / `bidi`, `minio`, `opentelemetry`, `sentry-sdk` —
  each ported or explicitly dropped per its module ADR (see
  PLAN-API.md D30–D69 for the original survey).

## Open questions — resolved (Wave 9 PR-1)

- Axum rewrite (path B) vs. PyO3 bridge-only (path A): **Axum** (ADR-005).
- Compatibility bar (identical routes / DB / auth): **yes** (ADR-006).
- First wave: front door + `/health` + `/ready` (ADR-007).
- Hard dependencies: decided per item — see "Hard dependencies" above.
- Equivalence bar for floats, error text, validation errors: status + error
  shape pinned, exact Pydantic wording not reproduced (ADR-006).

## Port progress — Waves 1–8 (complete) + Wave 9 (PR-3 in progress)

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
  origin. Details: ADR-044 – ADR-050. Total native routes: 233
  (Wave 2: 49, Wave 3: 81, Wave 4: 76 — quran 23 + hifz 53; Wave 5: 27 —
  videos 23 + voice 4; plus the Wave 1 front door `GET /health` +
  `GET /ready`). (Correction, Wave 9 PR-1 / ADR-148: the Wave 5 report
  said 232 with hifz 52; the recount is 233 with hifz 53. Wave 6's 302
  and the pre-Wave-7 304 were already right.)
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
  - Intentionally NOT native in Wave 5 (ADR-031): the ASR endpoints
    `POST /api/v1/quran/hifz/attempts/{id}/grade`, `grade-word`,
    `POST /api/v1/quran/hifz/voice-search`,
    `POST /api/v1/quran/hifz/tests/{id}/questions/{qid}/answer` were
    not registered then (they fell through to the proxy); **since Wave 8
    they are sidecar-contract routes (ADR-134)** — explicit forwarding
    to `SIDECAR_URL`, honest 503 `hifz_asr_unavailable` when it is
    unset/unreachable. The live stream `GET .../attempts/{id}/stream` is
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
  - Intentionally NOT native in Wave 5 (ADR-046): `POST /api/v1/voice/transcribe`
    and `POST /api/v1/voice/ask` were not registered then (no STT in the Rust
    binary; they fell through to the proxy). **Since Wave 8 they are
    sidecar-contract routes (ADR-134)** — explicit forwarding to
    `SIDECAR_URL`, honest 503 `voice_stt_unavailable` when it is
    unset/unreachable. Voice intents + TTS are ported as modules (ADR-047);
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
    (ZarinPal + IDPay live over HTTPS, ADR-078; receipt PDF was
    proxied in Wave 7 (ADR-076) and is a **sidecar-contract route
    since Wave 8 (ADR-134)** — explicit forwarding to `SIDECAR_URL`,
    honest 503 `receipt_pdf_unavailable` when it is unset/unreachable;
    receipt images use the ADR-051 scan gate, ADR-077)
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
  - workers: ARQ queues + dispatcher stay Python (ADR-125) — Rust only
    writes outbox rows; the Python workers still do the processing
    (see `scripts/cutover-README.md` §6 for the six queues). `/ws/events`,
    `/metrics` and `/api/v1/health` were still on the Python origin in
    Wave 7 (ADR-126 – ADR-128) but are **native since Wave 8
    (ADR-135 – ADR-137)**. `projects/due_cron` was out of scope in Wave 7
    (ADR-129); the `projects` module (17 endpoints + the due-soon-cycle
    endpoint = 18 routes) was **ported natively in Wave 8
    (ADR-141/142)**.
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
  - Cutover: `scripts/cutover.sh` (read-only preflight: live-project guard, env, Postgres, Redis, schema, bundled data, sidecar UP/DOWN table; exit 0 GO / 2 DEGRADED / 1 NO-GO; `--dry-run`) + `scripts/cutover-README.md` (Persian runbook: deploy order, env-var table, smoke checklist, rollback). Script only — never executed against a live server. `Dockerfile` + `railway.toml` were added in the Wave 8 push and are documented as-is in Wave 9 PR-1 (`scripts/cutover-README.md` §3) — **neither has ever been built; the first Railway build is their first verification**. Open items: whether Railway Postgres provides `vector`/`pg_trgm`; the mail/SMS delivery path on the Rust deployment (mailer is a stub); widget CORS wildcard must come from the deploy layer.
  - Tests: `rust/app/tests/wave8_projects.rs` (18: 17 sqlite + `wave8_projects_pg_smoke`), `rust/app/tests/wave8_lift.rs` (17: 15 sqlite + 2 pg: `wave8_pg_smoke` + `wave8_pg_smoke_poller_spawn_cutoff`, ADR-168), `rust/app/tests/wave8.rs` (2: `replay_all_app_cases` — 122 passed / 0 failed / 5 skipped over the 127 wave2–wave7 cases — + `wave8_pg_smoke`), `rust/app/tests/front_door.rs` (11, rewritten: proxy tests deleted, 404/405/metrics/health/openapi/sidecar-503 added). Cases: `rust/app-cases/wave8/` (`proj-01`–`proj-13`, `lift-01`–`lift-12` + README).
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
  - `POST /api/v1/fatwa/ask` was intentionally NOT native in Wave 3 (assistant/mufti,
    ADR-026) and fell through to the proxy then. **Since Wave 8 it is a
    sidecar-contract route (ADR-134)** — explicit forwarding to
    `SIDECAR_URL`, honest 503 `fatwa_ask_unavailable` when it is
    unset/unreachable.
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

## Wave 9 — Production Readiness (PR-3 in progress)

Goal: prepare the Rust backend for a real build, test and deployment on
an **independent** Railway project. Standing rules (user directive
2026-11-22 ~23:59 Tehran): the original repo `maturidi-academy` is
untouched; live Railway, domains, DNS and the live database are untouched;
the migration base is locked (see "Source base" above); web, CRM, mobile
and desktop UIs stay TypeScript — no Rust UI rewrite; every change goes in
an independent PR with tests and docs; no PR is declared complete on
SQLite tests alone. PR order: PR-1 Architecture and Cutover
Reconciliation → PR-2 CI and Build Verification → PR-3 Production
Blockers → PR-4 Staging Deployment.

### PR-1 reconciliation (verified from code, 2026-11-22)

| Item | Before (docs) | After (correct) | Source |
|---|---|---|---|
| Total served route+method combos | 453 (ADR-139/147, stale) | **471** = 463 native + 8 sidecar | paren-depth count of `.route(` in `lib.rs` + `front_door` OpenAPI assertion |
| Wave 5 total | 232 (hifz 52) | **233** (hifz 53) | same count; Wave 4 total is 76 |
| TEST_SCHEMA tables | 116 | **119** | distinct `CREATE TABLE` names in `db.rs` |
| Fatwa topics table | `fatwa_topics` (cutover.sh `CORE_TABLES`) | **`fiqh_topics`** | `apps/api/app/models/models.py:715` + TEST_SCHEMA |
| ADR-026/031/046/076 | proxied routes (supersession implicit) | **superseded by ADR-134** (sidecar contract) | ADR-148 makes it explicit |
| `CORS_ALLOW_ORIGINS` | "not read" (cutover README §5.3) | **IS read** — `/ws/events` Origin check (`lib.rs` + `routes_lift.rs` `origin_allowed`); only the widget `*` wildcard is deploy-layer (ADR-121) | `lib.rs` |
| Alembic head | head `f2a3b4c5d6e7`, 50 revisions | confirmed: single linear chain, 50 files, 50 distinct revision ids, single head `f2a3b4c5d6e7`, root `bd898906c7e2` | `rust-base` checkout |
| `ORIGIN_URL` | live in cutover.sh + docs | **deleted** — replaced by `SIDECAR_URL` (exactly the 8 sidecar routes) | ADR-133/134 |

Per-module route breakdown (471 total): other (front door) 2, activity 1,
admin 37, admin_content 17, api_keys 7, articles 13, auth 22, calculators
16, content 17, fatwa 48, finance 23, hifz 53, home 3, library 16, lift 13
(5 lift-new + 8 sidecar), lms 37, meta 3, places 9, projects 18, quran 23,
social 24, sync 7, telegram 15, users 16, videos 23, voice 4, widgets 4.
Per wave: Wave 2: 49, Wave 3: 81, Wave 4: 76 (quran 23 + hifz 53),
Wave 5: 27, Wave 6: 69, Wave 7: 136, front door 2 → 440 pre-lift;
+5 lift-new + 18 projects = 463 native; +8 sidecar = 471.
Full corrections in ADR-148.

### PR-1 scope (what changed)

- `scripts/cutover.sh` + `scripts/cutover-README.md` rewritten to Wave 8
  reality: no `ORIGIN_URL`; `SIDECAR_URL` for exactly the 8 sidecar
  routes; Step 7 added (live-endpoint probe of a running Rust binary,
  gated on optional `RUST_API_URL`); exit codes 0 GO / 2 DEGRADED /
  1 NO-GO; read-only preflight, never deploys, never prints secrets.
- Six ARQ queues documented (full detail in `cutover-README.md` §6):
  critical (payments, cron only), ai (fatwa/voice ingestion), media
  (STT/TTS/OCR/subtitles), telegram (bot + import), notifications (email/
  SMS/push, cron only), analytics (cron only). The dispatcher runs
  **in-process** in the Python sidecar API lifespan; the Rust WS poller
  only reads `domain_events` and never marks rows processed.
- `Dockerfile` + `railway.toml` documented as-is (full detail in
  `cutover-README.md` §3): both exist, **neither has ever been built**;
  the first Railway build is their first verification (PR-2). Safety
  property: Dockerfile `CMD` and railway.toml `startCommand` agree
  (`./maturidi-api-rs`).
- Tests: `rust/app/tests/wave9_pr1.rs` guards the preflight logic
  (script syntax + dry-run output + guard + live endpoints on sqlite
  and on local Postgres when `WAVE2_PG_URL` is set); cases under
  `rust/app-cases/wave9/`.

### PR-2 CI and Build Verification (merged as PR #2, 2026-11-23)

Four CI jobs, all merge gates — a failing run blocks the PR
(ADR-149/150/151/153/154). **Merged as `2a204d0`** (merge commit,
parents `f10b5cc` + `724ef81`, branch `wave9/pr-2-ci-build-verification`
retained); CI green on the branch (#91 push + #92 PR, all four jobs)
and on main (#93, all four jobs green, ~24 min). Local verification:
339 tests green, release build green, `alembic-check.sh` PASS.
**Branch protection is DONE (2026-11-23 ~10:15):** the rule on `main`
requires the checks `rust`, `redis`, `docker`, `alembic`, does not
allow bypassing (applies to admins too), no required approvals;
GitHub sudo-mode was confirmed by the user entering the emailed
one-time code himself in the live browser.

| Gate (job) | What runs | Service images | What it proves that local runs cannot |
|---|---|---|---|
| `rust` | fmt, clippy, `cargo build --release --locked`, `cargo test` (SQLite), PG smokes sequential, `cargo audit` | `pgvector/pgvector:pg16` (DB `maturidi_test`) | PG smokes against a real pgvector Postgres |
| `redis` | `cargo test --test wave9_pr2` (PING, SET/GET, RateLimiter, /ready live/dead) | `redis:7-alpine` | Redis tests against a real server — its own job so the service is never reachable from the `rust` job (ADR-154) |
| `docker` | build the repo Dockerfile, syft SBOM → artifact `sbom` | — (builds the image) | first-ever build of the Dockerfile |
| `alembic` | `scripts/alembic-check.sh` | `pgvector/pgvector:pg16` (DB `alembic_check`) | locked migrations upgrade a real Postgres; Rust /ready against the migrated schema |

- **Redis wiring (ADR-153, corrected by ADR-154):** the `redis` job
  is a job of its own — NOT a step in the `rust` job. A Redis service
  in the `rust` job is reachable at localhost:6379, which is exactly
  the default `redis_url` the test configs use
  (`Config::default()`), so every binary would share the auth
  rate-limit counter (same test IP) and the suite would 429 itself —
  proven locally twice. The `redis` job runs only
  `cargo test --test wave9_pr2` with `REDIS_URL` set; the `rust` job
  has no Redis service and never sets `REDIS_URL`. `WAVE2_PG_URL` is set ONLY in the sequential smoke step (`cargo test --test <bin>
  -- --test-threads=1 pg_smoke`, 16 binaries) — the smokes share one
  database and would clobber each other under parallel `cargo test`.
- **Docker (ADR-150):** `docker/build-push-action@v6`, `push: false`,
  `load: true`, tag `maturidi-api-rs:ci`. SBOM is SPDX JSON from syft
  on the built image (OS packages + binary), uploaded as artifact
  `sbom`. `cargo audit` fails the run on any RUSTSEC advisory. The
  two named exceptions from ADR-152 (`--ignore RUSTSEC-2023-0071`
  rsa and `--ignore RUSTSEC-2026-0459` libsqlite3-sys) were **closed
  by PR-3 (ADR-157)**: sqlx 0.9.1 + libsqlite3-sys 0.40.0, audit
  clean with no ignores. The first audit also surfaced chrono
  RUSTSEC-2026-0543 as a warning only (no fixed upgrade; recorded
  in ADR-152, no ignore needed).
- **Alembic (ADR-151, fixed by ADR-155):** the original repo is checked out at main `9154d17` (read-only) and the locked base is recreated by merging PR #193 + #194 — the locked-base merge commit `5d1cb02` was never pushed and cannot be checked out by SHA. `scripts/alembic-check.sh` proves the checkout is the locked base by TREE hash (`73422149…`, ADR-155), then asserts single head `f2a3b4c5d6e7`, runs `alembic upgrade head`,
  verifies version row + `vector`/`pg_trgm` + 120 tables (119 app +
  `alembic_version`), then serves the Rust release binary's /ready
  against the migrated schema. The same script runs locally.
- **What CI still cannot verify:** nothing in PR-2's scope — the
  Docker build, the vector extension on the service image, and the
  Alembic upgrade are all CI-verified by design. (No Docker daemon
  exists in the dev VM, so local runs cannot reproduce the `docker`
  job; the `rust` and `alembic` jobs are reproducible locally with a
  local Postgres + Redis.)
- **Tests:** `rust/app/tests/wave9_pr2.rs` (Redis PING/SET/GET,
  `RateLimiter` on real Redis, /ready with live/dead Redis, PG smoke
  for `vector` + `pg_trgm`) + cases `pr2-01`/`pr2-02` under
  `rust/app-cases/wave9/`. `RateLimiter` is newly re-exported from the
  crate root so the tests can reach it.

### PR-3 Production Blockers (this PR)

Closes the gaps PR-1/PR-2 left open. Full record: ADR-157–165;
contract docs under `rust/app-cases/wave9/`;
**`rust/RELEASE_BLOCKERS.md`** is the single registry of everything
that still blocks cutover (each entry: what / why it blocks / what
unblocks it / owner PR).

- **Real email + SMS (ADR-158):** the Wave 2 stub is gone.
  `mail.rs` sends for real — Resend HTTPS API preferred, SMTP
  (STARTTLS, lettre) fallback, Kavenegar SMS — credentials only
  from env (`RESEND_API_KEY`, `SMTP_*`, `SMS_API_KEY`); unset or
  unreachable fails honestly (`Result<(), MailError>`), never a fake
  success. Auth flows stay best-effort (token stored, failure logged,
  resend works); the admin broadcast path now records the real
  per-channel outcome instead of marking every attempted send
  delivered. Verification/reset emails render real bodies from
  `mail_templates.rs` (fa/ar/en copy, locale rule, `public_url`
  validation) instead of empty strings. Mock-transport tests prove
  request shape and error paths with no real network call; a real
  delivery is a PR-4 staging proof (Release Blockers #1) — no live
  Resend/Kavenegar key exists here.
- **Sidecar contract hardened (ADR-159):** per-route timeouts
  (fatwa ask 300 s, hifz ASR 300 s, voice 180 s, receipt 60 s;
  `SIDECAR_TIMEOUT_SECS` overrides), per-route body caps (fatwa
  1 MB, audio 32 MB — the Python-equivalent error codes), streaming
  upstream bodies (SSE is no longer buffered), no retries (none of
  these POSTs is idempotent). Contract:
  `rust/app-cases/wave9/sidecar-contract.md`. Mock-sidecar tests
  prove forward-on-success, honest 503s and SSE streaming; the real
  sidecar pairing is PR-4 (Release Blockers #2).
- **Python workers (ADR-160):** verified as far as locally possible
  — real ARQ 0.28.0 + real Redis: a job enqueues with the real call
  shape and is processed end-to-end for the media queue; the real
  `app.core.queues` registry loads with the documented settings;
  the real media startup gate runs and fails loud here (tesseract +
  faster-whisper missing). The real `app.worker` processes are a
  PR-4 staging proof (Release Blockers #3). Evidence:
  `rust/app-cases/wave9/workers-verification.md`.
- **Durable storage decided + implemented (ADR-162):**
  **S3-compatible is the production provider** (local is dev/test
  only) — decided by the product's own history (the 2026-11-19
  receipt-image loss) and the original's storage policy. `storage.rs`
  dispatches local vs S3 (SigV4 over reqwest); boot validation
  (production fail-fast, ported from Python) refuses bad S3 config
  instead of silently falling back to local. Migration procedure:
  `rust/app-cases/wave9/storage-migration.md`. Bucket provisioning
  + live round-trip + migration run are PR-4 (Release Blockers #4).
- **Upload limits fixed (ADR-161) + avatar scan (ADR-164):** the axum 2 MB router default
  was silently capping every upload above 2 MB (only donate/manual
  had disabled it). The default is now disabled on exactly the
  upload routes whose handler caps exceed 2 MB, so the ported Python
  caps (library 512 MB / 5 MB, receipt/avatar/font 5 MB, audio
  32 MB) are the real limits again — restored, not raised. The scan
  gate (ADR-051) is unchanged — and the avatar upload, which still
  carried the old ClamAV gate (503 in production, no daemon in the
  deployment), now runs the same ADR-051 scanner (EICAR + blocklist,
  422 `file_infected`); the scanner is extracted into one shared
  module used by all four upload paths, and the dead
  `CLAMAV_ALLOW_INSECURE_DEV_BYPASS` flag is removed.
- **sqlx 0.8 → 0.9 (ADR-157):** sqlx 0.9.1 + libsqlite3-sys 0.40.0;
  `rsa` gone from the tree; `cargo audit` exits 0 with **no
  ignores** — both `--ignore` flags removed from `ci.yml`. The
  upgrade's one breaking change (`SqlSafeStr`) is fixed at a single
  audited choke point in `db.rs` (dynamic-SQL audit recorded there).
- **Tests:** `rust/app/tests/wave9_pr3.rs` (24 tests: mock mail/SMS
  transports, mock sidecar, SigV4 vectors + mock S3, upload-limit
  proofs) + cases `pr3-01`–`pr3-04` under `rust/app-cases/wave9/`.
  Full suite after PR-3: **363 passed, 0 failed across 19 binaries**
  (339 pre-PR-3 + 24 new), fmt + clippy clean, `cargo audit` exits
  0 with no ignores, `scripts/alembic-check.sh` PASS, release build
  green. After the ADR-168 fix (spawn-time poller watermark +
  `wave8_pg_smoke_poller_spawn_cutoff` regression test): **364
  passed, 0 failed across 19 binaries** (363 + 1 new), full suite
  re-verified sequentially with the exact CI pg_smoke loop green on
  all 17 binaries. (One suite run during PR-3 failed every pg_smoke with
  exactly 1 failure per binary — the cause was a VM replacement
  that wiped Postgres; see TOOLS.md. The code was never at fault.)
- **CI flake fix (ADR-165):** CI run #101 failed the `rust` job
  twice in `rust/app/tests/wave8_lift.rs` (attempt #1
  `wave8_pg_smoke`, attempt #2 `ws_resume_replay_from_query_and_live_message`)
  — the outbox poller's 1 s tick can broadcast a live event frame
  between the client's subscribe and the server's ack/replay/resumed,
  and the tests read a single next frame. The resume test now waits
  for the frame it expects (`wait_frame_type` / `wait_event_id`,
  same shape as `wait_event`) and skips interleaved live frames;
  assertions unchanged. Test-only fix; the poller/WS server are
  untouched.
- **CI flake fix 2 (ADR-166):** `wave8_pg_smoke` failed a third
  time in CI (push run 45181799404 attempts #1 and #2 on HEAD
  80f6056, same `"live event on pg"` panic; the PR run on the same
  HEAD was all green and the ADR-165 test passed in the failing
  run), so no third blind re-run: its live-event wait goes from
  5 s to 15 s, justified by measurement (live event arrives
  ~45 ms after the insert locally, max 47 ms over 5 runs) — 3x
  the failing budget, ~330x local latency. Watermark-tie hazard
  checked and ruled out; no src/ change.
- **CI flake diagnosis (ADR-167):** the 15 s wait did NOT fix it —
  `wave8_pg_smoke` failed a 4th and 5th time in CI (push
  45204668807 + PR 45204683344 on HEAD a77db2c, both runs, same
  panic). A diagnosis pass could not reproduce it locally (6/6
  under full CPU saturation) and ruled out watermark tie,
  heartbeat timeout, CPU load, unparseable frames, and wrong
  DB/channel/visibility; the CI log shows the binary finished in
  19.95 s (consistent with a ~5 s baseline + the full 15 s wait,
  but not proof of a genuine timeout). `wait_event` collapsed
  timeout / stream-close / unparseable-frame into one `None`, so
  the failure mode was unobserved — it now logs which path fired
  and after how long (plus the wait's elapsed time at the
  `expect` site). Budgets and assertions unchanged; the budget
  is never raised again as a fix — the next red run decides
  between the poller-side and the WS-close-side investigation.
- **CI flake fix 3 (ADR-168):** the 15 s wait did NOT fix it either —
  `wave8_pg_smoke` failed again in CI on HEAD bf94882 (push run #115
  + PR run #116, both runs, same panic) — but the temporary
  `[poller-diag]` logging (one commit, reverted here) proved the
  mechanism from the CI log: the poller's first run was 3.54 s after
  spawn (locally 3.6 ms) and its watermark captured the test's own
  `user.login` event, so all 286 ticks fetched the same 4 rows and
  sent 0. The watermark was captured inside the spawned task, so a
  first poll delayed past an insert drops that event forever. The
  fix is in the binary: the cutoff is now the spawn instant
  `(spawned_at, Uuid::nil())`, captured synchronously before
  spawning — any row created after spawn is delivered even if the
  first poll runs seconds late. The sentinel idea is not adopted
  (it only moves the failure line). Proven by a deterministic
  regression test (`wave8_pg_smoke_poller_spawn_cutoff`: block the
  thread 2 s, insert from a second thread, assert delivery — fails
  on the old code, passes with the fix). The regression test is the
  file's second pg smoke, so both pg tests hold a tokio mutex for
  their whole body — they share one DB and both drop/re-create the
  schema. The temporary logging is
  reverted in the same change.

### PR-4 Staging Deployment (not started)

PR-4 works through **`rust/RELEASE_BLOCKERS.md`** with evidence:
new Railway project, independent Postgres + Redis, migrations on
the test DB, sidecar + workers running, Rust deploy, cutover
preflight, end-to-end tests, load test + FastAPI comparison,
backup/restore + rollback test. Until the list is empty, no DNS,
live domain, or real user traffic changes.

## Units (status)

All units are ported (Waves 1–8); the plan is complete. The per-wave
sections above are the record. Remaining work is Wave 9 (production
readiness), not porting.

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
