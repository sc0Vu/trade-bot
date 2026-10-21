//! Creates `trades` (append-only fill log) and `holdings` (current position
//! per instrument).

use sea_orm_migration::{prelude::*, schema::*};

/// Width of a money column. A `Decimal` prints as at most 28 digits plus
/// sign and point (30 chars); 40 leaves headroom.
const MONEY_LEN: u32 = 40;
/// `venue:SYMBOL`, each part at most 32 chars.
const INSTRUMENT_LEN: u32 = 65;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Trades::Table)
                    .if_not_exists()
                    .col(big_pk_auto(Trades::Id))
                    .col(string_len(Trades::OrderId, 36))
                    .col(string_len(Trades::Instrument, INSTRUMENT_LEN))
                    .col(string_len(Trades::Side, 4))
                    .col(string_len(Trades::Qty, MONEY_LEN))
                    .col(string_len(Trades::Price, MONEY_LEN))
                    .col(string_len(Trades::FeeKind, 5))
                    .col(string_len(Trades::FeeAmount, MONEY_LEN))
                    .col(string_len_null(Trades::FeeAsset, 32))
                    .col(string_len(Trades::Liquidity, 5))
                    .col(big_integer(Trades::Ts))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_trades_instrument_ts")
                    .table(Trades::Table)
                    .col(Trades::Instrument)
                    .col(Trades::Ts)
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(Holdings::Table)
                    .if_not_exists()
                    .col(string_len(Holdings::Instrument, INSTRUMENT_LEN).primary_key())
                    .col(string_len(Holdings::Qty, MONEY_LEN))
                    .col(string_len(Holdings::CostBasis, MONEY_LEN))
                    .col(string_len(Holdings::RealizedPnl, MONEY_LEN))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Holdings::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Trades::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Trades {
    Table,
    Id,
    OrderId,
    Instrument,
    Side,
    Qty,
    Price,
    FeeKind,
    FeeAmount,
    FeeAsset,
    Liquidity,
    Ts,
}

#[derive(DeriveIden)]
enum Holdings {
    Table,
    Instrument,
    Qty,
    CostBasis,
    RealizedPnl,
}
