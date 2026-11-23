# Wave 9 app-cases

HTTP request/response cases for the endpoints the cutover preflight's
Step 7 probes (`scripts/cutover.sh`, `RUST_API_URL` set). Each file is
one request with the expected status and the key response fields; they
are documentation/fixture cases, not an executable suite — the
executable coverage lives in `rust/app/tests/wave9_pr1.rs` (PR-1) and
`rust/app/tests/wave9_pr2.rs` (PR-2).

- `pr1-01-health.json` — `GET /health` → 200 `{"status": "ok"}`.
- `pr1-02-ready.json` — `GET /ready` → 200 `{"status": "ready", "db": "ok"}`.
- `pr1-03-health-v1.json` — `GET /api/v1/health` → 200, `media_tools.ready`
  false, `missing` the five tools (ffmpeg, ffprobe, tesseract, pdftoppm,
  faster-whisper) — ADR-137.
- `pr1-04-metrics.json` — `GET /metrics` → 200 native Prometheus
  exposition (ADR-136).
- `pr1-05-openapi.json` — `GET /openapi.json` → 200, 471 route+method
  combos (ADR-139/148).
- `pr1-06-static-404.json` — `GET /static/__cutover_probe__.woff2` → 404
  `http_error` (ADR-140).
- `pr1-07-docs-404.json` — `GET /docs` → 404 (dev-only, ADR-139).
- `pr1-08-unknown-404.json` — `GET /__cutover_unknown_path__` → 404
  `http_error` envelope (ADR-133).
- `pr1-09-sidecar-503s.json` — `POST /api/v1/fatwa/ask` with `SIDECAR_URL`
  unset → 503 `fatwa_ask_unavailable`; the file also lists the other
  seven sidecar routes and their codes (ADR-134).

PR-2 cases (ADR-149): `/ready` against live and dead Redis.

- `pr2-01-ready-redis-ok.json` — `GET /ready` with a live Redis → 200
  `{"status": "ready", "db": "ok", "redis": "ok"}`.
- `pr2-02-ready-redis-unavailable.json` — `GET /ready` with a dead
  Redis → still 200 `{"status": "ready", "db": "ok", "redis":
  "unavailable"}` — Redis never fails readiness.

PR-3 cases (ADR-158–163): the executable coverage lives in
`rust/app/tests/wave9_pr3.rs`.

- `pr3-01-mail-unconfigured.json` — mail/SMS with no credentials:
  the auth flow still succeeds (token stored) but delivery is an
  honest logged error, never a claimed success (ADR-158).
- `pr3-02-sidecar-timeout-and-caps.json` — forwarder caps and
  timeouts: 2 MB fatwa body → 413 `request_too_large`; 33 MB audio
  → 413 `hifz_audio_too_large`; a sleeping sidecar → 503 with the
  route's code (ADR-159).
- `pr3-03-s3-presigned-url.json` — S3 presigned URLs are SigV4
  query-signed against the public endpoint; bad S3 config in
  production refuses to boot instead of falling back to local
  (ADR-162).
- `pr3-04-upload-over-2mb.json` — a 3 MB avatar upload reaches its
  handler (5 MB cap) instead of 413'ing at the router's 2 MB
  default (ADR-161).

PR-3 contract docs (not request/response cases):

- `sidecar-contract.md` — the exact contract for the 8 sidecar
  routes (shapes, timeouts, caps, error mapping, no-retries rule).
- `workers-verification.md` — what PR-3 proved about the Python
  ARQ workers locally, and what only staging can prove.
- `storage-migration.md` — the local → S3 migration procedure.
