# Wave 8 app-cases

HTTP request/response cases for the Wave 8 native routes. Each file is
one request with the expected status and the key response fields; they
are documentation/fixture cases, not an executable suite — the
executable coverage lives in `rust/app/tests/wave8_*.rs`.

<!-- BEGIN PROJECTS (Wave 8, projects module) -->

## Projects (`proj-*`, 13 cases)

- `proj-01-project-create.json` — `POST /api/v1/projects` with
  `projects.manage` → 201, `task_count` 0, `status` active.
- `proj-02-projects-list-unauthorized.json` — `GET /api/v1/projects`
  without a token → 401 `unauthorized`.
- `proj-03-projects-list-plain-user-forbidden.json` — `GET
  /api/v1/projects` as a plain authenticated user (no permission, no
  staff row) → 403 `forbidden` (staff-only).
- `proj-04-projects-list-bad-cursor.json` — `GET /api/v1/projects`
  with a malformed cursor → 400 `bad_cursor`.
- `proj-05-project-create-bad-status.json` — `POST /api/v1/projects`
  with an unknown status → 422 `validation_error`.
- `proj-06-project-get-missing.json` — `GET
  /api/v1/projects/{unknown}` → 404 `project_not_found`.
- `proj-07-task-create-assignee-not-found.json` — `POST
  /api/v1/projects/{id}/tasks` with an unknown assignee → 404
  `assignee_not_found`.
- `proj-08-task-move-illegal-edge.json` — `POST
  /api/v1/projects/tasks/{id}/move` from `backlog` to `review` → 422
  `invalid_status_transition` with `{from, to, allowed: ["todo"]}`.
- `proj-09-tasks-by-entity-missing-param.json` — `GET
  /api/v1/projects/tasks/by-entity` without `related_entity_id` →
  422 `validation_error`.
- `proj-10-staff-create-duplicate.json` — `POST /api/v1/staff` for a
  user who already has a staff row → 409 `staff_exists`.
- `proj-11-staff-create-user-not-found.json` — `POST /api/v1/staff`
  for an unknown user → 404 `user_not_found`.
- `proj-12-due-soon-cycle.json` — `POST
  /api/v1/admin/projects/due-soon-cycle` with `projects.manage` →
  200, `{scanned, notified}`.
- `proj-13-due-soon-cycle-forbidden.json` — `POST
  /api/v1/admin/projects/due-soon-cycle` as a staff reader without
  the permission → 403 `forbidden`.

<!-- END PROJECTS (Wave 8, projects module) -->

<!-- BEGIN LIFT (Wave 8, proxy-removal lift) -->

## Lift (`lift-*`, 12 cases)

- `lift-01-unknown-path-404.json` — `GET /no/such/route` → 404 in
  Python's error envelope (`http_error` / `Not Found`), not
  FastAPI's default `{"detail": "Not Found"}`.
- `lift-02-metrics.json` — `GET /metrics` → 200,
  `text/plain; version=0.0.4`, all six Prometheus series present.
- `lift-03-health-v1.json` — `GET /api/v1/health` → 200, `api: v1`,
  `media_tools.ready: false` (no media toolchain in this binary).
- `lift-04-openapi.json` — `GET /openapi.json` → 200, generated
  spec listing the registered routes.
- `lift-05-static-404.json` — `GET /static/fonts/fa/does-not-exist.woff2`
  → 404.
- `lift-06-sidecar-fatwa-503.json` — `POST /api/v1/fatwa/ask` with
  `SIDECAR_URL` unset → 503 `fatwa_ask_unavailable`.
- `lift-07-sidecar-receipt-503.json` — `GET
  /api/v1/finance/receipts/{id}` with `SIDECAR_URL` unset → 503
  `receipt_pdf_unavailable`.
- `lift-08-sidecar-hifz-grade-503.json` — `POST
  /api/v1/quran/hifz/attempts/{id}/grade` with `SIDECAR_URL` unset →
  503 `hifz_asr_unavailable`.
- `lift-09-sidecar-voice-transcribe-503.json` — `POST
  /api/v1/voice/transcribe` with `SIDECAR_URL` unset → 503
  `voice_stt_unavailable`.
- `lift-10-ws-no-auth-4401.json` — `GET /ws/events` without auth →
  101 handshake, then a close frame with code 4401.
- `lift-11-docs-404.json` — `GET /docs` → 404 (`/docs` is dev-only
  in Python, so it 404s in production).
- `lift-12-kill-switch-ignored.json` — `GET /api/v1/themes/active`
  with `RUST_ANYTHING_UNIT_META=original` set → 200; the retired
  kill switch changes nothing.

<!-- END LIFT (Wave 8, proxy-removal lift) -->

<!-- BEGIN REPLAY (Wave 8, executable replay of wave2..wave7 cases) -->

## Replay (`rust/app/tests/wave8.rs`, 127 cases)

