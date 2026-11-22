//! Finance routes — mirrors `app/modules/finance/` (Wave 7).
//!
//! Natively implemented: online donation intents (ZarinPal + IDPay,
//! request + verify over HTTPS), the ZarinPal redirect callback, the
//! IDPay form callback, signed webhooks (server-side verify, idempotent
//! dedupe), donation methods, manual donations (card / crypto / PayPal
//! with receipt image or tracking text, admin approve/reject with live
//! FX conversion), campaigns (public + admin CRUD), admin donation and
//! ledger lists, gateway statuses, reconciliation, FX rates, and
//! recurring donations (registration only — periodic charging is the
//! ARQ worker's job, like Python).
//!
//! Deliberately NOT native (FIN-01): `GET /api/v1/finance/receipts/{id}`
//! — the trilingual receipt PDF needs reportlab + arabic-reshaper +
//! python-bidi + system fonts (ADR-008 / ADR-061 class). Since the
//! Wave 8 lift it is a sidecar-contract route in `routes_lift`
//! (ADR-134): it forwards to `SIDECAR_URL` when one is configured
//! and answers an honest 503 `receipt_pdf_unavailable` when it is
//! not, instead of answering a 503 for a working feature (ADR-046
//! precedent).
//!
//! Money rules ported exactly: idempotency-key replay of intents,
//! `_finalize_success` re-entrancy (one donation + one ledger row per
//! intent, campaign credit in IRR), manual-approve FX conversion with
//! the manual rate taking precedence over the live rate, and the
//! `manual:<intent_id>` fallback reference. Receipt images go through
//! the Wave 6 scan gate (ADR-051: SHA-256 blocklist + EICAR + the
//! pending/review workflow); Python's finance module itself never
//! scanned, so the gate is a deliberate, task-directed hardening
//! (FIN-02).

use axum::{
    body::Body,
    extract::{ConnectInfo, Multipart, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{Json, Response},
    Form,
};
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::{
    audit::log_audit,
    authn::{authenticate, client_ip, require_permission, AuthCtx},
    db::{utc_now, Arg, Db, DynRow, IntoArg},
    errors::{ApiError, ApiResult},
    settings::{get_setting, get_typed_setting},
    storage::{put_object, read_bytes},
    validate::{
        as_int, opt_localized, opt_str, opt_uuid, parse_body, parse_datetime_str, parse_path_uuid,
        parse_query, query_int, req_localized,
    },
    AppState, Config,
};

/// Per-transaction cap: 10^11 IRR (Python `_MAX_DONATE_IRR`).
const MAX_DONATE_IRR: i64 = 100_000_000_000;
/// Cap for non-IRR manual amounts, in whole currency units.
const MAX_MANUAL_OTHER: i64 = 1_000_000_000_000;
/// Receipt image cap: 5 MB (Python `RECEIPT_IMAGE_MAX_BYTES`).
const RECEIPT_IMAGE_MAX_BYTES: usize = 5 * 1024 * 1024;
/// Cursor page cap (Python `_CURSOR_LIMIT`).
const CURSOR_LIMIT: i64 = 50;

const ZARINPAL_SANDBOX_BASE: &str = "https://sandbox.zarinpal.com";
const ZARINPAL_PROD_BASE: &str = "https://api.zarinpal.com";
const ZARINPAL_SANDBOX_PAY: &str = "https://sandbox.zarinpal.com/pg/StartPay";
const ZARINPAL_PROD_PAY: &str = "https://www.zarinpal.com/pg/StartPay";
const IDPAY_BASE: &str = "https://api.idpay.ir/v1.1";
const IDPAY_PAY: &str = "https://idpay.ir/p/ws";
const IDPAY_SANDBOX_PAY: &str = "https://idpay.ir/p/ws-sandbox";

const WALLEX_MARKETS_URL: &str = "https://api.wallex.ir/v1/markets";
const COINGECKO_TON_URL: &str =
    "https://api.coingecko.com/api/v3/simple/price?ids=toncoin&vs_currencies=usd";
const FX_USER_AGENT: &str = "maturidi-academy/1.0 (+finance-fx)";
/// FX cache TTL: 5 minutes (Python `_CACHE_TTL_S`).
const FX_CACHE_TTL_SECS: f64 = 300.0;

/// The EICAR standard antivirus test file (same bytes as the Wave 6
/// library gate).
const EICAR: &[u8] = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*";

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// Python `bool(value)` — a stored JSON string `"false"` is truthy,
/// a JSON bool is itself, `null` is falsy. Callers that need the
/// "null means default" rule use `typed_bool_or` instead.
fn py_bool(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Number(n) => n
            .as_i64()
            .map(|i| i != 0)
            .unwrap_or_else(|| n.as_f64().map(|f| f != 0.0).unwrap_or(false)),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `get_typed_setting` bool with Python's "null means the default"
/// rule (`_flag` / `_gateway_config` / `gateway_statuses`).
fn typed_bool_or(value: &Value, default: bool) -> bool {
    if value.is_null() {
        default
    } else {
        py_bool(value)
    }
}

/// Optional auth (like the Python `_optional_user`): any auth failure
/// → guest, never an error.
async fn optional_user(state: &AppState, headers: &HeaderMap) -> Option<AuthCtx> {
    authenticate(state, headers, "/api/v1/finance").await.ok()
}

/// Admin auth for the finance prefix (`finance.manage`).
async fn admin_ctx(state: &AppState, headers: &HeaderMap) -> ApiResult<AuthCtx> {
    let ctx = authenticate(state, headers, "/api/v1/admin/finance").await?;
    require_permission(state, &ctx, "finance.manage").await?;
    Ok(ctx)
}

fn req_meta(
    state: &AppState,
    headers: &HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
) -> (Option<String>, Option<String>) {
    let ip = client_ip(headers, Some(peer.0), &state.config.trusted_proxies);
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (ip, ua)
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// --- finance cursor (Python `_encode_cursor` / `_decode_cursor`) --------

/// Python's finance cursor is `base64url("{iso}|{uuid}")` — a
/// different shape from the shared `validate::encode_cursor` (JSON
/// array), so it lives here. Malformed → 400 `bad_cursor`.
fn encode_cursor(created_at: DateTime<Utc>, id: Uuid) -> String {
    let raw = format!("{}|{}", created_at.to_rfc3339(), id);
    URL_SAFE.encode(raw.as_bytes())
}

fn decode_cursor(cursor: &str) -> Result<(DateTime<Utc>, Uuid), ApiError> {
    let invalid = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_cursor",
            "نشانگر صفحه‌بندی نامعتبر است",
        )
    };
    let bytes = URL_SAFE.decode(cursor.as_bytes()).map_err(|_| invalid())?;
    let text = String::from_utf8(bytes).map_err(|_| invalid())?;
    let (ts_s, id_s) = text.rsplit_once('|').ok_or_else(invalid)?;
    let ts = parse_datetime_str(ts_s).ok_or_else(invalid)?;
    let id = id_s.parse::<Uuid>().map_err(|_| invalid())?;
    Ok((ts, id))
}

/// `progress = round(raised / goal, 4)` — Rust rounds half away from
/// zero, Python `round()` is banker's (ADR-024 class edge difference).
fn round4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

// --- rows ------------------------------------------------------------------

struct Intent {
    id: Uuid,
    idempotency_key: String,
    amount: i64,
    currency: String,
    campaign_id: Option<Uuid>,
    donor_id: Option<Uuid>,
    donor_name: Option<String>,
    status: String,
    gateway: String,
    authority: Option<String>,
}

impl Intent {
    fn from_row(row: &DynRow) -> Result<Self, ApiError> {
        Ok(Self {
            id: row.try_get::<Uuid, _>("id")?,
            idempotency_key: row.try_get::<String, _>("idempotency_key")?,
            amount: row.try_get::<i64, _>("amount")?,
            currency: row.try_get::<String, _>("currency")?,
            campaign_id: row.try_get::<Option<Uuid>, _>("campaign_id")?,
            donor_id: row.try_get::<Option<Uuid>, _>("donor_id")?,
            donor_name: row.try_get::<Option<String>, _>("donor_name")?,
            status: row.try_get::<String, _>("status")?,
            gateway: row.try_get::<String, _>("gateway")?,
            authority: row.try_get::<Option<String>, _>("authority")?,
        })
    }
}

struct Campaign {
    id: Uuid,
    title: Value,
    description: Option<Value>,
    goal_amount: i64,
    raised_amount: i64,
    status: String,
    cover: Option<String>,
    created_at: DateTime<Utc>,
}

impl Campaign {
    fn from_row(row: &DynRow) -> Result<Self, ApiError> {
        Ok(Self {
            id: row.try_get::<Uuid, _>("id")?,
            title: row.try_get::<Value, _>("title")?,
            description: row.try_get::<Option<Value>, _>("description")?,
            goal_amount: row.try_get::<i64, _>("goal_amount")?,
            raised_amount: row.try_get::<i64, _>("raised_amount")?,
            status: row.try_get::<String, _>("status")?,
            cover: row.try_get::<Option<String>, _>("cover")?,
            created_at: row.try_get::<DateTime<Utc>, _>("created_at")?,
        })
    }

    fn out(&self) -> Value {
        let progress = if self.goal_amount != 0 {
            round4(self.raised_amount as f64 / self.goal_amount as f64)
        } else {
            0.0
        };
        json!({
            "id": self.id.to_string(),
            "title": self.title,
            "description": self.description,
            "goal_amount": self.goal_amount,
            "raised_amount": self.raised_amount,
            "progress": progress,
            "status": self.status,
            "cover": self.cover,
            "created_at": self.created_at.to_rfc3339(),
        })
    }
}

struct Donation {
    id: Uuid,
    payment_intent_id: Uuid,
    gateway: String,
    gateway_reference: String,
    status: String,
    amount: i64,
    currency: String,
    campaign_id: Option<Uuid>,
    donor_name: Option<String>,
    created_at: DateTime<Utc>,
}

impl Donation {
    fn from_row(row: &DynRow) -> Result<Self, ApiError> {
        // `donations.amount` is INTEGER in TEST_SCHEMA (ADR-028: read
        // as i32); production is BigInteger.
        Ok(Self {
            id: row.try_get::<Uuid, _>("id")?,
            payment_intent_id: row.try_get::<Uuid, _>("payment_intent_id")?,
            gateway: row.try_get::<String, _>("gateway")?,
            gateway_reference: row.try_get::<String, _>("gateway_reference")?,
            status: row.try_get::<String, _>("status")?,
            amount: row.try_get::<i32, _>("amount")? as i64,
            currency: row.try_get::<String, _>("currency")?,
            campaign_id: row.try_get::<Option<Uuid>, _>("campaign_id")?,
            donor_name: row.try_get::<Option<String>, _>("donor_name")?,
            created_at: row.try_get::<DateTime<Utc>, _>("created_at")?,
        })
    }

    fn out(&self) -> Value {
        json!({
            "id": self.id.to_string(),
            "intent_id": self.payment_intent_id.to_string(),
            "gateway": self.gateway,
            "gateway_reference": self.gateway_reference,
            "status": self.status,
            "amount": self.amount,
            "currency": self.currency,
            "campaign_id": self.campaign_id.map(|u| u.to_string()),
            "donor_name": self.donor_name,
            "created_at": self.created_at.to_rfc3339(),
        })
    }
}

struct Proof {
    id: Uuid,
    payment_intent_id: Uuid,
    method: String,
    proof_text: Option<String>,
    proof_image_key: Option<String>,
    wallet_label: Option<String>,
    amount_irr: Option<i64>,
    fx_rate_irr: Option<i64>,
    fx_source: Option<String>,
    review_status: String,
    review_note: Option<String>,
    reviewed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl Proof {
    fn from_row(row: &DynRow) -> Result<Self, ApiError> {
        Ok(Self {
            id: row.try_get::<Uuid, _>("id")?,
            payment_intent_id: row.try_get::<Uuid, _>("payment_intent_id")?,
            method: row.try_get::<String, _>("method")?,
            proof_text: row.try_get::<Option<String>, _>("proof_text")?,
            proof_image_key: row.try_get::<Option<String>, _>("proof_image_key")?,
            wallet_label: row.try_get::<Option<String>, _>("wallet_label")?,
            amount_irr: row.try_get::<Option<i64>, _>("amount_irr")?,
            fx_rate_irr: row.try_get::<Option<i64>, _>("fx_rate_irr")?,
            fx_source: row.try_get::<Option<String>, _>("fx_source")?,
            review_status: row.try_get::<String, _>("review_status")?,
            review_note: row.try_get::<Option<String>, _>("review_note")?,
            reviewed_at: row.try_get::<Option<DateTime<Utc>>, _>("reviewed_at")?,
            created_at: row.try_get::<DateTime<Utc>, _>("created_at")?,
        })
    }

    fn admin_out(&self, intent: &Intent) -> Value {
        json!({
            "id": self.id.to_string(),
            "intent_id": intent.id.to_string(),
            "method": self.method,
            "status": self.review_status,
            "amount": intent.amount,
            "currency": intent.currency,
            "campaign_id": intent.campaign_id.map(|u| u.to_string()),
            "donor_name": intent.donor_name,
            "proof_text": self.proof_text,
            "has_proof_image": self.proof_image_key.is_some(),
            "wallet_label": self.wallet_label,
            "review_note": self.review_note,
            "reviewed_at": self.reviewed_at.map(|t| t.to_rfc3339()),
            "created_at": self.created_at.to_rfc3339(),
            "amount_irr": self.amount_irr,
            "fx_rate_irr": self.fx_rate_irr,
            "fx_source": self.fx_source,
        })
    }
}

async fn get_intent_by_id(db: &Db, id: Uuid) -> ApiResult<Option<Intent>> {
    let row = db
        .fetch_optional(
            "SELECT id, idempotency_key, amount, currency, campaign_id, donor_id, donor_name, status, gateway, authority FROM payment_intents WHERE id = ?",
            &[(id).into_arg()],
        )
        .await?;
    row.map(|r| Intent::from_row(&r)).transpose()
}

async fn get_intent_by_key(db: &Db, key: &str) -> ApiResult<Option<Intent>> {
    let row = db
        .fetch_optional(
            "SELECT id, idempotency_key, amount, currency, campaign_id, donor_id, donor_name, status, gateway, authority FROM payment_intents WHERE idempotency_key = ?",
            &[(key).into_arg()],
        )
        .await?;
    row.map(|r| Intent::from_row(&r)).transpose()
}

async fn get_intent_by_authority(db: &Db, authority: &str) -> ApiResult<Option<Intent>> {
    let row = db
        .fetch_optional(
            "SELECT id, idempotency_key, amount, currency, campaign_id, donor_id, donor_name, status, gateway, authority FROM payment_intents WHERE authority = ?",
            &[(authority).into_arg()],
        )
        .await?;
    row.map(|r| Intent::from_row(&r)).transpose()
}

async fn get_campaign_by_id(db: &Db, id: Uuid) -> ApiResult<Option<Campaign>> {
    let row = db
        .fetch_optional(
            "SELECT id, title, description, goal_amount, raised_amount, status, cover, created_at FROM donation_campaigns WHERE id = ?",
            &[(id).into_arg()],
        )
        .await?;
    row.map(|r| Campaign::from_row(&r)).transpose()
}

async fn get_donation_by_id(db: &Db, id: Uuid) -> ApiResult<Option<Donation>> {
    let row = db
        .fetch_optional(
            "SELECT id, payment_intent_id, gateway, gateway_reference, status, amount, currency, campaign_id, donor_name, created_at FROM donations WHERE id = ?",
            &[(id).into_arg()],
        )
        .await?;
    row.map(|r| Donation::from_row(&r)).transpose()
}

async fn get_donation_by_intent(db: &Db, intent_id: Uuid) -> ApiResult<Option<Donation>> {
    let row = db
        .fetch_optional(
            "SELECT id, payment_intent_id, gateway, gateway_reference, status, amount, currency, campaign_id, donor_name, created_at FROM donations WHERE payment_intent_id = ?",
            &[(intent_id).into_arg()],
        )
        .await?;
    row.map(|r| Donation::from_row(&r)).transpose()
}

async fn get_donation_by_ref(db: &Db, ref_id: &str) -> ApiResult<Option<Donation>> {
    let row = db
        .fetch_optional(
            "SELECT id, payment_intent_id, gateway, gateway_reference, status, amount, currency, campaign_id, donor_name, created_at FROM donations WHERE gateway_reference = ?",
            &[(ref_id).into_arg()],
        )
        .await?;
    row.map(|r| Donation::from_row(&r)).transpose()
}

async fn get_proof_by_id(db: &Db, id: Uuid) -> ApiResult<Option<Proof>> {
    let row = db
        .fetch_optional(
            "SELECT id, payment_intent_id, method, proof_text, proof_image_key, proof_image_mime, wallet_label, amount_irr, fx_rate_irr, fx_source, review_status, review_note, reviewed_at, created_at FROM manual_donation_proofs WHERE id = ?",
            &[(id).into_arg()],
        )
        .await?;
    row.map(|r| Proof::from_row(&r)).transpose()
}

async fn get_proof_image_key(
    db: &Db,
    id: Uuid,
) -> ApiResult<Option<(Option<String>, Option<String>)>> {
    let row = db
        .fetch_optional(
            "SELECT proof_image_key, proof_image_mime FROM manual_donation_proofs WHERE id = ?",
            &[(id).into_arg()],
        )
        .await?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some((
            r.try_get::<Option<String>, _>("proof_image_key")?,
            r.try_get::<Option<String>, _>("proof_image_mime")?,
        ))),
    }
}

