use super::signed::{Method, SignedClient};
use crate::{Broker, ExchangeError};
use async_trait::async_trait;
use bot_core::{
    Fee, Fill, FillSpec, Instrument, InstrumentId, Liquidity, MarketEvent, Notional, Order,
    OrderId, OrderKind, Position, Price, Qty, Side, Size, TimeInForce, Timestamp,
};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

/// Decimal places Binance accepts for a USDT `quoteOrderQty`.
const QUOTE_DP: u32 = 8;
/// Binance's limit on `newClientOrderId` (`^[a-zA-Z0-9-_]{1,36}$`).
const CLIENT_ID_MAX: usize = 36;
/// Start of every client order id this bot sends, so its orders can be
/// told apart from ones placed by hand or by other software.
pub const CLIENT_ID_PREFIX: &str = "tb";

/// A per-run tag for client order ids: [`CLIENT_ID_PREFIX`] plus the run's
/// start time in base 36 (8 digits until 2059). Engine order ids restart at
/// `o-1` every run; the tag keeps `newClientOrderId` unique across runs.
pub fn run_tag(start_millis: i64) -> String {
    let mut n = start_millis.unsigned_abs();
    let mut digits = Vec::new();
    loop {
        digits.push(char::from_digit((n % 36) as u32, 36).unwrap_or('0'));
        n /= 36;
        if n == 0 {
            break;
        }
    }
    digits.reverse();
    format!(
        "{CLIENT_ID_PREFIX}{}",
        digits.into_iter().collect::<String>()
    )
}

struct LiveOrder {
    order: Order,
    /// Binance's own id, from the submit reply. Status and cancel go by
    /// this, never by client id: a client id is unique only among open
    /// orders, so an old closed order can share it.
    venue_id: i64,
    /// Cancel sent; still polled until Binance reports it closed, so a fill
    /// that raced the cancel isn't lost.
    cancelling: bool,
    seen_trades: HashSet<i64>,
    /// Base units across `seen_trades`.
    seen_qty: Qty,
}

/// Binance spot broker over signed REST.
///
/// `submit` places the order with `newClientOrderId` = run tag + our
/// `OrderId` ([`run_tag`]) and keeps Binance's `orderId` from the reply;
/// `settle` polls each open order's trades (`myTrades?orderId=`) and
/// status, turning new trades into fills (partial fills included) and
/// dropping orders Binance reports closed. Polling per event keeps the
/// `Broker` contract identical to the paper broker's: fills surface at the
/// next event, never mid-strategy.
///
/// Positions come only from this bot's fills, starting flat; balances the
/// account already held are ignored. Pre-submit checks match
/// `PaperBroker` (unknown instrument, reused id, sell beyond holding minus
/// pending sells), so a strategy sees the same refusals in paper and live
/// (invariant 3). A fee charged in a third asset (BNB) is an error: v1
/// can't value it, so trading stops instead of mis-accounting it.
pub struct BinanceBroker {
    client: SignedClient,
    instruments: HashMap<InstrumentId, Instrument>,
    positions: HashMap<InstrumentId, Position>,
    open: Vec<LiveOrder>,
    run_tag: String,
}

impl BinanceBroker {
    /// `run_tag` prefixes every client order id sent this run; see
    /// [`run_tag`].
    pub fn new(client: SignedClient, instruments: Vec<Instrument>, run_tag: String) -> Self {
        Self {
            client,
            run_tag,
            instruments: instruments.into_iter().map(|i| (i.id.clone(), i)).collect(),
            positions: HashMap::new(),
            open: Vec::new(),
        }
    }

    fn pending_sell_qty(&self, id: &InstrumentId) -> Qty {
        self.open
            .iter()
            .filter(|o| o.order.instrument == *id && o.order.side == Side::Sell)
            .map(|o| match o.order.size {
                Size::Base(q) => q,
                Size::Quote(_) => Qty::ZERO,
            })
            .sum()
    }

