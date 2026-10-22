//! `ratio_hop`: coin accumulation by rotation, after `ccxt/binance-trade-bot`.
//!
//! Hold one coin. When the units of another coin obtainable by selling the
//! current one through the bridge (quote) asset, after both legs' fees,
//! beat the units last held of that coin, rotate `A → bridge → B`. Unit
//! counts therefore only ratchet up per coin. That maximizes units, not
//! value: a coin can be accumulated all the way down. The freefall guard
//! the original lacks refuses to rotate into a coin that fell more than
//! `freefall_pct` over the last `freefall_candles` candles.
//!
//! Strategy state (phase, baselines) is not persisted. Started with a coin
//! already held (a live restart: the engine seeds positions from the
//! broker's books), it skips the initial buy and resumes holding that coin,
//! the most valuable one if several are; baselines restart from that
//! evaluation's ratios, so the ratchet's history is lost. A restart between
//! the legs of a rotation (sold, not yet bought) holds no coin and buys
//! afresh with `quote_amount`; the earlier proceeds stay in the bridge
//! asset.
//!
//! Decisions use only closed candles and positions read through `Context`
//! (invariants 2 and 5). Expected edge is unverified: rotation pays two
//! fees and the spread each hop, so it only wins when relative moves
//! between the coins mean-revert by more than that.

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{
    Candle, Decimal, Fee, Fill, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size,
    TimeInForce, Timestamp,
};
use serde::Deserialize;
use std::collections::HashMap;

const NAME: &str = "ratio_hop";
/// Evaluations a leg may stay unfilled before the strategy stops. Strategies
/// can't see rejections yet, so a refused leg would otherwise wait forever.
const MAX_WAIT: u32 = 2;

/// How each leg is placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OrderType {
    /// Taker: fills next open plus slippage; certain but pays the spread.
    Market,
    /// Maker: GTC limit at the last close; fills only if price trades
    /// through it, never worse than the limit, maybe never.
    Limit,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RatioHopParams {
    /// Coins to rotate between, all quoted in the same bridge asset. The
    /// first is bought at the start.
    coins: Vec<InstrumentId>,
    /// Quote spent on the initial buy.
    quote_amount: Notional,
    /// Fee per leg assumed when estimating a rotation (`"0.001"`).
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per leg assumed when estimating a rotation, in basis
    /// points (`"5"`). Required: market orders never fill at the close the
    /// decision saw, and a silent 0 would make every hop look cheaper than
    /// it is. Match the backtest's `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
    /// Extra improvement over the baseline required to rotate (`"0.002"`).
    #[serde(deserialize_with = "exact_decimal", default)]
    scout_margin: Decimal,
    /// Refuse a target that fell more than this fraction (`"0.2"` = 20%)…
    #[serde(deserialize_with = "exact_decimal")]
    freefall_pct: Decimal,
    /// …over this many candles. Engine `history_len` must exceed it.
    freefall_candles: usize,
    /// `"market"` or `"limit"`. Required: the two have different costs and
    /// fill risk, so neither is a safe silent default.
    order_type: OrderType,
    /// Limit mode only: evaluations a leg may rest unfilled before it is
    /// cancelled and re-priced at the new close.
    limit_timeout: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Start,
    Buying {
        to: InstrumentId,
        /// Quote this leg may spend: the start amount or the sell proceeds.
        budget: Notional,
        waited: u32,
    },
    Holding(InstrumentId),
    Selling {
        from: InstrumentId,
        to: InstrumentId,
        waited: u32,
    },
    /// Stopped: bad universe, or a leg never filled.
    Stopped,
}

/// See the module docs.
pub struct RatioHop {
    p: RatioHopParams,
    phase: Phase,
    /// Units of each coin a rotation into it must beat.
    baseline: HashMap<InstrumentId, Qty>,
    last_eval: Option<Timestamp>,
}

