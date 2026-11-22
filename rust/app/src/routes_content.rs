//! Content Control Center + CMS — mirrors
//! `app/modules/content/{router,cms_router,cms_schemas,cms_service,service}.py`.
//!
//! Control center: the adapter registry is empty at runtime (no
//! `register_adapter` call exists anywhere in the Python app), so
//! `GET /api/v1/content/items` always returns `[]` and preview/policies
//! always 404 `unknown_content_type`.
//!
//! CMS: six resources (`events`, `quotes`, `pages`, `nav`, `banners`,
//! `announcements`) registered by one generic `_register_resource` —
//! admin list/create/get/update/transition/delete plus the public
//! reads. `_to_dict` serializes every table column in
//! `__table__.columns` order (own columns first, then the
//! `_PublishableMixin` columns — verified empirically against
//! SQLAlchemy); UUIDs as strings, datetimes as aware ISO strings,
//! dates as ISO.
//!
//! Notes on fidelity:
//! - `PageUpdate.slug_immutable` is unreachable in Python: `PageUpdate`
//!   has no `slug` field and Pydantic `extra='ignore'` drops it before
//!   the `"slug" in data` check, so a client-sent `slug` on PATCH is
//!   silently ignored — the Rust port ignores unknown body fields the
//!   same way and never emits `slug_immutable`.
//! - The Islamic-calendar cron (`run_islamic_calendar_cycle`), the
//!   seed-time `ensure_calendar_settings` / `seed_legal_pages` and the
//!   `admin_content_router` are not HTTP routes and are not ported
//!   here (the cron belongs to Wave 7 workers).

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Json,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    authn::{authenticate, require_permission},
    db::{new_id, utc_now, Arg, Db, DynRow, IntoArg},
    errors::{ApiError, ApiResult},
    hijri::hijri_month_day_today,
    settings::get_setting,
    validate::{
        as_bool, as_int, date_value, datetime_value, decode_cursor, dict_value, distribution_value,
        encode_cursor, localized_value, opt_date, opt_datetime, parse_body, parse_query, query_int,
        query_opt_int, req_int_loose, req_localized, req_str_raw, str_list_value, str_value,
        uuid_value, VResult,
    },
    workflow::{validate_transition, STATUSES},
    AppState,
};

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// `{"web": true, "mobile": false, "desktop": false, "telegram": false,
/// "ai_knowledge": false}` — the publish-time distribution default.
fn default_distribution() -> Value {
    json!({
        "web": true,
        "mobile": false,
        "desktop": false,
        "telegram": false,
        "ai_knowledge": false,
    })
}

// --- resource metadata --------------------------------------------------

struct Resource {
    name: &'static str,
    table: &'static str,
    prefix: &'static str,
    workflow: bool,
    columns: &'static [&'static str],
}

fn resource(name: &str) -> Option<&'static Resource> {
    const EVENTS: Resource = Resource {
        name: "events",
        table: "islamic_events",
        prefix: "islamic_event",
        workflow: true,
        columns: &[
            "id",
            "name",
            "hijri_month",
            "hijri_day",
            "recurring",
            "theme_id",
            "auto_publish",
            "needs_admin_confirmation",
            "image_url",
            "starts_at",
            "ends_at",
            "created_at",
            "updated_at",
            "workflow_status",
            "version",
            "created_by",
            "distribution",
        ],
    };
    const QUOTES: Resource = Resource {
        name: "quotes",
        table: "scholar_quotes",
        prefix: "scholar_quote",
        workflow: true,
        columns: &[
            "id",
            "text",
            "scholar",
            "source",
            "quote_date",
            "avatar_url",
            "placements",
            "created_at",
            "updated_at",
            "workflow_status",
            "version",
            "created_by",
            "distribution",
        ],
    };
    const PAGES: Resource = Resource {
        name: "pages",
        table: "pages",
        prefix: "page",
        workflow: true,
        columns: &[
            "id",
            "slug",
            "title",
            "blocks",
            "seo_meta",
            "content_type",
            "published_at",
            "created_at",
            "updated_at",
            "workflow_status",
            "version",
            "created_by",
            "distribution",
        ],
    };
    const NAV: Resource = Resource {
        name: "nav",
        table: "nav_items",
        prefix: "nav_item",
        workflow: false,
        columns: &[
            "id",
            "label",
            "parent_id",
            "sort_order",
            "visible",
            "locale",
            "link_type",
            "url",
            "starts_at",
            "ends_at",
            "created_at",
            "updated_at",
        ],
    };
    const BANNERS: Resource = Resource {
        name: "banners",
        table: "banners",
        prefix: "banner",
        workflow: true,
        columns: &[
            "id",
            "title",
            "subtitle",
            "media",
            "cta",
            "placement",
            "priority",
            "audience",
            "starts_at",
            "ends_at",
            "created_at",
            "updated_at",
            "workflow_status",
            "version",
            "created_by",
            "distribution",
        ],
    };
    const ANNOUNCEMENTS: Resource = Resource {
        name: "announcements",
        table: "announcements",
        prefix: "announcement",
        workflow: true,
        columns: &[
            "id",
            "workflow_status",
            "version",
            "created_by",
            "message",
            "dismissible",
            "audience",
            "placement",
            "starts_at",
            "ends_at",
            "distribution",
            "created_at",
            "updated_at",
        ],
    };
    match name {
        "events" => Some(&EVENTS),
        "quotes" => Some(&QUOTES),
        "pages" => Some(&PAGES),
        "nav" => Some(&NAV),
        "banners" => Some(&BANNERS),
        "announcements" => Some(&ANNOUNCEMENTS),
        _ => None,
    }
}

/// 404 `http_error` / `Not Found` — what FastAPI returns for a path
/// that matches no route (the CMS `{resource}` capture swallows unknown
/// resources, so the handler must answer the same way).
fn unknown_resource() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "http_error", "Not Found")
}

// --- `_to_dict` -----------------------------------------------------------

fn dt_json(v: Option<DateTime<Utc>>) -> Value {
    match v {
        Some(dt) => Value::String(dt.to_rfc3339()),
        None => Value::Null,
    }
}

fn uuid_json(v: Option<Uuid>) -> Value {
    match v {
        Some(u) => Value::String(u.to_string()),
        None => Value::Null,
    }
}

