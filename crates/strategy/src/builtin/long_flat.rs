//! Shared by the single-coin, long-or-flat indicator strategies
//! (`stochastic`, `supertrend`, `ichimoku`): what "long" means and how a
//! decision becomes a signal, so each strategy is only its rule.

use crate::{Context, StrategyError};
use bot_core::{Candle, Decimal, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size};

/// Lot-rounded units held, if a market sell of them would pass the venue's
/// filters at `c`'s close. `None` is cash: flat, or dust a buy's base fee
/// left behind (else the strategy could neither sell it nor buy again).
pub(crate) fn long_units(ctx: &Context, coin: &InstrumentId, c: &Candle) -> Option<Qty> {
    let inst = ctx.instrument(coin)?;
    let units = inst.round_qty(ctx.position(coin).qty());
    inst.check_market(units, c.close, Side::Sell)
        .ok()
        .map(|()| units)
}

/// Whether `c` is a decision for `coin`: its candle, and no order of ours
/// on it working (deciding again would double it).
pub(crate) fn deciding(ctx: &Context, coin: &InstrumentId, c: &Candle) -> bool {
    c.instrument == *coin && ctx.open_orders(coin).next().is_none()
}

/// The signal for a long-or-flat rule: from cash, `enter` buys `quote`
/// (quote-budget market buy); long, `exit` sells the whole lot-rounded
/// holding at market; otherwise nothing.
pub(crate) fn decide(
    ctx: &Context,
    coin: &InstrumentId,
    c: &Candle,
    quote: Notional,
    enter: bool,
    exit: bool,
) -> Vec<Signal> {
    match long_units(ctx, coin, c) {
        None if enter => vec![Signal::Submit {
            instrument: coin.clone(),
            side: Side::Buy,
            size: Size::Quote(quote),
            kind: OrderKind::Market,
        }],
        Some(units) if exit => vec![Signal::Submit {
            instrument: coin.clone(),
            side: Side::Sell,
            size: Size::Base(units),
            kind: OrderKind::Market,
        }],
        _ => Vec::new(),
    }
}

/// Checks the params every such strategy has.
pub(crate) fn check_common(
    name: &str,
    quote: Notional,
    fee: Decimal,
    slippage_bps: Decimal,
) -> Result<(), StrategyError> {
    let bad = |reason: &str| {
        Err(StrategyError::Params {
            strategy: name.to_owned(),
            reason: reason.to_owned(),
        })
    };
    if !quote.is_positive() {
        return bad("quote_amount must be positive");
    }
    if fee < Decimal::ZERO || fee >= Decimal::ONE {
        return bad("fee must be in [0, 1)");
    }
    if slippage_bps < Decimal::ZERO || slippage_bps >= Decimal::from(10_000) {
        return bad("slippage_bps must be in [0, 10000)");
    }
    Ok(())
}

/// The newest `n` candles of `coin`, oldest first; `None` with fewer.
pub(crate) fn newest<'a>(
    ctx: &'a Context,
    coin: &InstrumentId,
    n: usize,
) -> Option<Vec<&'a Candle>> {
    let h = ctx.history(coin);
    if h.len() < n {
        return None;
    }
    let mut v: Vec<&Candle> = h.rev().take(n).collect();
    v.reverse();
    Some(v)
}
