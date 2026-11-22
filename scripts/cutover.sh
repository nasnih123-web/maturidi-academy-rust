#!/usr/bin/env bash
# cutover.sh — Wave 9 cutover preflight for the Maturidi Academy Rust API.
#
# What this is:
#   A READ-ONLY preflight check for deploying the Rust binary
#   (maturidi-api-rs) on a NEW Railway project. It verifies env vars,
#   Postgres + schema, Redis, bundled Quran data, and every declared
#   Python sidecar / external provider, then prints a GO / DEGRADED /
#   NO-GO summary. It never starts the server, never runs migrations,
#   never writes to the database, and never deletes anything — it is
#   safe to run repeatedly.
#
# What this is NOT:
#   - It does NOT deploy anything. Creating the Railway project,
#     services, volumes, domains and env vars is a dashboard job;
#     see scripts/cutover-README.md for the exact runbook.
#   - It does NOT apply the schema. The Rust binary has NO migrations
#     (no sqlx migrate, no alembic in this repo). The production schema
#     comes from the ORIGINAL repo's alembic history
#     (apps/api/alembic, head f2a3b4c5d6e7, 50 revisions, verified
#     2026-11-22 on the rust-base checkout) run against the NEW
#     database, or from a restore of a backup of the live database.
#     This script only CHECKS that the schema is there and fails
#     loudly if it is not.
#   - It does NOT touch the live project. Step 0 refuses to run when
#     any live-project marker is present (live domains in APP_BASE_URL /
#     SIDECAR_URL / DATABASE_URL, or a matching RAILWAY_PROJECT_ID).
#     It also refuses to run until the operator sets
#     CUTOVER_ACK_NEW_PROJECT=yes, confirming the target is the NEW
#     Railway project.
#
# Usage:
#   scripts/cutover.sh [--dry-run] [--env-file FILE]
#   scripts/cutover.sh --help
#
#   --dry-run    Print what would be checked, contact nothing
#                (no DB, no Redis, no HTTP, no local file reads).
#   --env-file   Load env vars from FILE first (shell `VAR=value`
#                lines). The file is never printed. Prefer this over
#                exporting secrets in your shell history.
#
# Exit codes:
#   0  GO       — every hard check passed; no known degradation.
#   2  DEGRADED — hard checks passed, but at least one sidecar /
#                provider is DOWN. The Rust API can still start: the
#                affected features answer honestly per their ADRs
#                (503 with a route-specific sidecar code, or
#                enqueued:false) instead of faking success. Read the
#                table, decide whether the degraded features are
#                acceptable, then start the service deliberately —
#                not by accident.
#   1  NO-GO    — a hard blocker: live-project guard tripped, required
#                env var missing / placeholder secret, Postgres down,
#                schema missing, or a required tool missing.
#
# Safety rules baked in:
#   - Secret VALUES are never printed. Presence is reported as
#     set / missing / placeholder only; secret length is reported
#     only as ">= 32 chars" or "too short", never as a number.
#   - DATABASE_URL / REDIS_URL are printed only with the password
#     masked (user:***@host).
#   - Every check is read-only: SELECT 1 / information_schema /
#     pg_extension / counts, Redis PING, GET /health and public
#     reachability probes. Nothing is INSERTed, UPDATEd or DELETEd.
#
# Sources (all verified from the code, 2026-11-22):
#   - Env vars + defaults: rust/app/src/lib.rs (Config::from_env),
#     rust/app/src/routes_admin.rs (GITHUB_*, BUILDS_*, S3_*_PUBLIC,
#     SMTP_HOST/SMS_* status reads), rust/app/src/captcha.rs
#     (TURNSTILE_SECRET_KEY fail-closed), rust/app/src/routes_finance.rs
#     (ZarinPal/IDPay/Wallex/CoinGecko URLs), rust/app/src/quran_providers.rs
#     (AlQuran Cloud base URL), rust/app/src/routes_calculators.rs +
#     routes_widgets.rs (NISAB_PRICE_URL).
#   - ADRs: rust/DECISIONS.md — ADR-004 (new Railway only), ADR-013
#     (old proxy 502 — historical; the proxy is gone since ADR-133),
#     ADR-015 (local storage only), ADR-019 (hash embeddings),
#     ADR-026/031/046/076 (superseded by ADR-134: sidecar contract),
#     ADR-045 (subtitles sidecar), ADR-051 (scanner), ADR-053
#     (library media sidecar), ADR-061 (certificate PDF sidecar),
#     ADR-078 (ZarinPal/IDPay live), ADR-114/115/116 (telegram),
#     ADR-125 (ARQ/dispatcher stays Python), ADR-133 (proxy removed;
#     ORIGIN_URL deleted), ADR-134 (sidecar contract, SIDECAR_URL),
#     ADR-135 (/ws/events native), ADR-136 (/metrics native),
#     ADR-137 (/api/v1/health native), ADR-148 (Wave 9 PR-1
#     reconciliation corrections).
#   - Bundled data: rust/app/src/{adhan,hifz_logic,quran_providers,
#     routes_quran}.rs `include_str!("../data/quran/...")` — the files
#     are compiled INTO the binary, so this step validates the source
#     tree the binary was built from.
#   - Schema: original repo apps/api/alembic (50 revisions, one
#     linear chain, head f2a3b4c5d6e7 — re-verified 2026-11-22 on the
#     locked rust-base checkout, ADR-148); extensions `vector` +
#     `pg_trgm` from alembic/versions/e2b3c4d5f6a7_audit_b03_pgvector.py.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DATA_DIR="${BUNDLED_DATA_DIR:-$REPO_ROOT/rust/app/data/quran}"

DRY_RUN=0
ENV_FILE=""

# ---------------------------------------------------------------- helpers
HARD_FAILS=0
DEGRADED_COUNT=0
WARN_COUNT=0
declare -a DEGRADED_ITEMS=()
declare -a WARN_ITEMS=()

say()    { printf '%s\n' "$*"; }
ok()     { printf '  [OK]   %s\n' "$*"; }
info()   { printf '  [info] %s\n' "$*"; }
warn()   { printf '  [WARN] %s\n' "$*"; WARN_COUNT=$((WARN_COUNT + 1)); WARN_ITEMS+=("$*"); }
hard()   { printf '  [FAIL] %s\n' "$*"; HARD_FAILS=$((HARD_FAILS + 1)); }
degraded() { printf '  [DOWN] %s\n' "$*"; DEGRADED_COUNT=$((DEGRADED_COUNT + 1)); DEGRADED_ITEMS+=("$*"); }

# Mask the password in a URL for display: postgres://user:pass@host -> user:***@host
redact_url() {
    printf '%s' "$1" | sed -E 's#://([^:/@]+):[^@]*@#://\1:***@#'
}

# True if $1 is unset or empty/whitespace-only.
is_unset() { [ -z "${1:-}" ] || [ -z "$(printf '%s' "$1" | tr -d '[:space:]')" ]; }

