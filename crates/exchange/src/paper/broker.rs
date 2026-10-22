use super::{Decision, FeeModel, FillModel, OrderState, SlippageModel};
use crate::{Broker, ExchangeError};
use async_trait::async_trait;
use bot_core::{
    Candle, Fee, Fill, FillSpec, Instrument, InstrumentId, Liquidity, MarketEvent, Notional, Order,
    OrderId, OrderKind, Position, Price, Qty, Side, Size,
};
use std::collections::HashMap;

struct Pending {
    order: Order,
    state: OrderState,
}

/// Simulated broker for backtest and paper trading.
///
/// Orders wait until a candle that opened after they were created, then the
/// [`FillModel`] decides; the [`SlippageModel`] and [`FeeModel`] set the
/// final price and fee, and the fill is applied to the position with
/// `Position::apply`. Pending orders are settled in submission order.
///
/// A quote-budget market buy (`Size::Quote`) is sized at the fill: the
/// largest lot multiple whose notional plus any quote-denominated fee fits
/// the budget, at the slipped price. Spend never exceeds the budget. If
/// that size breaks a venue filter the order is dropped unfilled.
///
/// Rejects at submit what a spot venue would: unknown instruments, reused
/// ids, and sells larger than the holding minus sells already pending.
/// Does not model a cash balance yet, so buys are never refused for lack of
/// quote.
pub struct PaperBroker {
    instruments: HashMap<InstrumentId, Instrument>,
    fill_model: Box<dyn FillModel>,
    slippage: Box<dyn SlippageModel>,
    fees: Box<dyn FeeModel>,
    pending: Vec<Pending>,
    positions: HashMap<InstrumentId, Position>,
}

impl PaperBroker {
    /// A broker for `instruments` (the resolved universe) with the given
    /// models.
    pub fn new(
        instruments: Vec<Instrument>,
        fill_model: Box<dyn FillModel>,
        slippage: Box<dyn SlippageModel>,
        fees: Box<dyn FeeModel>,
    ) -> Self {
        Self {
            instruments: instruments.into_iter().map(|i| (i.id.clone(), i)).collect(),
            fill_model,
            slippage,
            fees,
            pending: Vec::new(),
            positions: HashMap::new(),
        }
    }

    fn pending_sell_qty(&self, id: &InstrumentId) -> Qty {
        self.pending
            .iter()
            .filter(|p| p.order.instrument == *id && p.order.side == Side::Sell)
            .map(|p| match p.order.size {
                Size::Base(q) => q,
                // Quote size is buy-only (Order::new), so never reached.
                Size::Quote(_) => Qty::ZERO,
            })
            .sum()
    }

    /// Largest lot multiple buyable with `budget` at `price`, counting a
    /// quote-denominated fee as spend. `None` if it breaks a venue filter.
    fn budget_qty(
        &self,
        instrument: &Instrument,
        budget: Notional,
        price: Price,
        liquidity: Liquidity,
    ) -> Option<Qty> {
        let cost = |q: Qty| {
            let notional = instrument.notional(q, price);
            match self.fees.fee(instrument, Side::Buy, q, price, liquidity) {
                Fee::Quote(f) => notional + f,
                Fee::Base(_) | Fee::Other { .. } => notional,
            }
        };
        let mut q = instrument.round_qty(instrument.qty_for(budget, price).ok()?);
        let first = cost(q);
        if first > budget {
            // Shrink by the effective fee rate, then step down lot by lot.
            q = instrument.round_qty(q * (budget / first));
        }
        while q.is_positive() && cost(q) > budget {
            q -= instrument.lot_size;
        }
        instrument.check_order(q, price).ok()?;
        Some(q)
    }

    fn execute(&mut self, order: &Order, candle: &Candle, exec: super::Execution) -> Option<Fill> {
        let instrument = self.instruments.get(&order.instrument)?;
        // A limit never fills worse than its limit: slippage on a taker
        // limit fill (marketable at the open) stops at the limit price.
        let price = cap_at_limit(order, self.slippage.apply(instrument, order.side, exec));
        let qty = match order.size {
            Size::Base(q) => q,
            Size::Quote(budget) => match self.budget_qty(instrument, budget, price, exec.liquidity)
            {
                Some(q) => q,
                None => {
                    tracing::warn!(order = %order.id, "quote budget too small at fill price; order dropped");
                    return None;
                }
            },
        };
        let fee = self
            .fees
            .fee(instrument, order.side, qty, price, exec.liquidity);
        let ts = if exec.at_open {
            candle.open_time
        } else {
            candle.close_time
        };
        let fill = match Fill::new(FillSpec {
            order_id: order.id.clone(),
            instrument: order.instrument.clone(),
            side: order.side,
            qty,
            price,
            fee,
            liquidity: exec.liquidity,
            ts,
        }) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(order = %order.id, error = %e, "paper fill invalid; order dropped");
                return None;
            }
        };
        let position = self
            .positions
            .entry(order.instrument.clone())
            .or_insert_with(|| Position::flat(order.instrument.clone()));
        if let Err(e) = position.apply(&fill, instrument) {
            tracing::warn!(order = %order.id, error = %e, "paper fill rejected by position; order dropped");
            return None;
        }
        Some(fill)
    }
}

