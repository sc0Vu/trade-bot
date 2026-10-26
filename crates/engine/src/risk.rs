//! Turns a strategy's [`Signal`](bot_core::Signal) into an [`Order`] a broker will accept.
//!
//! v1 risk is sizing and venue filters only: whitelist membership, quote
//! amount to base units, rounding to lot and tick, and
//! `Instrument::check_order`. A quote-sized market buy stays a budget: the
//! broker sizes it at the fill price, so it can't overspend when the price
//! moves between decision and fill. Position limits and the freefall guard come
//! later and slot in here.

use bot_core::{
    FilterError, Instrument, Order, OrderId, OrderKind, OrderSpec, Price, Side, Size, Timestamp,
};
use std::fmt;

/// Why a signal didn't become an order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RiskRejection {
    /// The instrument isn't in the resolved whitelist.
    NotInUniverse,
    /// A market order (or a quote-sized one) needs a reference price, and
    /// there is no candle for the instrument yet.
    NoReferencePrice,
    /// The sized, rounded order breaks a venue filter.
    Filter(FilterError),
    Invalid(bot_core::Error),
}

impl fmt::Display for RiskRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInUniverse => f.write_str("instrument not in universe"),
            Self::NoReferencePrice => f.write_str("no reference price yet"),
            Self::Filter(e) => write!(f, "{e}"),
            Self::Invalid(e) => write!(f, "{e}"),
        }
    }
}

/// The price an order is sized and checked at: its own limit for resting
/// kinds, else the last close as `Instrument::market_price` rounds it (a
/// market order sends no price; checking an off-tick close raw refused
/// every order on history fetched with today's filters).
fn order_price(
    i: &Instrument,
    side: Side,
    kind: OrderKind,
    reference: Option<Price>,
) -> Option<Price> {
    match kind {
        OrderKind::Market => reference.map(|p| i.market_price(p, side)),
        OrderKind::Limit { price, .. } => Some(price),
        OrderKind::StopMarket { stop } => Some(stop),
        OrderKind::StopLimit { limit, .. } => Some(limit),
    }
}

/// Rounds an order's prices to tick so rounding never makes it act sooner
/// or more aggressively: limits toward the passive side, stops away from
/// the market (a buy stop up, a sell stop down).
fn round_kind(i: &Instrument, side: Side, kind: OrderKind) -> OrderKind {
    let away = match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    };
    match kind {
        OrderKind::Market => OrderKind::Market,
        OrderKind::Limit { price, tif } => OrderKind::Limit {
            price: i.round_price(price, side),
            tif,
        },
        OrderKind::StopMarket { stop } => OrderKind::StopMarket {
            stop: i.round_price(stop, away),
        },
        OrderKind::StopLimit { stop, limit, tif } => OrderKind::StopLimit {
            stop: i.round_price(stop, away),
            limit: i.round_price(limit, side),
            tif,
        },
    }
}

