# Wave 5 HTTP cases (for `app record` / `app replay`)

Recorded request/response pairs for the natively ported Wave 5 routes
(videos public + admin, voice admin settings/usage/TTS download).
Each file is one case: method, path, request body (or `null` for none),
expected status, and the key response fields pinned by
`rust/app/tests/wave5.rs`. Bodies use placeholder values where the real
ones are secrets or generated (`<uuid>`, `<cursor>`).

Auth note: `Authorization: Bearer <access>` comes from
`POST /api/v1/auth/login` after `POST /api/v1/auth/register`. The admin
cases need a user with a role carrying `library.manage` (videos admin)
or `voice.manage` (voice admin) — see `grant` in the tests; the role
must NOT be `admin` / `super_admin`, or the token needs TOTP enrolment
first.

Notes pinned by the cases:
- Hidden/unpublished items answer **404, never 403** (library rule).
- The public videos list uses the `after` cursor parameter, but the
  admin voice usage log uses `cursor` (Python names them differently).
- `LocalizedText` always needs all three languages (fa+ar+en).
- The subtitle VTT route is registered as `{lang_vtt}` because matchit
  allows only one capture per segment — the `.vtt` suffix is stripped
  in the handler; a non-`.vtt` name answers 404 `subtitle_not_found`.
- `POST /api/v1/voice/transcribe` and `POST /api/v1/voice/ask` are
  intentionally NOT native (ADR-046: no STT engine is embedded in the
  Rust binary; STT quality must never be faked) — in Wave 5 they fell
  through to the proxy, so with a dead origin they answered 502
  `bad_gateway`. Since Wave 8 they are sidecar-contract routes
  (ADR-134): explicit forwarding to `SIDECAR_URL`, honest 503
  `voice_stt_unavailable` when it is unset/unreachable.
- Voice settings are raw strings (ADR-049): a stored int setting comes
  back as a string (e.g. `"60"`), while an unset key comes back as its
  typed default (e.g. `120`).
- The subtitle worker is a Python sidecar (ADR-045): the generate
  trigger only marks rows `pending` and honestly reports
  `enqueued: false`.
