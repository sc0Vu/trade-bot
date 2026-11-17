//! eToro's private WebSocket feed: the account's order and position events.
//!
//! `wss://ws.etoro.com/ws`; `Authenticate` with both keys, then `Subscribe`
//! to `private:demo` (demo account) or `private` (real). Checked on the
//! demo account 2026-11-18: within ~50 ms of an execution the feed pushes
//! the order's events (type `OrderEvent`, not the documented
//! `Trading.OrderForOpen.Update`), each `content` a JSON-encoded string.
//! An executed close carries `positionsExecutions[].closingData` with the
//! average price, units and **the fee** (`fees`), which no REST endpoint
//! gives for a close (the close-order record has none; trade history says
//! 0). A take-profit change is confirmed (`positionUpdateType:
//! takeProfitUpdated` with the rate) or refused (same, with `errorCode`;
//! e.g. 1053 for a take-profit below a long's market) only here: the REST
//! call answers 202 either way. The server sends an empty frame about
//! every 30 s.
//!
//! [`EtoroStream`] runs in the background and keeps what it saw; the broker
//! asks it. It never decides anything alone: fills are still settled by
//! the broker's REST polls; the stream supplies the exact close fee, a take-
//! profit close's execution, and take-profit acknowledgements. Down or
//! silent for 90 s → reconnect (1 s → 60 s backoff); while down the broker
//! falls back to REST (close fee derived, flagged in the log).

use super::client::{Account, EtoroKeys};
use crate::ExchangeError;
use crate::max::public::raw_decimal;
use bot_core::{Decimal, Timestamp};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// The WebSocket endpoint.
pub const WS_URL: &str = "wss://ws.etoro.com/ws";
/// No frame for this long (heartbeat ~30 s) → reconnect.
const SILENT: Duration = Duration::from_secs(90);

/// A position closed, as the venue executed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closed {
    /// The close order (the bot's, or eToro's for a take-profit).
    pub order_id: i64,
    pub position_id: i64,
    pub units: Decimal,
    pub price: Decimal,
    /// USD, as eToro charged it.
    pub fee: Decimal,
    pub time: Timestamp,
}

/// What became of a take-profit change on a position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TakeProfit {
    Set(Decimal),
    Cleared,
    Refused(String),
}

/// One thing the feed told us.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Closed(Closed),
    TakeProfit {
        position_id: i64,
        outcome: TakeProfit,
    },
}

fn decode(what: impl Into<String>) -> ExchangeError {
    ExchangeError::Decode(what.into())
}

/// The events in one pushed text frame (control replies and other topics
/// yield none).
pub(crate) fn parse_push(text: &str) -> Result<Vec<Event>, ExchangeError> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        messages: Vec<Push>,
    }
    #[derive(Deserialize)]
    struct Push {
        #[serde(default)]
        topic: String,
        #[serde(default)]
        content: String,
    }
    let env: Envelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for m in env.messages {
        if !m.topic.starts_with("private") || m.content.is_empty() {
            continue;
        }
        out.extend(parse_content(&m.content)?);
    }
    Ok(out)
}

fn parse_content(content: &str) -> Result<Vec<Event>, ExchangeError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Content<'a> {
        #[serde(default)]
        order_update_type: Option<String>,
        #[serde(borrow, default)]
        positions_executions: Vec<Exec<'a>>,
        #[serde(default)]
        position_update_type: Option<String>,
        #[serde(default)]
        position_id: Option<i64>,
        #[serde(borrow, default)]
        take_profit_rate: Option<&'a RawValue>,
        #[serde(default)]
        error_code: Option<serde_json::Value>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Exec<'a> {
        position_id: i64,
        #[serde(borrow, default)]
        closing_data: Option<Closing<'a>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Closing<'a> {
        order_id: i64,
        execution_time: String,
        #[serde(borrow)]
        units: &'a RawValue,
        #[serde(borrow)]
        avg_price: &'a RawValue,
        #[serde(borrow)]
        fees: &'a RawValue,
    }
    let c: Content = match serde_json::from_str(content) {
        Ok(c) => c,
        Err(_) => return Ok(Vec::new()), // another message kind
    };
    let mut out = Vec::new();
    if c.order_update_type.as_deref() == Some("executed") {
        for x in c.positions_executions {
            let Some(cd) = x.closing_data else { continue };
            out.push(Event::Closed(Closed {
                order_id: cd.order_id,
                position_id: x.position_id,
                units: raw_decimal(cd.units, "units")?,
                price: raw_decimal(cd.avg_price, "avgPrice")?,
                fee: raw_decimal(cd.fees, "fees")?,
                time: Timestamp::from_millis(super::market::parse_iso(&cd.execution_time)?),
            }));
        }
    }
    if c.position_update_type.as_deref() == Some("takeProfitUpdated") {
        let position_id = c
            .position_id
            .ok_or_else(|| decode("takeProfitUpdated without positionId"))?;
        let error = c.error_code.as_ref().and_then(|e| match e {
            serde_json::Value::String(s) if !s.is_empty() && s != "0" => Some(s.clone()),
            serde_json::Value::Number(n) if n.as_i64() != Some(0) => Some(n.to_string()),
            _ => None,
        });
        let outcome = match (error, c.take_profit_rate) {
            (Some(code), _) => TakeProfit::Refused(format!("eToro error {code}")),
            (None, Some(r)) if r.get() != "null" => {
                let rate = raw_decimal(r, "takeProfitRate")?;
                if rate.is_zero() {
                    TakeProfit::Cleared
                } else {
                    TakeProfit::Set(rate)
                }
            }
            (None, _) => TakeProfit::Cleared,
        };
        out.push(Event::TakeProfit {
            position_id,
            outcome,
        });
    }
    Ok(out)
}