/// Serialize one CMS row in `__table__.columns` order.
fn row_to_dict(res: &Resource, row: &DynRow) -> ApiResult<Value> {
    let mut out = Map::new();
    for col in res.columns {
        let value = match *col {
            "id" => {
                let v: Uuid = row.try_get("id")?;
                Value::String(v.to_string())
            }
            "theme_id" | "parent_id" | "created_by" => {
                uuid_json(row.try_get::<Option<Uuid>, _>(col)?)
            }
            "name" | "text" | "scholar" | "title" | "subtitle" | "label" | "message" | "blocks"
            | "seo_meta" | "media" | "cta" | "audience" | "placements" | "distribution" => {
                let v: Option<Value> = row.try_get::<Option<Value>, _>(col)?;
                v.unwrap_or(Value::Null)
            }
            "hijri_month" | "hijri_day" | "sort_order" | "priority" | "version" => {
                let v: i32 = row.try_get(col)?;
                json!(v)
            }
            "recurring"
            | "auto_publish"
            | "needs_admin_confirmation"
            | "visible"
            | "dismissible" => {
                let v: bool = row.try_get(col)?;
                json!(v)
            }
            "image_url" | "source" | "avatar_url" | "slug" | "content_type" | "locale"
            | "link_type" | "url" | "placement" | "workflow_status" => {
                let v: Option<String> = row.try_get::<Option<String>, _>(col)?;
                v.map(Value::String).unwrap_or(Value::Null)
            }
            "quote_date" => {
                let v: Option<NaiveDate> = row.try_get::<Option<NaiveDate>, _>(col)?;
                v.map(|d| Value::String(d.to_string()))
                    .unwrap_or(Value::Null)
            }
            "starts_at" | "ends_at" | "published_at" | "created_at" | "updated_at" => {
                dt_json(row.try_get::<Option<DateTime<Utc>>, _>(col)?)
            }
            _ => Value::Null,
        };
        out.insert((*col).to_string(), value);
    }
    Ok(Value::Object(out))
}

async fn fetch_row_by_id(db: &Db, res: &Resource, id: Uuid) -> ApiResult<Option<DynRow>> {
    let sql = format!(
        "SELECT {} FROM {} WHERE id = ?",
        res.columns.join(", "),
        res.table
    );
    db.fetch_optional(&sql, &[(id).into_arg()]).await
}

async fn row_dict_by_id(db: &Db, res: &Resource, id: Uuid) -> ApiResult<Value> {
    let Some(row) = fetch_row_by_id(db, res, id).await? else {
        return Err(ApiError::not_found("not_found", "آیتم یافت نشد"));
    };
    row_to_dict(res, &row)
}

// --- create / update validation -------------------------------------------

/// One prepared column: (column, value). `None` values are typed NULLs.
type Prepared = Vec<(String, Arg)>;

/// Required LocalizedText field on create.
fn create_localized(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    let v = req_localized(body, field)?;
    out.push((field.to_string(), v.into_arg()));
    Ok(())
}

/// Optional LocalizedText field on create: absent/null → SQL NULL.
fn create_opt_localized(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    match body.get(field) {
        Some(Value::Null) | None => {
            out.push((field.to_string(), Arg::NullJson));
        }
        Some(v) => {
            let checked = localized_value(v, field)?;
            out.push((field.to_string(), checked.into_arg()));
        }
    }
    Ok(())
}

/// Optional string field on create: absent/null → SQL NULL.
fn create_opt_str(body: &Value, field: &str, max: usize, out: &mut Prepared) -> VResult<()> {
    match body.get(field) {
        Some(Value::Null) | None => {
            out.push((field.to_string(), Arg::NullText));
        }
        Some(v) => {
            let s = str_value(v, field, max)?;
            out.push((field.to_string(), s.into_arg()));
        }
    }
    Ok(())
}

/// Optional datetime field on create: absent/null → SQL NULL.
fn create_opt_dt(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    match opt_datetime(body, field)? {
        Some(dt) => out.push((field.to_string(), dt.into_arg())),
        None => out.push((field.to_string(), Arg::NullTime)),
    }
    Ok(())
}

/// Optional distribution field on create: absent/null → SQL NULL.
fn create_opt_distribution(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    match body.get(field) {
        Some(Value::Null) | None => {
            out.push((field.to_string(), Arg::NullJson));
        }
        Some(v) => {
            let d = distribution_value(v, field)?;
            out.push((field.to_string(), d.into_arg()));
        }
    }
    Ok(())
}

/// Optional UUID field on create: absent/null → SQL NULL.
fn create_opt_uuid(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    match body.get(field) {
        Some(Value::Null) | None => {
            out.push((field.to_string(), Arg::NullUuid));
        }
        Some(v) => {
            let u = uuid_value(v, field)?;
            out.push((field.to_string(), u.into_arg()));
        }
    }
    Ok(())
}

/// Optional bool field on create (Pydantic lax coercion, default `dflt`).
fn create_opt_bool(body: &Value, field: &str, dflt: bool, out: &mut Prepared) -> VResult<()> {
    match body.get(field) {
        Some(Value::Null) | None => {
            out.push((field.to_string(), dflt.into_arg()));
        }
        Some(v) => {
            let b = as_bool(v).ok_or_else(|| validation_err(field, "must be a boolean"))?;
            out.push((field.to_string(), b.into_arg()));
        }
    }
    Ok(())
}

/// Optional int field on create (Pydantic lax coercion, default `dflt`).
fn create_opt_int(
    body: &Value,
    field: &str,
    min: i64,
    max: i64,
    dflt: i64,
    out: &mut Prepared,
) -> VResult<()> {
    match body.get(field) {
        Some(Value::Null) | None => {
            out.push((field.to_string(), dflt.into_arg()));
        }
        Some(v) => match as_int(v) {
            Some(n) if (min..=max).contains(&n) => {
                out.push((field.to_string(), n.into_arg()));
            }
            _ => return Err(validation_err(field, "out of range")),
        },
    }
    Ok(())
}