# Secret strength gate, mirroring Python's assert_production_secrets_ok
# (apps/api/app/core/config.py): placeholder / empty / <32 chars / known
# weak value is a failure in production. Never prints the value or its length.
secret_check() { # $1=name $2=value
    local name="$1" value="${2:-}"
    if is_unset "$value"; then hard "$name is not set"; return; fi
    case "$value" in
        CHANGE_ME|CHANGE_ME*) hard "$name is a placeholder (CHANGE_ME)"; return ;;
    esac
    local lower
    lower="$(printf '%s' "$value" | tr '[:upper:]' '[:lower:]')"
    case "$lower" in
        password|secret|changeme|12345678|qwerty|letmein|admin|minioadmin|test|dev|development)
            hard "$name is a known weak/default value"; return ;;
    esac
    if [ "${#value}" -lt 32 ]; then hard "$name is too short (< 32 chars)"; return; fi
    ok "$name is set (>= 32 chars)"
}

usage() {
    sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        --env-file) ENV_FILE="${2:-}"; [ -n "$ENV_FILE" ] || { say "ERROR: --env-file needs a path"; exit 1; }; shift 2 ;;
        --env-file=*) ENV_FILE="${1#--env-file=}"; shift ;;
        --help|-h) usage; exit 0 ;;
        *) say "ERROR: unknown argument: $1 (see --help)"; exit 1 ;;
    esac
done

if [ -n "$ENV_FILE" ]; then
    if [ ! -f "$ENV_FILE" ]; then say "ERROR: env file not found: $ENV_FILE"; exit 1; fi
    # shellcheck disable=SC1090
    set -a; . "$ENV_FILE"; set +a
    say "Loaded env from $ENV_FILE (values not printed)."
fi

say "=================================================================="
say "Maturidi Academy (Rust) — Wave 9 cutover preflight"
say "Repo: $REPO_ROOT"
if [ "$DRY_RUN" -eq 1 ]; then
    say "Mode: --dry-run (prints the plan only; contacts nothing)"
fi
say "=================================================================="

# ============================================================ Step 0: guard
say ""
say "Step 0 — Live-project guard (refuses to run against the live project)"
if [ "$DRY_RUN" -eq 1 ]; then
    say "  [dry-run] Would check: CUTOVER_ACK_NEW_PROJECT=yes is set;"
    say "  [dry-run] Would refuse if APP_BASE_URL / SIDECAR_URL / DATABASE_URL"
    say "            contain a live domain (api.matoridi.ir, academy.matoridi.ir,"
    say "            academy.hosseinnasanih.ir), or if RAILWAY_PROJECT_ID matches"
    say "            LIVE_RAILWAY_PROJECT_ID, or RAILWAY_ENVIRONMENT matches"
    say "            LIVE_RAILWAY_ENVIRONMENT (when those markers are set)."
else
    # The operator must positively confirm the target is the NEW project.
    if [ "${CUTOVER_ACK_NEW_PROJECT:-}" != "yes" ]; then
        hard "CUTOVER_ACK_NEW_PROJECT is not 'yes'. Set CUTOVER_ACK_NEW_PROJECT=yes only after verifying in the Railway dashboard that the target project is the NEW Rust project, not the live one. Refusing to run."
    else
        ok "CUTOVER_ACK_NEW_PROJECT=yes (operator confirmed the NEW project)"
    fi
    # Live domains (MEMORY.md, verified 2026-11-20): api.matoridi.ir is the
    # live API; academy.matoridi.ir is the live web. academy.hosseinnasanih.ir
    # was REMOVED at the user's order but is still a live-project marker.
    LIVE_DOMAINS="api.matoridi.ir academy.matoridi.ir academy.hosseinnasanih.ir"
    for var in APP_BASE_URL SIDECAR_URL DATABASE_URL REDIS_URL; do
        val="${!var:-}"
        [ -n "$val" ] || continue
        for dom in $LIVE_DOMAINS; do
            case "$val" in
                *"$dom"*) hard "$var points at live domain $dom — this looks like the LIVE project, not the new one. Refusing to run." ;;
            esac
        done
    done
    if [ -n "${LIVE_RAILWAY_PROJECT_ID:-}" ] && [ -n "${RAILWAY_PROJECT_ID:-}" ] \
        && [ "$LIVE_RAILWAY_PROJECT_ID" = "$RAILWAY_PROJECT_ID" ]; then
        hard "RAILWAY_PROJECT_ID matches LIVE_RAILWAY_PROJECT_ID — this is the live project. Refusing to run."
    elif [ -z "${LIVE_RAILWAY_PROJECT_ID:-}" ]; then
        warn "LIVE_RAILWAY_PROJECT_ID is not set, so the Railway project-ID check was skipped; the live-domain check above is the only guard. Set LIVE_RAILWAY_PROJECT_ID to the live project's ID for a second guard."
    else
        ok "RAILWAY_PROJECT_ID does not match the live project ID"
    fi
    if [ -n "${LIVE_RAILWAY_ENVIRONMENT:-}" ] && [ -n "${RAILWAY_ENVIRONMENT:-}" ] \
        && [ "$LIVE_RAILWAY_ENVIRONMENT" = "$RAILWAY_ENVIRONMENT" ]; then
        hard "RAILWAY_ENVIRONMENT matches LIVE_RAILWAY_ENVIRONMENT — this is the live environment. Refusing to run."
    fi
    if [ -n "${LIVE_DATABASE_HOST:-}" ]; then
        case "${DATABASE_URL:-}" in
            *"$LIVE_DATABASE_HOST"*) hard "DATABASE_URL host matches LIVE_DATABASE_HOST — this is the LIVE database. Refusing to run." ;;
        esac
    fi
fi

if [ "$HARD_FAILS" -gt 0 ]; then
    say ""
    say "NO-GO: live-project guard failed. Fix the target and re-run. Nothing was checked or changed."
    exit 1
fi

# ==================================================== Step 1: env preflight
say ""
say "Step 1 — Environment preflight (values are NEVER printed)"
say "  Source of truth: rust/app/src/lib.rs Config::from_env (+ direct env reads"
say "  in routes_admin.rs / captcha.rs / routes_calculators.rs / routes_widgets.rs)."

if [ "$DRY_RUN" -eq 1 ]; then
    say "  [dry-run] Would check presence/format (never values) of:"
    say "  [dry-run]   REQUIRED: DATABASE_URL, JWT_SECRET, ENCRYPTION_KEY,"
    say "  [dry-run]     STORAGE_SIGNING_SECRET, APP_ENV=production, APP_BASE_URL"
    say "  [dry-run]     (new public API origin), REDIS_URL (new Redis), SIDECAR_URL"
    say "  [dry-run]     (NEW Python sidecar service, never the live API; optional —"
    say "  [dry-run]     unset means the 8 sidecar routes answer 503, ADR-134), PORT,"
    say "  [dry-run]     STORAGE_PROVIDER=local, STORAGE_LOCAL_DIR."
    say "  [dry-run]   CONDITIONAL: TURNSTILE_SECRET_KEY (if CAPTCHA_ENABLED != false),"
    say "  [dry-run]     GITHUB_DISPATCH_TOKEN + GITHUB_REPO (only for /admin/builds/rebuild),"
    say "  [dry-run]     BUILDS_SERVICE_TOKEN (only for the build-release callback)."
    say "  [dry-run]   OPTIONAL: RUST_API_URL (probe a running Rust binary in Step 7;"
    say "  [dry-run]     when unset, Step 7 is skipped), NISAB_PRICE_URL, ENCRYPTION_KEYS,"
    say "  [dry-run]     TRUSTED_PROXIES,"
    say "  [dry-run]     TURNSTILE_SITE_KEY, ACCESS_TOKEN_EXPIRE_MINUTES, ENCRYPTION_KEY_VERSION."
    say "  [dry-run]   Would also warn: RESEND_API_KEY / SMTP_* / SMS_* / SENTRY_DSN /"
    say "  [dry-run]     OTEL_EXPORTER_OTLP_ENDPOINT / CLAMAV_* / S3_* are NOT read by the"
    say "  [dry-run]     Rust binary (see README §5). CORS_ALLOW_ORIGINS IS read — but only"
    say "  [dry-run]     for the /ws/events Origin check (ADR-135); the widget CORS"
    say "  [dry-run]     wildcard is still deploy-layer (ADR-121)."