// --- gateway config ----------------------------------------------------------

/// `(merchant_key, sandbox)` — the key always comes from
/// platform_settings; missing → 503 `payment_not_configured`.
async fn gateway_config(db: &Db, config: &Config, gateway_name: &str) -> ApiResult<(String, bool)> {
    let (key_name, sandbox_key) = match gateway_name {
        "zarinpal" => ("payment.zarinpal_key", "payment.zarinpal_sandbox"),
        "idpay" => ("payment.idpay_key", "payment.idpay_sandbox"),
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unknown_gateway",
                "درگاه پرداخت نامعتبر است",
            ));
        }
    };
    let key = get_setting(db, config, key_name).await?;
    let sandbox_v = get_typed_setting(db, config, sandbox_key, json!(true)).await?;
    let sandbox = typed_bool_or(&sandbox_v, true);
    let Some(key) = key.filter(|s| !s.is_empty()) else {
        return Err(ApiError::with_details(
            StatusCode::SERVICE_UNAVAILABLE,
            "payment_not_configured",
            "درگاه پرداخت پیکربندی نشده است",
            json!({ "gateway": gateway_name }),
        ));
    };
    Ok((key, sandbox))
}

async fn intent_sandbox(db: &Db, config: &Config, gateway_name: &str) -> ApiResult<bool> {
    let key = if gateway_name == "idpay" {
        "payment.idpay_sandbox"
    } else {
        "payment.zarinpal_sandbox"
    };
    let v = get_typed_setting(db, config, key, json!(true)).await?;
    Ok(typed_bool_or(&v, true))
}

fn pay_url_for(gateway: &str, authority: &str, sandbox: bool) -> String {
    if gateway == "idpay" {
        let (tx_id, _) = split_authority(authority);
        let base = if sandbox {
            IDPAY_SANDBOX_PAY
        } else {
            IDPAY_PAY
        };
        format!("{base}/{tx_id}")
    } else {
        let base = if sandbox {
            ZARINPAL_SANDBOX_PAY
        } else {
            ZARINPAL_PROD_PAY
        };
        format!("{base}/{authority}")
    }
}

/// `(tx_id, order_id)` — an authority without `:` → order_id is the
/// tx_id itself (Python `IdpayGateway.split_authority`).
fn split_authority(authority: &str) -> (&str, &str) {
    match authority.split_once(':') {
        Some((tx, order)) if !order.is_empty() => (tx, order),
        _ => (authority, authority),
    }
}

async fn intent_out(db: &Db, config: &Config, intent: &Intent) -> ApiResult<Value> {
    let Some(authority) = &intent.authority else {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "intent_broken",
            "نیت پرداخت ناقص است",
        ));
    };
    let sandbox = intent_sandbox(db, config, &intent.gateway).await?;
    Ok(json!({
        "intent_id": intent.id.to_string(),
        "authority": authority,
        "pay_url": pay_url_for(&intent.gateway, authority, sandbox),
        "status": intent.status,
        "amount": intent.amount,
        "currency": intent.currency,
    }))
}

// --- gateway HTTP (mirrors `finance/gateways.py`) ---------------------------

fn gateway_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(8))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

struct GatewayReq {
    authority: String,
}

struct GatewayVerify {
    ref_id: String,
}

fn first_n_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

async fn zarinpal_request(
    amount: i64,
    description: &str,
    callback_url: &str,
    merchant_key: &str,
    sandbox: bool,
) -> Result<GatewayReq, String> {
    let base = if sandbox {
        ZARINPAL_SANDBOX_BASE
    } else {
        ZARINPAL_PROD_BASE
    };
    let resp = gateway_client()
        .post(format!("{base}/pg/v4/payment/request.json"))
        .json(&json!({
            "merchant_id": merchant_key,
            "amount": amount,
            "description": first_n_chars(description, 255),
            "callback_url": callback_url,
            "currency": "IRR",
        }))
        .send()
        .await
        .map_err(|e| format!("zarinpal request failed: {e}"))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("zarinpal request failed: {e}"))?;
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("zarinpal request failed: {e}"))?;
    let data = body.get("data").cloned().unwrap_or(Value::Null);
    let code = data.get("code").and_then(|v| v.as_i64());
    let authority = data
        .get("authority")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    match (code, authority) {
        (Some(100), Some(authority)) => Ok(GatewayReq { authority }),
        _ => Err(format!(
            "zarinpal request rejected: code={code:?} errors={}",
            body.get("errors").cloned().unwrap_or(Value::Null)
        )),
    }
}

async fn zarinpal_verify(
    authority: &str,
    amount: i64,
    merchant_key: &str,
    sandbox: bool,
) -> Result<GatewayVerify, String> {
    let base = if sandbox {
        ZARINPAL_SANDBOX_BASE
    } else {
        ZARINPAL_PROD_BASE
    };
    let resp = gateway_client()
        .post(format!("{base}/pg/v4/payment/verify.json"))
        .json(&json!({
            "merchant_id": merchant_key,
            "amount": amount,
            "authority": authority,
        }))
        .send()
        .await
        .map_err(|e| format!("zarinpal verify failed: {e}"))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("zarinpal verify failed: {e}"))?;
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("zarinpal verify failed: {e}"))?;
    let data = body.get("data").cloned().unwrap_or(Value::Null);
    let code = data.get("code").and_then(|v| v.as_i64());
    let ref_id = data
        .get("ref_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    match (code, ref_id) {
        (Some(c), Some(ref_id)) if c == 100 || c == 101 => Ok(GatewayVerify { ref_id }),
        _ => Err(format!(
            "zarinpal verify rejected: code={code:?} errors={}",
            body.get("errors").cloned().unwrap_or(Value::Null)
        )),
    }
}

