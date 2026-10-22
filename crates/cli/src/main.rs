//! Bot binary: `run | backtest | fetch-data | migrate`.
//!
//! Safe by default: paper against testnet. Live trading requires
//! `mode = "live"`, a non-testnet provider, and `--i-understand`.

mod date;

use anyhow::Context;
use backtest::{DataDir, fetch};
use bot_core::{InstrumentId, Interval, Symbol, Timestamp, VenueId};
use clap::{Args, Parser, Subcommand};
use exchange::binance::{PUBLIC_DATA_URL, PublicClient};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(version, about = "Event-driven trading bot")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download public Binance klines and instrument filters into the cache.
    FetchData(FetchArgs),
}

#[derive(Args)]
struct FetchArgs {
    /// Spot symbols, comma-separated (`BTCUSDT,ETHUSDT`).
    #[arg(long, value_delimiter = ',', required = true)]
    symbol: Vec<Symbol>,
    /// Bar width (`1m`, `5m`, `15m`, `1h`, `4h`, `1d`).
    #[arg(long, default_value = "1h", value_parser = parse_interval)]
    interval: Interval,
    /// First day to fetch, `YYYY-MM-DD` (UTC).
    #[arg(long, value_parser = date::parse)]
    from: Timestamp,
    /// Day to stop before, `YYYY-MM-DD` (UTC); default now.
    #[arg(long, value_parser = date::parse)]
    to: Option<Timestamp>,
    /// Cache root.
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,
    /// REST host for public market data.
    #[arg(long, default_value = PUBLIC_DATA_URL)]
    base_url: String,
}

fn parse_interval(s: &str) -> anyhow::Result<Interval> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .with_context(|| format!("interval {s:?}: want 1m, 5m, 15m, 1h, 4h or 1d"))
}

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    Timestamp::from_millis(ms)
}

async fn fetch_data(a: FetchArgs) -> anyhow::Result<()> {
    let client = PublicClient::new(a.base_url);
    let dir = DataDir::new(a.data_dir);
    let to = a.to.unwrap_or_else(now);
    let venue = VenueId::new("binance")?;
    for symbol in a.symbol {
        let id = InstrumentId::new(venue.clone(), symbol);
        let s = fetch(&client, &dir, &id, a.interval, a.from, to)
            .await
            .with_context(|| format!("fetching {id}"))?;
        println!(
            "{id} {}: {} downloaded, {} cached",
            a.interval.code(),
            s.downloaded,
            s.cached
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match Cli::parse().command {
        Command::FetchData(a) => fetch_data(a).await,
    }
}
