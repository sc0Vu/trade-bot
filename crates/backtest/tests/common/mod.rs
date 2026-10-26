//! Test harness shared by the restart tests: a paper broker both halves of
//! a restarted run trade against (the books survive a restart), a provider
//! replaying a fixed event list, and an observer keeping the last saved
//! strategy state.

#![allow(dead_code)]

use async_trait::async_trait;
use bot_core::{
    AssetClass, Fill, Instrument, InstrumentId, MarketEvent, Order, OrderId, OrderSpec, Position,
};
use engine::{Observer, Snapshot};
use exchange::paper::{BpsSlippage, FeeAsset, NextOpenFillModel, PaperBroker, RateFee};
use exchange::{Broker, ExchangeError, InstrumentQuery, MarketData, Provider};
use rust_decimal_macros::dec;
use std::sync::Arc;
use tokio::sync::Mutex;

/// The paper broker shared by both halves of a restarted run. The second
/// half's order ids get a prefix: engine ids restart at o-1, as live, where
/// the run tag keeps them apart.
#[derive(Clone)]
pub struct Shared(pub Arc<Mutex<PaperBroker>>, pub &'static str);

#[async_trait]
impl Broker for Shared {
    async fn submit(&mut self, o: Order) -> Result<OrderId, ExchangeError> {
        let spec: OrderSpec = o.into();
        let id = OrderId::new(format!("{}{}", self.1, spec.id)).unwrap();
        let o = Order::new(OrderSpec { id, ..spec }).unwrap();
        self.0.lock().await.submit(o).await
    }
    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        self.0.lock().await.cancel(id).await
    }
    async fn settle(&mut self, e: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        self.0.lock().await.settle(e).await
    }
    fn position(&self, i: &InstrumentId) -> Position {
        self.0.try_lock().expect("uncontended").position(i)
    }
    fn open_orders(&self) -> Vec<Order> {
        self.0.try_lock().expect("uncontended").open_orders()
    }
}

struct Script(std::vec::IntoIter<MarketEvent>);

#[async_trait]
impl MarketData for Script {
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
        Ok(self.0.next())
    }
}

pub struct Replay {
    pub instruments: Vec<Instrument>,
    pub events: Vec<MarketEvent>,
    pub broker: Shared,
}

#[async_trait]
impl Provider for Replay {
    fn id(&self) -> &str {
        "replay"
    }
    fn asset_classes(&self) -> &[AssetClass] {
        &[AssetClass::Spot]
    }
    async fn market_data(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        Ok(Box::new(Script(self.events.clone().into_iter())))
    }
    async fn broker(&self) -> Result<Box<dyn Broker>, ExchangeError> {
        Ok(Box::new(self.broker.clone()))
    }
    async fn resolve(&self, q: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError> {
        let InstrumentQuery::Ids(ids) = q;
        Ok(ids
            .iter()
            .filter_map(|id| self.instruments.iter().find(|i| i.id == *id).cloned())
            .collect())
    }
}

pub struct LastState(pub Option<serde_json::Value>);

impl Observer for LastState {
    fn on_snapshot(&mut self, s: &Snapshot) {
        self.0 = s.strategy_state.clone();
    }
}

pub fn paper(instruments: &[Instrument]) -> Arc<Mutex<PaperBroker>> {
    Arc::new(Mutex::new(PaperBroker::new(
        instruments.to_vec(),
        Box::new(NextOpenFillModel),
        Box::new(BpsSlippage::new(dec!(10)).unwrap()),
        Box::new(RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap()),
    )))
}