async fn idpay_request(
    amount: i64,
    description: &str,
    callback_url: &str,
    merchant_key: &str,
    sandbox: bool,
) -> Result<GatewayReq, String> {
    let order_id = Uuid::new_v4().simple().to_string();
    let resp = gateway_client()
        .post(format!("{IDPAY_BASE}/payment"))
        .header("X-API-KEY", merchant_key)
        .header("X-SANDBOX", if sandbox { "1" } else { "0" })
        .json(&json!({
            "order_id": order_id,
            "amount": amount,
            "desc": first_n_chars(description, 255),
            "callback": callback_url,
        }))
        .send()
        .await
        .map_err(|e| format!("idpay request failed: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let tx_id = body.get("id").and_then(|v| v.as_str()).map(str::to_string);
    match (status.as_u16(), tx_id) {
        // The gateway's `link` is ignored: Python's `_intent_out`
        // rebuilds the pay URL from the authority, like here.
        (201, Some(tx_id)) => {
            let authority = format!("{tx_id}:{order_id}");
            Ok(GatewayReq { authority })
        }
        _ => Err(format!(
            "idpay request rejected: status={} error_code={} error_message={}",
            status.as_u16(),
            body.get("error_code").cloned().unwrap_or(Value::Null),
            body.get("error_message").cloned().unwrap_or(Value::Null)
        )),
    }
}

async fn idpay_verify(
    authority: &str,
    merchant_key: &str,
    sandbox: bool,
) -> Result<GatewayVerify, String> {
    let (tx_id, order_id) = split_authority(authority);
    let resp = gateway_client()
        .post(format!("{IDPAY_BASE}/payment/verify"))
        .header("X-API-KEY", merchant_key)
        .header("X-SANDBOX", if sandbox { "1" } else { "0" })
        .json(&json!({ "id": tx_id, "order_id": order_id }))
        .send()
        .await
        .map_err(|e| format!("idpay verify failed: {e}"))?;
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let status = body.get("status").and_then(|v| v.as_i64());
    let track_id = body.get("track_id");
    match (status, track_id) {
        (Some(s), Some(t)) if (s == 100 || s == 101) && !t.is_null() => Ok(GatewayVerify {
            ref_id: match t {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            },
        }),
        _ => Err(format!(
            "idpay verify rejected: status={status:?} error_code={} error_message={}",
            body.get("error_code").cloned().unwrap_or(Value::Null),
            body.get("error_message").cloned().unwrap_or(Value::Null)
        )),
    }
}

async fn gateway_request(
    gateway: &str,
    sandbox: bool,
    amount: i64,
    description: &str,
    callback_url: &str,
    merchant_key: &str,
) -> Result<GatewayReq, String> {
    match gateway {
        "zarinpal" => {
            zarinpal_request(amount, description, callback_url, merchant_key, sandbox).await
        }
        "idpay" => idpay_request(amount, description, callback_url, merchant_key, sandbox).await,
        other => Err(format!("unknown gateway: {other}")),
    }
}

async fn gateway_verify(
    gateway: &str,
    sandbox: bool,
    authority: &str,
    amount: i64,
    merchant_key: &str,
) -> Result<GatewayVerify, String> {
    match gateway {
        "zarinpal" => zarinpal_verify(authority, amount, merchant_key, sandbox).await,
        "idpay" => idpay_verify(authority, merchant_key, sandbox).await,
        other => Err(format!("unknown gateway: {other}")),
    }
}

// --- FX (mirrors `finance/fx.py`) -------------------------------------------

/// Exact decimal helpers (Python `Decimal` with ROUND_HALF_UP). No
/// decimal crate is in the dependencies (ADR-065 class): prices are
/// parsed as (mantissa, scale) and multiplied by an integer rate.
fn parse_decimal(s: &str) -> Option<(i128, u32)> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int_part, frac_part) = match t.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (t, None),
    };
    let frac = frac_part.unwrap_or("");
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
        || (int_part.is_empty() && frac.is_empty())
    {
        return None;
    }
    let mut mantissa: i128 = 0;
    for c in int_part.chars().chain(frac.chars()) {
        mantissa = mantissa
            .checked_mul(10)?
            .checked_add(c as i128 - '0' as i128)?;
    }
    if mantissa == 0 {
        return Some((0, 0));
    }
    let scale = frac.len() as u32;
    Some((if neg { -mantissa } else { mantissa }, scale))
}

/// `int((price * mult).quantize(ROUND_HALF_UP))` — prices are
/// positive, so half-up is away from zero.
fn dec_mul_round_half_up(price: &str, mult: i64) -> Option<i64> {
    let (m, scale) = parse_decimal(price)?;
    let v = m.checked_mul(mult as i128)?;
    let d = 10i128.checked_pow(scale)?;
    let rounded = if v >= 0 {
        (v + d / 2) / d
    } else {
        -((-v + d / 2) / d)
    };
    i64::try_from(rounded).ok()
}

type WallexCache = Mutex<Option<(Value, Instant)>>;
static WALLEX_CACHE: LazyLock<WallexCache> = LazyLock::new(|| Mutex::new(None));
type RateCache = Mutex<HashMap<String, (i64, String, Instant)>>;
static RATE_CACHE: LazyLock<RateCache> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn fx_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .connect_timeout(Duration::from_secs(4))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn wallex_symbols() -> Option<Value> {
    {
        let guard = WALLEX_CACHE.lock().ok()?;
        if let Some((symbols, at)) = guard.as_ref() {
            if at.elapsed().as_secs_f64() < FX_CACHE_TTL_SECS {
                return Some(symbols.clone());
            }
        }
    }
    let res = fx_client()
        .get(WALLEX_MARKETS_URL)
        .header("User-Agent", FX_USER_AGENT)
        .send()
        .await
        .ok()?;
    let res = res.error_for_status().ok()?;
    let data: Value = res.json().await.ok()?;
    let symbols = data
        .get("result")
        .and_then(|r| r.get("symbols"))
        .cloned()
        .unwrap_or(Value::Null);
    if !matches!(&symbols, Value::Object(o) if !o.is_empty()) {
        return None;
    }
    if let Ok(mut guard) = WALLEX_CACHE.lock() {
        *guard = Some((symbols.clone(), Instant::now()));
    }
    Some(symbols)
}

/// Wallex `lastPrice` as its raw decimal text, or `None` when absent
/// / `-` / non-positive (Python `_price`).
fn wallex_last_price(symbols: &Value, symbol: &str) -> Option<String> {
    let raw = symbols.get(symbol)?.get("stats")?.get("lastPrice")?;
    let text = match raw {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let (m, _) = parse_decimal(&text)?;
    if m <= 0 {
        return None;
    }
    Some(text)
}

async fn usdt_rate_irr() -> Option<(i64, &'static str)> {
    let symbols = wallex_symbols().await?;
    let tmn = wallex_last_price(&symbols, "USDTTMN")?;
    let rate = dec_mul_round_half_up(&tmn, 10)?;
    Some((rate, "wallex"))
}

async fn ton_rate_irr() -> Option<(i64, &'static str)> {
    let (usdt_irr, _) = usdt_rate_irr().await?;
    if let Some(symbols) = wallex_symbols().await {
        if let Some(ton_usdt) = wallex_last_price(&symbols, "TONUSDT") {
            if let Some(rate) = dec_mul_round_half_up(&ton_usdt, usdt_irr) {
                return Some((rate, "wallex"));
            }
        }
    }
    let res = fx_client()
        .get(COINGECKO_TON_URL)
        .header("User-Agent", FX_USER_AGENT)
        .send()
        .await
        .ok()?;
    let res = res.error_for_status().ok()?;
    let data: Value = res.json().await.ok()?;
    let raw = data.get("toncoin")?.get("usd")?;
    let text = match raw {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let (m, _) = parse_decimal(&text)?;
    if m <= 0 {
        return None;
    }
    let rate = dec_mul_round_half_up(&text, usdt_irr)?;
    Some((rate, "coingecko"))
}

/// `(rate_irr, source)` — `FxUnavailable` in Python becomes `None`
/// here; the caller maps it to 502 `fx_unavailable` or 422
/// `fx_rate_required`.
async fn get_rate_irr(currency: &str) -> Option<(i64, String)> {
    let cur = currency.to_uppercase();
    if cur == "IRR" {
        return Some((1, "fixed".to_string()));
    }
    {
        let guard = RATE_CACHE.lock().ok()?;
        if let Some((rate, source, at)) = guard.get(&cur) {
            if at.elapsed().as_secs_f64() < FX_CACHE_TTL_SECS {
                return Some((*rate, source.clone()));
            }
        }
    }
    let rate: Option<(i64, &'static str)> = match cur.as_str() {
        "USDT" | "USD" => usdt_rate_irr().await,
        "TON" => ton_rate_irr().await,
        _ => None,
    };
    let (rate, source) = rate?;
    if let Ok(mut guard) = RATE_CACHE.lock() {
        guard.insert(cur, (rate, source.to_string(), Instant::now()));
    }
    Some((rate, source.to_string()))
}

// --- receipt-image scan gate (ADR-051, FIN-02) -------------------------------

/// Hash-check + EICAR gate for receipt uploads, mirroring the Wave 6
/// library `scan_upload`: SHA-256 against the admin-managed
/// `library.blocked_hashes` setting plus the EICAR signature. A hit is
/// 422 `file_infected` with audit row
/// `finance.upload_rejected_infected`.
/// Audit context for the receipt scan gate (kept as one argument so the
/// scan helper stays under clippy's argument-count limit).
struct ScanAudit<'a> {
    actor_id: Option<Uuid>,
    actor_type: &'a str,
    ip: Option<&'a str>,
    user_agent: Option<&'a str>,
}

async fn scan_receipt_upload(
    db: &Db,
    config: &Config,
    audit: ScanAudit<'_>,
    data: &[u8],
    filename: Option<&str>,
) -> ApiResult<()> {
    let mut signature: Option<&str> = None;
    if data.len() >= EICAR.len() && data.windows(EICAR.len()).any(|w| w == EICAR) {
        signature = Some("Eicar-Test-Signature");
    } else if let Some(raw) = get_setting(db, config, "library.blocked_hashes").await? {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&raw) {
            let hex = sha256_hex(data);
            if items.iter().any(|v| v.as_str() == Some(hex.as_str())) {
                signature = Some("blocked-hash");
            }
        }
    }
    if let Some(sig) = signature {
        log_audit(
            db,
            audit.actor_id,
            audit.actor_type,
            "finance.upload_rejected_infected",
            "manual_donation_proof",
            None,
            None,
            Some(json!({ "filename": filename, "signature": sig })),
            audit.ip,
            audit.user_agent,
        )
        .await?;
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "file_infected",
            "فایل به ویروس آلوده است و رد شد",
            json!({ "signature": sig }),
        ));
    }
    Ok(())
}

// --- finalize (the idempotency heart of callbacks + webhooks) ---------------