/// Validate a create body per resource. Returns the resource columns
/// (everything except id / created_at / updated_at / workflow_status /
/// version / created_by, which the handler adds).
fn validate_create(res: &Resource, body: &Value) -> VResult<Prepared> {
    let mut out: Prepared = Vec::new();
    match res.name {
        "events" => {
            create_localized(body, "name", &mut out)?;
            let m = req_int_loose(body, "hijri_month", 1, 12)?;
            out.push(("hijri_month".to_string(), m.into_arg()));
            let d = req_int_loose(body, "hijri_day", 1, 30)?;
            out.push(("hijri_day".to_string(), d.into_arg()));
            create_opt_bool(body, "recurring", true, &mut out)?;
            create_opt_uuid(body, "theme_id", &mut out)?;
            create_opt_bool(body, "auto_publish", false, &mut out)?;
            create_opt_bool(body, "needs_admin_confirmation", true, &mut out)?;
            create_opt_str(body, "image_url", 1024, &mut out)?;
            create_opt_dt(body, "starts_at", &mut out)?;
            create_opt_dt(body, "ends_at", &mut out)?;
            create_opt_distribution(body, "distribution", &mut out)?;
        }
        "quotes" => {
            create_localized(body, "text", &mut out)?;
            create_localized(body, "scholar", &mut out)?;
            create_opt_str(body, "source", 512, &mut out)?;
            match opt_date(body, "quote_date")? {
                Some(d) => out.push(("quote_date".to_string(), d.into_arg())),
                None => out.push(("quote_date".to_string(), Arg::NullDate)),
            }
            create_opt_str(body, "avatar_url", 1024, &mut out)?;
            match body.get("placements") {
                Some(Value::Null) | None => {
                    out.push(("placements".to_string(), Value::Array(vec![]).into_arg()));
                }
                Some(v) => {
                    let list = str_list_value(v, "placements")?;
                    out.push((
                        "placements".to_string(),
                        Value::Array(list.into_iter().map(Value::String).collect()).into_arg(),
                    ));
                }
            }
            create_opt_distribution(body, "distribution", &mut out)?;
        }
        "pages" => {
            let slug = req_str_raw(body, "slug", 1, 160)?;
            if !is_page_slug(&slug) {
                return Err(validation_err("slug", "invalid slug"));
            }
            out.push(("slug".to_string(), slug.into_arg()));
            create_localized(body, "title", &mut out)?;
            match body.get("blocks") {
                Some(Value::Null) => {
                    return Err(validation_err("blocks", "must be a list"));
                }
                Some(v) => {
                    let Value::Array(items) = v else {
                        return Err(validation_err("blocks", "must be a list"));
                    };
                    if items.iter().any(|i| !i.is_object()) {
                        return Err(validation_err("blocks", "must be a list of objects"));
                    }
                    out.push(("blocks".to_string(), v.clone().into_arg()));
                }
                None => {
                    out.push(("blocks".to_string(), Value::Array(vec![]).into_arg()));
                }
            }
            match body.get("seo_meta") {
                Some(Value::Null) => {
                    return Err(validation_err("seo_meta", "must be an object"));
                }
                Some(v) => {
                    let d = dict_value(v, "seo_meta")?;
                    out.push(("seo_meta".to_string(), d.into_arg()));
                }
                None => {
                    out.push(("seo_meta".to_string(), Value::Object(Map::new()).into_arg()));
                }
            }
            match body.get("content_type") {
                Some(Value::Null) | None => {
                    out.push(("content_type".to_string(), "page".into_arg()));
                }
                Some(v) => {
                    let s = str_value(v, "content_type", 16)?;
                    if !["page", "article", "faq"].contains(&s.as_str()) {
                        return Err(validation_err("content_type", "invalid content type"));
                    }
                    out.push(("content_type".to_string(), s.into_arg()));
                }
            }
            out.push(("published_at".to_string(), Arg::NullTime));
            create_opt_distribution(body, "distribution", &mut out)?;
        }
        "nav" => {
            create_localized(body, "label", &mut out)?;
            create_opt_uuid(body, "parent_id", &mut out)?;
            create_opt_int(body, "sort_order", i64::MIN, i64::MAX, 0, &mut out)?;
            create_opt_bool(body, "visible", true, &mut out)?;
            match body.get("locale") {
                Some(Value::Null) | None => {
                    out.push(("locale".to_string(), Arg::NullText));
                }
                Some(v) => {
                    let s = str_value(v, "locale", 8)?;
                    if !["fa", "ar", "en"].contains(&s.as_str()) {
                        return Err(validation_err("locale", "invalid locale"));
                    }
                    out.push(("locale".to_string(), s.into_arg()));
                }
            }
            match body.get("link_type") {
                Some(Value::Null) | None => {
                    out.push(("link_type".to_string(), "internal".into_arg()));
                }
                Some(v) => {
                    let s = str_value(v, "link_type", 16)?;
                    if !["internal", "external"].contains(&s.as_str()) {
                        return Err(validation_err("link_type", "invalid link type"));
                    }
                    out.push(("link_type".to_string(), s.into_arg()));
                }
            }
            let url = req_str_raw(body, "url", 1, 1024)?;
            out.push(("url".to_string(), url.into_arg()));
            create_opt_dt(body, "starts_at", &mut out)?;
            create_opt_dt(body, "ends_at", &mut out)?;
        }
        "banners" => {
            create_localized(body, "title", &mut out)?;
            create_opt_localized(body, "subtitle", &mut out)?;
            match body.get("media") {
                Some(Value::Null) => {
                    return Err(validation_err("media", "must be an object"));
                }
                Some(v) => {
                    let d = dict_value(v, "media")?;
                    out.push(("media".to_string(), d.into_arg()));
                }
                None => {
                    out.push(("media".to_string(), Value::Object(Map::new()).into_arg()));
                }
            }
            match body.get("cta") {
                Some(Value::Null) | None => {
                    out.push(("cta".to_string(), Arg::NullJson));
                }
                Some(v) => {
                    let d = dict_value(v, "cta")?;
                    out.push(("cta".to_string(), d.into_arg()));
                }
            }
            match body.get("placement") {
                Some(Value::Null) | None => {
                    out.push(("placement".to_string(), "homepage".into_arg()));
                }
                Some(v) => {
                    let s = str_value(v, "placement", 64)?;
                    out.push(("placement".to_string(), s.into_arg()));
                }
            }
            create_opt_int(body, "priority", i64::MIN, i64::MAX, 0, &mut out)?;
            match body.get("audience") {
                Some(Value::Null) => {
                    return Err(validation_err("audience", "must be an object"));
                }
                Some(v) => {
                    let d = dict_value(v, "audience")?;
                    out.push(("audience".to_string(), d.into_arg()));
                }
                None => {
                    out.push(("audience".to_string(), Value::Object(Map::new()).into_arg()));
                }
            }
            create_opt_dt(body, "starts_at", &mut out)?;
            create_opt_dt(body, "ends_at", &mut out)?;
            create_opt_distribution(body, "distribution", &mut out)?;
        }
        "announcements" => {
            create_localized(body, "message", &mut out)?;
            create_opt_bool(body, "dismissible", true, &mut out)?;
            match body.get("audience") {
                Some(Value::Null) => {
                    return Err(validation_err("audience", "must be an object"));
                }
                Some(v) => {
                    let d = dict_value(v, "audience")?;
                    out.push(("audience".to_string(), d.into_arg()));
                }
                None => {
                    out.push(("audience".to_string(), Value::Object(Map::new()).into_arg()));
                }
            }
            match body.get("placement") {
                Some(Value::Null) | None => {
                    out.push(("placement".to_string(), "global".into_arg()));
                }
                Some(v) => {
                    let s = str_value(v, "placement", 64)?;
                    out.push(("placement".to_string(), s.into_arg()));
                }
            }
            create_opt_dt(body, "starts_at", &mut out)?;
            create_opt_dt(body, "ends_at", &mut out)?;
            create_opt_distribution(body, "distribution", &mut out)?;
        }
        _ => unreachable!(),
    }
    Ok(out)
}

