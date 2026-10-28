//! Bringing the books back in line with Binance: at startup, before
//! trading, and at exit.
//!
//! Binance is the source of truth for what happened; the books record
//! which orders and fills are this bot's. Every order the journal hasn't
//! closed is found on Binance, cancelled if still working, waited on until
//! Binance reports it closed with every executed unit visible as a trade,
//! and its trades recorded (idempotently, so nothing counts twice). Strategy
//! state isn't persisted, so a leftover order has no strategy to manage it:
//! cancelling is the only safe choice.

use super::broker::{CLIENT_ID_PREFIX, executed_qty, is_terminal, trade_id, trade_to_fill};
use super::fees::FeeValuer;
use super::signed::{Method, SignedClient};
use crate::{Books, ExchangeError};
use bot_core::{Instrument, InstrumentId, OrderId, Qty};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::Duration;
use storage::JournalOrder;

/// Binance: the order doesn't exist (never placed, or unknown id).
const NO_SUCH_ORDER: i64 = -2013;
/// Binance: cancel rejected, typically because the order just closed.
const CANCEL_REJECTED: i64 = -2011;
/// Status polls per order before giving up on it settling.
const ATTEMPTS: u32 = 10;
/// Wait between status polls while Binance catches up.
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// When a reconcile runs. Startup also refuses to trade next to orders
/// the books don't know and checks holdings against the account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Startup,
    Exit,
}

/// What a reconcile changed. Log the counts; the fills carry money
/// values and are for the strategy (`Broker::take_recovered_fills`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Journal orders closed (cancelled, finished, or never placed).
    pub orders_closed: usize,
    /// Fills recorded that the books didn't have yet.
    pub fills_recovered: usize,
    /// Those fills, in the order recorded.
    pub fills: Vec<bot_core::Fill>,
}

/// Reconciles the books with Binance for `instruments`, which must cover
/// every instrument with an unfinished journal order. See the module docs.
///
/// Any failure stops: trading on books that disagree with the venue would
/// size orders from a wrong position.
pub async fn reconcile(
    client: &SignedClient,
    instruments: &[Instrument],
    books: &dyn Books,
    phase: Phase,
) -> Result<Reconciled, ExchangeError> {
    reconcile_with(client, instruments, books, phase, RETRY_DELAY).await
}

pub(crate) async fn reconcile_with(
    client: &SignedClient,
    instruments: &[Instrument],
    books: &dyn Books,
    phase: Phase,
    retry_delay: Duration,
) -> Result<Reconciled, ExchangeError> {
    let by_id: HashMap<&InstrumentId, &Instrument> =
        instruments.iter().map(|i| (&i.id, i)).collect();
    let mut done = Reconciled::default();
    let mut fees = FeeValuer::default();
    for order in books.unfinished_orders().await? {
        let inst = by_id
            .get(&order.instrument)
            .ok_or_else(|| ExchangeError::UnknownInstrument(order.instrument.clone()))?;
        let fills = close_out(client, inst, books, &mut fees, &order, retry_delay).await?;
        done.fills_recovered += fills.len();
        done.fills.extend(fills);
        done.orders_closed += 1;
    }
    let label = match phase {
        Phase::Startup => "startup",
        Phase::Exit => "exit",
    };
    metrics::counter!(crate::metrics::RECONCILE_ORDERS, "phase" => label)
        .increment(done.orders_closed as u64);
    metrics::counter!(crate::metrics::RECONCILE_FILLS, "phase" => label)
        .increment(done.fills_recovered as u64);
    if phase == Phase::Startup {
        refuse_unknown_open_orders(client, instruments).await?;
        check_balances(client, instruments, books).await?;
    }
    Ok(done)
}

