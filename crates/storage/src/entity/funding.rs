//! `funding`: perpetual funding payments as the venue booked them.

use crate::decimal::DbDecimal;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "funding")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// `venue:SYMBOL`.
    pub instrument: String,
    /// The venue's transaction id; unique per instrument.
    pub venue_id: String,
    /// Received (negative: paid), in the quote.
    pub amount: DbDecimal,
    /// ms since the Unix epoch.
    pub ts: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
