//! `strategy_state`: the last state each strategy saved.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "strategy_state")]
pub struct Model {
    /// Registry name.
    #[sea_orm(primary_key, auto_increment = false)]
    pub strategy: String,
    /// Hash of the params the state was saved under.
    pub params_hash: String,
    /// JSON, as the strategy's `save_state` produced it.
    pub state: String,
    /// ms since the Unix epoch.
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