/// Settles one journal order; returns how many fills it recovered.
async fn close_out(
    client: &SignedClient,
    inst: &Instrument,
    books: &dyn Books,
    fees: &mut FeeValuer,
    order: &JournalOrder,
    retry_delay: Duration,
) -> Result<Vec<bot_core::Fill>, ExchangeError> {
    let symbol = inst.id.symbol().to_string();
    // A pending order has no venue id yet: the reply to its submit was
    // lost. Its client id is unique (run tag), so it finds the order.
    let lookup = match order.venue_order_id {
        Some(id) => ("orderId", id.to_string()),
        None => ("origClientOrderId", order.client_order_id.clone()),
    };
    let first = client
        .signed(
            Method::Get,
            "/api/v3/order",
            &[("symbol", symbol.clone()), lookup],
        )
        .await;
    let mut status = match first {
        Err(ExchangeError::Venue {
            code: NO_SUCH_ORDER,
            ..
        }) if order.venue_order_id.is_none() => {
            // The submit never reached Binance.
            books.mark_closed(&order.client_order_id).await?;
            return Ok(Vec::new());
        }
        other => other?,
    };
    let venue_id = status["orderId"]
        .as_i64()
        .ok_or_else(|| ExchangeError::Decode("order orderId".into()))?;
    let by_venue_id = [
        ("symbol", symbol.clone()),
        ("orderId", venue_id.to_string()),
    ];
    if !is_terminal(&status) {
        match client
            .signed(Method::Delete, "/api/v3/order", &by_venue_id)
            .await
        {
            Ok(_)
            | Err(ExchangeError::Venue {
                code: CANCEL_REJECTED,
                ..
            }) => {}
            Err(e) => return Err(e),
        }
    }
    let order_id = OrderId::new(order.client_order_id.as_str())
        .map_err(|e| ExchangeError::Decode(format!("journaled client id: {e}")))?;
    let mut recovered = Vec::new();
    // Trades already handled by an earlier attempt: counted, not re-valued.
    let mut handled = HashSet::new();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(retry_delay).await;
            status = client
                .signed(Method::Get, "/api/v3/order", &by_venue_id)
                .await?;
        }
        let trades = client
            .signed(Method::Get, "/api/v3/myTrades", &by_venue_id)
            .await?;
        let mut seen = Qty::ZERO;
        for t in trades
            .as_array()
            .ok_or_else(|| ExchangeError::Decode("myTrades".into()))?
        {
            let fill = trade_to_fill(inst, &order_id, order.side, t)?;
            seen += fill.qty;
            let id = trade_id(t)?;
            if !handled.insert(id) {
                continue;
            }
            let fill = fees.complete(client, inst, fill).await?;
            if books
                .record_venue_fill(&fill, inst, &id.to_string(), &order.client_order_id)
                .await?
                .is_some()
            {
                recovered.push(fill);
            }
        }
        // Same rule as the broker: closed only when Binance says so AND
        // every executed unit is visible as a trade.
        if is_terminal(&status) && seen.get() == executed_qty(&status)? {
            books.mark_closed(&order.client_order_id).await?;
            return Ok(recovered);
        }
    }
    Err(ExchangeError::Decode(format!(
        "order {} did not settle on Binance; check the account before restarting",
        order.client_order_id
    )))
}

/// After the journal is closed out, any order still open on a traded
/// symbol is one the books don't know.
///
/// One with this bot's client-id prefix was placed by the bot under
/// another database: its fills would never reach these books, so the run
/// is refused. Anyone else's (placed by hand, by other software) is only
/// logged: the books count only this bot's fills, so it can't corrupt
/// them. At worst it uses balance a bot order needed, and that order is
/// refused by Binance (`-2010`), which the engine records and survives.
async fn refuse_unknown_open_orders(
    client: &SignedClient,
    instruments: &[Instrument],
) -> Result<(), ExchangeError> {
    let mut unknown = Vec::new();
    for inst in instruments {
        let open = client
            .signed(
                Method::Get,
                "/api/v3/openOrders",
                &[("symbol", inst.id.symbol().to_string())],
            )
            .await?;
        for o in open
            .as_array()
            .ok_or_else(|| ExchangeError::Decode("openOrders".into()))?
        {
            let cid = o["clientOrderId"].as_str().unwrap_or("?");
            if cid.starts_with(CLIENT_ID_PREFIX) {
                unknown.push(format!("{} {cid}", inst.id));
            } else {
                tracing::warn!(
                    instrument = %inst.id,
                    client_order_id = cid,
                    "open order not placed by this bot; its fills are not counted"
                );
            }
        }
    }
    if unknown.is_empty() {
        return Ok(());
    }
    Err(ExchangeError::Decode(format!(
        "open orders placed by this bot but missing from this database: {}; \
         was it run with another db.url? Cancel them or use that database",
        unknown.join(", ")
    )))
}

