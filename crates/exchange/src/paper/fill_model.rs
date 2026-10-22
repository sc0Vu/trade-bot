use bot_core::{Candle, Liquidity, Order, OrderKind, Price, Side, TimeInForce};

/// Where a simulated order executes, before slippage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Execution {
    pub price: Price,
    pub liquidity: Liquidity,
    /// Executed at the candle's open (stamped `open_time`) rather than
    /// somewhere inside it (stamped `close_time`, the first moment the
    /// fill is known).
    pub at_open: bool,
}

/// Broker-tracked state of a pending order that the model may need.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OrderState {
    /// A stop-limit whose stop has traded; it now behaves as a limit.
    pub triggered: bool,
    /// This is the first candle the order is evaluated on in its current
    /// form (since submission, or since its stop triggered). Immediate-only
    /// orders (IOC/FOK) can only fill now.
    pub first_look: bool,
}

/// What happens to a pending order on one candle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Fill(Execution),
    /// Stop-limit triggered; evaluate as a limit from the next candle.
    Trigger,
    /// Keep waiting.
    Rest,
    /// Remove unfilled (IOC/FOK that couldn't fill immediately).
    Expire,
}

/// Decides whether and where a pending order fills against a candle.
///
/// Called only with candles that opened after the order was created, so the
/// model never sees the bar the order was decided on. A candle is four
/// prices with no path between them, so every model is an assumption; the
/// broker makes it swappable and the engine and strategies never see it.
pub trait FillModel: Send + Sync {
    fn evaluate(&self, order: &Order, state: OrderState, candle: &Candle) -> Decision;
}

/// Default fill model: the rules in CLAUDE.md "Backtest fill model".
///
/// - Market: fills at the candle's open, taker.
/// - Limit buy: on its first candle, if the open is strictly below the
///   limit the order was marketable and fills at the open, taker. Otherwise
///   it rests and fills at the limit, maker, only when price trades
///   through it (`low < limit`). Price merely touching the limit
///   (`low == limit`) is no fill: queue position is unknown, and an order
///   at the last trade price usually just joins the book. Sell mirrors with
///   `open > limit` and `high > limit`. IOC/FOK fill only at the open of
///   their first candle, else expire; GTC rests.
/// - Stop-market buy: fills at the open if the open is at or above the stop
///   (gapped through), else at the stop if `high >= stop`, taker. Sell
///   mirrors with `low`.
/// - Stop-limit: triggers like a stop, then becomes a limit evaluated from
///   the next candle. Never fills on the trigger candle: whether price came
///   back to the limit after touching the stop is unknowable from OHLC, so
///   the model assumes the worse case (no fill yet).
///
/// No partial fills: an order fills entirely or not at all.
#[derive(Clone, Copy, Debug, Default)]
pub struct NextOpenFillModel;

impl NextOpenFillModel {
    fn limit(side: Side, limit: Price, tif: TimeInForce, first_look: bool, c: &Candle) -> Decision {
        // Strict: an order at exactly the open price would rest, not cross.
        let marketable_at_open = match side {
            Side::Buy => c.open < limit,
            Side::Sell => c.open > limit,
        };
        if first_look && marketable_at_open {
            return Decision::Fill(Execution {
                price: c.open,
                liquidity: Liquidity::Taker,
                at_open: true,
            });
        }
        if tif != TimeInForce::Gtc {
            return Decision::Expire;
        }
        // A resting order fills at its own price only if the market traded
        // through it, crossing at the open (later candles) or inside the bar.
        let traded_through = match side {
            Side::Buy => c.low < limit,
            Side::Sell => c.high > limit,
        };
        if traded_through {
            Decision::Fill(Execution {
                price: limit,
                liquidity: Liquidity::Maker,
                at_open: false,
            })
        } else {
            Decision::Rest
        }
    }

