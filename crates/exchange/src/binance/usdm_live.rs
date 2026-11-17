//! Orders on Binance USDⓈ-M perpetuals (one-way mode): [`UsdmBroker`],
//! its startup and exit [`reconcile`], and [`UsdmProvider`].
//!
//! The venue is the source of truth for the position. The books (journal,
//! trade log, holdings, same tables as spot: a signed holding needs no
//! schema change) record which orders and fills are this bot's.
//!
//! - **Account setup** ([`prepare`], at startup): one-way position mode is
//!   required (hedge mode refused); each symbol's margin type and leverage
//!   are set to the run's (`-4046` "no need to change" is fine; Binance
//!   refuses a margin-type change while a position or order is open).
//! - **Orders**: `POST /fapi/v1/order`, `newClientOrderId` = run tag + our
//!   id (as spot), journaled `pending` first. Stops go to the algo-order
//!   service (`POST /fapi/v1/algoOrder`, `CONDITIONAL`, `triggerPrice`,
//!   last-price trigger as the backtest's; `/fapi/v1/order` refuses them
//!   with `-4120` since 2025-12-09): journaled with the `algoId`, polled as
//!   an algo order until its `actualOrderId` appears, then as that order.
//!   Reconcile looks every journal row up as an algo order first (by
//!   client id; `-2013` = not one). A trade of a stop that triggered
//!   between its poll and the bar's alignment is adopted as `venue-…`
//!   (books stay right; the strategy sees that order id). A quote budget (`Size::Quote`) has no futures
//!   equivalent: it is converted to base at the last close, lot-rounded
//!   (the paper broker converts at the fill price, a small difference).
//! - **Fills**: each settle polls every working order's status and
//!   `userTrades?orderId=`; a trade becomes a fill once, recorded by its
//!   trade id. Fees are in the margin asset; a fee charged in BNB is an
//!   error (turn off BNB fee burn): its value can't be known exactly.
//! - **Venue's own trades** ([`align`]): every candle, `positionRisk` is
//!   compared with the books. A difference is first looked for in the
//!   account's trades: any not from an order of ours still working
//!   (liquidation, auto-deleverage, a trade placed by hand) is adopted
//!   into the books and reported as a fill (order id `venue-<orderId>`),
//!   so the books follow the venue. One-way mode has one position per
//!   symbol, so the bot needs its symbols to itself. A difference left
//!   with no order of ours working stops the run.
//! - **Funding**: at each `MarketEvent::Funding` the venue's own records
//!   (`/fapi/v1/income`, `FUNDING_FEE`; `tranId` unique within that type
//!   only) since the last read are booked once each into the books
//!   (`Ledger::record_funding`: realized P&L, survives a restart). The
//!   first read reaches 7 days back for a symbol the books held (funding
//!   paid while the bot was down), else starts at the run's start.
//! - **Exit**: working orders are cancelled and their last fills recorded
//!   ([`reconcile`] with [`Phase::Exit`]); the position stays open on
//!   Binance, where it can still be liquidated while the bot is down.

use super::broker::{CLIENT_ID_PREFIX, executed_qty, is_terminal, trade_id};
use super::feed::{ReconnectingFeed, RestKlines, WsConnector};
use super::reconcile::{Orphans, Phase, Reconciled};
use super::signed::{Credentials, Method, ReqwestTransport, SignedClient, SystemClock};
use super::usdm::{self, DEMO_URL, LIVE_URL, TESTNET_URL};
use super::usdm_paper::FundingWatch;
use super::{Network, PublicClient};
use crate::rate_limit::RateLimiter;
use crate::{Books, Broker, ExchangeError, InstrumentQuery, MarketData, Provider};
use async_trait::async_trait;
use bot_core::margin::MarginMode;
use bot_core::{
    AssetClass, Fee, Fill, FillSpec, Instrument, InstrumentId, Liquidity, MarketEvent, Notional,
    Order, OrderId, OrderKind, Position, Price, Qty, Side, Size, TimeInForce, Timestamp,
};
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

/// Binance's limit on `newClientOrderId`.
const CLIENT_ID_MAX: usize = 36;
/// Futures: margin type already as asked.
const NO_NEED_TO_CHANGE_MARGIN: i64 = -4046;
/// The order doesn't exist (never placed, or unknown id).
const NO_SUCH_ORDER: i64 = -2013;
/// Cancel refused, typically because the order just closed.
const CANCEL_REJECTED: i64 = -2011;
/// How far back startup looks for trades the books lack (`userTrades`
/// serves at most 7 days per request).
const STARTUP_LOOKBACK_MS: i64 = 7 * 86_400_000;

/// Futures kline stream of `network` (the `/market` path).
pub fn ws_url(network: Network) -> &'static str {
    match network {
        Network::Live => super::usdm_paper::WS_URL,
        Network::Demo => "wss://demo-fstream.binance.com/market",
        Network::Testnet => "wss://fstream.binancefuture.com/market",
    }
}

/// Futures REST host of `network`.
pub fn rest_url(network: Network) -> &'static str {
    match network {
        Network::Live => LIVE_URL,
        Network::Demo => DEMO_URL,
        Network::Testnet => TESTNET_URL,
    }
}

/// Sets the account up for this run: one-way mode (checked, never
/// switched: that would touch positions of other software), then `mode`
/// and `leverage` on every symbol.
pub async fn prepare(
    client: &SignedClient,
    instruments: &[Instrument],
    leverage: u32,
    mode: MarginMode,
) -> Result<(), ExchangeError> {
    let dual = client
        .signed(Method::Get, "/fapi/v1/positionSide/dual", &[])
        .await?;
    if dual["dualSidePosition"].as_bool() != Some(false) {
        return Err(ExchangeError::Credentials(
            "the futures account is in hedge mode; switch it to one-way mode".into(),
        ));
    }
    let margin_type = match mode {
        MarginMode::Isolated => "ISOLATED",
        MarginMode::Cross => "CROSSED",
    };
    for i in instruments {
        let symbol = i.id.symbol().to_string();
        match client
            .signed(
                Method::Post,
                "/fapi/v1/marginType",
                &[
                    ("symbol", symbol.clone()),
                    ("marginType", margin_type.to_owned()),
                ],
            )
            .await
        {
            Ok(_) => {}
            Err(ExchangeError::Venue { code, .. }) if code == NO_NEED_TO_CHANGE_MARGIN => {}
            Err(e) => return Err(e),
        }
        let set = client
            .signed(
                Method::Post,
                "/fapi/v1/leverage",
                &[("symbol", symbol), ("leverage", leverage.to_string())],
            )
            .await?;
        if set["leverage"].as_u64() != Some(u64::from(leverage)) {
            return Err(ExchangeError::Decode(format!(
                "{}: leverage not set to {leverage}",
                i.id
            )));
        }
    }
    Ok(())
}