else
    # --- hard requirements (no usable default in the Rust binary) ---
    if is_unset "${DATABASE_URL:-}"; then
        hard "DATABASE_URL is not set (the Rust binary has no default; without it every native route answers 500)"
    else
        case "$DATABASE_URL" in
            postgres://*|postgresql://*) ok "DATABASE_URL is set (postgres, $(redact_url "$DATABASE_URL"))" ;;
            sqlite://*|sqlite+aiosqlite://*) hard "DATABASE_URL is sqlite — test-only. Production cutover needs postgres://." ;;
            *) hard "DATABASE_URL has an unsupported scheme (Rust accepts only postgres:// / postgresql:// / sqlite://)" ;;
        esac
    fi
    secret_check "JWT_SECRET" "${JWT_SECRET:-}"
    secret_check "ENCRYPTION_KEY" "${ENCRYPTION_KEY:-}"
    if is_unset "${STORAGE_SIGNING_SECRET:-}"; then
        hard "STORAGE_SIGNING_SECRET is not set (production must set it; empty means signed download URLs fall back to JWT_SECRET in dev only)"
    else
        if [ "${STORAGE_SIGNING_SECRET:-}" = "${JWT_SECRET:-}" ]; then
            hard "STORAGE_SIGNING_SECRET must not equal JWT_SECRET (secret reuse)"
        elif [ "${#STORAGE_SIGNING_SECRET}" -lt 32 ]; then
            hard "STORAGE_SIGNING_SECRET is too short (< 32 chars)"
        else
            ok "STORAGE_SIGNING_SECRET is set (>= 32 chars, differs from JWT_SECRET)"
        fi
    fi
    if [ "${APP_ENV:-dev}" != "production" ]; then
        hard "APP_ENV is '${APP_ENV:-dev}' (default dev) — cutover requires APP_ENV=production"
    else
        ok "APP_ENV=production"
    fi
    if is_unset "${APP_BASE_URL:-}"; then
        hard "APP_BASE_URL is not set (default is http://localhost:8000, which is wrong for production)"
    else
        case "$APP_BASE_URL" in
            http://localhost*|http://127.0.0.1*) hard "APP_BASE_URL is a localhost URL ($APP_BASE_URL) — must be the NEW public API origin" ;;
            https://*) ok "APP_BASE_URL is set to a public https origin" ;;
            *) warn "APP_BASE_URL is set but is not https ($APP_BASE_URL) — check it is the new public API origin" ;;
        esac
    fi
    if is_unset "${REDIS_URL:-}"; then
        hard "REDIS_URL is not set (default redis://localhost:6379/0 is wrong for production; auth rate-limiting fails closed without Redis)"
    else
        ok "REDIS_URL is set ($(redact_url "$REDIS_URL"))"
    fi
    # SIDECAR_URL is OPTIONAL (no default in the Rust binary): unset means
    # the 8 sidecar-contract routes answer honest 503s with route-specific
    # codes (ADR-134) — the API still starts. Set it to the NEW Python
    # sidecar service URL, never the live API.
    if is_unset "${SIDECAR_URL:-}"; then
        warn "SIDECAR_URL is not set. The 8 sidecar routes (fatwa/ask, hifz ASR grade/grade-word/voice-search/test-answer, voice transcribe/ask, receipt PDF) answer 503 with their route-specific codes until the NEW Python sidecar service is set. Set SIDECAR_URL to the new sidecar service, never the live API."
        DEGRADED_COUNT=$((DEGRADED_COUNT + 1)); DEGRADED_ITEMS+=("SIDECAR_URL unset — the 8 sidecar routes answer 503 (ADR-134)")
    else
        case "$SIDECAR_URL" in
            http://127.0.0.1*|http://localhost*) warn "SIDECAR_URL is localhost ($SIDECAR_URL) — correct only if the Python sidecar runs on the same host; on Railway it must be the sidecar service URL" ;;
            *) ok "SIDECAR_URL is set (Python sidecar service)" ;;
        esac
    fi
    # --- values with defaults that must still be right ---
    if [ "${STORAGE_PROVIDER:-local}" != "local" ]; then
        hard "STORAGE_PROVIDER is '${STORAGE_PROVIDER}' — the Rust binary implements ONLY 'local' (ADR-015); anything else answers 503 storage_not_configured"
    else
        ok "STORAGE_PROVIDER=local (only provider the Rust binary implements, ADR-015)"
    fi
    info "STORAGE_LOCAL_DIR=${STORAGE_LOCAL_DIR:-./storage} — on Railway this path needs a persistent Volume attached, or uploaded files vanish on redeploy (see README §3)."
    if [ -n "${PORT:-}" ]; then ok "PORT is set (Railway injects it; default 8000)"; else info "PORT is unset — the binary defaults to 8000 (Railway normally injects PORT)"; fi
    # --- conditional requirements ---
    if [ "${CAPTCHA_ENABLED:-true}" != "false" ]; then
        if is_unset "${TURNSTILE_SECRET_KEY:-}"; then
            hard "CAPTCHA_ENABLED is true but TURNSTILE_SECRET_KEY is not set — registration fails closed with 503 captcha_not_configured (captcha.rs)"
        else
            ok "TURNSTILE_SECRET_KEY is set (captcha enabled)"
        fi
    else
        warn "CAPTCHA_ENABLED=false — registration has no captcha. Only acceptable if that is a deliberate decision."
    fi
    if [ "${CLAMAV_ALLOW_INSECURE_DEV_BYPASS:-false}" = "true" ]; then
        hard "CLAMAV_ALLOW_INSECURE_DEV_BYPASS=true in a production cutover — the insecure dev bypass must never be on in production (uploads use the ADR-051 hash+EICAR gate instead)"
    fi
    if [ -n "${GITHUB_DISPATCH_TOKEN:-}" ] || [ -n "${GITHUB_REPO:-}" ]; then
        if [ -z "${GITHUB_DISPATCH_TOKEN:-}" ] || [ -z "${GITHUB_REPO:-}" ]; then
            warn "Only one of GITHUB_DISPATCH_TOKEN / GITHUB_REPO is set — /admin/builds/rebuild answers 503 build_provider_not_configured until both are set (ADR-087)"
        else
            ok "GITHUB_DISPATCH_TOKEN + GITHUB_REPO are set (build rebuild dispatch, ADR-087)"
        fi
    else
        info "GITHUB_DISPATCH_TOKEN / GITHUB_REPO are unset — /admin/builds/rebuild will honestly 503 (ADR-087). Fine unless CRM rebuilds are needed."
    fi
    if [ -n "${BUILDS_SERVICE_TOKEN:-}" ]; then ok "BUILDS_SERVICE_TOKEN is set (build-release callback auth)"; else info "BUILDS_SERVICE_TOKEN is unset — the CI build-release callback is inactive (only the admin user path works)"; fi
    if [ -n "${NISAB_PRICE_URL:-}" ]; then info "NISAB_PRICE_URL override is set"; fi
    if [ -n "${ENCRYPTION_KEYS:-}" ]; then
        if [ -z "${ENCRYPTION_KEY_VERSION:-}" ]; then warn "ENCRYPTION_KEYS is set but ENCRYPTION_KEY_VERSION is empty (default 1) — verify the current key version is inside ENCRYPTION_KEYS"; else ok "ENCRYPTION_KEYS rotation map is set (version ${ENCRYPTION_KEY_VERSION:-1})"; fi
    fi
    if [ -n "${TRUSTED_PROXIES:-}" ]; then info "TRUSTED_PROXIES is set — verify it lists only the Railway proxy"; fi
    # --- vars operators expect to matter but the Rust binary does NOT read ---
    # (CORS_ALLOW_ORIGINS is NOT in this list: it IS read, for the /ws/events
    # Origin check — ADR-135/ADR-148. Only the widget wildcard is deploy-layer.)
    for v in RESEND_API_KEY SMTP_HOST SMTP_PORT SMTP_USER SMTP_PASSWORD SMTP_FROM SMS_PROVIDER SMS_API_KEY SENTRY_DSN OTEL_EXPORTER_OTLP_ENDPOINT CLAMAV_HOST S3_ENDPOINT S3_ACCESS_KEY S3_SECRET_KEY S3_BUCKET S3_REGION; do
        if [ -n "${!v:-}" ]; then
            warn "$v is set but the Rust binary does NOT read it (see README §5). Do not assume it takes effect."
        fi
    done
    if [ -n "${RESEND_API_KEY:-}" ] || [ -n "${SMTP_HOST:-}" ]; then
        warn "Email delivery does NOT happen from the Rust binary (mail.rs is a stub, ADR-015): verification / reset emails are logged, not sent. This is a known cutover gap — see README §5. Registration flows that need email will not deliver mail until a mailer/sidecar exists."
        DEGRADED_COUNT=$((DEGRADED_COUNT + 1)); DEGRADED_ITEMS+=("Email/SMS delivery is a stub in the Rust binary (ADR-015) — verification and password-reset emails are logged, not sent")
    fi
