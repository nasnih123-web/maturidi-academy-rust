# Wave 9 PR-1 app-cases

HTTP request/response cases for the endpoints the cutover preflight's
Step 7 probes (`scripts/cutover.sh`, `RUST_API_URL` set). Each file is
one request with the expected status and the key response fields; they
are documentation/fixture cases, not an executable suite — the
executable coverage lives in `rust/app/tests/wave9_pr1.rs`.

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
