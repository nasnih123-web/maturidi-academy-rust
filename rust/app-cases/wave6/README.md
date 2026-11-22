# Wave 6 HTTP cases

Recorded cases for the Wave 6 modules (library, LMS, calculators). Each
file is one request/response pair, in the same shape as the Wave 2–5
case files. They are documentation of the wire behaviour; the executable
coverage lives in `rust/app/tests/wave6.rs` (sqlite + the combined
Postgres smoke `wave6_pg_smoke`).

## Library (`lib-*.json`)

- `lib-01.json` — public list as a guest (only `public` items, cursor page).
- `lib-02.json` — public list, cursor pagination (`after`).
- `lib-03.json` — public detail of one item.
- `lib-04.json` — lexical search (`q`, ranked, `matched_in`).
- `lib-05.json` — signed download-by-ref (bytes + `application/pdf`).
- `lib-06.json` — admin create, multipart PDF upload (201).
- `lib-07.json` — admin workflow transition (draft → in_review).
- `lib-08.json` — admin index-ai (hash embeddings, chunk approval).

## LMS (`0*-lms-*.json`)

- `01-lms-courses.json` — public course list.
- `02-lms-course-detail.json` — public course detail.
- `03-lms-progress-certificate.json` — lesson progress completing a course
  and issuing a certificate record (no PDF bytes — sidecar, ADR-061).
- `04-lms-cohort-enroll.json` — cohort enrollment.
- `05-lms-certificate-verify.json` — public certificate verify.
- `06-admin-lms-course-create.json` — admin course create.

## Calculators (`calc-*.json`)

- `calc-01-inheritance-gate.json` — inheritance calc gated on an approved
  rule config (no approved config → gate error).
- `calc-02-zakat-gate.json` — zakat calc gated on an approved rule config.
- `calc-03-nisab-rate-unavailable.json` — nisab rate with no price source
  (503-style `price_unavailable`).
- `calc-04-mosaref-ktb.json` — mosaref conjugation of the root k-t-b.
- `calc-05-mosaref-error.json` — mosaref engine error (bad root).
- `calc-06-inheritance-bad-madhab.json` — inheritance with an unknown
  madhhab (422).