fi

# ================================================== Step 2: Postgres + Redis
say ""
say "Step 2 — Postgres connectivity (read-only probes)"
if [ "$DRY_RUN" -eq 1 ]; then
    say "  [dry-run] Would run (read-only): psql SELECT 1; SELECT version();"
    say "            pg_extension check for vector + pg_trgm;"
    say "            alembic_version check (expect f2a3b4c5d6e7);"
    say "            information_schema check for ~30 core tables;"
    say "            domain_events outbox counts (pending / dead-letter)."
    say "  [dry-run] Would also PING Redis (redis-cli, or a python3 RESP PING"
    say "            fallback) — Redis DOWN is a NO-GO for production cutover"
    say "            because auth rate-limiting fails closed without it."
else
    if is_unset "${DATABASE_URL:-}"; then
        hard "Skipping Postgres checks — DATABASE_URL is not set (see Step 1)"
    elif ! command -v psql >/dev/null 2>&1; then
        hard "psql is not installed on this machine — install PostgreSQL client 16 (or run this script where psql exists) to check the database"
    else
        if psql "$DATABASE_URL" -tA -c "SELECT 1" >/dev/null 2>&1; then
            ok "Postgres: SELECT 1 succeeded ($(redact_url "$DATABASE_URL"))"
        else
            hard "Postgres: cannot connect / SELECT 1 failed for $(redact_url "$DATABASE_URL") — check the URL, network, and that the Railway Postgres service is running"
        fi
        if [ "$HARD_FAILS" -eq 0 ]; then
            PG_VERSION="$(psql "$DATABASE_URL" -tA -c "SELECT version()" 2>/dev/null | head -n 1 || true)"
            info "Postgres version: ${PG_VERSION:-unknown}"
        fi
    fi
fi

say ""
say "Step 3 — Redis connectivity (read-only PING)"
if [ "$DRY_RUN" -eq 1 ]; then
    say "  [dry-run] Would PING the Redis from REDIS_URL (password masked)."
else
    if is_unset "${REDIS_URL:-}"; then
        hard "Skipping Redis check — REDIS_URL is not set (see Step 1)"
    elif command -v redis-cli >/dev/null 2>&1; then
        # redis-cli -u understands redis:// URLs (password stays out of the log).
        if redis-cli -u "$REDIS_URL" ping 2>/dev/null | grep -q "PONG"; then
            ok "Redis: PING returned PONG"
        else
            hard "Redis: PING failed for $(redact_url "$REDIS_URL") — auth rate-limiting fails closed without Redis in production"
        fi
    elif command -v python3 >/dev/null 2>&1; then
        if python3 - "$REDIS_URL" <<'PYEOF' 2>/dev/null
import socket, sys
from urllib.parse import urlparse
u = urlparse(sys.argv[1])
host, port, pw = u.hostname or "localhost", u.port or 6379, u.password
s = socket.create_connection((host, port), timeout=5)
if pw:
    pw_b = pw.encode()
    s.sendall(b"*2\r\n$4\r\nAUTH\r\n$" + str(len(pw_b)).encode() + b"\r\n" + pw_b + b"\r\n")
    if not s.recv(64).startswith(b"+OK"):
        sys.exit(1)
s.sendall(b"*1\r\n$4\r\nPING\r\n")
sys.exit(0 if b"PONG" in s.recv(64) else 1)
PYEOF
        then
            ok "Redis: PING returned PONG (python3 RESP probe; redis-cli not installed)"
        else
            hard "Redis: PING failed for $(redact_url "$REDIS_URL") — auth rate-limiting fails closed without Redis in production"
        fi
    else
        hard "Neither redis-cli nor python3 is available to PING Redis — install redis-cli to check it"
    fi
fi

# ==================================================== Step 4: schema check
say ""
say "Step 4 — Schema check (the Rust binary has NO migrations)"
say "  Expected: original-repo alembic history applied to the NEW database"
say "  (head f2a3b4c5d6e7, 50 revisions). TEST_SCHEMA in rust/app/src/db.rs"
say "  is TEST-ONLY (sqlite in-memory) and is never applied to production."
if [ "$DRY_RUN" -eq 1 ]; then
    say "  [dry-run] Would check alembic_version.version_num = f2a3b4c5d6e7 and the"
    say "            presence of the core tables listed below (information_schema)."
