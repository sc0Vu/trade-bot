//! Live kline feed over Binance's combined WebSocket streams.
//!
//! Binance pushes a kline update many times per bar; only the final one,
//! flagged `"x": true`, is a closed bar. Only those become
//! `MarketEvent::Candle`, so the live engine sees exactly the closed-bar
//! stream a backtest replays (invariants 2 and 3).

use crate::{ExchangeError, MarketData};
use async_trait::async_trait;
use bot_core::{
    Candle, CandleSpec, InstrumentId, Interval, MarketEvent, Notional, Price, Qty, Timestamp,
};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Closed-kline feed for a set of instruments at one interval.
///
/// A dropped connection is an error, not a silent end of data: the engine
/// stops rather than trade on a gap.
pub struct KlineStream {
    socket: Socket,
    by_symbol: HashMap<String, InstrumentId>,
    interval: Interval,
}

/// `<base>/stream?streams=btcusdt@kline_1m/ethusdt@kline_1m`.
pub(crate) fn stream_url(base: &str, ids: &[InstrumentId], interval: Interval) -> String {
    let streams: Vec<String> = ids
        .iter()
        .map(|id| {
            format!(
                "{}@kline_{}",
                id.symbol().as_str().to_lowercase(),
                interval.code()
            )
        })
        .collect();
    format!(
        "{}/stream?streams={}",
        base.trim_end_matches('/'),
        streams.join("/")
    )
}

impl KlineStream {
    /// Connects to `base` (e.g. [`super::TESTNET_WS_URL`]) and subscribes
    /// to `ids` at `interval`.
    pub async fn connect(
        base: &str,
        ids: &[InstrumentId],
        interval: Interval,
    ) -> Result<Self, ExchangeError> {
        let (socket, _) = tokio_tungstenite::connect_async(stream_url(base, ids, interval))
            .await
            .map_err(|e| ExchangeError::Feed(e.to_string()))?;
        Ok(Self {
            socket,
            by_symbol: ids
                .iter()
                .map(|id| (id.symbol().to_string(), id.clone()))
                .collect(),
            interval,
        })
    }
}

#[async_trait]
impl MarketData for KlineStream {
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
        loop {
            let msg = self
                .socket
                .next()
                .await
                .ok_or_else(|| ExchangeError::Feed("websocket closed".into()))?
                .map_err(|e| ExchangeError::Feed(e.to_string()))?;
            match msg {
                Message::Text(text) => {
                    if let Some(c) = parse_kline_message(&text, &self.by_symbol, self.interval)? {
                        return Ok(Some(MarketEvent::Candle(c)));
                    }
                }
                Message::Close(frame) => {
                    return Err(ExchangeError::Feed(format!(
                        "websocket closed by server: {frame:?}"
                    )));
                }
                // tungstenite answers pings itself; nothing else carries klines.
                Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
            }
        }
    }
}

fn field<T: FromStr>(k: &Value, name: &str) -> Result<T, ExchangeError> {
    k[name]
        .as_str()
        .ok_or_else(|| ExchangeError::Decode(format!("kline {name}")))?
        .parse()
        .map_err(|_| ExchangeError::Decode(format!("kline {name}")))
}

/// Parses one combined-stream message. `Ok(None)` for an update to a bar
/// that hasn't closed yet. Errors on an unknown symbol, another interval,
/// a non-string price, or an inconsistent bar.
pub(crate) fn parse_kline_message(
    text: &str,
    by_symbol: &HashMap<String, InstrumentId>,
    interval: Interval,
) -> Result<Option<Candle>, ExchangeError> {
    let v: Value = serde_json::from_str(text).map_err(|e| ExchangeError::Decode(e.to_string()))?;
    let k = &v["data"]["k"];
    if !k.is_object() {
        return Err(ExchangeError::Decode("not a kline message".into()));
    }
    if k["x"].as_bool() != Some(true) {
        return Ok(None);
    }
    let symbol = k["s"]
        .as_str()
        .ok_or_else(|| ExchangeError::Decode("kline s".into()))?;
    let id = by_symbol
        .get(symbol)
        .ok_or_else(|| ExchangeError::Decode(format!("kline for unsubscribed {symbol}")))?;
    if k["i"].as_str() != Some(interval.code()) {
        return Err(ExchangeError::Decode("kline for another interval".into()));
    }
    let ms = |name: &str| {
        k[name]
            .as_i64()
            .ok_or_else(|| ExchangeError::Decode(format!("kline {name}")))
    };
    Candle::new(CandleSpec {
        instrument: id.clone(),
        interval,
        open_time: Timestamp::from_millis(ms("t")?),
        close_time: Timestamp::from_millis(ms("T")?),
        open: field::<Price>(k, "o")?,
        high: field::<Price>(k, "h")?,
        low: field::<Price>(k, "l")?,
        close: field::<Price>(k, "c")?,
        volume: field::<Qty>(k, "v")?,
        quote_volume: field::<Notional>(k, "q")?,
    })
    .map(Some)
    .map_err(|e| ExchangeError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn ids() -> HashMap<String, InstrumentId> {
        [("BTCUSDT".to_owned(), "binance:BTCUSDT".parse().unwrap())].into()
    }

    /// Shape from Binance's WebSocket stream docs, wrapped as a combined
    /// stream message.
    fn msg(closed: bool) -> String {
        format!(
            r#"{{"stream":"btcusdt@kline_1m","data":{{"e":"kline","E":1672515782136,"s":"BTCUSDT",
            "k":{{"t":1672515780000,"T":1672515839999,"s":"BTCUSDT","i":"1m","f":100,"L":200,
            "o":"16500.10","c":"16510.00","h":"16520.55","l":"16490.00","v":"12.5","n":100,
            "x":{closed},"q":"206375.00","V":"6.0","Q":"99000.00","B":"0"}}}}}}"#
        )
    }

    #[test]
    fn closed_bar_becomes_candle() {
        let c = parse_kline_message(&msg(true), &ids(), Interval::M1)
            .unwrap()
            .unwrap();
        assert_eq!(c.open_time, Timestamp::from_millis(1_672_515_780_000));
        assert_eq!(c.close_time, Timestamp::from_millis(1_672_515_839_999));
        assert_eq!(c.close, Price::new(dec!(16510.00)));
        assert_eq!(c.quote_volume, Notional::new(dec!(206375)));
    }

    #[test]
    fn forming_bar_is_skipped() {
        assert_eq!(
            parse_kline_message(&msg(false), &ids(), Interval::M1).unwrap(),
            None
        );
    }

    #[test]
    fn rejects_unexpected_messages() {
        assert!(parse_kline_message(&msg(true), &HashMap::new(), Interval::M1).is_err());
        assert!(parse_kline_message(&msg(true), &ids(), Interval::H1).is_err());
        let float = msg(true).replace(r#""c":"16510.00""#, r#""c":16510.0"#);
        assert!(parse_kline_message(&float, &ids(), Interval::M1).is_err());
        assert!(parse_kline_message(r#"{"result":null,"id":1}"#, &ids(), Interval::M1).is_err());
    }

    #[test]
    fn combined_stream_url() {
        let ids: Vec<InstrumentId> = vec![
            "binance:BTCUSDT".parse().unwrap(),
            "binance:ETHUSDT".parse().unwrap(),
        ];
        assert_eq!(
            stream_url("wss://stream.testnet.binance.vision/", &ids, Interval::M1),
            "wss://stream.testnet.binance.vision/stream?streams=btcusdt@kline_1m/ethusdt@kline_1m"
        );
    }
}
