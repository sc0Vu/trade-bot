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
use super::signed::{Method, SignedClient};
use crate::{Books, ExchangeError};
use bot_core::{Instrument, InstrumentId, OrderId, Qty};
use rust_decimal::Decimal;
use std::collections::HashMap;
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

/// What a reconcile changed. Counts only: no money values, safe to log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Journal orders closed (cancelled, finished, or never placed).
    pub orders_closed: usize,
    /// Fills recorded that the books didn't have yet.
    pub fills_recovered: usize,
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
    for order in books.unfinished_orders().await? {
        let inst = by_id
            .get(&order.instrument)
            .ok_or_else(|| ExchangeError::UnknownInstrument(order.instrument.clone()))?;
        done.fills_recovered += close_out(client, inst, books, &order, retry_delay).await?;
        done.orders_closed += 1;
    }
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
    order: &JournalOrder,
    retry_delay: Duration,
) -> Result<usize, ExchangeError> {
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
            return Ok(0);
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
    let mut recovered = 0;
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
            if books
                .record_venue_fill(&fill, inst, &trade_id(t)?.to_string())
                .await?
                .is_some()
            {
                recovered += 1;
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
/// symbol is one the books don't know: placed by hand, by other software,
/// or by this bot under another database. Its fills would move balances
/// the books can't see, so trading next to it is refused.
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
            let whose = if cid.starts_with(CLIENT_ID_PREFIX) {
                "this bot's, not in this database"
            } else {
                "not this bot's"
            };
            unknown.push(format!("{} {cid} ({whose})", inst.id));
        }
    }
    if unknown.is_empty() {
        return Ok(());
    }
    Err(ExchangeError::Decode(format!(
        "open orders on traded symbols that the books don't know: {}; cancel them \
         or trade these symbols from another account",
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
        assert_eq!(
            r,
            Reconciled {
                orders_closed: 1,
                fills_recovered: 0
            }
        );
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
        assert_eq!(
            r,
            Reconciled {
                orders_closed: 1,
                fills_recovered: 1
            }
        );
        assert_eq!(held(&db).await, Some(Qty::new(dec!(1))));
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
        assert_eq!(
            r,
            Reconciled {
                orders_closed: 1,
                fills_recovered: 1
            }
        );
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
    async fn startup_refuses_orders_the_books_dont_know() {
        for (cid, whose) in [("web_abc", "not this bot's"), ("tbother-o-1", "this bot's")] {
            let (t, db) = (FakeTransport::default(), TempDb::new().await);
            t.reply(
                200,
                &format!(r#"[{{"orderId":99,"clientOrderId":"{cid}"}}]"#),
            );
            let err = run(&t, &db, Phase::Startup).await.unwrap_err().to_string();
            assert!(err.contains(cid) && err.contains(whose), "{err}");
        }
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
