use crate::decimal::DbDecimal;
use crate::entity::{holding, trade};
use crate::{HoldingRepository, StorageError, TradeId, TradeRepository};
use async_trait::async_trait;
use bot_core::{
    Asset, Fee, Fill, FillSpec, InstrumentId, Liquidity, Notional, OrderId, Position, Price, Qty,
    Side, Timestamp,
};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::{NotSet, Set},
    ColumnTrait, Database, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
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

#[async_trait]
impl TradeRepository for SqlStore {
    async fn insert_trade(&self, fill: &Fill) -> Result<TradeId, StorageError> {
        let res = trade::Entity::insert(to_trade_row(fill))
            .exec(&self.db)
            .await
            .map_err(StorageError::backend)?;
        Ok(TradeId(res.last_insert_id))
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
            .exec_without_returning(&self.db)
            .await
            .map_err(StorageError::backend)?;
        Ok(())
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
        s.db.execute_unprepared("INSERT INTO holdings VALUES ('binance:BTCUSDT', '-1', '0', '0')")
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
}
