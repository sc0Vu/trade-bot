//! Creates `strategy_state`: the last state each strategy saved, so a
//! restart can carry on where the previous run stopped.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(StrategyState::Table)
                    .if_not_exists()
                    .col(string_len(StrategyState::Strategy, 64).primary_key())
                    .col(string_len(StrategyState::ParamsHash, 64))
                    .col(text(StrategyState::State))
                    .col(big_integer(StrategyState::UpdatedAt))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(StrategyState::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum StrategyState {
    Table,
    Strategy,
    ParamsHash,
    State,
    UpdatedAt,
}
