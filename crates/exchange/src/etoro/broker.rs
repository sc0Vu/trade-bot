//! Orders on eToro: [`EtoroBroker`] and its startup / exit [`reconcile`].
//!
//! eToro holds *positions*, not a balance of units: a buy opens one, and a
//! sell closes positions (eToro's guide: "you must close the specific line
//! item"). Every write is accepted first and executed later, so every
//! order is polled to its outcome. Shapes checked on the demo account
//! 2026-11-18.
//!
//! - **Buy** (the real asset, `settlementType real`, leverage 1): market,
//!   or a resting limit below the last close as `mit` (market-if-touched:
//!   executes at market once the price falls to `triggerRate`, so it can
//!   fill worse than the limit; waits as status 12; cancelled by `DELETE
//!   .../execution[/demo]/orders/{id}`); a limit at or above the last close
//!   is marketable and sent as a market buy (an `mit` above the market
//!   waits for the price to rise to it, like a buy-stop: demo 2026-11-18).
//!   A triggered `mit` executes under a new order id while its own lookup
//!   stays "PendingTriggeredRate"; the broker finds the execution through
//!   the position it opened (`triggered_execution`): `POST /api/v2/trading/execution[/demo]/orders` by `amount` (a
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
//!   The fee is eToro's own from the private feed ([`EtoroStream`]) when it
//!   reported that close; the derivation is the fallback.
//! - **Resting sell limit** above the last close: a take-profit (`PATCH
//!   /api/v2/trading[/demo]/positions/{id}`) on the oldest positions whose
//!   units add up exactly to the order (eToro closes a position whole);
//!   at or below the last close it is marketable and sent as a market
//!   close. Confirmed or refused through the private feed (REST answers
//!   202 either way) or seen on the position; filled when the position is
//!   gone, from the feed's close record (exact fee) or trade history (fee
//!   unknown, logged, booked 0). Cancelled by clearing it.
//! - Refused locally, as the backtest's [`TouchFillModel`] refuses them:
//!   immediate-or-cancel limits (eToro's `limitIOC` filled a buy 3% above
//!   its limit on demo) and stops.
//! - A late refusal (status `Rejected`, e.g. eToro's minimum) drops the
//!   order unfilled, logged with eToro's message.
//!
//! [`TouchFillModel`]: crate::paper::TouchFillModel
//! - **Books**: as Binance's: every venue order journaled `pending` before
//!   it is sent (`<tag>-o-N`, a close `<tag>-o-N.c<k>`), `open` with its
//!   eToro order id, fills recorded once by `open-<order>-<position>` /
//!   `close-<order>-<position>`.

use super::client::{Account, EtoroClient, Pool, Reply, ok};
use super::market::parse_iso;
use super::stream::{EtoroStream, TakeProfit};
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
    async fn patch(
        &self,
        pool: Pool,
        path: &str,
        body: serde_json::Value,
    ) -> Result<Reply, ExchangeError>;
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
    async fn patch(
        &self,
        pool: Pool,
        path: &str,
        body: serde_json::Value,
    ) -> Result<Reply, ExchangeError> {
        EtoroClient::patch(self, pool, path, &body).await
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
    /// The order that opened it: for a triggered `mit`, the execution's
    /// own id, not the `mit`'s.
    pub order_id: i64,
    pub etoro_id: i64,
    pub units: Decimal,
    pub open_time: i64,
    /// The take-profit on it, if any (`isNoTakeProfit` false).
    pub take_profit: Option<Decimal>,
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
        #[serde(rename = "orderID", default)]
        order_id: i64,
        #[serde(borrow)]
        units: &'a RawValue,
        open_date_time: String,
        #[serde(rename = "settlementTypeID", default)]
        settlement_type_id: i64,
        #[serde(default = "one")]
        leverage: i64,
        #[serde(borrow, default)]
        take_profit_rate: Option<&'a RawValue>,
        #[serde(default = "yes")]
        is_no_take_profit: bool,
    }
    fn one() -> i64 {
        1
    }
    fn yes() -> bool {
        true
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
            order_id: p.order_id,
            etoro_id: p.instrument_id,
            units: raw_decimal(p.units, "units")?,
            open_time: parse_iso(&p.open_date_time)?,
            take_profit: match (p.is_no_take_profit, p.take_profit_rate) {
                (false, Some(r)) => Some(raw_decimal(r, "takeProfitRate")?),
                _ => None,
            },
        });
    }
    out.sort_by_key(|p| (p.open_time, p.position_id));
    Ok(out)
}

/// The ids of the account's orders still waiting (`clientPortfolio.orders`:
/// a `mit` not yet triggered is listed here, and leaves the list when it
/// triggers or is cancelled).
pub(crate) fn parse_pending(body: &str) -> Result<Vec<i64>, ExchangeError> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| decode(format!("pnl: {e}")))?;
    Ok(v["clientPortfolio"]["orders"]
        .as_array()
        .map(|a| a.iter().filter_map(|o| o["orderID"].as_i64()).collect())
        .unwrap_or_default())
}

/// The account's positions and waiting orders, read once per settle.
pub(crate) struct Portfolio {
    positions: Vec<EtoroPosition>,
    pending: Vec<i64>,
}

impl Portfolio {
    fn parse(body: &str) -> Result<Self, ExchangeError> {
        Ok(Self {
            positions: parse_positions(body)?,
            pending: parse_pending(body)?,
        })
    }
}

/// A triggered `mit`'s execution. eToro runs a triggered `mit` as a new
/// order with its own id; a lookup of the `mit`'s id keeps answering
/// "PendingTriggeredRate" (demo 2026-11-18), while a lookup of the
/// execution's id answers with the `mit`'s id as `orderId`, Filled, and the
/// position. So: for each position on `etoro_id` opened by an order not in
/// `seen`, look that order up; the one naming `mit_id` is it. Returns its
/// lookup body; every id looked up goes into `seen`.
async fn triggered_execution(
    api: &dyn EtoroApi,
    paths: &Paths,
    mit_id: i64,
    etoro_id: i64,
    positions: &[EtoroPosition],
    seen: &mut std::collections::HashSet<i64>,
) -> Result<Option<String>, ExchangeError> {
    for p in positions.iter().filter(|p| p.etoro_id == etoro_id) {
        if p.order_id == mit_id || !seen.insert(p.order_id) {
            continue;
        }
        let body = api.get(Pool::Info, &paths.lookup(p.order_id)).await?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| decode(format!("lookup: {e}")))?;
        if v["orderId"].as_i64() == Some(mit_id) {
            return Ok(Some(body));
        }
    }
    Ok(None)
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

