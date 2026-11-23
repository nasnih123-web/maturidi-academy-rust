# Sidecar contract — the 8 routes that need the Python toolchain

Wave 9 PR-3 (ADR-159). The Rust binary owns no LLM, no ASR engine and
no PDF renderer, so these 8 routes forward to the Python sidecar
(`SIDECAR_URL`) and nothing else does. The old proxy fallback is gone
(ADR-133); this contract is the whole relationship between the two
processes. The Python side is the original repo's API, unmodified —
this document states what it must serve, so staging (PR-4) can verify
each route against the real sidecar instead of a mock.

## Rules common to all 8 routes

- **Forwarding is verbatim**: same method, path, query, headers
  (except `Host`, which reqwest sets for the sidecar, and the
  hop-by-hop headers `Content-Length` / `Transfer-Encoding` /
  `Connection`, which the streaming body owns) and the same body.
  Status, headers and body come back verbatim.
- **Auth is enforced by the sidecar, not by the forwarder.** The
  `Authorization` header is forwarded untouched; the forwarder does
  not decode it. With no sidecar configured, the 503 below is
  returned *before* any auth check — the client learns the route is
  unavailable, not whether its token was valid.
- **No retries.** Every one of these is a POST (except the receipt
  GET) whose execution is not idempotent — a grade consumes an
  attempt, a transcribe/ask consumes AI quota, a fatwa ask starts an
  LLM run. A retry could double-charge or double-record. One attempt;
  on failure the honest 503 below.
- **Timeouts are per-route** (below) and bound the whole upstream
  call, streaming included. A timeout is a 503 with the route's code,
  never a 502 and never a partial body. `SIDECAR_TIMEOUT_SECS` (env,
  seconds, `0` = per-route defaults) overrides every route's timeout
  at once — an operations knob and the knob the tests use to prove
  the timeout path without waiting minutes.
- **Body caps are enforced by the forwarder before any byte is sent
  upstream**, so a malicious client cannot make the Rust process
  buffer or forward an unbounded body. Over-cap answers carry the
  Python-equivalent error code (below), not a generic 413.
- **Streaming is never buffered.** `POST /api/v1/fatwa/ask?stream=true`
  returns `text/event-stream`; the forwarder streams the upstream
  body chunk-by-chunk (`Body::from_stream`) instead of collecting it,
  so the first SSE event reaches the client while the LLM is still
  generating. The other routes' responses are small and also stream;
  there is no code path that buffers a whole upstream body.
- **Unset or unreachable sidecar → 503** with the route's code
  (below) and the Persian message `سرویس جانبی در دسترس نیست`.
  Never a 502, never a faked success, never a fallback to a local
  implementation that does not exist.

## Per-route contract

