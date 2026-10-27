//! Binance user data stream over the WebSocket API: which orders changed,
//! never the fills themselves.
//!
//! The listenKey stream (`POST /api/v3/userDataStream`) is gone: HTTP 410 on
//! testnet, demo and mainnet (checked 2026-10-27). Its replacement is one
//! signed `userDataStream.subscribe.signature` request on a WebSocket API
//! connection (HMAC keys accepted, checked on demo the same day), after which
//! Binance pushes an `executionReport` for every change to the account's
//! orders.
//!
//! Only the order id is kept. The broker still reads status and trades over
//! REST for the orders the stream names, so fills, fee valuation and the
//! "terminal and trades sum to `executedQty`" rule have one code path,
//! stream or no stream. What the stream buys is request weight: an idle
//! order costs nothing instead of two signed requests per event.
//!
//! A signature subscription has no key to keep alive and no expiry of its
//! own; it lives as long as its connection. Binance ends a WebSocket API
//! connection at 24 hours and announces maintenance with `serverShutdown`,
//! so the task renews ahead of both: a new connection and subscription at
//! [`Timing::max_age`] (23 h) or on `serverShutdown`, straight away, without
//! the failure backoff. Renewing is a reconnect like any other, so the
//! broker polls every order once after it.
//!
//! Nothing depends on the stream being complete. Before the first
//! subscription, after every reconnect, while it is down, after a message
//! that can't be read, and every [`FULL_POLL_EVERY`] regardless, the broker
//! polls every order ([`Changes::Unknown`]). A stream that never connects
//! leaves the broker polling every order every event, as it did before
//! this module existed; a stream failure is a warning, never a stopped run.

use super::signed::{Clock, Credentials, sign};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// Longest the broker trusts the stream alone: past this since the last
/// full poll, every order is polled again. Bounds the cost of a stream that
/// looks alive but has stopped delivering.
pub const FULL_POLL_EVERY: Duration = Duration::from_secs(60);

/// Spot WebSocket API, mainnet. Reached only through the live gate.
pub const LIVE_WS_API_URL: &str = "wss://ws-api.binance.com:443/ws-api/v3";
/// Spot WebSocket API, testnet.
pub const TESTNET_WS_API_URL: &str = "wss://ws-api.testnet.binance.vision/ws-api/v3";
/// Spot WebSocket API, demo trading.
pub const DEMO_WS_API_URL: &str = "wss://demo-ws-api.binance.com/ws-api/v3";

/// Request id of the subscribe call, to match its reply.
const SUBSCRIBE_ID: &str = "subscribe";

/// What the broker learns from the stream since it last asked.
#[derive(Debug, PartialEq, Eq)]
pub enum Changes {
    /// The stream can't vouch for anything (down, just (re)subscribed, or
    /// a message it couldn't read): poll every order.
    Unknown,
    /// Binance order ids with an `executionReport` since the last ask. An
    /// order not named here hasn't changed.
    Orders(HashSet<i64>),
}

#[derive(Default)]
struct News {
    /// Successful subscriptions so far; 0 = never.
    epoch: u64,
    /// Subscribed right now.
    live: bool,
    touched: HashSet<i64>,
    /// A message couldn't be read: it may have named an order.
    garbled: bool,
}

fn lock(news: &Mutex<News>) -> MutexGuard<'_, News> {
    // Every update leaves `News` consistent, so a poisoned lock is usable.
    news.lock().unwrap_or_else(|e| e.into_inner())
}

/// Timings of a stream task; tests shorten them.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// First reconnect delay, doubling per failed attempt.
    pub retry_min: Duration,
    /// Cap on the reconnect delay.
    pub retry_max: Duration,
    /// Binance pings every 20 s; nothing at all for this long = dead socket.
    pub silence: Duration,
    /// Longest connect + subscribe may take.
    pub handshake: Duration,
    /// Age at which a subscribed connection is replaced, ahead of
    /// Binance's 24-hour cut-off.
    pub max_age: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(300),
            silence: Duration::from_secs(60),
            handshake: Duration::from_secs(10),
            max_age: Duration::from_secs(23 * 3600),
        }
    }
}

/// A background task holding one WebSocket API connection subscribed to
/// the account's user data stream, reconnecting forever with backoff.
/// Dropping it stops the task.
pub struct UserStream {
    news: Arc<Mutex<News>>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// The subscription the broker last fully polled under.
    seen_epoch: u64,
}

