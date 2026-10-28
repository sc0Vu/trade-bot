use crate::decimal::DbDecimal;
use crate::entity::{holding, meta, order, strategy_state, trade};
use crate::{
    HoldingRepository, JournalOrder, Ledger, OrderJournal, OrderStatus, SavedState, StorageError,
    StoredTrade, StrategyStateRepository, TradeId, TradeRepository,
};
use async_trait::async_trait;
use bot_core::{
    Asset, Fee, Fill, FillSpec, Instrument, InstrumentId, Liquidity, Notional, OrderId, Position,
    Price, Qty, Side, Timestamp,
};
use migration::{Migrator, MigratorTrait};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::{NotSet, Set},
    ColumnTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, TransactionTrait,
};

/// SeaORM-backed implementation of every repository trait. Works on any
/// backend SeaORM supports here (SQLite, MySQL) with the same code.
pub struct SqlStore {
    db: DatabaseConnection,
}

impl SqlStore {
    /// Connects to `url` (`sqlite://bot.db?mode=rwc`, `mysql://...`). The
    /// schema must already be migrated (`sea-orm-cli migrate up`).
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let db = Database::connect(url)
            .await
            .map_err(StorageError::backend)?;
        Ok(Self { db })
    }

    /// Connects to `url` and applies any pending migrations, so a fresh
    /// per-network database is usable at once. Migrations are versioned
    /// code (`migration/`), so this is the same schema `sea-orm-cli
    /// migrate up` would build.
    pub async fn open(url: &str) -> Result<Self, StorageError> {
        let store = Self::connect(url).await?;
        Migrator::up(&store.db, None)
            .await
            .map_err(StorageError::backend)?;
        Ok(store)
    }

    /// Ties this database to `network` (`testnet`, `demo`, `live`): the
    /// first call stamps it, later calls must name the same network or get
    /// [`StorageError::WrongNetwork`]. Holdings from fake-money fills must
    /// never be read as real ones.
    pub async fn bind_network(&self, network: &str) -> Result<(), StorageError> {
        let found = meta::Entity::find_by_id(NETWORK_KEY)
            .one(&self.db)
            .await
            .map_err(StorageError::backend)?;
        match found {
            Some(row) if row.value == network => Ok(()),
            Some(row) => Err(StorageError::WrongNetwork {
                wanted: network.to_owned(),
                found: row.value,
            }),
            None => {
                meta::Entity::insert(meta::ActiveModel {
                    key: Set(NETWORK_KEY.to_owned()),
                    value: Set(network.to_owned()),
                })
                .exec_without_returning(&self.db)
                .await
                .map_err(StorageError::backend)?;
                Ok(())
            }
        }
    }
}

const TRADES: &str = "trades";
const HOLDINGS: &str = "holdings";
const ORDERS: &str = "orders";
const NETWORK_KEY: &str = "network";

fn side_text(s: Side) -> &'static str {
    match s {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn liquidity_text(l: Liquidity) -> &'static str {
    match l {
        Liquidity::Maker => "maker",
        Liquidity::Taker => "taker",
    }
}

/// Where a venue fill came from: the venue's trade id and the journal's
/// client order id. `None` for a paper fill.
type VenueRef<'a> = Option<(&'a str, &'a str)>;

fn to_trade_row(f: &Fill, venue: VenueRef<'_>) -> trade::ActiveModel {
    let (kind, amount, asset, value) = match &f.fee {
        Fee::Base(q) => ("base", q.get(), None, None),
        Fee::Quote(n) => ("quote", n.get(), None, None),
        Fee::Other {
            asset,
            amount,
            value,
        } => (
            "other",
            amount.get(),
            Some(asset.to_string()),
            value.map(|v| DbDecimal(v.get())),
        ),
    };
    trade::ActiveModel {
        id: NotSet,
        order_id: Set(f.order_id.to_string()),
        instrument: Set(f.instrument.to_string()),
        side: Set(side_text(f.side).to_owned()),
        qty: Set(DbDecimal(f.qty.get())),
        price: Set(DbDecimal(f.price.get())),
        fee_kind: Set(kind.to_owned()),
        fee_amount: Set(DbDecimal(amount)),
        fee_asset: Set(asset),
        fee_value: Set(value),
        liquidity: Set(liquidity_text(f.liquidity).to_owned()),
        ts: Set(f.ts.as_millis()),
        venue_trade_id: Set(venue.map(|(t, _)| t.to_owned())),
        client_order_id: Set(venue.map(|(_, c)| c.to_owned())),
    }
}

