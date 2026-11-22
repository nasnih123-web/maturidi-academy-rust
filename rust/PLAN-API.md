# Rust migration plan: api (Python)

1 package, 270 source files (67350 lines), 128 test files, 0 UI files, grouped into 30 units.

| Ready | Caution | Blocked | Keep in source language |
| ---: | ---: | ---: | ---: |
| 2 | 3 | 25 | 0 |

## Start here

Best first ports: safe to port (readiness) and worth porting (loops and string work, tests, size).

| # | Unit | Lines | Priority | Readiness | Tests | Bridge | Main risks |
| ---: | --- | ---: | ---: | ---: | ---: | --- | --- |
| 1 | `alembic/` | 43 | 62 | 98 | 0 | PyO3 | Async and promises (5) |
| 2 | `scripts/` | 753 | 47 | 50 | 0 | PyO3 | Hash iteration order (8); String offsets and slicing (3); Dynamic typing (3) |
| 3 | `alembic/versions/` | 3898 | 13 | 0 | 0 | PyO3 | Number semantics (186); Exceptions (1) |

### `alembic/`

43 lines in 1 file, wave 1, priority 62, readiness 98, bridge **PyO3**.

- 1 imports use aliases or virtual modules and were not followed.
- Async and promises x5: `alembic/env.py:41`, `alembic/env.py:48`, `alembic/env.py:48`

### `scripts/`

753 lines in 4 files, wave 1, priority 47, readiness 50, bridge **PyO3**.

- Hash iteration order x8: `scripts/import_qul_tafsir.py:55`, `scripts/import_qul_tafsir.py:55`, `scripts/import_qul_tafsir.py:65`
- String offsets and slicing x3: `scripts/generate_reciter_timings.py:343`, `scripts/import_qul_tafsir.py:176`, `scripts/import_qul_tafsir.py:186`
- Dynamic typing x3: `scripts/generate_quran_fa_translations.py:91`, `scripts/generate_reciter_timings.py:343`, `scripts/import_quranenc_tafsir.py:81`
- Division and modulo x3: `scripts/generate_reciter_timings.py:304`, `scripts/generate_reciter_timings.py:352`, `scripts/import_quranenc_tafsir.py:111`
- Exceptions x20: `scripts/generate_quran_fa_translations.py:55`, `scripts/generate_quran_fa_translations.py:61`, `scripts/generate_quran_fa_translations.py:63`
- Number semantics x3: `scripts/generate_reciter_timings.py:141`, `scripts/import_qul_tafsir.py:74`, `scripts/import_quranenc_tafsir.py:72`
- Clock and random numbers x3: `scripts/generate_reciter_timings.py:178`, `scripts/generate_reciter_timings.py:288`, `scripts/generate_reciter_timings.py:362`

### `alembic/versions/`

3898 lines in 50 files, wave 0, priority 13, readiness 0, bridge **PyO3**.

- 1 PyPI dependencies not in the crate table: check them by hand.
- 50 imports use aliases or virtual modules and were not followed.
- Leaf: imports no other unit.
- Number semantics x186: `alembic/versions/2e6a9529d1a0_phase10_hifz_plans_srs_tests_tasmi.py:25`, `alembic/versions/2e6a9529d1a0_phase10_hifz_plans_srs_tests_tasmi.py:26`, `alembic/versions/2e6a9529d1a0_phase10_hifz_plans_srs_tests_tasmi.py:26`
- Exceptions x1: `alembic/versions/b5c6d7e8f9a0_fix2_11_release_group_retention.py:68`

## Order by package

Packages come after the workspace packages they use. Inside a package, port by wave: a unit's imports are in earlier waves.

### 1. api (`.`, portable, 128 test files)

