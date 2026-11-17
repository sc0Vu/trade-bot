//! Orders on eToro: [`EtoroBroker`] and its startup / exit [`reconcile`].
//!
//! eToro holds *positions*, not a balance of units: a buy opens one, and a
//! sell closes positions (eToro's guide: "you must close the specific line
//! item"). Every write is accepted first and executed later, so every
//! order is polled to its outcome. Shapes checked on the demo account
//! 2026-11-18.
//!
//! - **Buy** (market only; the real asset, `settlementType real`, leverage
//!   1): `POST /api/v2/trading/execution[/demo]/orders` by `amount` (a
//!   quote budget; eToro charges its fee on top, so a budget can be
//!   overspent by up to the fee, ~1% on crypto) or by `units`. Polled by
//!   `orderId` (`GET /api/v2/trading/info[/demo]/orders:lookup`; lookup by
//!   request id answers 404 although documented): `Filled` → one fill per
//!   `positionExecutions[]` (units, `avgPrice`, `fees` in USD).
//! - **Sell**: closes the instrument's real positions oldest first (the
//!   account's: the bot needs its symbols to itself) through
//!   `POST /api/v1/trading/execution[/demo]/market-close-orders/positions/
//!   {id}` (`InstrumentId`, demo `InstrumentID`; `UnitsToDeduct` only for a
//!   partial close, which eToro refuses below ~$10 deducted). Polled at
//!   `GET /api/v1/trading/info[/demo]/close-orders/{orderId}`: each closed
//!   `positions[]` entry is a fill at `rate`; the fee is `units × rate −
//!   proceeds` (proceeds are cent-rounded, so the fee is exact to the cent).
//! - A late refusal (status `Rejected`, e.g. eToro's minimum) drops the
//!   order unfilled, logged with eToro's message. Stops and limits are
//!   refused (not built).
//! - **Books**: as Binance's: every venue order journaled `pending` before
//!   it is sent (`<tag>-o-N`, a close `<tag>-o-N.c<k>`), `open` with its
//!   eToro order id, fills recorded once by `open-<order>-<position>` /
//!   `close-<order>-<position>`.

use super::client::{Account, EtoroClient, Pool, Reply, ok};
use super::market::parse_iso;
use crate::max::public::raw_decimal;
use crate::{Books, Broker, ExchangeError};
use async_trait::async_trait;
use bot_core::{
    Decimal, Fee, Fill, FillSpec, Instrument, InstrumentId, Liquidity, MarketEvent, Notional,
    Order, OrderId, OrderKind, Position, Price, Qty, Side, Size, Timestamp,
};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::sync::Arc;
use storage::JournalOrder;

/// What the broker needs from eToro; [`EtoroClient`] in a run, canned
/// replies in tests.
#[async_trait]
pub trait EtoroApi: Send + Sync {
    fn account(&self) -> Account;
    async fn get(&self, pool: Pool, path: &str) -> Result<String, ExchangeError>;
    async fn post(
        &self,
        pool: Pool,
        path: &str,
        body: serde_json::Value,
    ) -> Result<Reply, ExchangeError>;
    async fn delete(&self, pool: Pool, path: &str) -> Result<Reply, ExchangeError>;
}

#[async_trait]
impl EtoroApi for EtoroClient {
    fn account(&self) -> Account {
        EtoroClient::account(self)
    }
    async fn get(&self, pool: Pool, path: &str) -> Result<String, ExchangeError> {
        EtoroClient::get(self, pool, path, &[]).await
    }
    async fn post(
        &self,
        pool: Pool,
        path: &str,
        body: serde_json::Value,
    ) -> Result<Reply, ExchangeError> {
        EtoroClient::post(self, pool, path, &body, &uuid::Uuid::new_v4().to_string()).await
    }
    async fn delete(&self, pool: Pool, path: &str) -> Result<Reply, ExchangeError> {
        EtoroClient::delete(self, pool, path).await
    }
}

fn decode(what: impl Into<String>) -> ExchangeError {
    ExchangeError::Decode(what.into())
}

/// An order's eToro status id, eToro's message, and its fills keyed by
/// the venue id they are recorded under.
type Outcome = (i64, String, Vec<(String, Fill)>);

