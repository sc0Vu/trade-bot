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

    /// The two keys (api, user), for the WebSocket's `Authenticate`.
    pub(crate) fn parts(&self) -> (&str, &str) {
        (&self.api_key, &self.user_key)
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

/// The published per-minute limit at `HEADROOM`, and at most one request a
/// second: eToro answered 429 to a startup that spent well under a
/// minute's budget in a few seconds (2026-11-19), so bursts are spread out.
fn limiter(per_minute: u64) -> Arc<RateLimiter> {
    Arc::new(RateLimiter::new([
        Limit::with_headroom(Kind::Weight, Duration::from_secs(60), per_minute, HEADROOM),
        Limit {
            kind: Kind::Weight,
            window: Duration::from_secs(1),
            budget: 1,
        },
    ]))
}

/// Tries per request when eToro answers 429: it is waited out (its
/// `Retry-After`, else `BACKOFF`) and sent again, with the same
/// `x-request-id` (eToro deduplicates by it, so an order is never doubled).
const TRIES_429: u32 = 3;

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

    /// The keys, for the private WebSocket feed.
    pub(crate) fn keys(&self) -> &EtoroKeys {
        &self.keys
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

    /// PATCH of a JSON body (a position's take-profit).
    pub async fn patch(
        &self,
        pool: Pool,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<Reply, ExchangeError> {
        self.send(pool, reqwest::Method::PATCH, path, &[], Some(body), None)
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
        let id = request_id.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);
        let mut tries = 0;
        loop {
            tries += 1;
            limiter
                .acquire(1, 0)
                .await
                .map_err(|e| ExchangeError::RateLimited(e.to_string()))?;
            let res = retry_transport(TRANSPORT_TRIES, Duration::from_millis(500), || {
                let mut req = self
                    .http
                    .request(method.clone(), format!("{}{path}", self.base))
                    .query(query)
                    .header("x-api-key", &self.keys.api_key)
                    .header("x-user-key", &self.keys.user_key)
                    .header("x-request-id", &id)
                    .header("accept", "application/json");
                if let Some(b) = body {
                    req = req
                        .header("content-type", "application/json")
                        .body(b.to_string());
                }
                req.send()
            })
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
                if tries < TRIES_429 {
                    tracing::warn!(
                        path,
                        wait_secs = wait.as_secs(),
                        "eToro rate limit (429); waiting, then retrying"
                    );
                    continue;
                }
            }
            let body = res
                .text()
                .await
                .map_err(|e| ExchangeError::Http(e.without_url().to_string()))?;
            return Ok(Reply { status, body });
        }
    }
}

/// Attempts per request when the connection fails before eToro answers.
const TRANSPORT_TRIES: u32 = 3;

/// Runs `attempt` up to `tries` times while it fails, waiting `first`,
/// then twice as long, between tries. For transport failures only (the
/// caller retries nothing eToro answered): seen about once an hour on the
/// demo account (2026-11-18). Safe for orders too: every try carries the
/// same `x-request-id`, which eToro deduplicates by.
pub(crate) async fn retry_transport<T, E, F, Fut>(
    tries: u32,
    first: Duration,
    mut attempt: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut wait = first;
    let mut n = 1;
    loop {
        match attempt().await {
            Ok(v) => return Ok(v),
            Err(e) if n < tries => {
                tracing::warn!(error = %e, attempt = n, "eToro request failed before an answer; retrying");
                tokio::time::sleep(wait).await;
                wait *= 2;
                n += 1;
            }
            Err(e) => return Err(e),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Two failures then success: answered on the third try. Three
    /// failures: the third error comes back.
    #[tokio::test]
    async fn transport_failures_are_retried_three_times() {
        let calls = AtomicU32::new(0);
        let r: Result<u32, String> = retry_transport(3, Duration::ZERO, || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n < 3 {
                    Err(format!("drop {n}"))
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(r, Ok(3));
        let calls = AtomicU32::new(0);
        let r: Result<u32, String> = retry_transport(3, Duration::ZERO, || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move { Err(format!("drop {n}")) }
        })
        .await;
        assert_eq!(r, Err("drop 3".to_owned()));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// A local server answering each connection with the next canned
    /// status (and `Connection: close`), counting the requests it served.
    fn serve(statuses: Vec<u16>) -> (String, std::thread::JoinHandle<usize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut served = 0;
            for status in statuses {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf);
                let (reason, body) = if status == 429 {
                    ("Too Many Requests", r#"{"errorCode":"TooManyRequests"}"#)
                } else {
                    ("OK", r#"{"ok":true}"#)
                };
                let reply = format!(
                    "HTTP/1.1 {status} {reason}\r\nRetry-After: 0\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                conn.write_all(reply.as_bytes()).unwrap();
                served += 1;
            }
            served
        });
        (format!("http://{addr}"), handle)
    }

    fn client(base: &str) -> EtoroClient {
        EtoroClient::new(
            base,
            EtoroKeys::new("k", "u"),
            Account::Demo,
            Duration::from_secs(5),
        )
    }

    /// A 429 (Retry-After 0) is waited out and sent again: the second
    /// answer comes back. Requests are a second apart (the per-second cap).
    #[tokio::test]
    async fn a_429_is_retried_after_the_wait() {
        let (base, server) = serve(vec![429, 200]);
        let started = std::time::Instant::now();
        let body = client(&base).get(Pool::Info, "/x", &[]).await.unwrap();
        assert_eq!(body, r#"{"ok":true}"#);
        assert_eq!(server.join().unwrap(), 2);
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "paced a second apart"
        );
    }

    /// Three 429s in a row: the third comes back as the error.
    #[tokio::test]
    async fn three_429s_give_up() {
        let (base, server) = serve(vec![429, 429, 429]);
        let err = client(&base).get(Pool::Info, "/x", &[]).await.unwrap_err();
        assert!(err.to_string().contains("429"), "{err}");
        assert_eq!(server.join().unwrap(), 3);
    }

    #[test]
    fn each_pool_allows_its_minute_budget_but_one_request_a_second() {
        let windows: Vec<(u64, u64)> = limiter(60)
            .limits()
            .iter()
            .map(|l| (l.window.as_secs(), l.budget))
            .collect();
        assert_eq!(windows, vec![(60, 48), (1, 1)]);
    }
}