#[derive(Default)]
struct Seen {
    closes: HashMap<i64, Closed>,
    take_profits: HashMap<i64, TakeProfit>,
    connected: bool,
}

/// See the module docs. Cloning shares the background task's findings.
#[derive(Clone)]
pub struct EtoroStream {
    seen: Arc<Mutex<Seen>>,
}

impl EtoroStream {
    /// Starts the background connection for `account`.
    pub fn start(keys: EtoroKeys, account: Account) -> Self {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let task_seen = seen.clone();
        tokio::spawn(async move { run(keys, account, task_seen).await });
        Self { seen }
    }

    /// A stream fed by hand (tests).
    #[cfg(test)]
    pub(crate) fn manual() -> Self {
        Self {
            seen: Arc::new(Mutex::new(Seen {
                connected: true,
                ..Seen::default()
            })),
        }
    }

    /// Records `events` as if pushed (tests, and the background task).
    pub(crate) fn absorb(&self, events: Vec<Event>) {
        let mut s = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        for e in events {
            match e {
                Event::Closed(c) => {
                    s.closes.insert(c.position_id, c);
                }
                Event::TakeProfit {
                    position_id,
                    outcome,
                } => {
                    s.take_profits.insert(position_id, outcome);
                }
            }
        }
    }

    /// The venue's record of `position_id`'s close, if the feed saw it.
    pub fn close_of(&self, position_id: i64) -> Option<Closed> {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closes
            .get(&position_id)
            .cloned()
    }

    /// The last take-profit outcome the feed reported for `position_id`;
    /// taken (a later change reports anew).
    pub fn take_profit(&self, position_id: i64) -> Option<TakeProfit> {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_profits
            .remove(&position_id)
    }

    /// Whether the feed is connected and subscribed right now.
    pub fn connected(&self) -> bool {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .connected
    }
}

fn set_connected(seen: &Arc<Mutex<Seen>>, up: bool) {
    seen.lock().unwrap_or_else(|e| e.into_inner()).connected = up;
}

