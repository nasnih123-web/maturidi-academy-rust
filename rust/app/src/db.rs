//! Database access — mirrors `app/core/database.py`.
//!
//! The pool is an enum: Postgres in production, sqlite in tests.
//! `sqlx::any::AnyPool` cannot be used here — it only supports
//! bool/int/float/string/blob, and Wave 2 needs `Uuid`, `DateTime<Utc>`
//! and JSON values. Instead, every query goes through the four methods
//! below, which take the SQL once (with `?` placeholders — rewritten to
//! `$1..$n` for Postgres) and a slice of `Arg`s, and return `DynRow`s
//! that decode columns on either backend with the same `try_get` call.
//! IDs and timestamps are always generated in Rust (`Uuid::new_v4`,
//! `Utc::now`) and bound explicitly, so no `RETURNING` clause is needed.
//! Expiry comparisons are done in Rust, never in SQL, because the two
//! engines store timestamps differently.

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;
use sqlx::{
    postgres::{PgArguments, PgPool, PgRow},
    sqlite::{SqliteArguments, SqlitePool, SqliteRow},
    Arguments, Decode, Row, Type,
};
use uuid::Uuid;

use crate::errors::ApiError;

/// One bound query argument. Values are owned so call sites can pass
/// temporaries (`hash_token(&raw)`, `json!({...})`, `utc_now()`).
#[derive(Debug, Clone)]
pub enum Arg {
    Uuid(Uuid),
    Str(String),
    Bool(bool),
    I64(i64),
    F64(f64),
    Dt(DateTime<Utc>),
    Date(NaiveDate),
    Json(Value),
    /// SQL NULL, typed: Postgres rejects a text-typed NULL for jsonb /
    /// uuid / timestamptz columns, so `None` must carry the target type.
    NullText,
    NullUuid,
    NullTime,
    NullDate,
    NullJson,
    NullBool,
    NullInt,
    NullFloat,
}

/// Convert a Rust value into an `Arg`. Implemented for the types the
/// Wave 2 queries bind, including `Option<T>` (NULL) and references.
pub trait IntoArg {
    fn into_arg(self) -> Arg;
    /// The `Arg` for `None` of this type (Postgres needs a typed NULL).
    fn null_arg() -> Arg;
}

impl IntoArg for Uuid {
    fn into_arg(self) -> Arg {
        Arg::Uuid(self)
    }
    fn null_arg() -> Arg {
        Arg::NullUuid
    }
}
impl IntoArg for &Uuid {
    fn into_arg(self) -> Arg {
        Arg::Uuid(*self)
    }
    fn null_arg() -> Arg {
        Arg::NullUuid
    }
}
impl IntoArg for String {
    fn into_arg(self) -> Arg {
        Arg::Str(self)
    }
    fn null_arg() -> Arg {
        Arg::NullText
    }
}
impl IntoArg for &String {
    fn into_arg(self) -> Arg {
        Arg::Str(self.clone())
    }
    fn null_arg() -> Arg {
        Arg::NullText
    }
}
impl IntoArg for &str {
    fn into_arg(self) -> Arg {
        Arg::Str(self.to_string())
    }
    fn null_arg() -> Arg {
        Arg::NullText
    }
}
impl IntoArg for bool {
    fn into_arg(self) -> Arg {
        Arg::Bool(self)
    }
    fn null_arg() -> Arg {
        Arg::NullBool
    }
}
impl IntoArg for &bool {
    fn into_arg(self) -> Arg {
        Arg::Bool(*self)
    }
    fn null_arg() -> Arg {
        Arg::NullBool
    }
}
impl IntoArg for i64 {
    fn into_arg(self) -> Arg {
        Arg::I64(self)
    }
    fn null_arg() -> Arg {
        Arg::NullInt
    }
}
impl IntoArg for &i64 {
    fn into_arg(self) -> Arg {
        Arg::I64(*self)
    }
    fn null_arg() -> Arg {
        Arg::NullInt
    }
}
impl IntoArg for i32 {
    fn into_arg(self) -> Arg {
        Arg::I64(self as i64)
    }
    fn null_arg() -> Arg {
        Arg::NullInt
    }
}
impl IntoArg for f64 {
    fn into_arg(self) -> Arg {
        Arg::F64(self)
    }
    fn null_arg() -> Arg {
        Arg::NullFloat
    }
}
impl IntoArg for DateTime<Utc> {
    fn into_arg(self) -> Arg {
        Arg::Dt(self)
    }
    fn null_arg() -> Arg {
        Arg::NullTime
    }
}
impl IntoArg for NaiveDate {
    fn into_arg(self) -> Arg {
        Arg::Date(self)
    }
    fn null_arg() -> Arg {
        Arg::NullDate
    }
}
impl IntoArg for &DateTime<Utc> {
    fn into_arg(self) -> Arg {
        Arg::Dt(*self)
    }
    fn null_arg() -> Arg {
        Arg::NullTime
    }
}
impl IntoArg for Value {
    fn into_arg(self) -> Arg {
        Arg::Json(self)
    }
    fn null_arg() -> Arg {
        Arg::NullJson
    }
}
impl IntoArg for &Value {
    fn into_arg(self) -> Arg {
        Arg::Json(self.clone())
    }
    fn null_arg() -> Arg {
        Arg::NullJson
    }
}
impl<T: IntoArg> IntoArg for Option<T> {
    fn into_arg(self) -> Arg {
        match self {
            Some(v) => v.into_arg(),
            None => T::null_arg(),
        }
    }
    fn null_arg() -> Arg {
        T::null_arg()
    }
}

impl<T: IntoArg + Clone> IntoArg for &Option<T> {
    fn into_arg(self) -> Arg {
        match self {
            Some(v) => v.clone().into_arg(),
            None => T::null_arg(),
        }
    }
    fn null_arg() -> Arg {
        T::null_arg()
    }
}

/// A row from either backend. `try_get` decodes with the same call on
/// both — the value type only needs `Decode` + `Type` for Postgres and
/// for sqlite.
pub enum DynRow {
    Pg(PgRow),
    Sqlite(SqliteRow),
}

impl DynRow {
    pub fn try_get<T, I>(&self, index: I) -> Result<T, ApiError>
    where
        T: for<'a> Decode<'a, sqlx::Postgres> + Type<sqlx::Postgres>,
        T: for<'a> Decode<'a, sqlx::Sqlite> + Type<sqlx::Sqlite>,
        I: AsRef<str>,
    {
        let col = index.as_ref();
        match self {
            DynRow::Pg(row) => row.try_get(col).map_err(ApiError::from),
            DynRow::Sqlite(row) => row.try_get(col).map_err(ApiError::from),
        }
    }
}

