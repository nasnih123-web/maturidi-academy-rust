# Wave 3 HTTP cases (for `app record` / `app replay`)

Recorded request/response pairs for the natively ported Wave 3 routes
(home, content, articles, fatwa). Each file is one case: method, path,
request body (or `-` for none), expected status, and the key response
fields pinned by `rust/app/tests/wave3.rs`. Bodies use placeholder
values where the real ones are secrets or generated (`<uuid>`).

Auth note: `Authorization: Bearer <access>` comes from
`POST /api/v1/auth/login` after `POST /api/v1/auth/register`. Admin
cases need a user with a role carrying the permission the route
requires (`content.create` / `content.publish` for CMS,
`knowledge.manage` / `fatwa.approve` for articles and fatwa admin,
`fatwa.submit` for submissions, `platform.settings` for trusted
sources); see `grant` in the tests. The role must NOT be `admin` /
`super_admin`, or the token needs TOTP enrolment first.

Notes pinned by the cases:
- The content control-center registry is empty at runtime, so
  `GET /api/v1/content/items` is always `{"items": [], "total": 0}`
  and preview/policies always 404 `unknown_content_type`.
- The public pages list only serves `content_type=article`; a page
  created without it defaults to `content_type=page` and does not
  appear in the list (it is still served by slug).
- `POST /api/v1/fatwa/ask` is intentionally NOT native (assistant /
  mufti, out of scope for Wave 3) and stays proxied.
- Python has no HTTP route to approve a quiz question, so
  `POST /api/v1/fatwa/quiz/generate` answers 404
  `no_quiz_questions` until a question is approved out-of-band.
