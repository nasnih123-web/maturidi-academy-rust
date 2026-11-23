#!/usr/bin/env bash
# Wave 9 PR-2 — locked-base Alembic check (ADR-151).
#
# Verifies, against a real Postgres, that the ORIGINAL repo at the locked
# migration base (tree hash in LOCKED_TREE) has a single Alembic head, that
# `alembic upgrade head` migrates cleanly, and that the Rust binary's
# /ready answers against the migrated schema. The same script runs in CI
# (workflow job `alembic`) and locally, so the CI gate is reproducible.
#
# Run from the Rust repo root:
#   bash scripts/alembic-check.sh
#
# Inputs (all have defaults for a local run; CI sets them explicitly):
#   LOCKED_TREE       tree hash of the locked base (default: the PR-1 value)
#                     The merge commit SHA is NOT used: it was created locally
#                     and never pushed, so GitHub cannot check it out (ADR-155).
#   ALEMBIC_HEAD      expected single head (default f2a3b4c5d6e7)
#   DATABASE_URL      asyncpg URL for Alembic
#                     (default postgresql+asyncpg://postgres:postgres@localhost:5432/alembic_check)
#   RUST_DATABASE_URL postgres:// URL for the Rust binary
#                     (default postgres://postgres:postgres@localhost:5432/alembic_check)
#   ORIGINAL_DIR      checkout of the original repo at the locked base.
#                     Default: ./original (CI) if it exists, else
#                     ../maturidi-academy (the local rust-base checkout).
#   ALEMBIC_CHECK_PORT  port for the probe binary (default 18080)
#
# Requirements: python3 + venv, cargo, curl, a running Postgres with an
# (empty) `alembic_check` database. The database must already exist — the
# CI service creates it via POSTGRES_DB; locally create it first
# (`createdb alembic_check`). No Redis is needed: /ready must still be
# 200 with redis "unavailable" (Redis never fails readiness).
#
# Exit code: 0 only if every assertion holds. Nothing here swallows a
# failure.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

LOCKED_TREE="${LOCKED_TREE:-73422149c202d8b4e0a4a1c0bcc8dba3f79239f0}"
ALEMBIC_HEAD="${ALEMBIC_HEAD:-f2a3b4c5d6e7}"
DATABASE_URL="${DATABASE_URL:-postgresql+asyncpg://postgres:postgres@localhost:5432/alembic_check}"
RUST_DATABASE_URL="${RUST_DATABASE_URL:-postgres://postgres:postgres@localhost:5432/alembic_check}"
ALEMBIC_CHECK_PORT="${ALEMBIC_CHECK_PORT:-18080}"
if [ -n "${ORIGINAL_DIR:-}" ]; then
    ORIGINAL_DIR="$ORIGINAL_DIR"
elif [ -d "$REPO_ROOT/original/apps/api/alembic" ]; then
    ORIGINAL_DIR="$REPO_ROOT/original"
elif [ -d "$REPO_ROOT/../maturidi-academy/apps/api/alembic" ]; then
    ORIGINAL_DIR="$REPO_ROOT/../maturidi-academy"
else
    echo "alembic-check: no original-repo checkout found (looked for ./original and ../maturidi-academy)" >&2
    exit 1
fi

for cmd in python3 cargo curl git; do
    command -v "$cmd" >/dev/null || { echo "alembic-check: $cmd not found on PATH" >&2; exit 1; }
done

# 1. The checkout is exactly the locked base — never a moving target.
# Identity is proven by TREE hash, not commit SHA: the locked-base merge
# commit 5d1cb02 was created locally and never pushed to the original repo
# (the original repo is never written to), so GitHub cannot check it out
# by SHA. CI recreates the base by merging PR #193 + #194 onto main
# (ADR-155); a merge of the same parents is content-identical, so its
# tree hash must equal the locked base's tree hash.
LOCKED_TREE="${LOCKED_TREE:-73422149c202d8b4e0a4a1c0bcc8dba3f79239f0}"
tree="$(git -C "$ORIGINAL_DIR" rev-parse "HEAD^{tree}")"
if [ "$tree" != "$LOCKED_TREE" ]; then
    echo "alembic-check: original checkout tree $tree != locked base tree $LOCKED_TREE" >&2
    exit 1
fi
echo "alembic-check: original checkout matches the locked base tree ($tree)"

# 2. Minimal Python env. Pins are copied from the original
# apps/api/requirements.txt — nothing else is needed to import env.py
# (app.models needs pgvector + sqlalchemy; config needs pydantic-settings).
VENV_DIR="$(mktemp -d)/alembic-venv"
python3 -m venv "$VENV_DIR"
"$VENV_DIR/bin/pip" install -q --upgrade pip
"$VENV_DIR/bin/pip" install -q \
    "alembic==1.20.0" "sqlalchemy[asyncio]==2.1.5" "asyncpg==0.32.0" \
    "pydantic==2.14.1" "pydantic-settings==2.16.1" "pgvector==0.5.1"