impl RatioHop {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: RatioHopParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        let mut uniq = p.coins.clone();
        uniq.sort();
        uniq.dedup();
        if p.coins.len() < 2 || uniq.len() != p.coins.len() {
            return bad("coins: need at least two, no duplicates");
        }
        if !p.quote_amount.is_positive() {
            return bad("quote_amount must be positive");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        if p.scout_margin < Decimal::ZERO {
            return bad("scout_margin must be >= 0");
        }
        if p.freefall_pct <= Decimal::ZERO || p.freefall_pct >= Decimal::ONE {
            return bad("freefall_pct must be in (0, 1)");
        }
        if p.freefall_candles == 0 {
            return bad("freefall_candles must be >= 1");
        }
        if p.order_type == OrderType::Limit && !p.limit_timeout.is_some_and(|t| t >= 1) {
            return bad("limit_timeout (>= 1) is required with order_type = \"limit\"");
        }
        Ok(Box::new(Self {
            p,
            phase: Phase::Start,
            baseline: HashMap::new(),
            last_eval: None,
        }))
    }

    /// One leg as `order_type` says: a market order, or a GTC limit at the
    /// instrument's last known close. `None` if there's no close yet.
    fn order(
        &self,
        ctx: &Context,
        instrument: &InstrumentId,
        side: Side,
        size: Size,
    ) -> Option<Signal> {
        let kind = match self.p.order_type {
            OrderType::Market => OrderKind::Market,
            OrderType::Limit => OrderKind::Limit {
                price: ctx.last_candle(instrument)?.close,
                tif: TimeInForce::Gtc,
            },
        };
        Some(Signal::Submit {
            instrument: instrument.clone(),
            side,
            size,
            kind,
        })
    }

    /// Cancels for every open order of ours on `id`.
    fn cancels(ctx: &Context, id: &InstrumentId) -> Vec<Signal> {
        ctx.open_orders(id)
            .map(|o| Signal::Cancel(o.id.clone()))
            .collect()
    }

    /// The leg has now rested `waited + 1` evaluations, reaching the limit.
    fn timed_out(&self, waited: u32) -> bool {
        self.p.limit_timeout.is_some_and(|t| waited + 1 >= t)
    }

    /// The coin to resume holding: of the coins held in a sellable amount
    /// (venue filters at the last close), the one worth most at that close.
    /// `None` when flat, as every backtest starts.
    fn held_coin(&self, ctx: &Context) -> Option<InstrumentId> {
        self.p
            .coins
            .iter()
            .filter_map(|id| {
                let inst = ctx.instrument(id)?;
                let units = Self::sellable(ctx, id)?;
                let close = ctx.last_candle(id)?.close;
                inst.check_order(units, close).ok()?;
                Some((inst.notional(units, close), id))
            })
            .max_by(|x, y| x.0.cmp(&y.0))
            .map(|(_, id)| id.clone())
    }

    /// Lot-rounded units of `id` held, i.e. what a sell can actually move.
    fn sellable(ctx: &Context, id: &InstrumentId) -> Option<Qty> {
        Some(ctx.instrument(id)?.round_qty(ctx.position(id).qty()))
    }

    /// Units of `to` held after selling `units` of `from` and buying `to`
    /// with the proceeds, starting from current closes. Each market leg
    /// pays slippage (sell at close*(1-s) rounded down to tick, buy at
    /// close*(1+s) rounded up, as the paper broker does); a limit leg fills
    /// at its limit, the close, so pays none. Every leg pays `fee`: the
    /// sell fee in quote, the buy fee in base (Binance spot).
    fn estimate(
        &self,
        ctx: &Context,
        from: &InstrumentId,
        units: Qty,
        to: &InstrumentId,
    ) -> Option<Qty> {
        let (fi, ti) = (ctx.instrument(from)?, ctx.instrument(to)?);
        let s = match self.p.order_type {
            OrderType::Market => self.p.slippage_bps / Decimal::from(10_000),
            OrderType::Limit => Decimal::ZERO,
        };
        // round_price(.., Buy) rounds down, (.., Sell) up: against the trader.
        let pf = fi.round_price(ctx.last_candle(from)?.close * (Decimal::ONE - s), Side::Buy);
        let pt = ti.round_price(ctx.last_candle(to)?.close * (Decimal::ONE + s), Side::Sell);
        let keep = Decimal::ONE - self.p.fee;
        let proceeds = fi.notional(units, pf) * keep;
        let bought = ti.round_qty(ti.qty_for(proceeds, pt).ok()?);
        Some(bought * keep)
    }

    /// True if `id` fell more than `freefall_pct` over the last
    /// `freefall_candles` candles, or there isn't that much history yet.
    fn in_freefall(&self, ctx: &Context, id: &InstrumentId) -> bool {
        let n = self.p.freefall_candles;
        let hist: Vec<&Candle> = ctx.history(id).collect();
        if hist.len() <= n {
            return true;
        }
        let then = hist[hist.len() - 1 - n].close;
        let now = hist[hist.len() - 1].close;
        now / then - Decimal::ONE < -self.p.freefall_pct
    }

