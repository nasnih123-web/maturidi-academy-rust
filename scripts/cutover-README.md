# راهنمای کات‌اور موج ۹ — استقرار API راست روی سرور Railway جدید

> **وضعیت:** این سند و اسکریپت `cutover.sh` فقط آماده‌سازی‌اند. هیچ استقراری
> انجام نشده، هیچ دستوری روی سرور واقعی اجرا نشده، و پروژهٔ زنده دست نخورده است.
> کات‌اور فقط با تأیید صریح کاربر انجام می‌شود (ADR-004).
>
> **قانون ایستاده:** دامنه‌ها روی پروژهٔ زنده می‌مانند تا کاربر صراحتاً دستور
> تغییرشان را بدهد. این راهنما هیچ تغییری در DNS/دامنه ندارد (§۸).
>
> **بازبینی موج ۹ PR-1:** این سند با واقعیت موج ۸ هماهنگ شده است — پروکسی
> عمومی حذف شده (ADR-133)، فقط ۸ مسیر قرارداد سایدکار به `SIDECAR_URL`
> ارجاع می‌دهند (ADR-134)، و `/ws/events`، `/metrics` و `/api/v1/health`
> روی باینری راست بومی‌اند (ADR-135/136/137). ارجاع‌های قدیمی به
> `ORIGIN_URL` و «روت‌های proxied» حذف شده‌اند.

## ۰. این دو فایل چه هستند

| فایل | کار |
|---|---|
| `scripts/cutover.sh` | پیش‌پروازِ فقط‌خواندنی: متغیرهای محیطی، Postgres + اسکیما، Redis، داده‌های باندل‌شدهٔ قرآن، و تک‌تک سایدکارها/ارائه‌دهنده‌های خارجی را بررسی می‌کند و در پایان `GO / DEGRADED / NO-GO` می‌گوید. |
| `scripts/cutover-README.md` | همین سند: ران‌بوک قدم‌به‌قدم برای انسان. |

اسکریپت **هیچ‌چیز را تغییر نمی‌دهد**: نه migration اجرا می‌کند، نه سرور را
بالا می‌آورد، نه ردیفی می‌نویسد یا پاک می‌کند، و نه به پروژهٔ زنده دست می‌زند.
اجرای مکرر آن امن است. کد خروج: `0 = GO` ،`2 = DEGRADED` (سخت‌افزارها سالم‌اند
ولی یک سایدکار پایین است — API می‌تواند بالا بیاید و ویژگیِ مربوط صادقانه
degrade می‌شود)، `1 = NO-GO` (مانع سخت: گارد پروژهٔ زنده، متغیر لازمِ گمشده،
Postgres/اسکیما خراب، یا ابزارِ لازم نصب نیست).

```bash
# پیش‌نمایشِ خشک — فقط چاپ می‌کند چه چیزی را بررسی می‌کند؛ به هیچ سرویسی وصل نمی‌شود
bash scripts/cutover.sh --dry-run

# اجرای واقعی (متغیرها از محیط خوانده می‌شوند؛ مقدار هیچ secret چاپ نمی‌شود)
CUTOVER_ACK_NEW_PROJECT=yes bash scripts/cutover.sh

# یا با فایل env (ترجیحاً، تا secretها در history شل نمانند)
bash scripts/cutover.sh --env-file /path/to/new-project.env
```

## ۱. تصویر کلی — روی سرور جدید چه چیزی می‌نشیند

```
┌─────────────────────────────────────────────────────────────────┐
│ Railway project جدید (کاملاً جدا از پروژهٔ زنده)                │
│                                                                 │
│  Postgres (جایگزینِ DB زنده) ── Redis (جایگزینِ Redis زنده)    │
│        ▲                               ▲                       │
│        │                               │                       │
│  ┌─────┴──────────┐            ┌────────┴─────────┐             │
│  │ Rust API       │─SIDECAR──▶ │ Python sidecar   │             │
│  │ maturidi-api-rs│  _URL      │ API (اصل، تصویر  │             │
│  │ /health /ready │  (فقط ۸    │ apps/api) +      │             │
│  │ + همهٔ روت‌های  │  مسیر)     │ dispatcher داخل  │             │
│  │  native (۴۶۳)  │            │ همان پروسه       │             │
│  └────────────────┘            └────────┬─────────┘             │
│        ▲                               │ ARQ queues            │
│        │                               ▼                       │
│        │              workers: critical / ai / media /         │
│        │              telegram / notifications / analytics     │
│        └─ web / CRM (بعداً؛ فعلاً به API زنده وصل‌اند و        │
│           تا دستور صریح کاربر به API جدید وصل نمی‌شوند)         │
└─────────────────────────────────────────────────────────────────┘
```

**چرا هنوز Python لازم است؟** باینری راست عمداً چند چیز را ندارد و تا وقتی
سایدکار Python هست، فقط **۸ مسیر** مشخص به آن ارجاع می‌دهند
(قرارداد سایدکار، ADR-134 — ارجاع صریح هر مسیر به `SIDECAR_URL`؛ اگر
سایدکار نباشد، 503 صادقانه با کد مخصوص همان مسیر، نه 502 و نه موفقیتِ جعلی).
پروکسی عمومی **حذف شده** (ADR-133) — هیچ مسیر دیگری به بیرون نمی‌رود:

- `POST /api/v1/fatwa/ask` و SSE آن — دستیار/مفتی جداست (ADR-026 ← ADR-134)
- امتیازدهی hifz با ASR — ‏`grade`، ‏`grade-word`، ‏`voice-search`، ‏`answer` تست (ADR-031 ← ADR-134)؛ استریم زندهٔ hifz صراحتاً 503 می‌دهد (`hifz_live_unavailable`) و هیچ مسیر سایدکاری ندارد — برای استریم زنده باید مستقیم به خودِ سایدکار وصل شد
- `POST /api/v1/voice/transcribe` و `/voice/ask` — بدون STT در باینری راست (ADR-046 ← ADR-134)
- `GET /api/v1/finance/receipts/{id}` — رندر PDF رسید (ADR-076 ← ADR-134)
- **بومی روی باینری راست (دیگر سایدکار نیستند):** ‏`GET /api/v1/health` (‏`media_tools` صادقانه همه-unavailable، ADR-137)، ‏`GET /metrics` (متریک‌های Prometheus بومی، ADR-136)، ‏`GET /ws/events` (وب‌سوکت بومی Axum، ADR-135)
- صف‌های ARQ و dispatcher — فقط Python (ADR-125)؛ ‏dispatcher داخل lifespan همان API Python بالا می‌آید (`apps/api/app/main.py` → `start_realtime` از `app/core/dispatcher.py`)، پس سرویس جدا برای dispatcher لازم نیست. (پولر WS خودِ باینری راست فقط `domain_events` را **می‌خواند** و ردیفی را processed علامت نمی‌زند — با dispatcher تداخلی ندارد.)
- تولید زیرنویس (faster-whisper + ffmpeg + LLM)، ترنسکریپت/OCR کتابخانه، رندر PDF گواهی، ingest — همه فقط «pending» علامت می‌زنند و `enqueued: false` برمی‌گردانند تا وقتی ورکر Python روی همان DB کارشان را بکند (ADR-022/045/053/061/103).

## ۲. ترتیب استقرار (به همین ترتیب؛ هیچ قدمی را جابه‌جا نکنید)

1. **ساخت پروژهٔ جدید در Railway** — از ریپوی `nasnih123-web/maturidi-academy-rust`، شاخهٔ `main`. پروژهٔ زنده را باز نکنید و چیزی در آن تغییر ندهید.
2. **Postgres + Redis** — دو سرویس دادهٔ جدید در همان پروژهٔ جدید.
   - ⚠️ **بازِ تأییدنشده:** اسکیما به اکستنشن‌های `vector` و `pg_trgm` نیاز دارد (migration ‏`e2b3c4d5f6a7`). در compose اصلی از ایمیج `pgvector/pgvector:pg16` استفاده می‌شود؛ اینکه پلاگین Postgres خودِ Railway این اکستنشن‌ها را بدهد یا نه، از روی کد **تأیید نشده** و باید موقع ساخت سرویس آزمایش شود (`CREATE EXTENSION IF NOT EXISTS vector;` — اگر نشد، Postgres خارجی با pgvector لازم است). اسکریپت این دو اکستنشن را چک می‌کند و نبودشان NO-GO است.
3. **اسکیما (§۴)** — ‏`alembic upgrade head` روی DB جدید، یا ری‌استور بک‌آپ.
4. **Seed (فقط برای DB خالی، §۴)** — ساخت ادمین اول با seed پایتون.
5. **سایدکار Python + ورکرها (§۶)** — API پایتون (تصویر `apps/api`) و شش ورکر ARQ.
6. **سرویس Rust API (§۳)** — بعد از اینکه DB و سایدکارها بالا آمدند.
7. **`cutover.sh` بدون `--dry-run`** — باید `GO` یا `DEGRADED` بدهد؛ `NO-GO` یعنی هنوز ترافیک نفرستید.
8. **چک‌لیست تأیید (§۷)** — ‏`/health` و `/ready` و smoke روت‌ها.
9. **DNS (§۸)** — فقط با دستور صریح کاربر. تا آن لحظه دامنه‌ها روی پروژهٔ زنده می‌مانند.

## ۳. سرویس Rust API

> **وضعیت صریح (PR-1):** فایل‌های `Dockerfile` و `railway.toml` در ریشهٔ این
> ریپو **موجود‌اند** اما **هرگز جایی ساخته نشده‌اند** (حتی `docker build`
> محلی هم روی آن‌ها اجرا نشده). اولین بیلد Railway اولین آزمایهٔ واقعی
> آن‌هاست — PR-2 دقیقاً همین را بررسی می‌کند. **هیچ‌چیز** در این بخش را
> به‌عنوان «کار می‌کند» نخوانید.

**`Dockerfile` (وضعیت فعلی، بررسی‌شده در PR-1):**

- بیلد چندمرحله‌ای: مرحلهٔ بیلد از `rust:1.100-slim-bookworm`، دستور
  `cargo build --release -p maturidi-api-rs --locked`؛ مرحلهٔ ران‌تایم از
  `debian:bookworm-slim`، کاربر `appuser` (uid `10001`)، باینری در
  `/code/maturidi-api-rs`، دایرکتوری `/code/storage` ساخته می‌شود.
- `CMD ["./maturidi-api-rs"]` — باینری مستقیماً روی پورت `PORT` (پیش‌فرض
  `8000`) گوش می‌دهد (`main.rs` روی `0.0.0.0:$PORT` بایند می‌کند).
