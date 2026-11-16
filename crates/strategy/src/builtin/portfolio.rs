//! Holding target weights of a fixed capital: shared by every strategy
//! that holds weights rather than fixed amounts (`rebalance`, `vol_target`
//! and the ones after them).
//!
//! A strategy can't see the account balance (invariant 5: only positions,
//! history, orders), and the account may hold more than the strategy is
//! meant to use. So it manages a stated `capital` in the quote asset and
//! derives its own cash from its coins' positions:
//!
//! `cash = capital − Σ (cost_basis − realized_pnl)`
//!
//! Every buy adds what it cost to the cost basis; every sell removes a
//! share of the basis and adds proceeds less that share to realized P&L. So
//! `cost_basis − realized_pnl` is exactly the net quote spent on the coin
//! since its books were empty, and the cash needs no state of its own:
//! restarts rebuild it from the books. A fee paid in a third asset (BNB) is
//! counted as if paid in quote, so the cash is then understated (the safe
//! side). Books that already held these coins before the strategy started
//! (another strategy's trades on the same db) shift the cash by their net;
//! state `capital` with that in mind.
//!
//! A `capital` larger than the account really holds can't be detected
//! beforehand: the venue refuses the buy (`-2010`), and the next evaluation
//! would ask again. [`Backoff`] spaces those retries out.

use crate::Context;
use bot_core::{
    Decimal, InstrumentId, Notional, OrderKind, Price, Qty, Side, Signal, Size, Timestamp,
};

/// Spaces out evaluations after the venue refused an order, so a `capital`
/// the account can't cover (or a venue refusing for any other reason)
/// isn't re-sent on every bar.
///
/// After k refused evaluations in a row, the next 2^k − 1 evaluations are
/// skipped (k capped at 6: at most 63); any fill resets it. Refusals from
/// the same evaluation (several orders, one close time) count once. Not
/// saved: a restart retries at once, which is when a user would have
/// fixed the balance.
#[derive(Debug, Default)]
pub(crate) struct Backoff {
    refusals: u32,
    skip: u32,
    last: Option<Timestamp>,
}

impl Backoff {
    /// An order from the evaluation at `now` was refused.
    pub fn refused(&mut self, now: Option<Timestamp>) {
        if now.is_some() && now == self.last {
            return;
        }
        self.last = now;
        self.refusals = (self.refusals + 1).min(6);
        self.skip = (1 << self.refusals) - 1;
    }

    /// Something filled: the venue is taking orders again.
    pub fn filled(&mut self) {
        *self = Self::default();
    }

    /// Call once per evaluation: true means skip this one.
    pub fn hold(&mut self) -> bool {
        if self.skip == 0 {
            return false;
        }
        self.skip -= 1;
        true
    }
}

/// True when nothing the strategy placed on any of `coins` can still fill:
/// none open, none being cancelled. A portfolio decision made while an
/// order works would double it.
pub(crate) fn idle<'a>(ctx: &Context, coins: impl IntoIterator<Item = &'a InstrumentId>) -> bool {
    coins.into_iter().all(|id| {
        ctx.open_orders(id).next().is_none() && ctx.cancelling_orders(id).next().is_none()
    })
}

/// The fee plus slippage fraction one market trade is assumed to cost.
pub(crate) fn trade_cost(fee: Decimal, slippage_bps: Decimal) -> Decimal {
    fee + slippage_bps / Decimal::from(10_000)
}

/// Checks the params every portfolio strategy shares; the reason on error.
pub(crate) fn check_common(
    capital: Notional,
    band: Decimal,
    fee: Decimal,
    slippage_bps: Decimal,
) -> Result<(), &'static str> {
    if !capital.is_positive() {
        return Err("capital must be positive");
    }
    if band <= Decimal::ZERO || band >= Decimal::ONE {
        return Err("band must be in (0, 1)");
    }
    if fee < Decimal::ZERO || fee >= Decimal::ONE {
        return Err("fee must be in [0, 1)");
    }
    if slippage_bps < Decimal::ZERO || slippage_bps >= Decimal::from(10_000) {
        return Err("slippage_bps must be in [0, 10000)");
    }
    Ok(())
}

/// Refuses an empty coin list or one naming a coin twice.
pub(crate) fn check_coins(coins: &[InstrumentId], at_least: usize) -> Result<(), String> {
    if coins.len() < at_least {
        return Err(format!("coins must name at least {at_least}"));
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(dup) = coins.iter().find(|c| !seen.insert(*c)) {
        return Err(format!("coins names {dup} twice"));
    }
    Ok(())
}

/// A coin and the fraction of the portfolio's value it should be.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Target {
    pub coin: InstrumentId,
    pub weight: Decimal,
}

