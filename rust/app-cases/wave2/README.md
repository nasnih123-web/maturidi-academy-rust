# Wave 2 HTTP cases (for `app record` / `app replay`)

Recorded request/response pairs for the natively ported Wave 2 routes.
Each file is one case: method, path, request body (or `-` for none),
expected status, and the key response fields pinned by
`rust/app/tests/wave2.rs`. Bodies use placeholder values where the real
ones are secrets (passwords, tokens, TOTP codes).

Auth note: `Authorization: Bearer <access>` comes from
`POST /api/v1/auth/login` after `POST /api/v1/auth/register`.
Admin cases need a user with the `admin` role plus TOTP enrolled
(`api_keys.manage` / `users.manage`); see `make_admin` in the tests.