/// eToro order status ids that end an order.
fn terminal(status: i64) -> bool {
    matches!(status, 3 | 4 | 7 | 8 | 9 | 10)
}

/// One of the account's real-asset positions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EtoroPosition {
    pub position_id: i64,
    pub etoro_id: i64,
    pub units: Decimal,
    pub open_time: i64,
}

/// The account's real-asset positions (`GET .../info[/demo]/pnl`), oldest
/// first.
pub(crate) fn parse_positions(body: &str) -> Result<Vec<EtoroPosition>, ExchangeError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Reply<'a> {
        #[serde(borrow)]
        client_portfolio: Portfolio<'a>,
    }
    #[derive(Deserialize)]
    struct Portfolio<'a> {
        #[serde(borrow)]
        positions: Vec<Pos<'a>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Pos<'a> {
        #[serde(rename = "positionID")]
        position_id: i64,
        #[serde(rename = "instrumentID")]
        instrument_id: i64,
        #[serde(borrow)]
        units: &'a RawValue,
        open_date_time: String,
        #[serde(rename = "settlementTypeID", default)]
        settlement_type_id: i64,
        #[serde(default = "one")]
        leverage: i64,
    }
    fn one() -> i64 {
        1
    }
    let r: Reply = serde_json::from_str(body).map_err(|e| decode(format!("pnl: {e}")))?;
    let mut out = Vec::new();
    for p in r.client_portfolio.positions {
        // 1 = real asset; CFDs and leverage are someone else's.
        if p.settlement_type_id != 1 || p.leverage != 1 {
            continue;
        }
        out.push(EtoroPosition {
            position_id: p.position_id,
            etoro_id: p.instrument_id,
            units: raw_decimal(p.units, "units")?,
            open_time: parse_iso(&p.open_date_time)?,
        });
    }
    out.sort_by_key(|p| (p.open_time, p.position_id));
    Ok(out)
}

/// An open order's outcome: status, and its executions as fills.
pub(crate) fn parse_open(
    body: &str,
    order: &OrderId,
    inst: &InstrumentId,
) -> Result<Outcome, ExchangeError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Reply<'a> {
        order_id: i64,
        status: Status,
        #[serde(borrow, default)]
        position_executions: Vec<Exec<'a>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Status {
        id: i64,
        #[serde(default)]
        error_message: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Exec<'a> {
        position_id: i64,
        #[serde(borrow)]
        opening_data: Opening<'a>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Opening<'a> {
        execution_time: String,
        #[serde(borrow)]
        units: &'a RawValue,
        #[serde(borrow)]
        avg_price: &'a RawValue,
        #[serde(borrow)]
        fees: &'a RawValue,
    }
    let r: Reply = serde_json::from_str(body).map_err(|e| decode(format!("lookup: {e}")))?;
    let mut fills = Vec::new();
    for x in r.position_executions {
        let fill = Fill::new(FillSpec {
            order_id: order.clone(),
            instrument: inst.clone(),
            side: Side::Buy,
            qty: Qty::new(raw_decimal(x.opening_data.units, "units")?),
            price: Price::new(raw_decimal(x.opening_data.avg_price, "avgPrice")?),
            fee: Fee::Quote(Notional::new(raw_decimal(x.opening_data.fees, "fees")?)),
            liquidity: Liquidity::Taker,
            ts: Timestamp::from_millis(parse_iso(&x.opening_data.execution_time)?),
        })
        .map_err(|e| decode(e.to_string()))?;
        fills.push((format!("open-{}-{}", r.order_id, x.position_id), fill));
    }
    Ok((
        r.status.id,
        r.status.error_message.unwrap_or_default(),
        fills,
    ))
}