| Wave | Unit | Status | Lines | Readiness | Bridge | Main risks |
| ---: | --- | --- | ---: | ---: | --- | --- |
| 0 | `app/modules/` | ready | 0 | 100 | PyO3 | - |
| 0 | `app/modules/places/` | blocked | 689 | 34 | PyO3 | Hash iteration order (2); Async and promises (113) |
| 0 | `app/modules/home/` | blocked | 813 | 17 | PyO3 | String offsets and slicing (5); Hash iteration order (1) |
| 0 | `app/modules/sync/` | blocked | 1131 | 12 | PyO3 | String offsets and slicing (3); Hash iteration order (1) |
| 0 | `app/modules/users/` | blocked | 1127 | 12 | PyO3 | String offsets and slicing (3); Hash iteration order (2) |
| 0 | `app/modules/social/` | blocked | 1112 | 11 | PyO3 | String offsets and slicing (6); Hash iteration order (2) |
| 0 | `app/modules/content/` | blocked | 1452 | 8 | PyO3 | String offsets and slicing (1); Hash iteration order (1) |
| 0 | `app/models/` | caution | 2690 | 3 | PyO3 | Class inheritance (119); Number semantics (315) |
| 0 | `alembic/versions/` | caution | 3898 | 0 | PyO3 | Number semantics (186); Exceptions (1) |
| 0 | `app/core/` | blocked | 5798 | 0 | PyO3 | Hash iteration order (58); String offsets and slicing (6) |
| 0 | `app/modules/admin/` | blocked | 6244 | 0 | PyO3 | String offsets and slicing (35); Hash iteration order (17) |
| 0 | `app/modules/auth/` | blocked | 1969 | 0 | PyO3 | Hash iteration order (5); String offsets and slicing (2) |
| 0 | `app/modules/fatwa/` | blocked | 4885 | 0 | PyO3 | String offsets and slicing (17); Hash iteration order (10) |
| 0 | `app/modules/library/` | blocked | 2140 | 0 | PyO3 | String offsets and slicing (13); Hash iteration order (11) |
| 0 | `app/modules/quran/` | blocked | 13898 | 0 | PyO3 | String offsets and slicing (88); Hash iteration order (46) |
| 0 | `app/modules/telegram/` | blocked | 1803 | 0 | PyO3 | String offsets and slicing (10); Hash iteration order (1) |
| 0 | `app/modules/videos/` | blocked | 2344 | 0 | PyO3 | String offsets and slicing (14); Hash iteration order (11) |
| 0 | `app/modules/voice/` | blocked | 1389 | 0 | PyO3 | String offsets and slicing (4); Regexes built at runtime (3) |
| 1 | `alembic/` | ready | 43 | 98 | PyO3 | Async and promises (5) |
| 1 | `app/modules/meta/` | blocked | 133 | 81 | PyO3 | Async and promises (7) |
| 1 | `app/modules/activity/` | blocked | 69 | 60 | PyO3 | String offsets and slicing (4); Hash iteration order (2) |
| 1 | `scripts/` | caution | 753 | 50 | PyO3 | Hash iteration order (8); String offsets and slicing (3) |
| 1 | `app/modules/api_keys/` | blocked | 789 | 32 | PyO3 | String offsets and slicing (1); Async and promises (142) |
| 1 | `app/modules/articles/` | blocked | 690 | 31 | PyO3 | Hash iteration order (1); Async and promises (154) |
| 1 | `app/modules/projects/` | blocked | 982 | 27 | PyO3 | String offsets and slicing (2); Hash iteration order (2) |
| 1 | `app/modules/calculators/` | blocked | 3678 | 0 | PyO3 | String offsets and slicing (38); Hash iteration order (5) |
| 1 | `app/modules/lms/` | blocked | 2196 | 0 | PyO3 | String offsets and slicing (6); Hash iteration order (5) |
| 2 | `app/modules/widgets/` | blocked | 325 | 42 | PyO3 | String offsets and slicing (1); Async and promises (62) |
| 2 | `app/modules/finance/` | blocked | 2493 | 0 | PyO3 | String offsets and slicing (8); Hash iteration order (1) |
| 3 | `app/` | blocked | 1817 | 0 | PyO3 | Hash iteration order (3); Runtime changes to code (2) |

## PyPI dependencies

