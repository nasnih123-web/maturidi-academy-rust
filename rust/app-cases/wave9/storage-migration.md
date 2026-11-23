# Durable storage — decision, implementation, migration (local → S3)

Wave 9 PR-3 (ADR-162). This mirrors the original repo's own storage
policy (`docs/ops/storage.md`, Track 2 / §12) — the decision was
already made there; PR-3 ports it to Rust instead of re-deciding it.

## Decision: S3-compatible object storage is the production provider

- **Production: `STORAGE_PROVIDER=s3`** (any S3-compatible
  endpoint). **Development/test: `local`** (disk under
  `STORAGE_LOCAL_DIR`, default `./storage`).
- Why S3, from the product's own history: the 2026-11-19 Railway
  migration lost the bytes of 4 manual-donation receipt images —
  metadata migrated, files did not — because both APIs stored on
  local ephemeral disk and the old container was redeployed after
  the uploads. Local disk on a redeployable container is not durable
  storage; it is a cache that deletes itself. An S3-compatible bucket
  survives redeploys, is reachable from every service instance
  (API + workers + sidecar), and is the only provider the original
  already implements (`S3StorageProvider`, MinIO client).
- Why not a Railway volume: a volume is bound to one service, does
  not survive a project rebuild the way an external bucket does, and
  cannot be read by the Python sidecar/workers running as separate
  services unless they all mount the same volume — which Railway
  does not offer across services. The original's policy already
  rejects it by requiring `s3` in production.
- Which S3: the code accepts any S3-compatible endpoint via env
  (`S3_ENDPOINT` for server-side ops, `S3_ENDPOINT_PUBLIC` for the
  presigned URLs browsers receive — the internal host must never
  leak to a browser). The concrete endpoint for the new Railway
  project (Cloudflare R2 is the natural S3-compatible choice: no
  egress fees, and the project already uses Cloudflare; a self-hosted
  MinIO is the alternative) is a **PR-4 deployment decision** —
  this PR does not provision a bucket and does not name a live one.
  Until PR-4 provisions it, `STORAGE_PROVIDER=s3` against an
  unprovisioned endpoint fails fast at boot (below), never silently
  falls back to local.

## Implementation (this PR)

- `storage.rs` is now a two-provider dispatch behind the same four
  operations (`put_object`, `read_bytes`, `delete_object`,
  `presigned_url`): local (unchanged HMAC-signed behaviour) and S3
  (SigV4 over reqwest — PUT/GET/DELETE with header signing,
  presigned GET URLs with query signing, computed locally with no
  network call, exactly like the MinIO client's).
- Config (same env names as Python): `S3_ENDPOINT` (default
  `http://localhost:9000`), `S3_ENDPOINT_PUBLIC` (default "" =
  same as endpoint), `S3_ACCESS_KEY` (`minioadmin`),
  `S3_SECRET_KEY` (`minioadmin`), `S3_BUCKET` (`maturidi-media`),
  `S3_REGION` (`us-east-1`); `STORAGE_PROVIDER` (`local` | `s3` |
  `minio` — `minio` is Python's alias for `s3`).
- Boot validation (`validate_storage_config`, called from `main.rs`):
  with `STORAGE_PROVIDER=s3` (or `minio`) and
  `APP_ENV=production` the binary refuses to boot on: a missing or
  placeholder secret (`CHANGE_ME*`, `minioadmin`, weak/default
  keys), an `S3_SECRET_KEY` shorter than 32 chars, a demo endpoint
  (`localhost` / `example.*`) or demo bucket, an effective presigned
  host that is internal (`minio`, `localhost`, `127.*`, `*.local`),
  or a `STORAGE_SIGNING_SECRET` that is missing, short, or equal to
  `JWT_SECRET` / `ENCRYPTION_KEY`. In dev/test the checks are
  warnings, not boot failures — local development without an S3
  must keep working. A bad S3 config is never a silent fallback to
  local (the original's fail-fast rule, ported verbatim).
- Objects stay private: the bucket has no public policy; access is
  only via short-lived presigned URLs (default 300 s). The local
  HMAC `download-by-ref` endpoint keeps working for local objects;
  with S3, clients use the presigned URL directly (the HMAC check
  has nothing to verify, so that endpoint 403s for S3 keys — the
  original's behaviour, ported). Server-side authenticated reads
  (`read_bytes`, used by admin/transcribe paths) work on both
  providers.
- The scan gate (ADR-051: SHA-256 blocklist + EICAR + `draft` +
  manual admin review) runs in the handlers *before* `put_object`,
  so it applies identically on both providers. The 512 MB library
  cap etc. are unchanged (ADR-161).
- `/static/{*path}` (uploaded fonts) now reads through the storage
  abstraction instead of only local disk, so fonts uploaded under
  the S3 provider serve the same way they do under local.

## Migration: existing local files → S3 (procedure, PR-4)

Ported from the original's `docs/ops/storage.md` §12; the keys do
not change, only where the bytes live.

1. Full backup first: database + `STORAGE_LOCAL_DIR`
   (`infra/backup.sh` in the original repo).
2. Create the destination bucket; sync with keys mirrored exactly
   (no added prefix, so they match the keys stored in the DB):
   `mc mirror --overwrite <STORAGE_LOCAL_DIR> s3/<S3_BUCKET>/`
   (`mc` = MinIO client, configured with the same endpoint/keys.)
3. Verify by random sampling: download from S3, compare SHA-256
   against the local copy.
4. Switch `STORAGE_PROVIDER=s3` and redeploy. After the switch the
   old local `download-by-ref` URLs stop working (403) — client
   caches must expire (they are 5-minute URLs, so this clears
   itself within the hour).
5. Rollback (s3 → local) is the same path in reverse.

**Do not switch `STORAGE_PROVIDER` on a database full of local keys
without running this migration** — the keys would resolve to an
empty bucket and every file would 404.