/// One venue-side piece of an order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LegKind {
    /// An opening order (market, or `mit` for a resting buy limit).
    Open,
    /// A market close of one position.
    Close,
    /// A take-profit set on one position (a resting sell limit).
    TakeProfit { position_id: i64, rate: Decimal },
}

#[derive(Clone, Debug)]
struct VenueOrder {
    client_id: String,
    /// eToro's order id (Open, Close); for a take-profit, the position id.
    order_id: i64,
    kind: LegKind,
    /// A take-profit eToro confirmed (stream, or seen on the position).
    confirmed: bool,
    /// Settles a triggered `mit` went without its execution found.
    misses: u8,
}

struct Live {
    order: Order,
    legs: Vec<VenueOrder>,
    cancelling: bool,
}

/// See the module docs.
pub struct EtoroBroker {
    api: Box<dyn EtoroApi>,
    stream: Option<EtoroStream>,
    instruments: HashMap<InstrumentId, (Instrument, i64)>,
    positions: HashMap<InstrumentId, Position>,
    open: Vec<Live>,
    run_tag: String,
    books: Arc<dyn Books>,
    recovered: Vec<Fill>,
    last_close: HashMap<InstrumentId, Price>,
    /// Orders already looked up while searching for triggered `mit`s.
    seen_orders: std::collections::HashSet<i64>,
    /// Compare the books with the account each close time (`with_account_check`).
    account_check: bool,
    /// Close time last compared.
    checked_at: Option<bot_core::Timestamp>,
}

