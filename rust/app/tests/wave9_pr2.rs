//! Wave 9 PR-2 tests: the CI gates' runtime side — real Redis and the
//! Postgres extensions the CI services provide (ADR-149).
//!
//! * Redis (gated on `REDIS_URL`): PING, SET/GET round-trip with unique
//!   keys, and the distributed `RateLimiter` against real Redis
//!   (allow → deny at the limit → `reset` → allow again), plus the
//!   fail-closed auth behaviour with a dead Redis in production and the
//!   memory fallback in dev. Skipped with a clear message when
//!   `REDIS_URL` is unset — but CI always sets it to the Redis service,
//!   so a skip there means the gate is miswired, not green.
//! * `/ready` against real and dead Redis (sqlite DB): `redis: "ok"`
//!   with a live server, `"unavailable"` with a dead one, 200 either
//!   way — Redis never fails readiness.
//! * Postgres extensions (PG smoke, gated on `WAVE2_PG_URL`):
//!   `CREATE EXTENSION vector` + `pg_trgm`, a `vector(3)` round-trip
//!   with cosine distance, and a `pg_trgm` similarity check. This is
//!   the same pair the Alembic migration e2b3c4d5f6a7 creates, so the
//!   smoke proves the extensions are installable on the test Postgres,
//!   not just named in a migration file.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
};
use maturidi_api_rs::{build_router, AppState, Config, Db, RateLimiter};
use serde_json::Value;
use std::net::SocketAddr;
use tower::ServiceExt;

fn redis_url() -> Option<String> {
    std::env::var("REDIS_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn test_config(redis_url: &str) -> Config {
    Config {
        app_env: "test".to_string(),
        // /ready probes config.database_url itself (lib.rs probe_db),
        // not the test pool — so the config must name a reachable DB,
        // the same in-memory sqlite URL the pool uses (wave9_pr1 does
        // the same for its /ready tests).
        database_url: Some("sqlite://:memory:".to_string()),
        redis_url: redis_url.to_string(),
        jwt_secret: "test-secret-wave9-pr2".to_string(),
        encryption_key: "test-encryption-key-wave9-pr2".to_string(),
        captcha_enabled: false,
        storage_dir: format!(
            "{}/wave9-pr2-storage-{}",
            std::env::temp_dir().display(),
            std::process::id()
        ),
        ..Default::default()
    }
}

async fn sqlite_env(redis_url: &str) -> AppState {
    let db = Db::connect("sqlite://:memory:")
        .await
        .expect("sqlite connect");
    maturidi_api_rs::init_test_schema(&db)
        .await
        .expect("test schema");
    AppState::with_db(test_config(redis_url), Some(db))
}

async fn call_ready(state: AppState) -> (StatusCode, Value) {
    let res = build_router(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/ready")
                .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("read body");
    let value: Value = serde_json::from_slice(&bytes).expect("ready body is JSON");
    (status, value)
}

async fn redis_conn(url: &str) -> redis::aio::MultiplexedConnection {
    let client = redis::Client::open(url).expect("redis client");
    client
        .get_multiplexed_async_connection()
        .await
        .expect("redis connect")
}

// ============================ Redis ============================

/// PING against the Redis `REDIS_URL` points at. Skipped when unset.
#[tokio::test]
async fn wave9_pr2_redis_ping() {
    let Some(url) = redis_url() else {
        eprintln!("wave9_pr2_redis_ping: REDIS_URL unset, skipping (CI must set it)");
        return;
    };
    let mut conn = redis_conn(&url).await;
    let pong: String = redis::cmd("PING")
        .query_async(&mut conn)
        .await
        .expect("PING");
    assert_eq!(pong, "PONG");
}

/// SET/GET round-trip (plus SETEX + TTL + DEL) on unique keys, so the
/// test is safe under parallel test binaries. Skipped when unset.
#[tokio::test]
async fn wave9_pr2_redis_set_get() {
    let Some(url) = redis_url() else {
        eprintln!("wave9_pr2_redis_set_get: REDIS_URL unset, skipping (CI must set it)");
        return;
    };
    let mut conn = redis_conn(&url).await;
    let key = format!("pr2:setget:{}", std::process::id());
    let val = format!("hello-{}", std::process::id());
    let ok: String = redis::cmd("SET")
        .arg(&key)
        .arg(&val)
        .query_async(&mut conn)
        .await
        .expect("SET");
    assert_eq!(ok, "OK");
    let got: Option<String> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut conn)
        .await
        .expect("GET");
    assert_eq!(got.as_deref(), Some(val.as_str()));
    let ok: String = redis::cmd("SETEX")
        .arg(format!("{key}:ex"))
        .arg(60)
        .arg(&val)
        .query_async(&mut conn)
        .await
        .expect("SETEX");
    assert_eq!(ok, "OK");
    let ttl: i64 = redis::cmd("TTL")
        .arg(format!("{key}:ex"))
        .query_async(&mut conn)
        .await
        .expect("TTL");
    assert!(
        ttl > 0 && ttl <= 60,
        "TTL must be within the 60 s window, got {ttl}"
    );
    let deleted: i64 = redis::cmd("DEL")
        .arg(&key)
        .arg(format!("{key}:ex"))
        .query_async(&mut conn)
        .await
        .expect("DEL");
    assert_eq!(deleted, 2);
    let gone: Option<String> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut conn)
        .await
        .expect("GET after DEL");
    assert!(gone.is_none());
}