/// The books may hold less than the account (coins the bot never bought),
/// never more: more means fills the books counted didn't happen, or the
/// balance moved outside the bot (a withdrawal, a testnet reset).
async fn check_balances(
    client: &SignedClient,
    instruments: &[Instrument],
    books: &dyn Books,
) -> Result<(), ExchangeError> {
    let account = client.signed(Method::Get, "/api/v3/account", &[]).await?;
    let mut balances: HashMap<&str, Decimal> = HashMap::new();
    for b in account["balances"]
        .as_array()
        .ok_or_else(|| ExchangeError::Decode("account balances".into()))?
    {
        let amount = |k: &str| {
            b[k].as_str()
                .and_then(|s| Decimal::from_str(s).ok())
                .ok_or_else(|| ExchangeError::Decode(format!("balance {k}")))
        };
        let asset = b["asset"]
            .as_str()
            .ok_or_else(|| ExchangeError::Decode("balance asset".into()))?;
        *balances.entry(asset).or_default() += amount("free")? + amount("locked")?;
    }
    for inst in instruments {
        let Some(held) = books.get_holding(&inst.id).await? else {
            continue;
        };
        let have = balances
            .get(inst.base.as_str())
            .copied()
            .unwrap_or_default();
        if held.qty().get() > have {
            return Err(ExchangeError::Decode(format!(
                "the books hold more {} than the account does; if the network was \
                 reset or coins were moved, start from a fresh db.url",
                inst.base
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binance::fees::tests::{BNBUSDT_INFO, bnb_kline};
    use crate::binance::signed::Credentials;
    use crate::binance::signed::fake::{FakeTransport, FixedClock, param};
    use crate::test_db::TempDb;
    use bot_core::{Asset, AssetClass, InstrumentSpec, Notional, Position, Price, Side, Timestamp};
    use rust_decimal_macros::dec;
    use storage::{HoldingRepository, OrderJournal};

    fn btcusdt() -> Instrument {
        Instrument::new(InstrumentSpec {
            id: "binance:BTCUSDT".parse().unwrap(),
            class: AssetClass::Spot,
            base: Asset::new("BTC").unwrap(),
            quote: Asset::new("USDT").unwrap(),
            tick_size: Price::new(dec!(0.01)),
            lot_size: Qty::new(dec!(0.00001)),
            min_qty: Qty::new(dec!(0.00001)),
            min_notional: Notional::new(dec!(5)),
            max_open_orders: None,
            max_open_stops: None,
            price_band: None,
        })
        .unwrap()
    }

    fn client(t: &FakeTransport) -> SignedClient {
        SignedClient::new(
            "x",
            Box::new(t.clone()),
            Credentials::new("k", "s"),
            Box::new(FixedClock(0)),
        )
    }

    fn trade(id: i64, qty: &str) -> String {
        format!(
            r#"{{"id":{id},"orderId":28,"price":"100","qty":"{qty}","commission":"0","commissionAsset":"USDT","time":5,"isBuyer":true,"isMaker":true}}"#
        )
    }

    async fn journal(db: &TempDb, venue_id: Option<i64>) {
        db.store
            .insert_pending(
                "tbold-o-3",
                &btcusdt().id,
                Side::Buy,
                Timestamp::from_millis(1),
            )
            .await
            .unwrap();
        if let Some(id) = venue_id {
            db.store.mark_open("tbold-o-3", id).await.unwrap();
        }
    }

    async fn run(
        t: &FakeTransport,
        db: &TempDb,
        phase: Phase,
    ) -> Result<Reconciled, ExchangeError> {
        reconcile_with(&client(t), &[btcusdt()], &*db.store, phase, Duration::ZERO).await
    }

    async fn held(db: &TempDb) -> Option<Qty> {
        db.store
            .get_holding(&btcusdt().id)
            .await
            .unwrap()
            .map(|p| p.qty())
    }

    #[tokio::test]
    async fn submit_that_never_reached_binance_is_closed() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        journal(&db, None).await;
        t.reply(400, r#"{"code":-2013,"msg":"Order does not exist."}"#);
        let r = run(&t, &db, Phase::Exit).await.unwrap();
        assert_eq!((r.orders_closed, r.fills_recovered), (1, 0));
        assert_eq!(
            param(&t.sent()[0].query, "origClientOrderId"),
            Some("tbold-o-3")
        );
        assert!(db.store.unfinished_orders().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fill_that_happened_while_down_is_recovered() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        journal(&db, Some(28)).await;
        t.reply(200, r#"{"orderId":28,"status":"FILLED","executedQty":"1"}"#);
        t.reply(200, &format!("[{}]", trade(7, "1")));
        let r = run(&t, &db, Phase::Exit).await.unwrap();
        assert_eq!((r.orders_closed, r.fills_recovered), (1, 1));
        // The fill itself, for the strategy: under the journaled client id.
        assert_eq!(r.fills.len(), 1);
        assert_eq!(r.fills[0].order_id.as_str(), "tbold-o-3");
        assert_eq!(r.fills[0].qty, Qty::new(dec!(1)));
        assert_eq!(held(&db).await, Some(Qty::new(dec!(1))));
        let trades = storage::TradeRepository::recent_trades(db.store.as_ref(), 10)
            .await
            .unwrap();
        assert_eq!(trades[0].client_order_id.as_deref(), Some("tbold-o-3"));
        assert!(
            t.sent().iter().all(|r| r.method != Method::Delete),
            "a closed order is not cancelled"
        );
        assert!(db.store.unfinished_orders().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn working_order_is_cancelled_and_its_partial_fill_kept() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        journal(&db, Some(28)).await;
        t.reply(200, r#"{"orderId":28,"status":"NEW","executedQty":"0"}"#);
        t.reply(200, r#"{"orderId":28,"status":"CANCELED"}"#); // DELETE
        t.reply(200, "[]"); // trades not caught up yet
        t.reply(
            200,
            r#"{"orderId":28,"status":"CANCELED","executedQty":"0.4"}"#,
        );
        t.reply(200, &format!("[{}]", trade(7, "0.4")));
        let r = run(&t, &db, Phase::Exit).await.unwrap();
        assert_eq!((r.orders_closed, r.fills_recovered), (1, 1));
        let del = &t.sent()[1];
        assert_eq!(
            (del.method, param(&del.query, "orderId")),
            (Method::Delete, Some("28"))
        );
        assert_eq!(held(&db).await, Some(Qty::new(dec!(0.4))));
    }

    #[tokio::test]
    async fn order_that_filled_as_the_cancel_went_out_is_fine() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        journal(&db, Some(28)).await;
        t.reply(200, r#"{"orderId":28,"status":"NEW","executedQty":"0"}"#);
        t.reply(400, r#"{"code":-2011,"msg":"Unknown order sent."}"#);
        t.reply(200, "[]");
        t.reply(200, r#"{"orderId":28,"status":"FILLED","executedQty":"1"}"#);
        t.reply(200, &format!("[{}]", trade(7, "1")));
        assert_eq!(run(&t, &db, Phase::Exit).await.unwrap().fills_recovered, 1);
    }

    #[tokio::test]
    async fn pending_order_whose_reply_was_lost_is_found_by_client_id() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        journal(&db, None).await;
        t.reply(200, r#"{"orderId":28,"status":"FILLED","executedQty":"1"}"#);
        t.reply(200, &format!("[{}]", trade(7, "1")));
        assert_eq!(run(&t, &db, Phase::Exit).await.unwrap().fills_recovered, 1);
        assert_eq!(param(&t.sent()[1].query, "orderId"), Some("28"));
    }

    #[tokio::test]
    async fn fills_already_in_the_books_are_not_counted_again() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        journal(&db, Some(28)).await;
        for _ in 0..2 {
            t.reply(200, r#"{"orderId":28,"status":"FILLED","executedQty":"1"}"#);
            t.reply(200, &format!("[{}]", trade(7, "1")));
        }
        run(&t, &db, Phase::Exit).await.unwrap();
        // The row is closed now; reopen it as if a crash beat mark_closed.
        db.store
            .insert_pending(
                "tbold-o-4",
                &btcusdt().id,
                Side::Buy,
                Timestamp::from_millis(2),
            )
            .await
            .unwrap();
        db.store.mark_open("tbold-o-4", 28).await.unwrap();
        let r = run(&t, &db, Phase::Exit).await.unwrap();
        assert_eq!(r.fills_recovered, 0);
        assert_eq!(held(&db).await, Some(Qty::new(dec!(1))));
    }

    /// The demo-account case: an order filled with BNB fees before the
    /// bot could value them. Reconcile now values and records them.
    #[tokio::test]
    async fn bnb_fee_trades_are_valued_on_recovery() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        journal(&db, Some(28)).await;
        t.reply(200, r#"{"orderId":28,"status":"FILLED","executedQty":"1"}"#);
        t.reply(
            200,
            r#"[{"id":7,"orderId":28,"price":"100","qty":"1","commission":"0.0001","commissionAsset":"BNB","time":5,"isBuyer":true,"isMaker":true}]"#,
        );
        t.reply(200, BNBUSDT_INFO);
        t.reply(200, &bnb_kline(0, "600.00"));
        assert_eq!(run(&t, &db, Phase::Exit).await.unwrap().fills_recovered, 1);
        let pos = db.store.get_holding(&btcusdt().id).await.unwrap().unwrap();
        assert_eq!(pos.cost_basis(), Notional::new(dec!(100.06)));
    }

    #[tokio::test]
    async fn order_that_never_settles_stops_with_the_row_left_open() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        journal(&db, Some(28)).await;
        t.reply(200, r#"{"orderId":28,"status":"FILLED","executedQty":"1"}"#);
        t.reply(200, "[]");
        for _ in 1..ATTEMPTS {
            t.reply(200, r#"{"orderId":28,"status":"FILLED","executedQty":"1"}"#);
            t.reply(200, "[]");
        }
        let err = run(&t, &db, Phase::Exit).await.unwrap_err();
        assert!(err.to_string().contains("did not settle"), "{err}");
        assert_eq!(db.store.unfinished_orders().await.unwrap().len(), 1);
    }

    fn account(btc: &str) -> String {
        format!(
            r#"{{"balances":[{{"asset":"BTC","free":"{btc}","locked":"0.5"}},{{"asset":"USDT","free":"10","locked":"0"}}]}}"#
        )
    }

    #[tokio::test]
    async fn startup_refuses_bot_orders_missing_from_the_books() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        t.reply(200, r#"[{"orderId":99,"clientOrderId":"tbother-o-1"}]"#);
        let err = run(&t, &db, Phase::Startup).await.unwrap_err().to_string();
        assert!(err.contains("tbother-o-1"), "{err}");
    }

    #[tokio::test]
    async fn startup_tolerates_orders_placed_by_others() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        t.reply(200, r#"[{"orderId":99,"clientOrderId":"web_abc"}]"#);
        t.reply(200, &account("0"));
        run(&t, &db, Phase::Startup).await.unwrap();
        assert!(
            t.sent().iter().all(|r| r.method != Method::Delete),
            "someone else's order is left alone"
        );
    }

    #[tokio::test]
    async fn startup_checks_books_against_balances() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        db.store
            .upsert_holding(
                &Position::from_parts(
                    btcusdt().id.clone(),
                    Qty::new(dec!(1)),
                    Notional::new(dec!(100)),
                    Notional::ZERO,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        // 0.5 free + 0.5 locked covers the 1 BTC the books hold.
        t.reply(200, "[]");
        t.reply(200, &account("0.5"));
        run(&t, &db, Phase::Startup).await.unwrap();
        // 0.4 + 0.5 does not.
        t.reply(200, "[]");
        t.reply(200, &account("0.4"));
        let err = run(&t, &db, Phase::Startup).await.unwrap_err().to_string();
        assert!(err.contains("more BTC"), "{err}");
    }

    #[tokio::test]
    async fn exit_skips_the_startup_checks() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        run(&t, &db, Phase::Exit).await.unwrap();
        assert!(t.sent().is_empty());
    }
}
