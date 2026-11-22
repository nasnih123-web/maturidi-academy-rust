# Wave 4 HTTP cases (for `app record` / `app replay`)

Recorded request/response pairs for the natively ported Wave 4 routes
(quran reader, providers, notes, reading-state, tasbih, adhan, hifz).
Each file is one case: method, path, request body (or `null` for none),
expected status, and the key response fields pinned by
`rust/app/tests/wave4.rs`. Bodies use placeholder values where the real
ones are secrets or generated (`<uuid>`).

Auth note: `Authorization: Bearer <access>` comes from
`POST /api/v1/auth/login` after `POST /api/v1/auth/register`. The custom
provider key case needs a user with a role carrying
`platform.settings` (see `grant` in the tests); the role must NOT be
`admin` / `super_admin`, or the token needs TOTP enrolment first.

Offline note: the built-in provider defaults point at AlQuran Cloud
(translations `en.sahih`/`fa.makarem`, tafsir `ar.muyassar`, reciter
audio `ar.alafasy`). The surah/ayah cases therefore either pass bundled
editions explicitly (`?translation=fa.rowwad&tafsir=fa.mokhtasar`) or
require the settings `quran.translations=["fa.rowwad"]`,
`quran.tafsirs=["fa.mokhtasar"]`, `quran.reciters=[]` (as the tests do
via `set_setting`). Without that, the same calls answer 503
`quran_provider_unavailable` with `{"provider": "alquran_cloud"}` —
that is the honest behaviour, not a bug.

Notes pinned by the cases:
- The Tanzil text prefixes every surah's first ayah with the basmala
  (except surah 9), so the mutashabihat group for
  "بسم الله الرحمن الرحيم" has 114 occurrences, not 2.
- `PUT /api/v1/quran/reading-state` with an older `updated_at` answers
  409 with the stored state as the body, not the error envelope.
- Notes list pagination uses the `after` cursor parameter, not
  `cursor`.
- `POST /api/v1/quran/hifz/attempts/{id}/grade`,
  `grade-word`, `voice-search`, and test `answer` are intentionally NOT
  native (ADR-031: no ASR engine is embedded in the Rust binary; ASR
  quality must never be faked) — in Wave 4 they fell through to the
  proxy, so with a dead origin they answered 502 `bad_gateway`. Since
  Wave 8 they are sidecar-contract routes (ADR-134): explicit
  forwarding to `SIDECAR_URL`, honest 503 `hifz_asr_unavailable`
  when it is unset/unreachable.
- The live WebSocket stream (`GET .../attempts/{id}/stream`) is
  registered explicitly as 503 `hifz_live_unavailable` (no sidecar
  route covers it; use the Python sidecar directly).
- `GET /api/v1/quran/hifz/status` honestly reports the selected engine
  as unavailable (`available: false`) for the same reason.
- Test-passage sampling uses SplitMix64, not CPython's Mersenne
  Twister (ADR-033): same seed and wire shape, sampled passages may
  differ from Python's.