async fn run(keys: EtoroKeys, account: Account, seen: Arc<Mutex<Seen>>) {
    let stream = EtoroStream { seen: seen.clone() };
    let mut backoff = Duration::from_secs(1);
    loop {
        match session(&keys, account, &stream).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => tracing::warn!(error = %e, "eToro private feed down; reconnecting"),
        }
        set_connected(&seen, false);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// One connection, until it drops or goes silent.
async fn session(keys: &EtoroKeys, account: Account, stream: &EtoroStream) -> Result<(), String> {
    let mut req = WS_URL.into_client_request().map_err(|e| e.to_string())?;
    // Cloudflare refuses a client without a user agent (as on REST).
    req.headers_mut().insert(
        "user-agent",
        concat!("trade-bot/", env!("CARGO_PKG_VERSION"))
            .parse()
            .map_err(|_| "user agent".to_owned())?,
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| e.to_string())?;
    let (api_key, user_key) = keys.parts();
    let auth = serde_json::json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "operation": "Authenticate",
        "data": {"userKey": user_key, "apiKey": api_key},
    });
    ws.send(Message::Text(auth.to_string().into()))
        .await
        .map_err(|e| e.to_string())?;
    let topic = match account {
        Account::Demo => "private:demo",
        Account::Real => "private",
    };
    loop {
        let next = tokio::time::timeout(SILENT, ws.next())
            .await
            .map_err(|_| "silent for 90 s".to_owned())?;
        let msg = match next {
            Some(Ok(m)) => m,
            Some(Err(e)) => return Err(e.to_string()),
            None => return Err("closed".into()),
        };
        let Message::Text(text) = msg else {
            if let Message::Close(f) = msg {
                return Err(format!("closed by server: {f:?}"));
            }
            continue; // heartbeat (empty / binary frame), ping, pong
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v["operation"].as_str() {
            Some("Authenticate") => {
                if v["success"] != true {
                    return Err(format!("authentication refused: {}", v["errorCode"]));
                }
                let sub = serde_json::json!({
                    "id": uuid::Uuid::new_v4().to_string(),
                    "operation": "Subscribe",
                    "data": {"topics": [topic]},
                });
                ws.send(Message::Text(sub.to_string().into()))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Some("Subscribe") => {
                if v["success"] != true {
                    return Err(format!("subscription refused: {}", v["errorMessage"]));
                }
                set_connected(&stream.seen, true);
                tracing::info!(topic, "eToro private feed subscribed");
            }
            _ => match parse_push(&text) {
                Ok(events) => stream.absorb(events),
                Err(e) => tracing::warn!(error = %e, "unreadable eToro private event; ignored"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn push(content: &serde_json::Value) -> String {
        serde_json::json!({"messages": [{"topic": "private:demo", "type": "OrderEvent", "id": "x", "content": content.to_string()}]}).to_string()
    }

    /// The close of 2026-11-18 (trimmed): 0.000313 @ 63,734.39, fee 0.2 —
    /// eToro's own figure (derived from proceeds it was 0.19886).
    #[test]
    fn an_executed_close_carries_the_fee() {
        let c = serde_json::json!({"orderId": 394912032, "action": "close", "orderUpdateType": "executed", "orderState": "filled",
            "positionsExecutions": [{"positionId": 3644962874_i64, "positionUpdateType": "close", "positionState": "closed",
                "closingData": {"orderId": 394912032, "executionTime": "2026-11-17T19:22:53.1999949", "units": 0.000313,
                    "avgPrice": 63734.39, "markup": -0.03, "fees": 0.2, "taxes": 0, "pnl": -0.0045698}}]});
        let ev = parse_push(&push(&c)).unwrap();
        assert_eq!(
            ev,
            vec![Event::Closed(Closed {
                order_id: 394912032,
                position_id: 3644962874,
                units: dec!(0.000313),
                price: dec!(63734.39),
                fee: dec!(0.2),
                time: Timestamp::from_millis(1_794_943_373_199),
            })]
        );
        // The open's executed event has openingData only: no close.
        let open = serde_json::json!({"orderId": 1, "orderUpdateType": "executed", "positionsExecutions": [{"positionId": 9, "openingData": {"fees": 0.2}, "closingData": null}]});
        assert!(parse_push(&push(&open)).unwrap().is_empty());
    }

    /// Take-profit confirmed with its rate, refused with error 1053 (below
    /// a long's market), as the demo feed sent them.
    #[test]
    fn take_profit_outcomes() {
        let set = serde_json::json!({"positionId": 3644964669_i64, "positionUpdateType": "takeProfitUpdated", "changedFields": 2, "takeProfitRate": 95500.92});
        let refused = serde_json::json!({"positionId": 3644964669_i64, "positionUpdateType": "takeProfitUpdated", "errorCode": "1053"});
        assert_eq!(
            parse_push(&push(&set)).unwrap(),
            vec![Event::TakeProfit {
                position_id: 3644964669,
                outcome: TakeProfit::Set(dec!(95500.92))
            }]
        );
        assert_eq!(
            parse_push(&push(&refused)).unwrap(),
            vec![Event::TakeProfit {
                position_id: 3644964669,
                outcome: TakeProfit::Refused("eToro error 1053".into())
            }]
        );
        // Control replies and other topics: nothing.
        assert!(
            parse_push(r#"{"id":"a","success":true,"operation":"Subscribe"}"#)
                .unwrap()
                .is_empty()
        );
        let s = EtoroStream::manual();
        s.absorb(parse_push(&push(&set)).unwrap());
        assert_eq!(
            s.take_profit(3644964669),
            Some(TakeProfit::Set(dec!(95500.92)))
        );
        assert_eq!(s.take_profit(3644964669), None);
    }

    /// Live: connects with ETORO_PUBLIC_KEY / ETORO_PRIVATE_KEY (a demo
    /// user key) and subscribes to `private:demo` within 30 s.
    /// `cargo test -p exchange live_private_feed -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn live_private_feed() {
        let s = EtoroStream::start(EtoroKeys::from_env().unwrap(), Account::Demo);
        for _ in 0..60 {
            if s.connected() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!("not subscribed within 30 s");
    }
}