| # | Route | Request (Python shape) | Response (Python shape) | Timeout | Body cap / over-cap | 503 code |
|---|---|---|---|---|---|---|
| 1 | `POST /api/v1/fatwa/ask` | JSON `AskIn`: `question` 1–4000 chars, optional `session_id` UUID, `locale` fa/ar/en (default fa), `top_k` 1–10 (default 5). `?stream=true` → SSE | `AskOut`: `session_id`, `answer`, `status` (`answered` \| `llm_unavailable` \| `no_sources`), `citations`, `ai_label`, `model`. SSE: the final event carries the full `AskOut`, then `[DONE]` | 300 s | 1 MB / 413 `request_too_large` | `fatwa_ask_unavailable` |
| 2 | `GET /api/v1/finance/receipts/{donation_id}` | No body. Access: the donation owner or `finance.manage` (enforced by the sidecar) | Trilingual receipt PDF, `application/pdf`, `Content-Disposition: attachment` | 60 s | — (GET, no body) | `receipt_pdf_unavailable` |
| 3 | `POST /api/v1/quran/hifz/attempts/{attempt_id}/grade` | multipart: `surah` 1–114, `ayah` 1–286, `audio` file. Audio is transient, never stored | `HifzGradeOut`: `attempt_id`, `surah`, `ayah`, `word_index?`, `engine_used`, thresholds green ≥0.8 / amber 0.5–0.8, `timing_source` phoneme/dtw_fallback/none, `words`, counts | 300 s | 32 MB / 413 `hifz_audio_too_large` | `hifz_asr_unavailable` |
| 4 | `POST /api/v1/quran/hifz/attempts/{attempt_id}/grade-word` | As #3 plus `word_index` ≥ 0 | `HifzGradeOut` (same shape) | 300 s | 32 MB / 413 `hifz_audio_too_large` | `hifz_asr_unavailable` |
| 5 | `POST /api/v1/quran/hifz/tests/{test_id}/questions/{question_id}/answer` | multipart audio | `HifzTestAnswerOut`: `question_id`, `status`, `score?`, counts, `peek_count`, `raw_events` | 300 s | 32 MB / 413 `hifz_audio_too_large` | `hifz_asr_unavailable` |
| 6 | `POST /api/v1/quran/hifz/voice-search` | multipart audio | `HifzVoiceSearchOut`: `engine_used`, `level`, `candidates`, `note`. Results are probabilistic, not definitive | 300 s | 32 MB / 413 `hifz_audio_too_large` | `hifz_asr_unavailable` |
| 7 | `POST /api/v1/voice/transcribe` | multipart: `file`, optional `lang`. AI quota `voice.transcribe` is applied by the sidecar *before* processing | `TranscribeOut`: `text`, `language`, `duration_s`, `stt_ms`, `provider` | 180 s | `voice.max_audio_mb` setting / 422 `audio_too_large` | `voice_stt_unavailable` |
| 8 | `POST /api/v1/voice/ask` | multipart audio. AI quota `voice.ask` is applied by the sidecar first | `AskOut`: `intent` quran/fatwa/general, `lang` fa/ar, `text`, `transcript?`, `audio_kind` recitation/tts/none, `audio_url`/`base64`/`mime?`, `notice?`, `ayah?`. Quran intent returns only a real recitation URL, never TTS audio | 180 s | `voice.max_audio_mb` setting / 422 `audio_too_large` | `voice_stt_unavailable` |

Timeout reasoning (ADR-159): the sidecar's own work is slow — LLM
generation for #1, faster-whisper on up to ~15 minutes of audio for
#3–#6 — so the timeouts are generous; they exist to bound a hung
sidecar, not to rush a slow one. The 300 s ASR figure covers the
32 MB cap at the slowest plausible transcription speed with margin;
the 180 s voice figure covers the same for the shorter voice clips.
If staging shows real p99 latencies above these, the numbers move in
the contract (and the ADR), not silently in code.

Body-cap reasoning: the 32 MB audio cap is Python's
`MAX_AUDIO_BYTES` (hifz, ~15 min mono 16 kHz WAV); the voice cap is
the `voice.max_audio_mb` setting read by the sidecar — the forwarder
cannot read that setting, so it enforces the same 32 MB ceiling and
lets the sidecar apply the tighter configured cap itself. The 1 MB
fatwa cap is a forwarder-side DoS guard (a 4000-char question is
~16 KB; 1 MB is 60× headroom).

## What the Python sidecar must serve (staging checklist, PR-4)

For each route above, staging must prove against the *real* sidecar
(not the mock in `wave9_pr3.rs`): the route exists, the shapes match,
the SSE stream arrives event-by-event for `?stream=true`, the quota
is consumed before processing for #7/#8, and the 503 codes above are
what the Rust binary returns when the sidecar is stopped. The mock
tests prove the forwarder; only staging proves the pair.

## What PR-3 changed in the forwarder (vs Wave 8)

- Per-route timeouts (was: the shared 30 s client timeout for all
  routes — too short for LLM/ASR, and the same for a PDF as for a
  15-minute transcription).
- Per-route body caps (was: `to_bytes(body, usize::MAX)` —
  unbounded; and the axum router's 2 MB default body limit was
  silently capping the audio routes below their 32 MB contract —
  see ADR-161).
- Streaming upstream bodies (was: `upstream.bytes().await` buffered
  the whole response, which broke SSE for `?stream=true`: no event
  could reach the client until generation finished).
- No retries (was: also none — now stated as a rule, not an
  accident, because a future "add retries" change must confront the
  idempotency argument above).
