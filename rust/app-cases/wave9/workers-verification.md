# Python workers + queue processing — what PR-3 verified, and what only staging can

Wave 9 PR-3 (ADR-160). The Rust binary never runs a worker and never
enqueues an ARQ job (ADR-125): every trigger returns
`enqueued: false` and the Python sidecar API owns enqueueing. The
workers live entirely in the original repo and run as separate
processes in deployment. This document records the setup, what was
proven locally in PR-3, and the exact list staging (PR-4) must still
prove. Nothing here claims a worker was verified that was not run.

## Setup (verified from the original code, PR-1 + PR-3)

- Entry point: `python -m app.worker <queue>` (`apps/api/app/worker.py`).
  Unknown queue → exit 2. One process per queue.
- Six ARQ queues (`apps/api/app/core/queues.py`, arq 0.28.0):

  | Queue | max_jobs | max_tries | job_timeout | Functions | Cron |
  |---|---|---|---|---|---|
  | critical | 10 | 5 | 300 s | — (cron only) | `islamic-calendar-daily` 00:05, `task-due-soon` every 6 h |
  | ai | 2 | 3 | 600 s | `ingest_knowledge_run` | — |
  | media | 4 | 3 | 1800 s | `transcribe_media`, `ocr_pdf`, `generate_subtitles` | — |
  | telegram | 4 | 5 | 300 s | `telegram_import_channel`, `telegram_publish_message` | — |
  | notifications | 8 | 5 | 120 s | — (cron only) | task-due-soon (see critical) |
  | analytics | 4 | 3 | 120 s | — (cron only) | build-sync cron |

  (Verified by loading the real `app.core.queues` module in PR-3:
  the six queues and these settings are what the code actually
  returns from `worker_settings_for()`.)
- Registration: job functions register with `@register_task(queue)`
  in their modules (`library/tasks.py`, `videos/tasks.py`,
  `telegram/jobs.py`, `fatwa/ingestion.py`); cron jobs with
  `@register_cron(queue, ...)`. The worker entry point imports those
  modules for their registration side effects.
- Enqueue sites (Python sidecar API only): `fatwa/service.py` →
  `ai` (`ingest_knowledge_run`, `_queue_name="ai"`),
  `library/service.py` → `media` (`_queue_name="media"`),
  `telegram/distribute_service.py` → `telegram`
  (`_queue_name="telegram"`). All via
  `arq.create_pool(RedisSettings.from_dsn(redis_url))`.
- Dispatcher: **in-process** in the Python sidecar API lifespan
  (`apps/api/app/core/dispatcher.py`) — it is not a seventh worker.
  The Rust `/ws/events` poller only *reads* `domain_events` and never
  marks rows processed.
- Deployment shape: `railway.worker-*.toml` (six files, at the
  `apps/api` root): same Docker image as the API,
  `startCommand = "python -m app.worker <queue>"`,
  `restartPolicyType = "ON_FAILURE"`, max 10 retries, no HTTP port,
  no healthcheck. Status: prepared, NOT deployed.
- Media worker startup gate: `_media_startup` runs
  `assert_media_tools_ready()` — without ffmpeg, tesseract (+
  fas/ara/eng), poppler and faster-whisper the worker refuses to
  start (CRITICAL log + `RuntimeError`), instead of silently burning
  queued jobs. This is the fail-loud gate, by design (ADR-0027 in
  the original).

## What PR-3 proved locally (2026-11-23)

All of it ran on this VM with a real Redis 7.0.15 (local
`redis-server`, port 6380 — deliberately NOT 6379, so the running
Rust test suite's rate limiter was untouched) and real ARQ 0.28.0
(the exact pinned version), in a throwaway venv
(`/tmp/pr3-workers-venv`, not committed). Proof script:
`/tmp/pr3-arq-proof.py` (kept in the PR-3 report, not in the repo —
it is a scratch verification, not a product test).

1. **Enqueue → process → result, end to end, for the media queue.**
   A worker with the REAL media settings
   (`queue_name="media"`, `max_jobs=4`, `max_tries=3`,
   `job_timeout=1800`) and a job function named `transcribe_media`
   was started with `python -m arq`; a separate process enqueued
   with the REAL call shape
   (`create_pool(RedisSettings.from_dsn(...))` +
   `enqueue_job("transcribe_media", ..., _queue_name="media")` —
   the same call `library/service.py:379` makes). The worker picked
   the job up in 0.20 s, ran it, and the enqueuer received the
   result (`{'item_id': 'pr3-proof-item', 'language': 'fa',
   'status': 'processed'}`) plus a matching on-disk marker. First
   attempt without `_queue_name` timed out — the job landed on arq's
   default queue and the media worker never saw it — which itself
   proves the queue routing is real, not a shared pool.
2. **The real queue registry loads.** `app.core.queues` imports with
   only pydantic/pydantic-settings installed, and
   `worker_settings_for()` returns exactly the six queues and the
   settings in the table above.
3. **The real media startup gate runs and fails loud here.**
   `app.core.media_tools.media_tools_status()` on this VM:
   ffmpeg `/usr/bin/ffmpeg`, ffprobe present, pdftoppm present,
   **tesseract: missing, faster-whisper: missing**,
   `ready: False`. `assert_media_tools_ready()` raises
   `RuntimeError: media tools missing: tesseract, faster-whisper`
   with the CRITICAL log — exactly the designed behaviour. So the
   real media worker *cannot* start on this VM, and PR-3 does not
   pretend otherwise.

## What PR-3 did NOT prove (staging must — PR-4)

- The **real** `python -m app.worker <queue>` processes. Importing
  `app.worker` here stops at the full app dependency tree
  (`pydantic_settings` → `sqlalchemy` → `fastapi` → … → the whole
  `requirements.txt`, including faster-whisper/torch). Only the
  deployed image has all of it. The ARQ proof above proves the
  queue machinery; it does not prove the real job functions run.
- The real job functions: `transcribe_media` (faster-whisper),
  `ocr_pdf` (tesseract), `generate_subtitles`,
  `ingest_knowledge_run`, the telegram jobs — each needs its
  toolchain and its database rows. Staging must enqueue one real job
  per queue and watch it complete in the worker logs.
- The six `railway.worker-*.toml` services actually starting on the
  new Railway project, with the right image and restart policy.
- Cron jobs firing on schedule (critical / notifications /
  analytics queues).
- The dispatcher in the sidecar API lifespan marking `domain_events`
  rows processed, and the Rust WS poller delivering them.