    fn evaluate(&mut self, ctx: &mut Context) -> Vec<Signal> {
        match self.phase.clone() {
            Phase::Start => {
                if let Some(held) = self.held_coin(ctx) {
                    self.phase = Phase::Holding(held.clone());
                    return self.consider_rotation(ctx, &held);
                }
                let first = self.p.coins[0].clone();
                let budget = self.p.quote_amount;
                let Some(buy) = self.order(ctx, &first, Side::Buy, Size::Quote(budget)) else {
                    return Vec::new();
                };
                self.phase = Phase::Buying {
                    to: first,
                    budget,
                    waited: 0,
                };
                vec![buy]
            }
            Phase::Buying { to, budget, waited } => match self.p.order_type {
                // A stale limit buy: cancel and re-price at the new close,
                // same budget, so the leg follows the market.
                OrderType::Limit if self.timed_out(waited) => {
                    let mut out = Self::cancels(ctx, &to);
                    out.extend(self.order(ctx, &to, Side::Buy, Size::Quote(budget)));
                    self.phase = Phase::Buying {
                        to,
                        budget,
                        waited: 0,
                    };
                    out
                }
                OrderType::Market if waited >= MAX_WAIT => {
                    self.phase = Phase::Stopped;
                    Vec::new()
                }
                _ => {
                    self.phase = Phase::Buying {
                        to,
                        budget,
                        waited: waited + 1,
                    };
                    Vec::new()
                }
            },
            Phase::Selling { from, to, waited } => match self.p.order_type {
                // A stale limit sell: cancel and keep holding; the next
                // evaluation decides afresh at the new prices.
                OrderType::Limit if self.timed_out(waited) => {
                    self.phase = Phase::Holding(from.clone());
                    Self::cancels(ctx, &from)
                }
                OrderType::Market if waited >= MAX_WAIT => {
                    self.phase = Phase::Stopped;
                    Vec::new()
                }
                _ => {
                    self.phase = Phase::Selling {
                        from,
                        to,
                        waited: waited + 1,
                    };
                    Vec::new()
                }
            },
            Phase::Holding(a) => self.consider_rotation(ctx, &a),
            Phase::Stopped => Vec::new(),
        }
    }

    fn consider_rotation(&mut self, ctx: &Context, a: &InstrumentId) -> Vec<Signal> {
        let Some(units) = Self::sellable(ctx, a).filter(|u| u.is_positive()) else {
            return Vec::new();
        };
        if self.baseline.is_empty() {
            // First look after the initial buy: today's ratios are the bar
            // every later rotation must beat.
            self.baseline.insert(a.clone(), units);
            for b in self.p.coins.iter().filter(|b| *b != a) {
                if let Some(est) = self.estimate(ctx, a, units, b) {
                    self.baseline.insert(b.clone(), est);
                }
            }
            return Vec::new();
        }
        let need = Decimal::ONE + self.p.scout_margin;
        let best = self
            .p
            .coins
            .iter()
            .filter(|b| *b != a && !self.in_freefall(ctx, b))
            .filter_map(|b| {
                let est = self.estimate(ctx, a, units, b)?;
                let base = *self.baseline.get(b)?;
                (base.is_positive() && est > base * need).then(|| (est / base, b))
            })
            .max_by(|x, y| x.0.cmp(&y.0));
        let Some((_, b)) = best else {
            return Vec::new();
        };
        let b = b.clone();
        self.baseline.insert(a.clone(), units);
        self.phase = Phase::Selling {
            from: a.clone(),
            to: b,
            waited: 0,
        };
        self.order(ctx, a, Side::Sell, Size::Base(units))
            .into_iter()
            .collect()
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: RatioHop::build }
}

impl Strategy for RatioHop {
    fn name(&self) -> &str {
        NAME
    }

    fn on_start(&mut self, ctx: &mut Context) {
        let quotes: Option<Vec<_>> = self
            .p
            .coins
            .iter()
            .map(|id| ctx.instrument(id).map(|i| i.quote.clone()))
            .collect();
        let one_bridge = quotes.is_some_and(|q| q.windows(2).all(|w| w[0] == w[1]));
        if !one_bridge {
            // A coin outside the universe or a second quote asset: the
            // ratios would compare different bridges. Refuse to trade.
            self.phase = Phase::Stopped;
        }
    }