    /// Where a stop triggers on `c`, if it does: the open when gapped
    /// through, else the stop itself.
    fn stop_hit(side: Side, stop: Price, c: &Candle) -> Option<(Price, bool)> {
        match side {
            Side::Buy if c.open >= stop => Some((c.open, true)),
            Side::Buy if c.high >= stop => Some((stop, false)),
            Side::Sell if c.open <= stop => Some((c.open, true)),
            Side::Sell if c.low <= stop => Some((stop, false)),
            _ => None,
        }
    }
}

impl FillModel for NextOpenFillModel {
    fn evaluate(&self, order: &Order, state: OrderState, c: &Candle) -> Decision {
        match order.kind {
            OrderKind::Market => Decision::Fill(Execution {
                price: c.open,
                liquidity: Liquidity::Taker,
                at_open: true,
            }),
            OrderKind::Limit { price, tif } => {
                Self::limit(order.side, price, tif, state.first_look, c)
            }
            OrderKind::StopMarket { stop } => match Self::stop_hit(order.side, stop, c) {
                Some((price, at_open)) => Decision::Fill(Execution {
                    price,
                    liquidity: Liquidity::Taker,
                    at_open,
                }),
                None => Decision::Rest,
            },
            OrderKind::StopLimit { stop, limit, tif } => {
                if state.triggered {
                    Self::limit(order.side, limit, tif, state.first_look, c)
                } else if Self::stop_hit(order.side, stop, c).is_some() {
                    Decision::Trigger
                } else {
                    Decision::Rest
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paper::test_support::{btcusdt, candle};
    use bot_core::{OrderId, OrderSpec, Qty, Size, Timestamp};
    use rust_decimal_macros::dec;

    fn order(side: Side, kind: OrderKind) -> Order {
        Order::new(OrderSpec {
            id: OrderId::new("o-1").unwrap(),
            instrument: btcusdt().id.clone(),
            side,
            size: Size::Base(Qty::new(dec!(1))),
            kind,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    fn p(s: &str) -> Price {
        s.parse().unwrap()
    }

    fn eval(side: Side, kind: OrderKind, state: OrderState, ohlc: [&str; 4]) -> Decision {
        NextOpenFillModel.evaluate(&order(side, kind), state, &candle(1, ohlc))
    }

    const FIRST: OrderState = OrderState {
        triggered: false,
        first_look: true,
    };
    const LATER: OrderState = OrderState {
        triggered: false,
        first_look: false,
    };

    fn fill(price: &str, liquidity: Liquidity, at_open: bool) -> Decision {
        Decision::Fill(Execution {
            price: p(price),
            liquidity,
            at_open,
        })
    }

    const BAR: [&str; 4] = ["100", "105", "95", "102"];

    #[test]
    fn market_fills_at_open_as_taker() {
        assert_eq!(
            eval(Side::Buy, OrderKind::Market, FIRST, BAR),
            fill("100", Liquidity::Taker, true)
        );
        assert_eq!(
            eval(Side::Sell, OrderKind::Market, FIRST, BAR),
            fill("100", Liquidity::Taker, true)
        );
    }

    #[test]
    fn limit_buy() {
        let lim = |price: &str, tif| OrderKind::Limit {
            price: p(price),
            tif,
        };
        // Marketable at the open: fills at the (better) open, taker.
        assert_eq!(
            eval(Side::Buy, lim("101", TimeInForce::Gtc), FIRST, BAR),
            fill("100", Liquidity::Taker, true)
        );
        // Touched inside the bar: fills at the limit, maker.
        assert_eq!(
            eval(Side::Buy, lim("97", TimeInForce::Gtc), FIRST, BAR),
            fill("97", Liquidity::Maker, false)
        );
        // Never reached.
        assert_eq!(
            eval(Side::Buy, lim("90", TimeInForce::Gtc), FIRST, BAR),
            Decision::Rest
        );
        // Resting from an earlier bar: below-limit open still fills at limit.
        assert_eq!(
            eval(Side::Buy, lim("101", TimeInForce::Gtc), LATER, BAR),
            fill("101", Liquidity::Maker, false)
        );
        // Limit equal to the open isn't marketable; it rests and fills at
        // the limit, maker, because the low (95) trades through it.
        assert_eq!(
            eval(Side::Buy, lim("100", TimeInForce::Gtc), FIRST, BAR),
            fill("100", Liquidity::Maker, false)
        );
        // Low exactly at the limit: touched, not traded through. No fill.
        assert_eq!(
            eval(Side::Buy, lim("95", TimeInForce::Gtc), FIRST, BAR),
            Decision::Rest
        );
    }

    #[test]
    fn limit_sell() {
        let lim = |price: &str| OrderKind::Limit {
            price: p(price),
            tif: TimeInForce::Gtc,
        };
        assert_eq!(
            eval(Side::Sell, lim("99"), FIRST, BAR),
            fill("100", Liquidity::Taker, true)
        );
        assert_eq!(
            eval(Side::Sell, lim("104"), FIRST, BAR),
            fill("104", Liquidity::Maker, false)
        );
        assert_eq!(eval(Side::Sell, lim("106"), FIRST, BAR), Decision::Rest);
        // Equal to the open: rests, then the high (105) trades through it.
        assert_eq!(
            eval(Side::Sell, lim("100"), FIRST, BAR),
            fill("100", Liquidity::Maker, false)
        );
        // High exactly at the limit: no fill.
        assert_eq!(eval(Side::Sell, lim("105"), FIRST, BAR), Decision::Rest);
    }

    #[test]
    fn immediate_limits_only_fill_at_first_open() {
        for tif in [TimeInForce::Ioc, TimeInForce::Fok] {
            let at_open = OrderKind::Limit {
                price: p("101"),
                tif,
            };
            assert_eq!(
                eval(Side::Buy, at_open, FIRST, BAR),
                fill("100", Liquidity::Taker, true)
            );
            // Would touch inside the bar, but that isn't immediate.
            let inside = OrderKind::Limit {
                price: p("97"),
                tif,
            };
            assert_eq!(eval(Side::Buy, inside, FIRST, BAR), Decision::Expire);
            // At exactly the open: not marketable, so an immediate order expires.
            let at_par = OrderKind::Limit {
                price: p("100"),
                tif,
            };
            assert_eq!(eval(Side::Buy, at_par, FIRST, BAR), Decision::Expire);
        }
    }

    #[test]
    fn stop_market() {
        let stop = |s: &str| OrderKind::StopMarket { stop: p(s) };
        // Buy stop inside the bar: fills at the stop.
        assert_eq!(
            eval(Side::Buy, stop("103"), FIRST, BAR),
            fill("103", Liquidity::Taker, false)
        );
        // Gapped through: fills at the worse open.
        assert_eq!(
            eval(Side::Buy, stop("99"), FIRST, BAR),
            fill("100", Liquidity::Taker, true)
        );
        assert_eq!(eval(Side::Buy, stop("106"), FIRST, BAR), Decision::Rest);
        // Sell stop mirrors on the low.
        assert_eq!(
            eval(Side::Sell, stop("96"), FIRST, BAR),
            fill("96", Liquidity::Taker, false)
        );
        assert_eq!(
            eval(Side::Sell, stop("101"), FIRST, BAR),
            fill("100", Liquidity::Taker, true)
        );
        assert_eq!(eval(Side::Sell, stop("94"), FIRST, BAR), Decision::Rest);
    }

    #[test]
    fn stop_limit_triggers_then_fills_as_limit_next_bar() {
        let kind = OrderKind::StopLimit {
            stop: p("103"),
            limit: p("104"),
            tif: TimeInForce::Gtc,
        };
        assert_eq!(eval(Side::Buy, kind, FIRST, BAR), Decision::Trigger);
        let triggered = OrderState {
            triggered: true,
            first_look: true,
        };
        assert_eq!(
            eval(
                Side::Buy,
                kind,
                triggered,
                ["103.50", "104", "103", "103.80"]
            ),
            fill("103.50", Liquidity::Taker, true)
        );
        assert_eq!(
            eval(Side::Buy, kind, FIRST, ["100", "102", "99", "101"]),
            Decision::Rest
        );
    }
}