/// Page slug: `^[a-z0-9]+(?:-[a-z0-9]+)*$`.
fn is_page_slug(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    s.split('-').all(|part| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

// --- update validation ----------------------------------------------------

/// One present field on update: validate the raw value (which may be
/// JSON null → SQL NULL) and push the column. `present` means the key
/// exists in the body (Pydantic `exclude_unset=True` semantics).
fn upd_localized(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    let Some(raw) = body.get(field) else {
        return Ok(());
    };
    match raw {
        Value::Null => out.push((field.to_string(), Arg::NullJson)),
        v => {
            let checked = localized_value(v, field)?;
            out.push((field.to_string(), checked.into_arg()));
        }
    }
    Ok(())
}

fn upd_opt_localized(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    upd_localized(body, field, out)
}

fn upd_str(body: &Value, field: &str, max: usize, out: &mut Prepared) -> VResult<()> {
    let Some(raw) = body.get(field) else {
        return Ok(());
    };
    match raw {
        Value::Null => out.push((field.to_string(), Arg::NullText)),
        v => {
            let s = str_value(v, field, max)?;
            out.push((field.to_string(), s.into_arg()));
        }
    }
    Ok(())
}

fn upd_dt(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    let Some(raw) = body.get(field) else {
        return Ok(());
    };
    match raw {
        Value::Null => out.push((field.to_string(), Arg::NullTime)),
        v => {
            let dt = datetime_value(v, field)?;
            out.push((field.to_string(), dt.into_arg()));
        }
    }
    Ok(())
}

fn upd_date(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    let Some(raw) = body.get(field) else {
        return Ok(());
    };
    match raw {
        Value::Null => out.push((field.to_string(), Arg::NullDate)),
        v => {
            let d = date_value(v, field)?;
            out.push((field.to_string(), d.into_arg()));
        }
    }
    Ok(())
}

fn upd_uuid(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    let Some(raw) = body.get(field) else {
        return Ok(());
    };
    match raw {
        Value::Null => out.push((field.to_string(), Arg::NullUuid)),
        v => {
            let u = uuid_value(v, field)?;
            out.push((field.to_string(), u.into_arg()));
        }
    }
    Ok(())
}

fn upd_bool(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    let Some(raw) = body.get(field) else {
        return Ok(());
    };
    match raw {
        Value::Null => out.push((field.to_string(), Arg::NullBool)),
        v => {
            let b = as_bool(v).ok_or_else(|| validation_err(field, "must be a boolean"))?;
            out.push((field.to_string(), b.into_arg()));
        }
    }
    Ok(())
}

fn upd_int(body: &Value, field: &str, min: i64, max: i64, out: &mut Prepared) -> VResult<()> {
    let Some(raw) = body.get(field) else {
        return Ok(());
    };
    match raw {
        Value::Null => out.push((field.to_string(), Arg::NullInt)),
        v => match as_int(v) {
            Some(n) if (min..=max).contains(&n) => {
                out.push((field.to_string(), n.into_arg()));
            }
            _ => return Err(validation_err(field, "out of range")),
        },
    }
    Ok(())
}

fn upd_distribution(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    let Some(raw) = body.get(field) else {
        return Ok(());
    };
    match raw {
        Value::Null => out.push((field.to_string(), Arg::NullJson)),
        v => {
            let d = distribution_value(v, field)?;
            out.push((field.to_string(), d.into_arg()));
        }
    }
    Ok(())
}

fn upd_dict(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    let Some(raw) = body.get(field) else {
        return Ok(());
    };
    match raw {
        Value::Null => out.push((field.to_string(), Arg::NullJson)),
        v => {
            let d = dict_value(v, field)?;
            out.push((field.to_string(), d.into_arg()));
        }
    }
    Ok(())
}

fn upd_str_list(body: &Value, field: &str, out: &mut Prepared) -> VResult<()> {
    let Some(raw) = body.get(field) else {
        return Ok(());
    };
    match raw {
        Value::Null => out.push((field.to_string(), Arg::NullJson)),
        v => {
            let list = str_list_value(v, field)?;
            out.push((
                field.to_string(),
                Value::Array(list.into_iter().map(Value::String).collect()).into_arg(),
            ));
        }
    }
    Ok(())
}

/// Validate an update body per resource: only present fields are
/// returned (Pydantic `exclude_unset=True`); unknown fields (including
/// `slug` on pages) are silently ignored.
fn validate_update(res: &Resource, body: &Value) -> VResult<Prepared> {
    let mut out: Prepared = Vec::new();
    match res.name {
        "events" => {
            upd_localized(body, "name", &mut out)?;
            upd_int(body, "hijri_month", 1, 12, &mut out)?;
            upd_int(body, "hijri_day", 1, 30, &mut out)?;
            upd_bool(body, "recurring", &mut out)?;
            upd_uuid(body, "theme_id", &mut out)?;
            upd_bool(body, "auto_publish", &mut out)?;
            upd_bool(body, "needs_admin_confirmation", &mut out)?;
            upd_str(body, "image_url", 1024, &mut out)?;
            upd_dt(body, "starts_at", &mut out)?;
            upd_dt(body, "ends_at", &mut out)?;
            upd_distribution(body, "distribution", &mut out)?;
        }
        "quotes" => {
            upd_localized(body, "text", &mut out)?;
            upd_localized(body, "scholar", &mut out)?;
            upd_str(body, "source", 512, &mut out)?;
            upd_date(body, "quote_date", &mut out)?;
            upd_str(body, "avatar_url", 1024, &mut out)?;
            upd_str_list(body, "placements", &mut out)?;
            upd_distribution(body, "distribution", &mut out)?;
        }
        "pages" => {
            // PageUpdate has no `slug` field — a client-sent slug is
            // dropped by Pydantic `extra='ignore'` (see module docs).
            upd_localized(body, "title", &mut out)?;
            upd_dict(body, "blocks", &mut out)?;
            upd_dict(body, "seo_meta", &mut out)?;
            if let Some(raw) = body.get("content_type") {
                match raw {
                    Value::Null => out.push(("content_type".to_string(), Arg::NullText)),
                    v => {
                        let s = str_value(v, "content_type", 16)?;
                        if !["page", "article", "faq"].contains(&s.as_str()) {
                            return Err(validation_err("content_type", "invalid content type"));
                        }
                        out.push(("content_type".to_string(), s.into_arg()));
                    }
                }
            }
            upd_distribution(body, "distribution", &mut out)?;
        }
        "nav" => {
            upd_localized(body, "label", &mut out)?;
            upd_uuid(body, "parent_id", &mut out)?;
            upd_int(body, "sort_order", i64::MIN, i64::MAX, &mut out)?;
            upd_bool(body, "visible", &mut out)?;
            if let Some(raw) = body.get("locale") {
                match raw {
                    Value::Null => out.push(("locale".to_string(), Arg::NullText)),
                    v => {
                        let s = str_value(v, "locale", 8)?;
                        if !["fa", "ar", "en"].contains(&s.as_str()) {
                            return Err(validation_err("locale", "invalid locale"));
                        }
                        out.push(("locale".to_string(), s.into_arg()));
                    }
                }
            }
            if let Some(raw) = body.get("link_type") {
                match raw {
                    Value::Null => out.push(("link_type".to_string(), Arg::NullText)),
                    v => {
                        let s = str_value(v, "link_type", 16)?;
                        if !["internal", "external"].contains(&s.as_str()) {
                            return Err(validation_err("link_type", "invalid link type"));
                        }
                        out.push(("link_type".to_string(), s.into_arg()));
                    }
                }
            }
            upd_str(body, "url", 1024, &mut out)?;
            upd_dt(body, "starts_at", &mut out)?;
            upd_dt(body, "ends_at", &mut out)?;
        }
        "banners" => {
            upd_localized(body, "title", &mut out)?;
            upd_opt_localized(body, "subtitle", &mut out)?;
            upd_dict(body, "media", &mut out)?;
            upd_dict(body, "cta", &mut out)?;
            upd_str(body, "placement", 64, &mut out)?;
            upd_int(body, "priority", i64::MIN, i64::MAX, &mut out)?;
            upd_dict(body, "audience", &mut out)?;
            upd_dt(body, "starts_at", &mut out)?;
            upd_dt(body, "ends_at", &mut out)?;
            upd_distribution(body, "distribution", &mut out)?;
        }
        "announcements" => {
            upd_localized(body, "message", &mut out)?;
            upd_bool(body, "dismissible", &mut out)?;
            upd_dict(body, "audience", &mut out)?;
            upd_str(body, "placement", 64, &mut out)?;
            upd_dt(body, "starts_at", &mut out)?;
            upd_dt(body, "ends_at", &mut out)?;
            upd_distribution(body, "distribution", &mut out)?;
        }
        _ => unreachable!(),
    }
    Ok(out)
}

// --- control center -------------------------------------------------------

/// `GET /api/v1/content/items` — the adapter registry is empty, so the
/// list is always empty (the "machine" is what is tested, not data).
pub async fn content_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    authenticate(&state, &headers, "/api/v1/content/items").await?;
    let _params = parse_query(&uri);
    Ok(Json(json!({ "items": [], "total": 0 })))
}

/// `GET /api/v1/content/items/{content_type}/{item_id}/preview` —
/// always 404 `unknown_content_type` (empty registry).
pub async fn content_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((content_type, _item_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    authenticate(&state, &headers, "/api/v1/content/items").await?;
    Err(ApiError::not_found(
        "unknown_content_type",
        format!("نوع محتوای ناشناخته: {content_type}"),
    ))
}

/// `GET /api/v1/content/policies/{content_type}/{item_id}` — always
/// 404 `unknown_content_type` (empty registry).
pub async fn content_policies(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((content_type, _item_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    authenticate(&state, &headers, "/api/v1/content/items").await?;
    Err(ApiError::not_found(
        "unknown_content_type",
        format!("نوع محتوای ناشناخته: {content_type}"),
    ))
}

// --- admin: list ----------------------------------------------------------

/// `GET /api/v1/content/{resource}` — any authenticated user.
pub async fn cms_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(resource_name): Path<String>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let res = resource(&resource_name).ok_or_else(unknown_resource)?;
    authenticate(&state, &headers, "/api/v1/content").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 200)?;
    let offset = query_int(&params, "offset", 0, 0, i64::MAX)?;
    let cursor = params.get("cursor").filter(|s| !s.is_empty()).cloned();

    // Filters apply only where `_LIST_FILTERS` allows them; other
    // values are silently ignored (never validated).
    let workflow_status = params
        .get("workflow_status")
        .filter(|s| !s.is_empty())
        .cloned();
    let placement = params.get("placement").filter(|s| !s.is_empty()).cloned();
    let locale = params.get("locale").filter(|s| !s.is_empty()).cloned();
    let ws_filter = match res.name {
        "events" | "quotes" | "pages" | "banners" | "announcements" => workflow_status,
        _ => None,
    };
    if let Some(ws) = &ws_filter {
        if !STATUSES.contains(&ws.as_str()) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_status",
                format!("وضعیت ناشناخته: {ws}"),
            ));
        }
    }
    let placement_filter = match res.name {
        "banners" | "announcements" => placement,
        _ => None,
    };
    let locale_filter = match res.name {
        "nav" => locale,
        _ => None,
    };

    let db = state.db_or_500()?;
    let mut base = format!("SELECT {} FROM {}", res.columns.join(", "), res.table);
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(ws) = &ws_filter {
        conds.push("workflow_status = ?".to_string());
        args.push(ws.clone().into_arg());
    }
    if let Some(p) = &placement_filter {
        conds.push("placement = ?".to_string());
        args.push(p.clone().into_arg());
    }
    if let Some(l) = &locale_filter {
        conds.push("locale = ?".to_string());
        args.push(l.clone().into_arg());
    }
    if !conds.is_empty() {
        base.push_str(" WHERE ");
        base.push_str(&conds.join(" AND "));
    }
    let total: i64 = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS total FROM ({base}) AS _c"),
            &args,
        )
        .await?
        .try_get("total")?;
    let limit = limit.min(200);

    let (rows, next_cursor) = if res.name == "nav" {
        // Nav has tree order; keyset pagination is meaningless there —
        // offset only, `next_cursor` always null.
        let sql =
            format!("{base} ORDER BY parent_id IS NULL DESC, sort_order ASC LIMIT ? OFFSET ?");
        let mut a = args.clone();
        a.push(limit.into_arg());
        a.push(offset.max(0).into_arg());
        let rows = db.fetch_all(&sql, &a).await?;
        (rows, None)
    } else if let Some(cursor) = cursor {
        let (ts, lid) = decode_cursor(&cursor)?;
        let mut sql = base.clone();
        if conds.is_empty() {
            sql.push_str(" WHERE ");
        } else {
            sql.push_str(" AND ");
        }
        sql.push_str("((created_at < ?) OR (created_at = ? AND id < ?))");
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
        let mut a = args.clone();
        a.push(ts.into_arg());
        a.push(ts.into_arg());
        a.push(lid.into_arg());
        a.push((limit + 1).into_arg());
        let fetched = db.fetch_all(&sql, &a).await?;
        let next = if (fetched.len() as i64) > limit {
            let last = &fetched[(limit as usize) - 1];
            let ts: DateTime<Utc> = last.try_get("created_at")?;
            let id: Uuid = last.try_get("id")?;
            Some(encode_cursor(ts, id))
        } else {
            None
        };
        let rows = fetched.into_iter().take(limit as usize).collect();
        (rows, next)
    } else {
        let sql = format!("{base} ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?");
        let mut a = args.clone();
        a.push(limit.into_arg());
        a.push(offset.max(0).into_arg());
        let rows = db.fetch_all(&sql, &a).await?;
        let next = if !rows.is_empty() && offset.max(0) + (rows.len() as i64) < total {
            let last = rows.last().unwrap();
            let ts: DateTime<Utc> = last.try_get("created_at")?;
            let id: Uuid = last.try_get("id")?;
            Some(encode_cursor(ts, id))
        } else {
            None
        };
        (rows, next)
    };

    let mut items = Vec::new();
    for row in &rows {
        items.push(row_to_dict(res, row)?);
    }
    Ok(Json(json!({
        "items": items,
        "total": total,
        "next_cursor": next_cursor,
    })))
}