/// Mirrors Python `_finalize_success`: donation + ledger + campaign
/// credit + audit + `donation.completed` event. Re-entrant — if the
/// intent already succeeded, the existing donation comes back and no
/// second row is ever created.
#[allow(clippy::too_many_arguments)]
async fn finalize_success(
    db: &Db,
    intent: &Intent,
    ref_id: &str,
    actor_type: &str,
    actor_id: Option<Uuid>,
    ip: Option<&str>,
    ua: Option<&str>,
    campaign_credit_irr: Option<i64>,
) -> ApiResult<Donation> {
    if let Some(existing) = get_donation_by_intent(db, intent.id).await? {
        return Ok(existing);
    }
    if intent.status == "succeeded" {
        if let Some(d) = get_donation_by_ref(db, ref_id).await? {
            return Ok(d);
        }
        return Err(ApiError::internal());
    }
    let now = utc_now();
    db.execute(
        "UPDATE payment_intents SET status = 'succeeded', updated_at = ? WHERE id = ?",
        &[(now).into_arg(), (intent.id).into_arg()],
    )
    .await?;
    let donation_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO donations (id, donor_id, payment_intent_id, campaign_id, gateway, gateway_reference, status, amount, currency, donor_name, created_at) VALUES (?, ?, ?, ?, ?, ?, 'succeeded', ?, ?, ?, ?)",
        &[
            (donation_id).into_arg(),
            (intent.donor_id).into_arg(),
            (intent.id).into_arg(),
            (intent.campaign_id).into_arg(),
            (&intent.gateway).into_arg(),
            (ref_id).into_arg(),
            (intent.amount).into_arg(),
            (&intent.currency).into_arg(),
            (&intent.donor_name).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    db.execute(
        "INSERT INTO ledger_entries (id, entry_type, amount, currency, reference, donation_id, created_at) VALUES (?, 'donation_in', ?, ?, ?, ?, ?)",
        &[
            (Uuid::new_v4()).into_arg(),
            (intent.amount).into_arg(),
            (&intent.currency).into_arg(),
            (format!("donation:{donation_id}")).into_arg(),
            (donation_id).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    // Campaign totals are IRR. The gateway path only counts IRR; the
    // manual-approve path passes the converted credit explicitly.
    let credit = match campaign_credit_irr {
        Some(c) => Some(c),
        None if intent.currency == "IRR" => Some(intent.amount),
        None => None,
    };
    if let (Some(credit), Some(campaign_id)) = (credit.filter(|c| *c != 0), intent.campaign_id) {
        if let Some(campaign) = get_campaign_by_id(db, campaign_id).await? {
            db.execute(
                "UPDATE donation_campaigns SET raised_amount = ?, updated_at = ? WHERE id = ?",
                &[
                    (campaign.raised_amount + credit).into_arg(),
                    (now).into_arg(),
                    (campaign_id).into_arg(),
                ],
            )
            .await?;
        }
    }
    log_audit(
        db,
        actor_id,
        actor_type,
        "finance.donation_succeeded",
        "donation",
        Some(&donation_id.to_string()),
        None,
        Some(json!({
            "amount": intent.amount,
            "currency": intent.currency,
            "gateway": intent.gateway,
            "gateway_reference": ref_id,
            "campaign_id": intent.campaign_id.map(|u| u.to_string()),
        })),
        ip,
        ua,
    )
    .await?;
    // The event idempotency key derives from the command key
    // (`donate:<key>`), not the event id — unlike the shared
    // `emit_event` helper, which inserts NULL, this writes it
    // directly (FIN-03).
    db.execute(
        "INSERT INTO domain_events (event_id, idempotency_key, aggregate_type, aggregate_id, event_type, payload, schema_version, source_client, created_at, retry_count) VALUES (?, ?, 'donation', ?, 'donation.completed', ?, 1, 'api', ?, 0)",
        &[
            (Uuid::new_v4()).into_arg(),
            (format!("donate:{}", intent.idempotency_key)).into_arg(),
            (donation_id.to_string()).into_arg(),
            (json!({
                "donation_id": donation_id.to_string(),
                "intent_id": intent.id.to_string(),
                "amount": intent.amount,
                "currency": intent.currency,
                "gateway": intent.gateway,
                "gateway_reference": ref_id,
                "campaign_id": intent.campaign_id.map(|u| u.to_string()),
            }))
            .into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    get_donation_by_id(db, donation_id)
        .await?
        .ok_or_else(ApiError::internal)
}

async fn mark_intent_failed(
    db: &Db,
    intent: &Intent,
    gateway_status: &str,
    ip: Option<&str>,
    ua: Option<&str>,
) -> ApiResult<()> {
    db.execute(
        "UPDATE payment_intents SET status = 'failed', updated_at = ? WHERE id = ?",
        &[(utc_now()).into_arg(), (intent.id).into_arg()],
    )
    .await?;
    log_audit(
        db,
        None,
        "system",
        "finance.intent_failed",
        "payment_intent",
        Some(&intent.id.to_string()),
        None,
        Some(json!({ "status": "failed", "gateway_status": gateway_status })),
        ip,
        ua,
    )
    .await?;
    Ok(())
}

// --- donation methods --------------------------------------------------------

fn method_title(method: &str) -> Value {
    match method {
        "online" => {
            json!({"fa": "پرداخت آنلاین (درگاه)", "ar": "الدفع عبر الإنترنت (البوابة)", "en": "Online payment (gateway)"})
        }
        "card" => {
            json!({"fa": "کارت‌به‌کارت", "ar": "تحويل من بطاقة إلى بطاقة", "en": "Card-to-card transfer"})
        }
        "crypto" => json!({"fa": "ارز دیجیتال", "ar": "العملات الرقمية", "en": "Cryptocurrency"}),
        "paypal" => {
            json!({"fa": "حمایت خارجی (PayPal)", "ar": "الدعم الدولي (PayPal)", "en": "International support (PayPal)"})
        }
        _ => json!({"fa": method, "ar": method, "en": method}),
    }
}

struct WalletCfg {
    currency: String,
    network: String,
    address: String,
}

struct MethodCfg {
    currency: Option<String>,
    card_number: Option<String>,
    card_holder: Option<String>,
    wallets: Vec<WalletCfg>,
    paypal_account: Option<String>,
    instructions: Option<Value>,
}

async fn setting_flag(db: &Db, config: &Config, key: &str, default: bool) -> ApiResult<bool> {
    let v = get_typed_setting(db, config, key, json!(default)).await?;
    Ok(if v.is_null() { default } else { py_bool(&v) })
}

async fn localized_setting(db: &Db, config: &Config, key: &str) -> ApiResult<Option<Value>> {
    let v = get_typed_setting(db, config, key, Value::Null).await?;
    let Some(map) = v.as_object() else {
        return Ok(None);
    };
    if map.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!({
        "fa": map.get("fa").and_then(|v| v.as_str()).unwrap_or(""),
        "ar": map.get("ar").and_then(|v| v.as_str()).unwrap_or(""),
        "en": map.get("en").and_then(|v| v.as_str()).unwrap_or(""),
    })))
}

async fn crypto_wallets(db: &Db, config: &Config) -> ApiResult<Vec<WalletCfg>> {
    let v = get_typed_setting(db, config, "donation.crypto_wallets", Value::Null).await?;
    let Some(items) = v.as_array() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in items {
        let Some(map) = item.as_object() else {
            continue;
        };
        let currency = map
            .get("currency")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let address = map
            .get("address")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let network = map
            .get("network")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !currency.is_empty() && !address.is_empty() {
            out.push(WalletCfg {
                currency,
                network,
                address,
            });
        }
    }
    Ok(out)
}

/// Manual-method config — `None` means the method is unavailable
/// (off or incomplete) and must not be listed or accept submissions.
async fn manual_method_config(
    db: &Db,
    config: &Config,
    method: &str,
) -> ApiResult<Option<MethodCfg>> {
    match method {
        "card" => {
            if !setting_flag(db, config, "donation.card_enabled", false).await? {
                return Ok(None);
            }
            let number = get_setting(db, config, "donation.card_number").await?;
            let Some(number) = number
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            else {
                return Ok(None);
            };
            Ok(Some(MethodCfg {
                currency: Some("IRR".to_string()),
                card_number: Some(number),
                card_holder: get_setting(db, config, "donation.card_holder").await?,
                wallets: Vec::new(),
                paypal_account: None,
                instructions: localized_setting(db, config, "donation.card_instructions").await?,
            }))
        }
        "crypto" => {
            if !setting_flag(db, config, "donation.crypto_enabled", false).await? {
                return Ok(None);
            }
            let wallets = crypto_wallets(db, config).await?;
            if wallets.is_empty() {
                return Ok(None);
            }
            Ok(Some(MethodCfg {
                currency: None,
                card_number: None,
                card_holder: None,
                wallets,
                paypal_account: None,
                instructions: localized_setting(db, config, "donation.crypto_instructions").await?,
            }))
        }
        "paypal" => {
            if !setting_flag(db, config, "donation.paypal_enabled", false).await? {
                return Ok(None);
            }
            let account = get_setting(db, config, "donation.paypal_account").await?;
            let Some(account) = account
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            else {
                return Ok(None);
            };
            Ok(Some(MethodCfg {
                currency: Some("USD".to_string()),
                card_number: None,
                card_holder: None,
                wallets: Vec::new(),
                paypal_account: Some(account),
                instructions: localized_setting(db, config, "donation.paypal_instructions").await?,
            }))
        }
        _ => Ok(None),
    }
}

fn sniff_receipt_image(data: &[u8]) -> Option<(&'static str, &'static str)> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some(("image/jpeg", "jpg"))
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some(("image/webp", "webp"))
    } else {
        None
    }
}

// --- multipart ---------------------------------------------------------------

async fn read_field_capped(
    mut field: axum::extract::multipart::Field<'_>,
    cap: usize,
) -> ApiResult<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    let mut overflow = false;
    while let Some(chunk) = field.chunk().await.map_err(|_| ApiError::internal())? {
        if overflow {
            continue;
        }
        if bytes.len() + chunk.len() > cap {
            overflow = true;
        } else {
            bytes.extend_from_slice(&chunk);
        }
    }
    Ok((bytes, overflow))
}

struct ManualForm {
    method: Option<String>,
    amount: Option<String>,
    campaign_id: Option<String>,
    donor_name: Option<String>,
    proof_text: Option<String>,
    wallet_currency: Option<String>,
    wallet_network: Option<String>,
    receipt: Option<(Vec<u8>, bool)>,
}

async fn collect_manual_form(mut mp: Multipart) -> ApiResult<ManualForm> {
    let mut out = ManualForm {
        method: None,
        amount: None,
        campaign_id: None,
        donor_name: None,
        proof_text: None,
        wallet_currency: None,
        wallet_network: None,
        receipt: None,
    };
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|_| ApiError::validation(vec![json!({ "msg": "invalid multipart body" })]))?
    {
        let name = field.name().map(str::to_string);
        match name.as_deref() {
            Some("receipt") => {
                if out.receipt.is_none() {
                    let (bytes, overflow) =
                        read_field_capped(field, RECEIPT_IMAGE_MAX_BYTES).await?;
                    out.receipt = Some((bytes, overflow));
                }
            }
            Some(name) => {
                let text = field.text().await.map_err(|_| {
                    ApiError::validation(vec![json!({ "msg": "invalid multipart body" })])
                })?;
                match name {
                    "method" => {
                        if out.method.is_none() {
                            out.method = Some(text);
                        }
                    }
                    "amount" => {
                        if out.amount.is_none() {
                            out.amount = Some(text);
                        }
                    }
                    "campaign_id" => {
                        if out.campaign_id.is_none() {
                            out.campaign_id = Some(text);
                        }
                    }
                    "donor_name" if out.donor_name.is_none() => {
                        out.donor_name = Some(text);
                    }
                    "proof_text" if out.proof_text.is_none() => {
                        out.proof_text = Some(text);
                    }
                    "wallet_currency" if out.wallet_currency.is_none() => {
                        out.wallet_currency = Some(text);
                    }
                    "wallet_network" if out.wallet_network.is_none() => {
                        out.wallet_network = Some(text);
                    }
                    _ => {}
                }
            }
            None => {
                let mut field = field;
                while let Some(_chunk) = field.chunk().await.map_err(|_| ApiError::internal())? {}
            }
        }
    }
    Ok(out)
}

/// Pydantic lax `int` on a form string (whitespace tolerated,
/// integral floats accepted, bool rejected) — via `validate::as_int`.
fn parse_form_int(raw: &str) -> Option<i64> {
    as_int(&Value::String(raw.to_string()))
}

/// Python `int(value)` on a webhook JSON value: integral numbers and
/// digit strings, truncated floats; anything else is a `ValueError`
/// (the caller maps it to 401 `webhook_verification_failed`).
fn webhook_amount(value: Option<&Value>) -> Option<i64> {
    let v = value?;
    if v.is_null() {
        return Some(0);
    }
    if let Value::Bool(b) = v {
        return Some(i64::from(*b));
    }
    if let Value::String(s) = v {
        if s.trim().is_empty() {
            return Some(0);
        }
    }
    as_int(v)
}

// --- public: donate ----------------------------------------------------------

