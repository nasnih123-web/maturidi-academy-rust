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
fn to_pg_placeholders(sql: &str) -> String {
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
    out
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
                let row = sqlx::query_with(&to_pg_placeholders(sql), pg_args)
                    .fetch_optional(pool)
                    .await?;
                Ok(row.map(DynRow::Pg))
            }
            Db::Sqlite(pool) => {
                let mut lite_args = SqliteArguments::default();
                for a in args {
                    add_arg_sqlite(&mut lite_args, a);
                }
                let row = sqlx::query_with(sql, lite_args)
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
                let row = sqlx::query_with(&to_pg_placeholders(sql), pg_args)
                    .fetch_one(pool)
                    .await?;
                Ok(DynRow::Pg(row))
            }
            Db::Sqlite(pool) => {
                let mut lite_args = SqliteArguments::default();
                for a in args {
                    add_arg_sqlite(&mut lite_args, a);
                }
                let row = sqlx::query_with(sql, lite_args).fetch_one(pool).await?;
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
                let rows = sqlx::query_with(&to_pg_placeholders(sql), pg_args)
                    .fetch_all(pool)
                    .await?;
                Ok(rows.into_iter().map(DynRow::Pg).collect())
            }
            Db::Sqlite(pool) => {
                let mut lite_args = SqliteArguments::default();
                for a in args {
                    add_arg_sqlite(&mut lite_args, a);
                }
                let rows = sqlx::query_with(sql, lite_args).fetch_all(pool).await?;
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
                let res = sqlx::query_with(&to_pg_placeholders(sql), pg_args)
                    .execute(pool)
                    .await?;
                Ok(res.rows_affected())
            }
            Db::Sqlite(pool) => {
                let mut lite_args = SqliteArguments::default();
                for a in args {
                    add_arg_sqlite(&mut lite_args, a);
                }
                let res = sqlx::query_with(sql, lite_args).execute(pool).await?;
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
    key TEXT PRIMARY KEY, value TEXT NOT NULL, encryption_key_version INTEGER NOT NULL DEFAULT 1
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
CREATE TABLE roles (id UUID PRIMARY KEY, name TEXT NOT NULL UNIQUE, created_at TIMESTAMPTZ NOT NULL);
CREATE TABLE permissions (id UUID PRIMARY KEY, key TEXT NOT NULL UNIQUE, created_at TIMESTAMPTZ NOT NULL);
CREATE TABLE role_permissions (
    role_id UUID NOT NULL, permission_id UUID NOT NULL,
    PRIMARY KEY (role_id, permission_id)
);
CREATE TABLE user_roles (
    user_id UUID NOT NULL, role_id UUID NOT NULL,
    PRIMARY KEY (user_id, role_id)
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
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE courses (
    id UUID PRIMARY KEY, title JSONB NOT NULL, description JSONB,
    is_active BOOLEAN NOT NULL DEFAULT TRUE, visibility TEXT NOT NULL DEFAULT 'public',
    created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE course_cohorts (
    id UUID PRIMARY KEY, course_id UUID NOT NULL, created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE cohort_enrollments (
    id UUID PRIMARY KEY, cohort_id UUID NOT NULL, status TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
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
