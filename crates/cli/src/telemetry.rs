//! Metrics export for `run`: a Prometheus endpoint, process metrics, and
//! the live-state gauges fed from the engine's [`Observer`].
//!
//! Off unless `metrics.enabled`. The endpoint has no authentication, so it
//! binds to loopback unless `metrics.allow_public` says otherwise. It can
//! never stop trading: if it can't start, the run logs a warning and goes
//! on without it. Money values (positions, cost basis, realized P&L) are
//! exported only with `metrics.include_money`.

use crate::config::MetricsSection;
use bot_core::Decimal;
use engine::{Observer, Snapshot};
use rust_decimal::prelude::ToPrimitive;

/// Gauge: orders working on the venue.
pub const OPEN_ORDERS: &str = "tradebot_open_orders";
/// Gauge, per `instrument`: latest close. Public market data, always on.
pub const LAST_CLOSE: &str = "tradebot_last_close";
/// Gauge, per `instrument` (money; `include_money` only): units held.
pub const POSITION_QTY: &str = "tradebot_position_qty";
/// Gauge, per `instrument` (money): total cost of the units held, in quote.
pub const POSITION_COST: &str = "tradebot_position_cost_basis";
/// Gauge, per `instrument` (money): realized P&L, in quote.
pub const REALIZED_PNL: &str = "tradebot_realized_pnl";

/// How often process metrics (memory, CPU, open files) are sampled.
const PROCESS_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// Starts the exporter and the process sampler if enabled. Never fails the
/// run.
pub fn start(cfg: &MetricsSection) {
    if !cfg.enabled {
        return;
    }
    let installed = metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(cfg.listen)
        .install();
    if let Err(e) = installed {
        tracing::warn!(error = %e, listen = %cfg.listen, "metrics exporter not started; trading continues");
        return;
    }
    tracing::info!(listen = %cfg.listen, "metrics at /metrics");
    tokio::spawn(async {
        let process = metrics_process::Collector::default();
        process.describe();
        loop {
            process.collect();
            tokio::time::sleep(PROCESS_EVERY).await;
        }
    });
}

/// Feeds the live-state gauges, and the API's live view, from each engine
/// snapshot.
pub struct RunObserver {
    pub include_money: bool,
    /// The API's live view, when the API is on. `send_replace` never waits
    /// for a reader.
    pub live: Option<tokio::sync::watch::Sender<Option<Snapshot>>>,
    /// Saves the strategy's state when it changes.
    pub state: Option<crate::state::Saver>,
}

/// Exporter-only conversion: a gauge is display and alerting, never read
/// back into a decision or the books (invariant 1 holds where money is
/// decided).
fn shown(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(f64::NAN)
}

impl Observer for RunObserver {
    fn on_snapshot(&mut self, s: &Snapshot) {
        if let Some(tx) = &self.live {
            tx.send_replace(Some(s.clone()));
        }
        if let Some(saver) = &self.state {
            saver.publish(&s.strategy_state);
        }
        metrics::gauge!(OPEN_ORDERS).set(s.open_orders.len() as f64);
        for c in &s.last_candles {
            metrics::gauge!(LAST_CLOSE, "instrument" => c.instrument.to_string())
                .set(shown(c.close.get()));
        }
        if !self.include_money {
            return;
        }
        for p in &s.positions {
            let id = p.instrument().to_string();
            metrics::gauge!(POSITION_QTY, "instrument" => id.clone()).set(shown(p.qty().get()));
            metrics::gauge!(POSITION_COST, "instrument" => id.clone())
                .set(shown(p.cost_basis().get()));
            metrics::gauge!(REALIZED_PNL, "instrument" => id).set(shown(p.realized_pnl().get()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::{Notional, Position, Qty};
    use rust_decimal_macros::dec;

    fn snapshot() -> Snapshot {
        Snapshot {
            positions: vec![
                Position::from_parts(
                    "binance:BTCUSDT".parse().unwrap(),
                    Qty::new(dec!(0.5)),
                    Notional::new(dec!(30000)),
                    Notional::new(dec!(12.5)),
                )
                .unwrap(),
            ],
            ..Snapshot::default()
        }
    }

    /// Prometheus text the observer produces for `snapshot()`.
    fn render(include_money: bool) -> String {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            RunObserver {
                include_money,
                live: None,
                state: None,
            }
            .on_snapshot(&snapshot());
        });
        handle.render()
    }

    #[test]
    fn snapshots_reach_the_api_view() {
        let (tx, rx) = tokio::sync::watch::channel(None);
        let mut o = RunObserver {
            include_money: false,
            live: Some(tx),
            state: None,
        };
        o.on_snapshot(&snapshot());
        assert_eq!(*rx.borrow(), Some(snapshot()));
    }

    #[test]
    fn money_stays_out_unless_asked_for() {
        let text = render(false);
        assert!(text.contains(OPEN_ORDERS), "{text}");
        assert!(!text.contains(POSITION_QTY), "{text}");
        assert!(!text.contains(REALIZED_PNL), "{text}");
    }

    #[test]
    fn money_is_exported_when_enabled() {
        let text = render(true);
        assert!(
            text.contains(r#"tradebot_position_qty{instrument="binance:BTCUSDT"} 0.5"#),
            "{text}"
        );
        assert!(
            text.contains(r#"tradebot_position_cost_basis{instrument="binance:BTCUSDT"} 30000"#),
            "{text}"
        );
        assert!(
            text.contains(r#"tradebot_realized_pnl{instrument="binance:BTCUSDT"} 12.5"#),
            "{text}"
        );
    }
}