// --- admin: create ----------------------------------------------------------

/// `POST /api/v1/content/{resource}` — `content.create`, 201.
pub async fn cms_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(resource_name): Path<String>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    // POST /content/items exists in Python only as GET → 405.
    if resource_name == "items" {
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "http_error",
            "Method Not Allowed",
        ));
    }
    let res = resource(&resource_name).ok_or_else(unknown_resource)?;
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/content").await?;
    require_permission(&state, &ctx, "content.create").await?;
    let prepared = validate_create(res, &parsed)?;
    let db = state.db_or_500()?;
    if res.name == "pages" {
        let slug = prepared
            .iter()
            .find(|(c, _)| c == "slug")
            .map(|(_, a)| a)
            .and_then(|a| match a {
                Arg::Str(s) => Some(s.clone()),
                _ => None,
            });
        if let Some(slug) = slug {
            let exists = db
                .fetch_optional("SELECT id FROM pages WHERE slug = ?", &[slug.into_arg()])
                .await?;
            if exists.is_some() {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "slug_taken",
                    "این slug قبلاً استفاده شده است",
                ));
            }
        }
    }
    let id = new_id();
    let now = utc_now();
    let mut cols: Vec<String> = prepared.iter().map(|(c, _)| c.clone()).collect();
    let mut args: Vec<Arg> = prepared.into_iter().map(|(_, a)| a).collect();
    cols.push("id".to_string());
    args.push(id.into_arg());
    cols.push("created_at".to_string());
    args.push(now.into_arg());
    cols.push("updated_at".to_string());
    args.push(now.into_arg());
    if res.workflow {
        cols.push("workflow_status".to_string());
        args.push("draft".into_arg());
        cols.push("version".to_string());
        args.push(1i64.into_arg());
        cols.push("created_by".to_string());
        args.push(ctx.user.id.into_arg());
    }
    let placeholders: Vec<&str> = cols.iter().map(|_| "?").collect();
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        res.table,
        cols.join(", "),
        placeholders.join(", ")
    );
    db.execute(&sql, &args).await?;
    let prefix = res.prefix;
    if res.workflow {
        emit_event(
            db,
            prefix,
            &id.to_string(),
            &format!("{prefix}.created"),
            json!({ "workflow_status": "draft", "version": 1 }),
        )
        .await?;
    } else {
        emit_event(
            db,
            prefix,
            &id.to_string(),
            &format!("{prefix}.created"),
            json!({ "id": id.to_string() }),
        )
        .await?;
    }
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        &format!("cms.{}.create", res.name),
        prefix,
        Some(&id.to_string()),
        None,
        Some(json!({ "id": id.to_string() })),
        None,
        None,
    )
    .await?;
    let out = row_dict_by_id(db, res, id).await?;
    Ok((StatusCode::CREATED, Json(out)))
}

