use crate::{DataDir, DataError, KlineFeed};
use async_trait::async_trait;
use bot_core::{AssetClass, Candle, Instrument, InstrumentId, Interval, Notional, Timestamp};
use exchange::paper::{BpsSlippage, NextOpenFillModel, PaperBroker, RateFee};
use exchange::{Broker, ExchangeError, InstrumentQuery, MarketData, Provider};

/// How the backtest's paper broker trades: its fee and slippage models and
/// the quote it starts with.
#[derive(Clone, Copy, Debug)]
pub struct PaperConfig {
    pub fees: RateFee,
    pub slippage: BpsSlippage,
    /// The broker's starting balance (buys beyond it are refused) and the
    /// report's starting equity.
    pub initial_cash: Notional,
}

/// Backtest venue: cached candles as the feed, a [`PaperBroker`] with the
/// default [`NextOpenFillModel`] as the broker. The engine can't tell it
/// from a live provider, which is the point (invariant 3).
///
/// The broker trades from the initial cash in the first instrument's quote
/// asset and refuses buys it can't cover, as Binance would. The report
/// values everything in that one quote, so a universe mixing quote assets
/// gets its other-quoted orders refused.
pub struct BacktestProvider {
    instruments: Vec<Instrument>,
    candles: Vec<Candle>,
    paper: PaperConfig,
}

impl BacktestProvider {
    pub fn new(instruments: Vec<Instrument>, candles: Vec<Candle>, paper: PaperConfig) -> Self {
        Self {
            instruments,
            candles,
            paper,
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
        paper: PaperConfig,
    ) -> Result<Self, DataError> {
        let mut instruments = Vec::new();
        let mut candles = Vec::new();
        for id in ids {
            instruments.push(dir.read_instrument(id)?);
            candles.extend(dir.load_klines(id, interval, from, to)?);
        }
        Ok(Self::new(instruments, candles, paper))
    }

    /// The candles this provider replays, in no particular order.
    pub fn candles(&self) -> &[Candle] {
        &self.candles
    }

    pub fn instruments(&self) -> &[Instrument] {
        &self.instruments
    }

    /// Quote the broker starts with; the report's starting equity.
    pub fn initial_cash(&self) -> Notional {
        self.paper.initial_cash
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
        let broker = PaperBroker::new(
            self.instruments.clone(),
            Box::new(NextOpenFillModel),
            Box::new(self.paper.slippage),
            Box::new(self.paper.fees),
        );
        Ok(Box::new(match self.instruments.first() {
            Some(i) => broker.with_cash(i.quote.clone(), self.paper.initial_cash),
            None => broker,
        }))
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