- **بدون `HEALTHCHECK`** — چون Railway خودش `healthcheckPath` را چک می‌کند.
- فرض‌ها: کانتکست بیلد = ریشهٔ ریپو (فایل‌های `rust/`، ‏`Cargo.toml`،
  `Cargo.lock`، ‏`rust/data/`؛ داده‌های باندل‌شدهٔ ۱۸ مگابایتی — متن Tanzil،
  ترجمه‌های فارسی، جدول اذان تربت‌جام، تایمینگ‌های Husary سوره‌های ۱ و ۶۷ —
  داخل باینری خوانده می‌شوند و در بیلد لازم‌اند)؛ شبکهٔ بیلد برای crates.io.
- **نکتهٔ مهم:** بیلد Dockerfile هیچ مایگریشنی اجرا نمی‌کند و هیچ seedای
  نمی‌زند (برخلاف `start.sh` اصل، که `alembic upgrade head` و seed را
  تحمل‌پذیر اجرا می‌کرد) — مایگریشن DB با `alembic upgrade head` جداگانه
  روی DB جدید انجام می‌شود (§۴)؛ برای بذر اولیه (`seed`) هنوز تصمیمی
  گرفته نشده است.

**`railway.toml` (وضعیت فعلی، بررسی‌شده در PR-1):**

- `builder = "DOCKERFILE"` با `dockerfilePath = "Dockerfile"`؛
  `startCommand = "./maturidi-api-rs"`؛ `healthcheckPath = "/health"` با
  `healthcheckTimeout = 300`؛ `restartPolicyType = "ON_FAILURE"` با
  `restartPolicyMaxRetries = 10`.
- این مقادیر آینهٔ `apps/api/railway.toml` اصل‌اند (startCommand اصل
  `"sh start.sh"` است؛ healthcheck و restart همان‌اند).
- **خاصیت ایمنی:** ‏`CMD` در Dockerfile و `startCommand` در railway.toml
  دقیقاً یکی‌اند (`./maturidi-api-rs`)، پس «Railway ‏startCommand را نادیده
  می‌گیرد» (درسی که روی پروژهٔ اصل دیده شد) نمی‌تواند بایت کند — هر دو
  همان باینری را می‌رسانند.
- `railway.toml` هیچ متغیری برای باینری ست نمی‌کند؛ همهٔ متغیرهای §۵ باید
  در سرویس Railway تعریف شوند (متن را هرگز commit نکنید).

**سرویس‌های لازم (تصویر Python برای سایدکار و ورکرها):**

- `api` (باینری Rust) — ورودی: `PORT` (پیش‌فرض `8000`)؛ باید `healthcheckPath` را روی `/health` بگذارد؛ `restartPolicyType = "ON_FAILURE"` با `restartPolicyMaxRetries = 10`.
- `postgres` — جداگانه (نه `DATABASE_URL` زنده)؛ بعد از ساخت، مایگریشن (§۴) روی آن اجرا شود.
- `redis` — جداگانه؛ بدون آن rate-limit احراز هویت بسته می‌شود.
- `sidecar` — تصویر `apps/api` اصل (همان Dockerfile اصل)؛ نام سرویس و `PORT` جدا؛ با همان متغیرهای سرویس `api` (§۵)؛ متغیر `SIDECAR_URL` روی سرویس `api` باید به URL داخلی این سرویس اشاره کند، نه به `api.matoridi.ir`.
- `worker-critical` ،`worker-ai` ،`worker-media` ،`worker-telegram` ،`worker-notifications` ،`worker-analytics` — هرکدام از تصویر `apps/api` اصل با همان متغیرها (به §۶ نگاه کنید).

**چیزهایی که از روی کد قطعی است:**

- باینری `maturidi-api-rs` است (`rust/app/Cargo.toml`، بخش `[[bin]]`) و workspace روت، ممبر `rust/app` دارد (`Cargo.toml` روت).
- پورت از متغیر `PORT` خوانده می‌شود (پیش‌فرض 8000، `rust/app/src/main.rs`) و روی `0.0.0.0` گوش می‌دهد.
- هلث‌چک: `GET /health` (همیشه 200 اگر پروسه زنده است)؛ آمادگی واقعی `GET /ready` است (DB را با `SELECT 1` پروب می‌کند؛ Redis فقط گزارش می‌شود و readiness را خراب نمی‌کند).
- داده‌های قرآن با `include_str!` **داخل باینری** کامپایل می‌شوند؛ بعد از هر تغییر داده باید rebuild شود.

- **Volume برای فایل‌ها:** ‏`STORAGE_PROVIDER` فقط `local` است (ADR-015) و فایل‌ها زیر `STORAGE_LOCAL_DIR` (پیش‌فرض `./storage`) روی دیسک می‌روند. روی Railway به آن مسیر یک **Volume** وصل کنید؛ وگرنه با هر redeploy فایل‌ها می‌پرند. (سابقه: در مهاجرت قبلی، بایتِ ۴ رسیدِ دستی مهاجرت نکرد چون storage لوکال بود — DB به‌تنهایی فایل‌ها را نمی‌آورد.)
- **Replica:** فعلاً **۱ replica**. چند replica یعنی چند حافظهٔ quota/کشِ جدا (quota دقیقه‌ای، کش ayah-of-day، و limiter purge داخل پروسه‌اند) — مثل Python بدون Redis.
- **CORS ویجت:** وایلدکارد `Access-Control-Allow-Origin: *` روی `/api/v1/widgets/*` در باینری راست **پیاده نشده** و مال لایهٔ deploy (reverse proxy) است (ADR-121). تا وقتی لایهٔ جلو آن را ندهد، امبدهای cross-origin در مرورگر بلاک می‌شوند، حتی اگر endpoint جواب بدهد. پیکربندیِ تأییدشده‌ای برای این لایه در این ریپو نیست — بازِ تأییدنشده.