// --- admin: get -------------------------------------------------------------

fn parse_cms_uuid(item_id: &str) -> Result<Uuid, ApiError> {
    item_id
        .parse::<Uuid>()
        .map_err(|_| ApiError::not_found("not_found", "آیتم یافت نشد"))
}

/// `GET /api/v1/content/{resource}/{item_id}` — any authenticated user.
pub async fn cms_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((resource_name, item_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let res = resource(&resource_name).ok_or_else(unknown_resource)?;
    authenticate(&state, &headers, "/api/v1/content").await?;
    let id = parse_cms_uuid(&item_id)?;
    let db = state.db_or_500()?;
    Ok(Json(row_dict_by_id(db, res, id).await?))
}

// --- admin: update ----------------------------------------------------------

/// `PATCH /api/v1/content/{resource}/{item_id}` — `content.create`.
pub async fn cms_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((resource_name, item_id)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let res = resource(&resource_name).ok_or_else(unknown_resource)?;
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/content").await?;
    require_permission(&state, &ctx, "content.create").await?;
    let id = parse_cms_uuid(&item_id)?;
    let db = state.db_or_500()?;
    let current = row_dict_by_id(db, res, id).await?;
    let prepared = validate_update(res, &parsed)?;
    let old_status = current
        .get("workflow_status")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let old_version = current.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
    let new_version = old_version + 1;
    let now = utc_now();
    let mut sets: Vec<String> = prepared.iter().map(|(c, _)| format!("{c} = ?")).collect();
    let mut args: Vec<Arg> = prepared.iter().map(|(_, a)| a.clone()).collect();
    let mut field_names: Vec<String> = prepared.iter().map(|(c, _)| c.clone()).collect();
    if res.workflow {
        sets.push("version = ?".to_string());
        args.push(new_version.into_arg());
    }
    sets.push("updated_at = ?".to_string());
    args.push(now.into_arg());
    args.push(id.into_arg());
    let sql = format!("UPDATE {} SET {} WHERE id = ?", res.table, sets.join(", "));
    db.execute(&sql, &args).await?;
    let prefix = res.prefix;
    if res.workflow {
        emit_event(
            db,
            prefix,
            &id.to_string(),
            &format!("{prefix}.updated"),
            json!({ "workflow_status": old_status, "version": new_version }),
        )
        .await?;
    } else {
        emit_event(
            db,
            prefix,
            &id.to_string(),
            &format!("{prefix}.updated"),
            json!({ "version": Value::Null }),
        )
        .await?;
    }
    field_names.sort();
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        &format!("cms.{}.update", res.name),
        prefix,
        Some(&id.to_string()),
        None,
        Some(json!({ "updated_fields": field_names })),
        None,
        None,
    )
    .await?;
    Ok(Json(row_dict_by_id(db, res, id).await?))
}

// --- admin: transition ------------------------------------------------------

