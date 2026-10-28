//! Read-only HTTP API over a running bot: its live state and its books.
//!
//! Routes, all `GET`, all JSON (money as decimal strings, invariant 1):
//!
//! | Route | Returns |
//! |---|---|
//! | `/status` | network, strategy, start time, and the latest engine [`Snapshot`] (`null` before the first event) |
//! | `/positions` | the books' holdings |
//! | `/open-orders` | the broker's working orders, as of the latest snapshot |
//! | `/orders?limit=N` | the latest journaled orders, newest first |
//! | `/trades?limit=N` | the latest fills, newest first |
//!
//! There is deliberately no route that places, cancels or changes
//! anything: orders come only from the strategy through risk, and nothing
//! here can loosen the live gate. Live state arrives over a `watch`
//! channel, so the engine never waits for a reader. When a token is
//! configured, every request needs `Authorization: Bearer <token>`.

use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bot_core::{Fill, InstrumentId, Side, Timestamp};
use engine::Snapshot;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use storage::{HoldingRepository, JournalOrder, OrderJournal, OrderStatus, TradeRepository};
use tokio::sync::watch;

/// Most rows one `/orders` or `/trades` request may ask for.
pub const MAX_LIMIT: u64 = 500;
const DEFAULT_LIMIT: u64 = 50;

/// The books the API reads. Any type with all three (`storage::SqlStore`)
/// qualifies.
pub trait ApiBooks: HoldingRepository + TradeRepository + OrderJournal {}
impl<T: HoldingRepository + TradeRepository + OrderJournal + ?Sized> ApiBooks for T {}

/// Facts about the run that don't change while it lasts.
#[derive(Clone, Debug, Serialize)]
pub struct RunInfo {
    /// `testnet`, `demo` or `live`.
    pub network: String,
    pub strategy: String,
    pub started_at: Timestamp,
}

/// Everything the handlers read.
#[derive(Clone)]
pub struct ApiState {
    pub info: RunInfo,
    /// Latest engine snapshot; `None` until the first event.
    pub live: watch::Receiver<Option<Snapshot>>,
    pub books: Arc<dyn ApiBooks>,
    /// Required bearer token, if any.
    pub token: Option<String>,
}

/// Binds `listen` and serves the API in the background. Returns the bound
/// address (useful with port 0) once listening.
pub async fn serve(listen: SocketAddr, state: ApiState) -> std::io::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;
    let app = router(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(error = %e, "API server stopped; trading continues");
        }
    });
    Ok(addr)
}

fn router(state: ApiState) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/positions", get(positions))
        .route("/open-orders", get(open_orders))
        .route("/orders", get(orders))
        .route("/trades", get(trades))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

async fn require_token(State(s): State<ApiState>, req: Request, next: Next) -> Response {
    if let Some(token) = &s.token {
        let given = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if !given.is_some_and(|g| constant_time_eq(g.as_bytes(), token.as_bytes())) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorBody::new("missing or wrong token")),
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// Compares without stopping at the first difference, so response time
/// doesn't reveal how much of a guessed token was right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl ErrorBody {
    fn new(e: impl ToString) -> Self {
        Self {
            error: e.to_string(),
        }
    }
}

/// A storage failure as a 500. Storage errors name tables and rules, never
/// credentials.
fn internal(e: storage::StorageError) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorBody::new(e))).into_response()
}

#[derive(Serialize)]
struct StatusBody {
    #[serde(flatten)]
    info: RunInfo,
    live: Option<Snapshot>,
}

async fn status(State(s): State<ApiState>) -> Json<StatusBody> {
    Json(StatusBody {
        info: s.info.clone(),
        live: s.live.borrow().clone(),
    })
}

async fn positions(State(s): State<ApiState>) -> Response {
    match s.books.list_holdings().await {
        Ok(p) => Json(p).into_response(),
        Err(e) => internal(e),
    }
}