/// A close order's outcome: status, message, and closed positions as
/// fills (fee = units × rate − proceeds; module docs).
pub(crate) fn parse_close(
    body: &str,
    order: &OrderId,
    inst: &Instrument,
) -> Result<Outcome, ExchangeError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Reply<'a> {
        #[serde(rename = "orderID")]
        order_id: i64,
        #[serde(rename = "statusID")]
        status_id: i64,
        #[serde(default)]
        error_message: Option<String>,
        #[serde(borrow)]
        proceeds: &'a RawValue,
        #[serde(borrow, default)]
        positions: Vec<Closed<'a>>,
    }
    #[derive(Deserialize)]
    struct Closed<'a> {
        #[serde(rename = "positionID")]
        position_id: i64,
        #[serde(default)]
        occurred: Option<String>,
        #[serde(borrow, default)]
        rate: Option<&'a RawValue>,
        #[serde(borrow, default)]
        units: Option<&'a RawValue>,
    }
    let r: Reply = serde_json::from_str(body).map_err(|e| decode(format!("close order: {e}")))?;
    // Entries without a rate are placeholders (seen on demo): skipped.
    let closed: Vec<(i64, String, Decimal, Decimal)> = r
        .positions
        .iter()
        .filter_map(|p| Some((p.position_id, p.occurred.clone()?, p.rate?, p.units?)))
        .map(|(id, t, rate, units)| {
            Ok((
                id,
                t,
                raw_decimal(rate, "rate")?,
                raw_decimal(units, "units")?,
            ))
        })
        .collect::<Result<_, ExchangeError>>()?;
    let mut fills = Vec::new();
    if r.status_id == 3 || r.status_id == 5 || r.status_id == 9 || r.status_id == 10 {
        let gross: Decimal = closed.iter().map(|(_, _, rate, units)| rate * units).sum();
        let fee_total = (gross - raw_decimal(r.proceeds, "proceeds")?).max(Decimal::ZERO);
        let mut fee_left = fee_total;
        for (k, (pid, t, rate, units)) in closed.iter().enumerate() {
            // The fee is per order: split by gross share, the last takes
            // the remainder so the shares sum exactly.
            let fee = if k + 1 == closed.len() {
                fee_left
            } else if gross.is_zero() {
                Decimal::ZERO
            } else {
                fee_total * rate * units / gross
            };
            fee_left -= fee;
            let fill = Fill::new(FillSpec {
                order_id: order.clone(),
                instrument: inst.id.clone(),
                side: Side::Sell,
                qty: Qty::new(*units),
                price: Price::new(*rate),
                fee: Fee::Quote(Notional::new(fee)),
                liquidity: Liquidity::Taker,
                ts: Timestamp::from_millis(parse_iso(t)?),
            })
            .map_err(|e| decode(e.to_string()))?;
            fills.push((format!("close-{}-{pid}", r.order_id), fill));
        }
    }
    Ok((r.status_id, r.error_message.unwrap_or_default(), fills))
}

#[derive(Clone, Debug)]
struct VenueOrder {
    client_id: String,
    order_id: i64,
    close: bool,
}

struct Live {
    order: Order,
    legs: Vec<VenueOrder>,
}

/// See the module docs.
pub struct EtoroBroker {
    api: Box<dyn EtoroApi>,
    instruments: HashMap<InstrumentId, (Instrument, i64)>,
    positions: HashMap<InstrumentId, Position>,
    open: Vec<Live>,
    run_tag: String,
    books: Arc<dyn Books>,
    recovered: Vec<Fill>,
}

impl EtoroBroker {
    /// Starts from the books' holdings; `instruments` pairs each with its
    /// eToro id.
    pub async fn open(
        api: Box<dyn EtoroApi>,
        instruments: Vec<(Instrument, i64)>,
        run_tag: String,
        books: Arc<dyn Books>,
    ) -> Result<Self, ExchangeError> {
        let mut positions = HashMap::new();
        for (i, _) in &instruments {
            if let Some(p) = books.get_holding(&i.id).await? {
                p.check_against(i)
                    .map_err(|e| decode(format!("stored holding: {e}")))?;
                positions.insert(i.id.clone(), p);
            }
        }
        Ok(Self {
            api,
            instruments: instruments
                .into_iter()
                .map(|(i, id)| (i.id.clone(), (i, id)))
                .collect(),
            positions,
            open: Vec::new(),
            run_tag,
            books,
            recovered: Vec::new(),
        })
    }

    pub fn with_recovered(mut self, fills: Vec<Fill>) -> Self {
        self.recovered = fills;
        self
    }

    fn paths(&self) -> Paths {
        Paths(self.api.account())
    }