| Package | Units | Rust | Fit | Note |
| --- | ---: | --- | --- | --- |
| `fastapi` | 25 | - | none | Web framework: keep the app in Python. |
| `starlette` | 3 | - | none | Web framework: keep the app in Python. |
| `arq` | 5 | ? | unknown |  |
| `pgvector` | 3 | ? | unknown |  |
| `reportlab` | 3 | ? | unknown |  |
| `faster-whisper` | 2 | ? | unknown |  |
| `arabic-reshaper` | 1 | ? | unknown |  |
| `argon2` | 1 | ? | unknown |  |
| `bidi` | 1 | ? | unknown |  |
| `hijri-converter` | 1 | ? | unknown |  |
| `minio` | 1 | ? | unknown |  |
| `opentelemetry` | 1 | ? | unknown |  |
| `pydantic-core` | 1 | ? | unknown |  |
| `pydantic-settings` | 1 | ? | unknown |  |
| `pyotp` | 1 | ? | unknown |  |
| `sentence-transformers` | 1 | ? | unknown |  |
| `sentry-sdk` | 1 | ? | unknown |  |
| `sherpa-onnx` | 1 | ? | unknown |  |
| `telethon` | 1 | ? | unknown |  |
| `sqlalchemy` | 28 | sqlx | partial | ORM layers port by hand. |
| `pydantic` | 22 | serde, garde | partial | Validation errors will differ. |
| `numpy` | 2 | ndarray | partial | Broadcasting and dtype rules differ; check numerics. |
| `anyio` | 1 | tokio | partial |  |
| `av` | 1 | ffmpeg-next | partial | Binds the C libraries. For pure Rust, remade-ffmpeg covers a subset of codecs (and has an ffmpeg-compatible CLI): check its codec table. |
| `cryptography` | 1 | ring, RustCrypto crates | partial |  |
| `pil` | 1 | image | partial |  |
| `httpx` | 8 | reqwest | good |  |
| `redis` | 4 | redis | good |  |
| `jwt` | 1 | jsonwebtoken | good |  |

## Blocked

- `app/`: `fastapi` has no Rust equivalent.
- `app/core/`: `fastapi` has no Rust equivalent.
- `app/modules/activity/`: `fastapi` has no Rust equivalent.
- `app/modules/admin/`: `fastapi` has no Rust equivalent.
- `app/modules/api_keys/`: `fastapi` has no Rust equivalent.
- `app/modules/articles/`: `fastapi` has no Rust equivalent.
- `app/modules/auth/`: `fastapi` has no Rust equivalent.
- `app/modules/calculators/`: `fastapi` has no Rust equivalent.
- `app/modules/content/`: `fastapi` has no Rust equivalent.
- `app/modules/fatwa/`: `fastapi` has no Rust equivalent.
- `app/modules/finance/`: `fastapi` has no Rust equivalent.
- `app/modules/home/`: `fastapi` has no Rust equivalent.
- `app/modules/library/`: `fastapi` has no Rust equivalent.
- `app/modules/lms/`: `fastapi` has no Rust equivalent.
- `app/modules/meta/`: `fastapi` has no Rust equivalent.
- `app/modules/places/`: `fastapi` has no Rust equivalent.
- `app/modules/projects/`: `fastapi` has no Rust equivalent.
- `app/modules/quran/`: `fastapi` has no Rust equivalent.
- `app/modules/social/`: `fastapi` has no Rust equivalent.
- `app/modules/sync/`: `fastapi` has no Rust equivalent.
- `app/modules/telegram/`: `fastapi` has no Rust equivalent.
- `app/modules/users/`: `fastapi` has no Rust equivalent.
- `app/modules/videos/`: `fastapi` has no Rust equivalent.
- `app/modules/voice/`: `fastapi` has no Rust equivalent.
- `app/modules/widgets/`: `fastapi` has no Rust equivalent.

## Risk guide