fn from_trade_row(m: trade::Model) -> Result<(TradeId, Fill), StorageError> {
    let bad = |reason: &str| StorageError::corrupt(TRADES, reason);
    let side = match m.side.as_str() {
        "buy" => Side::Buy,
        "sell" => Side::Sell,
        _ => return Err(bad("unknown side")),
    };
    let liquidity = match m.liquidity.as_str() {
        "maker" => Liquidity::Maker,
        "taker" => Liquidity::Taker,
        _ => return Err(bad("unknown liquidity")),
    };
    let amount = m.fee_amount.0;
    let fee = match (m.fee_kind.as_str(), m.fee_asset, m.fee_value) {
        ("base", None, None) => Fee::Base(Qty::new(amount)),
        ("quote", None, None) => Fee::Quote(Notional::new(amount)),
        ("other", Some(a), value) => Fee::Other {
            asset: Asset::new(a).map_err(|e| StorageError::corrupt(TRADES, e))?,
            amount: Qty::new(amount),
            value: value.map(|v| Notional::new(v.0)),
        },
        _ => return Err(bad("fee_kind disagrees with fee_asset or fee_value")),
    };
    let corrupt = |e: bot_core::Error| StorageError::corrupt(TRADES, e);
    let fill = Fill::new(FillSpec {
        order_id: OrderId::new(m.order_id).map_err(corrupt)?,
        instrument: m.instrument.parse().map_err(corrupt)?,
        side,
        qty: Qty::new(m.qty.0),
        price: Price::new(m.price.0),
        fee,
        liquidity,
        ts: Timestamp::from_millis(m.ts),
    })
    .map_err(corrupt)?;
    Ok((TradeId(m.id), fill))
}

fn from_holding_row(m: holding::Model) -> Result<Position, StorageError> {
    let corrupt = |e: bot_core::Error| StorageError::corrupt(HOLDINGS, e);
    Position::from_parts(
        m.instrument.parse().map_err(corrupt)?,
        Qty::new(m.qty.0),
        Notional::new(m.cost_basis.0),
        Notional::new(m.realized_pnl.0),
    )
    .map_err(corrupt)
}

/// Appends a trade on `conn`, a plain connection or an open transaction.
async fn insert_trade_on<C: ConnectionTrait>(
    conn: &C,
    fill: &Fill,
    venue: VenueRef<'_>,
) -> Result<TradeId, StorageError> {
    let res = trade::Entity::insert(to_trade_row(fill, venue))
        .exec(conn)
        .await
        .map_err(StorageError::backend)?;
    Ok(TradeId(res.last_insert_id))
}

/// Inserts or replaces a holding on `conn`, a plain connection or an open
/// transaction.
async fn upsert_holding_on<C: ConnectionTrait>(
    conn: &C,
    position: &Position,
) -> Result<(), StorageError> {
    let row = holding::ActiveModel {
        instrument: Set(position.instrument().to_string()),
        qty: Set(DbDecimal(position.qty().get())),
        cost_basis: Set(DbDecimal(position.cost_basis().get())),
        realized_pnl: Set(DbDecimal(position.realized_pnl().get())),
    };
    holding::Entity::insert(row)
        .on_conflict(
            OnConflict::column(holding::Column::Instrument)
                .update_columns([
                    holding::Column::Qty,
                    holding::Column::CostBasis,
                    holding::Column::RealizedPnl,
                ])
                .to_owned(),
        )
        .exec_without_returning(conn)
        .await
        .map_err(StorageError::backend)?;
    Ok(())
}

#[async_trait]
impl TradeRepository for SqlStore {
    async fn insert_trade(&self, fill: &Fill) -> Result<TradeId, StorageError> {
        insert_trade_on(&self.db, fill, None).await
    }

    async fn get_trade(&self, id: TradeId) -> Result<Option<Fill>, StorageError> {
        trade::Entity::find_by_id(id.0)
            .one(&self.db)
            .await
            .map_err(StorageError::backend)?
            .map(|m| from_trade_row(m).map(|(_, f)| f))
            .transpose()
    }