    async fn poll(&mut self, idx: usize) -> Result<(Vec<Fill>, bool), ExchangeError> {
        let (symbol, exchange_id) = {
            let o = &self.open[idx];
            (o.order.instrument.symbol().to_string(), o.venue_id)
        };
        let status = self
            .client
            .signed(
                Method::Get,
                "/api/v3/order",
                &[
                    ("symbol", symbol.clone()),
                    ("orderId", exchange_id.to_string()),
                ],
            )
            .await?;
        let trades = self
            .client
            .signed(
                Method::Get,
                "/api/v3/myTrades",
                &[("symbol", symbol), ("orderId", exchange_id.to_string())],
            )
            .await?;
        let mut fills = Vec::new();
        for t in trades
            .as_array()
            .ok_or_else(|| ExchangeError::Decode("myTrades".into()))?
        {
            let id = t["id"]
                .as_i64()
                .ok_or_else(|| ExchangeError::Decode("trade id".into()))?;
            if self.open[idx].seen_trades.contains(&id) {
                continue;
            }
            let fill = self.trade_to_fill(&self.open[idx].order, t)?;
            let inst = self
                .instruments
                .get(&fill.instrument)
                .ok_or_else(|| ExchangeError::UnknownInstrument(fill.instrument.clone()))?;
            self.positions
                .entry(fill.instrument.clone())
                .or_insert_with(|| Position::flat(fill.instrument.clone()))
                .apply(&fill, inst)
                .map_err(|e| {
                    ExchangeError::Decode(format!("fill can't be applied to position: {e}"))
                })?;
            self.open[idx].seen_trades.insert(id);
            self.open[idx].seen_qty += fill.qty;
            fills.push(fill);
        }
        // Closed only when Binance says nothing more will fill AND every
        // executed unit has shown up as a trade: the two endpoints can lag
        // each other, and dropping the order early would lose a fill.
        let terminal = matches!(
            status["status"].as_str(),
            Some("FILLED" | "CANCELED" | "EXPIRED" | "REJECTED" | "EXPIRED_IN_MATCH")
        );
        let executed = status["executedQty"]
            .as_str()
            .and_then(|s| Decimal::from_str(s).ok())
            .ok_or_else(|| ExchangeError::Decode("order executedQty".into()))?;
        let done = terminal && self.open[idx].seen_qty.get() == executed;
        Ok((fills, done))
    }

    fn trade_to_fill(&self, order: &Order, t: &Value) -> Result<Fill, ExchangeError> {
        let inst = self
            .instruments
            .get(&order.instrument)
            .ok_or_else(|| ExchangeError::UnknownInstrument(order.instrument.clone()))?;
        let dec = |name: &str| -> Result<Decimal, ExchangeError> {
            t[name]
                .as_str()
                .and_then(|s| Decimal::from_str(s).ok())
                .ok_or_else(|| ExchangeError::Decode(format!("trade {name}")))
        };
        let commission = dec("commission")?;
        let asset = t["commissionAsset"].as_str().unwrap_or_default();
        let fee = if asset == inst.base.as_str() {
            Fee::Base(Qty::new(commission))
        } else if asset == inst.quote.as_str() {
            Fee::Quote(Notional::new(commission))
        } else {
            return Err(ExchangeError::Decode(format!(
                "fee charged in {asset}; v1 values fees only in base or quote. \
                 Turn off paying fees with BNB on the account."
            )));
        };
        let side = if t["isBuyer"].as_bool() == Some(true) {
            Side::Buy
        } else {
            Side::Sell
        };
        if side != order.side {
            return Err(ExchangeError::Decode(
                "trade side differs from order side".into(),
            ));
        }
        Fill::new(FillSpec {
            order_id: order.id.clone(),
            instrument: order.instrument.clone(),
            side,
            qty: Qty::new(dec("qty")?),
            price: Price::new(dec("price")?),
            fee,
            liquidity: if t["isMaker"].as_bool() == Some(true) {
                Liquidity::Maker
            } else {
                Liquidity::Taker
            },
            ts: Timestamp::from_millis(
                t["time"]
                    .as_i64()
                    .ok_or_else(|| ExchangeError::Decode("trade time".into()))?,
            ),
        })
        .map_err(|e| ExchangeError::Decode(e.to_string()))
    }
}

/// Plain decimal text Binance accepts: no exponent, no trailing zeros.
fn text(d: Decimal) -> String {
    d.normalize().to_string()
}