    /// Sends one venue order journaled as `client_id`.
    async fn send(
        &mut self,
        client_id: &str,
        order: &Order,
        path: &str,
        body: serde_json::Value,
        close: bool,
    ) -> Result<VenueOrder, ExchangeError> {
        self.books
            .insert_pending(client_id, &order.instrument, order.side, order.created_at)
            .await?;
        let reply = self.api.post(Pool::Trading, path, body).await?;
        let text = match ok(path, reply) {
            Ok(t) => t,
            Err(e) => {
                // An HTTP 4xx is a refusal: nothing was placed.
                self.books.mark_closed(client_id).await?;
                return Err(ExchangeError::Venue {
                    code: 400,
                    msg: e.to_string(),
                });
            }
        };
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| decode(format!("order ack: {e}")))?;
        let order_id = if close {
            v["orderForClose"]["orderID"].as_i64()
        } else {
            v["orderId"].as_i64()
        }
        .ok_or_else(|| decode("order ack without an order id"))?;
        self.books.mark_open(client_id, order_id).await?;
        Ok(VenueOrder {
            client_id: client_id.to_owned(),
            order_id,
            close,
        })
    }

    /// Reads a leg; `Some(fills)` once it ended.
    async fn poll_leg(
        &self,
        order: &Order,
        leg: &VenueOrder,
    ) -> Result<Option<Vec<Fill>>, ExchangeError> {
        let (inst, _) = &self.instruments[&order.instrument];
        let paths = self.paths();
        let (status, msg, fills) = if leg.close {
            let body = self
                .api
                .get(Pool::Info, &paths.close_order(leg.order_id))
                .await?;
            parse_close(&body, &order.id, inst)?
        } else {
            let body = self
                .api
                .get(Pool::Info, &paths.lookup(leg.order_id))
                .await?;
            parse_open(&body, &order.id, &inst.id)?
        };
        if !terminal(status) {
            return Ok(None);
        }
        if fills.is_empty() {
            tracing::warn!(order = %leg.client_id, status, reason = %msg, "eToro order ended unfilled");
        }
        let mut out = Vec::new();
        for (venue_id, fill) in fills {
            if self
                .books
                .record_venue_fill(&fill, inst, &venue_id, &leg.client_id)
                .await?
                .is_some()
            {
                out.push(fill);
            }
        }
        self.books.mark_closed(&leg.client_id).await?;
        Ok(Some(out))
    }
}

/// Account-specific paths.
struct Paths(Account);

impl Paths {
    fn open(&self) -> String {
        self.0.path(
            "/api/v2/trading/execution/orders",
            "/api/v2/trading/execution/demo/orders",
        )
    }
    fn lookup(&self, id: i64) -> String {
        self.0.path(
            &format!("/api/v2/trading/info/orders:lookup?orderId={id}"),
            &format!("/api/v2/trading/info/demo/orders:lookup?orderId={id}"),
        )
    }
    fn close(&self, position: i64) -> String {
        self.0.path(
            &format!("/api/v1/trading/execution/market-close-orders/positions/{position}"),
            &format!("/api/v1/trading/execution/demo/market-close-orders/positions/{position}"),
        )
    }
    fn close_order(&self, id: i64) -> String {
        self.0.path(
            &format!("/api/v1/trading/info/real/close-orders/{id}"),
            &format!("/api/v1/trading/info/demo/close-orders/{id}"),
        )
    }
    fn pnl(&self) -> String {
        self.0.path(
            "/api/v1/trading/info/real/pnl",
            "/api/v1/trading/info/demo/pnl",
        )
    }
    fn instrument_key(&self) -> &'static str {
        // The close body's field casing differs between the two (docs and
        // demo agree on it).
        match self.0 {
            Account::Real => "InstrumentId",
            Account::Demo => "InstrumentID",
        }
    }
}

fn number(d: Decimal) -> Result<serde_json::Value, ExchangeError> {
    // eToro takes JSON numbers; written from the decimal's text, so no
    // float rounding happens on our side.
    serde_json::from_str(&d.normalize().to_string()).map_err(|e| decode(e.to_string()))
}

