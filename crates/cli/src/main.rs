//! Bot binary: `run | backtest | fetch-data | migrate`.
//!
//! Safe by default: paper against testnet. Live trading requires
//! `mode = "live"`, a non-testnet provider, and `--i-understand`.

mod date;

use anyhow::Context;
use backtest::{BacktestProvider, DataDir, fetch, run_backtest};
use bot_core::{Decimal, InstrumentId, Interval, Notional, Symbol, Timestamp, VenueId};
use clap::{Args, Parser, Subcommand};
use engine::EngineConfig;
use exchange::binance::{PUBLIC_DATA_URL, PublicClient};
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use strategy::Params;

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
    /// Replay cached klines through the engine with a paper broker.
    Backtest(BacktestArgs),
}

#[derive(Args)]
struct BacktestArgs {
    /// Registered strategy name.
    #[arg(long)]
    strategy: String,
    /// Strategy params as a TOML file (money values as strings).
    #[arg(long)]
    params: Option<PathBuf>,
    /// Whitelisted spot symbols, comma-separated; must be fetched already.
    #[arg(long, value_delimiter = ',', required = true)]
    symbol: Vec<Symbol>,
    #[arg(long, default_value = "1h", value_parser = parse_interval)]
    interval: Interval,
    /// First day, `YYYY-MM-DD` (UTC).
    #[arg(long, value_parser = date::parse)]
    from: Timestamp,
    /// Day to stop before, `YYYY-MM-DD` (UTC).
    #[arg(long, value_parser = date::parse)]
    to: Timestamp,
    /// Starting cash for the report, in quote.
    #[arg(long, default_value = "1000", value_parser = parse_decimal)]
    initial_cash: Decimal,
    /// Maker and taker fee as a fraction (`0.001` = 0.1%), charged in the
    /// received asset like Binance spot.
    #[arg(long, default_value = "0.001", value_parser = parse_decimal)]
    fee: Decimal,
    /// Taker slippage in basis points.
    #[arg(long, default_value = "0", value_parser = parse_decimal)]
    slippage_bps: Decimal,
    /// Candles of history kept per instrument for the strategy.
    #[arg(long, default_value_t = 500)]
    history_len: usize,
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,
}

/// Exact decimal from text; never via a float (invariant 1).
fn parse_decimal(s: &str) -> anyhow::Result<Decimal> {
    Decimal::from_str_exact(s).with_context(|| format!("{s:?} is not an exact decimal"))
}

async fn backtest(a: BacktestArgs) -> anyhow::Result<()> {
    let params = match &a.params {
        Some(path) => Params::from_toml(
            &std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("parsing {}", path.display()))?,
        None => Params::default(),
    };
    let venue = VenueId::new("binance")?;
    let ids: Vec<InstrumentId> = a
        .symbol
        .into_iter()
        .map(|s| InstrumentId::new(venue.clone(), s))
        .collect();
    let fees = RateFee::new(a.fee, a.fee, FeeAsset::Received).context("fee must be in [0, 1)")?;
    let slippage = BpsSlippage::new(a.slippage_bps).context("slippage must be >= 0")?;
    let provider = BacktestProvider::from_cache(
        &DataDir::new(a.data_dir),
        &ids,
        a.interval,
        a.from,
        a.to,
        fees,
        slippage,
    )?;
    let mut strategy = strategy::build(&a.strategy, &params)?;
    let config = EngineConfig {
        whitelist: ids,
        history_len: a.history_len,
    };
    let report = run_backtest(
        &provider,
        strategy.as_mut(),
        params,
        &config,
        Notional::new(a.initial_cash),
        a.interval,
    )
    .await?;
    println!("{report}");
    println!("(model output under the stated fill model; unverified against live)");
    Ok(())
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
        Command::Backtest(a) => backtest(a).await,
    }
}