    async fn list_trades(
        &self,
        instrument: &InstrumentId,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<(TradeId, Fill)>, StorageError> {
        trade::Entity::find()
            .filter(trade::Column::Instrument.eq(instrument.to_string()))
            .filter(trade::Column::Ts.gte(from.as_millis()))
            .filter(trade::Column::Ts.lt(to.as_millis()))
            .order_by_asc(trade::Column::Ts)
            .order_by_asc(trade::Column::Id)
            .all(&self.db)
            .await
            .map_err(StorageError::backend)?
            .into_iter()
            .map(from_trade_row)
            .collect()
    }

    async fn recent_trades(&self, limit: u64) -> Result<Vec<StoredTrade>, StorageError> {
        trade::Entity::find()
            .order_by_desc(trade::Column::Ts)
            .order_by_desc(trade::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(StorageError::backend)?
            .into_iter()
            .map(|m| {
                let client_order_id = m.client_order_id.clone();
                from_trade_row(m).map(|(id, fill)| StoredTrade {
                    id,
                    fill,
                    client_order_id,
                })
            })
            .collect()
    }
}

#[async_trait]
impl HoldingRepository for SqlStore {
    async fn upsert_holding(&self, position: &Position) -> Result<(), StorageError> {
        upsert_holding_on(&self.db, position).await
    }

    async fn get_holding(
        &self,
        instrument: &InstrumentId,
    ) -> Result<Option<Position>, StorageError> {
        holding::Entity::find_by_id(instrument.to_string())
            .one(&self.db)
            .await
            .map_err(StorageError::backend)?
            .map(from_holding_row)
            .transpose()
    }

    async fn list_holdings(&self) -> Result<Vec<Position>, StorageError> {
        holding::Entity::find()
            .order_by_asc(holding::Column::Instrument)
            .all(&self.db)
            .await
            .map_err(StorageError::backend)?
            .into_iter()
            .map(from_holding_row)
            .collect()
    }
}

#[async_trait]
impl Ledger for SqlStore {
    async fn record_fill(
        &self,
        fill: &Fill,
        instrument: &Instrument,
    ) -> Result<(TradeId, Position), StorageError> {
        self.record(fill, instrument, None)
            .await
            .map(|r| r.unwrap_or_else(|| unreachable!("only venue fills can be duplicates")))
    }

    async fn record_venue_fill(
        &self,
        fill: &Fill,
        instrument: &Instrument,
        venue_trade_id: &str,
        client_order_id: &str,
    ) -> Result<Option<(TradeId, Position)>, StorageError> {
        self.record(fill, instrument, Some((venue_trade_id, client_order_id)))
            .await
    }
}

impl SqlStore {
    /// One transaction: skip if the venue trade id is already recorded,
    /// else append the trade and fold it into the holding.
    async fn record(
        &self,
        fill: &Fill,
        instrument: &Instrument,
        venue: VenueRef<'_>,
    ) -> Result<Option<(TradeId, Position)>, StorageError> {
        if fill.instrument != instrument.id {
            return Err(StorageError::Rejected(bot_core::Error::PositionUpdate(
                "fill is for a different instrument",
            )));
        }
        // Every early return drops `txn`, which rolls it back.
        let txn = self.db.begin().await.map_err(StorageError::backend)?;
        if let Some((vid, _)) = venue {
            let seen = trade::Entity::find()
                .filter(trade::Column::Instrument.eq(fill.instrument.to_string()))
                .filter(trade::Column::VenueTradeId.eq(vid))
                .one(&txn)
                .await
                .map_err(StorageError::backend)?;
            if seen.is_some() {
                return Ok(None);
            }
        }
        // FOR UPDATE on MySQL so a concurrent writer can't fold into the same
        // stale holding. SQLite omits the clause; it allows one writer at a
        // time per database, so the race can't happen there.
        let current = holding::Entity::find_by_id(fill.instrument.to_string())
            .lock_exclusive()
            .one(&txn)
            .await
            .map_err(StorageError::backend)?;
        let mut position = match current {
            Some(row) => from_holding_row(row)?,
            None => Position::flat(fill.instrument.clone()),
        };
        // A stored holding that breaks the asset-class policy (a negative spot
        // holding) is corruption, not a bad fill: refuse to trade on it.
        position
            .check_against(instrument)
            .map_err(|e| StorageError::corrupt(HOLDINGS, e))?;
        position
            .apply(fill, instrument)
            .map_err(StorageError::Rejected)?;
        // The unique (instrument, venue_trade_id) index backs the check
        // above: a concurrent duplicate fails here and rolls back.
        let id = insert_trade_on(&txn, fill, venue).await?;
        upsert_holding_on(&txn, &position).await?;
        txn.commit().await.map_err(StorageError::backend)?;
        Ok(Some((id, position)))
    }

    async fn set_order(
        &self,
        client_order_id: &str,
        row: order::ActiveModel,
    ) -> Result<(), StorageError> {
        let res = order::Entity::update_many()
            .set(row)
            .filter(order::Column::ClientOrderId.eq(client_order_id))
            .exec(&self.db)
            .await
            .map_err(StorageError::backend)?;
        if res.rows_affected == 0 {
            return Err(StorageError::UnknownOrder(client_order_id.to_owned()));
        }
        Ok(())
    }
}

fn status_text(s: OrderStatus) -> &'static str {
    match s {
        OrderStatus::Pending => "pending",
        OrderStatus::Open => "open",
        OrderStatus::Closed => "closed",
    }
}

fn from_order_row(m: order::Model) -> Result<JournalOrder, StorageError> {
    let bad = |reason: &str| StorageError::corrupt(ORDERS, reason);
    let side = match m.side.as_str() {
        "buy" => Side::Buy,
        "sell" => Side::Sell,
        _ => return Err(bad("unknown side")),
    };
    let status = match m.status.as_str() {
        "pending" => OrderStatus::Pending,
        "open" => OrderStatus::Open,
        "closed" => OrderStatus::Closed,
        _ => return Err(bad("unknown status")),
    };
    if status == OrderStatus::Open && m.venue_order_id.is_none() {
        return Err(bad("open order without venue_order_id"));
    }
    Ok(JournalOrder {
        client_order_id: m.client_order_id,
        instrument: m
            .instrument
            .parse()
            .map_err(|e: bot_core::Error| StorageError::corrupt(ORDERS, e))?,
        side,
        status,
        venue_order_id: m.venue_order_id,
        created_at: Timestamp::from_millis(m.created_at),
    })
}

#[async_trait]
impl StrategyStateRepository for SqlStore {
    async fn load_state(&self, strategy: &str) -> Result<Option<SavedState>, StorageError> {
        Ok(strategy_state::Entity::find_by_id(strategy.to_owned())
            .one(&self.db)
            .await
            .map_err(StorageError::backend)?
            .map(|m| SavedState {
                params_hash: m.params_hash,
                state: m.state,
                updated_at: Timestamp::from_millis(m.updated_at),
            }))
    }

