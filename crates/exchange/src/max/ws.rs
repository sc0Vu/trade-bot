//! Live MAX kline feed over its public WebSocket, and the REST backfill
//! source, for [`crate::binance::feed::ReconnectingFeed`].
//!
//! MAX pushes a kline update many times per bar; only the one flagged
//! `"x": true` is a closed bar, and only those become
//! `MarketEvent::Candle` (invariants 2 and 3). A bar with no trades still
//! closes (volume `"0"`). Unlike Binance, MAX expects the client to ping:
//! with no ping for 130 s it closes the socket, so the stream pings every
//! [`PING_EVERY`].

use super::public::{MaxPublicClient, raw_decimal};
use crate::binance::feed::{Connector, KlineSource};
use crate::binance::ws::stale_after;
use crate::{ExchangeError, MarketData};
use async_trait::async_trait;
use bot_core::{
    Candle, CandleSpec, InstrumentId, Interval, MarketEvent, Notional, Price, Qty, Timestamp,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// How often the stream pings MAX (it drops a socket after 130 s without).
pub const PING_EVERY: Duration = Duration::from_secs(30);

/// Closed-kline feed for a set of MAX markets at one interval. A dropped
/// or stalled socket is an error ([`stale_after`], as on Binance), for the
/// reconnecting wrapper to handle.
pub struct MaxKlineStream {
    socket: Socket,
    by_market: HashMap<String, InstrumentId>,
    interval: Interval,
    stale_after: Duration,
    deadline: Instant,
    ping_every: Duration,
    next_ping: Instant,
}

impl MaxKlineStream {
    /// Connects to `url` (normally [`super::WS_URL`]) and subscribes to
    /// `ids`' klines at `interval`; fails if MAX refuses the subscription.
    pub async fn connect(
        url: &str,
        ids: &[InstrumentId],
        interval: Interval,
    ) -> Result<Self, ExchangeError> {
        Self::connect_with(url, ids, interval, PING_EVERY).await
    }

    async fn connect_with(
        url: &str,
        ids: &[InstrumentId],
        interval: Interval,
        ping_every: Duration,
    ) -> Result<Self, ExchangeError> {
        if interval == Interval::S1 {
            return Err(ExchangeError::Feed("MAX serves no 1s klines".into()));
        }
        let feed = |e: tokio_tungstenite::tungstenite::Error| ExchangeError::Feed(e.to_string());
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.map_err(feed)?;
        let subs: Vec<Value> = ids
            .iter()
            .map(|id| {
                json!({
                    "channel": "kline",
                    "market": id.symbol().as_str().to_lowercase(),
                    "resolution": interval.code(),
                })
            })
            .collect();
        let sub = json!({ "action": "sub", "subscriptions": subs, "id": "klines" });
        socket
            .send(Message::Text(sub.to_string().into()))
            .await
            .map_err(feed)?;
        let stale = stale_after(interval);
        let now = Instant::now();
        Ok(Self {
            socket,
            by_market: ids
                .iter()
                .map(|id| (id.symbol().as_str().to_lowercase(), id.clone()))
                .collect(),
            interval,
            stale_after: stale,
            deadline: now + stale,
            ping_every,
            next_ping: now + ping_every,
        })
    }
}

#[async_trait]
impl MarketData for MaxKlineStream {
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
        loop {
            let wake = self.deadline.min(self.next_ping);
            let msg = match tokio::time::timeout_at(wake, self.socket.next()).await {
                Err(_) if Instant::now() >= self.deadline => {
                    return Err(ExchangeError::Feed(format!(
                        "no closed candle for {}s; feed stalled",
                        self.stale_after.as_secs()
                    )));
                }
                Err(_) => {
                    self.socket
                        .send(Message::Ping(Vec::new().into()))
                        .await
                        .map_err(|e| ExchangeError::Feed(e.to_string()))?;
                    self.next_ping = Instant::now() + self.ping_every;
                    continue;
                }
                Ok(None) => return Err(ExchangeError::Feed("websocket closed".into())),
                Ok(Some(m)) => m.map_err(|e| ExchangeError::Feed(e.to_string()))?,
            };
            match msg {
                Message::Text(text) => {
                    if let Some(c) = parse_kline_message(&text, &self.by_market, self.interval)? {
                        self.deadline = Instant::now() + self.stale_after;
                        return Ok(Some(MarketEvent::Candle(c)));
                    }
                }
                Message::Close(frame) => {
                    return Err(ExchangeError::Feed(format!(
                        "websocket closed by server: {frame:?}"
                    )));
                }
                Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
            }
        }
    }
}