/// `POST /api/v1/finance/donate` — create (or replay) a payment
/// intent. New intent → 201; replayed key → 200 with the same intent
/// and no new gateway call.
pub async fn donate(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let amount = match parsed.get("amount").and_then(as_int) {
        Some(n) if n > 0 => n,
        _ => return Err(validation_err("amount", "must be a positive integer")),
    };
    let currency = match parsed.get("currency") {
        None | Some(Value::Null) => "IRR".to_string(),
        Some(Value::String(s)) if s == "IRR" => s.clone(),
        Some(_) => return Err(validation_err("currency", "must be IRR")),
    };
    let gateway = match parsed.get("gateway") {
        None | Some(Value::Null) => "zarinpal".to_string(),
        Some(Value::String(s)) if s == "zarinpal" || s == "idpay" => s.clone(),
        Some(_) => return Err(validation_err("gateway", "must be zarinpal or idpay")),
    };
    let donor_name = opt_str(&parsed, "donor_name", 0, 256)?;
    let callback_url = opt_str(&parsed, "callback_url", 0, 1024)?;
    let campaign_id = opt_uuid(&parsed, "campaign_id")?;
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let Some(idempotency_key) = idempotency_key else {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "missing_idempotency_key",
            "هدر Idempotency-Key اجباری است",
        ));
    };
    if amount > MAX_DONATE_IRR {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "amount_too_large",
            "مبلغ از سقف مجاز بیشتر است",
        ));
    }
    let db = state.db_or_500()?;
    let user = optional_user(&state, &headers).await;
    let (ip, ua) = req_meta(&state, &headers, peer);

    if let Some(existing) = get_intent_by_key(db, &idempotency_key).await? {
        let out = intent_out(db, &state.config, &existing).await?;
        return Ok((StatusCode::OK, Json(out)));
    }
    if !setting_flag(db, &state.config, "donation.online_enabled", true).await? {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "donation_method_disabled",
            "پرداخت آنلاین در حال حاضر غیرفعال است",
        ));
    }
    if let Some(cid) = campaign_id {
        let campaign = get_campaign_by_id(db, cid).await?;
        if campaign.is_none() || campaign.map(|c| c.status) != Some("active".to_string()) {
            return Err(ApiError::not_found(
                "campaign_not_found",
                "کمپین فعال یافت نشد",
            ));
        }
    }
    let (merchant_key, sandbox) = gateway_config(db, &state.config, &gateway).await?;
    let callback_url = match callback_url.filter(|s| !s.is_empty()) {
        Some(u) => Some(u),
        None => get_setting(db, &state.config, "payment.callback_url")
            .await?
            .filter(|s| !s.is_empty()),
    };
    let Some(callback_url) = callback_url else {
        return Err(ApiError::with_details(
            StatusCode::SERVICE_UNAVAILABLE,
            "payment_not_configured",
            "آدرس بازگشت درگاه پیکربندی نشده است",
            json!({ "gateway": gateway }),
        ));
    };

    let intent_id = Uuid::new_v4();
    let now = utc_now();
    let user_id = user.as_ref().map(|c| c.user.id);
    let actor_type = if user.is_some() { "user" } else { "anonymous" };
    let inserted = db
        .execute(
            "INSERT INTO payment_intents (id, idempotency_key, amount, currency, campaign_id, donor_id, donor_name, status, gateway, authority, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?, NULL, ?, NULL)",
            &[
                (intent_id).into_arg(),
                (idempotency_key.clone()).into_arg(),
                (amount).into_arg(),
                (currency.clone()).into_arg(),
                (campaign_id).into_arg(),
                (user_id).into_arg(),
                (donor_name).into_arg(),
                (gateway.clone()).into_arg(),
                (now).into_arg(),
            ],
        )
        .await;
    if let Err(e) = inserted {
        // Simultaneous race on the same key — return the winner.
        if let Some(winner) = get_intent_by_key(db, &idempotency_key).await? {
            let out = intent_out(db, &state.config, &winner).await?;
            return Ok((StatusCode::OK, Json(out)));
        }
        return Err(e);
    }
    let req = match gateway_request(
        &gateway,
        sandbox,
        amount,
        "Donation to Maturidi Academy",
        &callback_url,
        &merchant_key,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            db.execute(
                "UPDATE payment_intents SET status = 'failed', updated_at = ? WHERE id = ?",
                &[(utc_now()).into_arg(), (intent_id).into_arg()],
            )
            .await?;
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "gateway_request_failed",
                "درخواست پرداخت به درگاه ناموفق بود",
            ));
        }
    };
    db.execute(
        "UPDATE payment_intents SET authority = ?, updated_at = ? WHERE id = ?",
        &[
            (req.authority).into_arg(),
            (utc_now()).into_arg(),
            (intent_id).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        user_id,
        actor_type,
        "finance.intent_created",
        "payment_intent",
        Some(&intent_id.to_string()),
        None,
        Some(json!({
            "amount": amount,
            "currency": currency,
            "gateway": gateway,
            "campaign_id": campaign_id.map(|u| u.to_string()),
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let intent = get_intent_by_id(db, intent_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    let out = intent_out(db, &state.config, &intent).await?;
    Ok((StatusCode::CREATED, Json(out)))
}

// --- public: callbacks + webhook ---------------------------------------------

/// `GET /api/v1/finance/callback` — ZarinPal redirect destination
/// (`?Authority=&Status=OK`). Re-entrant.
pub async fn payment_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let Some(authority) = params.get("Authority").cloned() else {
        return Err(validation_err("Authority", "field is required"));
    };
    let status = params.get("Status").cloned().unwrap_or_default();
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let Some(intent) = get_intent_by_authority(db, &authority).await? else {
        return Err(ApiError::not_found(
            "intent_not_found",
            "نیت پرداختی با این شناسه یافت نشد",
        ));
    };
    if intent.status == "succeeded" {
        let donation = get_donation_by_intent(db, intent.id).await?;
        return Ok(Json(json!({
            "intent_id": intent.id.to_string(),
            "status": "succeeded",
            "donation_id": donation.map(|d| d.id.to_string()),
        })));
    }
    if status.to_uppercase() != "OK" {
        if intent.status == "pending" {
            mark_intent_failed(db, &intent, &status, ip.as_deref(), ua.as_deref()).await?;
        }
        // Re-read the (possibly updated) status.
        let intent = get_intent_by_id(db, intent.id)
            .await?
            .ok_or_else(ApiError::internal)?;
        return Ok(Json(json!({
            "intent_id": intent.id.to_string(),
            "status": intent.status,
            "donation_id": Value::Null,
        })));
    }
    if intent.status == "failed" || intent.status == "cancelled" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "intent_closed",
            "این نیت پرداخت بسته شده است",
        ));
    }
    let (merchant_key, sandbox) = gateway_config(db, &state.config, &intent.gateway).await?;
    let result = gateway_verify(
        &intent.gateway,
        sandbox,
        &authority,
        intent.amount,
        &merchant_key,
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "payment_verification_failed",
            "تأیید پرداخت در درگاه ناموفق بود",
        )
    })?;
    let donation = finalize_success(
        db,
        &intent,
        &result.ref_id,
        "system",
        None,
        ip.as_deref(),
        ua.as_deref(),
        None,
    )
    .await?;
    Ok(Json(json!({
        "intent_id": intent.id.to_string(),
        "status": "succeeded",
        "donation_id": donation.id.to_string(),
        "gateway_reference": donation.gateway_reference,
    })))
}

/// `POST /api/v1/finance/callback/idpay` — IDPay browser return
/// (form POST: id / order_id / status / track_id). Authenticity is
/// proven only by the server-side verify, never by the form fields.
pub async fn idpay_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Form(form): Form<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let tx_id = form.get("id").cloned().unwrap_or_default();
    let order_id = form.get("order_id").cloned().unwrap_or_default();
    let status = form.get("status").cloned().unwrap_or_default();
    if tx_id.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_callback_payload",
            "فیلد id اجباری است",
        ));
    }
    let authority = if order_id.is_empty() {
        tx_id.clone()
    } else {
        format!("{tx_id}:{order_id}")
    };
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let mut intent = get_intent_by_authority(db, &authority).await?;
    if intent.is_none() && !order_id.is_empty() {
        // Older records may hold the bare tx_id as authority.
        intent = get_intent_by_authority(db, &tx_id).await?;
    }
    let Some(intent) = intent else {
        return Err(ApiError::not_found(
            "intent_not_found",
            "نیت پرداختی با این شناسه یافت نشد",
        ));
    };
    if intent.status == "succeeded" {
        let donation = get_donation_by_intent(db, intent.id).await?;
        return Ok(Json(json!({
            "intent_id": intent.id.to_string(),
            "status": "succeeded",
            "donation_id": donation.map(|d| d.id.to_string()),
        })));
    }
    if !matches!(status.as_str(), "1" | "10" | "100" | "101") {
        if intent.status == "pending" {
            mark_intent_failed(db, &intent, &status, ip.as_deref(), ua.as_deref()).await?;
        }
        let intent = get_intent_by_id(db, intent.id)
            .await?
            .ok_or_else(ApiError::internal)?;
        return Ok(Json(json!({
            "intent_id": intent.id.to_string(),
            "status": intent.status,
            "donation_id": Value::Null,
        })));
    }
    if intent.status == "failed" || intent.status == "cancelled" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "intent_closed",
            "این نیت پرداخت بسته شده است",
        ));
    }
    let (merchant_key, sandbox) = gateway_config(db, &state.config, "idpay").await?;
    let verify_authority = intent.authority.clone().unwrap_or(authority);
    let result = gateway_verify(
        "idpay",
        sandbox,
        &verify_authority,
        intent.amount,
        &merchant_key,
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "payment_verification_failed",
            "تأیید پرداخت در درگاه ناموفق بود",
        )
    })?;
    let donation = finalize_success(
        db,
        &intent,
        &result.ref_id,
        "system",
        None,
        ip.as_deref(),
        ua.as_deref(),
        None,
    )
    .await?;
    Ok(Json(json!({
        "intent_id": intent.id.to_string(),
        "status": "succeeded",
        "donation_id": donation.id.to_string(),
        "gateway_reference": donation.gateway_reference,
    })))
}

async fn log_webhook_rejected(
    db: &Db,
    gateway_name: &str,
    raw_body: &[u8],
    reason: &str,
) -> ApiResult<()> {
    // The rejected hash is salted so a legitimate retry with the same
    // payload does not collide with it (dedupe only looks at
    // processed/duplicate rows).
    let salt = sha256_hex(format!("{}{}", utc_now().to_rfc3339(), reason).as_bytes());
    let salt = &salt[..12];
    db.execute(
        "INSERT INTO payment_webhook_log (id, gateway, payload_hash, status, processed_at) VALUES (?, ?, ?, 'rejected', ?)",
        &[
            (Uuid::new_v4()).into_arg(),
            (gateway_name).into_arg(),
            (format!("{}:rej:{}", sha256_hex(raw_body), salt)).into_arg(),
            (utc_now()).into_arg(),
        ],
    )
    .await?;
    Ok(())
}

fn webhook_signature_header(headers: &HeaderMap) -> Option<String> {
    for key in ["x-webhook-signature", "x-signature", "signature"] {
        if let Some(v) = headers.get(key).and_then(|v| v.to_str().ok()) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// HMAC-SHA256 over the raw body (the default for signed webhooks;
/// ZarinPal and IDPay override it to "verify server-side", so this
/// branch is unreachable for them — kept for faithfulness).
fn verify_webhook_hmac(raw_body: &[u8], signature: &str, secret: &str) -> bool {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(raw_body);
    let expected_hex = signature.trim();
    let Ok(expected) = hex_decode(expected_hex) else {
        return false;
    };
    mac.verify_slice(&expected).is_ok()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let digit = |c: u8| -> Result<u8, ()> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(()),
        }
    };
    for pair in bytes.chunks(2) {
        out.push((digit(pair[0])? << 4) | digit(pair[1])?);
    }
    Ok(out)
}