    async fn save_state(&self, strategy: &str, saved: &SavedState) -> Result<(), StorageError> {
        let row = strategy_state::ActiveModel {
            strategy: Set(strategy.to_owned()),
            params_hash: Set(saved.params_hash.clone()),
            state: Set(saved.state.clone()),
            updated_at: Set(saved.updated_at.as_millis()),
        };
        strategy_state::Entity::insert(row)
            .on_conflict(
                OnConflict::column(strategy_state::Column::Strategy)
                    .update_columns([
                        strategy_state::Column::ParamsHash,
                        strategy_state::Column::State,
                        strategy_state::Column::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec_without_returning(&self.db)
            .await
            .map_err(StorageError::backend)?;
        Ok(())
    }
}

#[async_trait]
impl OrderJournal for SqlStore {
    async fn insert_pending(
        &self,
        client_order_id: &str,
        instrument: &InstrumentId,
        side: Side,
        created_at: Timestamp,
    ) -> Result<(), StorageError> {
        order::Entity::insert(order::ActiveModel {
            client_order_id: Set(client_order_id.to_owned()),
            instrument: Set(instrument.to_string()),
            side: Set(side_text(side).to_owned()),
            status: Set(status_text(OrderStatus::Pending).to_owned()),
            venue_order_id: Set(None),
            created_at: Set(created_at.as_millis()),
        })
        .exec_without_returning(&self.db)
        .await
        .map_err(StorageError::backend)?;
        Ok(())
    }

    async fn mark_open(
        &self,
        client_order_id: &str,
        venue_order_id: i64,
    ) -> Result<(), StorageError> {
        self.set_order(
            client_order_id,
            order::ActiveModel {
                status: Set(status_text(OrderStatus::Open).to_owned()),
                venue_order_id: Set(Some(venue_order_id)),
                ..Default::default()
            },
        )
        .await
    }

    async fn mark_closed(&self, client_order_id: &str) -> Result<(), StorageError> {
        self.set_order(
            client_order_id,
            order::ActiveModel {
                status: Set(status_text(OrderStatus::Closed).to_owned()),
                ..Default::default()
            },
        )
        .await
    }

    async fn unfinished_orders(&self) -> Result<Vec<JournalOrder>, StorageError> {
        order::Entity::find()
            .filter(order::Column::Status.ne(status_text(OrderStatus::Closed)))
            .order_by_asc(order::Column::CreatedAt)
            .order_by_asc(order::Column::ClientOrderId)
            .all(&self.db)
            .await
            .map_err(StorageError::backend)?
            .into_iter()
            .map(from_order_row)
            .collect()
    }

    async fn recent_orders(&self, limit: u64) -> Result<Vec<JournalOrder>, StorageError> {
        order::Entity::find()
            .order_by_desc(order::Column::CreatedAt)
            .order_by_desc(order::Column::ClientOrderId)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(StorageError::backend)?
            .into_iter()
            .map(from_order_row)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use migration::{Migrator, MigratorTrait};
    use rust_decimal_macros::dec;
    use sea_orm::{ConnectOptions, ConnectionTrait};

    /// Fresh, migrated in-memory SQLite. One connection: each SQLite
    /// connection to `:memory:` is a separate database.
    async fn store() -> SqlStore {
        let mut opts = ConnectOptions::new("sqlite::memory:");
        opts.max_connections(1).min_connections(1);
        let db = Database::connect(opts).await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        SqlStore { db }
    }

    fn btc() -> InstrumentId {
        "binance:BTCUSDT".parse().unwrap()
    }

    fn fill(side: Side, qty: Qty, price: Price, fee: Fee, ts: i64) -> Fill {
        Fill::new(FillSpec {
            order_id: OrderId::new("bt-1").unwrap(),
            instrument: btc(),
            side,
            qty,
            price,
            fee,
            liquidity: Liquidity::Taker,
            ts: Timestamp::from_millis(ts),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn trade_round_trip() {
        let s = store().await;
        let f = fill(
            Side::Buy,
            Qty::new(dec!(0.00166)),
            Price::new(dec!(60000.01)),
            Fee::Base(Qty::new(dec!(0.00000166))),
            1_704_070_799_999,
        );
        let id = s.insert_trade(&f).await.unwrap();
        assert_eq!(s.get_trade(id).await.unwrap(), Some(f));
    }

    #[tokio::test]
    async fn every_fee_kind_and_side_round_trips() {
        let s = store().await;
        let bnb = Asset::new("BNB").unwrap();
        let fills = [
            fill(
                Side::Buy,
                Qty::new(dec!(1)),
                Price::new(dec!(100)),
                Fee::Base(Qty::new(dec!(0.001))),
                1,
            ),
            fill(
                Side::Sell,
                Qty::new(dec!(1)),
                Price::new(dec!(100)),
                Fee::Quote(Notional::new(dec!(0.1))),
                2,
            ),
            fill(
                Side::Sell,
                Qty::new(dec!(1)),
                Price::new(dec!(100)),
                Fee::Other {
                    asset: bnb.clone(),
                    amount: Qty::new(dec!(0.0002)),
                    value: None,
                },
                3,
            ),
            fill(
                Side::Buy,
                Qty::new(dec!(1)),
                Price::new(dec!(100)),
                Fee::Other {
                    asset: bnb,
                    amount: Qty::new(dec!(0.0001)),
                    value: Some(Notional::new(dec!(0.06))),
                },
                4,
            ),
        ];
        for f in &fills {
            let id = s.insert_trade(f).await.unwrap();
            assert_eq!(s.get_trade(id).await.unwrap().as_ref(), Some(f));
        }
    }

    /// 28 significant digits: more than f64 holds (~15-17), so any float on
    /// the path would change the value.
    #[tokio::test]
    async fn money_is_stored_as_exact_text() {
        let s = store().await;
        let price = Price::new(dec!(60060.060060060060060060060));
        let f = fill(
            Side::Buy,
            Qty::new(dec!(0.1)),
            price,
            Fee::Quote(Notional::new(dec!(0.2))),
            1,
        );
        let id = s.insert_trade(&f).await.unwrap();

        assert_eq!(s.get_trade(id).await.unwrap().unwrap().price, price);
        let row =
            s.db.query_one_raw(sea_orm::Statement::from_string(
                s.db.get_database_backend(),
                "SELECT typeof(price) AS t, price AS p FROM trades",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<String>("", "t").unwrap(), "text");
        assert_eq!(
            row.try_get::<String>("", "p").unwrap(),
            "60060.060060060060060060060"
        );
    }

    #[tokio::test]
    async fn get_missing_trade_is_none() {
        assert_eq!(store().await.get_trade(TradeId(42)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn list_filters_by_instrument_and_half_open_range_in_time_order() {
        let s = store().await;
        let p = Price::new(dec!(100));
        let q = Qty::new(dec!(1));
        let fee = || Fee::Quote(Notional::ZERO);
        // Inserted out of time order on purpose.
        let late = s
            .insert_trade(&fill(Side::Sell, q, p, fee(), 20))
            .await
            .unwrap();
        let early = s
            .insert_trade(&fill(Side::Buy, q, p, fee(), 10))
            .await
            .unwrap();
        s.insert_trade(&fill(Side::Buy, q, p, fee(), 30))
            .await
            .unwrap(); // at `to`: excluded
        let eth = Fill::new(FillSpec {
            instrument: "binance:ETHUSDT".parse().unwrap(),
            ..fill(Side::Buy, q, p, fee(), 15).into()
        })
        .unwrap();
        s.insert_trade(&eth).await.unwrap();

        let got = s
            .list_trades(
                &btc(),
                Timestamp::from_millis(10),
                Timestamp::from_millis(30),
            )
            .await
            .unwrap();
        let ids: Vec<TradeId> = got.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![early, late]);
    }

    #[tokio::test]
    async fn corrupt_trade_row_is_an_error() {
        let s = store().await;
        s.db.execute_unprepared(
            "INSERT INTO trades (order_id, instrument, side, qty, price, fee_kind, fee_amount, \
             fee_asset, liquidity, ts) VALUES ('x', 'binance:BTCUSDT', 'hold', '1', '1', 'quote', \
             '0', NULL, 'taker', 0)",
        )
        .await
        .unwrap();
        let err = s.get_trade(TradeId(1)).await.unwrap_err();
        assert!(matches!(
            err,
            StorageError::Corrupt {
                table: "trades",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn holding_upsert_replaces_and_round_trips() {
        let s = store().await;
        let first = Position::from_parts(
            btc(),
            Qty::new(dec!(2)),
            Notional::new(dec!(300.3)),
            Notional::ZERO,
        )
        .unwrap();
        s.upsert_holding(&first).await.unwrap();
        assert_eq!(s.get_holding(&btc()).await.unwrap(), Some(first));

        let second = Position::from_parts(
            btc(),
            Qty::new(dec!(1.5)),
            Notional::new(dec!(225.225)),
            Notional::new(dec!(74.775)),
        )
        .unwrap();
        s.upsert_holding(&second).await.unwrap();
        assert_eq!(s.get_holding(&btc()).await.unwrap(), Some(second.clone()));
        assert_eq!(s.list_holdings().await.unwrap(), vec![second]);
    }

    #[tokio::test]
    async fn missing_holding_is_none() {
        assert_eq!(store().await.get_holding(&btc()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn invalid_stored_holding_is_an_error() {
        let s = store().await;
        s.db.execute_unprepared("INSERT INTO holdings VALUES ('binance:BTCUSDT', '0', '5', '0')")
            .await
            .unwrap();
        assert!(matches!(
            s.get_holding(&btc()).await.unwrap_err(),
            StorageError::Corrupt {
                table: "holdings",
                ..
            }
        ));
    }

    fn btcusdt() -> Instrument {
        Instrument::new(bot_core::InstrumentSpec {
            id: btc(),
            class: bot_core::AssetClass::Spot,
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

    fn quote_fee(d: rust_decimal::Decimal) -> Fee {
        Fee::Quote(Notional::new(d))
    }

    async fn trade_count(s: &SqlStore) -> usize {
        s.list_trades(
            &btc(),
            Timestamp::from_millis(0),
            Timestamp::from_millis(i64::MAX),
        )
        .await
        .unwrap()
        .len()
    }

    /// Same hand-worked sequence as core's position test:
    ///   buy 1 @ 100 fee 0.1, buy 1 @ 200 fee 0.2 -> qty 2, cost 300.3
    ///   sell 0.5 @ 300 fee 0.15 -> realized 74.775, qty 1.5, cost 225.225
    #[tokio::test]
    async fn record_fill_writes_trade_and_folds_holding() {
        let s = store().await;
        let i = btcusdt();
        let one = Qty::new(dec!(1));
        s.record_fill(
            &fill(
                Side::Buy,
                one,
                Price::new(dec!(100)),
                quote_fee(dec!(0.1)),
                1,
            ),
            &i,
        )
        .await
        .unwrap();
        let (_, after_buys) = s
            .record_fill(
                &fill(
                    Side::Buy,
                    one,
                    Price::new(dec!(200)),
                    quote_fee(dec!(0.2)),
                    2,
                ),
                &i,
            )
            .await
            .unwrap();
        assert_eq!(after_buys.qty(), Qty::new(dec!(2)));
        assert_eq!(after_buys.cost_basis(), Notional::new(dec!(300.3)));

        let sell = fill(
            Side::Sell,
            Qty::new(dec!(0.5)),
            Price::new(dec!(300)),
            quote_fee(dec!(0.15)),
            3,
        );
        let (id, pos) = s.record_fill(&sell, &i).await.unwrap();
        assert_eq!(pos.realized_pnl(), Notional::new(dec!(74.775)));
        assert_eq!(pos.qty(), Qty::new(dec!(1.5)));
        assert_eq!(pos.cost_basis(), Notional::new(dec!(225.225)));

        assert_eq!(s.get_trade(id).await.unwrap(), Some(sell));
        assert_eq!(s.get_holding(&btc()).await.unwrap(), Some(pos));
        assert_eq!(trade_count(&s).await, 3);
    }

    /// The holding must equal the trade log folded from flat: the invariant
    /// that makes `holdings` a safe cache.
    #[tokio::test]
    async fn holding_equals_replay_of_trade_log() {
        let s = store().await;
        let i = btcusdt();
        let fills = [
            fill(
                Side::Buy,
                Qty::new(dec!(0.5)),
                Price::new(dec!(60000)),
                Fee::Base(Qty::new(dec!(0.0005))),
                1,
            ),
            fill(
                Side::Buy,
                Qty::new(dec!(0.25)),
                Price::new(dec!(58000)),
                quote_fee(dec!(14.5)),
                2,
            ),
            fill(
                Side::Sell,
                Qty::new(dec!(0.3)),
                Price::new(dec!(61000)),
                quote_fee(dec!(18.3)),
                3,
            ),
        ];
        for f in &fills {
            s.record_fill(f, &i).await.unwrap();
        }

        let mut replay = Position::flat(btc());
        let log = s
            .list_trades(
                &btc(),
                Timestamp::from_millis(0),
                Timestamp::from_millis(i64::MAX),
            )
            .await
            .unwrap();
        for (_, f) in &log {
            replay.apply(f, &i).unwrap();
        }
        assert_eq!(s.get_holding(&btc()).await.unwrap(), Some(replay));
    }

    #[tokio::test]
    async fn rejected_fill_writes_nothing() {
        let s = store().await;
        let i = btcusdt();
        let one = Qty::new(dec!(1));
        let (_, before) = s
            .record_fill(
                &fill(Side::Buy, one, Price::new(dec!(100)), quote_fee(dec!(0)), 1),
                &i,
            )
            .await
            .unwrap();

        let oversell = fill(
            Side::Sell,
            Qty::new(dec!(2)),
            Price::new(dec!(100)),
            quote_fee(dec!(0)),
            2,
        );
        let err = s.record_fill(&oversell, &i).await.unwrap_err();
        assert!(matches!(err, StorageError::Rejected(_)));
        assert_eq!(trade_count(&s).await, 1);
        assert_eq!(s.get_holding(&btc()).await.unwrap(), Some(before));
    }

    /// The trade insert succeeds, then the holding write fails: the
    /// transaction must roll the trade back too.
    #[tokio::test]
    async fn failed_holding_write_rolls_back_trade() {
        let s = store().await;
        for event in ["INSERT", "UPDATE"] {
            s.db.execute_unprepared(&format!(
                "CREATE TRIGGER fail_holding_{event} BEFORE {event} ON holdings \
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END"
            ))
            .await
            .unwrap();
        }

        let buy = fill(
            Side::Buy,
            Qty::new(dec!(1)),
            Price::new(dec!(100)),
            quote_fee(dec!(0)),
            1,
        );
        let err = s.record_fill(&buy, &btcusdt()).await.unwrap_err();
        assert!(matches!(err, StorageError::Backend(_)));
        assert_eq!(trade_count(&s).await, 0);
        assert_eq!(s.get_holding(&btc()).await.unwrap(), None);
    }

    /// A negative spot holding is a valid `Position` value but breaks spot
    /// policy: `record_fill` must refuse to fold into it and write nothing.
    #[tokio::test]
    async fn record_fill_refuses_negative_spot_holding() {
        let s = store().await;
        s.db.execute_unprepared(
            "INSERT INTO holdings VALUES ('binance:BTCUSDT', '-1', '-100', '0')",
        )
        .await
        .unwrap();
        let buy = fill(
            Side::Buy,
            Qty::new(dec!(1)),
            Price::new(dec!(100)),
            quote_fee(dec!(0)),
            1,
        );
        let err = s.record_fill(&buy, &btcusdt()).await.unwrap_err();
        assert!(matches!(
            err,
            StorageError::Corrupt {
                table: "holdings",
                ..
            }
        ));
        assert_eq!(trade_count(&s).await, 0);
    }

    /// Buy 1 @ 100 with a BNB fee worth 0.06 USDT: hold 1 at cost 100.06.
    #[tokio::test]
    async fn valued_bnb_fee_folds_into_the_holding() {
        let s = store().await;
        let buy = fill(
            Side::Buy,
            Qty::new(dec!(1)),
            Price::new(dec!(100)),
            Fee::Other {
                asset: Asset::new("BNB").unwrap(),
                amount: Qty::new(dec!(0.0001)),
                value: Some(Notional::new(dec!(0.06))),
            },
            1,
        );
        let (id, pos) = s.record_fill(&buy, &btcusdt()).await.unwrap();
        assert_eq!(pos.qty(), Qty::new(dec!(1)));
        assert_eq!(pos.cost_basis(), Notional::new(dec!(100.06)));
        assert_eq!(s.get_trade(id).await.unwrap(), Some(buy));
    }

    #[tokio::test]
    async fn fee_value_on_a_quote_fee_is_corrupt() {
        let s = store().await;
        s.db.execute_unprepared(
            "INSERT INTO trades (order_id, instrument, side, qty, price, fee_kind, fee_amount, \
             fee_asset, liquidity, ts, fee_value) VALUES ('x', 'binance:BTCUSDT', 'buy', '1', '1', \
             'quote', '0', NULL, 'taker', 0, '1')",
        )
        .await
        .unwrap();
        assert!(matches!(
            s.get_trade(TradeId(1)).await.unwrap_err(),
            StorageError::Corrupt {
                table: "trades",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn venue_fill_is_recorded_once() {
        let s = store().await;
        let i = btcusdt();
        let buy = fill(
            Side::Buy,
            Qty::new(dec!(1)),
            Price::new(dec!(100)),
            quote_fee(dec!(0.1)),
            1,
        );
        let (_, pos) = s
            .record_venue_fill(&buy, &i, "12345", "tbx-o-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pos.cost_basis(), Notional::new(dec!(100.1)));
        // A restart replaying the venue's history sees the same trade again.
        assert_eq!(
            s.record_venue_fill(&buy, &i, "12345", "tbx-o-1")
                .await
                .unwrap(),
            None
        );
        assert_eq!(trade_count(&s).await, 1);
        assert_eq!(s.get_holding(&btc()).await.unwrap(), Some(pos));
        // Another trade id is another fill.
        assert!(
            s.record_venue_fill(&buy, &i, "12346", "tbx-o-1")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(trade_count(&s).await, 2);
    }

    /// Two runs both number their first order `o-1`; the journal's
    /// run-tagged id tells their fills apart. A paper fill has none.
    #[tokio::test]
    async fn trades_keep_the_journal_order_id() {
        let s = store().await;
        let i = btcusdt();
        let at = |ts| {
            Fill::new(FillSpec {
                order_id: OrderId::new("o-1").unwrap(),
                ..fill(
                    Side::Buy,
                    Qty::new(dec!(1)),
                    Price::new(dec!(100)),
                    quote_fee(dec!(0)),
                    ts,
                )
                .into()
            })
            .unwrap()
        };
        s.record_venue_fill(&at(1), &i, "1", "tbrun1-o-1")
            .await
            .unwrap();
        s.record_venue_fill(&at(2), &i, "2", "tbrun2-o-1")
            .await
            .unwrap();
        s.record_fill(&at(3), &i).await.unwrap();
        let got: Vec<_> = s
            .recent_trades(10)
            .await
            .unwrap()
            .into_iter()
            .map(|t| (t.fill.order_id.to_string(), t.client_order_id))
            .collect();
        assert_eq!(
            got,
            vec![
                ("o-1".to_owned(), None),
                ("o-1".to_owned(), Some("tbrun2-o-1".to_owned())),
                ("o-1".to_owned(), Some("tbrun1-o-1".to_owned())),
            ]
        );
    }

    #[tokio::test]
    async fn venue_trade_ids_are_unique_per_instrument_only() {
        // Binance numbers trades per symbol, so BTCUSDT and ETHUSDT can
        // both have trade 7.
        let s = store().await;
        let eth_inst = Instrument::new(bot_core::InstrumentSpec {
            id: "binance:ETHUSDT".parse().unwrap(),
            base: Asset::new("ETH").unwrap(),
            ..btcusdt().into()
        })
        .unwrap();
        let btc_buy = fill(
            Side::Buy,
            Qty::new(dec!(1)),
            Price::new(dec!(100)),
            quote_fee(dec!(0)),
            1,
        );
        let eth_buy = Fill::new(FillSpec {
            instrument: eth_inst.id.clone(),
            ..btc_buy.clone().into()
        })
        .unwrap();
        assert!(
            s.record_venue_fill(&btc_buy, &btcusdt(), "7", "tbx-o-1")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            s.record_venue_fill(&eth_buy, &eth_inst, "7", "tbx-o-2")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn order_journal_lifecycle() {
        let s = store().await;
        let t = Timestamp::from_millis;
        s.insert_pending("tbx-o-2", &btc(), Side::Sell, t(20))
            .await
            .unwrap();
        s.insert_pending("tbx-o-1", &btc(), Side::Buy, t(10))
            .await
            .unwrap();
        let pending = s.unfinished_orders().await.unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|o| (o.client_order_id.as_str(), o.status))
                .collect::<Vec<_>>(),
            vec![
                ("tbx-o-1", OrderStatus::Pending),
                ("tbx-o-2", OrderStatus::Pending)
            ],
            "oldest first"
        );
        assert_eq!(pending[0].side, Side::Buy);
        assert_eq!(pending[0].venue_order_id, None);

        s.mark_open("tbx-o-1", 28).await.unwrap();
        let open = &s.unfinished_orders().await.unwrap()[0];
        assert_eq!(
            (open.status, open.venue_order_id),
            (OrderStatus::Open, Some(28))
        );

        s.mark_closed("tbx-o-1").await.unwrap();
        s.mark_closed("tbx-o-2").await.unwrap();
        assert!(s.unfinished_orders().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn order_journal_refuses_reuse_and_unknown_ids() {
        let s = store().await;
        let t = Timestamp::from_millis(0);
        s.insert_pending("tbx-o-1", &btc(), Side::Buy, t)
            .await
            .unwrap();
        assert!(matches!(
            s.insert_pending("tbx-o-1", &btc(), Side::Buy, t).await,
            Err(StorageError::Backend(_))
        ));
        assert!(matches!(
            s.mark_open("nope", 1).await,
            Err(StorageError::UnknownOrder(_))
        ));
        assert!(matches!(
            s.mark_closed("nope").await,
            Err(StorageError::UnknownOrder(_))
        ));
    }

    #[tokio::test]
    async fn corrupt_order_row_is_an_error() {
        let s = store().await;
        s.db.execute_unprepared(
            "INSERT INTO orders VALUES ('tbx-o-1', 'binance:BTCUSDT', 'buy', 'open', NULL, 0)",
        )
        .await
        .unwrap();
        assert!(matches!(
            s.unfinished_orders().await.unwrap_err(),
            StorageError::Corrupt {
                table: "orders",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn recent_trades_are_newest_first_and_limited() {
        let s = store().await;
        let p = Price::new(dec!(100));
        let q = Qty::new(dec!(1));
        let fee = || Fee::Quote(Notional::ZERO);
        // Inserted out of time order on purpose.
        let mid = s
            .insert_trade(&fill(Side::Sell, q, p, fee(), 20))
            .await
            .unwrap();
        let old = s
            .insert_trade(&fill(Side::Buy, q, p, fee(), 10))
            .await
            .unwrap();
        let new = s
            .insert_trade(&fill(Side::Buy, q, p, fee(), 30))
            .await
            .unwrap();
        let ids = |v: Vec<StoredTrade>| v.into_iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(s.recent_trades(10).await.unwrap()), vec![new, mid, old]);
        assert_eq!(ids(s.recent_trades(2).await.unwrap()), vec![new, mid]);
    }

    #[tokio::test]
    async fn recent_orders_include_closed_ones_newest_first() {
        let s = store().await;
        let t = Timestamp::from_millis;
        s.insert_pending("tbx-o-1", &btc(), Side::Buy, t(10))
            .await
            .unwrap();
        s.insert_pending("tbx-o-2", &btc(), Side::Sell, t(20))
            .await
            .unwrap();
        s.mark_closed("tbx-o-1").await.unwrap();
        let got: Vec<_> = s
            .recent_orders(10)
            .await
            .unwrap()
            .into_iter()
            .map(|o| (o.client_order_id, o.status))
            .collect();
        assert_eq!(
            got,
            vec![
                ("tbx-o-2".to_owned(), OrderStatus::Pending),
                ("tbx-o-1".to_owned(), OrderStatus::Closed)
            ]
        );
        assert_eq!(s.recent_orders(1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn strategy_state_round_trips_and_is_replaced() {
        let s = store().await;
        assert_eq!(s.load_state("ratio_hop").await.unwrap(), None);
        let first = SavedState {
            params_hash: "abc".into(),
            state: r#"{"v":1,"phase":"start"}"#.into(),
            updated_at: Timestamp::from_millis(5),
        };
        s.save_state("ratio_hop", &first).await.unwrap();
        assert_eq!(s.load_state("ratio_hop").await.unwrap(), Some(first));
        let second = SavedState {
            params_hash: "def".into(),
            state: r#"{"v":1,"phase":"holding"}"#.into(),
            updated_at: Timestamp::from_millis(9),
        };
        s.save_state("ratio_hop", &second).await.unwrap();
        assert_eq!(s.load_state("ratio_hop").await.unwrap(), Some(second));
        assert_eq!(s.load_state("sma_trend").await.unwrap(), None);
    }

    #[tokio::test]
    async fn database_is_bound_to_one_network() {
        let s = store().await;
        s.bind_network("demo").await.unwrap();
        s.bind_network("demo").await.unwrap();
        assert!(matches!(
            s.bind_network("live").await,
            Err(StorageError::WrongNetwork { .. })
        ));
    }

    #[tokio::test]
    async fn open_migrates_a_fresh_file() {
        let path = std::env::temp_dir().join(format!("trade-bot-open-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let s = SqlStore::open(&url).await.unwrap();
        assert!(s.unfinished_orders().await.unwrap().is_empty());
        drop(s);
        // Opening again finds nothing pending to apply.
        SqlStore::open(&url).await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn record_fill_rejects_mismatched_instrument() {
        let s = store().await;
        let eth = Fill::new(FillSpec {
            instrument: "binance:ETHUSDT".parse().unwrap(),
            ..fill(
                Side::Buy,
                Qty::new(dec!(1)),
                Price::new(dec!(100)),
                quote_fee(dec!(0)),
                1,
            )
            .into()
        })
        .unwrap();
        let err = s.record_fill(&eth, &btcusdt()).await.unwrap_err();
        assert!(matches!(err, StorageError::Rejected(_)));
        assert_eq!(s.list_holdings().await.unwrap(), vec![]);
    }
}
