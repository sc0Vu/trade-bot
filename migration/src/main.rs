//! Migration CLI driven by `sea-orm-cli migrate`; reads `DATABASE_URL`.

use sea_orm_migration::prelude::*;

#[tokio::main]
async fn main() {
    cli::run_cli(migration::Migrator).await;
}