/// The distributed `RateLimiter` against real Redis: two hits allowed at
/// limit 2, the third denied, `reset` clears the counter. Then the dead
/// Redis contract: auth endpoints fail closed in production, non-auth
/// falls back to the in-memory window in dev. Skipped when unset.
#[tokio::test]
async fn wave9_pr2_redis_rate_limiter() {
    let Some(url) = redis_url() else {
        eprintln!("wave9_pr2_redis_rate_limiter: REDIS_URL unset, skipping (CI must set it)");
        return;
    };
    let limiter = RateLimiter::new(url.clone());
    let key = format!("pr2-rl-{}", std::process::id());
    assert!(limiter.is_allowed(&key, 2, 60, false, "dev").await);
    assert!(limiter.is_allowed(&key, 2, 60, false, "dev").await);
    assert!(!limiter.is_allowed(&key, 2, 60, false, "dev").await);
    limiter.reset(&key).await;
    assert!(limiter.is_allowed(&key, 2, 60, false, "dev").await);

    let dead = RateLimiter::new("redis://127.0.0.1:6399/0".to_string());
    let dead_key = format!("pr2-rl-dead-{}", std::process::id());
    assert!(
        !dead.is_allowed(&dead_key, 5, 60, true, "production").await,
        "auth + production + dead Redis must fail closed"
    );
    assert!(
        dead.is_allowed(&dead_key, 5, 60, false, "dev").await,
        "non-auth + dev + dead Redis must fall back to memory"
    );
}

// ============================ /ready ============================

/// /ready reports `redis: "ok"` with a live Redis (sqlite DB, 200).
/// Skipped when `REDIS_URL` is unset.
#[tokio::test]
async fn wave9_pr2_ready_redis_ok() {
    let Some(url) = redis_url() else {
        eprintln!("wave9_pr2_ready_redis_ok: REDIS_URL unset, skipping (CI must set it)");
        return;
    };
    let state = sqlite_env(&url).await;
    let (status, body) = call_ready(state).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"].as_str(), Some("ready"));
    assert_eq!(body["db"].as_str(), Some("ok"));
    assert_eq!(body["redis"].as_str(), Some("ok"));
}

/// /ready reports `redis: "unavailable"` with a dead Redis — and is
/// still 200, because Redis never fails readiness. Always runs.
#[tokio::test]
async fn wave9_pr2_ready_redis_dead() {
    let state = sqlite_env("redis://127.0.0.1:6399/0").await;
    let (status, body) = call_ready(state).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"].as_str(), Some("ready"));
    assert_eq!(body["db"].as_str(), Some("ok"));
    assert_eq!(body["redis"].as_str(), Some("unavailable"));
}

// ============================ Postgres smoke ============================

/// The `vector` + `pg_trgm` extensions on Postgres: creatable, present,
/// and functional (vector cosine distance + trigram similarity). Gated
/// on `WAVE2_PG_URL`; the CI sequential step runs it against the
/// pgvector service image.
#[tokio::test]
async fn wave9_pr2_pg_smoke() {
    let Some(url) = std::env::var("WAVE2_PG_URL").ok().filter(|u| !u.is_empty()) else {
        eprintln!("wave9_pr2_pg_smoke: WAVE2_PG_URL unset, skipping");
        return;
    };
    let db = Db::connect(&url).await.expect("postgres connect");
    db.execute("CREATE EXTENSION IF NOT EXISTS vector", &[])
        .await
        .expect("CREATE EXTENSION vector");
    db.execute("CREATE EXTENSION IF NOT EXISTS pg_trgm", &[])
        .await
        .expect("CREATE EXTENSION pg_trgm");
    let row = db
        .fetch_one(
            "SELECT count(*) AS n FROM pg_extension WHERE extname IN ('vector', 'pg_trgm')",
            &[],
        )
        .await
        .expect("count extensions");
    assert_eq!(
        row.try_get::<i64, _>("n").expect("n"),
        2,
        "both vector and pg_trgm must be installed"
    );
    db.execute("DROP TABLE IF EXISTS pr2_vec_smoke", &[])
        .await
        .expect("drop leftover");
    db.execute(
        "CREATE TABLE pr2_vec_smoke (id integer PRIMARY KEY, e vector(3))",
        &[],
    )
    .await
    .expect("create vector table");
    db.execute("INSERT INTO pr2_vec_smoke VALUES (1, '[1,2,3]')", &[])
        .await
        .expect("insert vector");
    let row = db
        .fetch_one(
            "SELECT (e <=> '[1,2,3]'::vector)::float8 AS dist FROM pr2_vec_smoke WHERE id = 1",
            &[],
        )
        .await
        .expect("cosine distance");
    let dist = row.try_get::<f64, _>("dist").expect("dist");
    assert!(
        dist.abs() < 1e-9,
        "cosine distance of a vector to itself must be 0, got {dist}"
    );
    let row = db
        .fetch_one(
            "SELECT similarity('maturidi', 'maturidi')::float8 AS sim",
            &[],
        )
        .await
        .expect("trigram similarity");
    let sim = row.try_get::<f64, _>("sim").expect("sim");
    assert!(
        (sim - 1.0).abs() < 1e-9,
        "similarity of identical strings must be 1, got {sim}"
    );
    db.execute("DROP TABLE pr2_vec_smoke", &[])
        .await
        .expect("drop vector table");
}
