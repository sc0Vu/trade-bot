//! `trades`: append-only log of fills.

use crate::decimal::DbDecimal;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "trades")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub order_id: String,
    /// `venue:SYMBOL`.
    pub instrument: String,
    /// `buy` | `sell`.
    pub side: String,
    pub qty: DbDecimal,
    pub price: DbDecimal,
    /// `base` | `quote` | `other`.
    pub fee_kind: String,
    pub fee_amount: DbDecimal,
    /// Set only when `fee_kind = other`.
    pub fee_asset: Option<String>,
    /// `fee_kind = other` only: the fee's value in the quote asset, if
    /// known.
    pub fee_value: Option<DbDecimal>,
    /// `maker` | `taker`.
    pub liquidity: String,
    /// Fill time, ms since the Unix epoch.
    pub ts: i64,
    /// The venue's trade id; unique per instrument. `None` for paper
    /// fills.
    pub venue_trade_id: Option<String>,
    /// The journal's id for the order a venue fill belongs to
    /// (`orders.client_order_id`, run-tagged). `None` for paper fills and
    /// fills recorded before it existed.
    pub client_order_id: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