- **String offsets and slicing** (High): JS indexes strings in UTF-16 code units, Rust in UTF-8 bytes. Offsets that cross the boundary must be converted, or non-ASCII text gets wrong ranges silently.
- **Regex features the `regex` crate lacks** (High): Lookaround, backreferences and the `y` flag are not in the `regex` crate. Use `fancy-regex` for those patterns, or rewrite them, and test them on non-ASCII text: `\w`, `\b` and case-insensitive matching differ between engines even where a pattern compiles. JS `/i` without `u` never matches a non-ASCII letter to an ASCII one, Rust `(?i)` does (`ſ` matches `s`, the Kelvin sign `K` matches `k`): write `(?i-u)` or compare with `eq_ignore_ascii_case`.
- **Regexes built at runtime** (High): Patterns come from data or config in JS syntax. Rust regex syntax differs, so user-visible behavior can change. Pin it with tests on real patterns.
- **Dynamic typing** (Medium): Values whose shape is checked at runtime need an explicit Rust enum or `serde_json::Value`.
- **Exceptions** (Low): Thrown errors become `Result`. Errors crossing the bridge must map back to the same JS error shape.
- **Async and promises** (Medium): Promises become futures. Bridge with napi-rs `async fn` or `wasm-bindgen-futures`, or keep the async shell in JS and port the sync core.
- **Generators** (Medium): Lazy generators become iterators, or return a collected Vec across the bridge.
- **Class inheritance** (Medium): Class hierarchies become traits plus composition; `instanceof` checks become enums.
- **Reflection and prototypes** (Medium): Proxy, Reflect, Symbol and prototype tricks need a redesign, not a translation.
- **Number semantics** (Low): JS numbers are f64; bitwise ops truncate to i32; parseInt is lenient. Pick Rust integer types deliberately. Printing differs: `${n}` gives `1e+21`, `1e-7` and `0` for -0 where Rust's `{}` does not, so use `js::number_to_string`. `Math.round` rounds halves up (-2.5 gives -2), Rust's `round` away from zero: use `js::math_round`. `toFixed` rounds exact halves up ((2.5).toFixed(0) is "3"), Rust's `{:.0}` to even.
- **Integer width and overflow** (Medium): Python ints never overflow; C, Go and Java ints wrap or are undefined on overflow. Pick i64, i128 or a bignum crate deliberately, and use wrapping_* where the source wraps.
- **Division and modulo** (Medium): Python `//` and `%` floor toward negative infinity; Rust `/` and `%` truncate: use `div_euclid`/`rem_euclid` or explicit floor math. C division by zero is undefined.
- **Hash iteration order** (High): Set and map iteration order comes from the source language's hash function. A port cannot reproduce it: sort before output, or make the order part of the contract.
- **Runtime changes to code** (High): Code that rewrites classes or modules at runtime has no Rust equivalent: keep it in the source language and port the stable parts.
- **Clock and random numbers** (Low): Results that depend on the time or on random numbers differ on every run, so a plain recording cannot pin them down. Record with `record --pin-clock` (TS/JS with the napi bridge, Python): `Date.now()`, `new Date()` and `Math.random` (`time.time`, `datetime.now` and `random`) then return fixed values during record, diff and gate, and the port reads the same ones from `src/port/pinned.rs` (`now_ms()`, `random()`, a documented mulberry32). Or pass the time and the seed in as parameters.

## Whole app

Shape: **server**.

| Entry point | Kind | Found from |
| --- | --- | --- |
| `app/core/dispatcher.py` | server | `.listen(` at app/core/dispatcher.py:127 |
| `app/core/hifz_assets.py` | cli | `if __name__ == "__main__"` at app/core/hifz_assets.py:192 |
| `app/main.py` | server | `FastAPI(` at app/main.py:227 |
| `app/modules/fatwa/reindex.py` | cli | `if __name__ == "__main__"` at app/modules/fatwa/reindex.py:328 |
| `app/modules/places/seed_places.py` | cli | `if __name__ == "__main__"` at app/modules/places/seed_places.py:96 |
| `app/seed.py` | cli | `if __name__ == "__main__"` at app/seed.py:739 |
| `app/worker.py` | cli | `if __name__ == "__main__"` at app/worker.py:77 |
| `scripts/generate_quran_fa_translations.py` | cli | `if __name__ == "__main__"` at scripts/generate_quran_fa_translations.py:115 |
| `scripts/generate_reciter_timings.py` | cli | `if __name__ == "__main__"` at scripts/generate_reciter_timings.py:378 |
| `scripts/import_qul_tafsir.py` | cli | `if __name__ == "__main__"` at scripts/import_qul_tafsir.py:208 |
| `scripts/import_quranenc_tafsir.py` | cli | `if __name__ == "__main__"` at scripts/import_quranenc_tafsir.py:152 |