/// `POST /api/v1/finance/webhook/{gateway}` — idempotent webhook
/// processing: payload-hash dedupe, server-side verify for
/// ZarinPal/IDPay, shared `_finalize_success` for the final booking.
pub async fn payment_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(gateway_name): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let (ip, _) = req_meta(&state, &headers, peer);
    let raw_body = body.to_vec();
    let payload_hash = sha256_hex(&raw_body);
    let seen = db
        .fetch_optional(
            "SELECT id FROM payment_webhook_log WHERE payload_hash = ? AND status IN ('processed', 'duplicate')",
            &[(payload_hash.clone()).into_arg()],
        )
        .await?;
    if seen.is_some() {
        let dup_suffix = &sha256_hex(utc_now().to_rfc3339().as_bytes())[..8];
        db.execute(
            "INSERT INTO payment_webhook_log (id, gateway, payload_hash, status, processed_at) VALUES (?, ?, ?, 'duplicate', ?)",
            &[
                (Uuid::new_v4()).into_arg(),
                (gateway_name.clone()).into_arg(),
                (format!("{payload_hash}:dup:{dup_suffix}")).into_arg(),
                (utc_now()).into_arg(),
            ],
        )
        .await?;
        return Ok(Json(
            json!({ "already_processed": true, "status": "duplicate" }),
        ));
    }
    let payload: Value = match String::from_utf8(raw_body.clone()) {
        Ok(text) => match serde_json::from_str::<Value>(if text.is_empty() { "{}" } else { &text })
        {
            Ok(v) => v,
            Err(_) => {
                log_webhook_rejected(db, &gateway_name, &raw_body, "bad_json").await?;
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "bad_webhook_payload",
                    "بدنهٔ وبهوک نامعتبر است",
                ));
            }
        },
        Err(_) => {
            log_webhook_rejected(db, &gateway_name, &raw_body, "bad_json").await?;
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "bad_webhook_payload",
                "بدنهٔ وبهوک نامعتبر است",
            ));
        }
    };
    if !payload.is_object() {
        // Python calls `payload.get(...)` on the parsed value — a
        // non-object JSON body dies as a 500 there.
        return Err(ApiError::internal());
    }
    let (merchant_key, sandbox) = gateway_config(db, &state.config, &gateway_name).await?;
    let (authority, ref_id): (String, String) =
        if gateway_name == "zarinpal" || gateway_name == "idpay" {
            let authority: Option<String> = if gateway_name == "zarinpal" {
                payload
                    .get("authority")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            } else {
                let tx = payload.get("id").and_then(|v| v.as_str());
                let order = payload.get("order_id").and_then(|v| v.as_str());
                match (tx, order) {
                    (Some(t), Some(o)) => Some(format!("{t}:{o}")),
                    (Some(t), None) => Some(t.to_string()),
                    _ => None,
                }
            };
            let Some(authority) = authority.filter(|s| !s.is_empty()) else {
                log_webhook_rejected(db, &gateway_name, &raw_body, "missing_authority").await?;
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "bad_webhook_payload",
                    "فیلد authority اجباری است",
                ));
            };
            let amount = if gateway_name == "zarinpal" {
                match webhook_amount(payload.get("amount")) {
                    Some(n) => n,
                    None => {
                        log_webhook_rejected(db, &gateway_name, &raw_body, "verify_failed").await?;
                        return Err(ApiError::new(
                            StatusCode::UNAUTHORIZED,
                            "webhook_verification_failed",
                            "تأیید وبهوک ناموفق بود",
                        ));
                    }
                }
            } else {
                0
            };
            let result =
                gateway_verify(&gateway_name, sandbox, &authority, amount, &merchant_key).await;
            match result {
                Ok(r) => (authority, r.ref_id),
                Err(_) => {
                    log_webhook_rejected(db, &gateway_name, &raw_body, "verify_failed").await?;
                    return Err(ApiError::new(
                        StatusCode::UNAUTHORIZED,
                        "webhook_verification_failed",
                        "تأیید وبهوک ناموفق بود",
                    ));
                }
            }
        } else {
            let webhook_secret = get_setting(db, &state.config, "payment.webhook_secret").await?;
            let signature = webhook_signature_header(&headers);
            let ok = match (signature, webhook_secret) {
                (Some(sig), Some(secret)) => verify_webhook_hmac(&raw_body, &sig, &secret),
                _ => false,
            };
            if !ok {
                log_webhook_rejected(db, &gateway_name, &raw_body, "bad_signature").await?;
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "bad_webhook_signature",
                    "امضای وبهوک نامعتبر است",
                ));
            }
            let authority = payload
                .get("authority")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let ref_id = payload
                .get("gateway_reference")
                .or_else(|| payload.get("ref_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            match (authority, ref_id) {
                (Some(a), Some(r)) if !a.is_empty() && !r.is_empty() => (a, r),
                _ => {
                    log_webhook_rejected(db, &gateway_name, &raw_body, "missing_fields").await?;
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "bad_webhook_payload",
                        "فیلدهای وبهوک ناقص است",
                    ));
                }
            }
        };
    let Some(intent) = get_intent_by_authority(db, &authority).await? else {
        log_webhook_rejected(db, &gateway_name, &raw_body, "intent_not_found").await?;
        return Err(ApiError::not_found(
            "intent_not_found",
            "نیت پرداختی با این شناسه یافت نشد",
        ));
    };
    let donation = finalize_success(
        db,
        &intent,
        &ref_id,
        "system",
        None,
        ip.as_deref(),
        None,
        None,
    )
    .await?;
    db.execute(
        "INSERT INTO payment_webhook_log (id, gateway, payload_hash, status, processed_at) VALUES (?, ?, ?, 'processed', ?)",
        &[
            (Uuid::new_v4()).into_arg(),
            (gateway_name.clone()).into_arg(),
            (payload_hash).into_arg(),
            (utc_now()).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        None,
        "system",
        "finance.webhook_received",
        "donation",
        Some(&donation.id.to_string()),
        None,
        Some(json!({ "gateway": gateway_name, "gateway_reference": ref_id })),
        ip.as_deref(),
        None,
    )
    .await?;
    Ok(Json(
        json!({ "already_processed": false, "status": "processed" }),
    ))
}

// --- public: methods, manual donation, campaigns, recurring ------------------

/// `GET /api/v1/finance/donation-methods` — only enabled and fully
/// configured methods; a disabled method never appears here.
pub async fn donation_methods(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let mut methods: Vec<Value> = Vec::new();
    if setting_flag(db, &state.config, "donation.online_enabled", true).await? {
        let mut configured: Vec<&str> = Vec::new();
        if matches!(get_setting(db, &state.config, "payment.zarinpal_key").await?, Some(ref s) if !s.is_empty())
        {
            configured.push("zarinpal");
        }
        if matches!(get_setting(db, &state.config, "payment.idpay_key").await?, Some(ref s) if !s.is_empty())
        {
            configured.push("idpay");
        }
        if !configured.is_empty() {
            methods.push(json!({
                "method": "online",
                "title": method_title("online"),
                "instructions": Value::Null,
                "currency": Value::Null,
                "gateways": configured,
                "card_number": Value::Null,
                "card_holder": Value::Null,
                "wallets": [],
                "paypal_account": Value::Null,
            }));
        }
    }
    if let Some(card) = manual_method_config(db, &state.config, "card").await? {
        methods.push(json!({
            "method": "card",
            "title": method_title("card"),
            "instructions": card.instructions,
            "currency": "IRR",
            "gateways": [],
            "card_number": card.card_number,
            "card_holder": card.card_holder,
            "wallets": [],
            "paypal_account": Value::Null,
        }));
    }
    if let Some(crypto) = manual_method_config(db, &state.config, "crypto").await? {
        methods.push(json!({
            "method": "crypto",
            "title": method_title("crypto"),
            "instructions": crypto.instructions,
            "currency": Value::Null,
            "gateways": [],
            "card_number": Value::Null,
            "card_holder": Value::Null,
            "wallets": crypto.wallets.iter().map(|w| json!({
                "currency": w.currency,
                "network": w.network,
                "address": w.address,
            })).collect::<Vec<_>>(),
            "paypal_account": Value::Null,
        }));
    }
    if let Some(paypal) = manual_method_config(db, &state.config, "paypal").await? {
        methods.push(json!({
            "method": "paypal",
            "title": method_title("paypal"),
            "instructions": paypal.instructions,
            "currency": "USD",
            "gateways": [],
            "card_number": Value::Null,
            "card_holder": Value::Null,
            "wallets": [],
            "paypal_account": paypal.paypal_account,
        }));
    }
    Ok(Json(json!({ "methods": methods })))
}

/// `POST /api/v1/finance/donate/manual` — manual donation
/// (card-to-card / crypto / PayPal), always `pending`. At least one
/// proof is required: a receipt image (PNG/JPG/WEBP, max 5 MB) or
/// tracking text. Money is only booked when an admin approves.
pub async fn donate_manual(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    mp: Multipart,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let form = collect_manual_form(mp).await?;
    // The router reads the receipt first: an oversized image is 413
    // before any service validation runs.
    if let Some((_, overflow)) = &form.receipt {
        if *overflow {
            return Err(ApiError::with_details(
                StatusCode::PAYLOAD_TOO_LARGE,
                "receipt_too_large",
                "حجم تصویر رسید نباید بیش از ۵ مگابایت باشد",
                json!({ "max_bytes": RECEIPT_IMAGE_MAX_BYTES }),
            ));
        }
    }
    let Some(method) = form.method.clone() else {
        return Err(validation_err("method", "field is required"));
    };
    let Some(amount_raw) = form.amount.clone() else {
        return Err(validation_err("amount", "field is required"));
    };
    let Some(amount) = parse_form_int(&amount_raw) else {
        return Err(validation_err("amount", "must be an integer"));
    };
    let campaign_id = match form.campaign_id.as_deref() {
        None => None,
        Some(s) => Some(
            s.parse::<Uuid>()
                .map_err(|_| validation_err("campaign_id", "must be a UUID"))?,
        ),
    };
    let db = state.db_or_500()?;
    let user = optional_user(&state, &headers).await;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let user_id = user.as_ref().map(|c| c.user.id);
    let actor_type = if user.is_some() { "user" } else { "anonymous" };

    if !matches!(method.as_str(), "card" | "crypto" | "paypal") {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unknown_method",
            "روش پرداخت نامعتبر است",
        ));
    }
    let Some(cfg) = manual_method_config(db, &state.config, &method).await? else {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "donation_method_disabled",
            "این روش پرداخت در حال حاضر فعال نیست",
        ));
    };
    if amount <= 0 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_amount",
            "مبلغ باید بزرگ‌تر از صفر باشد",
        ));
    }
    let cap = if method == "card" {
        MAX_DONATE_IRR
    } else {
        MAX_MANUAL_OTHER
    };
    if amount > cap {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "amount_too_large",
            "مبلغ از سقف مجاز بیشتر است",
        ));
    }
    if let Some(cid) = campaign_id {
        let campaign = get_campaign_by_id(db, cid).await?;
        if campaign.is_none() || campaign.map(|c| c.status) != Some("active".to_string()) {
            return Err(ApiError::not_found(
                "campaign_not_found",
                "کمپین فعال یافت نشد",
            ));
        }
    }
    let mut currency = cfg.currency.clone().unwrap_or_default();
    let mut wallet_label: Option<String> = None;
    if method == "crypto" {
        let want_currency = form
            .wallet_currency
            .clone()
            .unwrap_or_default()
            .trim()
            .to_uppercase();
        let want_network = form
            .wallet_network
            .clone()
            .unwrap_or_default()
            .trim()
            .to_uppercase();
        let wallet = cfg.wallets.iter().find(|w| {
            w.currency.to_uppercase() == want_currency && w.network.to_uppercase() == want_network
        });
        let Some(wallet) = wallet else {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "wallet_not_available",
                "کیف پول انتخاب‌شده در دسترس نیست",
            ));
        };
        currency = wallet.currency.to_uppercase();
        wallet_label = Some(if wallet.network.is_empty() {
            wallet.currency.clone()
        } else {
            format!("{} · {}", wallet.currency, wallet.network)
        });
    }
    let proof_text_clean = form
        .proof_text
        .clone()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let (image_data, image_mime, image_ext) = match &form.receipt {
        Some((bytes, _)) if !bytes.is_empty() => {
            let Some((mime, ext)) = sniff_receipt_image(bytes) else {
                return Err(ApiError::new(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "unsupported_receipt_image",
                    "تصویر رسید باید PNG یا JPG یا WEBP باشد",
                ));
            };
            (Some(bytes.clone()), Some(mime), Some(ext))
        }
        _ => (None, None, None),
    };
    if proof_text_clean.is_none() && image_data.is_none() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "proof_required",
            "ارسال رسید (تصویر یا متن/کد رهگیری) الزامی است",
        ));
    }
    if let Some(data) = &image_data {
        scan_receipt_upload(
            db,
            &state.config,
            ScanAudit {
                actor_id: user_id,
                actor_type,
                ip: ip.as_deref(),
                user_agent: ua.as_deref(),
            },
            data,
            Some("receipt"),
        )
        .await?;
    }

    let intent_id = Uuid::new_v4();
    let now = utc_now();
    // Store the image before inserting the intent, so a storage
    // failure leaves no orphan row (Python rolls the whole session
    // back on the same failure).
    let image_key: Option<String> = match (&image_data, image_ext) {
        (Some(data), Some(ext)) => {
            let key = format!(
                "donation-receipts/{intent_id}/{}.{}",
                Uuid::new_v4().simple(),
                ext
            );
            put_object(&state.config, &key, data).await?;
            Some(key)
        }
        _ => None,
    };
    db.execute(
        "INSERT INTO payment_intents (id, idempotency_key, amount, currency, campaign_id, donor_id, donor_name, status, gateway, authority, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?, NULL, ?, NULL)",
        &[
            (intent_id).into_arg(),
            (format!("manual:{}", Uuid::new_v4().simple())).into_arg(),
            (amount).into_arg(),
            (currency.clone()).into_arg(),
            (campaign_id).into_arg(),
            (user_id).into_arg(),
            (form.donor_name.clone()).into_arg(),
            (method.clone()).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    let proof_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO manual_donation_proofs (id, payment_intent_id, method, proof_text, proof_image_key, proof_image_mime, wallet_label, amount_irr, fx_rate_irr, fx_source, review_status, reviewed_by, reviewed_at, review_note, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL, 'pending', NULL, NULL, NULL, ?, NULL)",
        &[
            (proof_id).into_arg(),
            (intent_id).into_arg(),
            (method.clone()).into_arg(),
            (proof_text_clean.clone()).into_arg(),
            (image_key.clone()).into_arg(),
            (image_mime.map(str::to_string)).into_arg(),
            (wallet_label).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        user_id,
        actor_type,
        "finance.manual_donation_submitted",
        "payment_intent",
        Some(&intent_id.to_string()),
        None,
        Some(json!({
            "method": method,
            "amount": amount,
            "currency": currency,
            "campaign_id": campaign_id.map(|u| u.to_string()),
            "has_proof_image": image_key.is_some(),
            "has_proof_text": proof_text_clean.is_some(),
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": proof_id.to_string(),
            "intent_id": intent_id.to_string(),
            "method": method,
            "status": "pending",
            "amount": amount,
            "currency": currency,
            "created_at": now.to_rfc3339(),
        })),
    ))
}