The wave2..wave7 case files are now executable. `replay_all_app_cases`
walks `rust/app-cases/wave2..wave7/**/*.json` in order and replays each
case against the pure-Rust router (`build_router`, fresh in-memory
sqlite `TEST_SCHEMA` per case), with `wave8_pg_smoke` (gated on
`WAVE2_PG_URL`) replaying the lift-critical subset on Postgres. Wave 8
cases (`proj-*`, `lift-*`) are NOT replayed here; they are covered by
`wave8_projects.rs` / `wave8_lift.rs`.

- Auth is provisioned from the case's `auth` label: a user is seeded
  by SQL and an HS256 token is minted with the same claims shape as
  `jwt::create_access_token`; permissions are granted through a fresh
  non-privileged `editor` role (never `admin`/`super_admin`, so no
  TOTP enrolment is triggered). A bare permission name grants exactly
  that permission; `admin (…)` grants the listed permissions;
  `moderator` grants `social.moderate`; the telegram webhook case
  sets `telegram.webhook_secret` in settings and sends it as the
  `X-Telegram-Bot-Api-Secret-Token` header.
- The per-file seed hook (`seed`) supplies what the (b)-tier cases
  need: rows (videos, courses/lessons/cohorts, library items, posts,
  reports, conflicts, changelog rows, banned words, quran settings,
  telegram policy/secret), a real API key id for the embed-snippet
  path, a registered user for the login case, and a newer stored
  reading state for the stale-PUT case.
- Placeholders are substituted mechanically before sending: `<uuid>`
  (fresh, or the seeded row id), `<password>` → `ReplayPass123!`,
  `<post_id>` / `<report_id>` / `{item_id}` → seeded ids, `<jwt>` →
  the case's token, `<opaque>` → a fixed non-empty string. Two
  bodies are prose and handled specially: wave5 12/13 send an EMPTY
  body (the sidecar handler 503s before reading it); wave7 fin-07
  sends the raw string `not json`.
- Check semantics: `expected_status` must match EXACTLY; then
  `expected` is subset-matched against the response JSON (every key
  must exist; objects recurse; numbers compare numerically;
  non-empty arrays compare element-wise up to the expected length;
  empty arrays compare exactly). `<uuid>` = any valid UUID,
  `<cursor>` = any non-empty string, `<password>` / `<jwt>` /
  `<opaque>` / `<cert-code>` / `<mk_...>` = any non-empty string,
  `<N items>` = an array of exactly N, `<string|null>` = string or
  null, prose `"<...>"` = the key exists with a non-empty value of
  its own type (non-empty array for `array…`, integer for
  `integer…`, null only when the prose mentions `null`); a `note`
  key is case-file annotation and is skipped, as is an `absent`
  prose value (the key must NOT exist). `expected_content_type`,
  when present, must prefix-match the response content-type.
  Overrides: wave2/12 html must contain `https://example.com` and
  `اوقات شرعی` and must NOT contain `mk_`; wave4/08
  `last_read.surah` must be 2 after the stale PUT.

### Results (2026-11-22, first full replay)

| wave | passed | failed | skipped |
|------|--------|--------|---------|
| wave2 | 10 | 1 | 3 |
| wave3 | 14 | 0 | 0 |
| wave4 | 15 | 0 | 0 |
| wave5 | 14 | 0 | 0 |
| wave6 | 17 | 1 | 2 |
| wave7 | 49 | 1 | 0 |
| **total** | **119** | **3** | **5** |

Skipped (c-tier, documentation-only as written): wave2/08
(refresh token from a prior login), wave2/09 (email-verification
token issued at registration), wave2/10 (TOTP secret + live code),
wave6/lib-05 (path is a `{media_key}&exp={exp}&sig={sig}` template —
a signature must be minted first), wave6/lib-06 (body is a prose
multipart description, not sendable bytes).

Failed (3) — all three are case-file expectation drift, verified
against the original Python API and the wave tests; the Rust code
matches Python in each case, so no Rust fix applies and the case
files were left untouched (only the parent updates case files):

- `wave2/01-meta-settings-public.json` — expects flat keys
  `places.globe.defaultLat` / `places.globe.defaultLng` and a
  `push.vapid_public_key: null` key. Python returns `places.globe`
  as a nested object (`_PUBLIC_DEFAULTS` in
  `apps/api/app/modules/meta/router.py`) and has no
  `push.vapid_public_key` in the public response at all.
- `wave6/lib-04.json` — expects a `total` key. Python's
  `search_items` (`apps/api/app/modules/library/service.py`)
  returns `{"query", "hits"}` only.
- `wave7/syn-08-publish.json` — expects `{"items": [...]}`.
  Python's publish endpoint returns `list[PublishedOut]` (a bare
  array), as does the Rust handler.

<!-- END REPLAY (Wave 8, executable replay of wave2..wave7 cases) -->
