//! Moving a signed position (long > 0, short < 0) toward a signed target,
//! one market order at a time: shared by the long/short strategies.
//!
//! A flip closes first, reduce-only, and the new side opens at the next
//! decision, from flat. One order for both would need margin for the whole
//! flip; after a loss the broker refuses it and the losing position stays
//! open, refused bar after bar (seen in a 2023 tsmom_long_short backtest).
//! A reduce-only close needs no margin, so an exit always goes. Exits and
//! size cuts are reduce-only too; a same-side resize happens only when the
//! target is more than `band` (fraction of the target) away.

use bot_core::{Candle, Decimal, Instrument, OrderKind, Qty, Side, Signal, Size};

/// Signed lot-rounded units held at `c`'s close; dust (no market order the
/// venue would take) is flat.
pub(crate) fn held(inst: &Instrument, raw: Decimal, c: &Candle) -> Decimal {
    let units = inst.round_qty(Qty::new(raw.abs()));
    let closing = if raw.is_sign_negative() {
        Side::Buy
    } else {
        Side::Sell
    };
    if inst.check_market(units, c.close, closing).is_err() {
        Decimal::ZERO
    } else if raw.is_sign_negative() {
        -units.get()
    } else {
        units.get()
    }
}

/// The one order that moves `held` toward `target` (both signed,
/// lot-rounded) on `inst` at `c`'s close, or `None`: already there, a
/// same-side drift within `band`, or an order the venue wouldn't take.
pub(crate) fn step(
    inst: &Instrument,
    c: &Candle,
    held: Decimal,
    target: Decimal,
    band: Decimal,
) -> Option<Signal> {
    let delta = target - held;
    if delta.is_zero() {
        return None;
    }
    let same_side =
        held.is_zero() || target.is_zero() || (held > Decimal::ZERO) == (target > Decimal::ZERO);
    let entering = held.is_zero();
    let flipping = !same_side;
    // Same side, not entering: resize only beyond the band.
    if same_side && !entering && !target.is_zero() && delta.abs() <= band * target.abs() {
        return None;
    }
    // A flip closes the whole position now; the new side opens at the next
    // decision, from flat.
    let delta = if flipping { -held } else { delta };
    let side = if delta > Decimal::ZERO {
        Side::Buy
    } else {
        Side::Sell
    };
    let qty = Qty::new(delta.abs());
    inst.check_market(qty, c.close, side).ok()?;
    // Toward zero (exit, cut, a flip's close) is reduce-only; entries and
    // same-side growth open.
    let reducing = !entering && (flipping || target.abs() < held.abs());
    let instrument = inst.id.clone();
    Some(if reducing {
        Signal::Reduce {
            instrument,
            side,
            qty,
            kind: OrderKind::Market,
        }
    } else {
        Signal::Submit {
            instrument,
            side,
            size: Size::Base(qty),
            kind: OrderKind::Market,
        }
    })
}