fn cap_at_limit(order: &Order, price: Price) -> Price {
    let limit = match order.kind {
        OrderKind::Limit { price, .. } => price,
        OrderKind::StopLimit { limit, .. } => limit,
        OrderKind::Market | OrderKind::StopMarket { .. } => return price,
    };
    match order.side {
        Side::Buy => price.min(limit),
        Side::Sell => price.max(limit),
    }
}

#[async_trait]
impl Broker for PaperBroker {
    async fn submit(&mut self, order: Order) -> Result<OrderId, ExchangeError> {
        if !self.instruments.contains_key(&order.instrument) {
            return Err(ExchangeError::UnknownInstrument(order.instrument.clone()));
        }
        if self.pending.iter().any(|p| p.order.id == order.id) {
            return Err(ExchangeError::DuplicateOrder(order.id.clone()));
        }
        if let (Side::Sell, Size::Base(qty)) = (order.side, order.size) {
            let held = self.position(&order.instrument).qty();
            if self.pending_sell_qty(&order.instrument) + qty > held {
                return Err(ExchangeError::Rejected("sell exceeds holding"));
            }
        }
        let id = order.id.clone();
        self.pending.push(Pending {
            order,
            state: OrderState {
                triggered: false,
                first_look: true,
            },
        });
        Ok(id)
    }

    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        let before = self.pending.len();
        self.pending.retain(|p| p.order.id != id);
        if self.pending.len() == before {
            return Err(ExchangeError::UnknownOrder(id));
        }
        Ok(())
    }

    async fn settle(&mut self, event: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        let MarketEvent::Candle(candle) = event else {
            return Ok(Vec::new());
        };
        let mut fills = Vec::new();
        let mut still_pending = Vec::with_capacity(self.pending.len());
        for mut p in std::mem::take(&mut self.pending) {
            // Only candles that opened after the order was decided: the bar
            // it was decided on (or any earlier one) can't fill it.
            if p.order.instrument != candle.instrument || candle.open_time <= p.order.created_at {
                still_pending.push(p);
                continue;
            }
            match self.fill_model.evaluate(&p.order, p.state, candle) {
                Decision::Fill(exec) => fills.extend(self.execute(&p.order, candle, exec)),
                Decision::Trigger => {
                    p.state = OrderState {
                        triggered: true,
                        first_look: true,
                    };
                    still_pending.push(p);
                }
                Decision::Rest => {
                    p.state.first_look = false;
                    still_pending.push(p);
                }
                Decision::Expire => {}
            }
        }
        self.pending = still_pending;
        Ok(fills)
    }

    fn position(&self, instrument: &InstrumentId) -> Position {
        self.positions
            .get(instrument)
            .cloned()
            .unwrap_or_else(|| Position::flat(instrument.clone()))
    }

    fn open_orders(&self) -> Vec<Order> {
        self.pending.iter().map(|p| p.order.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paper::test_support::{HOUR, btcusdt, candle};
    use crate::paper::{BpsSlippage, FeeAsset, NextOpenFillModel, RateFee};
    use bot_core::{Fee, Liquidity, Notional, OrderKind, OrderSpec, Price, Timestamp};
    use rust_decimal_macros::dec;

    fn broker(slippage_bps: rust_decimal::Decimal) -> PaperBroker {
        PaperBroker::new(
            vec![btcusdt()],
            Box::new(NextOpenFillModel),
            Box::new(BpsSlippage::new(slippage_bps).unwrap()),
            Box::new(RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap()),
        )
    }

    /// Market order decided at the close of candle `i`.
    fn market(id: &str, side: Side, qty: rust_decimal::Decimal, decided_on: i64) -> Order {
        Order::new(OrderSpec {
            id: OrderId::new(id).unwrap(),
            instrument: btcusdt().id.clone(),
            side,
            size: Size::Base(Qty::new(qty)),
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(decided_on * HOUR + HOUR - 1),
        })
        .unwrap()
    }

    fn ev(i: i64, ohlc: [&str; 4]) -> MarketEvent {
        MarketEvent::Candle(candle(i, ohlc))
    }

    #[tokio::test]
    async fn never_fills_on_the_bar_it_was_decided_on() {
        let mut b = broker(dec!(0));
        b.submit(market("o-1", Side::Buy, dec!(1), 0))
            .await
            .unwrap();
        let fills = b.settle(&ev(0, ["100", "101", "99", "100"])).await.unwrap();
        assert!(fills.is_empty());
        assert_eq!(b.open_orders().len(), 1);
    }

    /// Buy 10 @ next open 101, no slippage, fee 0.1% in base:
    /// fee 0.01 BTC, position 9.99 BTC at cost 1010 USDT.
    #[tokio::test]
    async fn market_buy_fills_at_next_open() {
        let mut b = broker(dec!(0));
        b.submit(market("o-1", Side::Buy, dec!(10), 0))
            .await
            .unwrap();
        let fills = b
            .settle(&ev(1, ["101", "103", "100", "102"]))
            .await
            .unwrap();

        assert_eq!(fills.len(), 1);
        let f = &fills[0];
        assert_eq!(f.price, Price::new(dec!(101)));
        assert_eq!(f.fee, Fee::Base(Qty::new(dec!(0.01))));
        assert_eq!(f.liquidity, Liquidity::Taker);
        assert_eq!(f.ts, Timestamp::from_millis(HOUR));

        let pos = b.position(&btcusdt().id);
        assert_eq!(pos.qty(), Qty::new(dec!(9.99)));
        assert_eq!(pos.cost_basis(), Notional::new(dec!(1010)));
        assert_eq!(b.open_orders().len(), 0);
    }

    /// Same with 10 bps: 101 * 1.001 = 101.101, up to tick 101.11;
    /// cost 10 * 101.11 = 1011.1.
    #[tokio::test]
    async fn slippage_applies_to_market_fill() {
        let mut b = broker(dec!(10));
        b.submit(market("o-1", Side::Buy, dec!(10), 0))
            .await
            .unwrap();
        let fills = b
            .settle(&ev(1, ["101", "103", "100", "102"]))
            .await
            .unwrap();
        assert_eq!(fills[0].price, Price::new(dec!(101.11)));
        assert_eq!(
            b.position(&btcusdt().id).cost_basis(),
            Notional::new(dec!(1011.1))
        );
    }

    fn budget_buy(budget: rust_decimal::Decimal) -> Order {
        Order::new(OrderSpec {
            size: Size::Quote(Notional::new(budget)),
            ..market("o-1", Side::Buy, dec!(1), 0).into()
        })
        .unwrap()
    }

    /// Budget 1000 at open 101, fee 0.1% in base:
    /// 1000 / 101 = 9.90099009.. -> lot 9.90099; spend 9.90099 * 101 =
    /// 999.99999 <= 1000; fee 0.0099009900 BTC.
    #[tokio::test]
    async fn quote_budget_sized_at_fill_price() {
        let mut b = broker(dec!(0));
        b.submit(budget_buy(dec!(1000))).await.unwrap();
        let f = &b
            .settle(&ev(1, ["101", "103", "100", "102"]))
            .await
            .unwrap()[0];
        assert_eq!(f.qty, Qty::new(dec!(9.90099)));
        assert_eq!(
            btcusdt().notional(f.qty, f.price),
            Notional::new(dec!(999.99999))
        );
        assert_eq!(f.fee, Fee::Base(Qty::new(dec!(0.0099009900))));
    }

    /// Budget 1000 at open 100, fee 0.1% in quote. 10 BTC would cost
    /// 1000 + 1 fee = 1001. Largest fitting lot multiple: 9.99 BTC ->
    /// 999 + 0.999 = 999.999; one more lot (9.99001) costs 1000.000001.
    #[tokio::test]
    async fn quote_budget_counts_quote_fee_as_spend() {
        let mut b = PaperBroker::new(
            vec![btcusdt()],
            Box::new(NextOpenFillModel),
            Box::new(BpsSlippage::zero()),
            Box::new(RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Quote).unwrap()),
        );
        b.submit(budget_buy(dec!(1000))).await.unwrap();
        let f = &b
            .settle(&ev(1, ["100", "100", "100", "100"]))
            .await
            .unwrap()[0];
        assert_eq!(f.qty, Qty::new(dec!(9.99)));
        assert_eq!(f.fee, Fee::Quote(Notional::new(dec!(0.999))));
    }

    #[tokio::test]
    async fn budget_below_min_notional_is_dropped() {
        let mut b = broker(dec!(0));
        b.submit(budget_buy(dec!(4))).await.unwrap();
        assert!(
            b.settle(&ev(1, ["100", "100", "100", "100"]))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(b.position(&btcusdt().id).is_flat());
        assert_eq!(b.open_orders().len(), 0);
    }

    #[tokio::test]
    async fn rejects_what_a_spot_venue_would() {
        let mut b = broker(dec!(0));
        // Nothing held: can't sell.
        assert!(matches!(
            b.submit(market("o-1", Side::Sell, dec!(1), 0)).await,
            Err(ExchangeError::Rejected(_))
        ));
        let other = Order::new(OrderSpec {
            instrument: "binance:ETHUSDT".parse().unwrap(),
            ..market("o-2", Side::Buy, dec!(1), 0).into()
        })
        .unwrap();
        assert!(matches!(
            b.submit(other).await,
            Err(ExchangeError::UnknownInstrument(_))
        ));

        b.submit(market("o-3", Side::Buy, dec!(1), 0))
            .await
            .unwrap();
        assert!(matches!(
            b.submit(market("o-3", Side::Buy, dec!(1), 0)).await,
            Err(ExchangeError::DuplicateOrder(_))
        ));
    }

    #[tokio::test]
    async fn pending_sells_reserve_the_holding() {
        let mut b = broker(dec!(0));
        b.submit(market("o-1", Side::Buy, dec!(1), 0))
            .await
            .unwrap();
        b.settle(&ev(1, ["100", "100", "100", "100"]))
            .await
            .unwrap();
        // Holding 0.999 BTC after the base fee.
        b.submit(market("o-2", Side::Sell, dec!(0.5), 1))
            .await
            .unwrap();
        assert!(
            b.submit(market("o-3", Side::Sell, dec!(0.5), 1))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancel_removes_open_order() {
        let mut b = broker(dec!(0));
        b.submit(market("o-1", Side::Buy, dec!(1), 0))
            .await
            .unwrap();
        b.cancel(OrderId::new("o-1").unwrap()).await.unwrap();
        assert!(
            b.settle(&ev(1, ["100", "100", "100", "100"]))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            b.cancel(OrderId::new("o-1").unwrap()).await,
            Err(ExchangeError::UnknownOrder(_))
        ));
    }

    #[tokio::test]
    async fn resting_limit_waits_then_fills() {
        let mut b = broker(dec!(10));
        let limit = Order::new(OrderSpec {
            kind: OrderKind::Limit {
                price: Price::new(dec!(95)),
                tif: bot_core::TimeInForce::Gtc,
            },
            ..market("o-1", Side::Buy, dec!(1), 0).into()
        })
        .unwrap();
        b.submit(limit).await.unwrap();
        assert!(
            b.settle(&ev(1, ["100", "101", "96", "97"]))
                .await
                .unwrap()
                .is_empty()
        );
        let fills = b.settle(&ev(2, ["97", "98", "94", "96"])).await.unwrap();
        // Maker at the limit; slippage not applied to maker fills.
        assert_eq!(fills[0].price, Price::new(dec!(95)));
        assert_eq!(fills[0].liquidity, Liquidity::Maker);
        assert_eq!(fills[0].ts, Timestamp::from_millis(3 * HOUR - 1));
    }

    /// Buy limit 101, marketable at open 100.95 (taker). 10 bps slippage
    /// would make it 100.95 * 1.001 = 101.05095 -> 101.06, above the
    /// limit; the fill is capped at 101.
    #[tokio::test]
    async fn slippage_never_pushes_a_limit_past_its_price() {
        let mut b = broker(dec!(10));
        let limit = Order::new(OrderSpec {
            kind: OrderKind::Limit {
                price: Price::new(dec!(101)),
                tif: bot_core::TimeInForce::Gtc,
            },
            ..market("o-1", Side::Buy, dec!(1), 0).into()
        })
        .unwrap();
        b.submit(limit).await.unwrap();
        let fills = b
            .settle(&ev(1, ["100.95", "101", "100.90", "101"]))
            .await
            .unwrap();
        assert_eq!(fills[0].liquidity, Liquidity::Taker);
        assert_eq!(fills[0].price, Price::new(dec!(101)));
    }
}