impl UserStream {
    /// Starts the task against `url` (e.g. [`DEMO_WS_API_URL`]). Requests
    /// are timestamped with `clock` plus `offset_ms`, the server-time offset
    /// the REST client measured.
    pub fn spawn(
        url: String,
        creds: Credentials,
        clock: Box<dyn Clock>,
        offset_ms: i64,
        timing: Timing,
    ) -> Self {
        let news = Arc::new(Mutex::new(News::default()));
        let task = tokio::spawn(run(url, creds, clock, offset_ms, timing, news.clone()));
        Self {
            news,
            task: Some(task),
            seen_epoch: 0,
        }
    }

    /// Takes what arrived since the last call. [`Changes::Unknown`] once per
    /// new subscription, and whenever the stream is down.
    pub fn take(&mut self) -> Changes {
        let mut n = lock(&self.news);
        let touched = std::mem::take(&mut n.touched);
        let garbled = std::mem::take(&mut n.garbled);
        if !n.live || n.epoch != self.seen_epoch || garbled {
            self.seen_epoch = n.epoch;
            Changes::Unknown
        } else {
            Changes::Orders(touched)
        }
    }

    /// A stream with no task, driven by the test through the returned
    /// handle.
    #[cfg(test)]
    pub(crate) fn manual() -> (Self, ManualStream) {
        let news = Arc::new(Mutex::new(News::default()));
        (
            Self {
                news: news.clone(),
                task: None,
                seen_epoch: 0,
            },
            ManualStream(news),
        )
    }
}

impl Drop for UserStream {
    fn drop(&mut self) {
        if let Some(t) = &self.task {
            t.abort();
        }
    }
}

/// Test handle playing Binance's side of a [`UserStream::manual`].
#[cfg(test)]
pub(crate) struct ManualStream(Arc<Mutex<News>>);

#[cfg(test)]
impl ManualStream {
    pub fn subscribed(&self) {
        let mut n = lock(&self.0);
        n.epoch += 1;
        n.live = true;
    }
    pub fn down(&self) {
        lock(&self.0).live = false;
    }
    pub fn order_event(&self, venue_id: i64) {
        lock(&self.0).touched.insert(venue_id);
    }
}

/// One message from the WebSocket API, as far as this module cares.
#[derive(Debug, PartialEq, Eq)]
enum Incoming {
    Subscribed,
    /// The subscribe call failed: Binance's code and message.
    Refused(String),
    /// An `executionReport` for this Binance order id.
    Order(i64),
    /// `eventStreamTerminated`: the subscription ended, the socket may not.
    Terminated,
    /// `serverShutdown`: Binance is about to drop the connection.
    ServerShutdown,
    /// Balance and account events, replies to other requests.
    Other,
    /// Not JSON, or an `executionReport` without an order id.
    Garbled,
}

fn parse(text: &str) -> Incoming {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Incoming::Garbled;
    };
    if v["id"].as_str() == Some(SUBSCRIBE_ID) {
        return match v["status"].as_i64() {
            Some(200) => Incoming::Subscribed,
            status => Incoming::Refused(format!(
                "status {}: {} {}",
                status.unwrap_or_default(),
                v["error"]["code"],
                v["error"]["msg"].as_str().unwrap_or_default()
            )),
        };
    }
    let event = &v["event"];
    match event["e"].as_str() {
        Some("executionReport") => event["i"]
            .as_i64()
            .map_or(Incoming::Garbled, Incoming::Order),
        Some("eventStreamTerminated") => Incoming::Terminated,
        Some("serverShutdown") => Incoming::ServerShutdown,
        _ => Incoming::Other,
    }
}

/// `userDataStream.subscribe.signature`: `apiKey` and `timestamp`, signed
/// over the parameters sorted by name (WebSocket API signing).
fn subscribe_request(creds: &Credentials, timestamp: i64) -> String {
    let payload = format!("apiKey={}&timestamp={timestamp}", creds.key);
    json!({
        "id": SUBSCRIBE_ID,
        "method": "userDataStream.subscribe.signature",
        "params": {
            "apiKey": creds.key,
            "timestamp": timestamp,
            "signature": sign(&creds.secret, &payload),
        }
    })
    .to_string()
}