/// Builds the order for one `Submit` signal.
///
/// `reference` is the instrument's last close; `now` is the time of the
/// event the signal was decided on, which becomes the order's `created_at`
/// so the broker can't fill it on that same event.
pub fn to_order(
    id: OrderId,
    instrument: Option<&Instrument>,
    side: Side,
    size: Size,
    kind: OrderKind,
    reference: Option<Price>,
    now: Timestamp,
) -> Result<Order, RiskRejection> {
    let i = instrument.ok_or(RiskRejection::NotInUniverse)?;
    let kind = round_kind(i, side, kind);
    let price = order_price(i, side, kind, reference).ok_or(RiskRejection::NoReferencePrice)?;
    let raw = match size {
        Size::Base(q) => q,
        Size::Quote(n) => i.qty_for(n, price).map_err(RiskRejection::Filter)?,
    };
    let qty = i.round_qty(raw);
    // For a budget buy this checks the size it would have at the reference
    // price; the broker re-checks at the real fill price.
    i.check_order(qty, price).map_err(RiskRejection::Filter)?;
    let size = match (size, kind, side) {
        (Size::Quote(budget), OrderKind::Market, Side::Buy) => Size::Quote(budget),
        _ => Size::Base(qty),
    };
    Order::new(OrderSpec {
        id,
        instrument: i.id.clone(),
        side,
        size,
        kind,
        created_at: now,
    })
    .map_err(RiskRejection::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::btcusdt;
    use bot_core::{Notional, Qty, TimeInForce};
    use rust_decimal_macros::dec;

    fn go(size: Size, kind: OrderKind, reference: Option<Price>) -> Result<Order, RiskRejection> {
        to_order(
            OrderId::new("o-1").unwrap(),
            Some(&btcusdt()),
            Side::Buy,
            size,
            kind,
            reference,
            Timestamp::from_millis(0),
        )
    }

    #[test]
    fn quote_market_buy_stays_a_budget() {
        let o = go(
            Size::Quote(Notional::new(dec!(1000))),
            OrderKind::Market,
            Some(Price::new(dec!(60000))),
        )
        .unwrap();
        assert_eq!(o.size, Size::Quote(Notional::new(dec!(1000))));
    }

    #[test]
    fn quote_market_sell_converts_at_last_close_and_rounds_down() {
        // 1000 / 60000 = 0.016666.. -> lot 0.00001 -> 0.01666
        let o = to_order(
            OrderId::new("o-1").unwrap(),
            Some(&btcusdt()),
            Side::Sell,
            Size::Quote(Notional::new(dec!(1000))),
            OrderKind::Market,
            Some(Price::new(dec!(60000))),
            Timestamp::from_millis(0),
        )
        .unwrap();
        assert_eq!(o.size, Size::Base(Qty::new(dec!(0.01666))));
    }

    #[test]
    fn limit_sized_at_its_own_price_and_rounded_passive() {
        let kind = OrderKind::Limit {
            price: Price::new(dec!(99.999)),
            tif: TimeInForce::Gtc,
        };
        let o = go(Size::Quote(Notional::new(dec!(100))), kind, None).unwrap();
        // Buy limit 99.999 rounds down to 99.99; 100 / 99.99 = 1.00010001.. -> 1.00010
        assert_eq!(
            o.kind,
            OrderKind::Limit {
                price: Price::new(dec!(99.99)),
                tif: TimeInForce::Gtc
            }
        );
        assert_eq!(o.size, Size::Base(Qty::new(dec!(1.0001))));
    }

    /// A last close off the tick (history under today's filters) is
    /// rounded against the trader, not refused. 0.0001 BTC at 49999.999:
    /// a buy checks at 50000.00 (notional 5, the minimum: accepted); a
    /// sell at 49999.99 (4.999999 < 5: refused as too small).
    #[test]
    fn market_reference_off_tick_is_rounded_against_the_trader() {
        let at = |side| {
            to_order(
                OrderId::new("o-1").unwrap(),
                Some(&btcusdt()),
                side,
                Size::Base(Qty::new(dec!(0.0001))),
                OrderKind::Market,
                Some(Price::new(dec!(49999.999))),
                Timestamp::from_millis(0),
            )
        };
        assert!(at(Side::Buy).is_ok());
        assert_eq!(
            at(Side::Sell).unwrap_err(),
            RiskRejection::Filter(FilterError::BelowMinNotional)
        );
    }

    #[test]
    fn rejections() {
        let q = Size::Base(Qty::new(dec!(1)));
        assert_eq!(
            go(q, OrderKind::Market, None).unwrap_err(),
            RiskRejection::NoReferencePrice
        );
        // 4 USDT < 5 min notional.
        assert_eq!(
            go(
                Size::Quote(Notional::new(dec!(4))),
                OrderKind::Market,
                Some(Price::new(dec!(100)))
            )
            .unwrap_err(),
            RiskRejection::Filter(FilterError::BelowMinNotional)
        );
        let none = to_order(
            OrderId::new("o-1").unwrap(),
            None,
            Side::Buy,
            q,
            OrderKind::Market,
            Some(Price::new(dec!(100))),
            Timestamp::from_millis(0),
        );
        assert_eq!(none.unwrap_err(), RiskRejection::NotInUniverse);
    }
}