else
    if is_unset "${DATABASE_URL:-}" || ! command -v psql >/dev/null 2>&1; then
        hard "Skipping schema checks — DATABASE_URL / psql missing (see Steps 1–2)"
    elif ! psql "$DATABASE_URL" -tA -c "SELECT 1" >/dev/null 2>&1; then
        hard "Skipping schema checks — Postgres is unreachable (see Step 2)"
    else
        # Extensions required by the schema (alembic e2b3c4d5f6a7).
        # The Rust binary itself never uses pgvector (ADR-019: hash-only
        # embeddings, embedding_vec always NULL) — but the production
        # schema contains the vector column, so the extension must exist.
        EXT="$(psql "$DATABASE_URL" -tA -c "SELECT string_agg(extname, ',' ORDER BY extname) FROM pg_extension WHERE extname IN ('vector','pg_trgm')" 2>/dev/null || true)"
        case ",${EXT}," in
            *,vector,*) ok "Extension 'vector' is installed" ;;
            *) hard "Extension 'vector' is NOT installed — run CREATE EXTENSION IF NOT EXISTS vector (alembic e2b3c4d5f6a7; Railway Postgres needs the pgvector image or a superuser CREATE EXTENSION)" ;;
        esac
        case ",${EXT}," in
            *,pg_trgm,*) ok "Extension 'pg_trgm' is installed" ;;
            *) hard "Extension 'pg_trgm' is NOT installed — run CREATE EXTENSION IF NOT EXISTS pg_trgm" ;;
        esac
        # Alembic head.
        ALEM_VERSION="$(psql "$DATABASE_URL" -tA -c "SELECT version_num FROM alembic_version" 2>/dev/null || true)"
        if [ -z "$ALEM_VERSION" ]; then
            hard "alembic_version table is missing or unreadable — the schema has NOT been applied. Apply it first: from the ORIGINAL repo checkout (rust-base) run: alembic upgrade head  (against the NEW database only), or restore a backup of the live database. See README §4."
        elif [ "$ALEM_VERSION" = "f2a3b4c5d6e7" ]; then
            ok "alembic_version = f2a3b4c5d6e7 (expected head)"
        else
            hard "alembic_version = $ALEM_VERSION, expected f2a3b4c5d6e7 — the schema is at a different revision. Run 'alembic upgrade head' from the rust-base checkout (or re-check the head if the base moved)."
        fi
        # Core tables the Rust binary reads/writes (subset of TEST_SCHEMA's
        # 119 tables — one per module family, enough to catch a missing/partial
        # schema; a full 119-table diff belongs in CI, not in this script).
        CORE_TABLES="users sessions refresh_tokens platform_settings domain_events audit_logs roles permissions api_keys home_sections articles article_categories fiqh_topics fiqh_knowledge_chunks ingestion_runs library_items item_subtitles courses lessons certificates payment_intents donations hifz_attempts hifz_tests social_posts sync_changelog telegram_published religious_places user_devices notifications"
        MISSING=""
        for t in $CORE_TABLES; do
            EXISTS="$(psql "$DATABASE_URL" -tA -c "SELECT to_regclass('public.$t')" 2>/dev/null || true)"
            if [ "$EXISTS" != "$t" ]; then MISSING="$MISSING $t"; fi
        done
        if [ -n "$MISSING" ]; then
            hard "Missing core tables:${MISSING} — schema is partial. Apply the full alembic history (see README §4)."
        else
            ok "All ${CORE_TABLES} core tables exist (counted: $(printf '%s' "$CORE_TABLES" | wc -w) tables)"
        fi
        # Outbox drain state — the dispatcher (Python, ADR-125) is the only
        # consumer. Pending rows with an old age mean the dispatcher is not
        # draining; failed rows are the dead-letter pile.
        PENDING="$(psql "$DATABASE_URL" -tA -c "SELECT count(*) FROM domain_events WHERE processed_at IS NULL AND failed_at IS NULL" 2>/dev/null || echo "?")"
        OLDEST="$(psql "$DATABASE_URL" -tA -c "SELECT COALESCE(EXTRACT(EPOCH FROM (now() - min(created_at)))::bigint, 0) FROM domain_events WHERE processed_at IS NULL AND failed_at IS NULL" 2>/dev/null || echo "?")"
        DEAD="$(psql "$DATABASE_URL" -tA -c "SELECT count(*) FROM domain_events WHERE failed_at IS NOT NULL" 2>/dev/null || echo "?")"
        info "Outbox: $PENDING pending events, oldest pending ${OLDEST}s, $DEAD dead-lettered"
        if [ "$PENDING" != "?" ] && [ "$PENDING" -gt 0 ] 2>/dev/null; then
            if [ "$OLDEST" != "?" ] && [ "$OLDEST" -gt 300 ] 2>/dev/null; then
                warn "Outbox has $PENDING pending events, oldest ${OLDEST}s old — the Python dispatcher may not be draining (ADR-125). Check the dispatcher/worker service before cutover."
            else
                info "Outbox pending events are fresh (< 300s old) — dispatcher looks alive."
            fi
        fi
        if [ "$DEAD" != "?" ] && [ "$DEAD" -gt 0 ] 2>/dev/null; then
            warn "Outbox has $DEAD dead-lettered events — review them in the admin dead-letter list before cutover (they will not retry on their own)."
        fi
        # Sidecar work queues visible in the DB (verified columns only).
        INGEST_QUEUED="$(psql "$DATABASE_URL" -tA -c "SELECT count(*) FROM ingestion_runs WHERE status='queued'" 2>/dev/null || echo "?")"
        SUB_PENDING="$(psql "$DATABASE_URL" -tA -c "SELECT count(*) FROM item_subtitles WHERE status='pending'" 2>/dev/null || echo "?")"
        CERT_PENDING="$(psql "$DATABASE_URL" -tA -c "SELECT count(*) FROM certificates WHERE pdf_storage_ref IS NULL" 2>/dev/null || echo "?")"
        MEDIA_PENDING="$(psql "$DATABASE_URL" -tA -c "SELECT count(*) FROM library_items WHERE transcript_status='pending' OR ocr_status='pending'" 2>/dev/null || echo "?")"
        info "Sidecar queues in DB: ingestion_runs queued=$INGEST_QUEUED, subtitles pending=$SUB_PENDING, certificates without PDF=$CERT_PENDING, library items with pending transcript/OCR=$MEDIA_PENDING"
        info "(These counts are only meaningful if the DB already holds live data — e.g. after a restore. On a fresh empty DB they are all 0.)"
    fi
fi

# ==================================================== Step 5: bundled data
say ""
say "Step 5 — Bundled Quran data (compiled into the binary via include_str!)"
say "  Files: rust/app/data/quran/ — if any is missing from the checkout the"
say "  binary was built from, the corresponding endpoint serves wrong/empty data."
if [ "$DRY_RUN" -eq 1 ]; then
    say "  [dry-run] Would check these files exist and are non-empty (JSON files"
    say "            also parsed) under $DATA_DIR:"
    for f in tanzil_uthmani.txt fa_rowwad.json fa_taji.json tafsir/fa.mokhtasar.json tafsir/fa.saadi.json torbat_jam_table.json mushaf-lines.json timings/manifest.json timings/ar.husary/001.json timings/ar.husary/067.json; do
        say "  [dry-run]   $f"
    done
