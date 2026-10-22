use crate::{DataDir, DataError, KlineFeed};
use async_trait::async_trait;
use bot_core::{AssetClass, Candle, Instrument, InstrumentId, Interval, Timestamp};
use exchange::paper::{BpsSlippage, NextOpenFillModel, PaperBroker, RateFee};
use exchange::{Broker, ExchangeError, InstrumentQuery, MarketData, Provider};

/// Backtest venue: cached candles as the feed, a [`PaperBroker`] with the
/// default [`NextOpenFillModel`] as the broker. The engine can't tell it
/// from a live provider, which is the point (invariant 3).
pub struct BacktestProvider {
    instruments: Vec<Instrument>,
    candles: Vec<Candle>,
    fees: RateFee,
    slippage: BpsSlippage,
}

impl BacktestProvider {
    pub fn new(
        instruments: Vec<Instrument>,
        candles: Vec<Candle>,
        fees: RateFee,
        slippage: BpsSlippage,
    ) -> Self {
        Self {
            instruments,
            candles,
            fees,
            slippage,
        }
    }

    /// Loads `ids` at `interval` with `from <= open_time < to` from the
    /// cache. Fails if any series or instrument was never fetched.
    pub fn from_cache(
        dir: &DataDir,
        ids: &[InstrumentId],
        interval: Interval,
        from: Timestamp,
        to: Timestamp,
        fees: RateFee,
        slippage: BpsSlippage,
    ) -> Result<Self, DataError> {
        let mut instruments = Vec::new();
        let mut candles = Vec::new();
        for id in ids {
            instruments.push(dir.read_instrument(id)?);
            candles.extend(dir.load_klines(id, interval, from, to)?);
        }
        Ok(Self::new(instruments, candles, fees, slippage))
    }

    /// The candles this provider replays, in no particular order.
    pub fn candles(&self) -> &[Candle] {
        &self.candles
    }

    pub fn instruments(&self) -> &[Instrument] {
        &self.instruments
    }
}

#[async_trait]
impl Provider for BacktestProvider {
    fn id(&self) -> &str {
        "backtest"
    }

    fn asset_classes(&self) -> &[AssetClass] {
        &[AssetClass::Spot]
    }

    async fn market_data(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        Ok(Box::new(KlineFeed::new(self.candles.clone())))
    }

    async fn broker(&self) -> Result<Box<dyn Broker>, ExchangeError> {
        Ok(Box::new(PaperBroker::new(
            self.instruments.clone(),
            Box::new(NextOpenFillModel),
            Box::new(self.slippage),
            Box::new(self.fees),
        )))
    }

    async fn resolve(&self, query: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError> {
        let InstrumentQuery::Ids(ids) = query;
        ids.iter()
            .map(|id| {
                self.instruments
                    .iter()
                    .find(|i| i.id == *id)
                    .cloned()
                    .ok_or_else(|| ExchangeError::UnknownInstrument(id.clone()))
            })
            .collect()
    }
}