async fn run(
    url: String,
    creds: Credentials,
    clock: Box<dyn Clock>,
    offset_ms: i64,
    timing: Timing,
    news: Arc<Mutex<News>>,
) {
    let mut delay = timing.retry_min;
    loop {
        let timestamp = clock.now_millis() + offset_ms;
        let end = session(&url, &creds, timestamp, timing, &news).await;
        lock(&news).live = false;
        metrics::gauge!(crate::metrics::USER_STREAM_UP).set(0.0);
        match end {
            End::Renew(reason) => {
                tracing::info!(reason, "renewing user data stream");
                delay = timing.retry_min;
                continue;
            }
            End::Dropped(reason) => {
                metrics::counter!(crate::metrics::USER_STREAM_DROPS).increment(1);
                delay = timing.retry_min;
                tracing::warn!(
                    reason,
                    retry_in_secs = delay.as_secs_f64(),
                    "user data stream down; polling every order until it is back"
                );
            }
            End::Failed(reason) => tracing::warn!(
                reason,
                retry_in_secs = delay.as_secs_f64(),
                "user data stream down; polling every order until it is back"
            ),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(timing.retry_max);
    }
}

/// How a connection ended.
enum End {
    /// Never subscribed: retry with backoff.
    Failed(String),
    /// Subscribed, then lost: retry from the shortest delay.
    Dropped(String),
    /// Replaced on purpose (age limit, `serverShutdown`): reconnect now.
    Renew(&'static str),
}

/// One connection, from connect to its end.
async fn session(
    url: &str,
    creds: &Credentials,
    timestamp: i64,
    timing: Timing,
    news: &Mutex<News>,
) -> End {
    let handshake = async {
        let (mut socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| format!("connect: {e}"))?;
        socket
            .send(Message::Text(subscribe_request(creds, timestamp).into()))
            .await
            .map_err(|e| format!("subscribe: {e}"))?;
        while let Some(msg) = socket.next().await {
            match msg.map_err(|e| format!("subscribe: {e}"))? {
                Message::Text(t) => match parse(&t) {
                    Incoming::Subscribed => return Ok(socket),
                    Incoming::Refused(why) => return Err(format!("subscribe refused: {why}")),
                    _ => {}
                },
                Message::Close(frame) => return Err(format!("closed by server: {frame:?}")),
                _ => {}
            }
        }
        Err("closed before subscribing".to_owned())
    };
    let mut socket = match tokio::time::timeout(timing.handshake, handshake).await {
        Ok(Ok(s)) => s,
        Ok(Err(why)) => return End::Failed(why),
        Err(_) => return End::Failed("connect and subscribe timed out".to_owned()),
    };
    let renew_at = tokio::time::Instant::now() + timing.max_age;
    {
        let mut n = lock(news);
        n.epoch += 1;
        n.live = true;
    }
    metrics::gauge!(crate::metrics::USER_STREAM_UP).set(1.0);
    tracing::info!("user data stream subscribed");
    loop {
        let silent_at = tokio::time::Instant::now() + timing.silence;
        let msg = match tokio::time::timeout_at(silent_at.min(renew_at), socket.next()).await {
            Err(_) if tokio::time::Instant::now() >= renew_at => {
                return End::Renew("connection age limit");
            }
            Err(_) => {
                return End::Dropped(format!("silent for {}s", timing.silence.as_secs()));
            }
            Ok(None) => return End::Dropped("closed".to_owned()),
            Ok(Some(Err(e))) => return End::Dropped(e.to_string()),
            Ok(Some(Ok(m))) => m,
        };
        match msg {
            Message::Text(t) => match parse(&t) {
                Incoming::Order(id) => {
                    lock(news).touched.insert(id);
                }
                Incoming::Garbled => {
                    tracing::warn!(message = %t, "unreadable user data stream message");
                    lock(news).garbled = true;
                }
                Incoming::Terminated => return End::Dropped("subscription terminated".to_owned()),
                Incoming::ServerShutdown => return End::Renew("server shutdown announced"),
                Incoming::Subscribed | Incoming::Refused(_) | Incoming::Other => {}
            },
            Message::Close(frame) => return End::Dropped(format!("closed by server: {frame:?}")),
            // tungstenite answers pings itself.
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binance::signed::fake::FixedClock;
    use tokio::net::TcpListener;

    /// `executionReport` exactly as demo delivered one on 2026-10-27 (a
    /// limit buy placed and cancelled to capture it), with `order_id` as
    /// Binance's id.
    fn execution_report(order_id: i64) -> String {
        format!(
            r#"{{"subscriptionId":0,"event":{{"e":"executionReport","E":1793109167700,
            "s":"BTCUSDT","c":"probe-1793109167","S":"BUY","o":"LIMIT","f":"GTC",
            "q":"0.00010000","p":"72106.89000000","P":"0.00000000","F":"0.00000000","g":-1,
            "C":"","x":"NEW","X":"NEW","r":"NONE","i":{order_id},"l":"0.00000000",
            "z":"0.00000000","L":"0.00000000","n":"0","N":null,"T":1793109167700,"t":-1,
            "I":155867607010,"w":true,"m":false,"M":false,"O":1793109167700,
            "Z":"0.00000000","Y":"0.00000000","Q":"0.00000000","W":1793109167700,
            "V":"EXPIRE_MAKER"}}}}"#
        )
    }

    #[test]
    fn reads_what_the_broker_needs() {
        assert_eq!(
            parse(&execution_report(77503693553)),
            Incoming::Order(77503693553)
        );
        assert_eq!(
            parse(r#"{"id":"subscribe","status":200,"result":{"subscriptionId":0}}"#),
            Incoming::Subscribed
        );
        assert_eq!(
            parse(
                r#"{"id":"subscribe","status":401,"error":{"code":-2015,"msg":"Invalid API-key, IP, or permissions for action."}}"#
            ),
            Incoming::Refused(
                "status 401: -2015 Invalid API-key, IP, or permissions for action.".into()
            )
        );
        assert_eq!(
            parse(r#"{"subscriptionId":0,"event":{"e":"eventStreamTerminated","E":1}}"#),
            Incoming::Terminated
        );
        // Binance's WebSocket API docs, "Server Shutdown".
        assert_eq!(
            parse(r#"{"event":{"e":"serverShutdown","E":1770123456789}}"#),
            Incoming::ServerShutdown
        );
        assert_eq!(
            parse(
                r#"{"subscriptionId":0,"event":{"e":"outboundAccountPosition","E":1793109167700,"u":1793109167700,"B":[{"a":"BTC","f":"0.00914263","l":"0.00698000"}]}}"#
            ),
            Incoming::Other
        );
        assert_eq!(
            parse(r#"{"subscriptionId":0,"event":{"e":"executionReport"}}"#),
            Incoming::Garbled
        );
        assert_eq!(parse("not json"), Incoming::Garbled);
    }

    #[test]
    fn subscribe_is_signed_over_sorted_params() {
        let creds = Credentials::new("k", "s");
        let v: Value = serde_json::from_str(&subscribe_request(&creds, 1_700_000_000_000)).unwrap();
        assert_eq!(v["method"], "userDataStream.subscribe.signature");
        assert_eq!(v["params"]["apiKey"], "k");
        assert_eq!(v["params"]["timestamp"], 1_700_000_000_000_i64);
        assert_eq!(
            v["params"]["signature"],
            sign("s", "apiKey=k&timestamp=1700000000000")
        );
    }

    #[test]
    fn take_vouches_only_within_one_subscription() {
        let (mut s, binance) = UserStream::manual();
        assert_eq!(s.take(), Changes::Unknown, "never subscribed");
        binance.subscribed();
        assert_eq!(s.take(), Changes::Unknown, "new subscription");
        assert_eq!(s.take(), Changes::Orders(HashSet::new()));
        binance.order_event(28);
        assert_eq!(s.take(), Changes::Orders([28].into()));
        assert_eq!(s.take(), Changes::Orders(HashSet::new()), "taken once");
        binance.order_event(29);
        binance.down();
        assert_eq!(s.take(), Changes::Unknown, "down");
        assert_eq!(s.take(), Changes::Unknown, "still down");
        binance.subscribed();
        assert_eq!(s.take(), Changes::Unknown, "resubscribed");
        assert_eq!(s.take(), Changes::Orders(HashSet::new()));
        lock(&s.news).garbled = true;
        assert_eq!(s.take(), Changes::Unknown, "garbled message");
    }

    fn fast() -> Timing {
        Timing {
            retry_min: Duration::from_millis(10),
            retry_max: Duration::from_millis(10),
            silence: Duration::from_secs(5),
            handshake: Duration::from_secs(5),
            max_age: Duration::from_secs(3600),
        }
    }

    async fn wait_for(s: &UserStream, done: impl Fn(&News) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done(&lock(&s.news)) {
            assert!(std::time::Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A local server playing Binance: checks the signed subscribe, pushes
    /// one order event, drops the connection, and takes the reconnect.
    #[tokio::test]
    async fn subscribes_reports_orders_and_resubscribes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for round in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                let Some(Ok(Message::Text(req))) = ws.next().await else {
                    panic!("no subscribe request");
                };
                let req: Value = serde_json::from_str(&req).unwrap();
                assert_eq!(req["method"], "userDataStream.subscribe.signature");
                // FixedClock + offset 5.
                assert_eq!(req["params"]["timestamp"], 1_700_000_000_005_i64);
                assert_eq!(
                    req["params"]["signature"],
                    sign("s", "apiKey=k&timestamp=1700000000005")
                );
                let ok = r#"{"id":"subscribe","status":200,"result":{"subscriptionId":0}}"#;
                ws.send(Message::Text(ok.into())).await.unwrap();
                if round == 0 {
                    ws.send(Message::Text(execution_report(28).into()))
                        .await
                        .unwrap();
                    // Hold the socket until the client has the event.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    ws.close(None).await.unwrap();
                } else {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        });
        let mut s = UserStream::spawn(
            format!("ws://{addr}"),
            Credentials::new("k", "s"),
            Box::new(FixedClock(1_700_000_000_000)),
            5,
            fast(),
        );
        wait_for(&s, |n| n.touched.contains(&28)).await;
        assert_eq!(s.take(), Changes::Unknown, "first subscription");
        wait_for(&s, |n| n.epoch == 2 && n.live).await;
        assert_eq!(s.take(), Changes::Unknown, "after reconnect");
        assert_eq!(s.take(), Changes::Orders(HashSet::new()));
        drop(s);
        server.abort();
    }

    #[tokio::test]
    async fn refused_subscribe_is_never_live() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                let _ = ws.next().await;
                let no = r#"{"id":"subscribe","status":401,"error":{"code":-2015,"msg":"no"}}"#;
                let _ = ws.send(Message::Text(no.into())).await;
            }
        });
        let mut s = UserStream::spawn(
            format!("ws://{addr}"),
            Credentials::new("k", "s"),
            Box::new(FixedClock(0)),
            0,
            fast(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let n = lock(&s.news);
        assert_eq!((n.epoch, n.live), (0, false));
        drop(n);
        assert_eq!(s.take(), Changes::Unknown);
        server.abort();
    }

    /// Accepts connections forever, subscribing each; `after` runs on the
    /// subscribed socket. Returns how many subscribes it has answered.
    fn binance_server<F, Fut>(listener: TcpListener, after: F) -> Arc<Mutex<u32>>
    where
        F: Fn(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let count = Arc::new(Mutex::new(0));
        let seen = count.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                let _ = ws.next().await;
                let ok = r#"{"id":"subscribe","status":200,"result":{"subscriptionId":0}}"#;
                ws.send(Message::Text(ok.into())).await.unwrap();
                *seen.lock().unwrap() += 1;
                tokio::spawn(after(ws));
            }
        });
        count
    }

    /// Renewal skips the backoff: with a 30 s retry delay, a second
    /// subscription within seconds can only be the renewal path.
    fn slow_retry() -> Timing {
        Timing {
            retry_min: Duration::from_secs(30),
            retry_max: Duration::from_secs(30),
            ..fast()
        }
    }

    #[tokio::test]
    async fn renews_before_the_connection_age_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let subscribes = binance_server(listener, |mut ws| async move {
            // Hold the socket open; Binance would ping it.
            while ws.next().await.is_some() {}
        });
        let s = UserStream::spawn(
            format!("ws://{addr}"),
            Credentials::new("k", "s"),
            Box::new(FixedClock(0)),
            0,
            Timing {
                max_age: Duration::from_millis(200),
                ..slow_retry()
            },
        );
        wait_for(&s, |n| n.epoch >= 3 && n.live).await;
        assert!(*subscribes.lock().unwrap() >= 3);
    }

    #[tokio::test]
    async fn server_shutdown_renews_at_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let first = Arc::new(Mutex::new(true));
        binance_server(listener, move |mut ws| {
            let first = std::mem::replace(&mut *first.lock().unwrap(), false);
            async move {
                if first {
                    let bye = r#"{"event":{"e":"serverShutdown","E":1770123456789}}"#;
                    ws.send(Message::Text(bye.into())).await.unwrap();
                }
                while ws.next().await.is_some() {}
            }
        });
        let mut s = UserStream::spawn(
            format!("ws://{addr}"),
            Credentials::new("k", "s"),
            Box::new(FixedClock(0)),
            0,
            slow_retry(),
        );
        wait_for(&s, |n| n.epoch == 2 && n.live).await;
        assert_eq!(s.take(), Changes::Unknown, "new connection: full poll");
    }
}