trap 'rm -rf "$(dirname "$VENV_DIR")"' EXIT

# 3. Single head, then upgrade.
cd "$ORIGINAL_DIR/apps/api"
export DATABASE_URL
heads="$("$VENV_DIR/bin/alembic" heads)"
if [ "$heads" != "$ALEMBIC_HEAD (head)" ]; then
    echo "alembic-check: expected single head '$ALEMBIC_HEAD (head)', got:" >&2
    echo "$heads" >&2
    exit 1
fi
echo "alembic-check: single head $ALEMBIC_HEAD confirmed"
"$VENV_DIR/bin/alembic" upgrade head

# 4. Verify the migrated schema: version row, both extensions (created
# by migration e2b3c4d5f6a7), and the table count — 120 = 119 app tables
# (the same 119 as the Rust TEST_SCHEMA, ADR-148) + alembic_version.
ALEMBIC_HEAD="$ALEMBIC_HEAD" "$VENV_DIR/bin/python" - <<'PYEOF'
import asyncio
import os
import sys

import asyncpg


async def main() -> None:
    url = os.environ["DATABASE_URL"].replace("+asyncpg", "")
    conn = await asyncpg.connect(url)
    try:
        version = await conn.fetchval("SELECT version_num FROM alembic_version")
        exts = {
            r[0]
            for r in await conn.fetch(
                "SELECT extname FROM pg_extension WHERE extname IN ('vector', 'pg_trgm')"
            )
        }
        tables = await conn.fetchval(
            "SELECT count(*) FROM information_schema.tables "
            "WHERE table_schema = 'public' AND table_type = 'BASE TABLE'"
        )
    finally:
        await conn.close()
    head = os.environ["ALEMBIC_HEAD"]
    errors = []
    if version != head:
        errors.append(f"alembic_version is {version!r}, expected {head!r}")
    if exts != {"vector", "pg_trgm"}:
        errors.append(f"extensions present are {sorted(exts)}, expected ['pg_trgm', 'vector']")
    if tables != 120:
        errors.append(f"public tables: {tables}, expected 120 (119 app + alembic_version)")
    if errors:
        print("alembic-check: migrated-schema verification failed:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        sys.exit(1)
    print(f"alembic-check: schema verified (head {version}, extensions vector+pg_trgm, 120 tables)")


asyncio.run(main())
PYEOF

# 5. The Rust binary's /ready against the migrated schema. Release build,
# like the Dockerfile's; no Redis service here on purpose.
cd "$REPO_ROOT"
cargo build --release -p maturidi-api-rs --locked
BIN="$REPO_ROOT/target/release/maturidi-api-rs"
[ -x "$BIN" ] || { echo "alembic-check: release binary missing at $BIN" >&2; exit 1; }
PORT="$ALEMBIC_CHECK_PORT" DATABASE_URL="$RUST_DATABASE_URL" \
    REDIS_URL="redis://127.0.0.1:6399/0" "$BIN" &
API_PID=$!
trap 'kill $API_PID 2>/dev/null || true; rm -rf "$(dirname "$VENV_DIR")"' EXIT
ready_json="$(mktemp)"
for _ in $(seq 1 30); do
    if curl -fsS -o "$ready_json" "http://127.0.0.1:$ALEMBIC_CHECK_PORT/ready" 2>/dev/null; then
        break
    fi
    sleep 1
done
[ -s "$ready_json" ] || { echo "alembic-check: /ready never answered" >&2; exit 1; }
READY_JSON="$ready_json" python3 - <<'PYEOF'
import json
import os
import sys

with open(os.environ["READY_JSON"]) as f:
    body = json.load(f)
errors = []
if body.get("status") != "ready":
    errors.append(f"status is {body.get('status')!r}, expected 'ready'")
if body.get("db") != "ok":
    errors.append(f"db is {body.get('db')!r}, expected 'ok'")
if body.get("redis") != "unavailable":
    errors.append(f"redis is {body.get('redis')!r}, expected 'unavailable' (no Redis in this check)")
if errors:
    print("alembic-check: /ready verification failed:", file=sys.stderr)
    for e in errors:
        print(f"  - {e}", file=sys.stderr)
    print(f"  body: {body}", file=sys.stderr)
    sys.exit(1)
print(f"alembic-check: /ready OK against the migrated schema: {body}")
PYEOF
rm -f "$ready_json"
echo "alembic-check: PASS"