#[async_trait]
impl Broker for EtoroBroker {
    async fn submit(&mut self, order: Order) -> Result<OrderId, ExchangeError> {
        let Some((_, etoro_id)) = self.instruments.get(&order.instrument).cloned() else {
            return Err(ExchangeError::UnknownInstrument(order.instrument.clone()));
        };
        if self.open.iter().any(|o| o.order.id == order.id) {
            return Err(ExchangeError::DuplicateOrder(order.id.clone()));
        }
        if order.kind != OrderKind::Market {
            return Err(ExchangeError::Rejected("only market orders on eToro"));
        }
        crate::check_reduce_only(self.position(&order.instrument).qty().get(), &order)?;
        let base_id = self.client_order_id(&order.id);
        let paths = self.paths();
        let legs = match order.side {
            Side::Buy => {
                let mut body = serde_json::json!({
                    "action": "open", "transaction": "buy", "instrumentId": etoro_id,
                    "settlementType": "real", "orderType": "mkt", "leverage": 1,
                    "orderCurrency": "usd",
                });
                match order.size {
                    Size::Quote(n) => body["amount"] = number(n.get())?,
                    Size::Base(q) => body["units"] = number(q.get())?,
                }
                vec![
                    self.send(&base_id, &order, &paths.open(), body, false)
                        .await?,
                ]
            }
            Side::Sell => {
                let Size::Base(qty) = order.size else {
                    return Err(ExchangeError::Rejected("a sell is sized in units"));
                };
                let held = self.position(&order.instrument).qty().get();
                if qty.get() > held {
                    return Err(ExchangeError::Rejected("sell exceeds holding"));
                }
                let body = self.api.get(Pool::Info, &paths.pnl()).await?;
                let mut left = qty.get();
                let mut legs = Vec::new();
                for (k, p) in parse_positions(&body)?
                    .into_iter()
                    .filter(|p| p.etoro_id == etoro_id)
                    .enumerate()
                {
                    if left <= Decimal::ZERO {
                        break;
                    }
                    let mut body = serde_json::json!({ paths.instrument_key(): etoro_id });
                    // Selling all we hold closes whole positions (no unit
                    // rounding against eToro's precision).
                    let selling_all = qty.get() == held;
                    if !selling_all && left < p.units {
                        body["UnitsToDeduct"] = number(left)?;
                    }
                    let cid = format!("{base_id}.c{k}");
                    legs.push(
                        self.send(&cid, &order, &paths.close(p.position_id), body, true)
                            .await?,
                    );
                    left -= p.units.min(left);
                }
                if legs.is_empty() {
                    return Err(ExchangeError::Rejected("no eToro position to close"));
                }
                legs
            }
        };
        let id = order.id.clone();
        self.open.push(Live { order, legs });
        Ok(id)
    }

    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        // Market orders fill or end by themselves; one waiting for the
        // market to open could be cancelled, which isn't built: refuse.
        if self.open.iter().any(|o| o.order.id == id) {
            return Err(ExchangeError::Rejected(
                "eToro market orders can't be cancelled here",
            ));
        }
        Err(ExchangeError::UnknownOrder(id))
    }

    async fn settle(&mut self, _event: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        let mut fills = Vec::new();
        let mut i = 0;
        while i < self.open.len() {
            let order = self.open[i].order.clone();
            let mut k = 0;
            while k < self.open[i].legs.len() {
                let leg = self.open[i].legs[k].clone();
                match self.poll_leg(&order, &leg).await? {
                    Some(new) => {
                        fills.extend(new);
                        self.open[i].legs.remove(k);
                    }
                    None => k += 1,
                }
            }
            if self.open[i].legs.is_empty() {
                self.open.remove(i);
            } else {
                i += 1;
            }
        }
        if !fills.is_empty() {
            for f in &fills {
                if let Some(p) = self.books.get_holding(&f.instrument).await? {
                    self.positions.insert(f.instrument.clone(), p);
                }
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
        self.open.iter().map(|o| o.order.clone()).collect()
    }

    fn client_order_id(&self, id: &OrderId) -> String {
        if id.as_str().starts_with(crate::binance::CLIENT_ID_PREFIX) {
            id.to_string()
        } else {
            format!("{}-{}", self.run_tag, id)
        }
    }

    fn take_recovered_fills(&mut self) -> Vec<Fill> {
        std::mem::take(&mut self.recovered)
    }
}

/// Settles every unfinished journal row on `instruments`: a row with no
/// eToro id can't be looked up (eToro's lookup by request id answers 404)
/// and is closed with a warning (if it did execute, its position is the
/// account's, outside the books); one with an id is polled until it ends
/// and its fills recorded. Then refuses to start if the books hold more of
/// an instrument than the account's real positions do.
pub async fn reconcile(
    api: &dyn EtoroApi,
    instruments: &[(Instrument, i64)],
    books: &dyn Books,
    retry: std::time::Duration,
) -> Result<Vec<Fill>, ExchangeError> {
    let paths = Paths(api.account());
    let by_id: HashMap<&InstrumentId, &Instrument> =
        instruments.iter().map(|(i, _)| (&i.id, i)).collect();
    let mut recovered = Vec::new();
    for row in books.unfinished_orders().await? {
        let Some(inst) = by_id.get(&row.instrument) else {
            continue;
        };
        let JournalOrder {
            client_order_id,
            venue_order_id,
            ..
        } = &row;
        let Some(order_id) = venue_order_id else {
            tracing::warn!(order = %client_order_id, "eToro order without an id (reply lost): can't be looked up; closed");
            books.mark_closed(client_order_id).await?;
            continue;
        };
        let close = client_order_id.contains(".c");
        let oid = OrderId::new(client_order_id.clone()).map_err(|e| decode(e.to_string()))?;
        let mut done = false;
        for _ in 0..20 {
            let (status, _, fills) = if close {
                let body = api.get(Pool::Info, &paths.close_order(*order_id)).await?;
                parse_close(&body, &oid, inst)?
            } else {
                let body = api.get(Pool::Info, &paths.lookup(*order_id)).await?;
                parse_open(&body, &oid, &inst.id)?
            };
            if terminal(status) {
                for (venue_id, fill) in fills {
                    if books
                        .record_venue_fill(&fill, inst, &venue_id, client_order_id)
                        .await?
                        .is_some()
                    {
                        recovered.push(fill);
                    }
                }
                books.mark_closed(client_order_id).await?;
                done = true;
                break;
            }
            tokio::time::sleep(retry).await;
        }
        if !done {
            return Err(decode(format!(
                "eToro order {client_order_id} still in flight (waiting for the market?); retry when it ends"
            )));
        }
    }
    let body = api.get(Pool::Info, &paths.pnl()).await?;
    let positions = parse_positions(&body)?;
    for (inst, etoro_id) in instruments {
        let held = books
            .get_holding(&inst.id)
            .await?
            .map_or(Decimal::ZERO, |p| p.qty().get());
        let venue: Decimal = positions
            .iter()
            .filter(|p| p.etoro_id == *etoro_id)
            .map(|p| p.units)
            .sum();
        if held > venue {
            return Err(decode(format!(
                "{}: the books hold {held} but the eToro account only {venue}: \
                 positions were closed outside the bot",
                inst.id
            )));
        }
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_db::TempDb;
    use bot_core::{AssetClass, InstrumentSpec, OrderSpec};
    use rust_decimal_macros::dec;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Replies from the demo account, 2026-11-18 (trimmed).
    const OPEN_ACK: &str = r#"{"token":"2510f263","orderId":394898072,"referenceId":"81516bcb"}"#;
    const LOOKUP_FILLED: &str = r#"{"accountId":17799278,"orderId":394898072,"action":"open","transaction":"buy","type":"mkt","status":{"id":3,"name":"Filled","errorCode":0},"asset":{"symbol":"BTC","instrumentId":100000,"settlementType":"REAL","leverage":1,"side":"long"},"requestedAmount":19.98,"positionExecutions":[{"positionId":3644945509,"state":"open","remainingUnits":0.000314,"openingData":{"openTime":"2026-11-17T18:18:57.227Z","orderId":394898072,"executionTime":"2026-11-17T18:18:57.373Z","units":0.000314,"avgPrice":63638.48,"fees":0.2,"taxes":0.0}}]}"#;
    const PNL: &str = r#"{"clientPortfolio":{"positions":[{"positionID":3644945509,"openDateTime":"2026-11-17T18:18:57.373Z","openRate":63638.48,"instrumentID":100000,"isBuy":true,"amount":19.98,"leverage":1,"units":0.000314,"settlementTypeID":1},{"positionID":7,"openDateTime":"2026-11-10T00:00:00Z","instrumentID":100000,"isBuy":true,"leverage":2,"units":1,"settlementTypeID":0}],"credit":99979.82}}"#;
    const CLOSE_ACK: &str = r#"{"orderForClose":{"positionID":3644945509,"instrumentID":100000,"orderID":394881160,"orderType":19,"statusID":1},"token":"b9c8"}"#;
    const CLOSE_FILLED: &str = r#"{"token":"b9c8","orderID":394881160,"statusID":3,"errorCode":0,"instrumentID":100000,"proceeds":19.79,"positions":[{"positionID":3644945509,"occurred":"2026-11-17T18:19:26.93Z","rate":63658.09,"units":0.000314,"conversionRate":1.0,"amount":19.98},{"positionID":3644945509}]}"#;
    const CLOSE_REJECTED: &str = r#"{"orderID":394881158,"statusID":4,"errorCode":776,"errorMessage":"Calculated deducted equity: 7.25 USD, is under the allowed MinPoisitionAmount","proceeds":0.0,"positions":[]}"#;

    #[derive(Default)]
    struct Fake {
        replies: Mutex<VecDeque<(u16, String)>>,
        sent: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl Fake {
        fn reply(&self, status: u16, body: &str) {
            self.replies
                .lock()
                .unwrap()
                .push_back((status, body.to_owned()));
        }
        fn next(&self, what: &str, path: &str) -> Reply {
            self.sent
                .lock()
                .unwrap()
                .push((path.to_owned(), what.to_owned()));
            let (status, body) = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("canned reply");
            Reply { status, body }
        }
    }

    #[async_trait]
    impl EtoroApi for Fake {
        fn account(&self) -> Account {
            Account::Demo
        }
        async fn get(&self, _: Pool, path: &str) -> Result<String, ExchangeError> {
            ok(path, self.next("", path))
        }
        async fn post(
            &self,
            _: Pool,
            path: &str,
            body: serde_json::Value,
        ) -> Result<Reply, ExchangeError> {
            Ok(self.next(&body.to_string(), path))
        }
        async fn delete(&self, _: Pool, path: &str) -> Result<Reply, ExchangeError> {
            Ok(self.next("", path))
        }
    }

    fn btc() -> Instrument {
        Instrument::new(InstrumentSpec {
            id: "etoro:BTC".parse().unwrap(),
            class: AssetClass::Spot,
            base: bot_core::Asset::new("BTC").unwrap(),
            quote: bot_core::Asset::new("USD").unwrap(),
            tick_size: Price::new(dec!(0.00000001)),
            lot_size: Qty::new(dec!(0.00000001)),
            min_qty: Qty::new(dec!(0.00000001)),
            min_notional: Notional::new(dec!(10)),
            price_band: None,
            max_open_orders: None,
            max_open_stops: None,
        })
        .unwrap()
    }

    fn order(id: &str, side: Side, size: Size) -> Order {
        Order::new(OrderSpec {
            id: OrderId::new(id).unwrap(),
            instrument: btc().id.clone(),
            side,
            size,
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    fn tick() -> MarketEvent {
        MarketEvent::SessionOpen {
            venue: "etoro".parse().unwrap(),
            ts: Timestamp::from_millis(0),
        }
    }

    /// The demo round trip of 2026-11-18, replayed: $20 buys 0.000314 @
    /// 63,638.48, fee 0.2 → holding 0.000314, basis 19.98248272 + 0.2.
    /// Selling all closes the position whole (no UnitsToDeduct, demo
    /// casing `InstrumentID`): 0.000314 @ 63,658.09 = 19.98864026,
    /// proceeds 19.79 → fee 0.19864026.
    /// Realized = 19.79 − 20.18248272 = −0.39248272; flat.
    #[tokio::test]
    async fn the_demo_round_trip_lands_in_the_books() {
        let db = TempDb::new().await;
        let fake = Fake::default();
        let sent = fake.sent.clone();
        fake.reply(200, OPEN_ACK);
        fake.reply(200, LOOKUP_FILLED);
        fake.reply(200, PNL);
        fake.reply(200, CLOSE_ACK);
        fake.reply(200, CLOSE_FILLED);
        let mut b = EtoroBroker::open(
            Box::new(fake),
            vec![(btc(), 100000)],
            "tbrun".into(),
            db.store.clone(),
        )
        .await
        .unwrap();
        b.submit(order(
            "o-1",
            Side::Buy,
            Size::Quote(Notional::new(dec!(20))),
        ))
        .await
        .unwrap();
        let (path, body) = sent.lock().unwrap()[0].clone();
        assert_eq!(path, "/api/v2/trading/execution/demo/orders");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["amount"].to_string(), "20");
        assert_eq!(
            (body["settlementType"].as_str(), body["leverage"].as_i64()),
            (Some("real"), Some(1))
        );
        let f = b.settle(&tick()).await.unwrap();
        assert_eq!(
            (f[0].qty.get(), f[0].price.get()),
            (dec!(0.000314), dec!(63638.48))
        );
        assert_eq!(f[0].fee, Fee::Quote(Notional::new(dec!(0.2))));
        let p = b.position(&btc().id);
        assert_eq!(
            (p.qty().get(), p.cost_basis().get()),
            (dec!(0.000314), dec!(20.18248272))
        );
        b.submit(order(
            "o-2",
            Side::Sell,
            Size::Base(Qty::new(dec!(0.000314))),
        ))
        .await
        .unwrap();
        let (path, body) = sent.lock().unwrap()[3].clone();
        assert_eq!(
            path,
            "/api/v1/trading/execution/demo/market-close-orders/positions/3644945509"
        );
        assert_eq!(body, r#"{"InstrumentID":100000}"#);
        let f = b.settle(&tick()).await.unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].fee, Fee::Quote(Notional::new(dec!(0.19864026))));
        let p = b.position(&btc().id);
        assert!(p.is_flat());
        assert_eq!(p.realized_pnl().get(), dec!(-0.39248272));
        assert!(b.open_orders().is_empty());
    }

    /// A partial sell sends UnitsToDeduct; eToro's later refusal (minimum
    /// $10 deducted) ends the order unfilled: nothing recorded, no order
    /// left. Stops, and sells beyond the holding, are refused locally.
    #[tokio::test]
    async fn late_refusals_drop_the_order_and_local_checks_send_nothing() {
        let db = TempDb::new().await;
        let fake = Fake::default();
        let sent = fake.sent.clone();
        fake.reply(200, OPEN_ACK);
        fake.reply(200, LOOKUP_FILLED);
        fake.reply(200, PNL);
        fake.reply(
            200,
            r#"{"orderForClose":{"orderID":394881158,"statusID":1}}"#,
        );
        fake.reply(200, CLOSE_REJECTED);
        let mut b = EtoroBroker::open(
            Box::new(fake),
            vec![(btc(), 100000)],
            "tbrun".into(),
            db.store.clone(),
        )
        .await
        .unwrap();
        b.submit(order(
            "o-1",
            Side::Buy,
            Size::Quote(Notional::new(dec!(20))),
        ))
        .await
        .unwrap();
        b.settle(&tick()).await.unwrap();
        b.submit(order(
            "o-2",
            Side::Sell,
            Size::Base(Qty::new(dec!(0.000114))),
        ))
        .await
        .unwrap();
        let body = sent.lock().unwrap()[3].1.clone();
        assert_eq!(body, r#"{"InstrumentID":100000,"UnitsToDeduct":0.000114}"#);
        assert!(b.settle(&tick()).await.unwrap().is_empty());
        assert!(b.open_orders().is_empty());
        assert_eq!(b.position(&btc().id).qty().get(), dec!(0.000314));
        let n = sent.lock().unwrap().len();
        let stop = Order::new(OrderSpec {
            kind: OrderKind::StopMarket {
                stop: Price::new(dec!(1)),
            },
            ..order("o-3", Side::Sell, Size::Base(Qty::new(dec!(0.0001)))).into()
        })
        .unwrap();
        assert!(b.submit(stop).await.is_err());
        assert!(
            b.submit(order("o-4", Side::Sell, Size::Base(Qty::new(dec!(1)))))
                .await
                .is_err()
        );
        assert_eq!(sent.lock().unwrap().len(), n);
    }

    /// Real positions only, oldest first; a leveraged CFD is skipped.
    #[test]
    fn positions_are_the_real_asset_ones() {
        let p = parse_positions(PNL).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].position_id, p[0].units), (3644945509, dec!(0.000314)));
    }
}
