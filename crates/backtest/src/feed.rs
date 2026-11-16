use async_trait::async_trait;
use bot_core::{Candle, MarketEvent};
use exchange::{ExchangeError, MarketData};

/// Replays cached candles as the engine's market data feed.
///
/// Candles of several instruments are merged into one stream ordered by
/// close time (the moment each bar became known), ties broken by
/// instrument id, so the order is deterministic and never shows a bar
/// before it closed (invariant 2). Emits `MarketEvent::Candle`, the same
/// event type the live feed produces.
pub struct KlineFeed {
    events: std::vec::IntoIter<MarketEvent>,
}

/// Close time, then instrument id: the order bars became known.
fn sort(candles: &mut [Candle]) {
    candles.sort_by(|a, b| (a.close_time, &a.instrument).cmp(&(b.close_time, &b.instrument)));
}

impl KlineFeed {
    pub fn new(candles: Vec<Candle>) -> Self {
        Self::with_warmup(Vec::new(), candles)
    }

    /// `warmup` (bars before the backtest's start) replayed first as
    /// `MarketEvent::Backfill`, then `candles` as `MarketEvent::Candle`,
    /// as a live run warms up before its first decision: backfilled bars
    /// join the strategy's history but are no decision, and the broker
    /// doesn't settle against them.
    pub fn with_warmup(warmup: Vec<Candle>, candles: Vec<Candle>) -> Self {
        Self::with_events(warmup, candles, Vec::new())
    }

    /// [`with_warmup`](Self::with_warmup), plus `extra` events (delistings)
    /// merged in by time. At equal times an extra event comes before the
    /// candles, so a decision at that close already knows of it.
    pub fn with_events(
        mut warmup: Vec<Candle>,
        mut candles: Vec<Candle>,
        extra: Vec<MarketEvent>,
    ) -> Self {
        sort(&mut warmup);
        sort(&mut candles);
        let bars = warmup
            .into_iter()
            .map(MarketEvent::Backfill)
            .chain(candles.into_iter().map(MarketEvent::Candle))
            .map(|e| (1, e));
        let mut events: Vec<(u8, MarketEvent)> =
            extra.into_iter().map(|e| (0, e)).chain(bars).collect();
        // Stable: bars keep their (close time, id) order among themselves.
        events.sort_by_key(|(rank, e)| (e.ts(), *rank));
        Self {
            events: events
                .into_iter()
                .map(|(_, e)| e)
                .collect::<Vec<_>>()
                .into_iter(),
        }
    }
}

#[async_trait]
impl MarketData for KlineFeed {
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
        Ok(self.events.next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{btcusdt, candle};
    use bot_core::InstrumentId;

    #[tokio::test]
    async fn merges_instruments_by_close_time_then_id() {
        let btc = btcusdt().id.clone();
        let eth: InstrumentId = "binance:ETHUSDT".parse().unwrap();
        let mut feed = KlineFeed::new(vec![
            candle(&eth, 1, ["1", "1", "1", "1"]),
            candle(&btc, 1, ["1", "1", "1", "1"]),
            candle(&eth, 0, ["1", "1", "1", "1"]),
            candle(&btc, 0, ["1", "1", "1", "1"]),
        ]);
        let mut order = Vec::new();
        while let Some(MarketEvent::Candle(c)) = feed.next().await.unwrap() {
            order.push((
                c.open_time.as_millis() / 3_600_000,
                c.instrument.symbol().to_string(),
            ));
        }
        let expect = [
            (0, "BTCUSDT"),
            (0, "ETHUSDT"),
            (1, "BTCUSDT"),
            (1, "ETHUSDT"),
        ];
        assert_eq!(order, expect.map(|(i, s)| (i, s.to_owned())).to_vec());
    }

    /// Warm-up bars come first, as backfill, in close-time order; then the
    /// backtest's own bars as candles.
    #[tokio::test]
    async fn warmup_is_replayed_first_as_backfill() {
        let btc = btcusdt().id.clone();
        let mut feed = KlineFeed::with_warmup(
            vec![candle(&btc, 1, ["1"; 4]), candle(&btc, 0, ["1"; 4])],
            vec![candle(&btc, 2, ["1"; 4])],
        );
        let mut got = Vec::new();
        while let Some(e) = feed.next().await.unwrap() {
            got.push(match e {
                MarketEvent::Backfill(c) => ("backfill", c.open_time.as_millis() / 3_600_000),
                MarketEvent::Candle(c) => ("candle", c.open_time.as_millis() / 3_600_000),
                _ => unreachable!(),
            });
        }
        assert_eq!(got, vec![("backfill", 0), ("backfill", 1), ("candle", 2)]);
    }
}
