//! Binance account access: credentials, HMAC-SHA256 request signing, and
//! the HTTP transport behind a trait so tests never touch the network.
//!
//! Keys come only from the `BINANCE_API_KEY` / `BINANCE_API_SECRET`
//! environment variables (invariant 6). The secret never appears in
//! `Debug` output, logs or errors.

use crate::ExchangeError;
use crate::rate_limit::{Kind, RateLimiter};
use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Env var holding the API key.
pub const API_KEY_VAR: &str = "BINANCE_API_KEY";
/// Env var holding the API secret.
pub const API_SECRET_VAR: &str = "BINANCE_API_SECRET";

/// API key and secret. `Debug` prints neither.
#[derive(Clone)]
pub struct Credentials {
    pub(super) key: String,
    pub(super) secret: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials { key: <redacted>, secret: <redacted> }")
    }
}

impl Credentials {
    pub fn new(key: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            secret: secret.into(),
        }
    }

    /// Reads `BINANCE_API_KEY` and `BINANCE_API_SECRET`. Missing or empty
    /// is an error naming the variable, never echoing a value.
    pub fn from_env() -> Result<Self, ExchangeError> {
        Self::from_vars(
            std::env::var(API_KEY_VAR).ok(),
            std::env::var(API_SECRET_VAR).ok(),
        )
    }

    fn from_vars(key: Option<String>, secret: Option<String>) -> Result<Self, ExchangeError> {
        let need = |v: Option<String>, name: &str| {
            v.filter(|s| !s.trim().is_empty())
                .ok_or_else(|| ExchangeError::Credentials(format!("{name} is not set")))
        };
        Ok(Self::new(
            need(key, API_KEY_VAR)?,
            need(secret, API_SECRET_VAR)?,
        ))
    }
}

/// Lowercase hex HMAC-SHA256 of `payload` keyed by `secret`: Binance's
/// `signature` parameter.
pub fn sign(secret: &str, payload: &str) -> String {
    // HMAC accepts keys of any length, so this can't fail.
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

/// One HTTP call, already fully built (query includes any signature).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: Method,
    pub path: String,
    /// URL-encoded query string, without `?`.
    pub query: String,
    /// Sent as `X-MBX-APIKEY` when present.
    pub api_key: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    /// `X-MBX-USED-WEIGHT-1M`: request weight Binance has counted this
    /// minute for the IP.
    pub used_weight_1m: Option<u64>,
    /// `X-MBX-ORDER-COUNT-10S`: orders Binance has counted in the last 10 s
    /// for the account.
    pub order_count_10s: Option<u64>,
    /// `Retry-After`, seconds, sent with 429 and 418.
    pub retry_after: Option<u64>,
}

/// After a 429 or 418 without `Retry-After`, hold requests this long.
const DEFAULT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// Feeds one response's rate-limit information back into `limiter`: the
/// venue's usage counts, and for 429 (limited) or 418 (IP banned) a hold on
/// every request until `Retry-After` passes.
pub(crate) fn observe(limiter: &RateLimiter, res: &HttpResponse) {
    if let Some(u) = res.used_weight_1m {
        limiter.observe_used(Kind::Weight, super::limits::WEIGHT_WINDOW, u);
    }
    if let Some(o) = res.order_count_10s {
        limiter.observe_used(Kind::Orders, super::limits::ORDER_WINDOW, o);
    }
    if matches!(res.status, 418 | 429) {
        let hold = res
            .retry_after
            .map_or(DEFAULT_BACKOFF, std::time::Duration::from_secs);
        limiter.block_for(hold);
        metrics::counter!(crate::metrics::RATE_LIMITED).increment(1);
        tracing::warn!(
            status = res.status,
            hold_secs = hold.as_secs(),
            "Binance rate limit hit; holding every request"
        );
    }
    if let Some(u) = limiter.used(Kind::Weight, super::limits::WEIGHT_WINDOW) {
        metrics::gauge!(crate::metrics::BINANCE_WEIGHT_USED).set(u as f64);
    }
}

