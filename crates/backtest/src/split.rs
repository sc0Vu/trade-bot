//! Spot and perpetuals in one backtest, each in its own paper broker and
//! wallet, as Binance keeps the spot and USDⓈ-M wallets apart (the bot
//! never transfers between them; a hedge's spot gains can't top up its
//! short's margin).

use async_trait::async_trait;
use bot_core::{Fill, InstrumentId, MarketEvent, Order, OrderId, Position};
use exchange::paper::{PaperBroker, PerpPaperBroker};
use exchange::{Broker, ExchangeError};
use std::collections::HashSet;

/// Routes each order by instrument: perpetuals to `perp`, the rest to
/// `spot`. Both settle every event (each ignores instruments it doesn't
/// hold); fills come back spot first, then perp, each in its own order.
pub(crate) struct SplitBroker {
    pub spot: PaperBroker,
    pub perp: PerpPaperBroker,
    /// The instruments `perp` trades.
    pub perps: HashSet<InstrumentId>,
}

impl SplitBroker {
    fn perp_order(&self, id: &OrderId) -> bool {
        self.perp
            .open_orders()
            .iter()
            .chain(&self.perp.cancelling_orders())
            .any(|o| o.id == *id)
    }
}

#[async_trait]
impl Broker for SplitBroker {
    async fn submit(&mut self, order: Order) -> Result<OrderId, ExchangeError> {
        if self.perps.contains(&order.instrument) {
            self.perp.submit(order).await
        } else {
            self.spot.submit(order).await
        }
    }

    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        if self.perp_order(&id) {
            self.perp.cancel(id).await
        } else {
            self.spot.cancel(id).await
        }
    }

    async fn settle(&mut self, event: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        let mut fills = self.spot.settle(event).await?;
        fills.extend(self.perp.settle(event).await?);
        Ok(fills)
    }

    fn position(&self, instrument: &InstrumentId) -> Position {
        if self.perps.contains(instrument) {
            self.perp.position(instrument)
        } else {
            self.spot.position(instrument)
        }
    }

    fn open_orders(&self) -> Vec<Order> {
        let mut all = self.spot.open_orders();
        all.extend(self.perp.open_orders());
        all
    }

    fn cancelling_orders(&self) -> Vec<Order> {
        let mut all = self.spot.cancelling_orders();
        all.extend(self.perp.cancelling_orders());
        all
    }
}