### Every dependency

| Dependency | Declared | Used by | Rust | Fit | Note |
| --- | --- | --- | --- | --- | --- |
| `fastapi` (also `starlette`) | yes | 25 | - | none | Web framework: keep the app in Python. |
| `mypy` (also `pytest`) | yes | 0 | - | none | Tooling, not runtime. |
| `uvicorn` | yes | 0 | axum | none | a server is an application rewrite, not a port |
| `arq` | yes | 5 | ? | unknown |  |
| `pgvector` | yes | 3 | ? | unknown |  |
| `reportlab` | yes | 3 | ? | unknown |  |
| `faster-whisper` | yes | 2 | ? | unknown |  |
| `arabic-reshaper` | yes | 1 | ? | unknown |  |
| `argon2` | no | 1 | ? | unknown |  |
| `bidi` | no | 1 | ? | unknown |  |
| `hijri-converter` | yes | 1 | ? | unknown |  |
| `minio` | yes | 1 | ? | unknown |  |
| `opentelemetry` | no | 1 | ? | unknown |  |
| `pydantic-core` | no | 1 | ? | unknown |  |
| `pydantic-settings` | yes | 1 | ? | unknown |  |
| `pyotp` | yes | 1 | ? | unknown |  |
| `sentence-transformers` | yes | 1 | ? | unknown |  |
| `sentry-sdk` | yes | 1 | ? | unknown |  |
| `sherpa-onnx` | yes | 1 | ? | unknown |  |
| `telethon` | yes | 1 | ? | unknown |  |
| `aiosqlite` | yes | 0 | ? | unknown |  |
| `alembic` | yes | 0 | ? | unknown |  |
| `argon2-cffi` | yes | 0 | ? | unknown |  |
| `asyncpg` | yes | 0 | ? | unknown |  |
| `email-validator` | yes | 0 | ? | unknown |  |
| `fakeredis` | yes | 0 | ? | unknown |  |
| `greenlet` | yes | 0 | ? | unknown |  |
| `jsonschema` | yes | 0 | ? | unknown |  |
| `opentelemetry-exporter-otlp` | yes | 0 | ? | unknown |  |
| `opentelemetry-sdk` | yes | 0 | ? | unknown |  |
| `pytest-asyncio` | yes | 0 | ? | unknown |  |
| `python-bidi` | yes | 0 | ? | unknown |  |
| `python-multipart` | yes | 0 | ? | unknown |  |
| `python-telegram-bot` | yes | 0 | ? | unknown |  |
| `ruff` | yes | 0 | ? | unknown |  |
| `types-reportlab` | yes | 0 | ? | unknown |  |
| `sqlalchemy` | yes | 28 | sqlx | partial | ORM layers port by hand. |
| `pydantic` | yes | 22 | serde, garde | partial | Validation errors will differ. |
| `numpy` | no | 2 | ndarray | partial | Broadcasting and dtype rules differ; check numerics. |
| `anyio` | no | 1 | tokio | partial |  |
| `av` | no | 1 | ffmpeg-next | partial | Binds the C libraries. For pure Rust, remade-ffmpeg covers a subset of codecs (and has an ffmpeg-compatible CLI): check its codec table. |
| `cryptography` | yes | 1 | ring, RustCrypto crates | partial |  |
| `pil` | no | 1 | image | partial |  |
| `httpx` | yes | 8 | reqwest | good |  |
| `redis` | yes | 4 | redis | good |  |
| `pyjwt` (also `jwt`) | yes | 1 | jsonwebtoken | good |  |