/// The strategy's own quote cash; see the module docs.
pub(crate) fn own_cash<'a>(
    ctx: &Context,
    coins: impl IntoIterator<Item = &'a InstrumentId>,
    capital: Notional,
) -> Decimal {
    coins.into_iter().fold(capital.get(), |cash, id| {
        let p = ctx.position(id);
        cash - (p.cost_basis().get() - p.realized_pnl().get())
    })
}

/// Market orders that bring the holdings back to `targets`, or nothing if
/// every coin's weight is within `band` of its target.
///
/// Value is cash plus each coin at its last close. Once any coin has
/// drifted more than `band` (absolute: `"0.05"` = 5 points), every coin is
/// traded back to its target, so the next rebalance starts from the
/// targets rather than from the one that tripped. Sells come first; buys
/// spend cash plus the sells' estimated proceeds (close less `cost`, the
/// fee plus slippage fraction, less a tick per unit for rounding), scaled
/// down together if that isn't enough: a live venue would refuse the
/// excess (`-2010`). A trade the venue's filters would refuse (dust, under
/// minimum notional) is left out. A coin scheduled for delisting
/// (`Context::delisting`) is targeted at 0, whatever its weight: it is
/// sold now, on the strategy's terms, rather than by the engine's exit
/// before the delisting, and never bought. Once past its delisting it can't
/// be traded at all: its holding counts at its last close and no order is
/// placed on it. Needs every coin's instrument and last close; `None`
/// without them.
pub(crate) fn rebalance(
    ctx: &Context,
    targets: &[Target],
    capital: Notional,
    band: Decimal,
    cost: Decimal,
) -> Option<Vec<Signal>> {
    struct Row<'a> {
        t: &'a Target,
        inst: &'a bot_core::Instrument,
        close: Price,
        held: Qty,
        value: Decimal,
    }
    let leaving: Vec<Target> = targets
        .iter()
        .map(|t| Target {
            coin: t.coin.clone(),
            weight: if ctx.delisting(&t.coin).is_some() {
                Decimal::ZERO
            } else {
                t.weight
            },
        })
        .collect();
    let targets = &leaving[..];
    let rows: Vec<Row> = targets
        .iter()
        .map(|t| {
            let inst = ctx.instrument(&t.coin)?;
            let close = ctx.last_candle(&t.coin)?.close;
            let held = ctx.position(&t.coin).qty();
            let value = inst.notional(held, close).get();
            Some(Row {
                t,
                inst,
                close,
                held,
                value,
            })
        })
        .collect::<Option<_>>()?;
    let cash = own_cash(ctx, targets.iter().map(|t| &t.coin), capital);
    let total = cash + rows.iter().map(|r| r.value).sum::<Decimal>();
    if total <= Decimal::ZERO {
        return Some(Vec::new());
    }
    if rows
        .iter()
        .all(|r| (r.value / total - r.t.weight).abs() <= band)
    {
        return Some(Vec::new());
    }

    let mut out = Vec::new();
    let mut available = cash;
    let mut wants: Vec<(&Row, Decimal)> = Vec::new();
    // A coin already delisted can't be traded: its holding stays in the
    // total at its last close, and no order is placed on it.
    for r in rows.iter().filter(|r| !ctx.delisted(&r.t.coin)) {
        let delta = r.t.weight * total - r.value;
        if delta.is_sign_negative() && !delta.is_zero() {
            let Ok(q) = r.inst.qty_for(Notional::new(-delta), r.close) else {
                continue;
            };
            let q = r.inst.round_qty(q.min(r.held));
            if !q.is_positive() || r.inst.check_market(q, r.close, Side::Sell).is_err() {
                continue;
            }
            let per_unit = r.close.get() * (Decimal::ONE - cost) - r.inst.tick_size.get();
            available += q.get() * per_unit;
            out.push(Signal::Submit {
                instrument: r.t.coin.clone(),
                side: Side::Sell,
                size: Size::Base(q),
                kind: OrderKind::Market,
            });
        } else if delta.is_sign_positive() && !delta.is_zero() {
            wants.push((r, delta));
        }
    }
    let wanted: Decimal = wants.iter().map(|(_, d)| *d).sum();
    if available <= Decimal::ZERO || wanted.is_zero() {
        return Some(out);
    }
    let scale = (available / wanted).min(Decimal::ONE);
    for (r, delta) in wants {
        // Toward zero at 8 dp, as Binance's quoteOrderQty: never more.
        let budget = (delta * scale).trunc_with_scale(8);
        if budget < r.inst.min_notional.get() {
            continue;
        }
        out.push(Signal::Submit {
            instrument: r.t.coin.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(budget)),
            kind: OrderKind::Market,
        });
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{Params, driver};
    use bot_core::{Asset, Instrument, InstrumentSpec, Position};
    use rust_decimal_macros::dec;

    pub fn ethusdt() -> Instrument {
        Instrument::new(InstrumentSpec {
            id: "binance:ETHUSDT".parse().unwrap(),
            base: Asset::new("ETH").unwrap(),
            ..btcusdt().into()
        })
        .unwrap()
    }

    /// Candle `i` for ETH, flat at `price`.
    pub fn eth_candle(i: i64, price: &str) -> bot_core::Candle {
        let c = candle(i, price);
        bot_core::Candle::new(bot_core::CandleSpec {
            instrument: ethusdt().id.clone(),
            ..c.into()
        })
        .unwrap()
    }

    pub fn position(
        id: &InstrumentId,
        qty: Decimal,
        basis: Decimal,
        realized: Decimal,
    ) -> Position {
        Position::from_parts(
            id.clone(),
            Qty::new(qty),
            Notional::new(basis),
            Notional::new(realized),
        )
        .unwrap()
    }

    fn btc() -> InstrumentId {
        btcusdt().id.clone()
    }

    fn eth() -> InstrumentId {
        ethusdt().id.clone()
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt(), ethusdt()], Params::default(), 10)
    }

    fn half_half() -> Vec<Target> {
        vec![
            Target {
                coin: btc(),
                weight: dec!(0.5),
            },
            Target {
                coin: eth(),
                weight: dec!(0.25),
            },
        ]
    }

    fn sell(id: InstrumentId, q: Decimal) -> Signal {
        Signal::Submit {
            instrument: id,
            side: Side::Sell,
            size: Size::Base(Qty::new(q)),
            kind: OrderKind::Market,
        }
    }

    fn buy(id: InstrumentId, budget: Decimal) -> Signal {
        Signal::Submit {
            instrument: id,
            side: Side::Buy,
            size: Size::Quote(Notional::new(budget)),
            kind: OrderKind::Market,
        }
    }

    fn ts(ms: i64) -> Option<Timestamp> {
        Some(Timestamp::from_millis(ms))
    }

    /// Refused at evaluation 1: skip 1. Refused again: skip 3, then 7. Two
    /// refusals from one evaluation count once; a fill clears it all.
    #[test]
    fn backoff_doubles_the_wait_until_a_fill() {
        let mut b = Backoff::default();
        assert!(!b.hold(), "nothing refused yet");
        b.refused(ts(1));
        b.refused(ts(1));
        assert_eq!(
            (0..3).map(|_| b.hold()).collect::<Vec<_>>(),
            [true, false, false]
        );
        b.refused(ts(3));
        let waits: Vec<bool> = (0..4).map(|_| b.hold()).collect();
        assert_eq!(waits, [true, true, true, false]);
        b.refused(ts(7));
        assert_eq!((0..8).filter(|_| b.hold()).count(), 7);
        b.refused(ts(15));
        b.filled();
        assert!(!b.hold());
    }

    #[test]
    fn backoff_waits_at_most_63() {
        let mut b = Backoff::default();
        for i in 0..20 {
            b.refused(ts(i));
        }
        assert_eq!((0..100).filter(|_| b.hold()).count(), 63);
    }

    /// Bought 10 BTC for 1001 (fee included), sold 4 for 500 proceeds:
    /// basis left 1001·0.6 = 600.6, realized 500 − 400.4 = 99.6.
    /// Net spent 600.6 − 99.6 = 501 = 1001 − 500, so cash 10000 − 501.
    #[test]
    fn own_cash_is_capital_less_net_spent() {
        let mut ctx = ctx();
        driver::set_position(&mut ctx, position(&btc(), dec!(6), dec!(600.6), dec!(99.6)));
        let cash = own_cash(&ctx, [&btc(), &eth()], Notional::new(dec!(10000)));
        assert_eq!(cash, dec!(9499));
    }

    /// Flat with 10000: targets BTC 50%, ETH 25% (25% stays cash). Both are
    /// 0% → drifted; buy 5000 BTC, 2500 ETH.
    #[test]
    fn flat_buys_every_target() {
        let mut ctx = ctx();
        driver::push_candle(&mut ctx, candle(0, "100"));
        driver::push_candle(&mut ctx, eth_candle(0, "10"));
        let got = rebalance(
            &ctx,
            &half_half(),
            Notional::new(dec!(10000)),
            dec!(0.05),
            dec!(0.002),
        );
        assert_eq!(
            got,
            Some(vec![buy(btc(), dec!(5000)), buy(eth(), dec!(2500))])
        );
    }

    /// Holding 50 BTC (basis 5000) and 250 ETH (basis 2500), cash 2500.
    ///   BTC at 110: 5500; ETH at 10: 2500; total 10500.
    ///   BTC weight 0.5238 (drift 0.0238), ETH 0.2381 (0.0119): both
    ///   inside a 0.05 band → nothing.
    ///   BTC at 130: 6500; total 11500; BTC 0.5652 (0.0652 > 0.05) → trade:
    ///   BTC target 5750 → sell 750/130 = 5.769230… → 5.76923 (lot);
    ///   ETH target 2875 → buy 375. Cash 2500 covers it: unscaled.
    #[test]
    fn trades_everything_back_once_one_coin_leaves_the_band() {
        let mut ctx = ctx();
        driver::set_position(&mut ctx, position(&btc(), dec!(50), dec!(5000), dec!(0)));
        driver::set_position(&mut ctx, position(&eth(), dec!(250), dec!(2500), dec!(0)));
        driver::push_candle(&mut ctx, candle(0, "110"));
        driver::push_candle(&mut ctx, eth_candle(0, "10"));
        let cap = Notional::new(dec!(10000));
        assert_eq!(
            rebalance(&ctx, &half_half(), cap, dec!(0.05), dec!(0.002)),
            Some(vec![])
        );

        driver::push_candle(&mut ctx, candle(1, "130"));
        driver::push_candle(&mut ctx, eth_candle(1, "10"));
        let got = rebalance(&ctx, &half_half(), cap, dec!(0.05), dec!(0.002));
        assert_eq!(
            got,
            Some(vec![sell(btc(), dec!(5.76923)), buy(eth(), dec!(375))])
        );
    }

    /// Cash short of the buys: they shrink together. Holding 50 BTC at 100
    /// (5000) and 100 ETH at 10 (1000), but 9500 already spent (basis 9500,
    /// a loss on paper), so cash 500; total 6500. Targets 3250 / 1625:
    /// sell BTC 1750/100 = 17.5, proceeds est. 17.5·(100·0.998 − 0.01) =
    /// 1746.325; ETH wants 625; available 2246.325 > 625 → unscaled.
    /// Now ETH at weight 0.5 too (targets sum to 1): wants 2250 → scale
    /// 2246.325 / 2250 → buy 2246.325.
    #[test]
    fn buys_shrink_to_cash_plus_sale_proceeds() {
        let mut ctx = ctx();
        driver::set_position(&mut ctx, position(&btc(), dec!(50), dec!(8000), dec!(0)));
        driver::set_position(&mut ctx, position(&eth(), dec!(100), dec!(1500), dec!(0)));
        driver::push_candle(&mut ctx, candle(0, "100"));
        driver::push_candle(&mut ctx, eth_candle(0, "10"));
        let cap = Notional::new(dec!(10000));
        let got = rebalance(&ctx, &half_half(), cap, dec!(0.05), dec!(0.002));
        assert_eq!(
            got,
            Some(vec![sell(btc(), dec!(17.5)), buy(eth(), dec!(625))])
        );

        let even = vec![
            Target {
                coin: btc(),
                weight: dec!(0.5),
            },
            Target {
                coin: eth(),
                weight: dec!(0.5),
            },
        ];
        let got = rebalance(&ctx, &even, cap, dec!(0.05), dec!(0.002));
        assert_eq!(
            got,
            Some(vec![sell(btc(), dec!(17.5)), buy(eth(), dec!(2246.325))])
        );
    }

    /// Trades under the venue's filters are left out: ETH 0.25 of 10 total
    /// value wants a 2.5 buy, under the 5 minimum notional.
    #[test]
    fn leaves_out_what_the_venue_would_refuse() {
        let mut ctx = ctx();
        driver::push_candle(&mut ctx, candle(0, "100"));
        driver::push_candle(&mut ctx, eth_candle(0, "10"));
        let got = rebalance(
            &ctx,
            &half_half(),
            Notional::new(dec!(10)),
            dec!(0.05),
            dec!(0.002),
        );
        assert_eq!(got, Some(vec![buy(btc(), dec!(5))]));
    }

    #[test]
    fn idle_means_nothing_open_or_cancelling() {
        let mut ctx = ctx();
        assert!(idle(&ctx, [&btc(), &eth()]));
        let o = bot_core::Order::new(bot_core::OrderSpec {
            id: bot_core::OrderId::new("o-1").unwrap(),
            instrument: eth(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(10))),
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap();
        driver::set_cancelling_orders(&mut ctx, vec![o.clone()]);
        assert!(!idle(&ctx, [&btc(), &eth()]));
        assert!(idle(&ctx, [&btc()]));
        driver::set_cancelling_orders(&mut ctx, vec![]);
        driver::set_open_orders(&mut ctx, vec![o]);
        assert!(!idle(&ctx, [&eth()]));
    }

    #[test]
    fn common_checks() {
        let ok = |c: &str, b: &str, f: &str, s: &str| {
            check_common(
                Notional::new(c.parse().unwrap()),
                b.parse().unwrap(),
                f.parse().unwrap(),
                s.parse().unwrap(),
            )
            .is_ok()
        };
        assert!(ok("100", "0.05", "0.001", "5"));
        assert!(!ok("0", "0.05", "0.001", "5"));
        assert!(!ok("100", "0", "0.001", "5"));
        assert!(!ok("100", "1", "0.001", "5"));
        assert!(!ok("100", "0.05", "1", "5"));
        assert!(!ok("100", "0.05", "0.001", "10000"));
        assert_eq!(trade_cost(dec!(0.001), dec!(10)), dec!(0.002));
        assert!(check_coins(&[btc(), eth()], 2).is_ok());
        assert!(check_coins(&[btc()], 2).is_err());
        assert!(check_coins(&[btc(), btc()], 1).is_err());
    }

    #[test]
    fn needs_every_coins_last_close() {
        let mut ctx = ctx();
        driver::push_candle(&mut ctx, candle(0, "100"));
        let got = rebalance(
            &ctx,
            &half_half(),
            Notional::new(dec!(10000)),
            dec!(0.05),
            dec!(0.002),
        );
        assert_eq!(got, None);
    }

    /// As `trades_everything_back_once_one_coin_leaves_the_band` at 110 /
    /// 10 (in band), but BTC is scheduled for delisting: its target is 0, so
    /// all 50 BTC is sold now and ETH goes to its 0.25 of the 10500 total
    /// (2625: buy 125), funded by the sale.
    #[test]
    fn a_delisting_coin_is_targeted_at_zero() {
        let mut ctx = ctx();
        driver::set_position(&mut ctx, position(&btc(), dec!(50), dec!(5000), dec!(0)));
        driver::set_position(&mut ctx, position(&eth(), dec!(250), dec!(2500), dec!(0)));
        driver::push_candle(&mut ctx, candle(0, "110"));
        driver::push_candle(&mut ctx, eth_candle(0, "10"));
        driver::set_delisting(
            &mut ctx,
            btc(),
            Some(Timestamp::from_millis(1_000 * 3_600_000)),
        );
        let got = rebalance(
            &ctx,
            &half_half(),
            Notional::new(dec!(10000)),
            dec!(0.05),
            dec!(0.002),
        );
        assert_eq!(
            got,
            Some(vec![sell(btc(), dec!(50)), buy(eth(), dec!(125))])
        );
    }

    /// BTC is past its delisting (stuck holding of 50 at 110 = 5500): no
    /// order on it. It still counts in the total (10500), so ETH goes to
    /// 0.25 of it: 2625, buy 125 from the 2500 cash.
    #[test]
    fn a_delisted_coin_is_never_traded() {
        let mut ctx = ctx();
        driver::set_position(&mut ctx, position(&btc(), dec!(50), dec!(5000), dec!(0)));
        driver::set_position(&mut ctx, position(&eth(), dec!(250), dec!(2500), dec!(0)));
        driver::push_candle(&mut ctx, candle(0, "110"));
        driver::push_candle(&mut ctx, eth_candle(0, "10"));
        driver::set_delisting(&mut ctx, btc(), Some(Timestamp::from_millis(1)));
        driver::set_now(&mut ctx, Timestamp::from_millis(2));
        let got = rebalance(
            &ctx,
            &half_half(),
            Notional::new(dec!(10000)),
            dec!(0.05),
            dec!(0.002),
        );
        assert_eq!(got, Some(vec![buy(eth(), dec!(125))]));
    }
}