    /// Evaluates once per close time, when every coin's candle for it has
    /// arrived, so no coin is judged on a stale price.
    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let t = c.close_time;
        if !self.p.coins.contains(&c.instrument) || self.last_eval == Some(t) {
            return Vec::new();
        }
        let complete = self
            .p
            .coins
            .iter()
            .all(|id| ctx.last_candle(id).is_some_and(|l| l.close_time == t));
        if !complete {
            return Vec::new();
        }
        self.last_eval = Some(t);
        self.evaluate(ctx)
    }

    fn on_fill(&mut self, ctx: &mut Context, f: &Fill) {
        match self.phase.clone() {
            Phase::Selling { from, to, .. } if f.instrument == from && f.side == Side::Sell => {
                let Some(inst) = ctx.instrument(&from) else {
                    self.phase = Phase::Stopped;
                    return;
                };
                let fee = match &f.fee {
                    Fee::Quote(n) => *n,
                    Fee::Base(_) | Fee::Other { .. } => Notional::ZERO,
                };
                let proceeds = inst.notional(f.qty, f.price) - fee;
                // Spend exactly what the sell raised, never more.
                match self.order(ctx, &to, Side::Buy, Size::Quote(proceeds)) {
                    Some(buy) => {
                        ctx.submit(buy);
                        self.phase = Phase::Buying {
                            to,
                            budget: proceeds,
                            waited: 0,
                        };
                    }
                    None => self.phase = Phase::Stopped,
                }
            }
            Phase::Buying { to, .. } if f.instrument == to && f.side == Side::Buy => {
                self.phase = Phase::Holding(to);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build, names};

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "market"
               {extra}"#
        ))
        .unwrap()
    }

    #[test]
    fn limit_mode_builds_with_timeout() {
        assert!(build(NAME, &Params::from_toml(&LIMIT.replace("X", "")).unwrap()).is_ok());
    }

    const LIMIT: &str = r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "limit"
               limit_timeout = 3X"#;

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params("")).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        for bad in [
            r#"coins = ["binance:BTCUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "market""#,
            r#"coins = ["binance:BTCUSDT", "binance:BTCUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "market""#,
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = 0.001
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "market""#,
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "1.5"
               freefall_candles = 2
               order_type = "market""#,
            // slippage_bps missing: must be explicit.
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "market""#,
            // order_type missing: must be explicit.
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2"#,
            // limit without limit_timeout.
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "limit""#,
        ] {
            assert!(
                build(NAME, &Params::from_toml(bad).unwrap()).is_err(),
                "{bad}"
            );
        }
    }

    mod resume {
        use super::*;
        use crate::driver;
        use bot_core::{
            Asset, AssetClass, CandleSpec, Instrument, InstrumentSpec, Interval, Position, Price,
        };
        use rust_decimal_macros::dec;

        fn inst(symbol: &str, base: &str, lot: Decimal) -> Instrument {
            Instrument::new(InstrumentSpec {
                id: format!("binance:{symbol}").parse().unwrap(),
                class: AssetClass::Spot,
                base: Asset::new(base).unwrap(),
                quote: Asset::new("USDT").unwrap(),
                tick_size: Price::new(dec!(0.01)),
                lot_size: Qty::new(lot),
                min_qty: Qty::new(lot),
                min_notional: Notional::new(dec!(5)),
            })
            .unwrap()
        }

        fn btc() -> Instrument {
            inst("BTCUSDT", "BTC", dec!(0.00001))
        }

        fn eth() -> Instrument {
            inst("ETHUSDT", "ETH", dec!(0.0001))
        }

        fn held(i: &Instrument, qty: Decimal, cost: Decimal) -> Position {
            Position::from_parts(
                i.id.clone(),
                Qty::new(qty),
                Notional::new(cost),
                Notional::ZERO,
            )
            .unwrap()
        }

        const HOUR: i64 = 3_600_000;

        /// One hourly close of both coins, fed the way the engine does;
        /// returns every signal the strategy emitted.
        fn close(
            s: &mut dyn Strategy,
            ctx: &mut Context,
            n: i64,
            btc_px: Decimal,
            eth_px: Decimal,
        ) -> Vec<Signal> {
            let mut out = Vec::new();
            for (i, px) in [(btc(), btc_px), (eth(), eth_px)] {
                let c = Candle::new(CandleSpec {
                    instrument: i.id.clone(),
                    interval: Interval::H1,
                    open_time: Timestamp::from_millis(n * HOUR),
                    close_time: Timestamp::from_millis((n + 1) * HOUR - 1),
                    open: Price::new(px),
                    high: Price::new(px),
                    low: Price::new(px),
                    close: Price::new(px),
                    volume: Qty::new(dec!(1)),
                    quote_volume: Notional::new(px),
                })
                .unwrap();
                driver::set_now(ctx, c.close_time);
                driver::push_candle(ctx, c.clone());
                out.extend(s.on_candle(ctx, &c));
                out.extend(driver::take_signals(ctx));
            }
            out
        }

        fn start(positions: Vec<Position>) -> (Box<dyn Strategy>, Context) {
            let p = params("");
            let mut ctx = driver::new(vec![btc(), eth()], p.clone(), 10);
            for pos in positions {
                driver::set_position(&mut ctx, pos);
            }
            let mut s = build(NAME, &p).unwrap();
            s.on_start(&mut ctx);
            (s, ctx)
        }

        fn sell_btc(qty: Decimal) -> Signal {
            Signal::Submit {
                instrument: btc().id.clone(),
                side: Side::Sell,
                size: Size::Base(Qty::new(qty)),
                kind: OrderKind::Market,
            }
        }

        /// Hand-worked, fee 0.001, slippage 10 bps, BTC flat at 60000:
        /// selling 0.01 BTC fills at 59940 -> 599.4 * 0.999 = 598.8006 USDT.
        /// t0, ETH 3000 (buy at 3003): lot(0.199400...) = 0.1994 * 0.999 =
        /// 0.1992006 ETH, the baseline. t2, ETH 2900 (buy at 2902.90):
        /// lot(0.206276...) = 0.2062 * 0.999 = 0.2059938 ETH > 0.1992006,
        /// and 2900/3000 is no freefall at 20%: rotate, selling 0.01 BTC.
        #[test]
        fn restart_holding_a_coin_resumes_instead_of_buying() {
            let (mut s, mut ctx) = start(vec![held(&btc(), dec!(0.01), dec!(600))]);
            assert!(close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000)).is_empty());
            assert!(close(s.as_mut(), &mut ctx, 1, dec!(60000), dec!(3000)).is_empty());
            assert_eq!(
                close(s.as_mut(), &mut ctx, 2, dec!(60000), dec!(2900)),
                vec![sell_btc(dec!(0.01))]
            );
        }

        #[test]
        fn flat_start_still_buys_the_first_coin() {
            let (mut s, mut ctx) = start(vec![]);
            assert_eq!(
                close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000)),
                vec![Signal::Submit {
                    instrument: btc().id.clone(),
                    side: Side::Buy,
                    size: Size::Quote(Notional::new(dec!(1000))),
                    kind: OrderKind::Market,
                }]
            );
        }

        /// 0.01 BTC (600 USDT) outweighs 0.1 ETH (300 USDT): BTC is the
        /// held coin, so the t2 rotation sells BTC.
        #[test]
        fn several_coins_held_resumes_the_most_valuable() {
            let (mut s, mut ctx) = start(vec![
                held(&btc(), dec!(0.01), dec!(600)),
                held(&eth(), dec!(0.1), dec!(300)),
            ]);
            close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000));
            close(s.as_mut(), &mut ctx, 1, dec!(60000), dec!(3000));
            assert_eq!(
                close(s.as_mut(), &mut ctx, 2, dec!(60000), dec!(2900)),
                vec![sell_btc(dec!(0.01))]
            );
        }

        /// 0.00001 BTC is worth 0.6 USDT, under the 5 USDT minimum: it
        /// can't be sold, so it isn't a holding to resume.
        #[test]
        fn unsellable_dust_is_not_a_holding() {
            let (mut s, mut ctx) = start(vec![held(&btc(), dec!(0.00001), dec!(0.6))]);
            let first = close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000));
            assert!(matches!(
                first.as_slice(),
                [Signal::Submit {
                    side: Side::Buy,
                    ..
                }]
            ));
        }
    }
}