## ۴. دیتابیس و اسکیما

**باینری راست هیچ migrationای ندارد.** نه `sqlx migrate` دارد، نه alembic.
`TEST_SCHEMA` در `rust/app/src/db.rs` فقط برای تست‌های sqlite درون‌حافظه‌ای است
(۱۱۹ جدول) و هرگز روی production اعمال نمی‌شود. اسکیمای production **همان
اسکیمای Python** است (ADR-006: روت‌ها، شکل پاسخ، اسکیما و auth یکسان می‌مانند).

**دو راه برای DB جدید — یکی را انتخاب کنید (تصمیم با کاربر):**

| | A) خالی + alembic + seed | B) ری‌استور بک‌آپِ DB زنده |
|---|---|---|
| دستور | از checkout ریپوی **اصلی** روی شاخهٔ `rust-base` (شامل PRهای #193 و #194)، با `DATABASE_URL` اشاره به DB **جدید**: `alembic upgrade head` — این دستور دقیقاً در `docs/ops/railway-deployment.md` ریپوی اصلی مستند است (یا از `railway run --service api alembic upgrade head`) | بک‌آپِ DB زنده را روی DB جدید ری‌استور کنید (روش ری‌استور به ابزار بک‌آپ شما بستگی دارد؛ در این سند دستور آن نیست) |
| Head مورد انتظار | `f2a3b4c5d6e7` — ۵۰ revision، روی checkout موجود تأیید شده (۲۰۲۶-۱۱-۲۲). اسکریپت دقیقاً همین مقدار را در جدول `alembic_version` چک می‌کند. اگر base جلو رفت، اول `alembic heads` را دوباره چک کنید. | بعد از ری‌استور هم `alembic_version` باید همان head باشد؛ اسکریپت چک می‌کند |
| داده | خالی؛ ادمین اول با seed ساخته می‌شود (پایین) | همهٔ کاربران/محتواها/تنظیمات زنده کپی می‌شوند |
| Secretها | می‌توانید secretهای تازه بسازید | **باید** همان `JWT_SECRET` و `ENCRYPTION_KEY` (+ `ENCRYPTION_KEY_VERSION` / `ENCRYPTION_KEYS`) و `STORAGE_SIGNING_SECRET` زنده را کپی کنید؛ وگرنه sessionها و تنظیماتِ رمزنگاری‌شدهٔ موجود خوانده نمی‌شوند |
| فایل‌ها | — | ⚠️ فایل‌های `./storage` (رسیدها، آواتارها، مدیای کتابخانه، PDF گواهی‌ها) با ری‌استور DB **نمی‌آیند**؛ جداگانه باید کپی شوند (و روی Volume بنشینند، §۳) |

**Seed (فقط راه A):** از ریپوی اصلی، با `DATABASE_URL` جدید:

```bash
ADMIN_EMAIL="<...>" ADMIN_PASSWORD="<...>" python -m app.seed
```

(‏`ADMIN_PHONE` هم به‌جای ایمیل قبول است؛ `apps/api/app/seed.py`.) این seed فقط
تنظیمات پیش‌فرض پلتفرم و ادمین اول را می‌سازد؛ ساخت ادمین در باینری راست
دستور جدایی ندارد.

**چک‌هایی که اسکریپت روی DB می‌کند (فقط‌خواندنی):** ‏`SELECT 1`، نسخهٔ Postgres،
اکستنشن‌های `vector` و `pg_trgm`، مقدار `alembic_version`، وجود ~۳۰ جدولِ هسته
(از `users` و `platform_settings` تا `payment_intents` و `hifz_attempts`)، و
وضعیت outbox: تعداد `domain_events` با `processed_at IS NULL` (pending)،
سنِ قدیمی‌ترین pending، و تعداد dead-letter (`failed_at IS NOT NULL`) — و
صف‌های سایدکار: ‏`ingestion_runs` با `status='queued'`، ‏`item_subtitles` با
`status='pending'`، گواهی‌های با `pdf_storage_ref IS NULL`، و آیتم‌های کتابخانه
با `transcript_status/ocr_status='pending'`. اگر DB خالی و تازه باشد، همهٔ این
تعدادها صفر اند و این طبیعی است.

## ۵. جدول متغیرهای محیطی

منبع هر سطر: `rust/app/src/lib.rs` (‏`Config::from_env`) مگر اینکه خلافش گفته
شده باشد. **مقدار هیچ secretای در اسکریپت چاپ نمی‌شود** — فقط «set / missing /
placeholder» و «≥ ۳۲ کاراکتر یا کوتاه‌تر».

### ۵.۱ الزامی — بدون این‌ها کات‌اور NO-GO است

| متغیر | پیش‌فرض در کد | منبع مقدار | secret؟ |
|---|---|---|---|
| `DATABASE_URL` | ندارد (بدون آن همهٔ روت native ‏500 می‌دهند) | **جدید** — Postgres سرویس جدید | بله (پسورد در URL است؛ اسکریپت آن را ماسک می‌کند) |
| `JWT_SECRET` | `CHANGE_ME` | **کپی از زنده** (راه B) یا تازه (راه A)؛ حداقل ۳۲ کاراکتر، نه placeholder، نه برابر `ENCRYPTION_KEY` | بله |
| `ENCRYPTION_KEY` | `CHANGE_ME` | **کپی از زنده** (راه B) یا تازه (راه A)؛ حداقل ۳۲ کاراکتر | بله |
| `ENCRYPTION_KEY_VERSION` | `1` | همان مقدار زنده (راه B) | خیر |
| `ENCRYPTION_KEYS` | خالی | همان map چرخش زنده (راه B) — اگر خالی است، یعنی فقط کلید تکی | بله |
| `STORAGE_SIGNING_SECRET` | خالی | **کپی از زنده** (راه B) یا تازه (راه A)؛ حداقل ۳۲ کاراکتر، متفاوت از `JWT_SECRET` | بله |
| `STORAGE_SIGNING_SECRET_PREVIOUS` | خالی | مقدار قبلی در دورهٔ گذار (فقط برای verify) | بله |
| `APP_ENV` | `dev` | `production` (الزامی برای کات‌اور) | خیر |
| `APP_BASE_URL` | `http://localhost:8000` | **جدید** — origin عمومی API جدید (‏`https://…`)؛ هرگز دامنهٔ زنده | خیر |
| `REDIS_URL` | `redis://localhost:6379/0` | **جدید** — Redis سرویس جدید | بله (پسورد در URL است) |
| `SIDECAR_URL` | ندارد (اختیاری؛ بدون آن ۸ مسیر سایدکار 503 می‌دهند) | **جدید** — URL سرویس سایدکار Python جدید؛ هرگز API زنده. وقتی ست نیست، ۸ مسیر سایدکار 503 صادقانه با کد مخصوص همان مسیر می‌دهند (ADR-134) | خیر |
| `RUST_API_URL` | ندارد (اختیاری) | URL باینری راستِ در حال اجرا برای پروب زنده در گام ۷ اسکریپت؛ وقتی ست نیست، گام ۷ رد می‌شود (info) | خیر |
| `PORT` | `8000` | Railway تزریق می‌کند | خیر |
| `STORAGE_PROVIDER` | `local` | `local` (تنها مقدار ممکن — ADR-015؛ غیر آن 503 می‌دهد) | خیر |
| `STORAGE_LOCAL_DIR` | `./storage` | مسیر Volume روی سرویس راست (§۳) | خیر |

### ۵.۲ مشروط

| متغیر | پیش‌فرض | شرط |
|---|---|---|
| `TURNSTILE_SECRET_KEY` (+ `TURNSTILE_SITE_KEY` عمومی) | خالی | اگر `CAPTCHA_ENABLED` خاموش نشده (پیش‌فرض روشن)، نبودنِ secret یعنی ثبت‌نام 503 ‏`captcha_not_configured` می‌خورد (`captcha.rs`). یا secret را بدهید، یا آگاهانه `CAPTCHA_ENABLED=false` کنید. |
| `GITHUB_DISPATCH_TOKEN` + `GITHUB_REPO` | خالی | فقط برای `POST /admin/builds/rebuild` (‏`workflow_dispatch` واقعی، ADR-087). هر دو با هم لازم‌اند؛ وگرنه 503 ‏`build_provider_not_configured`. خوانده‌شده مستقیم در `routes_admin.rs`. |
| `BUILDS_SERVICE_TOKEN` | خالی | فقط برای احراز هویت callback سرویس بیلد (`POST /admin/builds/releases`)؛ بدون آن فقط مسیر کاربرِ مجاز کار می‌کند. خوانده‌شده در `routes_admin.rs`. |
| `S3_ENDPOINT_PUBLIC` / `BUILDS_S3_*` | خالی | فقط برای اعتبارسنجی host در `download_url` انتشارها (ADR-088: ردیف‌های `artifact_key` در راست presign نمی‌شوند). خوانده‌شده در `routes_admin.rs`. |
| `CLAMAV_ALLOW_INSECURE_DEV_BYPASS` | `false` | در کات‌اور production باید `false`/خالی بماند. آپلودها از گیت ADR-051 (بلاک‌لیست SHA-256 + EICAR + بازبینی دستی) می‌گذرند؛ ClamAV برنگشته است. |
| `TRUSTED_PROXIES` | خالی | فقط IPهای پروکسی مورداعتماد (کاما-جدا) که `X-Forwarded-For`شان پذیرفته می‌شود. |
| `ACCESS_TOKEN_EXPIRE_MINUTES` | `15` | همان مقدار زنده. |
| `NISAB_PRICE_URL` | ندارد | override اختیاری URL قیمت نصاب (خوانده‌شده در `routes_calculators.rs` و `routes_widgets.rs`). |
| `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` | ندارد | پروکسی خروجی برای کلاینت‌های Quran provider و timing (‏`quran_providers.rs`، ‏`routes_quran.rs`). فقط در صورت نیاز. |

### ۵.۳ متغیرهایی که هست ولی باینری راست **نمی‌خواند** — گول نخورید

| متغیر | واقعیت در باینری راست |
|---|---|
| `RESEND_API_KEY`، ‏`SMTP_HOST/PORT/USER/PASSWORD/FROM` | **ایمیل از باینری راست ارسال نمی‌شود.** ‏`mail.rs` استاب است (ADR-015): ‏`send_email`/`send_sms` فقط لاگ می‌کنند و موفق برمی‌گردند. توکن تأیید ایمیل/ریست پسورد ذخیره می‌شود و فلو ادامه پیدا می‌کند، ولی ایمیلی نمی‌رسد. اگر این متغیرها را بگذارید، صفحهٔ وضعیت اتصال‌ها (`/admin/connections/status`) آن‌ها را «پیکربندی‌شده» نشان می‌دهد (`routes_admin.rs:3019`) — اما این یعنی «تنظیم هست»، نه «ایمیل می‌رسد». **این بزرگ‌ترین شکافِ شناخته‌شدهٔ کات‌اور است** و قبل از ترافیک واقعی باید تصمیم گرفته شود: یا mailer پورت شود، یا ارسال ایمیل به ورکر Python سپرده شود (تصمیمی که از روی کد تأیید نشده). |
| `SMS_PROVIDER` / `SMS_API_KEY` | مثل ایمیل: فقط در گزارش وضعیت اتصال خوانده می‌شود؛ پیامک واقعی از راست ارسال نمی‌شود. |
| `SENTRY_DSN`، ‏`OTEL_EXPORTER_OTLP_ENDPOINT`، ‏`LOG_FORMAT` | در کد راست هیچ خواندنی ندارند — تله‌متری/لاگ ساخت‌یافته در باینری نیست. |
| `CORS_ALLOW_ORIGINS` | **خوانده می‌شود — اما فقط برای چک Origin وب‌سوکت** ‏`GET /ws/events` (‏`lib.rs` ‏`Config::from_env` + ‏`routes_lift.rs` ‏`origin_allowed`؛ پیش‌فرض خالی). وایلدکارد `Access-Control-Allow-Origin: *` ویجت همچنان مال لایهٔ deploy است (§۳، ADR-121). |
| `S3_ENDPOINT` / `S3_ACCESS_KEY` / `S3_SECRET_KEY` / `S3_BUCKET` / `S3_REGION` | خوانده نمی‌شوند (فقط `S3_ENDPOINT_PUBLIC` خوانده می‌شود، §۵.۲). باینری راست S3/MinIO ندارد (ADR-015). |
| `CLAMAV_HOST` / `CLAMAV_PORT` | خوانده نمی‌شوند؛ ClamAV برنگشته است (ADR-050/051). |

## ۶. سایدکارها و ترتیب بالا آمدن

همهٔ سایدکارها روی **پروژهٔ جدید** و با همان `DATABASE_URL` و `REDIS_URL` جدید
بالا می‌آیند (تصویر همان `apps/api/Dockerfile` ریپوی اصلی؛ دستور start از
‏`railway.worker-<queue>.toml`های همان ریپو، تأییدشده):

| سرویس | Start (از فایل‌های `railway.worker-*.toml`) | چه کار می‌کند |
|---|---|---|
| `api` (سایدکار Python) | `sh start.sh` — اول `alembic upgrade head`، بعد seed، بعد uvicorn روی `$PORT`؛ ورکر media هم اگر `MEDIA_WORKER != off` در پس‌زمینه می‌چرخد | فقط ۸ مسیر قرارداد سایدکار (ADR-134) + **dispatcher** (داخل lifespan همین پروسه) |
| `worker-critical` | `python -m app.worker critical` | صف critical |
| `worker-ai` | `python -m app.worker ai` | ingest و reindex (‏`ingestion_runs`) |
| `worker-media` | `python -m app.worker media` | زیرنویس/ترنسکریپت/OCR (‏faster-whisper + ffmpeg + tesseract) |
| `worker-telegram` | `python -m app.worker telegram` | ارسال/توزیع تلگرام |
| `worker-notifications` | `python -m app.worker notifications` | اعلان‌ها |
| `worker-analytics` | `python -m app.worker analytics` | آنالیتیکس (+ cron همگام‌سازی وضعیت بیلد) |

**نکتهٔ مهم دربارهٔ بررسی سایدکارها:** ورکرهای ARQ **هیچ endpointای HTTP ندارند**،
پس اسکریپت نمی‌تواند آن‌ها را با HTTP چک کند. اسکریپت دو کار می‌کند: (۱) برای
سرویس‌هایی که HTTP دارند (سایدکار پایتون، AlQuran Cloud، ZarinPal، IDPay، Wallex،
CoinGecko، YouTube oEmbed، GitHub API، Telegram API، Resend API) یک جدول UP/DOWN
چاپ می‌کند؛ (۲) برای ورکرها، تعداد صف‌های معلق در DB (§۴) را نشان می‌دهد و
باقی‌مانده را به **داشبورد Railway و لاگ سرویس‌ها** می‌سپارد — حتماً قبل از
کات‌اور، هر شش ورکر را در داشبورد `Running` ببینید و لاگشان را چک کنید.
**dispatcher** سرویس جدا نیست — داخل همان پروسهٔ سایدکار Python بالا می‌آید
(‏`apps/api/app/main.py` → ‏`start_realtime`)؛ پس چکِ آن، چکِ سرویس سایدکار
است. پولر WS خودِ باینری راست فقط `domain_events` را می‌خواند و ردیفی را
processed علامت نمی‌زند.

**معنای DOWN برای هر سایدکار (همه از ADRها):**

| سایدکار DOWN | چه degrade می‌شود |
|---|---|
| `python-sidecar` | فقط ۸ مسیر قرارداد سایدکار (ADR-134) — ‏`fatwa/ask`، امتیازدهی ASR، ‏`voice/transcribe` و `/voice/ask`، PDF رسید — همه 503 با کد مخصوص همان مسیر (‏`fatwa_ask_unavailable`، ‏`hifz_asr_unavailable`، ‏`voice_stt_unavailable`، ‏`receipt_pdf_unavailable`)؛ ‏`/api/v1/health`، ‏`/metrics` و `/ws/events` بومی‌اند و تحت‌تأثیر نیستند |
| `alquran-cloud` | روت‌های قرآن با edition پیش‌فرض (‏`en.sahih`/`fa.makarem`) ‏503 `quran_provider_unavailable`؛ editionهای باندل‌شده (`fa.rowwad`/`fa.mokhtasar`) همچنان کار می‌کنند (ADR-041) |
| `zarinpal` / `idpay` | پرداخت آنلاین آن درگاه 503 ‏`gateway_unconfigured` / خطای درگاه (ADR-078) |
| `wallex` + `coingecko` | نرخ FX/نصاب با خطای صادقانه برمی‌گردد؛ هرگز نرخِ قدیمیِ ساختگی داده نمی‌شود (ADR-079) |
| `youtube-oembed` | ساخت ویدیوی خارجی بدون عنوانِ دستی 422 ‏`title_required`؛ عنوان هرگز ساخته نمی‌شود (ADR-052) |
| `github-api` | ‏`/admin/builds/rebuild` نمی‌تواند dispatch کند (503) |
| `telegram-api` | ارسال تلگرام کلاً ناممکن است؛ حتی وقتی API بالاست، بدون توکن در تنظیمات، ارسال‌ها `failed` می‌شوند (ADR-115) و اتصال Telethon صراحتاً 503 است (ADR-114) |
| dispatcher/ورکرها پایین | ‏`domain_events` انباشته می‌شود (اسکریپت سنِ قدیمی‌ترین pending را نشان می‌دهد)؛ ingest، زیرنویس، ترنسکریپت/OCR، PDF گواهی، و publish تلگرام در حالت `queued`/`pending` می‌مانند و `enqueued: false` برمی‌گردد — **هیچ‌کدام جعل نمی‌شوند** |

## ۷. چک‌لیست تأیید بعد از استقرار

با `API` = آدرس عمومی سرویس راست جدید (هنوز نه دامنهٔ زنده). همهٔ GETها باید
200 بدهند مگر اینکه خلافش گفته شده باشد:

```bash
API="https://<rust-service-public-url>"

curl -s $API/health
# {"status":"ok","env":"production"}

curl -s $API/ready
# {"status":"ready","env":"production","db":"ok","redis":"ok"}  (اگر redis "unavailable" بود، readiness هنوز 200 است ولی rate-limit احراز هویت fail-closed می‌شود — اول Redis را درست کنید)

# Smoke روت‌های native (فقط‌خواندنی):
curl -s $API/api/v1/themes/active
curl -s $API/api/v1/settings/public
curl -s $API/api/v1/home/sections
curl -s $API/api/v1/search?q=نماز
curl -s $API/api/v1/quran/surahs
curl -s "$API/api/v1/quran/surah/1?translation=fa.rowwad&tafsir=fa.mokhtasar"
curl -s $API/api/v1/articles
curl -s $API/api/v1/fatwa/categories
curl -s $API/api/v1/videos
curl -s $API/api/v1/library/items
curl -s "$API/api/v1/adhan/times?latitude=35.24&longitude=60.62"
curl -s $API/api/v1/widgets/ayah-of-day            # نیاز به X-API-Key یا ?embed_token= دارد
```

و این‌ها را دستی چک کنید:

- **احراز هویت (با حساب آزمایشی، نه حساب واقعی):** ثبت‌نام → ورود → refresh. ⚠️ توجه: ایمیل تأیید **نمی‌رسد** (§۵.۳)؛ فلوهایی که به ایمیل وابسته‌اند را جدا تست کنید و نتیجه را به کاربر گزارش دهید.
- **روت‌های سایدکار:** وقتی `SIDECAR_URL` ست است، ‏`POST /api/v1/fatwa/ask` باید جواب بدهد؛ وقتی ست نیست، همان روت 503 با کد `fatwa_ask_unavailable` می‌دهد (ADR-134) — نه 502.
- **‏`/ws/events` (‏ADR-135، بومی روی باینری راست):** با curl نمی‌شود تست کرد — با یک کلاینت WebSocket واقعی و `ws-ticket` (از `POST /api/v1/auth/ws-ticket`) مستقیماً به سرویس راست وصل شوید.
- **گواهی:** یک گواهیِ صادرشده باید فوراً verify شود (`/certificates/{code}/verify`) ولی دانلود PDF تا وقتی سایدکار رندر نکرده 404 ‏`certificate_not_found` می‌دهد (ADR-061) — این رفتارِ درست است، نه باگ.
- **زیرنویس/ingest:** ‏`POST /admin/videos/{id}/subtitles/generate` باید 202 با `enqueued: false` بدهد تا وقتی ورکر media آن‌ها را بسازد (ADR-045).
- **لاگ‌ها:** در لاگ سرویس راست دنبال `email delivery not ported yet` بگردید — هر بار که ایمیلی قرار بود برود، این هشدار می‌آید.

## ۸. DNS و دامنه — هیچ تغییری در این راهنما نیست

دامنه‌های زنده (`api.matoridi.ir` برای API و `academy.matoridi.ir` برای وب)
روی پروژهٔ زنده می‌مانند تا وقتی کاربر صراحتاً دستور جابه‌جایی بدهد. در این
راهنما:

- سرویس راست جدید فقط با URL موقتِ Railway خودش (‏`*.up.railway.app`) تست می‌شود.
- `APP_BASE_URL` سرویس جدید همان URL موقت است، نه دامنهٔ زنده.
- وب/CRM فعلاً به API زنده وصل‌اند و به API جدید وصل **نمی‌شوند**؛ وصل‌کردنشان
  (`API_URL` / ‏`VITE_API_URL` — مقدار build-time در CRM) تصمیم و قدم جداگانه‌ای
  است که به تأیید کاربر نیاز دارد.

## ۹. Rollback — برگشت به پروژهٔ زنده

**ترافیک:** برگشت یعنی DNS (یا `API_URL` وب/CRM) را به سرویس‌های زنده
برگردانید. همین. هیچ دستوری روی پروژهٔ زنده لازم نیست، چون پروژهٔ زنده
اصلاً دست نخورده است.

**داده — چه چیزی برگشت‌پذیر است و چه چیزی نیست:**

- **برگشت‌پذیر:** باینری راست همان اسکیمای Python را می‌نویسد (ADR-006)، پس
  داده‌ای که روی **DB جدید** نوشته شده (کاربر، مقاله، فتوا، hifz، پرداخت…)
  را Python هم می‌خواند؛ سوییچِ ترافیک بین دو API روی یک DB مشکلی ندارد.
- **برگشت‌ناپذیر / نیازمند توجه:**
  - DB جدید و DB زنده **دو DB جدا** هستند. داده‌ای که بعد از کات‌اور روی DB جدید ساخته شد، روی DB زنده **وجود ندارد** و خودبه‌خود برنمی‌گردد. Rollback یعنی «ترافیک به زنده برمی‌گردد»؛ یعنی کاربرانِ جدیدِ ثبت‌شده روی سرویس جدید و محتوای جدید، فقط روی DB جدید می‌مانند تا وقتی جداگانه migrate شوند (برای آن برنامه‌ای در این سند نیست).
  - پرداخت‌های نهایی‌شده از درگاه (ZarinPal/IDPay) پولِ واقعی‌اند؛ برگشت ترافیک، پول را برنمی‌گرداند.
  - ایمیل/پیامکی که فرستاده **نشده** (استاب ADR-015) قابل «پس‌گرفتن» نیست، ولی کاربری که منتظر ایمیل تأیید مانده، بعد از rollback هم ایمیلش را از سرویس زنده می‌گیرد، چون فلو از اول با همان توکنِ ذخیره‌شده ادامه پیدا می‌کند.
  - سه انحرافِ عمدیِ راست که در rollback به چشم می‌آید: (۱) embeddingهای ساخته‌شده با hash (‏`embedding_vec` همیشه NULL — ADR-019)؛ جستجوی pgvector (Backend A) در Python ممکن است ردیف‌های ساخته‌شده با راست را نبیند تا reindex شوند. (۲) شمارش گیرندگان broadcast با `DISTINCT` درست است، ولی Python همان کاربرِ چندنقشه را دوبار می‌شمرد (ADR-091) — عددها فرق می‌کنند. (۳) خوانده‌شدنِ اعلانِ سوشال در راست **ذخیره** می‌شود، ولی در Python گم می‌شود (ADR-108) — ‏`unread-count` بعد از rollback دوباره بالا می‌رود.
  - Quotaهای دقیقه‌ای/روزانه و کش ayah-of-day داخل پروسه‌اند و با restart/replica عوض می‌شوند (در هر دو stack همین‌طور است).
  - گواهی‌ها/زیرنویس‌ها/ingestهایی که در حالت `pending`/`queued` مانده‌اند، بعد از rollback هم در همان حالت می‌مانند تا ورکر Python آن‌ها را بردارد — گم نمی‌شوند.

## ۱۰. چیزهایی که از روی کد تأیید **نشد** (باز برای بررسی)

این‌ها حدس نیستند — دقیقاً جاهایی‌اند که کد جوابشان را نمی‌دهد و قبل از کات‌اور
باید جداگانه بررسی شوند:

1. **Dockerfile / railway.toml هرگز ساخته نشده‌اند** (§۳) — اولین بیلد Railway اولین آزمایهٔ آن‌هاست (PR-2).
2. **اکستنشن‌های `vector` و `pg_trgm` روی Postgres پلاگین Railway** (§۲) — تأییدنشده؛ ممکن است Postgres خارجی لازم شود.
3. **ایمیل/پیامک** (§۵.۳) — هیچ مسیرِ تأییدشده‌ای برای رساندن ایمیل تأیید/ریست در استقرار راست نیست؛ تصمیم لازم است.
4. **CORS ویجت در لایهٔ deploy** (ADR-121، §۳) — پیکربندیِ تأییدشده‌ای نیست.
5. **‏`/ws/events` از پشت باینری راست** (ADR-135) — بومی است و مستقیم تست می‌شود؛ رفتارش از پشت reverse proxy هنوز جداگانه تست نشده است.
6. **Head دقیق alembic در لحظهٔ کات‌اور** — ‏`f2a3b4c5d6e7` روی checkout موجود تأیید شد؛ اگر base جلو رفت، `alembic heads` را دوباره بگیرید.
7. **دستورهای Railway CLI** — به‌جز `railway run --service api alembic upgrade head` که در `docs/ops/railway-deployment.md` ریپوی اصلی مستند است، دستور CLI دیگری از روی منبع تأییدشده به کار نرفته؛ این راهنما عمداً قدم‌های داشبوردی می‌گوید.
8. **اندازهٔ Volume برای `./storage`** — حجم فایل‌های زنده (که بخشی از آن‌ها پیش‌تر از دست رفته) اندازه‌گیری نشده؛ قبل از سفارش Volume برآورد کنید.
9. **مقیاس‌پذیری replica** — فعلاً ۱ replica فرض شده (§۳)؛ چند replica یعنی چند حافظهٔ quota/کشِ جدا.
10. **ساعت/تایم‌زون سرویس‌ها** — محاسبه‌های adhan و hifz به ساعت UTC تکیه می‌کنند؛ اگر Railway ساعت کانتینر را UTC نگه ندارد (معمولاً نگه می‌دارد)، باید جداگانه چک شود.