/// One `userTrades` entry as a fill of `order_id` on `inst`.
pub(crate) fn trade_to_fill(
    inst: &Instrument,
    order_id: &OrderId,
    t: &Value,
) -> Result<Fill, ExchangeError> {
    let dec = |name: &str| -> Result<Decimal, ExchangeError> {
        t[name]
            .as_str()
            .and_then(|s| Decimal::from_str(s).ok())
            .ok_or_else(|| ExchangeError::Decode(format!("futures trade {name}")))
    };
    let asset = t["commissionAsset"].as_str().unwrap_or_default();
    if asset != inst.quote.as_str() {
        return Err(ExchangeError::Decode(format!(
            "futures fee paid in {asset}: turn off BNB fee burn (its value isn't known exactly)"
        )));
    }
    let side = match t["side"].as_str() {
        Some("BUY") => Side::Buy,
        Some("SELL") => Side::Sell,
        _ => return Err(ExchangeError::Decode("futures trade side".into())),
    };
    Fill::new(FillSpec {
        order_id: order_id.clone(),
        instrument: inst.id.clone(),
        side,
        qty: Qty::new(dec("qty")?),
        price: Price::new(dec("price")?),
        fee: Fee::Quote(Notional::new(dec("commission")?)),
        liquidity: if t["maker"].as_bool() == Some(true) {
            Liquidity::Maker
        } else {
            Liquidity::Taker
        },
        ts: Timestamp::from_millis(
            t["time"]
                .as_i64()
                .ok_or_else(|| ExchangeError::Decode("futures trade time".into()))?,
        ),
    })
    .map_err(|e| ExchangeError::Decode(e.to_string()))
}

fn order_id_of(t: &Value) -> Result<i64, ExchangeError> {
    t["orderId"]
        .as_i64()
        .ok_or_else(|| ExchangeError::Decode("futures trade orderId".into()))
}

/// The venue's signed position (`positionAmt`) on `inst`; flat when it
/// lists none.
async fn venue_qty(client: &SignedClient, inst: &Instrument) -> Result<Decimal, ExchangeError> {
    let v = client
        .signed(
            Method::Get,
            "/fapi/v3/positionRisk",
            &[("symbol", inst.id.symbol().to_string())],
        )
        .await?;
    let rows = v
        .as_array()
        .ok_or_else(|| ExchangeError::Decode("positionRisk".into()))?;
    let mut total = Decimal::ZERO;
    for r in rows {
        total += r["positionAmt"]
            .as_str()
            .and_then(|s| Decimal::from_str(s).ok())
            .ok_or_else(|| ExchangeError::Decode("positionRisk positionAmt".into()))?;
    }
    Ok(total)
}

/// Brings the books' position on `inst` in line with the venue's (module
/// docs): adopts trades since `since` that aren't from an order in
/// `working` and aren't recorded yet. Returns the adopted fills and the
/// position. Errors when the two still differ and nothing of ours is
/// working that could explain it.
pub async fn align(
    client: &SignedClient,
    books: &dyn Books,
    inst: &Instrument,
    held: Position,
    working: &HashSet<i64>,
    since: Timestamp,
) -> Result<(Vec<Fill>, Position), ExchangeError> {
    let venue = venue_qty(client, inst).await?;
    if venue == held.qty().get() {
        return Ok((Vec::new(), held));
    }
    let trades = client
        .signed(
            Method::Get,
            "/fapi/v1/userTrades",
            &[
                ("symbol", inst.id.symbol().to_string()),
                ("startTime", since.as_millis().to_string()),
                ("limit", "1000".to_owned()),
            ],
        )
        .await?;
    let mut position = held;
    let mut adopted = Vec::new();
    for t in trades
        .as_array()
        .ok_or_else(|| ExchangeError::Decode("userTrades".into()))?
    {
        let order = order_id_of(t)?;
        if working.contains(&order) {
            continue; // ours: its poll reports it
        }
        let oid = OrderId::new(format!("venue-{order}"))
            .map_err(|e| ExchangeError::Decode(e.to_string()))?;
        let fill = trade_to_fill(inst, &oid, t)?;
        if let Some((_, p)) = books
            .record_venue_fill(&fill, inst, &trade_id(t)?.to_string(), oid.as_str())
            .await?
        {
            tracing::warn!(instrument = %inst.id, venue_order = order, side = ?fill.side, "adopted a trade the bot didn't place (liquidation, deleverage or manual)");
            position = p;
            adopted.push(fill);
        }
    }
    if venue != position.qty().get() && working.is_empty() {
        return Err(ExchangeError::Decode(format!(
            "{}: Binance holds a different position than the books, and no trade explains it; \
             does other software trade this symbol?",
            inst.id
        )));
    }
    Ok((adopted, position))
}

struct LiveOrder {
    order: Order,
    client_id: String,
    /// The matching engine's order: from the ack, or for a stop once its
    /// algo order triggered (`actualOrderId`).
    venue_id: Option<i64>,
    /// A stop's algo order (`algoId`).
    algo_id: Option<i64>,
    cancelling: bool,
    seen_trades: HashSet<i64>,
    seen_qty: Qty,
}

/// See the module docs.
pub struct UsdmBroker {
    client: SignedClient,
    instruments: HashMap<InstrumentId, Instrument>,
    positions: HashMap<InstrumentId, Position>,
    open: Vec<LiveOrder>,
    run_tag: String,
    books: Arc<dyn Books>,
    recovered: Vec<Fill>,
    last_close: HashMap<InstrumentId, Price>,
    /// Trades older than this were checked at startup.
    since: Timestamp,
    /// Where the next funding read starts, per instrument.
    funding_from: HashMap<InstrumentId, Timestamp>,
}

impl UsdmBroker {
    /// Starts from the books' holdings for `instruments` (already
    /// reconciled and aligned).
    pub async fn open(
        client: SignedClient,
        instruments: Vec<Instrument>,
        run_tag: String,
        books: Arc<dyn Books>,
        since: Timestamp,
    ) -> Result<Self, ExchangeError> {
        let mut positions = HashMap::new();
        let mut funding_from = HashMap::new();
        for i in &instruments {
            // Held before: funding paid while the bot was down is ours,
            // as far back as the venue serves (7 days). Never held: only
            // from now (earlier funding was someone else's position).
            let from = if let Some(p) = books.get_holding(&i.id).await? {
                p.check_against(i)
                    .map_err(|e| ExchangeError::Decode(format!("stored holding: {e}")))?;
                positions.insert(i.id.clone(), p);
                Timestamp::from_millis(since.as_millis() - STARTUP_LOOKBACK_MS)
            } else {
                since
            };
            funding_from.insert(i.id.clone(), from);
        }
        Ok(Self {
            client,
            instruments: instruments.into_iter().map(|i| (i.id.clone(), i)).collect(),
            positions,
            open: Vec::new(),
            run_tag,
            books,
            recovered: Vec::new(),
            last_close: HashMap::new(),
            since,
            funding_from,
        })
    }

    /// Books the venue's funding payments on `id` since the last read
    /// (`/fapi/v1/income`, `FUNDING_FEE`, weight 30), each once by its
    /// `tranId`, into the position's realized P&L.
    async fn sync_funding(&mut self, id: &InstrumentId) -> Result<(), ExchangeError> {
        let inst = self.inst(id)?.clone();
        let from = self.funding_from.get(id).copied().unwrap_or(self.since);
        let rows = self
            .client
            .signed(
                Method::Get,
                "/fapi/v1/income",
                &[
                    ("symbol", id.symbol().to_string()),
                    ("incomeType", "FUNDING_FEE".to_owned()),
                    ("startTime", from.as_millis().to_string()),
                    ("limit", "1000".to_owned()),
                ],
            )
            .await?;
        let mut newest = from;
        for r in rows
            .as_array()
            .ok_or_else(|| ExchangeError::Decode("income".into()))?
        {
            if r["asset"].as_str() != Some(inst.quote.as_str()) {
                return Err(ExchangeError::Decode(format!(
                    "{id}: funding paid in {}, not the quote",
                    r["asset"]
                )));
            }
            let amount = r["income"]
                .as_str()
                .and_then(|s| Decimal::from_str(s).ok())
                .ok_or_else(|| ExchangeError::Decode("income amount".into()))?;
            let tran = r["tranId"]
                .as_i64()
                .ok_or_else(|| ExchangeError::Decode("income tranId".into()))?;
            let ts = Timestamp::from_millis(
                r["time"]
                    .as_i64()
                    .ok_or_else(|| ExchangeError::Decode("income time".into()))?,
            );
            newest = newest.max(ts);
            if let Some(p) = self
                .books
                .record_funding(id, &tran.to_string(), Notional::new(amount), ts)
                .await?
            {
                self.positions.insert(id.clone(), p);
            }
        }
        self.funding_from.insert(id.clone(), newest);
        Ok(())
    }