async fn open_orders(State(s): State<ApiState>) -> Json<Vec<bot_core::Order>> {
    Json(
        s.live
            .borrow()
            .as_ref()
            .map(|snap| snap.open_orders.clone())
            .unwrap_or_default(),
    )
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<u64>,
}

impl Limit {
    fn get(&self) -> u64 {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }
}

/// A journaled order as the API shows it.
#[derive(Serialize)]
struct OrderBody {
    client_order_id: String,
    instrument: InstrumentId,
    side: Side,
    status: &'static str,
    venue_order_id: Option<i64>,
    created_at: Timestamp,
}

impl From<JournalOrder> for OrderBody {
    fn from(o: JournalOrder) -> Self {
        Self {
            client_order_id: o.client_order_id,
            instrument: o.instrument,
            side: o.side,
            status: match o.status {
                OrderStatus::Pending => "pending",
                OrderStatus::Open => "open",
                OrderStatus::Closed => "closed",
            },
            venue_order_id: o.venue_order_id,
            created_at: o.created_at,
        }
    }
}

async fn orders(State(s): State<ApiState>, Query(q): Query<Limit>) -> Response {
    match s.books.recent_orders(q.get()).await {
        Ok(v) => Json(v.into_iter().map(OrderBody::from).collect::<Vec<_>>()).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Serialize)]
struct TradeBody {
    id: i64,
    #[serde(flatten)]
    fill: Fill,
    /// The journal's run-tagged order id, matching `/orders`; `null` for
    /// paper fills and fills recorded before it was kept. `order_id` (from
    /// the fill) restarts at `o-1` every run.
    client_order_id: Option<String>,
}

