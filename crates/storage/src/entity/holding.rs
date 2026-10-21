//! `holdings`: current position per instrument, one row each.

use crate::decimal::DbDecimal;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "holdings")]
pub struct Model {
    /// `venue:SYMBOL`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub instrument: String,
    pub qty: DbDecimal,
    pub cost_basis: DbDecimal,
    pub realized_pnl: DbDecimal,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