    /// Fills the startup reconcile recovered or adopted.
    pub fn with_recovered(mut self, fills: Vec<Fill>) -> Self {
        self.recovered = fills;
        self
    }

    fn inst(&self, id: &InstrumentId) -> Result<&Instrument, ExchangeError> {
        self.instruments
            .get(id)
            .ok_or_else(|| ExchangeError::UnknownInstrument(id.clone()))
    }

    /// New trades of order `idx`; whether it is finished.
    /// A stop not yet triggered: reads its algo order. Returns `Some(done)`
    /// while there is no matching-engine order to poll (`done`: it ended
    /// untriggered: cancelled, expired, rejected); `None` once it has one.
    async fn poll_algo(&mut self, idx: usize) -> Result<Option<bool>, ExchangeError> {
        let Some(algo_id) = self.open[idx]
            .algo_id
            .filter(|_| self.open[idx].venue_id.is_none())
        else {
            return Ok(None);
        };
        let algo = self
            .client
            .signed(
                Method::Get,
                "/fapi/v1/algoOrder",
                &[("algoId", algo_id.to_string())],
            )
            .await?;
        match actual_order_id(&algo)? {
            Some(id) => {
                self.open[idx].venue_id = Some(id);
                Ok(None)
            }
            None => {
                let ended = algo_ended(&algo);
                if ended {
                    self.books.mark_closed(&self.open[idx].client_id).await?;
                }
                Ok(Some(ended))
            }
        }
    }

    async fn poll(&mut self, idx: usize) -> Result<(Vec<Fill>, bool), ExchangeError> {
        if let Some(done) = self.poll_algo(idx).await? {
            return Ok((Vec::new(), done));
        }
        let (symbol, venue_id) = {
            let o = &self.open[idx];
            let id = o
                .venue_id
                .ok_or_else(|| ExchangeError::Decode("order without a venue id".into()))?;
            (o.order.instrument.symbol().to_string(), id)
        };
        let status = self
            .client
            .signed(
                Method::Get,
                "/fapi/v1/order",
                &[
                    ("symbol", symbol.clone()),
                    ("orderId", venue_id.to_string()),
                ],
            )
            .await?;
        let trades = self
            .client
            .signed(
                Method::Get,
                "/fapi/v1/userTrades",
                &[("symbol", symbol), ("orderId", venue_id.to_string())],
            )
            .await?;
        let mut fills = Vec::new();
        for t in trades
            .as_array()
            .ok_or_else(|| ExchangeError::Decode("userTrades".into()))?
        {
            let id = trade_id(t)?;
            if self.open[idx].seen_trades.contains(&id) {
                continue;
            }
            let inst = self.inst(&self.open[idx].order.instrument)?.clone();
            let fill = trade_to_fill(&inst, &self.open[idx].order.id, t)?;
            let recorded = self
                .books
                .record_venue_fill(&fill, &inst, &id.to_string(), &self.open[idx].client_id)
                .await?;
            self.open[idx].seen_trades.insert(id);
            self.open[idx].seen_qty += fill.qty;
            if let Some((_, position)) = recorded {
                self.positions.insert(fill.instrument.clone(), position);
                fills.push(fill);
            }
        }
        let done = is_terminal(&status) && self.open[idx].seen_qty.get() == executed_qty(&status)?;
        if done {
            self.books.mark_closed(&self.open[idx].client_id).await?;
        }
        Ok((fills, done))
    }
}

fn text(d: Decimal) -> String {
    d.normalize().to_string()
}

/// A triggered algo order's matching-engine order (`actualOrderId`, a
/// string, empty until triggered).
fn actual_order_id(algo: &Value) -> Result<Option<i64>, ExchangeError> {
    match &algo["actualOrderId"] {
        Value::String(s) if s.is_empty() => Ok(None),
        Value::String(s) => s
            .parse()
            .map(Some)
            .map_err(|_| ExchangeError::Decode("algo actualOrderId".into())),
        Value::Number(n) => Ok(n.as_i64()),
        Value::Null => Ok(None),
        _ => Err(ExchangeError::Decode("algo actualOrderId".into())),
    }
}

/// An algo order that ended without triggering.
fn algo_ended(algo: &Value) -> bool {
    matches!(
        algo["algoStatus"].as_str(),
        Some("CANCELED" | "EXPIRED" | "REJECTED" | "FINISHED")
    )
}

/// Parameters for `POST /fapi/v1/algoOrder`: a stop as a conditional
/// order triggered by the last price (`CONTRACT_PRICE`, as the backtest's
/// fill model triggers on the bar's last-price range).
fn algo_params(o: &Order, qty: Qty, client_id: String) -> Vec<(&'static str, String)> {
    let side = match o.side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    };
    let mut p = vec![
        ("algoType", "CONDITIONAL".to_owned()),
        ("symbol", o.instrument.symbol().to_string()),
        ("side", side.to_owned()),
    ];
    match o.kind {
        OrderKind::StopMarket { stop } => {
            p.push(("type", "STOP_MARKET".to_owned()));
            p.push(("triggerPrice", text(stop.get())));
        }
        OrderKind::StopLimit { stop, limit, tif } => {
            p.push(("type", "STOP".to_owned()));
            p.push(("triggerPrice", text(stop.get())));
            p.push(("price", text(limit.get())));
            p.push(("timeInForce", tif_code(tif).to_owned()));
        }
        OrderKind::Market | OrderKind::Limit { .. } => {}
    }
    p.push(("quantity", text(qty.get())));
    if o.reduce_only() {
        p.push(("reduceOnly", "true".to_owned()));
    }
    p.push(("workingType", "CONTRACT_PRICE".to_owned()));
    p.push(("clientAlgoId", client_id));
    p.push(("newOrderRespType", "ACK".to_owned()));
    p
}

fn tif_code(t: TimeInForce) -> &'static str {
    match t {
        TimeInForce::Gtc => "GTC",
        TimeInForce::Ioc => "IOC",
        TimeInForce::Fok => "FOK",
    }
}

/// Parameters for `POST /fapi/v1/order` with base quantity `qty`.
fn order_params(
    o: &Order,
    qty: Qty,
    client_id: String,
) -> Result<Vec<(&'static str, String)>, ExchangeError> {
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
    match o.kind {
        OrderKind::Market => p.push(("type", "MARKET".to_owned())),
        OrderKind::Limit { price, tif } => {
            p.push(("type", "LIMIT".to_owned()));
            p.push(("timeInForce", tif_code(tif).to_owned()));
            p.push(("price", text(price.get())));
        }
        OrderKind::StopMarket { .. } | OrderKind::StopLimit { .. } => {
            return Err(ExchangeError::Rejected("stops go through algo_params"));
        }
    }
    p.push(("quantity", text(qty.get())));
    if o.reduce_only() {
        p.push(("reduceOnly", "true".to_owned()));
    }
    p.push(("newClientOrderId", client_id));
    p.push(("newOrderRespType", "ACK".to_owned()));
    Ok(p)
}

