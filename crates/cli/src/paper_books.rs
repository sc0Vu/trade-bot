//! Books for the read-only API on a local paper run (Binance futures
//! paper, MAX paper). Those runs keep no database: the paper broker in
//! memory is the books, and it starts flat every run. So the API reads an
//! in-memory store ([`storage::SqlStore::in_memory`]) that this recorder
//! fills from the engine's snapshots: every fill once, and each changed
//! position. A background task writes, so the engine never waits; a write
//! failure is a warning (the API is display only). Nothing survives exit,
//! like the run itself. No order journal: `/orders` stays empty, working
//! orders show in `/open-orders` from the snapshot.

use bot_core::{Fill, Position};
use engine::Snapshot;
use std::sync::Arc;
use storage::{HoldingRepository, SqlStore, TradeRepository};
use tokio::sync::mpsc;

/// Feeds the in-memory API books from snapshots.
pub struct Recorder {
    tx: mpsc::UnboundedSender<(Vec<Fill>, Vec<Position>)>,
    /// Positions last sent, so an unchanged one isn't rewritten each event.
    sent: Vec<Position>,
}

impl Recorder {
    /// Starts the writer task over `books`.
    pub fn start(books: Arc<SqlStore>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<(Vec<Fill>, Vec<Position>)>();
        tokio::spawn(async move {
            while let Some((fills, positions)) = rx.recv().await {
                for f in &fills {
                    if let Err(e) = books.insert_trade(f).await {
                        tracing::warn!(error = %e, "API books: fill not recorded");
                    }
                }
                for p in &positions {
                    if let Err(e) = books.upsert_holding(p).await {
                        tracing::warn!(error = %e, "API books: position not recorded");
                    }
                }
            }
        });
        Self {
            tx,
            sent: Vec::new(),
        }
    }

    /// Sends `s`'s new fills and whichever positions changed since the
    /// last call (funding moves realized P&L without a fill).
    pub fn publish(&mut self, s: &Snapshot) {
        let changed: Vec<Position> = s
            .positions
            .iter()
            .filter(|p| !self.sent.contains(p))
            .cloned()
            .collect();
        if s.new_fills.is_empty() && changed.is_empty() {
            return;
        }
        self.sent = s.positions.clone();
        // The writer only stops with the runtime: a send can't fail first.
        let _ = self.tx.send((s.new_fills.clone(), changed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::{FillSpec, Liquidity, Notional, OrderId, Price, Qty, Side, Timestamp};
    use rust_decimal_macros::dec;

    fn btc() -> bot_core::InstrumentId {
        "binance-usdm:BTCUSDT".parse().unwrap()
    }

    /// A fill and its position reach the store once; a snapshot with
    /// nothing new writes nothing; a position changed without a fill
    /// (funding) is rewritten.
    #[tokio::test]
    async fn fills_once_and_changed_positions_reach_the_books() {
        let books = Arc::new(SqlStore::in_memory().await.unwrap());
        let mut r = Recorder::start(books.clone());
        let fill = Fill::new(FillSpec {
            order_id: OrderId::new("o-1").unwrap(),
            instrument: btc(),
            side: Side::Sell,
            qty: Qty::new(dec!(0.01)),
            price: Price::new(dec!(60000)),
            fee: bot_core::Fee::Quote(Notional::new(dec!(0.3))),
            liquidity: Liquidity::Taker,
            ts: Timestamp::from_millis(1_000),
        })
        .unwrap();
        let short = |realized| {
            Position::from_parts(
                btc(),
                Qty::new(dec!(-0.01)),
                Notional::new(dec!(-600)),
                Notional::new(realized),
            )
            .unwrap()
        };
        let snap = |fills: Vec<Fill>, p: Position| Snapshot {
            positions: vec![p],
            new_fills: fills,
            ..Snapshot::default()
        };
        r.publish(&snap(vec![fill.clone()], short(dec!(-0.3))));
        r.publish(&snap(vec![], short(dec!(-0.3))));
        r.publish(&snap(vec![], short(dec!(-0.25))));
        // Let the writer drain.
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            if books
                .get_holding(&btc())
                .await
                .unwrap()
                .map(|p| p.realized_pnl().get())
                == Some(dec!(-0.25))
            {
                break;
            }
        }
        let trades = books.recent_trades(10).await.unwrap();
        assert_eq!(trades.len(), 1, "one fill, recorded once");
        assert_eq!(trades[0].fill, fill);
        let held = books.get_holding(&btc()).await.unwrap().unwrap();
        assert_eq!(held, short(dec!(-0.25)), "the funding change was written");
    }
}