/// Parses one MAX WebSocket message. `Ok(None)` for anything but a closed
/// kline (open-bar updates, the subscription ack). Errors on a refused
/// subscription, an unsubscribed market, another interval, or a malformed
/// bar. Quote volume is MAX's `qv`; if a message lacks it, it is estimated
/// as on REST ([`super::public::parse_klines`]).
pub(crate) fn parse_kline_message(
    text: &str,
    by_market: &HashMap<String, InstrumentId>,
    interval: Interval,
) -> Result<Option<Candle>, ExchangeError> {
    let v: Value = serde_json::from_str(text).map_err(|e| ExchangeError::Decode(e.to_string()))?;
    if v["e"] == "error" {
        return Err(ExchangeError::Feed(format!("MAX refused: {}", v["E"])));
    }
    if v["c"] != "kline" || v["k"]["x"].as_bool() != Some(true) {
        return Ok(None);
    }
    #[derive(serde::Deserialize)]
    struct K<'a> {
        #[serde(rename = "ST")]
        start: i64,
        #[serde(rename = "ET")]
        end: i64,
        #[serde(rename = "M")]
        market: String,
        #[serde(rename = "R")]
        resolution: String,
        #[serde(rename = "O", borrow)]
        open: &'a RawValue,
        #[serde(rename = "H", borrow)]
        high: &'a RawValue,
        #[serde(rename = "L", borrow)]
        low: &'a RawValue,
        #[serde(rename = "C", borrow)]
        close: &'a RawValue,
        #[serde(borrow)]
        v: &'a RawValue,
        #[serde(borrow)]
        qv: Option<&'a RawValue>,
    }
    #[derive(serde::Deserialize)]
    struct Msg<'a> {
        #[serde(borrow)]
        k: K<'a>,
    }
    let m: Msg =
        serde_json::from_str(text).map_err(|e| ExchangeError::Decode(format!("kline: {e}")))?;
    let k = m.k;
    let id = by_market
        .get(&k.market)
        .ok_or_else(|| ExchangeError::Decode(format!("kline for unsubscribed {}", k.market)))?;
    if k.resolution != interval.code() {
        return Err(ExchangeError::Decode("kline for another interval".into()));
    }
    let [o, h, l, c] = [k.open, k.high, k.low, k.close].map(|x| raw_decimal(x, "kline price"));
    let (o, h, l, c) = (o?, h?, l?, c?);
    let volume = raw_decimal(k.v, "kline volume")?;
    let quote = match k.qv {
        Some(q) => raw_decimal(q, "kline quote volume")?,
        None => volume * (o + h + l + c) / bot_core::Decimal::from(4),
    };
    Candle::new(CandleSpec {
        instrument: id.clone(),
        interval,
        open_time: Timestamp::from_millis(k.start),
        close_time: Timestamp::from_millis(k.end),
        open: Price::new(o),
        high: Price::new(h),
        low: Price::new(l),
        close: Price::new(c),
        volume: Qty::new(volume),
        quote_volume: Notional::new(quote),
    })
    .map(Some)
    .map_err(|e| ExchangeError::Decode(e.to_string()))
}

/// [`Connector`] for MAX's kline WebSocket.
pub struct MaxWsConnector {
    pub url: String,
    pub ids: Vec<InstrumentId>,
    pub interval: Interval,
}

#[async_trait]
impl Connector for MaxWsConnector {
    async fn connect(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        Ok(Box::new(
            MaxKlineStream::connect(&self.url, &self.ids, self.interval).await?,
        ))
    }
}

/// [`KlineSource`] over MAX's REST klines (warm-up and gap backfill).
pub struct MaxRestKlines {
    pub client: MaxPublicClient,
    pub interval: Interval,
}

