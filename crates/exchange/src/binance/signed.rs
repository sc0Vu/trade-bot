//! Binance account access: credentials, HMAC-SHA256 request signing, and
//! the HTTP transport behind a trait so tests never touch the network.
//!
//! Keys come only from the `BINANCE_API_KEY` / `BINANCE_API_SECRET`
//! environment variables (invariant 6). The secret never appears in
//! `Debug` output, logs or errors.

use crate::ExchangeError;
use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Env var holding the API key.
pub const API_KEY_VAR: &str = "BINANCE_API_KEY";
/// Env var holding the API secret.
pub const API_SECRET_VAR: &str = "BINANCE_API_SECRET";

/// API key and secret. `Debug` prints neither.
#[derive(Clone)]
pub struct Credentials {
    key: String,
    secret: String,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// Sends HTTP requests. Real code uses [`ReqwestTransport`]; tests use a
/// fake that records requests and returns canned bodies.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, base: &str, req: HttpRequest) -> Result<HttpResponse, ExchangeError>;
}

/// [`Transport`] over reqwest.
#[derive(Clone, Default)]
pub struct ReqwestTransport(reqwest::Client);

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
        let body = res
            .text()
            .await
            .map_err(|e| ExchangeError::Http(e.without_url().to_string()))?;
        Ok(HttpResponse { status, body })
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
        }
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
                         its IP whitelist and spot permission, and that testnet \
                         keys come from testnet.binance.vision"
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
        let path = req.path.clone();
        let res = self.transport.send(&self.base, req).await?;
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
            });
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
