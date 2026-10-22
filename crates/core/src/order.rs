use crate::{Error, InstrumentId, Notional, Price, Qty, Timestamp};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Deref;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

/// Client-assigned order id.
///
/// The engine mints ids from a deterministic counter, so backtest runs are
/// reproducible and live orders map one-to-one to Binance
/// `newClientOrderId`. Accepts Binance's rule, `[A-Za-z0-9.:/_-]{1,36}`,
/// the strictest of the planned venues. Case-sensitive.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OrderId(String);

impl OrderId {
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let v = value.into();
        let ok_char =
            |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '/' | '_' | '-');
        if v.is_empty() || v.len() > 36 || !v.chars().all(ok_char) {
            return Err(Error::InvalidId {
                kind: "order id",
                value: v,
            });
        }
        Ok(Self(v))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for OrderId {
    type Error = Error;
    fn try_from(s: String) -> Result<Self, Error> {
        Self::new(s)
    }
}

impl From<OrderId> for String {
    fn from(id: OrderId) -> String {
        id.0
    }
}

/// How long a resting order stays live. Only resting kinds carry one, so a
/// market order with a meaningless time-in-force can't be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    /// Good till cancelled.
    Gtc,
    /// Immediate or cancel: fill what you can now, cancel the rest.
    Ioc,
    /// Fill or kill: fill entirely now or not at all.
    Fok,
}

/// How an order may execute. The backtest fill model (CLAUDE.md) reads this
/// directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderKind {
    /// Fills at the next event's open in backtest; pays taker fee.
    Market,
    /// Fills at `price` or better.
    Limit { price: Price, tif: TimeInForce },
    /// Becomes a market order once `stop` trades.
    StopMarket { stop: Price },
    /// Becomes a limit order at `limit` once `stop` trades.
    StopLimit {
        stop: Price,
        limit: Price,
        tif: TimeInForce,
    },
}

/// How much to trade.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Size {
    /// Exact base units (sell 0.5 BTC).
    Base(Qty),
    /// Quote amount to spend or receive (buy 100 USDT of BTC). On a market
    /// buy it stays a budget all the way to the broker, which sizes the fill
    /// at the actual fill price so spending never exceeds it (Binance
    /// `quoteOrderQty`). Otherwise risk converts it to base units through
    /// `Instrument::qty_for` at the order's price.
    Quote(Notional),
}

/// A strategy's intent, before risk.
///
/// Not an order yet and not validated: risk resizes it, rejects it
/// (whitelist, freefall guard, venue filters) or turns it into an [`Order`].
/// Strategies emit only signals, never orders, so nothing skips risk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// Place a new order.
    Submit {
        instrument: InstrumentId,
        side: Side,
        size: Size,
        kind: OrderKind,
    },
    /// Cancel a resting order the strategy placed earlier.
    Cancel(OrderId),
}

/// Unchecked order fields. Turn them into an [`Order`] with [`Order::new`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderSpec {
    pub id: OrderId,
    pub instrument: InstrumentId,
    pub side: Side,
    /// `Base`: units, already rounded to the instrument's lot size.
    /// `Quote`: market buys only; a budget the broker turns into units at
    /// the fill price, so spend (fees included) never exceeds it.
    pub size: Size,
    pub kind: OrderKind,
    /// Event time the order was decided on. The engine fills it no earlier
    /// than the next event, so a strategy never trades on the price it
    /// decided from.
    pub created_at: Timestamp,
}

/// Order as risk hands it to a broker.
///
/// Always has a positive size and positive prices, and a quote-sized order
/// is always a market buy (the only quote-sized order Binance accepts, so
/// paper and live agree). Venue-filter
/// compliance (lot, tick, minimum notional) is risk's job, done with
/// `Instrument::check_order` before building the order, since a market
/// order's notional needs a reference price this type doesn't hold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "OrderSpec", into = "OrderSpec")]
pub struct Order(OrderSpec);

