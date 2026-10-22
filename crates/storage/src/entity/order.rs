//! `orders`: journal of every order sent to a venue.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "orders")]
pub struct Model {
    /// The id sent as the venue's client order id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub client_order_id: String,
    /// `venue:SYMBOL`.
    pub instrument: String,
    /// `buy` | `sell`.
    pub side: String,
    /// `pending` | `open` | `closed`.
    pub status: String,
    /// The venue's id, once the venue acknowledged the order.
    pub venue_order_id: Option<i64>,
    /// ms since the Unix epoch.
    pub created_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