else
    if [ ! -d "$DATA_DIR" ]; then
        hard "Bundled data directory is missing: $DATA_DIR (expected rust/app/data/quran/ under the repo root $REPO_ROOT)"
    else
        for f in tanzil_uthmani.txt fa_rowwad.json fa_taji.json tafsir/fa.mokhtasar.json tafsir/fa.saadi.json torbat_jam_table.json mushaf-lines.json timings/manifest.json timings/ar.husary/001.json timings/ar.husary/067.json; do
            p="$DATA_DIR/$f"
            if [ ! -f "$p" ]; then
                hard "Bundled data file is missing: $f"
            elif [ ! -s "$p" ]; then
                hard "Bundled data file is empty: $f"
            else
                case "$f" in
                    *.json)
                        if command -v python3 >/dev/null 2>&1; then
                            if python3 -c "import json,sys; json.load(open(sys.argv[1]))" "$p" 2>/dev/null; then
                                ok "Bundled data: $f (valid JSON)"
                            else
                                hard "Bundled data file is not valid JSON: $f"
                            fi
                        else
                            ok "Bundled data: $f (exists, non-empty; python3 missing so JSON was not validated)"
                        fi
                        ;;
                    *) ok "Bundled data: $f (exists, non-empty)" ;;
                esac
            fi
        done
        info "Note: these files are compiled INTO the binary (include_str!). This step validates the checkout, not a running binary — rebuild after any data change."
    fi
fi

# ==================================================== Step 6: sidecars
say ""
say "Step 6 — Sidecars and external providers (UP/DOWN table)"
say "  A DOWN row does NOT stop the Rust API from starting — the feature"
say "  answers honestly per its ADR (503 with a route-specific sidecar"
say "  code, or enqueued:false). It DOES decide whether cutover with"
say "  that feature degraded is acceptable."
say ""
printf '  %-18s %-42s %-6s %s\n' "SIDECAR" "SERVES" "STATE" "NOTE"
printf '  %-18s %-42s %-6s %s\n' "------------------" "------------------------------------------" "------" "----"

# http_probe <url> <expect_code|any>: prints UP/DOWN, sets PROBE_CODE.
http_probe() { # $1=url $2=expect (200 or "any")
    local url="$1" expect="$2" code=""
    if ! command -v curl >/dev/null 2>&1; then PROBE_STATE="UNKNOWN (curl missing)"; return 1; fi
    # curl -w prints "000" itself on connection failure, so capture it
    # directly; do NOT append a fallback "000" (that would make "000000").
    code="$(curl --max-time 10 -s -o /dev/null -w '%{http_code}' "$url" 2>/dev/null || true)"
    [ -n "$code" ] || code="000"
    PROBE_CODE="$code"
    if [ "$code" = "000" ]; then PROBE_STATE="DOWN"; return 1; fi
    if [ "$expect" = "any" ]; then PROBE_STATE="UP"; return 0; fi
    if [ "$code" = "$expect" ]; then PROBE_STATE="UP"; return 0; fi
    PROBE_STATE="DOWN"; return 1
}

sidecar_row() { # $1=name $2=serves $3=state $4=note
    printf '  %-18s %-42s %-6s %s\n' "$1" "$2" "$3" "$4"
    case "$3" in
        DOWN*) degraded "$1 is DOWN — $4" ;;
    esac
}

if [ "$DRY_RUN" -eq 1 ]; then
    say "  [dry-run] Would probe (HTTP GET, 10s timeout, no auth):"
    say "  [dry-run]   python-sidecar: GET \$SIDECAR_URL/health (expect 200) — the sidecar"
    say "  [dry-run]                   serves exactly the 8 sidecar-contract routes:"
    say "  [dry-run]                   fatwa/ask, hifz ASR grade/grade-word/voice-search/"
    say "  [dry-run]                   test-answer, voice transcribe/ask, receipt PDF"
    say "  [dry-run]                   (ADR-134; unset/unreachable sidecar -> honest 503s)."
    say "  [dry-run]                   /api/v1/health and /metrics are NATIVE on the Rust"
    say "  [dry-run]                   binary now (ADR-136/137) and are probed in Step 7."
    say "  [dry-run]   alquran-cloud:  GET https://api.alquran.cloud/v1/surah/1 (expect 200)."
    say "  [dry-run]   zarinpal:       https://api.zarinpal.com (any HTTP response = reachable)."
    say "  [dry-run]   idpay:          https://api.idpay.ir (any HTTP response = reachable)."
    say "  [dry-run]   wallex:         https://api.wallex.ir/v1/markets (any HTTP response)."
    say "  [dry-run]   coingecko:      https://api.coingecko.com/api/v3/ping (expect 200)."
    say "  [dry-run]   youtube-oembed: https://www.youtube.com/oembed (any HTTP response)."
    say "  [dry-run]   github-api:     https://api.github.com (any HTTP response)."
    say "  [dry-run]   telegram-api:   https://api.telegram.org (any HTTP response; informational —"
    say "  [dry-run]                   sends still degrade to failed without a bot token, ADR-115)."
    say "  [dry-run]   resend-api:     https://api.resend.com (any HTTP response; informational —"
    say "  [dry-run]                   the Rust mailer is a stub regardless, ADR-015)."
    say "  [dry-run]   ARQ workers (critical/ai/media/telegram/notifications/analytics),"
    say "  [dry-run]   the subtitle worker, certificate renderer and media pipeline"
    say "  [dry-run]   have NO HTTP endpoint: they are checked via the DB queue counts"
    say "  [dry-run]   in Step 4 + the Railway dashboard/services list (see README §6)."
    say "  [dry-run]   The dispatcher runs INSIDE the Python sidecar API process"
    say "  [dry-run]   (ADR-125), not as a separate service."
    say "  [dry-run]   /ws/events is native on the Rust binary (ADR-135) and cannot be"
    say "  [dry-run]   probed with curl — manual check with a real WS client (README §7)."