/// `GET /api/v1/finance/campaigns` — active campaigns, newest first.
pub async fn list_campaigns(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, title, description, goal_amount, raised_amount, status, cover, created_at FROM donation_campaigns WHERE status = 'active' ORDER BY created_at DESC",
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        out.push(Campaign::from_row(&row)?.out());
    }
    Ok(Json(Value::Array(out)))
}

/// `GET /api/v1/finance/campaigns/{campaign_id}` — active only.
pub async fn get_campaign(
    State(state): State<AppState>,
    Path(campaign_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&campaign_id, "campaign_id")?;
    let db = state.db_or_500()?;
    let campaign = get_campaign_by_id(db, id).await?;
    match campaign {
        Some(c) if c.status == "active" => Ok(Json(c.out())),
        _ => Err(ApiError::not_found(
            "campaign_not_found",
            "کمپین فعال یافت نشد",
        )),
    }
}

/// `POST /api/v1/finance/recurring` — register a recurring donation.
/// Periodic charging is the ARQ worker's job (Python phase 16), so
/// `next_charge_at` stays null, exactly like Python.
pub async fn create_recurring(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = authenticate(&state, &headers, "/api/v1/finance/recurring").await?;
    let parsed = parse_body(&body)?;
    let amount = match parsed.get("amount").and_then(as_int) {
        Some(n) if n > 0 => n,
        _ => return Err(validation_err("amount", "must be a positive integer")),
    };
    let currency = match parsed.get("currency") {
        None | Some(Value::Null) => "IRR".to_string(),
        Some(Value::String(s)) if s == "IRR" => s.clone(),
        Some(_) => return Err(validation_err("currency", "must be IRR")),
    };
    let interval = match parsed.get("interval") {
        None | Some(Value::Null) => "monthly".to_string(),
        Some(Value::String(s)) if s == "weekly" || s == "monthly" || s == "yearly" => s.clone(),
        Some(_) => {
            return Err(validation_err(
                "interval",
                "must be weekly, monthly or yearly",
            ));
        }
    };
    let campaign_id = opt_uuid(&parsed, "campaign_id")?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    if let Some(cid) = campaign_id {
        let campaign = get_campaign_by_id(db, cid).await?;
        if campaign.is_none() || campaign.map(|c| c.status) != Some("active".to_string()) {
            return Err(ApiError::not_found(
                "campaign_not_found",
                "کمپین فعال یافت نشد",
            ));
        }
    }
    let rec_id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO recurring_donations (id, donor_id, campaign_id, amount, currency, interval, status, next_charge_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'active', NULL, ?, NULL)",
        &[
            (rec_id).into_arg(),
            (ctx.user.id).into_arg(),
            (campaign_id).into_arg(),
            (amount).into_arg(),
            (currency.clone()).into_arg(),
            (interval.clone()).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "finance.recurring_created",
        "recurring_donation",
        Some(&rec_id.to_string()),
        None,
        Some(json!({ "amount": amount, "interval": interval })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": rec_id.to_string(),
            "amount": amount,
            "currency": currency,
            "campaign_id": campaign_id.map(|u| u.to_string()),
            "interval": interval,
            "status": "active",
            "next_charge_at": Value::Null,
        })),
    ))
}

// --- admin: campaigns ---------------------------------------------------------

/// Validated campaign body: `(title, description, goal_amount, status, cover)`.
type CampaignBody = (Value, Option<Value>, i64, String, Option<String>);

fn validate_campaign_body(body: &Value) -> ApiResult<CampaignBody> {
    let title = req_localized(body, "title")?;
    let description = opt_localized(body, "description")?;
    let goal_amount = match body.get("goal_amount").and_then(as_int) {
        Some(n) if n >= 0 => n,
        _ => return Err(validation_err("goal_amount", "must be an integer >= 0")),
    };
    let status = match body.get("status") {
        None | Some(Value::Null) => "active".to_string(),
        Some(Value::String(s)) if s == "active" || s == "draft" || s == "archived" => s.clone(),
        Some(_) => {
            return Err(validation_err(
                "status",
                "must be active, draft or archived",
            ));
        }
    };
    let cover = opt_str(body, "cover", 0, 1024)?;
    Ok((title, description, goal_amount, status, cover))
}

