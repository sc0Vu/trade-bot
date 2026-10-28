//! MAX market status, live: which markets accept orders right now.
//!
//! MAX moves a market between `active`, `cancel-only` (no new orders) and
//! `suspended` (no new orders, no cancels). [`MarketStatus`] starts from the
//! statuses in `/api/v3/markets` and follows MAX's public `market_status`
//! channel, and as an [`OrderGate`] makes the paper broker refuse what MAX
//! would. If the channel drops, the last known statuses stand while it
//! reconnects (1 s doubling to 60 s); a status change and any change to a
//! market's precision or minimums are logged (filters changed mid-run take
//! effect on restart).

use super::public::{Market, raw_decimal, step};
use crate::paper::OrderGate;
use bot_core::{Decimal, InstrumentId};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A MAX market's trading status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Active,
    /// Existing orders can be cancelled; new ones are refused.
    CancelOnly,
    /// Neither placing nor cancelling.
    Suspended,
    /// A status MAX didn't document: treated as no new orders.
    Other(String),
}

impl Status {
    pub fn parse(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "cancel-only" => Self::CancelOnly,
            "suspended" => Self::Suspended,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// The filters MAX publishes with a status: lot, tick and minimums.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filters {
    pub lot: Decimal,
    pub tick: Decimal,
    pub min_base: Decimal,
    pub min_quote: Decimal,
}

/// One market's entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub status: Status,
    pub filters: Filters,
}

type Book = Arc<Mutex<HashMap<String, Entry>>>;

fn lock(book: &Book) -> MutexGuard<'_, HashMap<String, Entry>> {
    // Every update leaves the map consistent, so a poisoned lock is usable.
    book.lock().unwrap_or_else(|e| e.into_inner())
}

/// See the module docs. Dropping it stops the channel task.
pub struct MarketStatus {
    book: Book,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl MarketStatus {
    /// Statuses from `markets` (the REST list), not yet followed live.
    pub fn from_markets(markets: &[Market]) -> Self {
        let book = markets
            .iter()
            .map(|m| {
                let i = &m.instrument;
                (
                    i.id.symbol().as_str().to_lowercase(),
                    Entry {
                        status: Status::parse(&m.status),
                        filters: Filters {
                            lot: i.lot_size.get(),
                            tick: i.tick_size.get(),
                            min_base: i.min_qty.get(),
                            min_quote: i.min_notional.get(),
                        },
                    },
                )
            })
            .collect();
        Self {
            book: Arc::new(Mutex::new(book)),
            task: None,
        }
    }

    /// Follows MAX's `market_status` channel at `url` from now on.
    pub fn follow(mut self, url: String) -> Self {
        self.task = Some(tokio::spawn(run(
            url,
            self.book.clone(),
            super::ws::PING_EVERY,
        )));
        self
    }

    /// `market`'s entry (`btctwd`), if MAX listed it.
    pub fn entry(&self, market: &str) -> Option<Entry> {
        lock(&self.book).get(market).cloned()
    }

    fn status(&self, id: &InstrumentId) -> Option<Status> {
        lock(&self.book)
            .get(&id.symbol().as_str().to_lowercase())
            .map(|e| e.status.clone())
    }
}

impl Drop for MarketStatus {
    fn drop(&mut self) {
        if let Some(t) = &self.task {
            t.abort();
        }
    }
}

impl OrderGate for MarketStatus {
    fn check_submit(&self, id: &InstrumentId) -> Result<(), &'static str> {
        match self.status(id) {
            Some(Status::Active) => Ok(()),
            Some(Status::CancelOnly) => Err("MAX market is cancel-only"),
            Some(Status::Suspended) => Err("MAX market is suspended"),
            Some(Status::Other(_)) => Err("MAX market status is not active"),
            None => Err("MAX no longer lists the market"),
        }
    }

    fn check_cancel(&self, id: &InstrumentId) -> Result<(), &'static str> {
        match self.status(id) {
            Some(Status::Suspended) => Err("MAX market is suspended"),
            _ => Ok(()),
        }
    }
}

#[derive(Deserialize)]
struct RawStatus<'a> {
    #[serde(rename = "M")]
    market: String,
    st: String,
    bup: i64,
    qup: i64,
    #[serde(borrow)]
    mba: &'a RawValue,
    #[serde(borrow)]
    mqa: &'a RawValue,
}

#[derive(Deserialize)]
struct RawMessage<'a> {
    #[serde(borrow)]
    ms: Vec<RawStatus<'a>>,
}