/// `POST /api/v1/content/{resource}/{item_id}/transition` — the
/// permission is chosen from the body (`published` → `content.publish`,
/// anything else → `content.create`) and checked before the item is
/// loaded, exactly like the Python router.
pub async fn cms_transition(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((resource_name, item_id)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let res = resource(&resource_name).ok_or_else(unknown_resource)?;
    let parsed = parse_body(&body)?;
    let to = match parsed.get("to") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(validation_err("to", "field is required")),
    };
    let ctx = authenticate(&state, &headers, "/api/v1/content").await?;
    let perm = if to == "published" {
        "content.publish"
    } else {
        "content.create"
    };
    require_permission(&state, &ctx, perm).await?;
    if !res.workflow {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no_workflow",
            "این منبع گردش‌کار ندارد",
        ));
    }
    let id = parse_cms_uuid(&item_id)?;
    let db = state.db_or_500()?;
    let current = row_dict_by_id(db, res, id).await?;
    let current_status = current
        .get("workflow_status")
        .and_then(|v| v.as_str())
        .unwrap_or("draft")
        .to_string();
    validate_transition(&current_status, &to)?;
    let old_version = current.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
    let new_version = old_version + 1;
    let old_distribution = current.get("distribution").cloned();
    let now = utc_now();
    let mut sets = vec!["workflow_status = ?".to_string(), "version = ?".to_string()];
    let mut args: Vec<Arg> = vec![to.clone().into_arg(), new_version.into_arg()];
    let mut new_distribution = old_distribution.clone();
    if to == "published" {
        if res.name == "pages"
            && current
                .get("published_at")
                .map(|v| v.is_null())
                .unwrap_or(true)
        {
            sets.push("published_at = ?".to_string());
            args.push(now.into_arg());
        }
        if old_distribution
            .as_ref()
            .map(|v| v.is_null())
            .unwrap_or(true)
        {
            new_distribution = Some(default_distribution());
            sets.push("distribution = ?".to_string());
            args.push(default_distribution().into_arg());
        }
    }
    sets.push("updated_at = ?".to_string());
    args.push(now.into_arg());
    args.push(id.into_arg());
    let sql = format!("UPDATE {} SET {} WHERE id = ?", res.table, sets.join(", "));
    db.execute(&sql, &args).await?;
    let prefix = res.prefix;
    emit_event(
        db,
        prefix,
        &id.to_string(),
        &format!("{prefix}.{to}"),
        json!({
            "from": current_status,
            "to": to,
            "version": new_version,
            "distribution": new_distribution,
        }),
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        &format!("cms.{}.transition", res.name),
        prefix,
        Some(&id.to_string()),
        Some(json!({ "workflow_status": current_status })),
        Some(json!({ "workflow_status": to, "version": new_version })),
        None,
        None,
    )
    .await?;
    Ok(Json(row_dict_by_id(db, res, id).await?))
}

// --- admin: delete (archive) ------------------------------------------------

/// `DELETE /api/v1/content/{resource}/{item_id}` — archive, never a
/// physical delete. Workflow resources go to `archived` via the §9
/// machine; nav just sets `visible=false`.
pub async fn cms_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((resource_name, item_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let res = resource(&resource_name).ok_or_else(unknown_resource)?;
    let ctx = authenticate(&state, &headers, "/api/v1/content").await?;
    require_permission(&state, &ctx, "content.create").await?;
    let id = parse_cms_uuid(&item_id)?;
    let db = state.db_or_500()?;
    let current = row_dict_by_id(db, res, id).await?;
    let now = utc_now();
    if res.name == "nav" {
        db.execute(
            "UPDATE nav_items SET visible = ?, updated_at = ? WHERE id = ?",
            &[false.into_arg(), now.into_arg(), id.into_arg()],
        )
        .await?;
        emit_event(
            db,
            "nav_item",
            &id.to_string(),
            "nav_item.archived",
            json!({ "visible": false }),
        )
        .await?;
        log_audit(
            db,
            Some(ctx.user.id),
            "admin",
            "cms.nav.archive",
            "nav_item",
            Some(&id.to_string()),
            Some(json!({ "visible": true })),
            Some(json!({ "visible": false })),
            None,
            None,
        )
        .await?;
    } else {
        let current_status = current
            .get("workflow_status")
            .and_then(|v| v.as_str())
            .unwrap_or("draft")
            .to_string();
        validate_transition(&current_status, "archived")?;
        let old_version = current.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
        let new_version = old_version + 1;
        let distribution = current.get("distribution").cloned();
        db.execute(
            &format!(
                "UPDATE {} SET workflow_status = ?, version = ?, updated_at = ? WHERE id = ?",
                res.table
            ),
            &[
                "archived".into_arg(),
                new_version.into_arg(),
                now.into_arg(),
                id.into_arg(),
            ],
        )
        .await?;
        let prefix = res.prefix;
        emit_event(
            db,
            prefix,
            &id.to_string(),
            &format!("{prefix}.archived"),
            json!({
                "from": current_status,
                "to": "archived",
                "version": new_version,
                "distribution": distribution,
            }),
        )
        .await?;
        log_audit(
            db,
            Some(ctx.user.id),
            "admin",
            &format!("cms.{}.transition", res.name),
            prefix,
            Some(&id.to_string()),
            Some(json!({ "workflow_status": current_status })),
            Some(json!({ "workflow_status": "archived", "version": new_version })),
            None,
            None,
        )
        .await?;
    }
    Ok(Json(row_dict_by_id(db, res, id).await?))
}

// --- public reads -----------------------------------------------------------

fn in_window(
    starts_at: Option<DateTime<Utc>>,
    ends_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    if let Some(s) = starts_at {
        if s > now {
            return false;
        }
    }
    if let Some(e) = ends_at {
        if e < now {
            return false;
        }
    }
    true
}

fn window_ok(row: &DynRow, now: DateTime<Utc>) -> ApiResult<bool> {
    let starts: Option<DateTime<Utc>> = row.try_get("starts_at")?;
    let ends: Option<DateTime<Utc>> = row.try_get("ends_at")?;
    Ok(in_window(starts, ends, now))
}

async fn public_list_filtered(
    state: &AppState,
    sql: &str,
    args: &[Arg],
    res: &Resource,
) -> ApiResult<Vec<Value>> {
    let db = state.db_or_500()?;
    let rows = db.fetch_all(sql, args).await?;
    let now = utc_now();
    let mut out = Vec::new();
    for row in &rows {
        if window_ok(row, now)? {
            out.push(row_to_dict(res, row)?);
        }
    }
    Ok(out)
}

/// `GET /api/v1/content/public/nav?locale=` — visible items in their
/// time window, nested into a tree.
pub async fn public_nav(State(state): State<AppState>, uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let locale = params.get("locale").filter(|s| !s.is_empty()).cloned();
    let res = resource("nav").unwrap();
    let db = state.db_or_500()?;
    let mut sql =
        "SELECT id, label, parent_id, sort_order, visible, locale, link_type, url, starts_at, ends_at, created_at, updated_at FROM nav_items WHERE visible IS TRUE"
            .to_string();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(l) = &locale {
        sql.push_str(" AND (locale IS NULL OR locale = ?)");
        args.push(l.clone().into_arg());
    }
    sql.push_str(" ORDER BY parent_id IS NULL DESC, sort_order ASC");
    let rows = db.fetch_all(&sql, &args).await?;
    let now = utc_now();
    let mut items = Vec::new();
    for row in &rows {
        if window_ok(row, now)? {
            items.push(row_to_dict(res, row)?);
        }
    }
    Ok(Json(build_nav_tree(items)))
}

/// `_build_nav_tree`: flat rows nested by `parent_id` into `children`.
/// Roots are rows whose `parent_id` is null or points at a missing /
/// filtered-out row; children keep the SQL order.
fn build_nav_tree(items: Vec<Value>) -> Value {
    use std::collections::HashMap;
    let mut by_id: HashMap<String, Map<String, Value>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for item in &items {
        let Some(map) = item.as_object() else {
            continue;
        };
        let mut node = map.clone();
        node.insert("children".to_string(), Value::Array(vec![]));
        if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
            order.push(id.to_string());
            by_id.insert(id.to_string(), node);
        }
    }
    // parent_id → child ids, in items order.
    let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots: Vec<String> = Vec::new();
    for item in &items {
        let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let parent = item
            .get("parent_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        match parent {
            Some(p) if by_id.contains_key(p) => {
                children_of
                    .entry(p.to_string())
                    .or_default()
                    .push(id.to_string());
            }
            _ => roots.push(id.to_string()),
        }
    }
    fn build(
        id: &str,
        by_id: &HashMap<String, Map<String, Value>>,
        children_of: &HashMap<String, Vec<String>>,
    ) -> Value {
        let mut node = by_id.get(id).cloned().unwrap_or_default();
        let kids: Vec<Value> = children_of
            .get(id)
            .map(|ids| {
                ids.iter()
                    .map(|cid| build(cid, by_id, children_of))
                    .collect()
            })
            .unwrap_or_default();
        node.insert("children".to_string(), Value::Array(kids));
        Value::Object(node)
    }
    let out: Vec<Value> = roots
        .iter()
        .map(|id| build(id, &by_id, &children_of))
        .collect();
    Value::Array(out)
}