### Pieces, in port order

- wave 0: `alembic/versions/` (3898 lines)
- wave 0: `app/core/`, `app/models/`, `app/modules/admin/`, `app/modules/auth/`, `app/modules/content/`, `app/modules/fatwa/`, `app/modules/home/`, `app/modules/library/`, `app/modules/places/`, `app/modules/quran/`, `app/modules/social/`, `app/modules/sync/`, `app/modules/telegram/`, `app/modules/users/`, `app/modules/videos/`, `app/modules/voice/`, tangled: one piece (49484 lines)
- wave 0: `app/modules/` (0 lines)
- wave 1: `alembic/` (43 lines)
- wave 1: `app/modules/activity/` (69 lines)
- wave 1: `app/modules/api_keys/` (789 lines)
- wave 1: `app/modules/articles/` (690 lines)
- wave 1: `app/modules/calculators/` (3678 lines)
- wave 1: `app/modules/lms/` (2196 lines)
- wave 1: `app/modules/meta/` (133 lines)
- wave 1: `app/modules/projects/` (982 lines)
- wave 1: `scripts/` (753 lines)
- wave 2: `app/modules/finance/` (2493 lines)
- wave 2: `app/modules/widgets/` (325 lines)
- wave 3: `app/` (1817 lines)

### Decisions for the user

**D1** (shape): The app is a server, entered through app/core/dispatcher.py, app/core/hifz_assets.py, app/main.py, app/modules/fatwa/reindex.py, app/modules/places/seed_places.py, app/seed.py, app/worker.py, scripts/generate_quran_fa_translations.py, scripts/generate_reciter_timings.py, scripts/import_qul_tafsir.py, scripts/import_quranenc_tafsir.py. Port every entry point, in that shape?

> Yes: an axum front door (`app front-door`) takes the server over first

**D2** (blocked app/): app/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D3** (blocked app/core/): app/core/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D4** (blocked app/modules/activity/): app/modules/activity/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D5** (blocked app/modules/admin/): app/modules/admin/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D6** (blocked app/modules/api_keys/): app/modules/api_keys/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D7** (blocked app/modules/articles/): app/modules/articles/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D8** (blocked app/modules/auth/): app/modules/auth/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D9** (blocked app/modules/calculators/): app/modules/calculators/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D10** (blocked app/modules/content/): app/modules/content/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D11** (blocked app/modules/fatwa/): app/modules/fatwa/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D12** (blocked app/modules/finance/): app/modules/finance/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D13** (blocked app/modules/home/): app/modules/home/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D14** (blocked app/modules/library/): app/modules/library/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D15** (blocked app/modules/lms/): app/modules/lms/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D16** (blocked app/modules/meta/): app/modules/meta/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D17** (blocked app/modules/places/): app/modules/places/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D18** (blocked app/modules/projects/): app/modules/projects/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D19** (blocked app/modules/quran/): app/modules/quran/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D20** (blocked app/modules/social/): app/modules/social/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D21** (blocked app/modules/sync/): app/modules/sync/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D22** (blocked app/modules/telegram/): app/modules/telegram/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D23** (blocked app/modules/users/): app/modules/users/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D24** (blocked app/modules/videos/): app/modules/videos/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D25** (blocked app/modules/voice/): app/modules/voice/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D26** (blocked app/modules/widgets/): app/modules/widgets/ is blocked: `fastapi` has no Rust equivalent. Rework that part in Rust, or keep the unit outside?

> Research the blocker first; then port it with the piece that uses it

**D27** (dependency fastapi): No crate does what fastapi does (used by 25 units; Web framework: keep the app in Python). Reimplement what the app uses, or drop the feature?

> Reimplement only the part the app uses, proved by its recorded cases

**D28** (dependency mypy): No crate does what mypy does (declared, no unit imports it; Tooling, not runtime). Reimplement what the app uses, or drop the feature?