fn add_arg_pg(args: &mut PgArguments, arg: &Arg) {
    match arg {
        Arg::Uuid(v) => {
            let _ = args.add(*v);
        }
        Arg::Str(v) => {
            let _ = args.add(v.clone());
        }
        Arg::Bool(v) => {
            let _ = args.add(*v);
        }
        Arg::I64(v) => {
            let _ = args.add(*v);
        }
        Arg::F64(v) => {
            let _ = args.add(*v);
        }
        Arg::Dt(v) => {
            let _ = args.add(*v);
        }
        Arg::Date(v) => {
            let _ = args.add(*v);
        }
        Arg::Json(v) => {
            let _ = args.add(v.clone());
        }
        Arg::NullText => {
            let _ = args.add(Option::<String>::None);
        }
        Arg::NullUuid => {
            let _ = args.add(Option::<Uuid>::None);
        }
        Arg::NullTime => {
            let _ = args.add(Option::<DateTime<Utc>>::None);
        }
        Arg::NullDate => {
            let _ = args.add(Option::<NaiveDate>::None);
        }
        Arg::NullJson => {
            let _ = args.add(Option::<Value>::None);
        }
        Arg::NullBool => {
            let _ = args.add(Option::<bool>::None);
        }
        Arg::NullInt => {
            let _ = args.add(Option::<i64>::None);
        }
        Arg::NullFloat => {
            let _ = args.add(Option::<f64>::None);
        }
    }
}

fn add_arg_sqlite(args: &mut SqliteArguments, arg: &Arg) {
    match arg {
        Arg::Uuid(v) => {
            let _ = args.add(*v);
        }
        Arg::Str(v) => {
            let _ = args.add(v.clone());
        }
        Arg::Bool(v) => {
            let _ = args.add(*v);
        }
        Arg::I64(v) => {
            let _ = args.add(*v);
        }
        Arg::F64(v) => {
            let _ = args.add(*v);
        }
        Arg::Dt(v) => {
            let _ = args.add(*v);
        }
        Arg::Date(v) => {
            let _ = args.add(*v);
        }
        Arg::Json(v) => {
            let _ = args.add(v.clone());
        }
        Arg::NullText => {
            let _ = args.add(Option::<String>::None);
        }
        Arg::NullUuid => {
            let _ = args.add(Option::<Uuid>::None);
        }
        Arg::NullTime => {
            let _ = args.add(Option::<DateTime<Utc>>::None);
        }
        Arg::NullDate => {
            let _ = args.add(Option::<NaiveDate>::None);
        }
        Arg::NullJson => {
            let _ = args.add(Option::<Value>::None);
        }
        Arg::NullBool => {
            let _ = args.add(Option::<bool>::None);
        }
        Arg::NullInt => {
            let _ = args.add(Option::<i64>::None);
        }
        Arg::NullFloat => {
            let _ = args.add(Option::<f64>::None);
        }
    }
}

/// Rewrite `?` placeholders to `$1..$n` for Postgres. (No Wave 2 query
/// contains a literal `?` inside a string.)
/// Rewrite `?` placeholders to Postgres `$1`, `$2`, … — and nothing
/// else. The SQL text itself is always a hardcoded literal from the
/// call sites in this file; this function never interpolates a value
/// (all values travel as bound arguments), so the rewritten string is
/// safe by construction. sqlx 0.9 requires dynamic SQL to be marked
/// audited (`SqlSafeStr`); the `AssertSqlSafe` wrapper is that mark,
/// applied here — the single place where SQL text is ever rewritten.
fn to_pg_placeholders(sql: &str) -> sqlx::AssertSqlSafe<String> {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0;
    for c in sql.chars() {
        if c == '?' {
            n += 1;
            out.push_str(&format!("${n}"));
        } else {
            out.push(c);
        }
    }
    sqlx::AssertSqlSafe(out)
}

/// The sqlx 0.9 audit mark for a borrowed SQL string. Every SQL
/// string that reaches the `Db` methods is either a hardcoded literal
/// or built from code constants (`?`-placeholder counts,
/// enum-derived table names, hardcoded `SET` column lists) with all
/// values passed as bound arguments — audited 2026-11-23 (Wave 9
/// PR-3, sqlx 0.9 upgrade); no caller interpolates user input into
/// SQL. Keep that invariant: any future dynamically-built SQL must
/// go through the same audit before it is passed here.
fn safe_sql(sql: &str) -> sqlx::AssertSqlSafe<&str> {
    sqlx::AssertSqlSafe(sql)
}

/// Shared database handle. `None` in `AppState` means no `DATABASE_URL`
/// was configured — native routes then fail with 500, like Python's
/// `get_db` failing to connect.
#[derive(Debug, Clone)]
pub enum Db {
    Pg(PgPool),
    Sqlite(SqlitePool),
}

impl Db {
    /// Connect from a URL. `sqlite://` (and the Python SQLAlchemy form
    /// `sqlite+aiosqlite://`) use a sqlite pool; Postgres URLs a pg pool.
    /// Returns `Err` for anything else — the caller treats that as
    /// "DB unavailable", never a panic (ADR-013).
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        let url = url.trim();
        let normalized = if url == "sqlite://:memory:" {
            "sqlite::memory:".to_string()
        } else {
            url.replace("sqlite+aiosqlite://", "sqlite://")
        };
        if normalized.starts_with("sqlite") {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&normalized)
                .await?;
            Ok(Db::Sqlite(pool))
        } else if normalized.starts_with("postgres://") || normalized.starts_with("postgresql://") {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(5)
                .connect(&normalized)
                .await?;
            Ok(Db::Pg(pool))
        } else {
            Err(sqlx::Error::Configuration(
                format!("unsupported DATABASE_URL scheme: {url}").into(),
            ))
        }
    }

    /// In-memory sqlite pool for tests (single connection, so the
    /// `:memory:` database is shared by every query in the test).
    pub async fn sqlite_memory() -> Result<Self, sqlx::Error> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        Ok(Db::Sqlite(pool))
    }

    pub async fn fetch_optional(
        &self,
        sql: &str,
        args: &[Arg],
    ) -> Result<Option<DynRow>, ApiError> {
        match self {
            Db::Pg(pool) => {
                let mut pg_args = PgArguments::default();
                for a in args {
                    add_arg_pg(&mut pg_args, a);
                }
                let row = sqlx::query_with(to_pg_placeholders(sql), pg_args)
                    .fetch_optional(pool)
                    .await?;
                Ok(row.map(DynRow::Pg))
            }
            Db::Sqlite(pool) => {
                let mut lite_args = SqliteArguments::default();
                for a in args {
                    add_arg_sqlite(&mut lite_args, a);
                }
                let row = sqlx::query_with(safe_sql(sql), lite_args)
                    .fetch_optional(pool)
                    .await?;
                Ok(row.map(DynRow::Sqlite))
            }
        }
    }

    pub async fn fetch_one(&self, sql: &str, args: &[Arg]) -> Result<DynRow, ApiError> {
        match self {
            Db::Pg(pool) => {
                let mut pg_args = PgArguments::default();
                for a in args {
                    add_arg_pg(&mut pg_args, a);
                }
                let row = sqlx::query_with(to_pg_placeholders(sql), pg_args)
                    .fetch_one(pool)
                    .await?;
                Ok(DynRow::Pg(row))
            }
            Db::Sqlite(pool) => {
                let mut lite_args = SqliteArguments::default();
                for a in args {
                    add_arg_sqlite(&mut lite_args, a);
                }
                let row = sqlx::query_with(safe_sql(sql), lite_args)
                    .fetch_one(pool)
                    .await?;
                Ok(DynRow::Sqlite(row))
            }
        }
    }

    pub async fn fetch_all(&self, sql: &str, args: &[Arg]) -> Result<Vec<DynRow>, ApiError> {
        match self {
            Db::Pg(pool) => {
                let mut pg_args = PgArguments::default();
                for a in args {
                    add_arg_pg(&mut pg_args, a);
                }
                let rows = sqlx::query_with(to_pg_placeholders(sql), pg_args)
                    .fetch_all(pool)
                    .await?;
                Ok(rows.into_iter().map(DynRow::Pg).collect())
            }
            Db::Sqlite(pool) => {
                let mut lite_args = SqliteArguments::default();
                for a in args {
                    add_arg_sqlite(&mut lite_args, a);
                }
                let rows = sqlx::query_with(safe_sql(sql), lite_args)
                    .fetch_all(pool)
                    .await?;
                Ok(rows.into_iter().map(DynRow::Sqlite).collect())
            }
        }
    }

    /// Execute a statement; returns the number of affected rows.
    pub async fn execute(&self, sql: &str, args: &[Arg]) -> Result<u64, ApiError> {
        match self {
            Db::Pg(pool) => {
                let mut pg_args = PgArguments::default();
                for a in args {
                    add_arg_pg(&mut pg_args, a);
                }
                let res = sqlx::query_with(to_pg_placeholders(sql), pg_args)
                    .execute(pool)
                    .await?;
                Ok(res.rows_affected())
            }
            Db::Sqlite(pool) => {
                let mut lite_args = SqliteArguments::default();
                for a in args {
                    add_arg_sqlite(&mut lite_args, a);
                }
                let res = sqlx::query_with(safe_sql(sql), lite_args)
                    .execute(pool)
                    .await?;
                Ok(res.rows_affected())
            }
        }
    }
}