else
    # --- Python sidecar (serves exactly the 8 sidecar-contract routes) ---
    if is_unset "${SIDECAR_URL:-}"; then
        sidecar_row "python-sidecar" "8 sidecar routes only (ADR-134)" "DOWN" "SIDECAR_URL unset — the 8 sidecar routes answer 503 with route-specific codes (ADR-134)"
    elif http_probe "${SIDECAR_URL%/}/health" "200"; then
        sidecar_row "python-sidecar" "8 sidecar routes only (ADR-134)" "UP" "GET /health -> 200 (fatwa/ask, hifz ASR, voice STT, receipt PDF served by the sidecar)"
    else
        sidecar_row "python-sidecar" "8 sidecar routes only (ADR-134)" "DOWN" "GET /health -> ${PROBE_CODE:-000} (ADR-134 degraded: the 8 sidecar routes answer 503)"
    fi
    # --- External providers (reachability only; no auth, no payment) ---
    if http_probe "https://api.alquran.cloud/v1/surah/1" "200"; then
        sidecar_row "alquran-cloud" "Quran translation/tafsir/audio (default provider)" "UP" "surah 1 -> 200 (ADR-041: default editions need the network)"
    else
        sidecar_row "alquran-cloud" "Quran translation/tafsir/audio (default provider)" "DOWN" "unreachable (${PROBE_CODE:-000}); default-edition surah/ayah calls answer 503 quran_provider_unavailable — bundled editions (fa.rowwad/fa.mokhtasar) still work"
    fi
    if http_probe "https://api.zarinpal.com/pg/v4/payment/request.json" "any"; then
        sidecar_row "zarinpal" "Online donations (ADR-078)" "UP" "reachable (HTTP ${PROBE_CODE}); gateway key still comes from settings"
    else
        sidecar_row "zarinpal" "Online donations (ADR-078)" "DOWN" "unreachable; ZarinPal donations answer 503 gateway_unconfigured / gateway error"
    fi
    if http_probe "https://api.idpay.ir/v1.1/payment" "any"; then
        sidecar_row "idpay" "Online donations (ADR-078)" "UP" "reachable (HTTP ${PROBE_CODE}); gateway key still comes from settings"
    else
        sidecar_row "idpay" "Online donations (ADR-078)" "DOWN" "unreachable; IDPay donations answer 503 gateway_unconfigured / gateway error"
    fi
    if http_probe "https://api.wallex.ir/v1/markets" "any"; then
        sidecar_row "wallex" "FX rates for donations (ADR-079)" "UP" "reachable (HTTP ${PROBE_CODE})"
    else
        sidecar_row "wallex" "FX rates for donations (ADR-079)" "DOWN" "unreachable; FX conversion falls back to CoinGecko, then honest error (never a stale invented rate)"
    fi
    if http_probe "https://api.coingecko.com/api/v3/ping" "200"; then
        sidecar_row "coingecko" "FX fallback + nisab prices (ADR-079)" "UP" "ping -> 200"
    else
        sidecar_row "coingecko" "FX fallback + nisab prices (ADR-079)" "DOWN" "unreachable; FX/nisab price fetches fail honestly"
    fi
    if http_probe "https://www.youtube.com/oembed?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3DdQw4w9WgXcQ&format=json" "any"; then
        sidecar_row "youtube-oembed" "External video titles (ADR-052)" "UP" "reachable (HTTP ${PROBE_CODE}); a title failure still yields honest 422 title_required, never a fabricated title"
    else
        sidecar_row "youtube-oembed" "External video titles (ADR-052)" "DOWN" "unreachable; external-video creates without a manual title fail 422 title_required"
    fi
    if http_probe "https://api.github.com" "any"; then
        sidecar_row "github-api" "Build rebuild dispatch (ADR-087)" "UP" "reachable (HTTP ${PROBE_CODE})"
    else
        sidecar_row "github-api" "Build rebuild dispatch (ADR-087)" "DOWN" "unreachable; /admin/builds/rebuild cannot dispatch (503)"
    fi
    if http_probe "https://api.telegram.org" "any"; then
        sidecar_row "telegram-api" "Telegram Bot API reachability ONLY (ADR-115)" "UP" "reachable (HTTP ${PROBE_CODE}) — informational: sends still degrade to failed without a bot token in settings"
    else
        sidecar_row "telegram-api" "Telegram Bot API reachability ONLY (ADR-115)" "DOWN" "unreachable — Telegram sends/distribution cannot work at all"
    fi
    if http_probe "https://api.resend.com" "any"; then
        sidecar_row "resend-api" "Email reachability ONLY (ADR-015)" "UP" "reachable (HTTP ${PROBE_CODE}) — informational: the Rust mailer is a STUB, so reachability changes nothing"
    else
        sidecar_row "resend-api" "Email reachability ONLY (ADR-015)" "DOWN" "unreachable — and the Rust mailer is a stub regardless (ADR-015)"
    fi
    # --- Workers with no HTTP endpoint: report what the DB + dashboard say ---
    say ""
    say "  Workers with NO HTTP endpoint (verify in the Railway dashboard / service logs —"
    say "  see README §6). DB queue counts from Step 4 are the only in-script signal:"
    say "    - dispatcher (ADR-125): runs INSIDE the Python sidecar API process"
    say "      (lifespan task); it consumes domain_events outbox rows. Check the"
    say "      sidecar service is running and the pending count in Step 4 is draining."
    say "      (The Rust binary's own WS poller only READS domain_events for its"
    say "      native /ws/events clients; it never marks rows processed.)"
    say "    - worker-ai (ADR-022/103): consumes ingestion_runs queued rows."
    say "    - worker-media (ADR-045/053): consumes item_subtitles pending + library"
    say "      transcript/OCR pending rows (faster-whisper + ffmpeg + tesseract)."
    say "    - certificate renderer (ADR-061): consumes certificates with"
    say "      pdf_storage_ref IS NULL (reportlab + Arabic fonts)."
    say "    - worker-telegram / worker-notifications / worker-analytics (ADR-116/125):"
    say "      consume their ARQ queues; nothing in the DB proves they are running —"
    say "      check Railway service status + logs."
    say "    - /ws/events (ADR-135, native on the Rust binary) cannot be probed with"
    say "      curl: open a real WS client against RUST_API_URL with a ws-ticket"
    say "      (POST /api/v1/auth/ws-ticket) — see README §7."
fi

# ==================================================== Step 7: Rust API live endpoints
# Probes the RUNNING Rust binary — only when RUST_API_URL is set (e.g.
# a local build, or the new Railway service URL after deploy). When it
# is unset this step is skipped with an info line: the preflight is
# still meaningful (env/DB/Redis/schema/sidecars), it just cannot prove
# the binary answers. Every failure here is a HARD failure: if the
# binary is up and answers wrong, the deploy is broken, not degraded.
say ""
say "Step 7 — Rust API live endpoints (only when RUST_API_URL is set)"
if [ "$DRY_RUN" -eq 1 ]; then
    say "  [dry-run] Would probe, when RUST_API_URL is set (else skip):"
    say "  [dry-run]   GET \$RUST_API_URL/health -> 200 {status: ok}"
    say "  [dry-run]   GET \$RUST_API_URL/ready -> 200 {status: ready, db: ok}"
    say "  [dry-run]   GET \$RUST_API_URL/api/v1/health -> 200, media_tools.ready false"
    say "  [dry-run]   GET \$RUST_API_URL/metrics -> 200 text/plain, has http_requests_total"
    say "  [dry-run]   GET \$RUST_API_URL/openapi.json -> 200, has 471 route+method combos"
    say "  [dry-run]   GET \$RUST_API_URL/static/__cutover_probe__ -> 404"
    say "  [dry-run]   GET \$RUST_API_URL/docs -> 404 (dev-only in Python too)"
    say "  [dry-run]   GET \$RUST_API_URL/__cutover_unknown_path__ -> 404 http_error envelope"
    say "  [dry-run]   /ws/events cannot be probed with curl — manual WS check (README §7)."