impl Order {
    pub fn new(spec: OrderSpec) -> Result<Self, Error> {
        match spec.size {
            Size::Base(q) if !q.is_positive() => {
                return Err(Error::InvalidOrder("qty must be positive"));
            }
            Size::Quote(n) if !n.is_positive() => {
                return Err(Error::InvalidOrder("quote budget must be positive"));
            }
            Size::Quote(_) if spec.kind != OrderKind::Market || spec.side != Side::Buy => {
                return Err(Error::InvalidOrder("quote size only for market buys"));
            }
            _ => {}
        }
        let prices_ok = match spec.kind {
            OrderKind::Market => true,
            OrderKind::Limit { price, .. } => price.is_positive(),
            OrderKind::StopMarket { stop } => stop.is_positive(),
            OrderKind::StopLimit { stop, limit, .. } => stop.is_positive() && limit.is_positive(),
        };
        if !prices_ok {
            return Err(Error::InvalidOrder("prices must be positive"));
        }
        Ok(Self(spec))
    }
}

impl Deref for Order {
    type Target = OrderSpec;
    fn deref(&self) -> &OrderSpec {
        &self.0
    }
}

impl TryFrom<OrderSpec> for Order {
    type Error = Error;
    fn try_from(spec: OrderSpec) -> Result<Self, Error> {
        Self::new(spec)
    }
}

impl From<Order> for OrderSpec {
    fn from(o: Order) -> Self {
        o.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn spec(kind: OrderKind) -> OrderSpec {
        OrderSpec {
            id: OrderId::new("bt-1").unwrap(),
            instrument: "binance:BTCUSDT".parse().unwrap(),
            side: Side::Buy,
            size: Size::Base(Qty::new(dec!(0.001))),
            kind,
            created_at: Timestamp::from_millis(0),
        }
    }

    #[test]
    fn valid_orders() {
        assert!(Order::new(spec(OrderKind::Market)).is_ok());
        let limit = OrderKind::Limit {
            price: Price::new(dec!(60000)),
            tif: TimeInForce::Gtc,
        };
        assert!(Order::new(spec(limit)).is_ok());
    }

    #[test]
    fn rejects_non_positive_qty_or_price() {
        assert!(
            Order::new(OrderSpec {
                size: Size::Base(Qty::ZERO),
                ..spec(OrderKind::Market)
            })
            .is_err()
        );
        let limit = OrderKind::Limit {
            price: Price::ZERO,
            tif: TimeInForce::Gtc,
        };
        assert!(Order::new(spec(limit)).is_err());
        let stop = OrderKind::StopLimit {
            stop: Price::new(dec!(1)),
            limit: Price::new(dec!(-1)),
            tif: TimeInForce::Ioc,
        };
        assert!(Order::new(spec(stop)).is_err());
    }

    #[test]
    fn quote_budget_only_on_market_buys() {
        let budget = Size::Quote(Notional::new(dec!(1000)));
        let buy = OrderSpec {
            size: budget,
            ..spec(OrderKind::Market)
        };
        assert!(Order::new(buy.clone()).is_ok());
        assert!(
            Order::new(OrderSpec {
                side: Side::Sell,
                ..buy.clone()
            })
            .is_err()
        );
        let limit = OrderKind::Limit {
            price: Price::new(dec!(100)),
            tif: TimeInForce::Gtc,
        };
        assert!(
            Order::new(OrderSpec {
                kind: limit,
                ..buy.clone()
            })
            .is_err()
        );
        let zero = Size::Quote(Notional::ZERO);
        assert!(Order::new(OrderSpec { size: zero, ..buy }).is_err());
    }

    #[test]
    fn order_id_follows_binance_rule() {
        assert!(OrderId::new("x/Y:1.2_3-4").is_ok());
        assert!(OrderId::new("").is_err());
        assert!(OrderId::new("a".repeat(37)).is_err());
        assert!(OrderId::new("has space").is_err());
    }
}