/// Waits for room under `limiter` for one request; records the wait.
pub(crate) async fn admit(
    limiter: &RateLimiter,
    method: Method,
    path: &str,
    query: &str,
) -> Result<(), ExchangeError> {
    let waited = limiter
        .acquire(
            super::limits::weight(method, path, query),
            super::limits::orders(method, path),
        )
        .await
        .map_err(|e| ExchangeError::RateLimited(e.to_string()))?;
    if !waited.is_zero() {
        metrics::counter!(crate::metrics::RATE_LIMIT_WAITS).increment(1);
        tracing::info!(
            path,
            wait_ms = waited.as_millis() as u64,
            "rate limiter held a request"
        );
    }
    Ok(())
}

fn header_u64(res: &reqwest::Response, name: &str) -> Option<u64> {
    res.headers().get(name)?.to_str().ok()?.trim().parse().ok()
}

/// Sends HTTP requests. Real code uses [`ReqwestTransport`]; tests use a
/// fake that records requests and returns canned bodies.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, base: &str, req: HttpRequest) -> Result<HttpResponse, ExchangeError>;
}

/// [`Transport`] over reqwest. Every request is bounded by a timeout; one
/// that runs out fails with [`ExchangeError::Http`], which stops the run (a
/// submit that timed out may still have reached Binance; reconcile finds
/// it).
#[derive(Clone)]
pub struct ReqwestTransport(reqwest::Client);

impl ReqwestTransport {
    pub fn new(timeout: std::time::Duration) -> Result<Self, ExchangeError> {
        reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map(Self)
            .map_err(|e| ExchangeError::Http(format!("building HTTP client: {e}")))
    }
}

impl Default for ReqwestTransport {
    /// [`super::DEFAULT_HTTP_TIMEOUT`].
    fn default() -> Self {
        // `build` fails only if the TLS backend can't start, and then
        // `Client::new` panics with the same cause: never a client without
        // the timeout.
        Self::new(super::DEFAULT_HTTP_TIMEOUT).unwrap_or_else(|_| Self(reqwest::Client::new()))
    }
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn send(&self, base: &str, req: HttpRequest) -> Result<HttpResponse, ExchangeError> {
        let url = if req.query.is_empty() {
            format!("{base}{}", req.path)
        } else {
            format!("{base}{}?{}", req.path, req.query)
        };
        let mut b = match req.method {
            Method::Get => self.0.get(url),
            Method::Post => self.0.post(url),
            Method::Delete => self.0.delete(url),
        };
        if let Some(k) = &req.api_key {
            b = b.header("X-MBX-APIKEY", k);
        }
        let res = b
            .send()
            .await
            .map_err(|e| ExchangeError::Http(e.without_url().to_string()))?;
        let status = res.status().as_u16();
        let used_weight_1m = header_u64(&res, "x-mbx-used-weight-1m");
        let order_count_10s = header_u64(&res, "x-mbx-order-count-10s");
        let retry_after = header_u64(&res, "retry-after");
        let body = res
            .text()
            .await
            .map_err(|e| ExchangeError::Http(e.without_url().to_string()))?;
        Ok(HttpResponse {
            status,
            body,
            used_weight_1m,
            order_count_10s,
            retry_after,
        })
    }
}

/// Wall-clock milliseconds, injectable so signed requests are testable.
pub trait Clock: Send + Sync {
    fn now_millis(&self) -> i64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
}

/// Percent-encodes everything outside the URL unreserved set. Values here
/// are symbols, decimals and client order ids, so this is all Binance needs;
/// the signature is computed over exactly this encoded string.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={}", encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Client for signed (`USER_DATA` / `TRADE`) endpoints.
///
/// Appends `recvWindow` and `timestamp` (local clock plus the offset
/// measured by [`sync_time`](Self::sync_time)) to each request and signs
/// the full query. Venue errors (`{"code":..,"msg":..}`) become
/// [`ExchangeError::Venue`].
pub struct SignedClient {
    base: String,
    transport: Box<dyn Transport>,
    creds: Credentials,
    clock: Box<dyn Clock>,
    offset_ms: i64,
    recv_window_ms: u64,
    limiter: Arc<RateLimiter>,
}