/// The entries in one `market_status` message (`snapshot` or `update`);
/// `None` for other messages (the subscription ack). Numbers are read from
/// their text, as on REST.
pub(crate) fn parse_status_message(
    text: &str,
) -> Result<Option<Vec<(String, Entry)>>, crate::ExchangeError> {
    let v: Value =
        serde_json::from_str(text).map_err(|e| crate::ExchangeError::Decode(e.to_string()))?;
    if v["e"] == "error" {
        return Err(crate::ExchangeError::Feed(format!(
            "MAX refused: {}",
            v["E"]
        )));
    }
    if v["c"] != "market_status" {
        return Ok(None);
    }
    let m: RawMessage = serde_json::from_str(text)
        .map_err(|e| crate::ExchangeError::Decode(format!("market_status: {e}")))?;
    m.ms.into_iter()
        .map(|s| {
            Ok((
                s.market,
                Entry {
                    status: Status::parse(&s.st),
                    filters: Filters {
                        lot: step(s.bup)?,
                        tick: step(s.qup)?,
                        min_base: raw_decimal(s.mba, "mba")?,
                        min_quote: raw_decimal(s.mqa, "mqa")?,
                    },
                },
            ))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Applies `entries`, logging what changed.
fn apply(book: &Book, entries: Vec<(String, Entry)>) {
    let mut b = lock(book);
    for (market, new) in entries {
        if let Some(old) = b.get(&market) {
            if old.status != new.status {
                if new.status == Status::Active {
                    tracing::info!(market, status = ?new.status, "MAX market accepts orders again");
                } else {
                    tracing::warn!(market, status = ?new.status, "MAX market no longer accepts new orders");
                }
            }
            if old.filters != new.filters {
                tracing::warn!(
                    market,
                    "MAX changed this market's precision or minimums; restart to trade on them"
                );
            }
        }
        b.insert(market, new);
    }
}

async fn run(url: String, book: Book, ping_every: Duration) {
    let mut delay = FIRST_BACKOFF;
    loop {
        let (subscribed, reason) = session(&url, &book, ping_every).await;
        if subscribed {
            delay = FIRST_BACKOFF;
        }
        tracing::warn!(
            reason,
            retry_in_secs = delay.as_secs(),
            "MAX market status channel down; keeping the last known statuses"
        );
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_BACKOFF);
    }
}

/// One connection. Returns whether its snapshot arrived, and why it ended.
async fn session(url: &str, book: &Book, ping_every: Duration) -> (bool, String) {
    let (mut socket, _) = match tokio_tungstenite::connect_async(url).await {
        Ok(s) => s,
        Err(e) => return (false, format!("connect: {e}")),
    };
    let sub = json!({
        "action": "sub",
        "subscriptions": [{ "channel": "market_status" }],
        "id": "market_status",
    });
    if let Err(e) = socket.send(Message::Text(sub.to_string().into())).await {
        return (false, format!("subscribe: {e}"));
    }
    let mut subscribed = false;
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + ping_every, ping_every);
    loop {
        let msg = tokio::select! {
            _ = ping.tick() => {
                if let Err(e) = socket.send(Message::Ping(Vec::new().into())).await {
                    return (subscribed, format!("ping: {e}"));
                }
                continue;
            }
            m = socket.next() => m,
        };
        match msg {
            None => return (subscribed, "closed".to_owned()),
            Some(Err(e)) => return (subscribed, e.to_string()),
            Some(Ok(Message::Text(t))) => match parse_status_message(&t) {
                Ok(Some(entries)) => {
                    subscribed = true;
                    apply(book, entries);
                }
                Ok(None) => {}
                Err(e) => return (subscribed, e.to_string()),
            },
            Some(Ok(Message::Close(f))) => return (subscribed, format!("closed by server: {f:?}")),
            Some(Ok(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// As MAX sent it on 2026-10-28 (two entries of the snapshot).
    const SNAPSHOT: &str = r#"{"c":"market_status","e":"snapshot","ms":[{"M":"btctwd","st":"active","bu":"btc","bup":8,"mba":0.0001,"qu":"twd","qup":1,"mqa":250,"mws":true,"gs":true,"gsm":"base+quote","ds":true},{"M":"grttwd","st":"active","bu":"grt","bup":1,"mba":270,"qu":"twd","qup":3,"mqa":250,"mws":false,"gs":true,"gsm":"base+quote","ds":true}],"T":1793189297049}"#;

    fn btctwd() -> InstrumentId {
        "max:BTCTWD".parse().unwrap()
    }

    fn status_with(entries: &[(&str, &str)]) -> MarketStatus {
        let s = MarketStatus::from_markets(&[]);
        apply(
            &s.book,
            entries
                .iter()
                .map(|(m, st)| {
                    (
                        (*m).to_owned(),
                        Entry {
                            status: Status::parse(st),
                            filters: Filters {
                                lot: dec!(0.00000001),
                                tick: dec!(0.1),
                                min_base: dec!(0.0001),
                                min_quote: dec!(250),
                            },
                        },
                    )
                })
                .collect(),
        );
        s
    }

    #[test]
    fn parses_the_snapshot_exactly() {
        let es = parse_status_message(SNAPSHOT).unwrap().unwrap();
        assert_eq!(es.len(), 2);
        assert_eq!(
            es[0],
            (
                "btctwd".to_owned(),
                Entry {
                    status: Status::Active,
                    filters: Filters {
                        lot: dec!(0.00000001),
                        tick: dec!(0.1),
                        min_base: dec!(0.0001),
                        min_quote: dec!(250),
                    },
                }
            )
        );
        assert_eq!(es[1].1.filters.lot, dec!(0.1));
        let ack = r#"{"e":"subscribed","s":[{"channel":"market_status"}],"i":"x","T":1}"#;
        assert_eq!(parse_status_message(ack).unwrap(), None);
        assert!(parse_status_message(r#"{"e":"error","E":["bad"],"T":1}"#).is_err());
    }

    #[test]
    fn gates_like_max() {
        let cases = [
            ("active", Ok(()), Ok(())),
            ("cancel-only", Err("MAX market is cancel-only"), Ok(())),
            (
                "suspended",
                Err("MAX market is suspended"),
                Err("MAX market is suspended"),
            ),
            ("paused", Err("MAX market status is not active"), Ok(())),
        ];
        for (st, submit, cancel) in cases {
            let s = status_with(&[("btctwd", st)]);
            assert_eq!(s.check_submit(&btctwd()), submit, "{st}");
            assert_eq!(s.check_cancel(&btctwd()), cancel, "{st}");
        }
        let gone = status_with(&[]);
        assert_eq!(
            gone.check_submit(&btctwd()),
            Err("MAX no longer lists the market")
        );
    }

    /// A local server playing MAX: a snapshot with the market active, then
    /// an update to cancel-only. The gate follows.
    #[tokio::test]
    async fn follows_the_channel() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let Some(Ok(Message::Text(sub))) = ws.next().await else {
                panic!("no subscription");
            };
            let sub: Value = serde_json::from_str(&sub).unwrap();
            assert_eq!(
                sub["subscriptions"][0],
                json!({ "channel": "market_status" })
            );
            ws.send(Message::Text(SNAPSHOT.into())).await.unwrap();
            let update = SNAPSHOT.replace("\"snapshot\"", "\"update\"").replacen(
                "\"st\":\"active\"",
                "\"st\":\"cancel-only\"",
                1,
            );
            ws.send(Message::Text(update.into())).await.unwrap();
            while ws.next().await.is_some() {}
        });
        let s = MarketStatus::from_markets(&[]).follow(format!("ws://{addr}"));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while s.check_submit(&btctwd()) != Err("MAX market is cancel-only") {
            assert!(
                std::time::Instant::now() < deadline,
                "{:?}",
                s.entry("btctwd")
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(s.check_cancel(&btctwd()).is_ok());
        drop(s);
        server.abort();
    }

    /// Against MAX itself (run with `--ignored`): the channel's snapshot
    /// agrees with the REST list it starts from, so no market logs a
    /// spurious change, and BTCTWD is active.
    #[tokio::test]
    #[ignore = "network: live MAX"]
    async fn live_snapshot_agrees_with_rest() {
        let markets = crate::max::public::MaxPublicClient::new(crate::max::REST_URL)
            .markets()
            .await
            .unwrap();
        let rest = MarketStatus::from_markets(&markets);
        let live = MarketStatus::from_markets(&[]).follow(crate::max::WS_URL.into());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while live.entry("btctwd").is_none() {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let differ: Vec<String> = lock(&rest.book)
            .iter()
            .filter(|(m, e)| live.entry(m).as_ref() != Some(e))
            .map(|(m, e)| format!("{m}: rest {e:?} ws {:?}", live.entry(m)))
            .collect();
        assert!(differ.is_empty(), "{differ:#?}");
        assert_eq!(live.check_submit(&btctwd()), Ok(()));
    }
}
