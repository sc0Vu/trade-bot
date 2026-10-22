//! Bot binary: `run | backtest | fetch-data | migrate`.
//!
//! Safe by default: paper against testnet. Live trading requires
//! `mode = "live"`, `exchange.network = "live"`, and `--i-understand`.

mod config;
mod date;
mod gate;

use anyhow::Context;
use backtest::{BacktestProvider, DataDir, fetch, run_backtest};
use bot_core::{Decimal, InstrumentId, Interval, Notional, Symbol, Timestamp, VenueId};
use clap::{ArgGroup, Args, Parser, Subcommand};
use engine::EngineConfig;
use exchange::Provider;
use exchange::binance::{
    BinanceConfig, BinanceSpotProvider, Credentials, Network, PUBLIC_DATA_URL, PublicClient,
};
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use storage::SqlStore;
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
    /// Trade on Binance: testnet or demo (`--paper`), mainnet only behind
    /// the live gate.
    Run(RunArgs),
}

#[derive(Args)]
#[command(group(ArgGroup::new("network").args(["paper", "live"])))]
struct RunArgs {
    /// Trade fake balances on the configured network (testnet or demo).
    /// The default.
    #[arg(long)]
    paper: bool,
    /// Trade real money on mainnet. Also needs `mode = "live"` and
    /// `exchange.network = "live"` in config, and `--i-understand`.
    #[arg(long)]
    live: bool,
    /// Acknowledge that `--live` sends real orders.
    #[arg(long, requires = "live")]
    i_understand: bool,
    /// Config file (default `BOT_CONFIG`, else config/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Opens (and migrates) the run's database and checks it belongs to
/// `network`.
async fn open_books(db: &config::DbSection, network: Network) -> anyhow::Result<Arc<SqlStore>> {
    let url = db.url_for(network);
    if db.url.is_none() {
        // The default SQLite file lives under data/ (gitignored).
        std::fs::create_dir_all("data").context("creating data/")?;
    }
    let store = SqlStore::open(&url).await.context("opening the database")?;
    store.bind_network(config::network_name(network)).await?;
    Ok(Arc::new(store))
}

async fn run_bot(a: RunArgs) -> anyhow::Result<()> {
    let cfg = config::Config::load(a.config.as_deref())?;
    let network = gate::decide(cfg.mode, cfg.exchange.network, a.live, a.i_understand)?;
    gate::check_overrides(
        network,
        &[
            cfg.exchange.rest_url.as_deref(),
            cfg.exchange.ws_url.as_deref(),
        ],
    )?;
    // Hard error in every mode: fake-money networks need their own keys too.
    let creds = Credentials::from_env()?;
    let books = open_books(&cfg.db, network).await?;
    let provider = BinanceSpotProvider::new(
        BinanceConfig {
            network,
            rest_url: cfg.exchange.rest_url.clone(),
            ws_url: cfg.exchange.ws_url.clone(),
            interval: cfg.exchange.interval,
        },
        creds,
        cfg.whitelist.clone(),
        books,
    );
    let params = Params::new(cfg.strategy.params);
    let mut strategy = strategy::build(&cfg.strategy.name, &params)?;
    let engine_cfg = EngineConfig {
        whitelist: cfg.whitelist,
        history_len: cfg.engine.history_len,
    };
    if network == Network::Live {
        eprintln!("LIVE: sending real orders to Binance mainnet");
    }
    tracing::info!(provider = provider.id(), strategy = %cfg.strategy.name, "starting");
    tokio::select! {
        r = engine::run(&provider, strategy.as_mut(), params, &engine_cfg) => {
            let r = r?;
            println!("events {}  fills {}  rejected signals {}", r.events, r.fills.len(), r.rejections.len());
            for p in &r.positions {
                println!("{}  qty {}  realized {}", p.instrument(), p.qty(), p.realized_pnl());
            }
            Ok(())
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("interrupted: open orders on the venue were NOT cancelled; check the account");
            Ok(())
        }
    }
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
    /// Taker slippage in basis points. Required, no default: a market order
    /// never fills at the price the decision saw, and a silent 0 would hide
    /// that cost.
    #[arg(long, value_parser = parse_decimal)]
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
    let cli = Cli::parse();
    // A live run must show its orders and fills without RUST_LOG set; a
    // backtest logging every simulated order would bury its report.
    let default = match cli.command {
        Command::Run(_) => "info",
        _ => "warn",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default)),
        )
        .init();
    match cli.command {
        Command::FetchData(a) => fetch_data(a).await,
        Command::Backtest(a) => backtest(a).await,
        Command::Run(a) => run_bot(a).await,
    }
}