#[async_trait]
impl Broker for UsdmBroker {
    async fn submit(&mut self, order: Order) -> Result<OrderId, ExchangeError> {
        let inst = self.inst(&order.instrument)?.clone();
        if self.open.iter().any(|o| o.order.id == order.id) {
            return Err(ExchangeError::DuplicateOrder(order.id.clone()));
        }
        crate::check_reduce_only(self.position(&order.instrument).qty().get(), &order)?;
        crate::check_open_count(
            &inst,
            self.open.iter().filter(|o| !o.cancelling).map(|o| &o.order),
            &order,
        )?;
        let qty = match order.size {
            Size::Base(q) => q,
            Size::Quote(n) => {
                let close =
                    self.last_close
                        .get(&order.instrument)
                        .ok_or(ExchangeError::Rejected(
                            "no price yet to size a quote budget",
                        ))?;
                let q = inst.round_qty(
                    inst.qty_for(n, *close)
                        .map_err(|_| ExchangeError::Rejected("quote budget too small"))?,
                );
                if !q.is_positive() {
                    return Err(ExchangeError::Rejected("quote budget too small"));
                }
                q
            }
        };
        let client_id = self.client_order_id(&order.id);
        if client_id.len() > CLIENT_ID_MAX {
            return Err(ExchangeError::Rejected(
                "client order id longer than 36 chars",
            ));
        }
        let stop = matches!(
            order.kind,
            OrderKind::StopMarket { .. } | OrderKind::StopLimit { .. }
        );
        let (path, params) = if stop {
            (
                "/fapi/v1/algoOrder",
                algo_params(&order, qty, client_id.clone()),
            )
        } else {
            (
                "/fapi/v1/order",
                order_params(&order, qty, client_id.clone())?,
            )
        };
        self.books
            .insert_pending(&client_id, &order.instrument, order.side, order.created_at)
            .await?;
        let ack = match self.client.signed(Method::Post, path, &params).await {
            Ok(ack) => ack,
            Err(e) => {
                if e.is_order_rejection() {
                    self.books.mark_closed(&client_id).await?;
                }
                return Err(e);
            }
        };
        // A stop's journal id is its algoId: reconcile looks an order up
        // as an algo order (by client id) before as a regular one.
        let key = if stop { "algoId" } else { "orderId" };
        let ack_id = ack[key]
            .as_i64()
            .ok_or_else(|| ExchangeError::Decode(format!("futures order ack {key}")))?;
        self.books.mark_open(&client_id, ack_id).await?;
        let id = order.id.clone();
        self.open.push(LiveOrder {
            order,
            client_id,
            venue_id: (!stop).then_some(ack_id),
            algo_id: stop.then_some(ack_id),
            cancelling: false,
            seen_trades: HashSet::new(),
            seen_qty: Qty::ZERO,
        });
        Ok(id)
    }

    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        let idx = self
            .open
            .iter()
            .position(|o| o.order.id == id && !o.cancelling)
            .ok_or_else(|| ExchangeError::UnknownOrder(id.clone()))?;
        if let (None, Some(algo_id)) = (self.open[idx].venue_id, self.open[idx].algo_id) {
            let cancelled = self
                .client
                .signed(
                    Method::Delete,
                    "/fapi/v1/algoOrder",
                    &[("algoId", algo_id.to_string())],
                )
                .await;
            match cancelled {
                Ok(_) => {
                    self.open[idx].cancelling = true;
                    return Ok(());
                }
                // It may have triggered meanwhile: then cancel the order
                // it became; if it hasn't, the refusal stands.
                Err(e @ ExchangeError::Venue { .. }) => {
                    if self.poll_algo(idx).await?.is_some() {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        let o = &self.open[idx];
        let venue_id = o
            .venue_id
            .ok_or_else(|| ExchangeError::Decode("order without a venue id".into()))?;
        self.client
            .signed(
                Method::Delete,
                "/fapi/v1/order",
                &[
                    ("symbol", o.order.instrument.symbol().to_string()),
                    ("orderId", venue_id.to_string()),
                ],
            )
            .await?;
        self.open[idx].cancelling = true;
        Ok(())
    }

    async fn settle(&mut self, event: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
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
        match event {
            MarketEvent::Funding { instrument, .. } => {
                if self.instruments.contains_key(instrument) {
                    self.sync_funding(instrument).await?;
                }
            }
            MarketEvent::Candle(c) => {
                self.last_close.insert(c.instrument.clone(), c.close);
                let inst = self.inst(&c.instrument)?.clone();
                let working: HashSet<i64> = self
                    .open
                    .iter()
                    .filter(|o| o.order.instrument == c.instrument)
                    .filter_map(|o| o.venue_id)
                    .collect();
                let (adopted, position) = align(
                    &self.client,
                    &*self.books,
                    &inst,
                    self.position(&c.instrument),
                    &working,
                    self.since,
                )
                .await?;
                if !adopted.is_empty() {
                    self.positions.insert(c.instrument.clone(), position);
                    fills.extend(adopted);
                }
            }
            _ => {}
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
        if id.as_str().starts_with(CLIENT_ID_PREFIX) {
            id.to_string()
        } else {
            format!("{}-{}", self.run_tag, id)
        }
    }

    fn take_recovered_fills(&mut self) -> Vec<Fill> {
        std::mem::take(&mut self.recovered)
    }
}

/// Settles every unfinished journal order on `instruments` (find it,
/// cancel if working, wait until closed with its trades in, record them,
/// close it). Startup also refuses (or cancels, `Orphans::Cancel`) open
/// orders carrying our prefix the books don't know, and aligns each
/// position with the venue over the last 7 days of trades.
pub async fn reconcile(
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
        let Some(inst) = by_id.get(&order.instrument) else {
            continue; // not a perpetual of this run (another venue's books)
        };
        let symbol = inst.id.symbol().to_string();
        // A stop is an algo order first: cancel it while untriggered (it
        // then ends with nothing filled); once triggered, settle the
        // order it became.
        let mut lookup = match order.venue_order_id {
            Some(id) => ("orderId", id.to_string()),
            None => ("origClientOrderId", order.client_order_id.clone()),
        };
        if let Some(algo) = find_algo(client, &order.client_order_id).await? {
            match settle_algo(client, &algo, retry_delay).await? {
                Some(actual) => lookup = ("orderId", actual.to_string()),
                None => {
                    books.mark_closed(&order.client_order_id).await?;
                    done.orders_closed += 1;
                    continue;
                }
            }
        }
        let mut status = match client
            .signed(
                Method::Get,
                "/fapi/v1/order",
                &[("symbol", symbol.clone()), lookup.clone()],
            )
            .await
        {
            Ok(s) => s,
            Err(ExchangeError::Venue { code, .. }) if code == NO_SUCH_ORDER => {
                books.mark_closed(&order.client_order_id).await?;
                done.orders_closed += 1;
                continue;
            }
            Err(e) => return Err(e),
        };
        let venue_id = status["orderId"]
            .as_i64()
            .ok_or_else(|| ExchangeError::Decode("futures order orderId".into()))?;
        if !is_terminal(&status) {
            match client
                .signed(
                    Method::Delete,
                    "/fapi/v1/order",
                    &[
                        ("symbol", symbol.clone()),
                        ("orderId", venue_id.to_string()),
                    ],
                )
                .await
            {
                Ok(_) => {}
                Err(ExchangeError::Venue { code, .. }) if code == CANCEL_REJECTED => {}
                Err(e) => return Err(e),
            }
        }
        let oid = OrderId::new(order.client_order_id.clone())
            .map_err(|e| ExchangeError::Decode(e.to_string()))?;
        let mut settled = false;
        for _ in 0..10 {
            let trades = client
                .signed(
                    Method::Get,
                    "/fapi/v1/userTrades",
                    &[
                        ("symbol", symbol.clone()),
                        ("orderId", venue_id.to_string()),
                    ],
                )
                .await?;
            let trades = trades
                .as_array()
                .ok_or_else(|| ExchangeError::Decode("userTrades".into()))?;
            let qty: Decimal = trades
                .iter()
                .filter_map(|t| t["qty"].as_str().and_then(|q| Decimal::from_str(q).ok()))
                .sum();
            if is_terminal(&status) && qty == executed_qty(&status)? {
                for t in trades {
                    let fill = trade_to_fill(inst, &oid, t)?;
                    if books
                        .record_venue_fill(
                            &fill,
                            inst,
                            &trade_id(t)?.to_string(),
                            &order.client_order_id,
                        )
                        .await?
                        .is_some()
                    {
                        done.fills_recovered += 1;
                        done.fills.push(fill);
                    }
                }
                books.mark_closed(&order.client_order_id).await?;
                done.orders_closed += 1;
                settled = true;
                break;
            }
            tokio::time::sleep(retry_delay).await;
            status = client
                .signed(
                    Method::Get,
                    "/fapi/v1/order",
                    &[
                        ("symbol", symbol.clone()),
                        ("orderId", venue_id.to_string()),
                    ],
                )
                .await?;
        }
        if !settled {
            return Err(ExchangeError::Decode(format!(
                "{}: order {} didn't settle on Binance; retry later",
                inst.id, order.client_order_id
            )));
        }
    }
    if let Phase::Startup(orphans) = phase {
        let known: HashSet<String> = books
            .unfinished_orders()
            .await?
            .into_iter()
            .map(|o| o.client_order_id)
            .collect();
        for inst in instruments {
            let symbol = inst.id.symbol().to_string();
            let open = client
                .signed(
                    Method::Get,
                    "/fapi/v1/openOrders",
                    &[("symbol", symbol.clone())],
                )
                .await?;
            let algos = client
                .signed(
                    Method::Get,
                    "/fapi/v1/openAlgoOrders",
                    &[("symbol", symbol.clone())],
                )
                .await?;
            // Regular orders by clientOrderId, untriggered stops by
            // clientAlgoId; both carry our prefix when they are ours.
            let regular = open
                .as_array()
                .ok_or_else(|| ExchangeError::Decode("openOrders".into()))?
                .iter()
                .map(|o| (o["clientOrderId"].as_str().unwrap_or_default(), false));
            let stops = algos
                .as_array()
                .ok_or_else(|| ExchangeError::Decode("openAlgoOrders".into()))?
                .iter()
                .map(|o| (o["clientAlgoId"].as_str().unwrap_or_default(), true));
            for (cid, algo) in regular.chain(stops).collect::<Vec<_>>() {
                if !cid.starts_with(CLIENT_ID_PREFIX) || known.contains(cid) {
                    continue;
                }
                match orphans {
                    Orphans::Refuse => {
                        return Err(ExchangeError::Decode(format!(
                            "{}: open order {cid} is this bot's but not in these books \
                             (another database or instance?); cancel it or run with --cancel-orphans",
                            inst.id
                        )));
                    }
                    Orphans::Cancel => {
                        let (path, params) = if algo {
                            ("/fapi/v1/algoOrder", vec![("clientAlgoId", cid.to_owned())])
                        } else {
                            (
                                "/fapi/v1/order",
                                vec![
                                    ("symbol", symbol.clone()),
                                    ("origClientOrderId", cid.to_owned()),
                                ],
                            )
                        };
                        client.signed(Method::Delete, path, &params).await?;
                        tracing::warn!(instrument = %inst.id, order = cid, "cancelled an orphan order");
                    }
                }
            }
            let held = books
                .get_holding(&inst.id)
                .await?
                .unwrap_or_else(|| Position::flat(inst.id.clone()));
            let since = Timestamp::from_millis(now_millis() - STARTUP_LOOKBACK_MS);
            let (adopted, _) = align(client, books, inst, held, &HashSet::new(), since).await?;
            done.fills_recovered += adopted.len();
            done.fills.extend(adopted);
        }
    }
    Ok(done)
}

/// The algo order with client id `cid`, if there is one (Binance answers
/// an unknown client id with `-2013`, checked on demo 2026-11-18).
async fn find_algo(client: &SignedClient, cid: &str) -> Result<Option<Value>, ExchangeError> {
    match client
        .signed(
            Method::Get,
            "/fapi/v1/algoOrder",
            &[("clientAlgoId", cid.to_owned())],
        )
        .await
    {
        Ok(v) if v["algoId"].is_i64() => Ok(Some(v)),
        Ok(_) => Err(ExchangeError::Decode(
            "algoOrder reply without algoId".into(),
        )),
        Err(ExchangeError::Venue { code, .. }) if code == NO_SUCH_ORDER => Ok(None),
        Err(e) => Err(e),
    }
}

/// Cancels an untriggered algo order and waits for it to end. Returns
/// the order it triggered into, if it did (before or during the cancel).
async fn settle_algo(
    client: &SignedClient,
    algo: &Value,
    retry_delay: Duration,
) -> Result<Option<i64>, ExchangeError> {
    let algo_id = algo["algoId"]
        .as_i64()
        .ok_or_else(|| ExchangeError::Decode("algoId".into()))?;
    let mut algo = algo.clone();
    for attempt in 0..10 {
        if let Some(id) = actual_order_id(&algo)? {
            return Ok(Some(id));
        }
        if algo_ended(&algo) {
            return Ok(None);
        }
        if attempt == 0 {
            match client
                .signed(
                    Method::Delete,
                    "/fapi/v1/algoOrder",
                    &[("algoId", algo_id.to_string())],
                )
                .await
            {
                Ok(_) | Err(ExchangeError::Venue { .. }) => {}
                Err(e) => return Err(e),
            }
        } else {
            tokio::time::sleep(retry_delay).await;
        }
        algo = client
            .signed(
                Method::Get,
                "/fapi/v1/algoOrder",
                &[("algoId", algo_id.to_string())],
            )
            .await?;
    }
    Err(ExchangeError::Decode(format!(
        "algo order {algo_id} didn't end on Binance; retry later"
    )))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// How a USDⓈ-M order run connects and margins.
#[derive(Clone, Debug)]
pub struct UsdmConfig {
    pub network: Network,
    pub interval: bot_core::Interval,
    pub warmup: usize,
    pub max_outage: Duration,
    pub http_timeout: Duration,
    pub leverage: u32,
    pub mode: MarginMode,
}

/// Binance USDⓈ-M with real (or testnet/demo) orders: futures klines and
/// funding of `config.network` as the feed, [`UsdmBroker`] as the broker.
pub struct UsdmProvider {
    config: UsdmConfig,
    creds: Credentials,
    universe: Vec<InstrumentId>,
    books: Arc<dyn Books>,
    run_tag: String,
    orphans: Orphans,
    limiter: Arc<RateLimiter>,
}

impl UsdmProvider {
    pub fn new(
        config: UsdmConfig,
        creds: Credentials,
        universe: Vec<InstrumentId>,
        books: Arc<dyn Books>,
    ) -> Self {
        Self {
            config,
            creds,
            universe,
            books,
            run_tag: super::run_tag(now_millis()),
            orphans: Orphans::Refuse,
            limiter: Arc::new(RateLimiter::new(super::limits::default_limits())),
        }
    }

    pub fn with_orphans(mut self, orphans: Orphans) -> Self {
        self.orphans = orphans;
        self
    }

    pub fn run_tag(&self) -> &str {
        &self.run_tag
    }

    fn public(&self) -> PublicClient {
        let mut c =
            PublicClient::usdm(rest_url(self.config.network)).with_limiter(self.limiter.clone());
        c.set_timeout(self.config.http_timeout);
        c
    }

    async fn client(&self) -> Result<SignedClient, ExchangeError> {
        let mut c = SignedClient::new(
            rest_url(self.config.network),
            Box::new(ReqwestTransport::new(self.config.http_timeout)?),
            self.creds.clone(),
            Box::new(SystemClock),
        )
        .with_limiter(self.limiter.clone());
        c.sync_time_at("/fapi/v1/time").await?;
        Ok(c)
    }

    async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
        let public = self.public();
        let mut out = Vec::with_capacity(self.universe.len());
        for id in &self.universe {
            out.push(public.perp_instrument(id.symbol()).await?.instrument);
        }
        Ok(out)
    }

    /// Cancels every working order of ours and records its last fills;
    /// positions stay open. Call it however a run ends.
    pub async fn close_out(&self) -> Result<Reconciled, ExchangeError> {
        let client = self.client().await?;
        let instruments = self.instruments().await?;
        reconcile(
            &client,
            &instruments,
            &*self.books,
            Phase::Exit,
            Duration::from_millis(500),
        )
        .await
    }
}

#[async_trait]
impl Provider for UsdmProvider {
    fn id(&self) -> &str {
        "binance-usdm"
    }

    fn asset_classes(&self) -> &[AssetClass] {
        &[AssetClass::Perpetual]
    }

    async fn market_data(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        let feed = ReconnectingFeed::new(
            Box::new(WsConnector {
                url: ws_url(self.config.network).to_owned(),
                ids: self.universe.clone(),
                interval: self.config.interval,
            }),
            Box::new(RestKlines {
                client: self.public(),
                interval: self.config.interval,
            }),
            self.universe.clone(),
            self.config.warmup,
            self.config.max_outage,
        );
        Ok(Box::new(FundingWatch::new(
            Box::new(feed),
            Box::new(self.public()),
            self.universe.clone(),
            Timestamp::from_millis(now_millis()),
            super::usdm_paper::FUNDING_POLL,
        )))
    }

    /// Sets the account up ([`prepare`]), reconciles, aligns, then opens
    /// the broker on the books.
    async fn broker(&self) -> Result<Box<dyn Broker>, ExchangeError> {
        let client = self.client().await?;
        let instruments = self.instruments().await?;
        prepare(
            &client,
            &instruments,
            self.config.leverage,
            self.config.mode,
        )
        .await?;
        let r = reconcile(
            &client,
            &instruments,
            &*self.books,
            Phase::Startup(self.orphans),
            Duration::from_millis(500),
        )
        .await?;
        tracing::info!(
            orders_closed = r.orders_closed,
            fills_recovered = r.fills_recovered,
            "reconciled with Binance futures"
        );
        let since = Timestamp::from_millis(now_millis());
        Ok(Box::new(
            UsdmBroker::open(
                client,
                instruments,
                self.run_tag.clone(),
                self.books.clone(),
                since,
            )
            .await?
            .with_recovered(r.fills),
        ))
    }

    async fn resolve(&self, query: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError> {
        let InstrumentQuery::Ids(ids) = query;
        let public = self.public();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if id.venue().as_str() != usdm::VENUE {
                return Err(ExchangeError::UnknownInstrument(id.clone()));
            }
            out.push(public.perp_instrument(id.symbol()).await?.instrument);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binance::signed::fake::{FakeTransport, FixedClock, param};
    use crate::test_db::TempDb;
    use bot_core::{Asset, CandleSpec, InstrumentSpec, Interval, OrderSpec};
    use rust_decimal_macros::dec;

    fn btc() -> Instrument {
        Instrument::new(InstrumentSpec {
            id: "binance-usdm:BTCUSDT".parse().unwrap(),
            class: AssetClass::Perpetual,
            base: Asset::new("BTC").unwrap(),
            quote: Asset::new("USDT").unwrap(),
            tick_size: Price::new(dec!(0.1)),
            lot_size: Qty::new(dec!(0.001)),
            min_qty: Qty::new(dec!(0.001)),
            min_notional: Notional::new(dec!(50)),
            price_band: None,
            max_open_orders: None,
            max_open_stops: None,
        })
        .unwrap()
    }

    fn client(t: &FakeTransport) -> SignedClient {
        SignedClient::new(
            TESTNET_URL,
            Box::new(t.clone()),
            Credentials::new("k", "s"),
            Box::new(FixedClock(1_700_000_000_000)),
        )
    }

    async fn broker(t: &FakeTransport, db: &TempDb) -> UsdmBroker {
        UsdmBroker::open(
            client(t),
            vec![btc()],
            "tbrun".into(),
            db.store.clone(),
            Timestamp::from_millis(1_000),
        )
        .await
        .unwrap()
    }

    fn order(id: &str, side: Side, size: Size, kind: OrderKind) -> Order {
        Order::new(OrderSpec {
            id: OrderId::new(id).unwrap(),
            instrument: btc().id.clone(),
            side,
            size,
            kind,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    fn candle(close: &str) -> MarketEvent {
        let p: Price = close.parse().unwrap();
        MarketEvent::Candle(
            bot_core::Candle::new(CandleSpec {
                instrument: btc().id.clone(),
                interval: Interval::H1,
                open_time: Timestamp::from_millis(0),
                close_time: Timestamp::from_millis(3_599_999),
                open: p,
                high: p,
                low: p,
                close: p,
                volume: Qty::new(dec!(1)),
                quote_volume: Notional::new(p.get()),
            })
            .unwrap(),
        )
    }

    fn trade(id: i64, order: i64, side: &str, price: &str, qty: &str, fee: &str) -> String {
        format!(
            r#"{{"symbol":"BTCUSDT","id":{id},"orderId":{order},"side":"{side}","price":"{price}","qty":"{qty}",
            "realizedPnl":"0","quoteQty":"0","commission":"{fee}","commissionAsset":"USDT","time":5000,
            "positionSide":"BOTH","buyer":{},"maker":false}}"#,
            side == "BUY"
        )
    }

    /// Market sell 2 from flat (fee 24 USDT): short 2 @ 30,000, entry
    /// notional −60,000, realized −24. At the funding event Binance's
    /// income history lists +6.1 (tranId 501, from the run's start: the
    /// symbol was never held) → −17.9. A candle with Binance agreeing (−2)
    /// asks nothing more. A restart keeps both the short and the funding;
    /// re-reading the same income (7-day look-back, now that it is held)
    /// books nothing twice.
    #[tokio::test]
    async fn a_short_round_trip_lands_in_the_books() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        let mut b = broker(&t, &db).await;
        t.reply(200, r#"{"orderId":77}"#);
        let sell = order(
            "o-1",
            Side::Sell,
            Size::Base(Qty::new(dec!(2))),
            OrderKind::Market,
        );
        b.submit(sell).await.unwrap();
        let req = &t.sent()[0];
        assert_eq!(
            (req.method, req.path.as_str()),
            (Method::Post, "/fapi/v1/order")
        );
        let (unsigned, _) = req.query.rsplit_once("&signature=").unwrap();
        assert!(unsigned.starts_with(
            "symbol=BTCUSDT&side=SELL&type=MARKET&quantity=2&newClientOrderId=tbrun-o-1&newOrderRespType=ACK"
        ));
        t.reply(
            200,
            r#"{"orderId":77,"status":"FILLED","executedQty":"2.000"}"#,
        );
        t.reply(
            200,
            &format!("[{}]", trade(9, 77, "SELL", "30000", "2.000", "24")),
        );
        let funding = MarketEvent::Funding {
            instrument: btc().id.clone(),
            rate: dec!(0.0001),
            mark: Price::new(dec!(30500)),
            ts: Timestamp::from_millis(6000),
        };
        t.reply(
            200,
            r#"[{"symbol":"BTCUSDT","incomeType":"FUNDING_FEE","income":"6.10000000","asset":"USDT","time":6000,"info":"FUNDING_FEE","tranId":501,"tradeId":""}]"#,
        );
        let fills = b.settle(&funding).await.unwrap();
        assert_eq!(fills.len(), 1);
        let income = t.sent().last().unwrap().clone();
        assert_eq!(income.path, "/fapi/v1/income");
        assert_eq!(param(&income.query, "startTime"), Some("1000"));
        let p = b.position(&btc().id);
        assert_eq!(
            (p.qty().get(), p.cost_basis().get(), p.realized_pnl().get()),
            (dec!(-2), dec!(-60000), dec!(-17.9))
        );
        t.reply(200, r#"[{"symbol":"BTCUSDT","positionAmt":"-2.000"}]"#);
        assert!(b.settle(&candle("30100")).await.unwrap().is_empty());
        assert_eq!(t.sent().last().unwrap().path, "/fapi/v3/positionRisk");
        let mut again = broker(&t, &db).await;
        let p = again.position(&btc().id);
        assert_eq!(
            (p.qty().get(), p.realized_pnl().get()),
            (dec!(-2), dec!(-17.9))
        );
        t.reply(
            200,
            r#"[{"symbol":"BTCUSDT","incomeType":"FUNDING_FEE","income":"6.10000000","asset":"USDT","time":6000,"tranId":501}]"#,
        );
        again.settle(&funding).await.unwrap();
        assert_eq!(
            param(&t.sent().last().unwrap().query, "startTime"),
            Some(&*(1_000 - 7 * 86_400_000_i64).to_string())
        );
        assert_eq!(again.position(&btc().id).realized_pnl().get(), dec!(-17.9));
    }

    /// Short 2 @ 30,000 in the books; Binance now says flat and lists a
    /// BUY 2 @ 31,000 (fee 31) from order 999, not ours: the liquidation
    /// is adopted as fill `venue-999`; realized −24 − 2,000 − 31 = −2,055.
    /// A difference no trade explains, with nothing of ours working,
    /// stops the run.
    #[tokio::test]
    async fn the_venues_own_trades_are_adopted_and_unexplained_drift_stops() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        let mut b = broker(&t, &db).await;
        t.reply(200, r#"{"orderId":77}"#);
        b.submit(order(
            "o-1",
            Side::Sell,
            Size::Base(Qty::new(dec!(2))),
            OrderKind::Market,
        ))
        .await
        .unwrap();
        t.reply(200, r#"{"orderId":77,"status":"FILLED","executedQty":"2"}"#);
        t.reply(
            200,
            &format!("[{}]", trade(9, 77, "SELL", "30000", "2", "24")),
        );
        t.reply(200, r#"[{"symbol":"BTCUSDT","positionAmt":"-2"}]"#);
        b.settle(&candle("30000")).await.unwrap();
        t.reply(200, r#"[]"#);
        t.reply(
            200,
            &format!("[{}]", trade(10, 999, "BUY", "31000", "2", "31")),
        );
        let fills = b.settle(&candle("31000")).await.unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].order_id.as_str(), "venue-999");
        let user_trades = t.sent().last().unwrap().clone();
        assert_eq!(param(&user_trades.query, "startTime"), Some("1000"));
        let p = b.position(&btc().id);
        assert!(p.is_flat());
        assert_eq!(p.realized_pnl().get(), dec!(-2055));
        t.reply(200, r#"[{"symbol":"BTCUSDT","positionAmt":"0.5"}]"#);
        t.reply(200, r#"[]"#);
        assert!(b.settle(&candle("31000")).await.is_err());
    }

    /// A quote budget before any close is refused without a request.
    /// After a close of 30,000, 1,000 USDT is sent as 0.033 BTC (lot
    /// 0.001, rounded down).
    #[tokio::test]
    async fn quote_budgets_are_sized_at_the_last_close() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        let mut b = broker(&t, &db).await;
        let budget = Size::Quote(Notional::new(dec!(1000)));
        assert!(
            b.submit(order("o-2", Side::Buy, budget, OrderKind::Market))
                .await
                .is_err()
        );
        assert!(t.sent().is_empty());
        t.reply(200, r#"[{"symbol":"BTCUSDT","positionAmt":"0"}]"#);
        b.settle(&candle("30000")).await.unwrap();
        t.reply(200, r#"{"orderId":5}"#);
        b.submit(order("o-3", Side::Buy, budget, OrderKind::Market))
            .await
            .unwrap();
        assert_eq!(
            param(&t.sent().last().unwrap().query, "quantity"),
            Some("0.033")
        );
    }

    /// A stop goes to the algo service (`STOP_MARKET`, `triggerPrice`,
    /// last-price trigger, `clientAlgoId` = our client id). While `NEW`
    /// nothing else is asked; once triggered, its `actualOrderId` (a
    /// string) is polled as a normal order: FILLED 1 @ 31,005 → one fill.
    /// Cancelling an untriggered stop deletes the algo order.
    #[tokio::test]
    async fn stops_live_as_algo_orders_until_they_trigger() {
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        let mut b = broker(&t, &db).await;
        let stop = OrderKind::StopMarket {
            stop: Price::new(dec!(31000)),
        };
        t.reply(200, r#"{"algoId":1000000291115025,"algoStatus":"NEW"}"#);
        b.submit(order("o-1", Side::Buy, Size::Base(Qty::new(dec!(1))), stop))
            .await
            .unwrap();
        let req = t.sent()[0].clone();
        assert_eq!(req.path, "/fapi/v1/algoOrder");
        for (k, v) in [
            ("algoType", "CONDITIONAL"),
            ("type", "STOP_MARKET"),
            ("triggerPrice", "31000"),
            ("quantity", "1"),
            ("workingType", "CONTRACT_PRICE"),
            ("clientAlgoId", "tbrun-o-1"),
        ] {
            assert_eq!(param(&req.query, k), Some(v), "{k}");
        }
        t.reply(
            200,
            r#"{"algoId":1000000291115025,"algoStatus":"NEW","actualOrderId":""}"#,
        );
        t.reply(200, r#"[{"symbol":"BTCUSDT","positionAmt":"0"}]"#);
        assert!(b.settle(&candle("30500")).await.unwrap().is_empty());
        t.reply(
            200,
            r#"{"algoId":1000000291115025,"algoStatus":"TRIGGERED","actualOrderId":"555"}"#,
        );
        t.reply(
            200,
            r#"{"orderId":555,"status":"FILLED","executedQty":"1"}"#,
        );
        t.reply(
            200,
            &format!("[{}]", trade(9, 555, "BUY", "31005", "1", "12.402")),
        );
        t.reply(200, r#"[{"symbol":"BTCUSDT","positionAmt":"1"}]"#);
        let fills = b.settle(&candle("31200")).await.unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(param(&t.sent()[4].query, "orderId"), Some("555"));
        assert_eq!(b.position(&btc().id).qty().get(), dec!(1));
        assert!(b.open_orders().is_empty());
        t.reply(200, r#"{"algoId":7,"algoStatus":"NEW"}"#);
        b.submit(order(
            "o-2",
            Side::Sell,
            Size::Base(Qty::new(dec!(1))),
            OrderKind::StopMarket {
                stop: Price::new(dec!(29000)),
            },
        ))
        .await
        .unwrap();
        t.reply(200, r#"{"algoId":7,"code":"200","msg":"success"}"#);
        b.cancel(OrderId::new("o-2").unwrap()).await.unwrap();
        let del = t.sent().last().unwrap().clone();
        assert_eq!(
            (del.method, del.path.as_str()),
            (Method::Delete, "/fapi/v1/algoOrder")
        );
        assert_eq!(param(&del.query, "algoId"), Some("7"));
    }

    /// Hedge mode is refused; "no need to change" margin type is fine;
    /// the leverage Binance confirms must be the one asked.
    #[tokio::test]
    async fn prepare_checks_one_way_mode_and_sets_margin_and_leverage() {
        let t = FakeTransport::default();
        t.reply(200, r#"{"dualSidePosition":true}"#);
        assert!(
            prepare(&client(&t), &[btc()], 3, MarginMode::Isolated)
                .await
                .is_err()
        );
        let t = FakeTransport::default();
        t.reply(200, r#"{"dualSidePosition":false}"#);
        t.reply(
            400,
            r#"{"code":-4046,"msg":"No need to change margin type."}"#,
        );
        t.reply(
            200,
            r#"{"leverage":3,"maxNotionalValue":"100000000","symbol":"BTCUSDT"}"#,
        );
        prepare(&client(&t), &[btc()], 3, MarginMode::Cross)
            .await
            .unwrap();
        let sent = t.sent();
        assert_eq!(param(&sent[1].query, "marginType"), Some("CROSSED"));
        assert_eq!(param(&sent[2].query, "leverage"), Some("3"));
        let t = FakeTransport::default();
        t.reply(200, r#"{"dualSidePosition":false}"#);
        t.reply(200, r#"{"code":200,"msg":"success"}"#);
        t.reply(200, r#"{"leverage":20}"#);
        assert!(
            prepare(&client(&t), &[btc()], 3, MarginMode::Isolated)
                .await
                .is_err()
        );
    }

    /// A pending journal row (submit reply lost) is found by client id:
    /// FILLED 1 @ 30,000 → its trade is recorded, the row closed, the fill
    /// recovered. Then an open `tb…` order the books don't know refuses
    /// startup; with `Orphans::Cancel` it is cancelled and the position
    /// (long 1, as Binance agrees) is aligned.
    #[tokio::test]
    async fn startup_recovers_lost_orders_and_refuses_orphans() {
        use storage::OrderJournal;
        let db = TempDb::new().await;
        db.store
            .insert_pending("tbold-o-1", &btc().id, Side::Buy, Timestamp::from_millis(0))
            .await
            .unwrap();
        let script = |t: &FakeTransport| {
            t.reply(400, r#"{"code":-2013,"msg":"Order does not exist."}"#);
            t.reply(200, r#"{"orderId":77,"status":"FILLED","executedQty":"1"}"#);
            t.reply(
                200,
                &format!("[{}]", trade(9, 77, "BUY", "30000", "1", "12")),
            );
            t.reply(200, r#"[{"clientOrderId":"tbelse-o-4","orderId":80}]"#);
            t.reply(200, "[]");
        };
        let t = FakeTransport::default();
        script(&t);
        let err = reconcile(
            &client(&t),
            &[btc()],
            &*db.store,
            Phase::Startup(Orphans::Refuse),
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--cancel-orphans"), "{err}");
        assert_eq!(
            param(&t.sent()[1].query, "origClientOrderId"),
            Some("tbold-o-1")
        );
        assert!(db.store.unfinished_orders().await.unwrap().is_empty());
        let t = FakeTransport::default();
        t.reply(200, r#"[{"clientOrderId":"tbelse-o-4","orderId":80}]"#);
        t.reply(200, "[]");
        t.reply(200, r#"{"orderId":80}"#);
        t.reply(200, r#"[{"symbol":"BTCUSDT","positionAmt":"1"}]"#);
        let r = reconcile(
            &client(&t),
            &[btc()],
            &*db.store,
            Phase::Startup(Orphans::Cancel),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(r.fills_recovered, 0);
        assert_eq!(t.sent()[2].method, Method::Delete);
        let b = broker(&t, &db).await;
        assert_eq!(b.position(&btc().id).qty().get(), dec!(1));
    }

    /// A stop journaled before a crash, still untriggered: found as an
    /// algo order by client id, cancelled, re-read until it ended
    /// (CANCELED, no `actualOrderId`), and its row closed with no fill.
    #[tokio::test]
    async fn exit_cancels_an_untriggered_stop() {
        use storage::OrderJournal;
        let db = TempDb::new().await;
        db.store
            .insert_pending(
                "tbold-o-2",
                &btc().id,
                Side::Sell,
                Timestamp::from_millis(0),
            )
            .await
            .unwrap();
        db.store.mark_open("tbold-o-2", 7).await.unwrap();
        let t = FakeTransport::default();
        t.reply(200, r#"{"algoId":7,"algoStatus":"NEW","actualOrderId":""}"#);
        t.reply(200, r#"{"algoId":7,"code":"200","msg":"success"}"#);
        t.reply(
            200,
            r#"{"algoId":7,"algoStatus":"CANCELED","actualOrderId":""}"#,
        );
        let r = reconcile(
            &client(&t),
            &[btc()],
            &*db.store,
            Phase::Exit,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!((r.orders_closed, r.fills_recovered), (1, 0));
        let sent = t.sent();
        assert_eq!(param(&sent[0].query, "clientAlgoId"), Some("tbold-o-2"));
        assert_eq!(
            (sent[1].method, sent[1].path.as_str()),
            (Method::Delete, "/fapi/v1/algoOrder")
        );
        assert!(db.store.unfinished_orders().await.unwrap().is_empty());
    }

    /// A reduce-only order says so to Binance, regular or stop; a plain
    /// one doesn't. Locally, a reduce-only buy with nothing short is
    /// refused before any request.
    #[tokio::test]
    async fn reduce_only_is_sent_and_checked() {
        let ro = order(
            "o-1",
            Side::Sell,
            Size::Base(Qty::new(dec!(1))),
            OrderKind::Market,
        )
        .with_reduce_only()
        .unwrap();
        let p = order_params(&ro, Qty::new(dec!(1)), "c".into()).unwrap();
        assert!(p.contains(&("reduceOnly", "true".to_owned())));
        let stop = Order::new(OrderSpec {
            kind: OrderKind::StopMarket {
                stop: Price::new(dec!(29000)),
            },
            ..ro.clone().into()
        })
        .unwrap()
        .with_reduce_only()
        .unwrap();
        assert!(
            algo_params(&stop, Qty::new(dec!(1)), "c".into())
                .contains(&("reduceOnly", "true".to_owned()))
        );
        let plain = order(
            "o-2",
            Side::Sell,
            Size::Base(Qty::new(dec!(1))),
            OrderKind::Market,
        );
        assert!(
            !order_params(&plain, Qty::new(dec!(1)), "c".into())
                .unwrap()
                .iter()
                .any(|(k, _)| *k == "reduceOnly")
        );
        let (t, db) = (FakeTransport::default(), TempDb::new().await);
        let mut b = broker(&t, &db).await;
        let buy = order(
            "o-3",
            Side::Buy,
            Size::Base(Qty::new(dec!(1))),
            OrderKind::Market,
        )
        .with_reduce_only()
        .unwrap();
        assert!(b.submit(buy).await.is_err());
        assert!(t.sent().is_empty());
    }
}
