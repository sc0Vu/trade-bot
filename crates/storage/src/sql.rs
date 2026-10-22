use crate::decimal::DbDecimal;
use crate::entity::{holding, trade};
use crate::{HoldingRepository, Ledger, StorageError, TradeId, TradeRepository};
use async_trait::async_trait;
use bot_core::{
    Asset, Fee, Fill, FillSpec, Instrument, InstrumentId, Liquidity, Notional, OrderId, Position,
    Price, Qty, Side, Timestamp,
};
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
}

const TRADES: &str = "trades";
const HOLDINGS: &str = "holdings";

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

fn to_trade_row(f: &Fill) -> trade::ActiveModel {
    let (kind, amount, asset) = match &f.fee {
        Fee::Base(q) => ("base", q.get(), None),
        Fee::Quote(n) => ("quote", n.get(), None),
        Fee::Other { asset, amount } => ("other", amount.get(), Some(asset.to_string())),
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
        liquidity: Set(liquidity_text(f.liquidity).to_owned()),
        ts: Set(f.ts.as_millis()),
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
    let fee = match (m.fee_kind.as_str(), m.fee_asset) {
        ("base", None) => Fee::Base(Qty::new(amount)),
        ("quote", None) => Fee::Quote(Notional::new(amount)),
        ("other", Some(a)) => Fee::Other {
            asset: Asset::new(a).map_err(|e| StorageError::corrupt(TRADES, e))?,
            amount: Qty::new(amount),
        },
        _ => return Err(bad("fee_kind and fee_asset disagree")),
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
) -> Result<TradeId, StorageError> {
    let res = trade::Entity::insert(to_trade_row(fill))
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
        insert_trade_on(&self.db, fill).await
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
        if fill.instrument != instrument.id {
            return Err(StorageError::Rejected(bot_core::Error::PositionUpdate(
                "fill is for a different instrument",
            )));
        }
        // Every early return drops `txn`, which rolls it back.
        let txn = self.db.begin().await.map_err(StorageError::backend)?;
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
        let id = insert_trade_on(&txn, fill).await?;
        upsert_holding_on(&txn, &position).await?;
        txn.commit().await.map_err(StorageError::backend)?;
        Ok((id, position))
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
                    asset: bnb,
                    amount: Qty::new(dec!(0.0002)),
                },
                3,
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
