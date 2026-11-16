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
    /// Bars before the start, replayed first as backfill (history, no
    /// decisions, no fills).
    warmup: Vec<Candle>,
    paper: PaperConfig,
}

impl BacktestProvider {
    pub fn new(instruments: Vec<Instrument>, candles: Vec<Candle>, paper: PaperConfig) -> Self {
        Self {
            instruments,
            candles,
            warmup: Vec::new(),
            paper,
        }
    }

    /// Replays `candles` (bars before the backtest's first) as warm-up:
    /// they reach the strategy's history as backfill, as a live run's
    /// warm-up does, so the first decision sees the same history live
    /// would. The report starts after them.
    pub fn with_warmup(mut self, candles: Vec<Candle>) -> Self {
        self.warmup = candles;
        self
    }

    /// [`with_warmup`](Self::with_warmup) from the cache: per instrument,
    /// the last `bars` cached candles opening before `from`. Fewer if the
    /// cache doesn't reach back that far; see
    /// [`warmup_bars`](Self::warmup_bars).
    pub fn warm_up_from_cache(
        self,
        dir: &DataDir,
        interval: Interval,
        from: Timestamp,
        bars: usize,
    ) -> Result<Self, DataError> {
        let start = from.as_millis().saturating_sub(
            i64::try_from(bars)
                .unwrap_or(i64::MAX)
                .saturating_mul(interval.as_millis()),
        );
        let mut warmup = Vec::new();
        for i in &self.instruments {
            let before = dir.load_klines(&i.id, interval, Timestamp::from_millis(start), from)?;
            let skip = before.len().saturating_sub(bars);
            warmup.extend(before.into_iter().skip(skip));
        }
        Ok(self.with_warmup(warmup))
    }

    /// Warm-up bars per instrument, in instrument order.
    pub fn warmup_bars(&self) -> Vec<(InstrumentId, usize)> {
        self.instruments
            .iter()
            .map(|i| {
                let n = self.warmup.iter().filter(|c| c.instrument == i.id).count();
                (i.id.clone(), n)
            })
            .collect()
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

    /// The candles this provider replays as decisions (warm-up excluded),
    /// in no particular order.
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
        Ok(Box::new(KlineFeed::with_warmup(
            self.warmup.clone(),
            self.candles.clone(),
        )))
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