> Reimplement only the part the app uses, proved by its recorded cases

**D29** (dependency uvicorn): No crate does what uvicorn does (declared, no unit imports it; a server is an application rewrite, not a port). Reimplement what the app uses, or drop the feature?

> Reimplement only the part the app uses, proved by its recorded cases

**D30** (dependency arq): Which crate replaces arq (used by 5 units)?

> Research it against primary sources into rust/research/arq.md before porting what uses it

**D31** (dependency pgvector): Which crate replaces pgvector (used by 3 units)?

> Research it against primary sources into rust/research/pgvector.md before porting what uses it

**D32** (dependency reportlab): Which crate replaces reportlab (used by 3 units)?

> Research it against primary sources into rust/research/reportlab.md before porting what uses it

**D33** (dependency faster-whisper): Which crate replaces faster-whisper (used by 2 units)?

> Research it against primary sources into rust/research/faster-whisper.md before porting what uses it

**D34** (dependency arabic-reshaper): Which crate replaces arabic-reshaper (used by 1 unit)?

> Research it against primary sources into rust/research/arabic-reshaper.md before porting what uses it

**D35** (dependency argon2): Which crate replaces argon2 (used by 1 unit)?

> Research it against primary sources into rust/research/argon2.md before porting what uses it

**D36** (dependency bidi): Which crate replaces bidi (used by 1 unit)?

> Research it against primary sources into rust/research/bidi.md before porting what uses it

**D37** (dependency hijri-converter): Which crate replaces hijri-converter (used by 1 unit)?

> Research it against primary sources into rust/research/hijri-converter.md before porting what uses it

**D38** (dependency minio): Which crate replaces minio (used by 1 unit)?

> Research it against primary sources into rust/research/minio.md before porting what uses it

**D39** (dependency opentelemetry): Which crate replaces opentelemetry (used by 1 unit)?

> Research it against primary sources into rust/research/opentelemetry.md before porting what uses it

**D40** (dependency pydantic-core): Which crate replaces pydantic-core (used by 1 unit)?

> Research it against primary sources into rust/research/pydantic-core.md before porting what uses it

**D41** (dependency pydantic-settings): Which crate replaces pydantic-settings (used by 1 unit)?

> Research it against primary sources into rust/research/pydantic-settings.md before porting what uses it

**D42** (dependency pyotp): Which crate replaces pyotp (used by 1 unit)?

> Research it against primary sources into rust/research/pyotp.md before porting what uses it

**D43** (dependency sentence-transformers): Which crate replaces sentence-transformers (used by 1 unit)?

> Research it against primary sources into rust/research/sentence-transformers.md before porting what uses it

**D44** (dependency sentry-sdk): Which crate replaces sentry-sdk (used by 1 unit)?

> Research it against primary sources into rust/research/sentry-sdk.md before porting what uses it

**D45** (dependency sherpa-onnx): Which crate replaces sherpa-onnx (used by 1 unit)?

> Research it against primary sources into rust/research/sherpa-onnx.md before porting what uses it

**D46** (dependency telethon): Which crate replaces telethon (used by 1 unit)?

> Research it against primary sources into rust/research/telethon.md before porting what uses it

**D47** (dependency aiosqlite): Which crate replaces aiosqlite (declared, no unit imports it)?

> Research it against primary sources into rust/research/aiosqlite.md before porting what uses it

**D48** (dependency alembic): Which crate replaces alembic (declared, no unit imports it)?

> Research it against primary sources into rust/research/alembic.md before porting what uses it

**D49** (dependency argon2-cffi): Which crate replaces argon2-cffi (declared, no unit imports it)?

> Research it against primary sources into rust/research/argon2-cffi.md before porting what uses it

**D50** (dependency asyncpg): Which crate replaces asyncpg (declared, no unit imports it)?

> Research it against primary sources into rust/research/asyncpg.md before porting what uses it

**D51** (dependency email-validator): Which crate replaces email-validator (declared, no unit imports it)?

