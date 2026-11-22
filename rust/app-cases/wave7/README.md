# Wave 7 app-cases

HTTP request/response cases for the Wave 7 native routes (finance + admin
+ admin content center + social + sync + telegram + places + widgets).
Each file is one request with the expected status and the key response
fields; they are documentation/fixture cases, not an executable suite —
the executable coverage lives in `rust/app/tests/wave7_*.rs`.

## Finance (`fin-*`, 8 cases)

- `fin-01-donate-missing-key.json` — `POST /api/v1/finance/donate` without
  an idempotency key → 422.
- `fin-02-donate-unconfigured.json` — donation intent with no gateway
  configured → 503 `gateway_unconfigured`.
- `fin-03-callback-unknown.json` — gateway callback for an unknown
  intent → 404.
- `fin-04-donation-methods.json` — `GET /api/v1/finance/donation-methods`
  → 200, the configured methods.
- `fin-05-campaigns-public.json` — `GET /api/v1/finance/campaigns` → 200,
  public campaigns only.
- `fin-06-admin-campaigns-unauthorized.json` — admin campaign create
  without `finance.manage` → 403.
- `fin-07-webhook-bad-json.json` — gateway webhook with a bad signature /
  bad JSON → 400/422, no donation row written.
- `fin-08-fx-rate-irr.json` — `GET /api/v1/admin/finance/fx-rate?currency=IRR`
  → 200, exact string-decimal rate (IRR fixed at 1).

## Admin core (`adm-*`, 10 cases)

- `adm-01-settings-put.json` — `PUT /api/v1/admin/settings` → 200.
- `adm-02-settings-unknown.json` — unknown setting key → 422.
- `adm-03-settings-unauthorized.json` — settings write without admin
  permission → 403.
- `adm-04-roles-create.json` — `POST /api/v1/admin/roles` → 201.
- `adm-05-roles-unknown-permission.json` — role create with an unknown
  permission → 422 `unknown_permission`.
- `adm-06-builds-prereqs.json` — `GET /api/v1/admin/builds/prereqs` → 200.
- `adm-07-builds-rebuild-unconfigured.json` — rebuild with no GitHub
  token configured → 503, never a shell.
- `adm-08-users-dashboard.json` — `GET /api/v1/admin/users/dashboard`
  → 200.
- `adm-09-broadcast-preview.json` — `POST /api/v1/admin/notifications/preview`
  → 200, deduped recipient count.
- `adm-10-connections-status.json` — `GET /api/v1/admin/connections/status`
  → 200, per-service status.

## Admin content center (`acc-*`, 8 cases)

- `acc-01-adapters-unauthorized.json` — content-center list without
  `content.manage` → 403.
- `acc-02-items-unknown-domain.json` — `GET /api/v1/admin/content/items`
  with an unknown domain → 422.
- `acc-03-items-bad-channel.json` — unknown distribution channel → 422.
- `acc-04-items-bad-cursor.json` — malformed `{"t","d","ct","id"}` cursor
  → 422.
- `acc-05-bulk-bad-action.json` — bulk transition with an unknown action
  → 422.
- `acc-06-placement-missing-surface.json` — placement PUT without a
  surface → 422.
- `acc-07-distribution-bad-channel.json` — distribution policy for an
  unknown channel → 422.
- `acc-08-reindex-unsupported.json` — reindex for an unsupported domain
  → 422; supported domains answer `enqueued: false`.

## Social (`soc-*`, 8 cases)

- `soc-01-create-group.json` — `POST /api/v1/social/groups` → 201.
- `soc-02-create-group-forbidden.json` — group create without
  `social.moderate` → 403.
- `soc-03-feed.json` — `GET /api/v1/social/feed` → 200, cursor-paged.
- `soc-04-post-banned-word.json` — post containing a banned word → 422
  `content_policy_violation` (the word is never echoed).
- `soc-05-like.json` — `POST /api/v1/social/posts/{id}/like` → 200.
- `soc-06-report.json` — `POST /api/v1/social/report` → 201.
- `soc-07-report-triage.json` — `POST /api/v1/admin/social/reports/{report_id}/action`
  with `{"action": "triage"}` without `social.moderate` → 403.
- `soc-08-unread-count.json` — `GET /api/v1/social/notifications/unread-count`
  → 200.

## Sync + Telegram (`syn-*`, 8 cases)

- `syn-01-push-note.json` — idempotent push batch with a note → 200,
  replay on `client_op_id` returns the stored result.
- `syn-02-pull.json` — `GET /api/v1/sync/pull?since_seq=0` → 200.
- `syn-03-conflicts.json` — pull with a note conflict → conflict row,
  both versions kept.
- `syn-04-resolve.json` — conflict resolve → 200.
- `syn-05-offline-pack.json` — scored offline pack → 200.
- `syn-06-webhook.json` — Telegram webhook with a bad secret → 403;
  a non-`tg:` update answers `{"ok": true, "ignored": true}`.
- `syn-07-policies.json` — `GET /api/v1/telegram/distribute/policies`
  → 200.
- `syn-08-publish.json` — distribute publish with no worker → 200 with
  `enqueued: false` (never a faked queue).

## Places + widgets (`plw-*`, 8 cases)

- `plw-01-public-list-empty.json` — `GET /api/v1/places` on an empty
  table → 200, `[]`.
- `plw-02-submit-draft.json` — anonymous place submission → 201,
  `workflow_status: draft`.
- `plw-03-submit-unknown-type.json` — submission with an unknown type
  → 422.
- `plw-04-admin-create-unauthorized.json` — admin place create without
  `places.manage` → 403.
- `plw-05-public-get-missing.json` — `GET /api/v1/places/{id}` for a
  missing id → 404.
- `plw-06-widget-prayer-no-key.json` — prayer-times widget without a key
  or embed token → 401.
- `plw-07-widget-ayah-no-key.json` — ayah-of-day widget without a key or
  embed token → 401.
- `plw-08-widget-zakat-no-key.json` — zakat-mini widget without a key or
  embed token → 401.