async fn trades(State(s): State<ApiState>, Query(q): Query<Limit>) -> Response {
    match s.books.recent_trades(q.get()).await {
        Ok(v) => Json(
            v.into_iter()
                .map(|t| TradeBody {
                    id: t.id.get(),
                    fill: t.fill,
                    client_order_id: t.client_order_id,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::{Fee, FillSpec, Liquidity, Notional, OrderId, Price, Qty};
    use rust_decimal_macros::dec;
    use serde_json::Value;
    use storage::{Ledger, SqlStore};

    struct Db {
        store: Arc<SqlStore>,
        path: std::path::PathBuf,
    }

    impl Drop for Db {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    async fn db() -> Db {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("trade-bot-api-{}-{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = SqlStore::open(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        Db {
            store: Arc::new(store),
            path,
        }
    }

    fn btcusdt() -> bot_core::Instrument {
        bot_core::Instrument::new(bot_core::InstrumentSpec {
            id: "binance:BTCUSDT".parse().unwrap(),
            class: bot_core::AssetClass::Spot,
            base: bot_core::Asset::new("BTC").unwrap(),
            quote: bot_core::Asset::new("USDT").unwrap(),
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

    /// Serves the API over `db` on a free loopback port; returns its base
    /// URL and the sender for live snapshots.
    async fn start(db: &Db, token: Option<&str>) -> (String, watch::Sender<Option<Snapshot>>) {
        let (tx, rx) = watch::channel(None);
        let state = ApiState {
            info: RunInfo {
                network: "demo".into(),
                strategy: "sma_trend".into(),
                started_at: Timestamp::from_millis(1_000),
            },
            live: rx,
            books: db.store.clone(),
            token: token.map(str::to_owned),
        };
        let addr = serve("127.0.0.1:0".parse().unwrap(), state).await.unwrap();
        (format!("http://{addr}"), tx)
    }

    async fn get(url: &str) -> (u16, Value) {
        let r = reqwest::get(url).await.unwrap();
        let code = r.status().as_u16();
        (code, r.json().await.unwrap())
    }

    #[tokio::test]
    async fn status_shows_run_info_then_live_state() {
        let db = db().await;
        let (base, tx) = start(&db, None).await;
        let (code, body) = get(&format!("{base}/status")).await;
        assert_eq!(code, 200);
        assert_eq!(body["network"], "demo");
        assert_eq!(body["strategy"], "sma_trend");
        assert!(body["live"].is_null(), "no event yet");

        tx.send_replace(Some(Snapshot {
            at: Some(Timestamp::from_millis(7_200_000)),
            events: 2,
            ..Snapshot::default()
        }));
        let (_, body) = get(&format!("{base}/status")).await;
        assert_eq!(body["live"]["events"], 2);
    }

    /// Money comes back as exact decimal strings, never JSON numbers.
    #[tokio::test]
    async fn positions_and_trades_come_from_the_books() {
        let db = db().await;
        let fill = bot_core::Fill::new(FillSpec {
            order_id: OrderId::new("o-1").unwrap(),
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            qty: Qty::new(dec!(9)),
            price: Price::new(dec!(110.11)),
            fee: Fee::Base(Qty::new(dec!(0.009))),
            liquidity: Liquidity::Taker,
            ts: Timestamp::from_millis(10_800_000),
        })
        .unwrap();
        db.store
            .record_venue_fill(&fill, &btcusdt(), "7", "tbx-o-1")
            .await
            .unwrap();
        let (base, _tx) = start(&db, None).await;

        let (code, positions) = get(&format!("{base}/positions")).await;
        assert_eq!(code, 200);
        assert_eq!(positions[0]["instrument"], "binance:BTCUSDT");
        assert_eq!(positions[0]["qty"], "8.991");
        assert_eq!(positions[0]["cost_basis"], "990.99");

        let (_, trades) = get(&format!("{base}/trades?limit=10")).await;
        assert_eq!(trades.as_array().unwrap().len(), 1);
        assert_eq!(trades[0]["price"], "110.11");
        assert_eq!(trades[0]["client_order_id"], "tbx-o-1");
        assert!(trades[0]["id"].is_number());
    }

    #[tokio::test]
    async fn orders_and_open_orders() {
        let db = db().await;
        db.store
            .insert_pending(
                "tbx-o-1",
                &btcusdt().id,
                Side::Buy,
                Timestamp::from_millis(5),
            )
            .await
            .unwrap();
        db.store.mark_open("tbx-o-1", 28).await.unwrap();
        let (base, _tx) = start(&db, None).await;
        let (_, orders) = get(&format!("{base}/orders")).await;
        assert_eq!(orders[0]["client_order_id"], "tbx-o-1");
        assert_eq!(orders[0]["status"], "open");
        assert_eq!(orders[0]["venue_order_id"], 28);
        let (_, open) = get(&format!("{base}/open-orders")).await;
        assert_eq!(open, serde_json::json!([]), "no snapshot yet");
    }

    #[tokio::test]
    async fn token_is_enforced_when_set() {
        let db = db().await;
        let (base, _tx) = start(&db, Some("s3cret")).await;
        let client = reqwest::Client::new();
        let status = |auth: Option<&'static str>| {
            let mut r = client.get(format!("{base}/status"));
            if let Some(a) = auth {
                r = r.header("Authorization", a);
            }
            async move { r.send().await.unwrap().status().as_u16() }
        };
        assert_eq!(status(None).await, 401);
        assert_eq!(status(Some("Bearer wrong")).await, 401);
        assert_eq!(status(Some("Bearer s3cret")).await, 200);
    }

    /// Read-only: anything but GET is refused, and there is no route that
    /// could place or cancel an order.
    #[tokio::test]
    async fn nothing_can_be_changed() {
        let db = db().await;
        let (base, _tx) = start(&db, None).await;
        let client = reqwest::Client::new();
        for path in ["/orders", "/positions", "/open-orders"] {
            let code = client
                .post(format!("{base}{path}"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16();
            assert_eq!(code, 405, "{path}");
        }
        let code = client
            .post(format!("{base}/order"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        assert_eq!(code, 404);
    }

    #[test]
    fn limit_is_clamped() {
        assert_eq!(Limit { limit: None }.get(), DEFAULT_LIMIT);
        assert_eq!(Limit { limit: Some(0) }.get(), 1);
        assert_eq!(
            Limit {
                limit: Some(10_000)
            }
            .get(),
            MAX_LIMIT
        );
    }
}