impl SignedClient {
    pub fn new(
        base: impl Into<String>,
        transport: Box<dyn Transport>,
        creds: Credentials,
        clock: Box<dyn Clock>,
    ) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_owned(),
            transport,
            creds,
            clock,
            offset_ms: 0,
            recv_window_ms: 5000,
            limiter: Arc::new(RateLimiter::new(super::limits::default_limits())),
        }
    }

    /// Paces requests through `limiter` instead of a private one. Every
    /// client talking to one host should share one: Binance counts weight
    /// per IP.
    pub fn with_limiter(mut self, limiter: Arc<RateLimiter>) -> Self {
        self.limiter = limiter;
        self
    }

    /// Measures server time minus local time, so a skewed local clock
    /// doesn't get every signed request rejected (`-1021`).
    pub async fn sync_time(&mut self) -> Result<(), ExchangeError> {
        let before = self.clock.now_millis();
        let v = self.public(Method::Get, "/api/v3/time", &[]).await?;
        let server = v["serverTime"]
            .as_i64()
            .ok_or_else(|| ExchangeError::Decode("serverTime".into()))?;
        let after = self.clock.now_millis();
        self.offset_ms = server - (before + after) / 2;
        Ok(())
    }

    /// Server time minus local time, as [`sync_time`](Self::sync_time) last
    /// measured it: for signing requests sent outside this client.
    pub(crate) fn offset_ms(&self) -> i64 {
        self.offset_ms
    }

    /// Proves the key works before trading: a signed, read-only
    /// `GET /api/v3/account` that must succeed and report `canTrade`.
    ///
    /// Without it a bad key surfaces only on the first order, which on a
    /// slow strategy can be hours in. Key, signature and permission errors
    /// (`-2014`, `-2015`, `-1022`) become [`ExchangeError::Credentials`]
    /// with a hint; other failures pass through unchanged.
    pub async fn check_credentials(&self) -> Result<(), ExchangeError> {
        let account = self
            .signed(
                Method::Get,
                "/api/v3/account",
                &[("omitZeroBalances", "true".to_owned())],
            )
            .await
            .map_err(|e| match e {
                ExchangeError::Venue { code, msg } if [-2014, -2015, -1022].contains(&code) => {
                    ExchangeError::Credentials(format!(
                        "Binance refused the API key ({code}: {msg}); check the key, \
                         its IP whitelist and spot permission, and that it belongs to \
                         the configured exchange.network (testnet.binance.vision, \
                         demo.binance.com and binance.com keys are not interchangeable)"
                    ))
                }
                other => other,
            })?;
        match account["canTrade"].as_bool() {
            Some(true) => Ok(()),
            Some(false) => Err(ExchangeError::Credentials(
                "API key works but the account cannot trade spot".into(),
            )),
            None => Err(ExchangeError::Decode("account: canTrade".into())),
        }
    }

    /// Unsigned request (no key, no signature).
    pub async fn public(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<Value, ExchangeError> {
        self.call(HttpRequest {
            method,
            path: path.into(),
            query: query(params),
            api_key: None,
        })
        .await
    }

    /// Signed request: `params`, then `recvWindow` and `timestamp`, then
    /// `signature` over all of it.
    pub async fn signed(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<Value, ExchangeError> {
        let mut all = params.to_vec();
        all.push(("recvWindow", self.recv_window_ms.to_string()));
        all.push((
            "timestamp",
            (self.clock.now_millis() + self.offset_ms).to_string(),
        ));
        let q = query(&all);
        let sig = sign(&self.creds.secret, &q);
        self.call(HttpRequest {
            method,
            path: path.into(),
            query: format!("{q}&signature={sig}"),
            api_key: Some(self.creds.key.clone()),
        })
        .await
    }

    async fn call(&self, req: HttpRequest) -> Result<Value, ExchangeError> {
        let r = self.call_inner(req.clone()).await;
        if let Err(e) = &r {
            let code = match e {
                ExchangeError::Venue { code, .. } => code.to_string(),
                _ => "http".to_owned(),
            };
            metrics::counter!(
                crate::metrics::BINANCE_ERRORS,
                "path" => req.path.clone(),
                "code" => code,
            )
            .increment(1);
        }
        r
    }

    async fn call_inner(&self, req: HttpRequest) -> Result<Value, ExchangeError> {
        let path = req.path.clone();
        let method = match req.method {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
        };
        admit(&self.limiter, req.method, &req.path, &req.query).await?;
        let started = std::time::Instant::now();
        let sent = self.transport.send(&self.base, req).await;
        if let Ok(res) = &sent {
            observe(&self.limiter, res);
        }
        metrics::histogram!(
            crate::metrics::BINANCE_REQUEST,
            "method" => method,
            "path" => path.clone(),
        )
        .record(started.elapsed().as_secs_f64());
        let res = sent?;
        let body: Value = serde_json::from_str(&res.body)
            .map_err(|e| ExchangeError::Decode(format!("{path}: HTTP {}: {e}", res.status)))?;
        if res.status >= 400 {
            return Err(match (body["code"].as_i64(), body["msg"].as_str()) {
                (Some(code), Some(msg)) => ExchangeError::Venue {
                    code,
                    msg: msg.to_owned(),
                },
                _ => ExchangeError::Http(format!("{path}: HTTP {}", res.status)),
            });
        }
        Ok(body)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Records every request; answers from a queue of canned responses.
    #[derive(Clone, Default)]
    pub struct FakeTransport {
        pub sent: Arc<Mutex<Vec<HttpRequest>>>,
        pub replies: Arc<Mutex<VecDeque<HttpResponse>>>,
    }

    impl FakeTransport {
        pub fn reply(&self, status: u16, body: &str) {
            self.replies.lock().unwrap().push_back(HttpResponse {
                status,
                body: body.into(),
                ..HttpResponse::default()
            });
        }
        /// Queues a full response, rate-limit headers included.
        pub fn reply_with(&self, res: HttpResponse) {
            self.replies.lock().unwrap().push_back(res);
        }
        pub fn sent(&self) -> Vec<HttpRequest> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn send(&self, _base: &str, req: HttpRequest) -> Result<HttpResponse, ExchangeError> {
            self.sent.lock().unwrap().push(req.clone());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ExchangeError::Http(format!("no canned reply for {}", req.path)))
        }
    }

    pub struct FixedClock(pub i64);

    impl Clock for FixedClock {
        fn now_millis(&self) -> i64 {
            self.0
        }
    }

    /// Value of `key` in a query string.
    pub fn param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeTransport, FixedClock};
    use super::*;

    // Example from Binance's spot API docs ("SIGNED endpoint examples").
    const DOC_SECRET: &str = "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
    const DOC_KEY: &str = "vmPUZE6mv9SD5VNHk4HlWFsOr6aKE2zvsw0MuIgwCIPy6utIco14y7Ju91duEh8A";
    const DOC_QUERY: &str = "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559";
    const DOC_SIG: &str = "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71";

    #[test]
    fn signature_matches_binance_docs() {
        assert_eq!(sign(DOC_SECRET, DOC_QUERY), DOC_SIG);
    }

    #[tokio::test]
    async fn signed_request_reproduces_doc_example() {
        let t = FakeTransport::default();
        t.reply(200, "{}");
        let c = SignedClient::new(
            "https://testnet.binance.vision",
            Box::new(t.clone()),
            Credentials::new(DOC_KEY, DOC_SECRET),
            Box::new(FixedClock(1_499_827_319_559)),
        );
        let params = [
            ("symbol", "LTCBTC".to_owned()),
            ("side", "BUY".to_owned()),
            ("type", "LIMIT".to_owned()),
            ("timeInForce", "GTC".to_owned()),
            ("quantity", "1".to_owned()),
            ("price", "0.1".to_owned()),
        ];
        c.signed(Method::Post, "/api/v3/order", &params)
            .await
            .unwrap();
        let req = &t.sent()[0];
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.query, format!("{DOC_QUERY}&signature={DOC_SIG}"));
        assert_eq!(req.api_key.as_deref(), Some(DOC_KEY));
    }

    #[tokio::test]
    async fn venue_error_is_surfaced_with_code() {
        let t = FakeTransport::default();
        t.reply(
            400,
            r#"{"code":-2010,"msg":"Account has insufficient balance for requested action."}"#,
        );
        let c = SignedClient::new(
            "x",
            Box::new(t),
            Credentials::new("k", "s"),
            Box::new(FixedClock(0)),
        );
        let err = c
            .signed(Method::Post, "/api/v3/order", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, ExchangeError::Venue { code: -2010, .. }));
    }

    /// Every request lands in the latency histogram by path; a failure is
    /// counted under Binance's code.
    #[tokio::test]
    async fn requests_and_errors_are_recorded() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let t = FakeTransport::default();
        t.reply(200, "{}");
        t.reply(400, r#"{"code":-2010,"msg":"insufficient balance"}"#);
        let c = client(&t);
        c.signed(Method::Get, "/api/v3/account", &[]).await.unwrap();
        c.signed(Method::Post, "/api/v3/order", &[])
            .await
            .unwrap_err();

        let snap = snapshotter.snapshot().into_vec();
        let has = |name: &str, labels: &[(&str, &str)]| {
            snap.iter().find(|(k, _, _, _)| {
                k.key().name() == name
                    && labels.iter().all(|(lk, lv)| {
                        k.key().labels().any(|l| l.key() == *lk && l.value() == *lv)
                    })
            })
        };
        assert!(matches!(
            has(crate::metrics::BINANCE_REQUEST, &[("method", "GET"), ("path", "/api/v3/account")]),
            Some((_, _, _, DebugValue::Histogram(h))) if h.len() == 1
        ));
        assert!(matches!(
            has(
                crate::metrics::BINANCE_ERRORS,
                &[("path", "/api/v3/order"), ("code", "-2010")]
            ),
            Some((_, _, _, DebugValue::Counter(1)))
        ));
    }

    #[tokio::test]
    async fn time_sync_offsets_timestamp() {
        let t = FakeTransport::default();
        t.reply(200, r#"{"serverTime":1000500}"#);
        t.reply(200, "{}");
        let mut c = SignedClient::new(
            "x",
            Box::new(t.clone()),
            Credentials::new("k", "s"),
            Box::new(FixedClock(1_000_000)),
        );
        c.sync_time().await.unwrap();
        c.signed(Method::Get, "/api/v3/account", &[]).await.unwrap();
        assert_eq!(
            fake::param(&t.sent()[1].query, "timestamp"),
            Some("1000500")
        );
    }

    fn client(t: &FakeTransport) -> SignedClient {
        SignedClient::new(
            "x",
            Box::new(t.clone()),
            Credentials::new("k", "s"),
            Box::new(FixedClock(0)),
        )
    }

    #[tokio::test]
    async fn credential_check_reads_account() {
        let t = FakeTransport::default();
        t.reply(200, r#"{"canTrade":true,"balances":[]}"#);
        client(&t).check_credentials().await.unwrap();
        let req = &t.sent()[0];
        assert_eq!(
            (req.method, req.path.as_str()),
            (Method::Get, "/api/v3/account")
        );
        assert!(fake::param(&req.query, "signature").is_some());
    }

    #[tokio::test]
    async fn rejected_key_is_a_credentials_error() {
        // The reply testnet gave for an unusable key.
        let t = FakeTransport::default();
        t.reply(
            401,
            r#"{"code":-2015,"msg":"Invalid API-key, IP, or permissions for action."}"#,
        );
        let err = client(&t).check_credentials().await.unwrap_err();
        let ExchangeError::Credentials(msg) = err else {
            panic!("{err:?}")
        };
        assert!(msg.contains("-2015"), "{msg}");
    }

    #[tokio::test]
    async fn account_that_cannot_trade_is_refused() {
        let t = FakeTransport::default();
        t.reply(200, r#"{"canTrade":false}"#);
        let err = client(&t).check_credentials().await.unwrap_err();
        assert!(matches!(err, ExchangeError::Credentials(_)));
    }

    #[tokio::test]
    async fn other_failures_pass_through() {
        let t = FakeTransport::default();
        t.reply(
            400,
            r#"{"code":-1021,"msg":"Timestamp outside recvWindow."}"#,
        );
        let err = client(&t).check_credentials().await.unwrap_err();
        assert!(matches!(err, ExchangeError::Venue { code: -1021, .. }));
    }

    /// A server that accepts the connection and never answers: the request
    /// must fail at the timeout instead of hanging.
    #[tokio::test]
    async fn stalled_server_times_out() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hold = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(conn);
        });
        let t = ReqwestTransport::new(std::time::Duration::from_millis(200)).unwrap();
        let started = std::time::Instant::now();
        let req = HttpRequest {
            method: Method::Get,
            path: "/api/v3/time".into(),
            query: String::new(),
            api_key: None,
        };
        let err = t.send(&format!("http://{addr}"), req).await.unwrap_err();
        assert!(matches!(err, ExchangeError::Http(_)), "{err:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        hold.join().unwrap();
    }

    fn limited(t: &FakeTransport, weight_budget: u64) -> SignedClient {
        use crate::rate_limit::Limit;
        client(t).with_limiter(Arc::new(RateLimiter::new([Limit {
            kind: Kind::Weight,
            window: std::time::Duration::from_secs(60),
            budget: weight_budget,
        }])))
    }

    /// Two `account` calls (weight 20 each) against a 30-per-minute budget:
    /// the second waits the full minute.
    #[tokio::test(start_paused = true)]
    async fn requests_wait_for_rate_limit_room() {
        let t = FakeTransport::default();
        t.reply(200, "{}");
        t.reply(200, "{}");
        let c = limited(&t, 30);
        let start = tokio::time::Instant::now();
        c.signed(Method::Get, "/api/v3/account", &[]).await.unwrap();
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
        c.signed(Method::Get, "/api/v3/account", &[]).await.unwrap();
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(60));
    }

    /// Binance says 4790 of the minute's weight is used (someone else on the
    /// IP): with the default 4800 budget the next 20-weight call waits.
    #[tokio::test(start_paused = true)]
    async fn binance_usage_header_is_respected() {
        let t = FakeTransport::default();
        t.reply_with(HttpResponse {
            status: 200,
            body: "{}".into(),
            used_weight_1m: Some(4790),
            ..HttpResponse::default()
        });
        t.reply(200, "{}");
        let c = client(&t);
        let start = tokio::time::Instant::now();
        c.signed(Method::Get, "/api/v3/time", &[]).await.unwrap();
        c.signed(Method::Get, "/api/v3/account", &[]).await.unwrap();
        assert!(start.elapsed() > std::time::Duration::ZERO);
    }

    /// A 429 with Retry-After 30: the call fails (the run stops), and every
    /// later request, the exit close-out's included, waits the 30 s out
    /// instead of hammering Binance into an IP ban.
    #[tokio::test(start_paused = true)]
    async fn too_many_requests_holds_everything_for_retry_after() {
        let t = FakeTransport::default();
        t.reply_with(HttpResponse {
            status: 429,
            body: r#"{"code":-1003,"msg":"Too many requests"}"#.into(),
            retry_after: Some(30),
            ..HttpResponse::default()
        });
        t.reply(200, "{}");
        let c = client(&t);
        let err = c
            .signed(Method::Get, "/api/v3/account", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, ExchangeError::Venue { code: -1003, .. }));
        assert!(!err.is_order_rejection(), "rate limiting stops the run");
        let start = tokio::time::Instant::now();
        c.signed(Method::Get, "/api/v3/time", &[]).await.unwrap();
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_that_can_never_fit_is_refused() {
        let t = FakeTransport::default();
        let c = limited(&t, 10);
        let err = c
            .signed(Method::Get, "/api/v3/account", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, ExchangeError::RateLimited(_)));
        assert!(t.sent().is_empty(), "never sent");
    }

    #[test]
    fn credentials_never_print_and_must_be_set() {
        let c = Credentials::new("my-key", "my-secret");
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("my-key") && !dbg.contains("my-secret"));
        let err = Credentials::from_vars(Some("k".into()), Some("  ".into())).unwrap_err();
        assert!(err.to_string().contains(API_SECRET_VAR));
        assert!(Credentials::from_vars(None, Some("s".into())).is_err());
    }

    #[test]
    fn query_encoding_escapes_reserved_chars() {
        assert_eq!(
            query(&[("newClientOrderId", "a:b/c".into())]),
            "newClientOrderId=a%3Ab%2Fc"
        );
    }
}
