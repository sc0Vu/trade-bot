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

impl KlineFeed {
    pub fn new(mut candles: Vec<Candle>) -> Self {
        candles.sort_by(|a, b| (a.close_time, &a.instrument).cmp(&(b.close_time, &b.instrument)));
        Self {
            events: candles
                .into_iter()
                .map(MarketEvent::Candle)
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
}