#[async_trait]
impl KlineSource for MaxRestKlines {
    async fn recent(
        &self,
        id: &InstrumentId,
        since: Option<Timestamp>,
        max: usize,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let bar = self.interval.as_millis();
        let start = match since {
            Some(t) => t.as_millis() + 1,
            None => now - (max as i64 + 1) * bar,
        };
        let mut bars = self
            .client
            .klines(
                id.symbol(),
                self.interval,
                Timestamp::from_millis(start),
                Timestamp::from_millis(now),
            )
            .await?;
        bars.retain(|c| since.is_none_or(|s| c.close_time > s));
        let drop = bars.len().saturating_sub(max);
        bars.drain(..drop);
        Ok(bars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use tokio::net::TcpListener;

    fn markets() -> HashMap<String, InstrumentId> {
        [("btcusdt".to_owned(), "max:BTCUSDT".parse().unwrap())].into()
    }

    /// The closed bar MAX sent on 2026-10-28 (captured), `x` as given.
    fn msg(closed: bool) -> String {
        format!(
            r#"{{"c":"kline","M":"btcusdt","e":"update","T":1793186941284,"k":{{"ST":1793186880000,"ET":1793186939999,"M":"btcusdt","R":"1m","O":"81418.53","H":"81420","L":"81400.1","C":"81410","v":"0.0123","qv":"1001.37","ti":156643545,"x":{closed}}}}}"#
        )
    }

    #[test]
    fn only_closed_bars_become_candles() {
        assert_eq!(
            parse_kline_message(&msg(false), &markets(), Interval::M1).unwrap(),
            None
        );
        let c = parse_kline_message(&msg(true), &markets(), Interval::M1)
            .unwrap()
            .unwrap();
        assert_eq!(c.instrument.to_string(), "max:BTCUSDT");
        assert_eq!(c.open_time, Timestamp::from_millis(1_793_186_880_000));
        assert_eq!(c.close_time, Timestamp::from_millis(1_793_186_939_999));
        assert_eq!(
            (c.open, c.close),
            (Price::new(dec!(81418.53)), Price::new(dec!(81410)))
        );
        assert_eq!(c.volume, Qty::new(dec!(0.0123)));
        assert_eq!(c.quote_volume, Notional::new(dec!(1001.37)));
        let ack = r#"{"e":"subscribed","s":[{"channel":"kline","market":"btcusdt","resolution":"1m"}],"i":"k","T":1}"#;
        assert_eq!(
            parse_kline_message(ack, &markets(), Interval::M1).unwrap(),
            None
        );
    }

    #[test]
    fn refuses_what_it_did_not_ask_for() {
        assert!(parse_kline_message(&msg(true), &markets(), Interval::H1).is_err());
        let eth = msg(true).replace("btcusdt", "ethusdt");
        assert!(parse_kline_message(&eth, &markets(), Interval::M1).is_err());
        let refused = r#"{"e":"error","E":["invalid market"],"i":"klines","T":1}"#;
        assert!(parse_kline_message(refused, &markets(), Interval::M1).is_err());
        // Without qv, estimated like REST: 0.0123 × (81418.53 + 81420 +
        // 81400.1 + 81410) / 4 = 0.0123 × 81412.1575 = 1001.36953725.
        let no_qv = msg(true).replace(r#","qv":"1001.37""#, "");
        let c = parse_kline_message(&no_qv, &markets(), Interval::M1)
            .unwrap()
            .unwrap();
        assert_eq!(c.quote_volume, Notional::new(dec!(1001.36953725)));
    }

    /// A local server playing MAX: checks the subscription, sees the
    /// client's pings, and sends one open and one closed bar.
    #[tokio::test]
    async fn subscribes_pings_and_emits_closed_bars() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (open, closed) = (msg(false), msg(true));
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let Some(Ok(Message::Text(sub))) = ws.next().await else {
                panic!("no subscription");
            };
            let sub: Value = serde_json::from_str(&sub).unwrap();
            assert_eq!(sub["action"], "sub");
            assert_eq!(
                sub["subscriptions"][0],
                json!({"channel": "kline", "market": "btcusdt", "resolution": "1m"})
            );
            let mut pings = 0;
            while pings < 2 {
                if let Some(Ok(Message::Ping(_))) = ws.next().await {
                    pings += 1;
                }
            }
            ws.send(Message::Text(open.into())).await.unwrap();
            ws.send(Message::Text(closed.into())).await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let ids = vec!["max:BTCUSDT".parse().unwrap()];
        let mut feed = MaxKlineStream::connect_with(
            &format!("ws://{addr}"),
            &ids,
            Interval::M1,
            Duration::from_millis(20),
        )
        .await
        .unwrap();
        let Some(MarketEvent::Candle(c)) = feed.next().await.unwrap() else {
            panic!("no candle");
        };
        assert_eq!(c.close, Price::new(dec!(81410)));
        server.abort();
    }

    /// Against MAX itself (run with `--ignored`): five warm-up bars over
    /// REST, then the next closed 1m bar over the WebSocket, in order.
    #[tokio::test]
    #[ignore = "network: live MAX"]
    async fn live_feed_warms_up_then_streams() {
        use crate::binance::feed::ReconnectingFeed;
        let ids: Vec<InstrumentId> = vec!["max:BTCUSDT".parse().unwrap()];
        let mut feed = ReconnectingFeed::new(
            Box::new(MaxWsConnector {
                url: super::super::WS_URL.into(),
                ids: ids.clone(),
                interval: Interval::M1,
            }),
            Box::new(MaxRestKlines {
                client: MaxPublicClient::new(super::super::REST_URL),
                interval: Interval::M1,
            }),
            ids,
            5,
            Duration::from_secs(60),
        );
        let mut backfill = Vec::new();
        let live = loop {
            match tokio::time::timeout(Duration::from_secs(120), feed.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
            {
                MarketEvent::Backfill(c) => backfill.push(c),
                MarketEvent::Candle(c) => break c,
                other => panic!("{other:?}"),
            }
        };
        assert!(backfill.len() >= 4, "{}", backfill.len());
        assert!(
            backfill
                .windows(2)
                .all(|w| w[0].close_time < w[1].close_time)
        );
        assert!(live.close_time > backfill.last().unwrap().close_time);
        eprintln!("backfill {} bars, then live {:?}", backfill.len(), live);
    }
}
