//! Distributed rate limiting — mirrors `app/core/rate_limit.py`.
//!
//! Primary path: Redis `INCR` + `PEXPIRE` on first hit (atomic window).
//! On Redis failure: auth endpoints fail **closed** in production and
//! fall back to an in-memory sliding window in dev/staging/test;
//! non-auth endpoints always fall back to memory. After a Redis error
//! the limiter stays degraded for 30 s (`redis_retry_cooldown_s`) before
//! probing Redis again, so a dead Redis does not add latency to every
//! request and a recovered Redis cannot reset quotas.

use std::{
    collections::{HashMap, VecDeque},
    sync::atomic::{AtomicI64, Ordering},
    time::{Duration, Instant},
};

use tokio::sync::Mutex;

pub struct RateLimiter {
    mem_hits: Mutex<HashMap<String, VecDeque<Instant>>>,
    redis_url: String,
    redis_dead_since: AtomicI64,
}

impl RateLimiter {
    pub fn new(redis_url: String) -> Self {
        Self {
            mem_hits: Mutex::new(HashMap::new()),
            redis_url,
            redis_dead_since: AtomicI64::new(0),
        }
    }

    fn redis_cooling_down(&self) -> bool {
        let dead = self.redis_dead_since.load(Ordering::Relaxed);
        if dead == 0 {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        now - dead < 30
    }

    fn mark_redis_dead(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.redis_dead_since.store(now, Ordering::Relaxed);
    }

    async fn redis_is_allowed(&self, key: &str, limit: u32, window_s: u64) -> Option<bool> {
        if self.redis_cooling_down() {
            return None;
        }
        let check = async {
            let client = redis::Client::open(self.redis_url.as_str()).ok()?;
            let mut conn = client.get_multiplexed_async_connection().await.ok()?;
            let full_key = format!("rl:maturidi:{key}");
            let count: i64 = redis::cmd("INCR")
                .arg(&full_key)
                .query_async(&mut conn)
                .await
                .ok()?;
            if count == 1 {
                let _: () = redis::cmd("PEXPIRE")
                    .arg(&full_key)
                    .arg(window_s * 1000)
                    .query_async(&mut conn)
                    .await
                    .ok()?;
            }
            Some(count <= limit as i64)
        };
        match tokio::time::timeout(Duration::from_millis(1000), check).await {
            Ok(Some(allowed)) => Some(allowed),
            _ => {
                self.mark_redis_dead();
                None
            }
        }
    }

    async fn mem_is_allowed(&self, key: &str, limit: u32, window_s: u64) -> bool {
        let mut map = self.mem_hits.lock().await;
        let now = Instant::now();
        let window = Duration::from_secs(window_s);
        let hits = map.entry(key.to_string()).or_default();
        while let Some(front) = hits.front() {
            if now.duration_since(*front) > window {
                hits.pop_front();
            } else {
                break;
            }
        }
        if hits.len() >= limit as usize {
            return false;
        }
        hits.push_back(now);
        true
    }

    async fn mem_reset(&self, key: &str) {
        let mut map = self.mem_hits.lock().await;
        map.remove(key);
    }

    /// `is_allowed(key, limit, window_s, auth=True)` — see module docs.
    pub async fn is_allowed(
        &self,
        key: &str,
        limit: u32,
        window_s: u64,
        auth: bool,
        app_env: &str,
    ) -> bool {
        if let Some(allowed) = self.redis_is_allowed(key, limit, window_s).await {
            return allowed;
        }
        if auth && app_env == "production" {
            return false;
        }
        self.mem_is_allowed(key, limit, window_s).await
    }

    /// `reset(key)` — clears the counter after a successful login.
    pub async fn reset(&self, key: &str) {
        self.mem_reset(key).await;
        if let Ok(client) = redis::Client::open(self.redis_url.as_str()) {
            if let Ok(mut conn) = client.get_multiplexed_async_connection().await {
                let _: () = redis::cmd("DEL")
                    .arg(format!("rl:maturidi:{key}"))
                    .query_async(&mut conn)
                    .await
                    .unwrap_or(());
            }
        }
    }
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("redis_url", &self.redis_url)
            .finish_non_exhaustive()
    }
}
