//! Authenticated eToro REST client (see the module docs of [`super`]).

use crate::ExchangeError;
use crate::rate_limit::{Kind, Limit, RateLimiter};
use std::sync::Arc;
use std::time::Duration;

/// Which eToro account a user key trades.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Account {
    /// Virtual money.
    #[default]
    Demo,
    Real,
}

impl Account {
    /// `real` with `/demo` spliced in for a demo account: trading and
    /// portfolio paths differ only by it (`.../execution/orders` →
    /// `.../execution/demo/orders`, `.../info/real/pnl` → `.../info/demo/pnl`).
    pub fn path(self, real: &str, demo: &str) -> String {
        match self {
            Self::Real => real.to_owned(),
            Self::Demo => demo.to_owned(),
        }
    }
}

/// The two keys every request carries. Debug never shows them.
#[derive(Clone)]
pub struct EtoroKeys {
    api_key: String,
    user_key: String,
}

/// Env var with the application's public API key.
pub const API_KEY_VAR: &str = "ETORO_PUBLIC_KEY";
/// Env var with the account's user key.
pub const USER_KEY_VAR: &str = "ETORO_PRIVATE_KEY";

impl std::fmt::Debug for EtoroKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EtoroKeys { <redacted> }")
    }
}

impl EtoroKeys {
    pub fn new(api_key: impl Into<String>, user_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            user_key: user_key.into(),
        }
    }

    /// From [`API_KEY_VAR`] and [`USER_KEY_VAR`]; missing or empty is an
    /// error naming the variable, never echoing a value.
    pub fn from_env() -> Result<Self, ExchangeError> {
        let need = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| ExchangeError::Credentials(format!("{name} is not set")))
        };
        Ok(Self::new(need(API_KEY_VAR)?, need(USER_KEY_VAR)?))
    }
}

/// Which rate-limit pool a request draws from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pool {
    /// Market data (instruments, rates, candles): 120/min.
    MarketData,
    /// Order execution (open, close, cancel): 20/min.
    Trading,
    /// Everything else (portfolio, lookups, history): 60/min.
    Info,
}

/// Share of each published limit the client uses.
const HEADROOM: f64 = 0.8;
/// Wait after a 429 without `Retry-After`.
const BACKOFF: Duration = Duration::from_secs(60);

fn limiter(per_minute: u64) -> Arc<RateLimiter> {
    Arc::new(RateLimiter::new([Limit::with_headroom(
        Kind::Weight,
        Duration::from_secs(60),
        per_minute,
        HEADROOM,
    )]))
}

/// See the module docs. Cloning shares the rate limiters (eToro counts
/// per user key).
#[derive(Clone)]
pub struct EtoroClient {
    http: reqwest::Client,
    base: String,
    keys: EtoroKeys,
    account: Account,
    market: Arc<RateLimiter>,
    trading: Arc<RateLimiter>,
    info: Arc<RateLimiter>,
}

/// A reply: status and body text (money parsed from the text by callers).
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl EtoroClient {
    pub fn new(
        base: impl Into<String>,
        keys: EtoroKeys,
        account: Account,
        timeout: Duration,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("trade-bot/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
            keys,
            account,
            market: limiter(120),
            trading: limiter(20),
            info: limiter(60),
        }
    }

    pub fn account(&self) -> Account {
        self.account
    }

    /// GET; the body text of a 2xx reply, else an error with the status
    /// and body.
    pub async fn get(
        &self,
        pool: Pool,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<String, ExchangeError> {
        let r = self
            .send(pool, reqwest::Method::GET, path, query, None, None)
            .await?;
        ok(path, r)
    }

    /// POST of a JSON body under `request_id` (an order's idempotency
    /// key: resending the same id can't place it twice).
    pub async fn post(
        &self,
        pool: Pool,
        path: &str,
        body: &serde_json::Value,
        request_id: &str,
    ) -> Result<Reply, ExchangeError> {
        self.send(
            pool,
            reqwest::Method::POST,
            path,
            &[],
            Some(body),
            Some(request_id),
        )
        .await
    }

    pub async fn delete(&self, pool: Pool, path: &str) -> Result<Reply, ExchangeError> {
        self.send(pool, reqwest::Method::DELETE, path, &[], None, None)
            .await
    }

    async fn send(
        &self,
        pool: Pool,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&serde_json::Value>,
        request_id: Option<&str>,
    ) -> Result<Reply, ExchangeError> {
        let limiter = match pool {
            Pool::MarketData => &self.market,
            Pool::Trading => &self.trading,
            Pool::Info => &self.info,
        };
        limiter
            .acquire(1, 0)
            .await
            .map_err(|e| ExchangeError::RateLimited(e.to_string()))?;
        let id = request_id.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .query(query)
            .header("x-api-key", &self.keys.api_key)
            .header("x-user-key", &self.keys.user_key)
            .header("x-request-id", id)
            .header("accept", "application/json");
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(b.to_string());
        }
        let res = req
            .send()
            .await
            .map_err(|e| ExchangeError::Http(e.without_url().to_string()))?;
        let status = res.status().as_u16();
        if status == 429 {
            let wait = res
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok())
                .map_or(BACKOFF, Duration::from_secs);
            limiter.block_for(wait);
            metrics::counter!(crate::metrics::RATE_LIMITED).increment(1);
        }
        let body = res
            .text()
            .await
            .map_err(|e| ExchangeError::Http(e.without_url().to_string()))?;
        Ok(Reply { status, body })
    }
}

/// The body of a 2xx reply; any other status is an error carrying it.
pub(crate) fn ok(path: &str, r: Reply) -> Result<String, ExchangeError> {
    if (200..300).contains(&r.status) {
        Ok(r.body)
    } else {
        Err(ExchangeError::Http(format!(
            "{path}: HTTP {}: {}",
            r.status, r.body
        )))
    }
}