pub fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

pub fn new_id() -> Uuid {
    Uuid::new_v4()
}

/// Minimal DDL for the Wave 2 tables, in sqlite dialect, for tests.
/// Column names and types mirror the Postgres models in
/// `app/models/models.py`; only the columns Wave 2 reads or writes are
/// present. Production uses the real migrations — this schema is never
/// applied to Postgres.
pub const TEST_SCHEMA: &str = r#"
CREATE TABLE users (
    id UUID PRIMARY KEY, full_name TEXT NOT NULL, phone TEXT UNIQUE,
    email TEXT UNIQUE, password_hash TEXT NOT NULL, locale TEXT NOT NULL DEFAULT 'fa',
    avatar_media_id UUID, date_of_birth TEXT, gender TEXT, country_code TEXT,
    deleted_at TIMESTAMPTZ, deleted_by UUID, is_active BOOLEAN NOT NULL DEFAULT TRUE,
    is_verified BOOLEAN NOT NULL DEFAULT FALSE, totp_secret TEXT,
    totp_last_step INTEGER, totp_enabled_at TIMESTAMPTZ, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE sessions (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, device_info TEXT, ip TEXT,
    last_active_at TIMESTAMPTZ NOT NULL, revoked BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE refresh_tokens (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, session_id UUID,
    token_hash TEXT NOT NULL UNIQUE, rotated_from UUID,
    revoked BOOLEAN NOT NULL DEFAULT FALSE, expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE password_reset_tokens (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, token_hash TEXT NOT NULL,
    purpose TEXT NOT NULL, expires_at TIMESTAMPTZ NOT NULL,
    used BOOLEAN NOT NULL DEFAULT FALSE, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE phone_verification_codes (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, phone TEXT NOT NULL,
    code_hash TEXT NOT NULL, expires_at TIMESTAMPTZ NOT NULL,
    used BOOLEAN NOT NULL DEFAULT FALSE, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE recovery_codes (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, code_hash TEXT NOT NULL,
    used BOOLEAN NOT NULL DEFAULT FALSE, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE ws_tickets (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, session_id UUID,
    ticket_hash TEXT NOT NULL, expires_at TIMESTAMPTZ NOT NULL, used_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE user_activity_events (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, kind TEXT NOT NULL,
    title TEXT, ref_type TEXT, ref_id TEXT, meta JSONB, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE themes (
    id UUID PRIMARY KEY, name JSONB NOT NULL, is_active BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE platform_settings (
    key TEXT PRIMARY KEY, value JSONB NOT NULL, encryption_key_version INTEGER NOT NULL DEFAULT 1,
    updated_at TIMESTAMPTZ
);
CREATE TABLE api_keys (
    id UUID PRIMARY KEY, key_hash TEXT NOT NULL UNIQUE, key_prefix TEXT NOT NULL,
    name TEXT NOT NULL, scopes JSONB NOT NULL DEFAULT '[]',
    rate_limit_per_min INTEGER NOT NULL DEFAULT 60, quota_monthly INTEGER NOT NULL DEFAULT 10000,
    used_this_month INTEGER NOT NULL DEFAULT 0, quota_period TEXT NOT NULL DEFAULT '',
    is_active BOOLEAN NOT NULL DEFAULT TRUE, abuse_flag BOOLEAN NOT NULL DEFAULT FALSE,
    last_used_at TIMESTAMPTZ, created_by UUID, created_at TIMESTAMPTZ NOT NULL, revoked_at TIMESTAMPTZ
);
CREATE TABLE user_devices (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, device_id TEXT NOT NULL,
    platform TEXT, push_token TEXT, last_seen TIMESTAMPTZ NOT NULL, created_at TIMESTAMPTZ NOT NULL,
    UNIQUE(user_id, device_id)
);
CREATE TABLE web_push_subscriptions (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, endpoint TEXT NOT NULL,
    p256dh TEXT NOT NULL, auth TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL,
    UNIQUE(user_id, endpoint)
);
CREATE TABLE roles (id UUID PRIMARY KEY, name TEXT NOT NULL UNIQUE, description TEXT, created_at TIMESTAMPTZ NOT NULL);
CREATE TABLE permissions (id UUID PRIMARY KEY, key TEXT NOT NULL UNIQUE, description TEXT, created_at TIMESTAMPTZ NOT NULL);
CREATE TABLE role_permissions (
    role_id UUID NOT NULL, permission_id UUID NOT NULL,
    PRIMARY KEY (role_id, permission_id)
);
CREATE TABLE user_roles (
    user_id UUID NOT NULL, role_id UUID NOT NULL,
    PRIMARY KEY (user_id, role_id)
);
CREATE TABLE ai_provider_configs (
    id UUID PRIMARY KEY, provider TEXT NOT NULL, model TEXT NOT NULL, version TEXT,
    enabled BOOLEAN NOT NULL DEFAULT FALSE, is_fallback BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL, UNIQUE(provider, model)
);
CREATE TABLE notification_broadcasts (
    id UUID PRIMARY KEY, title TEXT NOT NULL, body TEXT NOT NULL,
    cohort JSONB NOT NULL DEFAULT '{}', channels_requested JSONB NOT NULL DEFAULT '[]',
    status TEXT NOT NULL DEFAULT 'queued', total_recipients INTEGER NOT NULL DEFAULT 0,
    total_sent INTEGER NOT NULL DEFAULT 0, total_skipped INTEGER NOT NULL DEFAULT 0,
    total_failed INTEGER NOT NULL DEFAULT 0, created_by UUID,
    created_at TIMESTAMPTZ NOT NULL, finished_at TIMESTAMPTZ
);
CREATE TABLE notification_deliveries (
    id UUID PRIMARY KEY, broadcast_id UUID NOT NULL, user_id UUID NOT NULL,
    channel TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending', skip_reason TEXT,
    error TEXT, sent_at TIMESTAMPTZ, created_at TIMESTAMPTZ NOT NULL,
    UNIQUE(broadcast_id, user_id, channel)
);
CREATE TABLE build_releases (
    id UUID PRIMARY KEY, platform TEXT NOT NULL, version TEXT NOT NULL, build_number TEXT,
    download_url TEXT, download_expires_at TIMESTAMPTZ, artifact_key TEXT,
    file_size_bytes BIGINT, sha256 TEXT, signed BOOLEAN NOT NULL DEFAULT FALSE,
    build_metadata JSONB NOT NULL DEFAULT '{}', released_by UUID,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE build_requests (
    id UUID PRIMARY KEY, platform TEXT NOT NULL, version TEXT, ref TEXT,
    status TEXT NOT NULL DEFAULT 'queued', note TEXT, branding JSONB NOT NULL DEFAULT '{}',
    provider TEXT, provider_run_id TEXT, provider_run_url TEXT, artifact_name TEXT,
    dispatched_at TIMESTAMPTZ, requested_by UUID, created_at TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ
);
CREATE TABLE user_consent (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, consent_type TEXT NOT NULL,
    text_version TEXT NOT NULL, accepted_at TIMESTAMPTZ NOT NULL, ip TEXT
);
CREATE TABLE audit_logs (
    id UUID PRIMARY KEY, actor_id UUID, actor_type TEXT NOT NULL, action TEXT NOT NULL,
    entity_type TEXT NOT NULL, entity_id TEXT, old_values JSONB, new_values JSONB,
    ip TEXT, user_agent TEXT, request_id TEXT, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE content_placements (
    id UUID PRIMARY KEY, domain TEXT NOT NULL, content_type TEXT NOT NULL,
    content_id TEXT NOT NULL, surface TEXT NOT NULL, slot TEXT,
    sort_order INTEGER NOT NULL DEFAULT 0, audience JSONB NOT NULL DEFAULT '{}',
    starts_at TIMESTAMPTZ, ends_at TIMESTAMPTZ, locale TEXT,
    active BOOLEAN NOT NULL DEFAULT TRUE, created_by UUID,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE content_distributions (
    id UUID PRIMARY KEY, domain TEXT NOT NULL, content_type TEXT NOT NULL,
    content_id TEXT NOT NULL, channel TEXT NOT NULL, enabled BOOLEAN NOT NULL DEFAULT FALSE,
    scheduled_at TIMESTAMPTZ, schedule_tz TEXT, until_at TIMESTAMPTZ, updated_by UUID,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE social_posts (
    id UUID PRIMARY KEY, group_id UUID, author_id UUID NOT NULL, body TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'published', comment_count INTEGER NOT NULL DEFAULT 0,
    likes_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE religious_places (
    id UUID PRIMARY KEY, name JSONB NOT NULL, description JSONB,
    lat DOUBLE PRECISION NOT NULL, lng DOUBLE PRECISION NOT NULL,
    type TEXT NOT NULL DEFAULT 'mosque',
    city TEXT, image_url TEXT, images JSONB, video_url TEXT, video_360_id UUID,
    submitted_by UUID, workflow_status TEXT NOT NULL DEFAULT 'draft',
    version INTEGER NOT NULL DEFAULT 1, created_by UUID,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE domain_events (
    event_id UUID PRIMARY KEY, idempotency_key TEXT UNIQUE, aggregate_type TEXT NOT NULL,
    aggregate_id TEXT NOT NULL, event_type TEXT NOT NULL, payload JSONB NOT NULL,
    schema_version INTEGER NOT NULL DEFAULT 1, source_client TEXT NOT NULL DEFAULT 'api',
    created_at TIMESTAMPTZ NOT NULL, processed_at TIMESTAMPTZ, retry_count INTEGER NOT NULL DEFAULT 0,
    failed_at TIMESTAMPTZ
);
CREATE TABLE notifications (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, notif_type TEXT NOT NULL,
    title JSONB NOT NULL, body TEXT, data JSONB, read_at TIMESTAMPTZ, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE social_groups (
    id UUID PRIMARY KEY, name JSONB NOT NULL, description JSONB, slug TEXT NOT NULL UNIQUE,
    cover_media_id UUID, is_public BOOLEAN NOT NULL DEFAULT TRUE, member_count INTEGER NOT NULL DEFAULT 0,
    created_by UUID, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE group_memberships (
    id UUID PRIMARY KEY, group_id UUID NOT NULL, user_id UUID NOT NULL, role TEXT NOT NULL DEFAULT 'member',
    joined_at TIMESTAMPTZ NOT NULL, UNIQUE(group_id, user_id)
);
CREATE TABLE post_likes (
    id UUID PRIMARY KEY, post_id UUID NOT NULL, user_id UUID NOT NULL, created_at TIMESTAMPTZ NOT NULL,
    UNIQUE(post_id, user_id)
);
CREATE TABLE social_comments (
    id UUID PRIMARY KEY, post_id UUID NOT NULL, author_id UUID NOT NULL, body TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'published', created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE social_follows (
    id UUID PRIMARY KEY, follower_id UUID NOT NULL, followed_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL, UNIQUE(follower_id, followed_id)
);
CREATE TABLE social_reports (
    id UUID PRIMARY KEY, target_type TEXT NOT NULL, target_id UUID NOT NULL, reason TEXT NOT NULL,
    reported_by UUID NOT NULL, status TEXT NOT NULL DEFAULT 'new', reported_at TIMESTAMPTZ NOT NULL,
    triaged_at TIMESTAMPTZ, actioned_at TIMESTAMPTZ, sla_due_at TIMESTAMPTZ NOT NULL,
    action TEXT, note TEXT, actioned_by UUID
);
CREATE TABLE moderation_banned_words (
    id UUID PRIMARY KEY, word TEXT NOT NULL UNIQUE, added_by UUID, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE notification_preferences (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, channel TEXT NOT NULL,
    notif_type TEXT NOT NULL, enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE media_assets (
    id UUID PRIMARY KEY, owner_id UUID, kind TEXT NOT NULL, storage_key TEXT NOT NULL,
    thumbnail_key TEXT, mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL,
    duration_seconds INTEGER, checksum TEXT, visibility TEXT NOT NULL DEFAULT 'public',
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE fatwa_sessions (
    id UUID PRIMARY KEY, user_id UUID, locale TEXT, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE fatwa_messages (
    id UUID PRIMARY KEY, session_id UUID NOT NULL, role TEXT NOT NULL,
    content TEXT, citations JSONB, status TEXT, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE user_ayah_notes (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, surah INTEGER NOT NULL, ayah INTEGER NOT NULL,
    note_text TEXT NOT NULL, is_private BOOLEAN NOT NULL DEFAULT TRUE,
    version INTEGER NOT NULL DEFAULT 1, created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ
);
CREATE TABLE donations (
    id UUID PRIMARY KEY, donor_id UUID, amount INTEGER NOT NULL, currency TEXT NOT NULL,
    gateway TEXT, gateway_reference TEXT, status TEXT NOT NULL, donor_name TEXT,
    campaign_id UUID, created_at TIMESTAMPTZ NOT NULL
);
-- Wave 7 (finance): production `donations` carries payment_intent_id
-- (NOT NULL, unique) - the Wave 2 mirror omitted it. Added here so the
-- finance routes can read DonationOut.intent_id and finalize
-- re-entrantly. `amount` stays INTEGER per ADR-028 (production is
-- BigInteger - finance amounts in tests stay below 2^31).
ALTER TABLE donations ADD COLUMN payment_intent_id UUID;
CREATE TABLE donation_campaigns (
    id UUID PRIMARY KEY, title JSONB NOT NULL, description JSONB,
    goal_amount BIGINT NOT NULL DEFAULT 0, raised_amount BIGINT NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'active', cover TEXT,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ
);
CREATE TABLE payment_intents (
    id UUID PRIMARY KEY, idempotency_key TEXT NOT NULL UNIQUE, amount BIGINT NOT NULL,
    currency TEXT NOT NULL DEFAULT 'IRR', campaign_id UUID, donor_id UUID,
    donor_name TEXT, status TEXT NOT NULL DEFAULT 'pending', gateway TEXT NOT NULL DEFAULT 'zarinpal',
    authority TEXT, created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ
);
CREATE TABLE ledger_entries (
    id UUID PRIMARY KEY, entry_type TEXT NOT NULL, amount BIGINT NOT NULL,
    currency TEXT NOT NULL DEFAULT 'IRR', reference TEXT, donation_id UUID,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE payment_webhook_log (
    id UUID PRIMARY KEY, gateway TEXT NOT NULL, payload_hash TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL DEFAULT 'processed', processed_at TIMESTAMPTZ
);
CREATE TABLE recurring_donations (
    id UUID PRIMARY KEY, donor_id UUID NOT NULL, campaign_id UUID, amount BIGINT NOT NULL,
    currency TEXT NOT NULL DEFAULT 'IRR', interval TEXT NOT NULL DEFAULT 'monthly',
    status TEXT NOT NULL DEFAULT 'active', next_charge_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ
);
CREATE TABLE manual_donation_proofs (
    id UUID PRIMARY KEY, payment_intent_id UUID NOT NULL UNIQUE, method TEXT NOT NULL,
    proof_text TEXT, proof_image_key TEXT, proof_image_mime TEXT, wallet_label TEXT,
    amount_irr BIGINT, fx_rate_irr BIGINT, fx_source TEXT,
    review_status TEXT NOT NULL DEFAULT 'pending', reviewed_by UUID,
    reviewed_at TIMESTAMPTZ, review_note TEXT,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ
);
CREATE TABLE home_sections (
    id UUID PRIMARY KEY, key TEXT NOT NULL UNIQUE, sort_order INTEGER NOT NULL,
    visible BOOLEAN NOT NULL DEFAULT TRUE, title JSONB NOT NULL, subtitle JSONB,
    data JSONB, empty_title JSONB, empty_hint JSONB,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE islamic_events (
    id UUID PRIMARY KEY, name JSONB NOT NULL, hijri_month INTEGER NOT NULL,
    hijri_day INTEGER NOT NULL, recurring BOOLEAN NOT NULL DEFAULT TRUE,
    theme_id UUID, auto_publish BOOLEAN NOT NULL DEFAULT FALSE,
    needs_admin_confirmation BOOLEAN NOT NULL DEFAULT TRUE, image_url TEXT,
    starts_at TIMESTAMPTZ, ends_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL,
    workflow_status TEXT NOT NULL DEFAULT 'draft', version INTEGER NOT NULL DEFAULT 1,
    created_by UUID, distribution JSONB
);
CREATE TABLE scholar_quotes (
    id UUID PRIMARY KEY, text JSONB NOT NULL, scholar JSONB NOT NULL, source TEXT,
    quote_date DATE, avatar_url TEXT, placements JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL,
    workflow_status TEXT NOT NULL DEFAULT 'draft', version INTEGER NOT NULL DEFAULT 1,
    created_by UUID, distribution JSONB
);
CREATE TABLE pages (
    id UUID PRIMARY KEY, slug TEXT NOT NULL UNIQUE, title JSONB NOT NULL,
    blocks JSONB NOT NULL DEFAULT '[]', seo_meta JSONB NOT NULL DEFAULT '{}',
    content_type TEXT NOT NULL DEFAULT 'page', published_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL,
    workflow_status TEXT NOT NULL DEFAULT 'draft', version INTEGER NOT NULL DEFAULT 1,
    created_by UUID, distribution JSONB
);
CREATE TABLE nav_items (
    id UUID PRIMARY KEY, label JSONB NOT NULL, parent_id UUID, sort_order INTEGER NOT NULL DEFAULT 0,
    visible BOOLEAN NOT NULL DEFAULT TRUE, locale TEXT, link_type TEXT NOT NULL DEFAULT 'internal',
    url TEXT NOT NULL, starts_at TIMESTAMPTZ, ends_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE banners (
    id UUID PRIMARY KEY, title JSONB NOT NULL, subtitle JSONB, media JSONB NOT NULL DEFAULT '{}',
    cta JSONB, placement TEXT NOT NULL DEFAULT 'homepage', priority INTEGER NOT NULL DEFAULT 0,
    audience JSONB NOT NULL DEFAULT '{}', starts_at TIMESTAMPTZ, ends_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL,
    workflow_status TEXT NOT NULL DEFAULT 'draft', version INTEGER NOT NULL DEFAULT 1,
    created_by UUID, distribution JSONB
);
CREATE TABLE announcements (
    id UUID PRIMARY KEY, workflow_status TEXT NOT NULL DEFAULT 'draft',
    version INTEGER NOT NULL DEFAULT 1, created_by UUID, message JSONB NOT NULL,
    dismissible BOOLEAN NOT NULL DEFAULT TRUE, audience JSONB NOT NULL DEFAULT '{}',
    placement TEXT NOT NULL DEFAULT 'global', starts_at TIMESTAMPTZ, ends_at TIMESTAMPTZ,
    distribution JSONB, created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE article_categories (
    id UUID PRIMARY KEY, slug TEXT NOT NULL UNIQUE, title JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE articles (
    id UUID PRIMARY KEY, title JSONB NOT NULL, body JSONB NOT NULL, slug TEXT NOT NULL UNIQUE,
    seo_title TEXT, seo_description TEXT, category_id UUID, author_name TEXT,
    featured_image_url TEXT, workflow_status TEXT NOT NULL DEFAULT 'draft',
    view_count INTEGER NOT NULL DEFAULT 0, share_count INTEGER NOT NULL DEFAULT 0,
    published_at TIMESTAMPTZ, created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE fatwa_categories (
    id UUID PRIMARY KEY, slug TEXT NOT NULL UNIQUE, title JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE fiqh_topics (
    id UUID PRIMARY KEY, title JSONB NOT NULL, description JSONB,
    workflow_status TEXT NOT NULL DEFAULT 'draft', category_id UUID, author_name TEXT,
    featured_image_url TEXT, view_count INTEGER NOT NULL DEFAULT 0,
    share_count INTEGER NOT NULL DEFAULT 0, published_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE madhab_positions (
    id UUID PRIMARY KEY, topic_id UUID NOT NULL, madhab TEXT NOT NULL,
    position_text JSONB NOT NULL, evidence_refs JSONB,
    workflow_status TEXT NOT NULL DEFAULT 'draft', created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE fiqh_knowledge_chunks (
    id UUID PRIMARY KEY, chunk_text TEXT NOT NULL, embedding JSONB,
    embedding_provider TEXT, embedding_model TEXT, embedding_projection TEXT,
    source_type TEXT NOT NULL DEFAULT 'manual', source_ref JSONB,
    source_library_item_ref TEXT, source_citation JSONB, topic_category TEXT,
    madhab_scope TEXT, workflow_status TEXT NOT NULL DEFAULT 'draft',
    approved_by UUID, ai_scopes JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE fatwa_submissions (
    id UUID PRIMARY KEY, title JSONB NOT NULL, body TEXT NOT NULL, madhab TEXT,
    evidence TEXT, author_id UUID NOT NULL, workflow_status TEXT NOT NULL DEFAULT 'draft',
    version INTEGER NOT NULL DEFAULT 1, reviewed_by UUID, reviewed_at TIMESTAMPTZ,
    published_at TIMESTAMPTZ, created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE question_clusters (
    id UUID PRIMARY KEY, representative_question TEXT NOT NULL, embedding JSONB,
    status TEXT NOT NULL DEFAULT 'open', answer TEXT, answered_by UUID,
    answered_at TIMESTAMPTZ, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE cluster_members (
    id UUID PRIMARY KEY, cluster_id UUID NOT NULL, question_text TEXT NOT NULL,
    user_id UUID NOT NULL, source TEXT NOT NULL DEFAULT 'direct',
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE user_saved_fatwas (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, fatwa_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL, UNIQUE(user_id, fatwa_id)
);
CREATE TABLE quiz_questions (
    id UUID PRIMARY KEY, question_text JSONB NOT NULL, options JSONB NOT NULL,
    correct_option INTEGER NOT NULL, source_chunk_id UUID NOT NULL,
    difficulty TEXT NOT NULL DEFAULT 'medium', workflow_status TEXT NOT NULL DEFAULT 'draft',
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE quiz_attempts (
    id UUID PRIMARY KEY, user_id UUID, lesson_id UUID, question_ids JSONB NOT NULL,
    answers JSONB NOT NULL, score REAL NOT NULL DEFAULT 0.0, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE verification_checks (
    id UUID PRIMARY KEY, input_hash TEXT NOT NULL, input_excerpt TEXT,
    verdict TEXT NOT NULL, matched_chunk_ids JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE trusted_external_sources (
    id UUID PRIMARY KEY, name TEXT NOT NULL, base_url TEXT NOT NULL, domain TEXT,
    enabled BOOLEAN NOT NULL DEFAULT FALSE, allowed_paths JSONB NOT NULL DEFAULT '[]',
    priority INTEGER NOT NULL DEFAULT 0, citation_required BOOLEAN NOT NULL DEFAULT TRUE,
    last_checked_at TIMESTAMPTZ, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE ingestion_runs (
    id UUID PRIMARY KEY, source_type TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued',
    stats JSONB NOT NULL DEFAULT '{}', started_at TIMESTAMPTZ, finished_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE ai_usage_log (
    id UUID PRIMARY KEY, user_id UUID, endpoint TEXT NOT NULL, model TEXT NOT NULL,
    tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0,
    cost_estimate REAL NOT NULL DEFAULT 0.0, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE library_items (
    id UUID PRIMARY KEY, title JSONB NOT NULL, description JSONB, transcript TEXT,
    workflow_status TEXT NOT NULL DEFAULT 'draft', visibility TEXT NOT NULL DEFAULT 'public',
    kind TEXT NOT NULL DEFAULT 'video', external_url TEXT, media_key TEXT,
    cover_media_key TEXT, scholar_id UUID, topic_tags JSONB NOT NULL DEFAULT '[]',
    duration_seconds INTEGER, published_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
ALTER TABLE library_items ADD COLUMN related_lesson_id UUID;
ALTER TABLE library_items ADD COLUMN religious_place_ref TEXT;
ALTER TABLE library_items ADD COLUMN version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE library_items ADD COLUMN ai_scopes JSONB NOT NULL DEFAULT '[]';
ALTER TABLE library_items ADD COLUMN include_in_fatwa_rag BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE library_items ADD COLUMN ai_indexed BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE library_items ADD COLUMN download_policy JSONB;
ALTER TABLE library_items ADD COLUMN transcript_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE library_items ADD COLUMN ocr_text TEXT;
ALTER TABLE library_items ADD COLUMN ocr_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE library_items ADD COLUMN processing_error TEXT;
ALTER TABLE library_items ADD COLUMN file_size_bytes INTEGER;
ALTER TABLE library_items ADD COLUMN mime_type TEXT;
ALTER TABLE library_items ADD COLUMN source_type TEXT NOT NULL DEFAULT 'upload';
ALTER TABLE library_items ADD COLUMN source_ref JSONB;
ALTER TABLE library_items ADD COLUMN created_by UUID;
CREATE TABLE library_item_versions (
    id UUID PRIMARY KEY, item_id UUID NOT NULL, version INTEGER NOT NULL,
    snapshot JSONB NOT NULL, created_by UUID,
    created_at TIMESTAMPTZ NOT NULL, note TEXT
);
CREATE TABLE inheritance_rule_configs (
    id UUID PRIMARY KEY, madhab TEXT NOT NULL, source TEXT NOT NULL,
    version INTEGER NOT NULL, effective_from TIMESTAMPTZ NOT NULL,
    effective_to TIMESTAMPTZ, approved_by UUID, approved_at TIMESTAMPTZ,
    change_reason TEXT, status TEXT NOT NULL DEFAULT 'draft',
    rule_data JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ,
    UNIQUE(madhab, version)
);
CREATE TABLE zakat_rule_configs (
    id UUID PRIMARY KEY, madhab TEXT, source TEXT NOT NULL,
    version INTEGER NOT NULL, effective_from TIMESTAMPTZ NOT NULL,
    effective_to TIMESTAMPTZ, approved_by UUID, approved_at TIMESTAMPTZ,
    change_reason TEXT, status TEXT NOT NULL DEFAULT 'draft',
    rule_data JSONB NOT NULL DEFAULT '{}',
    asset_categories JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ,
    UNIQUE(version)
);
CREATE TABLE inheritance_cases (
    id UUID PRIMARY KEY, user_id UUID, estate_value NUMERIC(20,4) NOT NULL,
    madhab TEXT NOT NULL, heirs JSONB NOT NULL, result JSONB NOT NULL,
    rule_config_id UUID, idempotency_key TEXT UNIQUE,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE zakat_calculations (
    id UUID PRIMARY KEY, user_id UUID, inputs JSONB NOT NULL,
    result JSONB NOT NULL, nisab_snapshot JSONB NOT NULL,
    rule_config_id UUID, idempotency_key TEXT UNIQUE,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE nisab_price_cache (
    id UUID PRIMARY KEY, source TEXT NOT NULL UNIQUE,
    currency TEXT NOT NULL DEFAULT 'USD',
    gold_per_gram NUMERIC(20,6), silver_per_gram NUMERIC(20,6),
    fetched_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ
);
CREATE TABLE video_playlists (
    id UUID PRIMARY KEY, title JSONB NOT NULL, description JSONB,
    cover_media_key TEXT, scholar_id UUID, is_published BOOLEAN NOT NULL DEFAULT FALSE,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE video_playlist_items (
    id UUID PRIMARY KEY, playlist_id UUID NOT NULL, item_id UUID NOT NULL,
    position INTEGER NOT NULL DEFAULT 0, created_at TIMESTAMPTZ NOT NULL,
    UNIQUE(playlist_id, item_id)
);
CREATE TABLE user_video_progress (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, item_id UUID NOT NULL,
    position_seconds DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    duration_seconds DOUBLE PRECISION,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ,
    UNIQUE(user_id, item_id)
);
CREATE TABLE user_saved_videos (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, item_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL, UNIQUE(user_id, item_id)
);
CREATE TABLE item_subtitles (
    id UUID PRIMARY KEY, item_id UUID NOT NULL, lang TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending', vtt TEXT, error TEXT,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ,
    UNIQUE(item_id, lang)
);
CREATE TABLE voice_sessions (
    id UUID PRIMARY KEY, user_id UUID, lang TEXT NOT NULL DEFAULT 'fa',
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE voice_usage_log (
    id UUID PRIMARY KEY, session_id UUID NOT NULL, intent TEXT NOT NULL,
    lang TEXT NOT NULL, stt_ms INTEGER, tts_provider TEXT,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE curricula (
    id UUID PRIMARY KEY, title JSONB NOT NULL, description TEXT,
    sort_order INTEGER NOT NULL DEFAULT 0, is_active BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE courses (
    id UUID PRIMARY KEY, curriculum_id UUID, title JSONB NOT NULL, description JSONB,
    level TEXT NOT NULL DEFAULT 'beginner', visibility TEXT NOT NULL DEFAULT 'public',
    thumbnail_media_ref TEXT, sort_order INTEGER NOT NULL DEFAULT 0,
    is_active BOOLEAN NOT NULL DEFAULT TRUE, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE lessons (
    id UUID PRIMARY KEY, course_id UUID NOT NULL, title JSONB NOT NULL,
    body JSONB NOT NULL DEFAULT '[]', duration_minutes INTEGER NOT NULL DEFAULT 0,
    sort_order INTEGER NOT NULL DEFAULT 0, is_free_preview BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE user_progress (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, lesson_id UUID NOT NULL,
    status TEXT NOT NULL DEFAULT 'not_started', position_seconds INTEGER NOT NULL DEFAULT 0,
    completed_at TIMESTAMPTZ, updated_at TIMESTAMPTZ NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    UNIQUE(user_id, lesson_id)
);
CREATE TABLE certificates (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, course_id UUID NOT NULL,
    code TEXT NOT NULL UNIQUE, issued_at TIMESTAMPTZ NOT NULL,
    pdf_storage_ref TEXT, locale TEXT NOT NULL DEFAULT 'fa'
);
CREATE TABLE course_cohorts (
    id UUID PRIMARY KEY, course_id UUID NOT NULL, title JSONB NOT NULL,
    starts_at TIMESTAMPTZ NOT NULL, ends_at TIMESTAMPTZ NOT NULL,
    max_seats INTEGER NOT NULL, is_open BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE cohort_enrollments (
    id UUID PRIMARY KEY, cohort_id UUID NOT NULL, user_id UUID NOT NULL,
    enrolled_at TIMESTAMPTZ NOT NULL, status TEXT NOT NULL DEFAULT 'active',
    UNIQUE(cohort_id, user_id)
);
CREATE TABLE course_prerequisites (
    id UUID PRIMARY KEY, course_id UUID NOT NULL, requires_course_id UUID NOT NULL,
    UNIQUE(course_id, requires_course_id)
);
CREATE TABLE user_reading_states (
    user_id UUID PRIMARY KEY, bookmarks JSONB NOT NULL DEFAULT '[]', last_read JSONB,
    state_updated_at BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE dhikr_sessions (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, dhikr_text TEXT NOT NULL,
    target_count INTEGER, count INTEGER NOT NULL DEFAULT 0,
    started_at TIMESTAMPTZ NOT NULL, ended_at TIMESTAMPTZ
);
CREATE TABLE dhikr_daily_counts (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, date DATE NOT NULL, dhikr_text TEXT NOT NULL,
    total_count INTEGER NOT NULL DEFAULT 0, version INTEGER NOT NULL DEFAULT 1,
    updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE sync_changelog (
    seq INTEGER PRIMARY KEY, user_id UUID NOT NULL, device_id TEXT NOT NULL,
    entity_type TEXT NOT NULL, entity_id TEXT NOT NULL, op_type TEXT NOT NULL,
    payload JSONB, version INTEGER NOT NULL DEFAULT 1, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE sync_tombstones (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL, deleted_by UUID, deleted_at TIMESTAMPTZ NOT NULL,
    version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE device_cursors (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, device_id TEXT NOT NULL,
    cursor_seq BIGINT NOT NULL DEFAULT 0, updated_at TIMESTAMPTZ NOT NULL,
    UNIQUE(user_id, device_id)
);
CREATE TABLE sync_operations (
    id UUID PRIMARY KEY, client_op_id TEXT NOT NULL UNIQUE, user_id UUID NOT NULL,
    device_id TEXT NOT NULL, entity_type TEXT NOT NULL, entity_id TEXT NOT NULL,
    op_type TEXT NOT NULL, payload JSONB, base_version INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'pending', result JSONB,
    created_at TIMESTAMPTZ NOT NULL, applied_at TIMESTAMPTZ
);
CREATE TABLE conflict_records (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL, local_payload JSONB, remote_payload JSONB,
    local_version INTEGER, remote_version INTEGER,
    resolution TEXT NOT NULL DEFAULT 'pending', resolved_by UUID,
    resolved_at TIMESTAMPTZ, resolution_note TEXT, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE telegram_distribution_policies (
    id UUID PRIMARY KEY, content_type TEXT NOT NULL UNIQUE,
    policy TEXT NOT NULL DEFAULT 'never', message_template JSONB,
    target_channels JSONB NOT NULL DEFAULT '[]', spacing_seconds INTEGER NOT NULL DEFAULT 0,
    require_approval BOOLEAN NOT NULL DEFAULT TRUE, schedule_config JSONB,
    is_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE telegram_published (
    id UUID PRIMARY KEY, source_content_type TEXT NOT NULL,
    source_content_id TEXT NOT NULL, distribution_target TEXT NOT NULL,
    published_message_id BIGINT, idempotency_key TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL DEFAULT 'pending', retry_count INTEGER NOT NULL DEFAULT 0,
    error TEXT, created_at TIMESTAMPTZ NOT NULL, sent_at TIMESTAMPTZ
);
CREATE TABLE telegram_callbacks (
    nonce TEXT PRIMARY KEY, source_content_type TEXT NOT NULL,
    source_content_id TEXT NOT NULL, op TEXT NOT NULL, actor_user_id UUID NOT NULL,
    signature TEXT NOT NULL, expires_at TIMESTAMPTZ NOT NULL, used_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE hifz_attempts (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, surah INTEGER NOT NULL, page INTEGER,
    status TEXT NOT NULL DEFAULT 'in_progress', engine TEXT NOT NULL DEFAULT 'zipformer',
    extra_words JSONB NOT NULL DEFAULT '[]', start_ayah INTEGER, end_ayah INTEGER,
    current_surah INTEGER, current_ayah INTEGER, current_word_index INTEGER,
    repeat_ayah INTEGER NOT NULL DEFAULT 1, repeat_range INTEGER NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ, completed_at TIMESTAMPTZ
);
CREATE TABLE hifz_word_scores (
    id UUID PRIMARY KEY, attempt_id UUID NOT NULL, surah INTEGER NOT NULL,
    ayah INTEGER NOT NULL, word_index INTEGER NOT NULL, expected_text TEXT NOT NULL,
    status TEXT NOT NULL, prob FLOAT NOT NULL DEFAULT 0, heard TEXT,
    mistake_type TEXT, disputed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ,
    UNIQUE (attempt_id, surah, ayah, word_index)
);
CREATE TABLE hifz_mistakes (
    id UUID PRIMARY KEY, attempt_id UUID NOT NULL, user_id UUID NOT NULL,
    surah INTEGER NOT NULL, ayah INTEGER NOT NULL, word_index INTEGER,
    mistake_type TEXT NOT NULL, expected_text TEXT NOT NULL, heard TEXT,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE hifz_user_settings (
    user_id UUID PRIMARY KEY, freeze_on_error BOOLEAN NOT NULL DEFAULT FALSE,
    mistake_detection BOOLEAN NOT NULL DEFAULT TRUE,
    tashkeel_detection BOOLEAN NOT NULL DEFAULT TRUE,
    resumable_sessions BOOLEAN NOT NULL DEFAULT FALSE,
    hide_ayah_markers BOOLEAN NOT NULL DEFAULT FALSE,
    daily_goal_ayahs INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ
);
CREATE TABLE hifz_listen_positions (
    user_id UUID PRIMARY KEY, reciter TEXT NOT NULL DEFAULT 'alafasy',
    surah INTEGER NOT NULL DEFAULT 1, ayah INTEGER NOT NULL DEFAULT 1,
    progress_ms INTEGER NOT NULL DEFAULT 0, speed FLOAT NOT NULL DEFAULT 1.0,
    repeat_mode TEXT NOT NULL DEFAULT 'off', repeat_ayah INTEGER NOT NULL DEFAULT 1,
    repeat_range_start INTEGER, repeat_range_end INTEGER,
    repeat_range_count INTEGER NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ
);
CREATE TABLE hifz_reviews (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, surah INTEGER NOT NULL,
    ayah INTEGER NOT NULL, ease FLOAT NOT NULL DEFAULT 2.5,
    interval_days INTEGER NOT NULL DEFAULT 0, repetitions INTEGER NOT NULL DEFAULT 0,
    due_at TIMESTAMPTZ, last_quality INTEGER, last_reviewed_at TIMESTAMPTZ,
    params_version INTEGER NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ,
    UNIQUE (user_id, surah, ayah)
);
CREATE TABLE hifz_plans (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, title TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'memorize', direction TEXT NOT NULL DEFAULT 'normal',
    start_surah INTEGER NOT NULL, start_ayah INTEGER NOT NULL,
    end_surah INTEGER NOT NULL, end_ayah INTEGER NOT NULL,
    daily_target_new INTEGER NOT NULL DEFAULT 1,
    daily_target_review INTEGER NOT NULL DEFAULT 5,
    status TEXT NOT NULL DEFAULT 'active',
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ, completed_at TIMESTAMPTZ
);
CREATE TABLE hifz_plan_items (
    id UUID PRIMARY KEY, plan_id UUID NOT NULL, user_id UUID NOT NULL,
    surah INTEGER NOT NULL, ayah INTEGER NOT NULL, position INTEGER NOT NULL,
    is_new BOOLEAN NOT NULL DEFAULT TRUE, status TEXT NOT NULL DEFAULT 'pending',
    completed_at TIMESTAMPTZ, created_at TIMESTAMPTZ NOT NULL,
    UNIQUE (plan_id, surah, ayah)
);
CREATE TABLE hifz_tests (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, surah_start INTEGER NOT NULL,
    surah_end INTEGER NOT NULL, question_count INTEGER NOT NULL DEFAULT 10,
    time_limit_minutes INTEGER, question_length TEXT NOT NULL DEFAULT 'medium',
    stop_rule TEXT NOT NULL DEFAULT 'ayah_end', status TEXT NOT NULL DEFAULT 'in_progress',
    started_at TIMESTAMPTZ NOT NULL, ends_at TIMESTAMPTZ, completed_at TIMESTAMPTZ
);
CREATE TABLE hifz_test_questions (
    id UUID PRIMARY KEY, test_id UUID NOT NULL, position INTEGER NOT NULL,
    surah INTEGER NOT NULL, ayah_start INTEGER NOT NULL, ayah_end INTEGER NOT NULL,
    prompt_words INTEGER NOT NULL DEFAULT 3, prompt TEXT, status TEXT NOT NULL DEFAULT 'pending',
    score FLOAT, counts JSONB NOT NULL DEFAULT '{}', peek_count INTEGER NOT NULL DEFAULT 0,
    raw_events JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL, answered_at TIMESTAMPTZ,
    UNIQUE (test_id, position)
);
CREATE TABLE hifz_tasmi_sessions (
    id UUID PRIMARY KEY, user_id UUID NOT NULL, method TEXT NOT NULL,
    variant TEXT NOT NULL DEFAULT 'standard', start_surah INTEGER NOT NULL,
    start_ayah INTEGER NOT NULL, end_surah INTEGER NOT NULL, end_ayah INTEGER NOT NULL,
    state JSONB NOT NULL DEFAULT '{}', self_assessed BOOLEAN NOT NULL DEFAULT TRUE,
    reflection TEXT, status TEXT NOT NULL DEFAULT 'active',
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ, completed_at TIMESTAMPTZ
);
-- Wave 8 projects module - mirrors app modules projects
CREATE TABLE projects (
    id UUID PRIMARY KEY, title JSONB NOT NULL, status VARCHAR(16) NOT NULL DEFAULT 'active',
    created_by UUID, created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ
);
CREATE TABLE tasks (
    id UUID PRIMARY KEY, project_id UUID NOT NULL, title JSONB NOT NULL,
    description JSONB, assignee_id UUID, status VARCHAR(16) NOT NULL DEFAULT 'backlog',
    due_date TIMESTAMPTZ, priority VARCHAR(16) NOT NULL DEFAULT 'medium',
    related_entity_type VARCHAR(64), related_entity_id UUID,
    due_notified_at TIMESTAMPTZ, created_by UUID,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ
);
CREATE INDEX ix_tasks_related_entity ON tasks (related_entity_type, related_entity_id);
CREATE TABLE staff_members (
    id UUID PRIMARY KEY, user_id UUID NOT NULL UNIQUE, title JSONB NOT NULL,
    department JSONB NOT NULL, joined_date DATE, is_active BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ
);
"#;

/// Apply `TEST_SCHEMA` to a pool (sqlite tests only).
pub async fn init_test_schema(db: &Db) -> Result<(), ApiError> {
    for stmt in TEST_SCHEMA.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        db.execute(stmt, &[]).await?;
    }
    Ok(())
}
