//! Holding target weights of a fixed capital: shared by `rebalance` and
//! `vol_target`.
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

use crate::Context;
use bot_core::{Decimal, InstrumentId, Notional, OrderKind, Price, Qty, Side, Signal, Size};

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
/// minimum notional) is left out. Needs every coin's instrument and last
/// close; `None` without them.
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
    for r in &rows {
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
}