fn tif(t: TimeInForce) -> String {
    match t {
        TimeInForce::Gtc => "GTC",
        TimeInForce::Ioc => "IOC",
        TimeInForce::Fok => "FOK",
    }
    .to_owned()
}

/// Order parameters for `POST /api/v3/order`.
fn order_params(o: &Order, client_id: String) -> Vec<(&'static str, String)> {
    let mut p = vec![
        ("symbol", o.instrument.symbol().to_string()),
        (
            "side",
            match o.side {
                Side::Buy => "BUY",
                Side::Sell => "SELL",
            }
            .to_owned(),
        ),
    ];
    let (kind, extra): (&str, Vec<(&'static str, String)>) = match o.kind {
        OrderKind::Market => ("MARKET", vec![]),
        OrderKind::Limit { price, tif: t } => (
            "LIMIT",
            vec![("timeInForce", tif(t)), ("price", text(price.get()))],
        ),
        OrderKind::StopMarket { stop } => ("STOP_LOSS", vec![("stopPrice", text(stop.get()))]),
        OrderKind::StopLimit {
            stop,
            limit,
            tif: t,
        } => (
            "STOP_LOSS_LIMIT",
            vec![
                ("timeInForce", tif(t)),
                ("price", text(limit.get())),
                ("stopPrice", text(stop.get())),
            ],
        ),
    };
    p.push(("type", kind.to_owned()));
    match o.size {
        Size::Base(q) => p.push(("quantity", text(q.get()))),
        // Round toward zero to the quote asset's precision: never above budget.
        Size::Quote(n) => p.push((
            "quoteOrderQty",
            text(
                n.get()
                    .round_dp_with_strategy(QUOTE_DP, RoundingStrategy::ToZero),
            ),
        )),
    }
    p.extend(extra);
    p.push(("newClientOrderId", client_id));
    p.push(("newOrderRespType", "ACK".to_owned()));
    p
}

#[async_trait]
impl Broker for BinanceBroker {
    async fn submit(&mut self, order: Order) -> Result<OrderId, ExchangeError> {
        if !self.instruments.contains_key(&order.instrument) {
            return Err(ExchangeError::UnknownInstrument(order.instrument.clone()));
        }
        if self.open.iter().any(|o| o.order.id == order.id) {
            return Err(ExchangeError::DuplicateOrder(order.id.clone()));
        }
        if let (Side::Sell, Size::Base(q)) = (order.side, order.size) {
            let held = self.position(&order.instrument).qty();
            if self.pending_sell_qty(&order.instrument) + q > held {
                return Err(ExchangeError::Rejected("sell exceeds holding"));
            }
        }
        let client_id = format!("{}-{}", self.run_tag, order.id);
        if client_id.len() > CLIENT_ID_MAX {
            return Err(ExchangeError::Rejected(
                "client order id longer than 36 chars",
            ));
        }
        let ack = self
            .client
            .signed(
                Method::Post,
                "/api/v3/order",
                &order_params(&order, client_id),
            )
            .await?;
        let venue_id = ack["orderId"]
            .as_i64()
            .ok_or_else(|| ExchangeError::Decode("order ack orderId".into()))?;
        let id = order.id.clone();
        self.open.push(LiveOrder {
            order,
            venue_id,
            cancelling: false,
            seen_trades: HashSet::new(),
            seen_qty: Qty::ZERO,
        });
        Ok(id)
    }

    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        let o = self
            .open
            .iter_mut()
            .find(|o| o.order.id == id && !o.cancelling)
            .ok_or_else(|| ExchangeError::UnknownOrder(id.clone()))?;
        self.client
            .signed(
                Method::Delete,
                "/api/v3/order",
                &[
                    ("symbol", o.order.instrument.symbol().to_string()),
                    ("orderId", o.venue_id.to_string()),
                ],
            )
            .await?;
        o.cancelling = true;
        Ok(())
    }

    async fn settle(&mut self, _event: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        let mut fills = Vec::new();
        let mut idx = 0;
        while idx < self.open.len() {
            let (new, done) = self.poll(idx).await?;
            fills.extend(new);
            if done {
                self.open.remove(idx);
            } else {
                idx += 1;
            }
        }
        fills.sort_by_key(|f| f.ts);
        Ok(fills)
    }

    fn position(&self, instrument: &InstrumentId) -> Position {
        self.positions
            .get(instrument)
            .cloned()
            .unwrap_or_else(|| Position::flat(instrument.clone()))
    }

    fn open_orders(&self) -> Vec<Order> {
        self.open
            .iter()
            .filter(|o| !o.cancelling)
            .map(|o| o.order.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binance::signed::fake::{FakeTransport, FixedClock, param};
    use crate::binance::signed::{Credentials, sign};
    use bot_core::{Asset, AssetClass, InstrumentSpec, OrderSpec};
    use rust_decimal_macros::dec;

    const SECRET: &str = "test-secret";

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

    fn broker() -> (BinanceBroker, FakeTransport) {
        let t = FakeTransport::default();
        let client = SignedClient::new(
            "https://testnet.binance.vision",
            Box::new(t.clone()),
            Credentials::new("test-key", SECRET),
            Box::new(FixedClock(1_700_000_000_000)),
        );
        (
            BinanceBroker::new(client, vec![btcusdt()], "tbrun".into()),
            t,
        )
    }

    fn order(id: &str, side: Side, size: Size, kind: OrderKind) -> Order {
        Order::new(OrderSpec {
            id: OrderId::new(id).unwrap(),
            instrument: btcusdt().id.clone(),
            side,
            size,
            kind,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    fn any_event() -> MarketEvent {
        MarketEvent::SessionOpen {
            venue: bot_core::VenueId::new("binance").unwrap(),
            ts: Timestamp::from_millis(0),
        }
    }

    fn trade(
        id: i64,
        price: &str,
        qty: &str,
        commission: &str,
        asset: &str,
        buyer: bool,
    ) -> String {
        format!(
            r#"{{"symbol":"BTCUSDT","id":{id},"orderId":28,"orderListId":-1,"price":"{price}","qty":"{qty}",
            "quoteQty":"0","commission":"{commission}","commissionAsset":"{asset}","time":3600000,
            "isBuyer":{buyer},"isMaker":false,"isBestMatch":true}}"#
        )
    }

    /// The owner-confirmed paper scenario, replayed as a Binance round
    /// trip: market buy with a 1000 USDT budget fills 9.90099 BTC @ 101,
    /// fee 0.00990099 BTC -> hold 9.89108901 BTC at cost 999.99999.
    #[tokio::test]
    async fn market_quote_buy_round_trip() {
        let (mut b, t) = broker();
        t.reply(200, r#"{"symbol":"BTCUSDT","orderId":28,"orderListId":-1,"clientOrderId":"o-1","transactTime":1700000000000}"#);
        let buy = order(
            "o-1",
            Side::Buy,
            Size::Quote(Notional::new(dec!(1000))),
            OrderKind::Market,
        );
        assert_eq!(b.submit(buy).await.unwrap().as_str(), "o-1");

        let req = &t.sent()[0];
        assert_eq!(
            (req.method, req.path.as_str()),
            (Method::Post, "/api/v3/order")
        );
        assert_eq!(req.api_key.as_deref(), Some("test-key"));
        let (unsigned, sig) = req.query.rsplit_once("&signature=").unwrap();
        assert_eq!(
            unsigned,
            "symbol=BTCUSDT&side=BUY&type=MARKET&quoteOrderQty=1000&newClientOrderId=tbrun-o-1\
             &newOrderRespType=ACK&recvWindow=5000&timestamp=1700000000000"
        );
        assert_eq!(sig, sign(SECRET, unsigned));
        assert_eq!(b.open_orders().len(), 1);

        t.reply(
            200,
            r#"{"symbol":"BTCUSDT","orderId":28,"clientOrderId":"o-1","status":"FILLED","executedQty":"9.90099000"}"#,
        );
        t.reply(
            200,
            &format!(
                "[{}]",
                trade(5, "101.00", "9.90099", "0.00990099", "BTC", true)
            ),
        );
        let fills = b.settle(&any_event()).await.unwrap();

        let sent = t.sent();
        // Looked up by Binance's id from the ack, not by client id.
        assert_eq!(param(&sent[1].query, "orderId"), Some("28"));
        assert_eq!(param(&sent[1].query, "origClientOrderId"), None);
        assert_eq!(
            (sent[2].path.as_str(), param(&sent[2].query, "orderId")),
            ("/api/v3/myTrades", Some("28"))
        );
        assert_eq!(fills.len(), 1);
        let f = &fills[0];
        assert_eq!(
            (f.price, f.qty),
            (Price::new(dec!(101)), Qty::new(dec!(9.90099)))
        );
        assert_eq!(f.fee, Fee::Base(Qty::new(dec!(0.00990099))));
        assert_eq!(f.liquidity, Liquidity::Taker);
        let pos = b.position(&btcusdt().id);
        assert_eq!(pos.qty(), Qty::new(dec!(9.89108901)));
        assert_eq!(pos.cost_basis(), Notional::new(dec!(999.99999)));
        assert!(b.open_orders().is_empty());

        // Order closed: nothing polled, nothing re-reported.
        assert!(b.settle(&any_event()).await.unwrap().is_empty());
        assert_eq!(t.sent().len(), 3);
    }

    #[tokio::test]
    async fn partial_fills_are_reported_once_each() {
        let (mut b, t) = broker();
        t.reply(200, r#"{"orderId":28}"#);
        let limit = Order::new(OrderSpec {
            kind: OrderKind::Limit {
                price: Price::new(dec!(100)),
                tif: TimeInForce::Gtc,
            },
            ..order(
                "o-1",
                Side::Buy,
                Size::Base(Qty::new(dec!(2))),
                OrderKind::Market,
            )
            .into()
        })
        .unwrap();
        b.submit(limit).await.unwrap();
        assert!(
            t.sent()[0]
                .query
                .contains("type=LIMIT&quantity=2&timeInForce=GTC&price=100")
        );

        t.reply(
            200,
            r#"{"orderId":28,"status":"PARTIALLY_FILLED","executedQty":"0.5"}"#,
        );
        t.reply(
            200,
            &format!("[{}]", trade(1, "100", "0.5", "0.05", "USDT", true)),
        );
        assert_eq!(b.settle(&any_event()).await.unwrap().len(), 1);
        assert_eq!(b.open_orders().len(), 1);

        t.reply(200, r#"{"orderId":28,"status":"FILLED","executedQty":"2"}"#);
        t.reply(
            200,
            &format!(
                "[{},{}]",
                trade(1, "100", "0.5", "0.05", "USDT", true),
                trade(2, "100", "1.5", "0.15", "USDT", true)
            ),
        );
        let second = b.settle(&any_event()).await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].qty, Qty::new(dec!(1.5)));
        // 2 BTC @ 100 + 0.2 USDT fees.
        assert_eq!(
            b.position(&btcusdt().id).cost_basis(),
            Notional::new(dec!(200.2))
        );
        assert!(b.open_orders().is_empty());
    }

    #[tokio::test]
    async fn cancel_is_polled_until_closed() {
        let (mut b, t) = broker();
        t.reply(200, r#"{"orderId":28}"#);
        let limit = Order::new(OrderSpec {
            kind: OrderKind::Limit {
                price: Price::new(dec!(50)),
                tif: TimeInForce::Gtc,
            },
            ..order(
                "o-1",
                Side::Buy,
                Size::Base(Qty::new(dec!(1))),
                OrderKind::Market,
            )
            .into()
        })
        .unwrap();
        b.submit(limit).await.unwrap();
        t.reply(200, r#"{"orderId":28,"status":"CANCELED"}"#);
        b.cancel(OrderId::new("o-1").unwrap()).await.unwrap();
        let del = &t.sent()[1];
        assert_eq!(
            (del.method, param(&del.query, "orderId")),
            (Method::Delete, Some("28"))
        );
        assert!(
            b.open_orders().is_empty(),
            "cancelling orders are hidden from strategies"
        );

        t.reply(
            200,
            r#"{"orderId":28,"status":"CANCELED","executedQty":"0"}"#,
        );
        t.reply(200, "[]");
        assert!(b.settle(&any_event()).await.unwrap().is_empty());
        assert!(b.settle(&any_event()).await.unwrap().is_empty());
        assert_eq!(t.sent().len(), 4, "closed order is no longer polled");
    }

    /// Status says FILLED but myTrades hasn't caught up: the order must stay
    /// open until the trade shows, or the fill would be lost.
    #[tokio::test]
    async fn filled_status_ahead_of_trades_keeps_polling() {
        let (mut b, t) = broker();
        t.reply(200, r#"{"orderId":28}"#);
        b.submit(order(
            "o-1",
            Side::Buy,
            Size::Base(Qty::new(dec!(1))),
            OrderKind::Market,
        ))
        .await
        .unwrap();
        t.reply(200, r#"{"orderId":28,"status":"FILLED","executedQty":"1"}"#);
        t.reply(200, "[]");
        assert!(b.settle(&any_event()).await.unwrap().is_empty());
        assert_eq!(b.open_orders().len(), 1);
        t.reply(200, r#"{"orderId":28,"status":"FILLED","executedQty":"1"}"#);
        t.reply(
            200,
            &format!("[{}]", trade(7, "100", "1", "0.001", "BTC", true)),
        );
        assert_eq!(b.settle(&any_event()).await.unwrap().len(), 1);
        assert!(b.open_orders().is_empty());
    }

    #[tokio::test]
    async fn bnb_fee_stops_rather_than_misaccounts() {
        let (mut b, t) = broker();
        t.reply(200, r#"{"orderId":28}"#);
        b.submit(order(
            "o-1",
            Side::Buy,
            Size::Base(Qty::new(dec!(1))),
            OrderKind::Market,
        ))
        .await
        .unwrap();
        t.reply(200, r#"{"orderId":28,"status":"FILLED","executedQty":"1"}"#);
        t.reply(
            200,
            &format!("[{}]", trade(1, "100", "1", "0.0001", "BNB", true)),
        );
        let err = b.settle(&any_event()).await.unwrap_err();
        assert!(err.to_string().contains("BNB"));
        assert!(b.position(&btcusdt().id).is_flat());
    }

    #[tokio::test]
    async fn refuses_like_the_paper_broker_without_calling_binance() {
        let (mut b, t) = broker();
        let sell = order(
            "o-1",
            Side::Sell,
            Size::Base(Qty::new(dec!(1))),
            OrderKind::Market,
        );
        assert!(matches!(
            b.submit(sell).await,
            Err(ExchangeError::Rejected(_))
        ));
        assert!(t.sent().is_empty());
    }

    #[tokio::test]
    async fn venue_rejection_leaves_no_open_order() {
        let (mut b, t) = broker();
        t.reply(400, r#"{"code":-1013,"msg":"Filter failure: NOTIONAL"}"#);
        let buy = order(
            "o-1",
            Side::Buy,
            Size::Quote(Notional::new(dec!(1))),
            OrderKind::Market,
        );
        assert!(matches!(
            b.submit(buy).await,
            Err(ExchangeError::Venue { code: -1013, .. })
        ));
        assert!(b.open_orders().is_empty());
    }

    #[test]
    fn run_tag_is_base36_start_time() {
        assert_eq!(run_tag(0), "tb0");
        assert_eq!(run_tag(35), "tbz");
        assert_eq!(run_tag(36), "tb10");
        // 2026-10-22T00:00:00Z.
        assert_eq!(run_tag(1_792_627_200_000), "tbmviry800");
        // Any start time with a 9-digit tag (until about year 5100) fits
        // Binance's 36-char limit with the largest engine id.
        let longest = format!("{}-o-{}", run_tag(36_i64.pow(9) - 1), u64::MAX);
        assert!(longest.len() <= CLIENT_ID_MAX, "{longest}");
    }

    #[tokio::test]
    async fn ack_without_order_id_is_an_error() {
        let (mut b, t) = broker();
        t.reply(200, "{}");
        let buy = order(
            "o-1",
            Side::Buy,
            Size::Base(Qty::new(dec!(1))),
            OrderKind::Market,
        );
        assert!(matches!(b.submit(buy).await, Err(ExchangeError::Decode(_))));
    }

    #[test]
    fn quote_budget_rounds_toward_zero_to_8dp() {
        let o = order(
            "o-1",
            Side::Buy,
            Size::Quote(Notional::new(dec!(1239.7264230096))),
            OrderKind::Market,
        );
        let p = order_params(&o, "tbrun-o-1".into());
        assert!(p.contains(&(
            "quoteOrderQty",
            "1239.72642300".trim_end_matches('0').to_owned()
        )));
    }
}
