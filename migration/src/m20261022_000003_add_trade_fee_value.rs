//! Adds `trades.fee_value`: a third-asset fee's value in the instrument's
//! quote asset at fill time (`Fee::Other::value`). NULL for base and quote
//! fees, whose amount is already in a position asset.

use sea_orm_migration::{prelude::*, schema::*};

/// Width of a money column, as in the first migration.
const MONEY_LEN: u32 = 40;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Trades::Table)
                    .add_column(string_len_null(Trades::FeeValue, MONEY_LEN))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Trades::Table)
                    .drop_column(Trades::FeeValue)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum Trades {
    Table,
    FeeValue,
}