/// `POST /api/v1/admin/finance/campaigns` (201).
pub async fn admin_create_campaign(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let (title, description, goal_amount, status, cover) = validate_campaign_body(&parsed)?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let campaign_id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO donation_campaigns (id, title, description, goal_amount, raised_amount, status, cover, created_at, updated_at) VALUES (?, ?, ?, ?, 0, ?, ?, ?, NULL)",
        &[
            (campaign_id).into_arg(),
            (title.clone()).into_arg(),
            (description.clone()).into_arg(),
            (goal_amount).into_arg(),
            (status).into_arg(),
            (cover).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "finance.campaign_created",
        "donation_campaign",
        Some(&campaign_id.to_string()),
        None,
        Some(json!({ "title": title, "goal_amount": goal_amount })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let campaign = get_campaign_by_id(db, campaign_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok((StatusCode::CREATED, Json(campaign.out())))
}

/// `GET /api/v1/admin/finance/campaigns` — all statuses.
pub async fn admin_list_campaigns(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, title, description, goal_amount, raised_amount, status, cover, created_at FROM donation_campaigns ORDER BY created_at DESC",
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        out.push(Campaign::from_row(&row)?.out());
    }
    Ok(Json(Value::Array(out)))
}

/// `PUT|PATCH /api/v1/admin/finance/campaigns/{campaign_id}` — both
/// verbs share Python's full `CampaignIn` body.
pub async fn admin_update_campaign(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(campaign_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let id = parse_path_uuid(&campaign_id, "campaign_id")?;
    let parsed = parse_body(&body)?;
    let (title, description, goal_amount, status, cover) = validate_campaign_body(&parsed)?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let Some(campaign) = get_campaign_by_id(db, id).await? else {
        return Err(ApiError::not_found("campaign_not_found", "کمپین یافت نشد"));
    };
    let old_status = campaign.status.clone();
    let old_goal = campaign.goal_amount;
    db.execute(
        "UPDATE donation_campaigns SET title = ?, description = ?, goal_amount = ?, status = ?, cover = ?, updated_at = ? WHERE id = ?",
        &[
            (title.clone()).into_arg(),
            (description.clone()).into_arg(),
            (goal_amount).into_arg(),
            (status.clone()).into_arg(),
            (cover).into_arg(),
            (utc_now()).into_arg(),
            (id).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "finance.campaign_updated",
        "donation_campaign",
        Some(&id.to_string()),
        Some(json!({ "status": old_status, "goal_amount": old_goal })),
        Some(json!({ "status": status, "goal_amount": goal_amount })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let campaign = get_campaign_by_id(db, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(Json(campaign.out()))
}

/// `DELETE /api/v1/admin/finance/campaigns/{campaign_id}` (204) —
/// refused with 409 while the campaign has donations.
pub async fn admin_delete_campaign(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(campaign_id): Path<String>,
) -> ApiResult<StatusCode> {
    let ctx = admin_ctx(&state, &headers).await?;
    let id = parse_path_uuid(&campaign_id, "campaign_id")?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let Some(campaign) = get_campaign_by_id(db, id).await? else {
        return Err(ApiError::not_found("campaign_not_found", "کمپین یافت نشد"));
    };
    let has_donations = db
        .fetch_optional(
            "SELECT id FROM donations WHERE campaign_id = ? LIMIT 1",
            &[(id).into_arg()],
        )
        .await?
        .is_some();
    if has_donations {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "campaign_has_donations",
            "این کمپین اهدا دارد و نمی‌توان آن را حذف کرد",
        ));
    }
    db.execute(
        "DELETE FROM donation_campaigns WHERE id = ?",
        &[(id).into_arg()],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "finance.campaign_deleted",
        "donation_campaign",
        Some(&id.to_string()),
        Some(json!({ "title": campaign.title, "goal_amount": campaign.goal_amount })),
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- admin: lists --------------------------------------------------------------

/// Shared cursor page for the donations / ledger / manual lists:
/// `ORDER BY created_at DESC, id DESC`, `limit+1` rows, next cursor
/// from the last kept row.
fn page_limit(params: &HashMap<String, String>) -> ApiResult<i64> {
    let limit = query_int(params, "limit", 20, 1, CURSOR_LIMIT)?;
    Ok(limit)
}

/// `GET /api/v1/admin/finance/donations`.
pub async fn admin_list_donations(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    let limit = page_limit(&params)?;
    let cursor = params.get("cursor").filter(|s| !s.is_empty()).cloned();
    let cursor = match cursor {
        Some(c) => Some(decode_cursor(&c)?),
        None => None,
    };
    let db = state.db_or_500()?;
    let mut sql = String::from(
        "SELECT id, payment_intent_id, gateway, gateway_reference, status, amount, currency, campaign_id, donor_name, created_at FROM donations",
    );
    let mut args: Vec<Arg> = Vec::new();
    if let Some((ts, lid)) = cursor {
        sql.push_str(" WHERE (created_at < ? OR (created_at = ? AND id < ?))");
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(lid.into_arg());
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items: Vec<Value> = Vec::new();
    let mut keys: Vec<(DateTime<Utc>, Uuid)> = Vec::new();
    for row in rows {
        let d = Donation::from_row(&row)?;
        keys.push((d.created_at, d.id));
        items.push(d.out());
    }
    let mut next_cursor: Option<String> = None;
    if items.len() > limit as usize {
        let (ts, lid) = keys[(limit as usize) - 1];
        next_cursor = Some(encode_cursor(ts, lid));
        items.truncate(limit as usize);
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `GET /api/v1/admin/finance/ledger`.
pub async fn admin_list_ledger(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    let limit = page_limit(&params)?;
    let cursor = params.get("cursor").filter(|s| !s.is_empty()).cloned();
    let cursor = match cursor {
        Some(c) => Some(decode_cursor(&c)?),
        None => None,
    };
    let db = state.db_or_500()?;
    let mut sql = String::from(
        "SELECT id, entry_type, amount, currency, reference, donation_id, created_at FROM ledger_entries",
    );
    let mut args: Vec<Arg> = Vec::new();
    if let Some((ts, lid)) = cursor {
        sql.push_str(" WHERE (created_at < ? OR (created_at = ? AND id < ?))");
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(lid.into_arg());
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items: Vec<Value> = Vec::new();
    let mut keys: Vec<(DateTime<Utc>, Uuid)> = Vec::new();
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        keys.push((created_at, id));
        items.push(json!({
            "id": id.to_string(),
            "entry_type": row.try_get::<String, _>("entry_type")?,
            "amount": row.try_get::<i64, _>("amount")?,
            "currency": row.try_get::<String, _>("currency")?,
            "reference": row.try_get::<Option<String>, _>("reference")?,
            "donation_id": row.try_get::<Option<Uuid>, _>("donation_id")?.map(|u| u.to_string()),
            "created_at": created_at.to_rfc3339(),
        }));
    }
    let mut next_cursor: Option<String> = None;
    if items.len() > limit as usize {
        let (ts, lid) = keys[(limit as usize) - 1];
        next_cursor = Some(encode_cursor(ts, lid));
        items.truncate(limit as usize);
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `GET /api/v1/admin/finance/reconcile` — neither gateway exposes a
/// transaction report (`fetch_report` raises for both), so after the
/// config check this is honestly 503 `gateway_report_unavailable`,
/// exactly like Python. No report is ever fabricated.
pub async fn admin_reconcile(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    let gateway = params
        .get("gateway")
        .cloned()
        .unwrap_or_else(|| "zarinpal".to_string());
    let db = state.db_or_500()?;
    let _ = gateway_config(db, &state.config, &gateway).await?;
    Err(ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "gateway_report_unavailable",
        "گزارش تراکنش این درگاه در دسترس نیست",
    ))
}

/// `GET /api/v1/admin/finance/gateways` — only `configured`, never
/// a secret value.
pub async fn admin_gateway_statuses(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let z_key = get_setting(db, &state.config, "payment.zarinpal_key").await?;
    let z_sandbox =
        get_typed_setting(db, &state.config, "payment.zarinpal_sandbox", json!(true)).await?;
    let i_key = get_setting(db, &state.config, "payment.idpay_key").await?;
    let i_sandbox =
        get_typed_setting(db, &state.config, "payment.idpay_sandbox", json!(true)).await?;
    Ok(Json(json!({
        "zarinpal": {
            "configured": matches!(z_key, Some(ref s) if !s.is_empty()),
            "sandbox": typed_bool_or(&z_sandbox, true),
        },
        "idpay": {
            "configured": matches!(i_key, Some(ref s) if !s.is_empty()),
            "sandbox": typed_bool_or(&i_sandbox, true),
        },
    })))
}

/// `GET /api/v1/admin/finance/manual-donations` — optional `status`
/// filter (FastAPI 422 on any other value), cursor page.
pub async fn admin_list_manual_donations(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    if let Some(status) = params.get("status") {
        if !matches!(status.as_str(), "pending" | "approved" | "rejected") {
            return Err(validation_err(
                "status",
                "must be pending, approved or rejected",
            ));
        }
    }
    let limit = page_limit(&params)?;
    let cursor = params.get("cursor").filter(|s| !s.is_empty()).cloned();
    let cursor = match cursor {
        Some(c) => Some(decode_cursor(&c)?),
        None => None,
    };
    let db = state.db_or_500()?;
    let mut sql = String::from(
        "SELECT p.id AS pid, p.payment_intent_id, p.method, p.proof_text, p.proof_image_key, p.proof_image_mime, p.wallet_label, p.amount_irr, p.fx_rate_irr, p.fx_source, p.review_status, p.review_note, p.reviewed_at, p.created_at AS p_created_at, i.id AS iid, i.idempotency_key, i.amount, i.currency, i.campaign_id, i.donor_id, i.donor_name, i.status, i.gateway, i.authority FROM manual_donation_proofs p JOIN payment_intents i ON p.payment_intent_id = i.id",
    );
    let mut args: Vec<Arg> = Vec::new();
    let mut conds: Vec<String> = Vec::new();
    if let Some(status) = params.get("status") {
        conds.push("p.review_status = ?".to_string());
        args.push(status.clone().into_arg());
    }
    if let Some((ts, lid)) = cursor {
        conds.push("(p.created_at < ? OR (p.created_at = ? AND p.id < ?))".to_string());
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(lid.into_arg());
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY p.created_at DESC, p.id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items: Vec<Value> = Vec::new();
    let mut keys: Vec<(DateTime<Utc>, Uuid)> = Vec::new();
    for row in rows {
        // Rebuild the two rows from the aliased join (the Proof /
        // Intent readers expect their own column names).
        let proof = Proof {
            id: row.try_get::<Uuid, _>("pid")?,
            payment_intent_id: row.try_get::<Uuid, _>("payment_intent_id")?,
            method: row.try_get::<String, _>("method")?,
            proof_text: row.try_get::<Option<String>, _>("proof_text")?,
            proof_image_key: row.try_get::<Option<String>, _>("proof_image_key")?,
            wallet_label: row.try_get::<Option<String>, _>("wallet_label")?,
            amount_irr: row.try_get::<Option<i64>, _>("amount_irr")?,
            fx_rate_irr: row.try_get::<Option<i64>, _>("fx_rate_irr")?,
            fx_source: row.try_get::<Option<String>, _>("fx_source")?,
            review_status: row.try_get::<String, _>("review_status")?,
            review_note: row.try_get::<Option<String>, _>("review_note")?,
            reviewed_at: row.try_get::<Option<DateTime<Utc>>, _>("reviewed_at")?,
            created_at: row.try_get::<DateTime<Utc>, _>("p_created_at")?,
        };
        let intent = Intent {
            id: row.try_get::<Uuid, _>("iid")?,
            idempotency_key: row.try_get::<String, _>("idempotency_key")?,
            amount: row.try_get::<i64, _>("amount")?,
            currency: row.try_get::<String, _>("currency")?,
            campaign_id: row.try_get::<Option<Uuid>, _>("campaign_id")?,
            donor_id: row.try_get::<Option<Uuid>, _>("donor_id")?,
            donor_name: row.try_get::<Option<String>, _>("donor_name")?,
            status: row.try_get::<String, _>("status")?,
            gateway: row.try_get::<String, _>("gateway")?,
            authority: row.try_get::<Option<String>, _>("authority")?,
        };
        keys.push((proof.created_at, proof.id));
        items.push(proof.admin_out(&intent));
    }
    let mut next_cursor: Option<String> = None;
    if items.len() > limit as usize {
        let (ts, lid) = keys[(limit as usize) - 1];
        next_cursor = Some(encode_cursor(ts, lid));
        items.truncate(limit as usize);
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `GET /api/v1/admin/finance/manual-donations/{proof_id}/proof-image`
/// — receipt bytes for the CRM (finance.manage only, no public URL).
pub async fn admin_manual_proof_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(proof_id): Path<String>,
) -> ApiResult<Response<Body>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let id = parse_path_uuid(&proof_id, "proof_id")?;
    let db = state.db_or_500()?;
    let Some((key, mime)) = get_proof_image_key(db, id).await? else {
        return Err(ApiError::not_found(
            "manual_donation_not_found",
            "حمایت دستی یافت نشد",
        ));
    };
    let Some(key) = key else {
        return Err(ApiError::not_found(
            "proof_image_not_found",
            "برای این حمایت تصویر رسید ثبت نشده است",
        ));
    };
    // Python lets a missing file die as a 500 (FileNotFoundError);
    // only a non-local storage provider is a 503 there.
    let data = read_bytes(&state.config, &key).await.map_err(|e| {
        if e.code == "storage_not_configured" {
            e
        } else {
            ApiError::internal()
        }
    })?;
    let mime = mime.unwrap_or_else(|| "application/octet-stream".to_string());
    let res = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", mime)
        .body(Body::from(data))
        .map_err(|_| ApiError::internal())?;
    Ok(res)
}

/// `GET /api/v1/admin/finance/fx-rate?currency=USDT` — live rate for
/// the CRM to show before approving a foreign-currency donation.
pub async fn admin_fx_rate(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    let Some(currency) = params.get("currency").cloned() else {
        return Err(validation_err("currency", "field is required"));
    };
    let cur = currency.trim().to_uppercase();
    if cur.is_empty() || cur.len() > 10 || !cur.chars().all(|c| c.is_alphanumeric()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_currency",
            "ارز نامعتبر است",
        ));
    }
    match get_rate_irr(&cur).await {
        Some((rate, source)) => Ok(Json(json!({
            "currency": cur,
            "rate_irr": rate,
            "source": source,
        }))),
        None => Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "fx_unavailable",
            "نرخ لحظه‌ای ارز در دسترس نیست؛ نرخ را دستی وارد کنید",
        )),
    }
}

// --- admin: manual review ------------------------------------------------------

async fn review_manual_donation(
    state: &AppState,
    headers: &HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    proof_id: &str,
    body: axum::body::Bytes,
    approve: bool,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(state, headers).await?;
    let id = parse_path_uuid(proof_id, "proof_id")?;
    let (note, rate_irr): (Option<String>, Option<i64>) = if body.is_empty() {
        (None, None)
    } else {
        let parsed = parse_body(&body)?;
        let note = opt_str(&parsed, "note", 0, 512)?;
        let rate_irr = match parsed.get("rate_irr") {
            None | Some(Value::Null) => None,
            Some(v) => match as_int(v) {
                Some(n) if n > 0 && n <= 10_000_000_000_000 => Some(n),
                _ => return Err(validation_err("rate_irr", "must be between 1 and 10^13")),
            },
        };
        (note, rate_irr)
    };
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(state, headers, peer);
    let Some(proof) = get_proof_by_id(db, id).await? else {
        return Err(ApiError::not_found(
            "manual_donation_not_found",
            "حمایت دستی یافت نشد",
        ));
    };
    if proof.review_status != "pending" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "already_reviewed",
            "این حمایت قبلاً بازبینی شده است",
        ));
    }
    let Some(intent) = get_intent_by_id(db, proof.payment_intent_id).await? else {
        return Err(ApiError::not_found(
            "intent_not_found",
            "نیت پرداخت یافت نشد",
        ));
    };
    // The FX conversion is computed before any mutation, so a missing
    // rate leaves the donation pending (422) with no partial rows.
    let (credit_irr, fx_rate, fx_source): (Option<i64>, Option<i64>, Option<String>) = if approve {
        if intent.currency == "IRR" {
            (Some(intent.amount), None, None)
        } else if let Some(rate) = rate_irr {
            let credit = intent
                .amount
                .checked_mul(rate)
                .ok_or_else(ApiError::internal)?;
            (Some(credit), Some(rate), Some("manual".to_string()))
        } else {
            match get_rate_irr(&intent.currency).await {
                Some((rate, source)) => {
                    let credit = intent
                        .amount
                        .checked_mul(rate)
                        .ok_or_else(ApiError::internal)?;
                    (Some(credit), Some(rate), Some(source))
                }
                None => {
                    return Err(ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "fx_rate_required",
                        "نرخ لحظه‌ای ارز در دسترس نیست؛ نرخ هر واحد را دستی وارد کنید",
                    ));
                }
            }
        }
    } else {
        (None, None, None)
    };
    // A repeated proof text (same transaction submitted twice) hits
    // the unique gateway_reference — check first so the proof stays
    // pending on the 409, exactly like Python's rollback.
    let reference = if approve {
        let proof_ref = match &proof.proof_text {
            Some(t) if t.chars().count() <= 100 => t.clone(),
            _ => format!("manual:{}", intent.id),
        };
        if get_donation_by_ref(db, &proof_ref).await?.is_some() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "duplicate_reference",
                "این رسید/هش قبلاً برای اهدای دیگری ثبت شده است",
            ));
        }
        proof_ref
    } else {
        String::new()
    };
    let now = utc_now();
    if !approve {
        db.execute(
            "UPDATE manual_donation_proofs SET review_status = 'rejected', reviewed_by = ?, reviewed_at = ?, review_note = ?, updated_at = ? WHERE id = ?",
            &[
                (ctx.user.id).into_arg(),
                (now).into_arg(),
                (note.clone()).into_arg(),
                (now).into_arg(),
                (id).into_arg(),
            ],
        )
        .await?;
        db.execute(
            "UPDATE payment_intents SET status = 'cancelled', updated_at = ? WHERE id = ?",
            &[(now).into_arg(), (intent.id).into_arg()],
        )
        .await?;
        log_audit(
            db,
            Some(ctx.user.id),
            "user",
            "finance.manual_donation_rejected",
            "payment_intent",
            Some(&intent.id.to_string()),
            None,
            Some(json!({ "method": proof.method, "note": note })),
            ip.as_deref(),
            ua.as_deref(),
        )
        .await?;
    } else {
        db.execute(
            "UPDATE manual_donation_proofs SET review_status = 'approved', reviewed_by = ?, reviewed_at = ?, review_note = ?, amount_irr = ?, fx_rate_irr = ?, fx_source = ?, updated_at = ? WHERE id = ?",
            &[
                (ctx.user.id).into_arg(),
                (now).into_arg(),
                (note.clone()).into_arg(),
                (credit_irr).into_arg(),
                (fx_rate).into_arg(),
                (fx_source.clone()).into_arg(),
                (now).into_arg(),
                (id).into_arg(),
            ],
        )
        .await?;
        log_audit(
            db,
            Some(ctx.user.id),
            "user",
            "finance.manual_donation_approved",
            "payment_intent",
            Some(&intent.id.to_string()),
            None,
            Some(json!({
                "method": proof.method,
                "gateway_reference": reference,
                "note": note,
                "amount_irr": credit_irr,
                "fx_rate_irr": fx_rate,
                "fx_source": fx_source,
            })),
            ip.as_deref(),
            ua.as_deref(),
        )
        .await?;
        let intent = get_intent_by_id(db, intent.id)
            .await?
            .ok_or_else(ApiError::internal)?;
        finalize_success(
            db,
            &intent,
            &reference,
            "user",
            Some(ctx.user.id),
            ip.as_deref(),
            ua.as_deref(),
            credit_irr,
        )
        .await?;
    }
    let proof = get_proof_by_id(db, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    let intent = get_intent_by_id(db, proof.payment_intent_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(Json(proof.admin_out(&intent)))
}

/// `POST /api/v1/admin/finance/manual-donations/{proof_id}/approve`.
pub async fn approve_manual_donation(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(proof_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    review_manual_donation(&state, &headers, peer, &proof_id, body, true).await
}

/// `POST /api/v1/admin/finance/manual-donations/{proof_id}/reject`.
pub async fn reject_manual_donation(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(proof_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    review_manual_donation(&state, &headers, peer, &proof_id, body, false).await
}