/// Settles a triggered `mit` may go unmatched before it counts as ended
/// unfilled (the position can show a moment after the order leaves the
/// waiting list).
const MIT_PATIENCE: u8 = 5;

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
            stream: None,
            instruments: instruments
                .into_iter()
                .map(|(i, id)| (i.id.clone(), (i, id)))
                .collect(),
            positions,
            open: Vec::new(),
            run_tag,
            books,
            recovered: Vec::new(),
            last_close: HashMap::new(),
            seen_orders: std::collections::HashSet::new(),
            account_check: false,
            checked_at: None,
        })
    }

    /// Once per close time, while nothing of ours is working, compares
    /// each instrument's books with the account's units and stops the run
    /// (an error) when they differ: a stock split or other corporate action
    /// (eToro adjusts the units, the books can't), or positions opened or
    /// closed outside the bot. Trading on would sell the wrong amount. One
    /// portfolio read per close time.
    pub fn with_account_check(mut self) -> Self {
        self.account_check = true;
        self
    }

    pub fn with_recovered(mut self, fills: Vec<Fill>) -> Self {
        self.recovered = fills;
        self
    }

    /// Reads exact close fees and take-profit outcomes from `stream`.
    pub fn with_stream(mut self, stream: EtoroStream) -> Self {
        self.stream = Some(stream);
        self
    }

    fn paths(&self) -> Paths {
        Paths(self.api.account())
    }

    /// Journals then sends one venue order.
    async fn send(
        &mut self,
        client_id: &str,
        order: &Order,
        path: &str,
        body: serde_json::Value,
        kind: LegKind,
    ) -> Result<VenueOrder, ExchangeError> {
        self.books
            .insert_pending(client_id, &order.instrument, order.side, order.created_at)
            .await?;
        let reply = match &kind {
            LegKind::TakeProfit { .. } => self.api.patch(Pool::Info, path, body).await?,
            _ => self.api.post(Pool::Trading, path, body).await?,
        };
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
        let order_id = match &kind {
            LegKind::Close => v["orderForClose"]["orderID"].as_i64(),
            LegKind::Open => v["orderId"].as_i64(),
            LegKind::TakeProfit { position_id, .. } => Some(*position_id),
        }
        .ok_or_else(|| decode("order ack without an order id"))?;
        self.books.mark_open(client_id, order_id).await?;
        Ok(VenueOrder {
            client_id: client_id.to_owned(),
            order_id,
            kind,
            confirmed: false,
            misses: 0,
        })
    }

    /// The account's real positions on eToro instrument `etoro_id`, minus
    /// those a take-profit of ours holds, oldest first.
    async fn free_positions(&self, etoro_id: i64) -> Result<Vec<EtoroPosition>, ExchangeError> {
        let body = self.api.get(Pool::Info, &self.paths().pnl()).await?;
        let held: Vec<i64> = self
            .open
            .iter()
            .flat_map(|o| &o.legs)
            .filter_map(|l| match l.kind {
                LegKind::TakeProfit { position_id, .. } => Some(position_id),
                _ => None,
            })
            .collect();
        Ok(parse_positions(&body)?
            .into_iter()
            .filter(|p| p.etoro_id == etoro_id && !held.contains(&p.position_id))
            .collect())
    }

    /// A fill for `position_id`'s close by order `close_order` (0: not
    /// known yet, a take-profit) from the stream's record if it has one;
    /// else from trade history, its fee unknown (logged; 0 booked).
    async fn close_fill(
        &self,
        order: &Order,
        position_id: i64,
        close_order: i64,
    ) -> Result<Option<(String, Fill)>, ExchangeError> {
        let (inst, _) = &self.instruments[&order.instrument];
        let streamed = self
            .stream
            .as_ref()
            .and_then(|s| s.close_of(position_id))
            .filter(|c| close_order == 0 || c.order_id == close_order);
        let (venue_order, units, price, fee, ts) = if let Some(c) = streamed {
            (c.order_id, c.units, c.price, c.fee, c.time)
        } else {
            let Some(h) = self.history_close(position_id).await? else {
                return Ok(None); // not in history yet: next settle
            };
            tracing::warn!(
                position = position_id,
                "eToro close fee not reported (private feed down): booked as 0"
            );
            (close_order, h.1, h.0, Decimal::ZERO, h.2)
        };
        let fill = Fill::new(FillSpec {
            order_id: order.id.clone(),
            instrument: inst.id.clone(),
            side: Side::Sell,
            qty: Qty::new(units),
            price: Price::new(price),
            fee: Fee::Quote(Notional::new(fee)),
            liquidity: Liquidity::Taker,
            ts,
        })
        .map_err(|e| decode(e.to_string()))?;
        Ok(Some((format!("close-{venue_order}-{position_id}"), fill)))
    }

    /// `position_id`'s close (rate, units, time) from trade history.
    async fn history_close(
        &self,
        position_id: i64,
    ) -> Result<Option<(Decimal, Decimal, Timestamp)>, ExchangeError> {
        let since = super::market::iso(now_millis() - 86_400_000);
        let body = self
            .api
            .get(Pool::Info, &self.paths().history(&since[..10]))
            .await?;
        parse_history_close(&body, position_id)
    }

    /// Reads a leg; `Some(fills)` once it ended.
    async fn poll_leg(
        &mut self,
        idx: usize,
        k: usize,
        portfolio: &Option<Portfolio>,
    ) -> Result<Option<Vec<Fill>>, ExchangeError> {
        let positions = portfolio.as_ref().map(|p| &p.positions);
        let order = self.open[idx].order.clone();
        let leg = self.open[idx].legs[k].clone();
        let (inst, _) = self.instruments[&order.instrument].clone();
        let paths = self.paths();
        let ended: Option<Vec<(String, Fill)>> = match &leg.kind {
            LegKind::Open => {
                let body = self
                    .api
                    .get(Pool::Info, &paths.lookup(leg.order_id))
                    .await?;
                let (status, msg, fills) = parse_open(&body, &order.id, &inst.id)?;
                if terminal(status) {
                    if fills.is_empty() && status != 7 {
                        tracing::warn!(order = %leg.client_id, status, reason = %msg, "eToro order ended unfilled");
                    }
                    Some(fills)
                } else if status == 12 {
                    // Waiting for its trigger — or triggered: then it has
                    // left the waiting list and runs under another id.
                    match portfolio {
                        Some(pf) if !pf.pending.contains(&leg.order_id) => {
                            let etoro_id = self.instruments[&order.instrument].1;
                            let mut seen = std::mem::take(&mut self.seen_orders);
                            let found = triggered_execution(
                                &*self.api,
                                &paths,
                                leg.order_id,
                                etoro_id,
                                &pf.positions,
                                &mut seen,
                            )
                            .await;
                            self.seen_orders = seen;
                            match found? {
                                Some(body) => Some(parse_open(&body, &order.id, &inst.id)?.2),
                                None => {
                                    self.open[idx].legs[k].misses += 1;
                                    if self.open[idx].legs[k].misses >= MIT_PATIENCE {
                                        tracing::warn!(order = %leg.client_id, "eToro mit left the waiting list without a position; ended unfilled");
                                        Some(Vec::new())
                                    } else {
                                        None
                                    }
                                }
                            }
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            }
            LegKind::Close => {
                let body = self
                    .api
                    .get(Pool::Info, &paths.close_order(leg.order_id))
                    .await?;
                let (status, msg, mut fills) = parse_close(&body, &order.id, &inst)?;
                if terminal(status) {
                    if fills.is_empty() {
                        tracing::warn!(order = %leg.client_id, status, reason = %msg, "eToro close ended unfilled");
                    }
                    // eToro's own fee, when the private feed reported it.
                    if let Some(s) = &self.stream {
                        for (venue_id, f) in &mut fills {
                            let pid = venue_id.rsplit('-').next().and_then(|p| p.parse().ok());
                            if let Some(c) = pid
                                .and_then(|p| s.close_of(p))
                                .filter(|c| c.order_id == leg.order_id)
                            {
                                *f = Fill::new(FillSpec {
                                    fee: Fee::Quote(Notional::new(c.fee)),
                                    ..f.clone().into()
                                })
                                .map_err(|e| decode(e.to_string()))?;
                            }
                        }
                    }
                    Some(fills)
                } else {
                    None
                }
            }
            LegKind::TakeProfit { position_id, rate } => {
                let pos =
                    positions.and_then(|ps| ps.iter().find(|p| p.position_id == *position_id));
                let refused = match self
                    .stream
                    .as_ref()
                    .and_then(|s| s.take_profit(*position_id))
                {
                    Some(TakeProfit::Refused(why)) => Some(why),
                    Some(TakeProfit::Set(r)) if r == *rate => {
                        self.open[idx].legs[k].confirmed = true;
                        None
                    }
                    _ => None,
                };
                if let Some(why) = refused {
                    tracing::warn!(order = %leg.client_id, reason = %why, "eToro refused the take-profit");
                    Some(Vec::new())
                } else {
                    match pos {
                        // Still open: confirmed if it carries our rate.
                        Some(p) => {
                            if p.take_profit == Some(*rate) {
                                self.open[idx].legs[k].confirmed = true;
                            } else if self.open[idx].cancelling && p.take_profit.is_none() {
                                return self.finish_leg(idx, k, Vec::new()).await;
                            }
                            None
                        }
                        // Gone: the take-profit (or a cancel race) closed it.
                        None => self
                            .close_fill(&order, *position_id, 0)
                            .await?
                            .map(|f| vec![f]),
                    }
                }
            }
        };
        match ended {
            Some(fills) => self.finish_leg(idx, k, fills).await,
            None => Ok(None),
        }
    }

    /// Records a leg's fills once each and closes its journal row.
    async fn finish_leg(
        &mut self,
        idx: usize,
        k: usize,
        fills: Vec<(String, Fill)>,
    ) -> Result<Option<Vec<Fill>>, ExchangeError> {
        let order = &self.open[idx].order;
        let leg = &self.open[idx].legs[k];
        let (inst, _) = &self.instruments[&order.instrument];
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

/// `position_id`'s close in a trade-history reply: (rate, units, time).
pub(crate) fn parse_history_close(
    body: &str,
    position_id: i64,
) -> Result<Option<(Decimal, Decimal, Timestamp)>, ExchangeError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Row<'a> {
        position_id: i64,
        #[serde(borrow)]
        close_rate: &'a RawValue,
        #[serde(borrow)]
        units: &'a RawValue,
        close_timestamp: String,
    }
    let rows: Vec<Row> =
        serde_json::from_str(body).map_err(|e| decode(format!("trade history: {e}")))?;
    rows.iter()
        .find(|r| r.position_id == position_id)
        .map(|r| {
            Ok((
                raw_decimal(r.close_rate, "closeRate")?,
                raw_decimal(r.units, "units")?,
                Timestamp::from_millis(parse_iso(&r.close_timestamp)?),
            ))
        })
        .transpose()
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
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
    fn cancel_open(&self, id: i64) -> String {
        self.0.path(
            &format!("/api/v2/trading/execution/orders/{id}"),
            &format!("/api/v2/trading/execution/demo/orders/{id}"),
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
    fn position(&self, id: i64) -> String {
        self.0.path(
            &format!("/api/v2/trading/positions/{id}"),
            &format!("/api/v2/trading/demo/positions/{id}"),
        )
    }
    fn history(&self, min_date: &str) -> String {
        self.0.path(
            &format!("/api/v1/trading/info/trade/history?minDate={min_date}"),
            &format!("/api/v1/trading/info/trade/demo/history?minDate={min_date}"),
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

/// What eToro leaves at least in a position after a partial close (its
/// refusal reads "remaining live position amount in dollars: 0.01 < 1",
/// demo 2026-11-18).
const MIN_REMAINDER_USD: Decimal = Decimal::ONE;

/// Which positions a market sell of `qty` closes, oldest first, and how
/// many units of the last one (`None`: all of it). Checked before anything
/// is sent, against eToro's rules for a partial close:
///
/// - units are rounded down to the instrument's step (eToro's positions
///   carry 6 decimals; it publishes none);
/// - one that would leave less than $1 closes the whole position instead
///   (eToro refuses the crumb, and a crumb can never be sold on its own);
/// - one that would deduct less than the minimum position amount
///   (`min_notional`; eToro: "deducted equity … under the allowed
///   MinPositionAmount") is refused here, as a rejection the strategy
///   handles, rather than by eToro after the fact.
///
/// The value checks use the last close; with none yet, eToro decides.
pub(crate) fn close_plan<'a>(
    free: &'a [EtoroPosition],
    qty: Decimal,
    inst: &Instrument,
    last_close: Option<Price>,
) -> Result<Vec<(&'a EtoroPosition, Option<Decimal>)>, ExchangeError> {
    let total: Decimal = free.iter().map(|p| p.units).sum();
    let mut left = if qty == total {
        total
    } else {
        inst.round_qty(Qty::new(qty)).get()
    };
    let mut plan = Vec::new();
    for p in free {
        if left <= Decimal::ZERO {
            break;
        }
        if left >= p.units {
            plan.push((p, None));
            left -= p.units;
            continue;
        }
        if let Some(px) = last_close.map(|c| c.get()) {
            if (p.units - left) * px < MIN_REMAINDER_USD {
                plan.push((p, None));
                break;
            }
            if left * px < inst.min_notional.get() {
                return Err(ExchangeError::Rejected(
                    "eToro refuses a partial close under its minimum position amount",
                ));
            }
        }
        plan.push((p, Some(left)));
        break;
    }
    Ok(plan)
}

/// Oldest-first positions whose units add up to exactly `qty` (a
/// take-profit closes a position whole), if a prefix does.
fn exact_prefix(positions: &[EtoroPosition], qty: Decimal) -> Option<Vec<&EtoroPosition>> {
    let mut sum = Decimal::ZERO;
    let mut out = Vec::new();
    for p in positions {
        if sum == qty {
            break;
        }
        sum += p.units;
        out.push(p);
    }
    (sum == qty && !out.is_empty()).then_some(out)
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
        // Same refusals as the backtest's model (invariant 3).
        crate::paper::FillModel::accepts(&crate::paper::TouchFillModel, &order)
            .map_err(ExchangeError::Rejected)?;
        crate::check_reduce_only(self.position(&order.instrument).qty().get(), &order)?;
        let base_id = self.client_order_id(&order.id);
        let paths = self.paths();
        let legs = match order.side {
            Side::Buy => {
                let mut body = serde_json::json!({
                    "action": "open", "transaction": "buy", "instrumentId": etoro_id,
                    "settlementType": "real", "leverage": 1, "orderCurrency": "usd",
                });
                // A buy limit at or above the last close is marketable: a
                // market order (eToro's `mit` above the market would wait
                // for the price to *rise* to it, like a buy-stop — seen on
                // demo 2026-11-18). Below it, `mit` waits for the fall.
                let resting = match order.kind {
                    OrderKind::Limit { price, .. } => self
                        .last_close
                        .get(&order.instrument)
                        .is_none_or(|c| price < *c)
                        .then_some(price),
                    _ => None,
                };
                match resting {
                    Some(price) => {
                        body["orderType"] = "mit".into();
                        body["triggerRate"] = number(price.get())?;
                    }
                    None => body["orderType"] = "mkt".into(),
                }
                match order.size {
                    Size::Quote(n) => body["amount"] = number(n.get())?,
                    Size::Base(q) => body["units"] = number(q.get())?,
                }
                vec![
                    self.send(&base_id, &order, &paths.open(), body, LegKind::Open)
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
                let free = self.free_positions(etoro_id).await?;
                let resting = match order.kind {
                    // At or below the last close a limit sell is
                    // marketable: closed at market, as the model fills it.
                    OrderKind::Limit { price, .. } => self
                        .last_close
                        .get(&order.instrument)
                        .is_none_or(|c| price > *c)
                        .then_some(price),
                    _ => None,
                };
                let mut legs = Vec::new();
                if let Some(rate) = resting {
                    let chosen = exact_prefix(&free, qty.get()).ok_or(ExchangeError::Rejected(
                        "an eToro limit sell must close whole positions (oldest first)",
                    ))?;
                    for (k, p) in chosen.into_iter().enumerate() {
                        let cid = format!("{base_id}.t{k}");
                        let body = serde_json::json!({ "takeProfitRate": number(rate.get())? });
                        legs.push(
                            self.send(
                                &cid,
                                &order,
                                &paths.position(p.position_id),
                                body,
                                LegKind::TakeProfit {
                                    position_id: p.position_id,
                                    rate: rate.get(),
                                },
                            )
                            .await?,
                        );
                    }
                } else {
                    let total: Decimal = free.iter().map(|p| p.units).sum();
                    if qty.get() > total {
                        return Err(ExchangeError::Rejected(
                            "units held under a take-profit; cancel it first",
                        ));
                    }
                    let (inst, _) = self.instruments[&order.instrument].clone();
                    let plan = close_plan(
                        &free,
                        qty.get(),
                        &inst,
                        self.last_close.get(&order.instrument).copied(),
                    )?;
                    for (k, (p, deduct)) in plan.into_iter().enumerate() {
                        let mut body = serde_json::json!({ paths.instrument_key(): etoro_id });
                        if let Some(d) = deduct {
                            body["UnitsToDeduct"] = number(d)?;
                        }
                        let cid = format!("{base_id}.c{k}");
                        legs.push(
                            self.send(
                                &cid,
                                &order,
                                &paths.close(p.position_id),
                                body,
                                LegKind::Close,
                            )
                            .await?,
                        );
                    }
                }
                if legs.is_empty() {
                    return Err(ExchangeError::Rejected("no eToro position to close"));
                }
                legs
            }
        };
        let id = order.id.clone();
        self.open.push(Live {
            order,
            legs,
            cancelling: false,
        });
        Ok(id)
    }

    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        let Some(idx) = self
            .open
            .iter()
            .position(|o| o.order.id == id && !o.cancelling)
        else {
            return Err(ExchangeError::UnknownOrder(id));
        };
        if self.open[idx].order.kind == OrderKind::Market {
            // Market orders fill or end by themselves.
            return Err(ExchangeError::Rejected(
                "eToro market orders can't be cancelled",
            ));
        }
        let paths = self.paths();
        for leg in self.open[idx].legs.clone() {
            let reply = match leg.kind {
                LegKind::Open => {
                    self.api
                        .delete(Pool::Trading, &paths.cancel_open(leg.order_id))
                        .await?
                }
                LegKind::TakeProfit { position_id, .. } => {
                    self.api
                        .patch(
                            Pool::Info,
                            &paths.position(position_id),
                            serde_json::json!({ "clearTakeProfit": true }),
                        )
                        .await?
                }
                LegKind::Close => continue,
            };
            // Accepted only: the outcome is read at settle (a cancel can
            // lose the race to a fill, which is then reported).
            ok("cancel", reply)?;
        }
        self.open[idx].cancelling = true;
        Ok(())
    }

    async fn settle(&mut self, event: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        if let MarketEvent::Candle(c) = event {
            self.last_close.insert(c.instrument.clone(), c.close);
        }
        // One portfolio read serves every resting leg (take-profits, and
        // `mit`s that may have triggered).
        let resting = self.open.iter().any(|o| {
            o.order.kind != OrderKind::Market && o.legs.iter().any(|l| l.kind != LegKind::Close)
        });
        let portfolio = if resting {
            let body = self.api.get(Pool::Info, &self.paths().pnl()).await?;
            Some(Portfolio::parse(&body)?)
        } else {
            None
        };
        let mut fills = Vec::new();
        let mut i = 0;
        while i < self.open.len() {
            let mut k = 0;
            while k < self.open[i].legs.len() {
                match self.poll_leg(i, k, &portfolio).await? {
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
        for f in &fills {
            if let Some(p) = self.books.get_holding(&f.instrument).await? {
                self.positions.insert(f.instrument.clone(), p);
            }
        }
        if let MarketEvent::Candle(c) = event
            && self.account_check
            && self.open.is_empty()
            && fills.is_empty()
            && self.checked_at.is_none_or(|t| c.close_time > t)
        {
            self.checked_at = Some(c.close_time);
            let body = self.api.get(Pool::Info, &self.paths().pnl()).await?;
            let positions = parse_positions(&body)?;
            let instruments: Vec<(InstrumentId, i64)> = self
                .instruments
                .iter()
                .map(|(id, (_, etoro_id))| (id.clone(), *etoro_id))
                .collect();
            for (id, etoro_id) in instruments {
                let held = self.position(&id).qty().get();
                account_matches(&id, held, etoro_id, &positions)?;
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

    fn cancelling_orders(&self) -> Vec<Order> {
        self.open
            .iter()
            .filter(|o| o.cancelling)
            .map(|o| o.order.clone())
            .collect()
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

/// Settles every unfinished journal row on `instruments` (strategy state
/// doesn't own leftover orders, so resting ones are cancelled): a row with
/// no eToro id can't be looked up (lookup by request id answers 404) and
/// is closed with a warning; an opening order still waiting for its
/// trigger or the market is cancelled, then polled to its end and its fills
/// recorded; a close is polled to its end; a take-profit (`.t<k>`, id =
/// position) is cleared if its position is open, else its close recorded
/// from trade history (fee unknown). Then refuses to start if the books
/// hold more of an instrument than the account's real positions do.
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
        let oid = OrderId::new(client_order_id.clone()).map_err(|e| decode(e.to_string()))?;
        if client_order_id.contains(".t") {
            let body = api.get(Pool::Info, &paths.pnl()).await?;
            let open = parse_positions(&body)?
                .into_iter()
                .any(|p| p.position_id == *order_id);
            if open {
                ok(
                    "clear take-profit",
                    api.patch(
                        Pool::Info,
                        &paths.position(*order_id),
                        serde_json::json!({ "clearTakeProfit": true }),
                    )
                    .await?,
                )?;
            } else {
                let since = super::market::iso(now_millis() - 7 * 86_400_000);
                let body = api.get(Pool::Info, &paths.history(&since[..10])).await?;
                if let Some((rate, units, ts)) = parse_history_close(&body, *order_id)? {
                    tracing::warn!(
                        position = order_id,
                        "take-profit closed while the bot was down; fee unknown, booked as 0"
                    );
                    let fill = Fill::new(FillSpec {
                        order_id: oid.clone(),
                        instrument: inst.id.clone(),
                        side: Side::Sell,
                        qty: Qty::new(units),
                        price: Price::new(rate),
                        fee: Fee::Quote(Notional::ZERO),
                        liquidity: Liquidity::Taker,
                        ts,
                    })
                    .map_err(|e| decode(e.to_string()))?;
                    if books
                        .record_venue_fill(
                            &fill,
                            inst,
                            &format!("close-0-{order_id}"),
                            client_order_id,
                        )
                        .await?
                        .is_some()
                    {
                        recovered.push(fill);
                    }
                }
            }
            books.mark_closed(client_order_id).await?;
            continue;
        }
        let close = client_order_id.contains(".c");
        if !close {
            // A `mit` still waiting is cancelled below; one that already
            // triggered runs under another id (see `triggered_execution`).
            let body = api.get(Pool::Info, &paths.lookup(*order_id)).await?;
            if parse_open(&body, &oid, &inst.id)?.0 == 12 {
                let pnl = api.get(Pool::Info, &paths.pnl()).await?;
                if !parse_pending(&pnl)?.contains(order_id) {
                    let etoro_id = instruments
                        .iter()
                        .find(|(i, _)| i.id == inst.id)
                        .map_or(0, |(_, e)| *e);
                    let mut seen = std::collections::HashSet::new();
                    match triggered_execution(
                        api,
                        &paths,
                        *order_id,
                        etoro_id,
                        &parse_positions(&pnl)?,
                        &mut seen,
                    )
                    .await?
                    {
                        Some(exec) => {
                            for (venue_id, fill) in parse_open(&exec, &oid, &inst.id)?.2 {
                                if books
                                    .record_venue_fill(&fill, inst, &venue_id, client_order_id)
                                    .await?
                                    .is_some()
                                {
                                    recovered.push(fill);
                                }
                            }
                        }
                        None => {
                            tracing::warn!(order = %client_order_id, "eToro mit gone from the waiting list without a position found; closed")
                        }
                    }
                    books.mark_closed(client_order_id).await?;
                    continue;
                }
            }
        }
        let mut done = false;
        for attempt in 0..20 {
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
            // Waiting for a trigger (12) or the market (11): cancel.
            if attempt == 0 && !close && matches!(status, 11 | 12) {
                ok(
                    "cancel",
                    api.delete(Pool::Trading, &paths.cancel_open(*order_id))
                        .await?,
                )?;
            }
            tokio::time::sleep(retry).await;
        }
        if !done {
            return Err(decode(format!(
                "eToro order {client_order_id} still in flight; retry when it ends"
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
        account_matches(&inst.id, held, *etoro_id, &positions)?;
    }
    Ok(recovered)
}

/// Errors unless the account's real units on `etoro_id` equal the books'
/// `held`. The bot needs its symbols to itself: a sell closes the
/// account's oldest positions, so a position it didn't open would be
/// sold as its own; and after a split eToro changes the units while the
/// books can't.
pub(crate) fn account_matches(
    id: &InstrumentId,
    held: Decimal,
    etoro_id: i64,
    positions: &[EtoroPosition],
) -> Result<(), ExchangeError> {
    let venue: Decimal = positions
        .iter()
        .filter(|p| p.etoro_id == etoro_id)
        .map(|p| p.units)
        .sum();
    if held > venue {
        return Err(decode(format!(
            "{id}: the books hold {held} but the eToro account only {venue}: \
             positions were closed outside the bot"
        )));
    }
    if held < venue {
        return Err(decode(format!(
            "{id}: the eToro account holds {venue} but the books only {held}: \
             a stock split or other corporate action changed the units, or \
             positions were opened outside the bot (it needs its symbols to \
             itself); stopped rather than sell the wrong amount"
        )));
    }
    Ok(())
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
            Ok(self.next("DELETE", path))
        }
        async fn patch(
            &self,
            _: Pool,
            path: &str,
            body: serde_json::Value,
        ) -> Result<Reply, ExchangeError> {
            Ok(self.next(&body.to_string(), path))
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

    fn limit(id: &str, side: Side, size: Size, price: Decimal) -> Order {
        Order::new(OrderSpec {
            kind: OrderKind::Limit {
                price: Price::new(price),
                tif: bot_core::TimeInForce::Gtc,
            },
            ..order(id, side, size).into()
        })
        .unwrap()
    }

    fn close_bar(close: &str) -> MarketEvent {
        let p: Price = close.parse().unwrap();
        MarketEvent::Candle(
            bot_core::Candle::new(bot_core::CandleSpec {
                instrument: btc().id.clone(),
                interval: bot_core::Interval::H1,
                open_time: Timestamp::from_millis(0),
                close_time: Timestamp::from_millis(3_599_999),
                open: p,
                high: p,
                low: p,
                close: p,
                volume: Qty::new(dec!(0)),
                quote_volume: Notional::ZERO,
            })
            .unwrap(),
        )
    }

    /// A position as the demo pnl lists it, with or without a take-profit.
    fn pnl_with(tp: Option<&str>) -> String {
        let tp = match tp {
            Some(r) => format!(r#""takeProfitRate":{r},"isNoTakeProfit":false"#),
            None => r#""takeProfitRate":0.0,"isNoTakeProfit":true"#.to_owned(),
        };
        format!(
            r#"{{"clientPortfolio":{{"positions":[{{"positionID":3644945509,"openDateTime":"2026-11-17T18:18:57.373Z","instrumentID":100000,"leverage":1,"units":0.000314,"settlementTypeID":1,{tp}}}],"credit":1}}}}"#
        )
    }

    const PNL_EMPTY: &str = r#"{"clientPortfolio":{"positions":[],"credit":1}}"#;

    /// A resting buy limit goes out as `mit` at the limit (demo 2026-11-18:
    /// status 12 while waiting); cancelling sends DELETE; status 7 ends it
    /// with no fill.
    #[tokio::test]
    async fn a_resting_buy_is_market_if_touched_and_cancels() {
        let db = TempDb::new().await;
        let fake = Fake::default();
        let sent = fake.sent.clone();
        let waiting_pnl =
            r#"{"clientPortfolio":{"positions":[],"orders":[{"orderID":394815785}],"credit":1}}"#;
        fake.reply(200, r#"{"orderId":394815785}"#);
        fake.reply(200, waiting_pnl);
        fake.reply(200, r#"{"orderId":394815785,"status":{"id":12,"name":"PendingTriggeredRate"},"positionExecutions":[]}"#);
        fake.reply(200, r#"{"orderId":394815785,"referenceId":""}"#);
        fake.reply(200, waiting_pnl);
        fake.reply(
            200,
            r#"{"orderId":394815785,"status":{"id":7,"name":"Canceled"},"positionExecutions":[]}"#,
        );
        let mut b = EtoroBroker::open(
            Box::new(fake),
            vec![(btc(), 100000)],
            "tbrun".into(),
            db.store.clone(),
        )
        .await
        .unwrap();
        b.submit(limit(
            "o-1",
            Side::Buy,
            Size::Base(Qty::new(dec!(0.000349))),
            dec!(57285.29),
        ))
        .await
        .unwrap();
        let body: serde_json::Value = serde_json::from_str(&sent.lock().unwrap()[0].1).unwrap();
        assert_eq!(
            (body["orderType"].as_str(), body["triggerRate"].to_string()),
            (Some("mit"), "57285.29".to_owned())
        );
        assert_eq!(body["units"].to_string(), "0.000349");
        assert!(b.settle(&tick()).await.unwrap().is_empty());
        assert_eq!(b.open_orders().len(), 1);
        b.cancel(OrderId::new("o-1").unwrap()).await.unwrap();
        assert_eq!(
            sent.lock().unwrap()[3],
            (
                "/api/v2/trading/execution/demo/orders/394815785".to_owned(),
                "DELETE".to_owned()
            )
        );
        assert_eq!(b.cancelling_orders().len(), 1);
        assert!(b.settle(&tick()).await.unwrap().is_empty());
        assert!(b.open_orders().is_empty() && b.cancelling_orders().is_empty());
        // An IOC limit is refused before any request.
        let n = sent.lock().unwrap().len();
        let ioc = Order::new(OrderSpec {
            kind: OrderKind::Limit {
                price: Price::new(dec!(1)),
                tif: bot_core::TimeInForce::Ioc,
            },
            ..order("o-2", Side::Buy, Size::Base(Qty::new(dec!(1)))).into()
        })
        .unwrap();
        assert!(b.submit(ioc).await.is_err());
        assert_eq!(sent.lock().unwrap().len(), n);
    }

    /// Held: 0.000314 @ 63,638.48 + 0.2 fee (basis 20.18248272). A sell
    /// limit at 70,000 above the last close (63,000) becomes a take-profit
    /// on that position (PATCH, 202); the portfolio then shows it. The
    /// position later disappears and the private feed reports the close:
    /// 0.000314 @ 70,001, fee 0.2 (eToro's) → realized 0.000314 × 70,001 −
    /// 0.2 − 20.18248272 = 1.59783128; flat. A limit sell of 0.0001 (not a
    /// whole position) is refused with only the portfolio read.
    #[tokio::test]
    async fn a_resting_sell_is_a_take_profit_filled_with_the_feeds_fee() {
        let db = TempDb::new().await;
        let fake = Fake::default();
        let sent = fake.sent.clone();
        fake.reply(200, OPEN_ACK);
        fake.reply(200, LOOKUP_FILLED);
        fake.reply(200, &pnl_with(None));
        fake.reply(200, &pnl_with(None));
        fake.reply(
            202,
            r#"{"operationId":"92f1","positionId":3644945509,"referenceId":"18e7"}"#,
        );
        fake.reply(200, &pnl_with(Some("70000")));
        fake.reply(200, PNL_EMPTY);
        let stream = EtoroStream::manual();
        let mut b = EtoroBroker::open(
            Box::new(fake),
            vec![(btc(), 100000)],
            "tbrun".into(),
            db.store.clone(),
        )
        .await
        .unwrap()
        .with_stream(stream.clone());
        b.submit(order(
            "o-1",
            Side::Buy,
            Size::Quote(Notional::new(dec!(20))),
        ))
        .await
        .unwrap();
        b.settle(&close_bar("63000")).await.unwrap();
        assert!(
            b.submit(limit(
                "o-2",
                Side::Sell,
                Size::Base(Qty::new(dec!(0.0001))),
                dec!(70000)
            ))
            .await
            .is_err()
        );
        b.submit(limit(
            "o-3",
            Side::Sell,
            Size::Base(Qty::new(dec!(0.000314))),
            dec!(70000),
        ))
        .await
        .unwrap();
        let (path, body) = sent.lock().unwrap()[4].clone();
        assert_eq!(path, "/api/v2/trading/demo/positions/3644945509");
        assert_eq!(body, r#"{"takeProfitRate":70000}"#);
        assert!(b.settle(&tick()).await.unwrap().is_empty());
        assert_eq!(b.open_orders().len(), 1);
        stream.absorb(vec![super::super::stream::Event::Closed(
            super::super::stream::Closed {
                order_id: 777,
                position_id: 3644945509,
                units: dec!(0.000314),
                price: dec!(70001),
                fee: dec!(0.2),
                time: Timestamp::from_millis(9),
            },
        )]);
        let f = b.settle(&tick()).await.unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(
            (f[0].side, f[0].price.get(), f[0].fee.clone()),
            (
                Side::Sell,
                dec!(70001),
                Fee::Quote(Notional::new(dec!(0.2)))
            )
        );
        let p = b.position(&btc().id);
        assert!(p.is_flat());
        assert_eq!(p.realized_pnl().get(), dec!(1.59783128));
        assert!(b.open_orders().is_empty());
    }

    /// A market close books eToro's fee from the private feed (0.2) rather
    /// than the one derived from rounded proceeds (0.19864026): realized
    /// 0.000314 × 63,658.09 − 0.2 − 20.18248272 = −0.39384246.
    #[tokio::test]
    async fn a_market_close_takes_the_feeds_fee() {
        let db = TempDb::new().await;
        let fake = Fake::default();
        fake.reply(200, OPEN_ACK);
        fake.reply(200, LOOKUP_FILLED);
        fake.reply(200, PNL);
        fake.reply(200, CLOSE_ACK);
        fake.reply(200, CLOSE_FILLED);
        let stream = EtoroStream::manual();
        let mut b = EtoroBroker::open(
            Box::new(fake),
            vec![(btc(), 100000)],
            "tbrun".into(),
            db.store.clone(),
        )
        .await
        .unwrap()
        .with_stream(stream.clone());
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
            Size::Base(Qty::new(dec!(0.000314))),
        ))
        .await
        .unwrap();
        stream.absorb(vec![super::super::stream::Event::Closed(
            super::super::stream::Closed {
                order_id: 394881160,
                position_id: 3644945509,
                units: dec!(0.000314),
                price: dec!(63658.09),
                fee: dec!(0.2),
                time: Timestamp::from_millis(9),
            },
        )]);
        let f = b.settle(&tick()).await.unwrap();
        assert_eq!(f[0].fee, Fee::Quote(Notional::new(dec!(0.2))));
        assert_eq!(
            b.position(&btc().id).realized_pnl().get(),
            dec!(-0.39384246)
        );
    }

    /// After a close of 63,000: a buy limit at 63,500 (above) is
    /// marketable → `mkt`; at 62,000 (below) → `mit` at 62,000.
    #[tokio::test]
    async fn a_marketable_buy_limit_goes_at_market() {
        let db = TempDb::new().await;
        let fake = Fake::default();
        let sent = fake.sent.clone();
        fake.reply(200, r#"{"orderId":1}"#);
        fake.reply(200, r#"{"orderId":2}"#);
        let mut b = EtoroBroker::open(
            Box::new(fake),
            vec![(btc(), 100000)],
            "tbrun".into(),
            db.store.clone(),
        )
        .await
        .unwrap();
        b.settle(&close_bar("63000")).await.unwrap();
        let units = || Size::Base(Qty::new(dec!(0.0003)));
        b.submit(limit("o-1", Side::Buy, units(), dec!(63500)))
            .await
            .unwrap();
        b.submit(limit("o-2", Side::Buy, units(), dec!(62000)))
            .await
            .unwrap();
        let bodies: Vec<serde_json::Value> = sent
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| serde_json::from_str(b).unwrap())
            .collect();
        assert_eq!(bodies[0]["orderType"], "mkt");
        assert!(bodies[0].get("triggerRate").is_none());
        assert_eq!(
            (
                bodies[1]["orderType"].as_str(),
                bodies[1]["triggerRate"].to_string()
            ),
            (Some("mit"), "62000".to_owned())
        );
    }

    /// The demo case of 2026-11-18: mit 394898490 still answers status 12,
    /// has left the waiting list, and position 3644994131 was opened by
    /// execution order 394920309, whose lookup names 394898490: 0.011185
    /// ETH @ 1,787.34, fee 0.2 → one fill. An unrelated position's order
    /// is looked up once, not again next settle.
    #[tokio::test]
    async fn a_triggered_mit_is_found_through_its_execution() {
        let db = TempDb::new().await;
        let fake = Fake::default();
        let sent = fake.sent.clone();
        let pnl = |pending: bool| {
            let orders = if pending {
                r#"[{"orderID":394898490}]"#
            } else {
                "[]"
            };
            format!(
                r#"{{"clientPortfolio":{{"positions":[{{"positionID":7,"orderID":111,"openDateTime":"2026-11-17T19:00:00Z","instrumentID":100000,"leverage":1,"units":1,"settlementTypeID":1}},{{"positionID":3644994131,"orderID":394920309,"openDateTime":"2026-11-17T20:01:12.583Z","instrumentID":100000,"leverage":1,"units":0.011185,"settlementTypeID":1}}],"orders":{orders},"credit":1}}}}"#
            )
        };
        let waiting = r#"{"orderId":394898490,"type":"mit","status":{"id":12,"name":"PendingTriggeredRate"},"positionExecutions":[]}"#;
        fake.reply(200, r#"{"orderId":394898490}"#);
        fake.reply(200, &pnl(true));
        fake.reply(200, waiting);
        fake.reply(200, &pnl(false));
        fake.reply(200, waiting);
        fake.reply(
            200,
            r#"{"orderId":999,"status":{"id":3},"positionExecutions":[]}"#,
        );
        fake.reply(200, r#"{"orderId":394898490,"type":"mit","status":{"id":3,"name":"Filled"},"positionExecutions":[{"positionId":3644994131,"openingData":{"executionTime":"2026-11-17T20:01:12.583Z","units":0.011185,"avgPrice":1787.34,"fees":0.2}}]}"#);
        let mut b = EtoroBroker::open(
            Box::new(fake),
            vec![(btc(), 100000)],
            "tbrun".into(),
            db.store.clone(),
        )
        .await
        .unwrap();
        b.settle(&close_bar("1790")).await.unwrap();
        b.submit(limit(
            "o-1",
            Side::Buy,
            Size::Base(Qty::new(dec!(0.011185))),
            dec!(1788.135),
        ))
        .await
        .unwrap();
        assert!(b.settle(&tick()).await.unwrap().is_empty());
        let f = b.settle(&tick()).await.unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(
            (f[0].qty.get(), f[0].price.get()),
            (dec!(0.011185), dec!(1787.34))
        );
        assert_eq!(f[0].fee, Fee::Quote(Notional::new(dec!(0.2))));
        assert!(b.open_orders().is_empty());
        let looked: Vec<String> = sent
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.clone())
            .filter(|p| p.contains("lookup"))
            .collect();
        assert_eq!(looked.iter().filter(|p| p.ends_with("=111")).count(), 1);
        assert!(looked.iter().any(|p| p.ends_with("=394920309")));
    }

    fn eth() -> Instrument {
        Instrument::new(InstrumentSpec {
            id: "etoro:ETH".parse().unwrap(),
            lot_size: Qty::new(dec!(0.000001)),
            min_qty: Qty::new(dec!(0.000001)),
            ..(*btc()).clone()
        })
        .unwrap()
    }

    fn position(units: Decimal) -> EtoroPosition {
        EtoroPosition {
            position_id: 3644994131,
            order_id: 1,
            etoro_id: 100001,
            units,
            open_time: 0,
            take_profit: None,
        }
    }

    /// The demo case: position 0.011185 ETH, at 1,790:
    /// - selling 0.0111794 would leave 0.0000056 × 1,790 = $0.01 (< $1,
    ///   which eToro refused five times over) → the whole position closes;
    /// - selling 0.004 deducts 0.004 × 1,790 = $7.16 (< $10) → refused here;
    /// - selling 0.00600049 → 0.006 (step 1e-6) deducts $10.74, leaves
    ///   0.005185 × 1,790 = $9.28 → a partial close of 0.006;
    /// - with no close known yet, eToro decides: the partial goes out.
    #[test]
    fn close_plans_follow_etoros_partial_close_rules() {
        let free = [position(dec!(0.011185))];
        let px = Some(Price::new(dec!(1790)));
        let plan = close_plan(&free, dec!(0.0111794), &eth(), px).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].1, None);
        assert!(close_plan(&free, dec!(0.004), &eth(), px).is_err());
        let plan = close_plan(&free, dec!(0.00600049), &eth(), px).unwrap();
        assert_eq!(plan[0].1, Some(dec!(0.006)));
        let plan = close_plan(&free, dec!(0.004), &eth(), None).unwrap();
        assert_eq!(plan[0].1, Some(dec!(0.004)));
        // Two positions, sell the first and part of the second.
        let two = [position(dec!(0.01)), position(dec!(0.02))];
        let plan = close_plan(&two, dec!(0.018), &eth(), px).unwrap();
        assert_eq!((plan[0].1, plan[1].1), (None, Some(dec!(0.008))));
    }

    /// A BTC bar closing at the end of hour `h`.
    fn bar_at(h: i64) -> MarketEvent {
        let MarketEvent::Candle(c) = close_bar("63000") else {
            unreachable!()
        };
        MarketEvent::Candle(
            bot_core::Candle::new(bot_core::CandleSpec {
                open_time: Timestamp::from_millis(h * 3_600_000),
                close_time: Timestamp::from_millis(h * 3_600_000 + 3_599_999),
                ..c.into()
            })
            .unwrap(),
        )
    }

    /// The demo buy (0.000314 BTC) in the books, then one portfolio read
    /// per close time while idle: equal units pass (the leveraged CFD in
    /// `PNL` isn't ours and isn't counted), the same close time isn't read
    /// twice, and units ×10 (what a 10-for-1 split does to the position)
    /// stop the run.
    #[tokio::test]
    async fn the_account_check_stops_on_units_the_books_dont_have() {
        let db = TempDb::new().await;
        let fake = Fake::default();
        let sent = fake.sent.clone();
        fake.reply(200, OPEN_ACK);
        fake.reply(200, LOOKUP_FILLED);
        fake.reply(200, PNL);
        fake.reply(200, &PNL.replace("\"units\":0.000314", "\"units\":0.00314"));
        let mut b = EtoroBroker::open(
            Box::new(fake),
            vec![(btc(), 100000)],
            "tbrun".into(),
            db.store.clone(),
        )
        .await
        .unwrap()
        .with_account_check();
        b.submit(order(
            "o-1",
            Side::Buy,
            Size::Quote(Notional::new(dec!(20))),
        ))
        .await
        .unwrap();
        // The fill's own bar: no read (an order was working).
        assert_eq!(b.settle(&bar_at(0)).await.unwrap().len(), 1);
        assert_eq!(sent.lock().unwrap().len(), 2);
        b.settle(&bar_at(1)).await.unwrap();
        b.settle(&bar_at(1)).await.unwrap();
        assert_eq!(sent.lock().unwrap().len(), 3, "one read per close time");
        let err = b.settle(&bar_at(2)).await.unwrap_err().to_string();
        assert!(
            err.contains("holds 0.00314 but the books only 0.000314") && err.contains("split"),
            "{err}"
        );
    }

    #[test]
    fn the_account_must_match_the_books_both_ways() {
        let id = btc().id.clone();
        let pos = parse_positions(PNL).unwrap();
        assert!(account_matches(&id, dec!(0.000314), 100000, &pos).is_ok());
        let more = account_matches(&id, dec!(0.0004), 100000, &pos).unwrap_err();
        assert!(more.to_string().contains("closed outside the bot"));
        let less = account_matches(&id, dec!(0.0001), 100000, &pos).unwrap_err();
        assert!(less.to_string().contains("opened outside the bot"));
        assert!(
            account_matches(&id, dec!(0), 999, &pos).is_ok(),
            "other instrument"
        );
    }
}