> Research it against primary sources into rust/research/email-validator.md before porting what uses it

**D52** (dependency fakeredis): Which crate replaces fakeredis (declared, no unit imports it)?

> Research it against primary sources into rust/research/fakeredis.md before porting what uses it

**D53** (dependency greenlet): Which crate replaces greenlet (declared, no unit imports it)?

> Research it against primary sources into rust/research/greenlet.md before porting what uses it

**D54** (dependency jsonschema): Which crate replaces jsonschema (declared, no unit imports it)?

> Research it against primary sources into rust/research/jsonschema.md before porting what uses it

**D55** (dependency opentelemetry-exporter-otlp): Which crate replaces opentelemetry-exporter-otlp (declared, no unit imports it)?

> Research it against primary sources into rust/research/opentelemetry-exporter-otlp.md before porting what uses it

**D56** (dependency opentelemetry-sdk): Which crate replaces opentelemetry-sdk (declared, no unit imports it)?

> Research it against primary sources into rust/research/opentelemetry-sdk.md before porting what uses it

**D57** (dependency pytest-asyncio): Which crate replaces pytest-asyncio (declared, no unit imports it)?

> Research it against primary sources into rust/research/pytest-asyncio.md before porting what uses it

**D58** (dependency python-bidi): Which crate replaces python-bidi (declared, no unit imports it)?

> Research it against primary sources into rust/research/python-bidi.md before porting what uses it

**D59** (dependency python-multipart): Which crate replaces python-multipart (declared, no unit imports it)?

> Research it against primary sources into rust/research/python-multipart.md before porting what uses it

**D60** (dependency python-telegram-bot): Which crate replaces python-telegram-bot (declared, no unit imports it)?

> Research it against primary sources into rust/research/python-telegram-bot.md before porting what uses it

**D61** (dependency ruff): Which crate replaces ruff (declared, no unit imports it)?

> Research it against primary sources into rust/research/ruff.md before porting what uses it

**D62** (dependency types-reportlab): Which crate replaces types-reportlab (declared, no unit imports it)?

> Research it against primary sources into rust/research/types-reportlab.md before porting what uses it

**D63** (dependency sqlalchemy): sqlalchemy maps to sqlx only in part (used by 28 units; ORM layers port by hand). Use it?

> Use sqlx, and record cases for the behavior it does not cover

**D64** (dependency pydantic): pydantic maps to serde, garde only in part (used by 22 units; Validation errors will differ). Use it?

> Use serde, garde, and record cases for the behavior it does not cover

**D65** (dependency numpy): numpy maps to ndarray only in part (used by 2 units; Broadcasting and dtype rules differ; check numerics). Use it?

> Use ndarray, and record cases for the behavior it does not cover

**D66** (dependency anyio): anyio maps to tokio only in part (used by 1 unit). Use it?

> Use tokio, and record cases for the behavior it does not cover

**D67** (dependency av): av maps to ffmpeg-next only in part (used by 1 unit; Binds the C libraries. For pure Rust, remade-ffmpeg covers a subset of codecs (and has an ffmpeg-compatible CLI): check its codec table). Use it?

> Use ffmpeg-next, and record cases for the behavior it does not cover

**D68** (dependency cryptography): cryptography maps to ring, RustCrypto crates only in part (used by 1 unit). Use it?

> Use ring, RustCrypto crates, and record cases for the behavior it does not cover

**D69** (dependency pil): pil maps to image only in part (used by 1 unit). Use it?

> Use image, and record cases for the behavior it does not cover

**D70** (tangled units): app/core/, app/models/, app/modules/admin/, app/modules/auth/, app/modules/content/, app/modules/fatwa/, app/modules/home/, app/modules/library/, app/modules/places/, app/modules/quran/, app/modules/social/, app/modules/sync/, app/modules/telegram/, app/modules/users/, app/modules/videos/, app/modules/voice/ import each other (49484 lines). Port them as one piece?

> Yes: one piece in wave 0, no glue between its parts