elif is_unset "${RUST_API_URL:-}"; then
    info "RUST_API_URL is not set — skipping the live-endpoint probe. Set it to a running Rust binary (local build or the new service URL) to prove the binary answers."
else
    if ! command -v curl >/dev/null 2>&1; then
        hard "curl is not installed — cannot probe RUST_API_URL"
    else
        API="${RUST_API_URL%/}"
        # --- /health ---
        if [ "$(curl --max-time 10 -s -o /dev/null -w '%{http_code}' "$API/health" 2>/dev/null || echo 000)" = "200" ]; then
            if curl --max-time 10 -s "$API/health" 2>/dev/null | grep -q '"status"[[:space:]]*:[[:space:]]*"ok"'; then
                ok "GET /health -> 200 {status: ok}"
            else
                hard "GET /health -> 200 but the body is not {status: ok}"
            fi
        else
            hard "GET /health did not answer 200 — is the Rust binary running at $API ?"
        fi
        # --- /ready ---
        READY_BODY="$(curl --max-time 10 -s "$API/ready" 2>/dev/null || true)"
        READY_CODE="$(curl --max-time 10 -s -o /dev/null -w '%{http_code}' "$API/ready" 2>/dev/null || echo 000)"
        if [ "$READY_CODE" = "200" ] && printf '%s' "$READY_BODY" | grep -q '"status"[[:space:]]*:[[:space:]]*"ready"'; then
            ok "GET /ready -> 200 {status: ready}"
        else
            hard "GET /ready -> ${READY_CODE:-000} (expected 200 {status: ready}) — check the DB the binary points at"
        fi
        # --- /api/v1/health (native, media_tools honestly all-unavailable) ---
        HEALTH1_BODY="$(curl --max-time 10 -s "$API/api/v1/health" 2>/dev/null || true)"
        HEALTH1_CODE="$(curl --max-time 10 -s -o /dev/null -w '%{http_code}' "$API/api/v1/health" 2>/dev/null || echo 000)"
        if [ "$HEALTH1_CODE" = "200" ] && printf '%s' "$HEALTH1_BODY" | grep -q '"ready"[[:space:]]*:[[:space:]]*false'; then
            ok "GET /api/v1/health -> 200, media_tools.ready false (ADR-137)"
        else
            hard "GET /api/v1/health -> ${HEALTH1_CODE:-000} (expected 200 with media_tools.ready false)"
        fi
        # --- /metrics (native Prometheus exposition) ---
        METRICS_BODY="$(curl --max-time 10 -s "$API/metrics" 2>/dev/null || true)"
        METRICS_CODE="$(curl --max-time 10 -s -o /dev/null -w '%{http_code}' "$API/metrics" 2>/dev/null || echo 000)"
        if [ "$METRICS_CODE" = "200" ] && printf '%s' "$METRICS_BODY" | grep -q "http_requests_total"; then
            ok "GET /metrics -> 200, Prometheus exposition present (ADR-136)"
        else
            hard "GET /metrics -> ${METRICS_CODE:-000} (expected 200 with http_requests_total)"
        fi
        # --- /openapi.json (generated from the live router: 471 combos) ---
        OPENAPI_BODY="$(curl --max-time 10 -s "$API/openapi.json" 2>/dev/null || true)"
        OPENAPI_CODE="$(curl --max-time 10 -s -o /dev/null -w '%{http_code}' "$API/openapi.json" 2>/dev/null || echo 000)"
        if [ "$OPENAPI_CODE" = "200" ] && printf '%s' "$OPENAPI_BODY" | grep -q '"/api/v1/fatwa/ask"'; then
            ok "GET /openapi.json -> 200, lists the registered routes incl. /api/v1/fatwa/ask (ADR-139)"
        else
            hard "GET /openapi.json -> ${OPENAPI_CODE:-000} (expected 200 with the registered routes)"
        fi
        # --- /static missing file -> 404 ---
        if [ "$(curl --max-time 10 -s -o /dev/null -w '%{http_code}' "$API/static/__cutover_probe__.woff2" 2>/dev/null || echo 000)" = "404" ]; then
            ok "GET /static/__cutover_probe__.woff2 -> 404 (ADR-140)"
        else
            hard "GET /static/__cutover_probe__.woff2 did not answer 404"
        fi
        # --- /docs -> 404 (dev-only, like production Python) ---
        if [ "$(curl --max-time 10 -s -o /dev/null -w '%{http_code}' "$API/docs" 2>/dev/null || echo 000)" = "404" ]; then
            ok "GET /docs -> 404 (dev-only, ADR-139)"
        else
            hard "GET /docs did not answer 404"
        fi
        # --- unknown path -> 404 in Python's http_error envelope ---
        UNKNOWN_BODY="$(curl --max-time 10 -s "$API/__cutover_unknown_path__" 2>/dev/null || true)"
        UNKNOWN_CODE="$(curl --max-time 10 -s -o /dev/null -w '%{http_code}' "$API/__cutover_unknown_path__" 2>/dev/null || echo 000)"
        if [ "$UNKNOWN_CODE" = "404" ] && printf '%s' "$UNKNOWN_BODY" | grep -q '"code"[[:space:]]*:[[:space:]]*"http_error"'; then
            ok "GET /__cutover_unknown_path__ -> 404 http_error envelope (ADR-133)"
        else
            hard "GET /__cutover_unknown_path__ -> ${UNKNOWN_CODE:-000} (expected 404 http_error envelope)"
        fi
    fi
fi

# ==================================================== Final summary
say ""
say "=================================================================="
say "Cutover preflight — summary"
say "  Hard failures : $HARD_FAILS"
say "  Warnings      : $WARN_COUNT"
say "  Degraded items: $DEGRADED_COUNT"
if [ "$WARN_COUNT" -gt 0 ]; then
    say "  Warnings:"
    for w in "${WARN_ITEMS[@]}"; do say "    - $w"; done
fi
if [ "$DEGRADED_COUNT" -gt 0 ]; then
    say "  Degraded (API can still start; these features answer honestly degraded):"
    for d in "${DEGRADED_ITEMS[@]}"; do say "    - $d"; done
fi
say "=================================================================="
if [ "$DRY_RUN" -eq 1 ]; then
    say "DRY-RUN complete — nothing was contacted or changed."
    say "Run without --dry-run (with the real env) for the actual preflight."
    exit 0
fi
if [ "$HARD_FAILS" -gt 0 ]; then
    say "Result: NO-GO (exit 1). Fix the [FAIL] items above and re-run."
    say "Do NOT start traffic to the Rust service until this script says GO or DEGRADED."
    exit 1
fi
if [ "$DEGRADED_COUNT" -gt 0 ]; then
    say "Result: DEGRADED (exit 2). Hard checks passed, but the items above are down."
    say "The Rust API can start — degraded features answer 503 with their"
    say "route-specific sidecar codes, or enqueued:false, per their ADRs —"
    say "but confirm with the user that the degradation is acceptable"
    say "before pointing any traffic at it. See README §7–§8."
    exit 2
fi
say "Result: GO (exit 0). All hard checks passed and no sidecar is down."
say "Next: deploy the binary, run the README §7 verification checklist"
say "(health/ready + smoke routes), and only then consider DNS (README §8)."
exit 0