/// `GET /api/v1/content/public/banners?placement=`.
pub async fn public_banners(State(state): State<AppState>, uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let placement = params.get("placement").filter(|s| !s.is_empty()).cloned();
    let res = resource("banners").unwrap();
    let mut sql = "SELECT id, title, subtitle, media, cta, placement, priority, audience, starts_at, ends_at, created_at, updated_at, workflow_status, version, created_by, distribution FROM banners WHERE workflow_status = 'published'".to_string();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(p) = &placement {
        sql.push_str(" AND placement = ?");
        args.push(p.clone().into_arg());
    }
    sql.push_str(" ORDER BY priority DESC, created_at DESC");
    Ok(Json(Value::Array(
        public_list_filtered(&state, &sql, &args, res).await?,
    )))
}

/// `GET /api/v1/content/public/announcements?placement=`.
pub async fn public_announcements(
    State(state): State<AppState>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let placement = params.get("placement").filter(|s| !s.is_empty()).cloned();
    let res = resource("announcements").unwrap();
    let mut sql = "SELECT id, workflow_status, version, created_by, message, dismissible, audience, placement, starts_at, ends_at, distribution, created_at, updated_at FROM announcements WHERE workflow_status = 'published'".to_string();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(p) = &placement {
        sql.push_str(" AND placement = ?");
        args.push(p.clone().into_arg());
    }
    sql.push_str(" ORDER BY created_at DESC");
    Ok(Json(Value::Array(
        public_list_filtered(&state, &sql, &args, res).await?,
    )))
}

/// `GET /api/v1/content/public/pages?content_type=article&limit&offset`
/// — only `article` is supported (422 otherwise).
pub async fn public_pages(State(state): State<AppState>, uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 200)?;
    let offset = query_int(&params, "offset", 0, 0, i64::MAX)?;
    let content_type = params
        .get("content_type")
        .cloned()
        .unwrap_or_else(|| "article".to_string());
    if content_type != "article" {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_content_type",
            "فقط content_type=article پشتیبانی می‌شود",
        ));
    }
    let res = resource("pages").unwrap();
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, slug, title, blocks, seo_meta, content_type, published_at, created_at, updated_at, workflow_status, version, created_by, distribution FROM pages WHERE workflow_status = 'published' AND content_type = 'article' ORDER BY published_at DESC NULLS LAST, created_at DESC LIMIT ? OFFSET ?",
            &[limit.min(200).into_arg(), offset.max(0).into_arg()],
        )
        .await?;
    let mut items = Vec::new();
    for row in &rows {
        items.push(row_to_dict(res, row)?);
    }
    Ok(Json(Value::Array(items)))
}

/// `GET /api/v1/content/public/pages/slug/{slug}`.
pub async fn public_page_slug(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> ApiResult<Json<Value>> {
    let res = resource("pages").unwrap();
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT id, slug, title, blocks, seo_meta, content_type, published_at, created_at, updated_at, workflow_status, version, created_by, distribution FROM pages WHERE slug = ? AND workflow_status = 'published'",
            &[slug.into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::not_found("page_not_found", "صفحه یافت نشد"));
    };
    Ok(Json(row_to_dict(res, &row)?))
}

/// `GET /api/v1/content/public/events?hijri_month&hijri_day`.
pub async fn public_events(State(state): State<AppState>, uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let hijri_month = query_opt_int(&params, "hijri_month", 1, 12)?;
    let hijri_day = query_opt_int(&params, "hijri_day", 1, 30)?;
    let res = resource("events").unwrap();
    let db = state.db_or_500()?;
    let mut sql = "SELECT id, name, hijri_month, hijri_day, recurring, theme_id, auto_publish, needs_admin_confirmation, image_url, starts_at, ends_at, created_at, updated_at, workflow_status, version, created_by, distribution FROM islamic_events WHERE workflow_status = 'published'".to_string();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(m) = hijri_month {
        sql.push_str(" AND hijri_month = ?");
        args.push(m.into_arg());
    }
    if let Some(d) = hijri_day {
        sql.push_str(" AND hijri_day = ?");
        args.push(d.into_arg());
    }
    sql.push_str(" ORDER BY hijri_month ASC, hijri_day ASC");
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items = Vec::new();
    for row in &rows {
        items.push(row_to_dict(res, row)?);
    }
    Ok(Json(Value::Array(items)))
}

/// `GET /api/v1/content/public/quotes?placement=` — placement is
/// filtered in Rust (`placement in (r.placements or [])`).
pub async fn public_quotes(State(state): State<AppState>, uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let placement = params.get("placement").filter(|s| !s.is_empty()).cloned();
    let res = resource("quotes").unwrap();
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, text, scholar, source, quote_date, avatar_url, placements, created_at, updated_at, workflow_status, version, created_by, distribution FROM scholar_quotes WHERE workflow_status = 'published' ORDER BY created_at DESC",
            &[],
        )
        .await?;
    let mut items = Vec::new();
    for row in &rows {
        let dict = row_to_dict(res, row)?;
        if let Some(p) = &placement {
            let placements = dict
                .get("placements")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .any(|s| s == p.as_str())
                })
                .unwrap_or(false);
            if !placements {
                continue;
            }
        }
        items.push(dict);
    }
    Ok(Json(Value::Array(items)))
}

/// `GET /api/v1/content/public/calendar/theme` — the active calendar
/// theme from `platform_settings` + today's Hijri month/day.
pub async fn public_calendar_theme(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let raw = get_setting(db, &state.config, "cms.calendar.active").await?;
    let active: Value = match raw.as_deref() {
        Some(s) if !s.is_empty() => serde_json::from_str(s).map_err(|_| ApiError::internal())?,
        _ => json!({}),
    };
    let (month, day) = hijri_month_day_today();
    Ok(Json(json!({
        "theme_id": active.get("theme_id").cloned().unwrap_or(Value::Null),
        "event_id": active.get("event_id").cloned().unwrap_or(Value::Null),
        "hijri": { "month": month, "day": day },
    })))
}
