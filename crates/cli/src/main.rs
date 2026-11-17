//! Bot binary: `run | backtest | fetch-data | migrate`.
//!
//! Safe by default: paper against testnet. Live trading requires
//! `mode = "live"`, `exchange.network = "live"`, and `--i-understand`.

mod config;
mod date;
mod gate;
mod logging;
mod shutdown;
mod state;
mod telemetry;

use anyhow::Context;
use backtest::{BacktestProvider, DataDir, HistorySource, PaperConfig, fetch, run_backtest};
use bot_core::{Decimal, InstrumentId, Interval, MarketEvent, Notional, Symbol, Timestamp};
use clap::{ArgGroup, Args, Parser, Subcommand};
use config::Venue;
use engine::EngineConfig;
use exchange::Provider;
use exchange::binance::reconcile::Orphans;
use exchange::binance::{
    BinanceConfig, BinanceSpotProvider, Credentials, Network, PUBLIC_DATA_URL, PublicClient,
};
use exchange::max::paper::{MaxPaperConfig, MaxPaperProvider};
use exchange::max::public::MaxPublicClient;
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
    /// Cancel open orders this bot placed under another database (the
    /// startup refusal "missing from this database") instead of stopping.
    /// Their fills so far stay in the account, outside the books. Only for
    /// a lost database: if another instance is still running, it cancels
    /// that instance's orders.
    #[arg(long)]
    cancel_orphans: bool,
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

async fn run_bot(a: RunArgs, cfg: config::Config) -> anyhow::Result<()> {
    if cfg.exchange.venue == Venue::Max {
        gate::decide_max(cfg.mode, a.live)?;
        if a.cancel_orphans {
            tracing::warn!("--cancel-orphans ignored: a MAX paper run places no orders on MAX");
        }
        return run_max_paper(cfg).await;
    }
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
    let api_token = config::api_token(&cfg.api, std::env::var(config::API_TOKEN_VAR).ok())?;
    let provider = BinanceSpotProvider::new(
        BinanceConfig {
            network,
            rest_url: cfg.exchange.rest_url.clone(),
            ws_url: cfg.exchange.ws_url.clone(),
            interval: cfg.exchange.interval,
            http_timeout: std::time::Duration::from_secs(cfg.exchange.http_timeout_secs),
            warmup: cfg.engine.history_len,
            max_outage: std::time::Duration::from_secs(cfg.exchange.max_outage_secs),
            user_stream: cfg.exchange.user_stream,
            earn: cfg.earn.to_exchange(),
        },
        creds,
        cfg.whitelist.clone(),
        books.clone(),
    )
    .with_orphans(if a.cancel_orphans {
        Orphans::Cancel
    } else {
        Orphans::Refuse
    });
    let state_hash = state::params_hash(&cfg.strategy.name, &cfg.strategy.params);
    let resume = state::load(&*books, &cfg.strategy.name, &state_hash).await?;
    if resume.is_some() {
        tracing::info!("resuming saved strategy state");
    }
    let saver = state::Saver::start(
        books.clone(),
        cfg.strategy.name.clone(),
        state_hash,
        resume.clone(),
        now,
    );
    let params = Params::new(cfg.strategy.params);
    let mut strategy = strategy::build(&cfg.strategy.name, &params)?;
    let engine_cfg = EngineConfig {
        whitelist: cfg.whitelist,
        history_len: cfg.engine.history_len,
        delist_exit: cfg.engine.delist_exit(),
    };
    if network == Network::Live {
        eprintln!("LIVE: sending real orders to Binance mainnet");
    }
    telemetry::start(&cfg.metrics);
    let live = if cfg.api.enabled {
        let (tx, rx) = tokio::sync::watch::channel(None);
        let state = api::ApiState {
            info: api::RunInfo {
                network: config::network_name(network).to_owned(),
                strategy: cfg.strategy.name.clone(),
                started_at: now(),
            },
            live: rx,
            books: books.clone(),
            token: api_token,
        };
        // Like metrics, the API can never stop trading.
        match api::serve(cfg.api.listen, state).await {
            Ok(addr) => tracing::info!(%addr, "API listening"),
            Err(e) => {
                tracing::warn!(error = %e, listen = %cfg.api.listen, "API not started; trading continues");
            }
        }
        Some(tx)
    } else {
        None
    };
    let mut observer = telemetry::RunObserver {
        include_money: cfg.metrics.include_money,
        live,
        state: Some(saver),
    };
    tracing::info!(provider = provider.id(), strategy = %cfg.strategy.name, run_tag = provider.run_tag(), "starting");
    let outcome = tokio::select! {
        r = engine::run_observed(&provider, strategy.as_mut(), params, &engine_cfg, &mut observer, resume) => r.map(Some),
        s = shutdown::signal() => {
            eprintln!("{}: stopping; cancelling open orders (signal again to skip)", s?);
            Ok(None)
        }
    };
    // However the run ended, no order may be left working on the venue
    // with no strategy watching it.
    let closed = match shutdown::close_out_unless(provider.close_out(), shutdown::signal()).await {
        shutdown::CloseOut::Done(r) => r.map_err(anyhow::Error::from),
        shutdown::CloseOut::Abandoned(s) => Err(anyhow::anyhow!(
            "{s} during close-out: abandoned; orders may still be open on Binance \
             until the next start reconciles them"
        )),
    };
    match &closed {
        Ok(c) => tracing::info!(
            orders_closed = c.orders_closed,
            fills_recovered = c.fills_recovered,
            "closed out open orders"
        ),
        Err(e) => eprintln!("could not close out open orders: {e}; check the account"),
    }
    if let Some(r) = outcome? {
        print_summary(&r);
    }
    if let Some(saver) = observer.state.take() {
        saver.finish().await;
    }
    closed?;
    Ok(())
}

/// A paper run on MAX: MAX's live klines, the local paper broker. No key is
/// read and no order leaves the process; books are in memory, so the run
/// starts flat and saved strategy state is not resumed (it would describe
/// positions this broker doesn't have). The API needs the order database
/// and stays off.
async fn run_max_paper(cfg: config::Config) -> anyhow::Result<()> {
    let paper = cfg
        .paper
        .as_ref()
        .context("exchange.venue = \"max\" needs a [paper] section")?;
    let provider = MaxPaperProvider::new(
        MaxPaperConfig {
            interval: cfg.exchange.interval,
            warmup: cfg.engine.history_len,
            max_outage: std::time::Duration::from_secs(cfg.exchange.max_outage_secs),
            http_timeout: std::time::Duration::from_secs(cfg.exchange.http_timeout_secs),
            fees: RateFee::new(paper.fee, paper.fee, FeeAsset::Received)
                .context("paper.fee must be in [0, 1)")?,
            slippage: BpsSlippage::new(paper.slippage_bps)
                .context("paper.slippage_bps must be >= 0")?,
            initial_cash: paper.initial_cash,
        },
        cfg.whitelist.clone(),
    );
    if cfg.api.enabled {
        tracing::warn!("api is not available on a MAX paper run (no order database); not started");
    }
    let params = Params::new(cfg.strategy.params);
    let mut strategy = strategy::build(&cfg.strategy.name, &params)?;
    let engine_cfg = EngineConfig {
        whitelist: cfg.whitelist,
        history_len: cfg.engine.history_len,
        delist_exit: cfg.engine.delist_exit(),
    };
    telemetry::start(&cfg.metrics);
    let mut observer = telemetry::RunObserver {
        include_money: cfg.metrics.include_money,
        live: None,
        state: None,
    };
    tracing::info!(
        provider = provider.id(),
        strategy = %cfg.strategy.name,
        "starting: MAX prices, local paper fills, nothing sent to MAX"
    );
    let outcome = tokio::select! {
        r = engine::run_observed(&provider, strategy.as_mut(), params, &engine_cfg, &mut observer, None) => r.map(Some),
        s = shutdown::signal() => {
            eprintln!("{}: stopping", s?);
            Ok(None)
        }
    };
    if let Some(r) = outcome? {
        print_summary(&r);
    }
    Ok(())
}

/// The end-of-run summary `run` prints.
fn print_summary(r: &engine::RunReport) {
    println!(
        "events {}  fills {}  rejected signals {}",
        r.events,
        r.fills.len(),
        r.rejections.len()
    );
    for p in &r.positions {
        println!(
            "{}  qty {}  realized {}",
            p.instrument(),
            p.qty(),
            p.realized_pnl()
        );
    }
}

/// Where `fetch-data` downloads `venue`'s public market data from.
fn data_url(venue: Venue) -> &'static str {
    match venue {
        Venue::Binance => PUBLIC_DATA_URL,
        Venue::Max => exchange::max::REST_URL,
        Venue::BinanceUsdm => exchange::binance::usdm::LIVE_URL,
    }
}

#[derive(Args)]
struct BacktestArgs {
    /// Registered strategy name.
    #[arg(long)]
    strategy: String,
    /// Venue the symbols are on (their cache directory).
    #[arg(long, value_enum, default_value = "binance")]
    venue: Venue,
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
    /// Starting cash in quote: the paper broker's balance (buys beyond it
    /// are refused) and the report's starting equity.
    #[arg(long, default_value = "1000", value_parser = parse_decimal)]
    initial_cash: Decimal,
    /// Maker and taker fee as a fraction (`0.001` = 0.1%), charged in the
    /// received asset like Binance spot. The default is Binance's base
    /// rate; pass the venue's own rate for another venue.
    #[arg(long, default_value = "0.001", value_parser = parse_decimal)]
    fee: Decimal,
    /// Taker slippage in basis points. Required, no default: a market order
    /// never fills at the price the decision saw, and a silent 0 would hide
    /// that cost.
    #[arg(long, value_parser = parse_decimal)]
    slippage_bps: Decimal,
    /// Announced delistings to replay (see `config/delistings/`): default
    /// `config/delistings/<venue>.toml` when it exists.
    #[arg(long)]
    delistings: Option<PathBuf>,
    /// A series that stops for longer than this many hours, or ends this
    /// long before `--to`, is cut there and delisted with no notice (a halt
    /// nobody announced; also keeps a reused symbol's later bars out).
    #[arg(long, default_value_t = 48)]
    max_gap_hours: u64,
    /// Share of cash kept in Simple Earn (`[earn] ratio` live): out of what
    /// buys may spend. In [0, 1).
    #[arg(long, value_parser = parse_decimal)]
    earn_ratio: Option<Decimal>,
    /// Annual rate assumed for that share (`0.05` = 5%), labelled in the
    /// report as an assumption; needs `--earn-ratio`. Without it the share
    /// is held back but earns nothing.
    #[arg(long, value_parser = parse_decimal, requires = "earn_ratio")]
    earn_apr: Option<Decimal>,
    /// Hours before an announced delisting at which the engine sells the
    /// holding, as `[engine] delist_exit_hours` live.
    #[arg(long, default_value_t = 24)]
    delist_exit_hours: u64,
    /// Candles of history kept per instrument for the strategy. As many
    /// cached bars before `--from` are replayed first as warm-up (history,
    /// no decisions), as `run` does before trading.
    #[arg(long, default_value_t = 500)]
    history_len: usize,
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,
    /// `binance-usdm`: leverage every position is margined at (≥ 1, at
    /// most what the cached brackets allow at the order's size).
    #[arg(long)]
    leverage: Option<u32>,
    /// `binance-usdm`: margin mode.
    #[arg(long, value_enum, default_value = "isolated")]
    margin: MarginArg,
    /// `binance-usdm`: liquidation clearance fee rate; default the highest
    /// cached `liquidationFee` of the symbols.
    #[arg(long, value_parser = parse_decimal)]
    clearance: Option<Decimal>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum MarginArg {
    Isolated,
    Cross,
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
    let venue = a.venue.id();
    let ids: Vec<InstrumentId> = a
        .symbol
        .iter()
        .map(|s| InstrumentId::new(venue.clone(), s.clone()))
        .collect();
    let perp = a.venue == Venue::BinanceUsdm;
    // Futures charge every fee in the quote (margin) asset.
    let fee_asset = if perp {
        FeeAsset::Quote
    } else {
        FeeAsset::Received
    };
    let fees = RateFee::new(a.fee, a.fee, fee_asset).context("fee must be in [0, 1)")?;
    let slippage = BpsSlippage::new(a.slippage_bps).context("slippage must be >= 0")?;
    let dir = DataDir::new(a.data_dir.clone());
    let provider = BacktestProvider::from_cache(
        &dir,
        &ids,
        a.interval,
        a.from,
        a.to,
        PaperConfig {
            fees,
            slippage,
            initial_cash: Notional::new(a.initial_cash),
        },
    )?
    // As `run` warms up `history_len` bars before its first decision.
    .warm_up_from_cache(&dir, a.interval, a.from, a.history_len)?;
    let reference = a
        .delistings
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("config/delistings/{}.toml", venue.as_str())));
    let announced = if reference.exists() {
        backtest::read_delistings(&reference, &venue)?
    } else if a.delistings.is_some() {
        anyhow::bail!("no delistings file at {}", reference.display());
    } else {
        Vec::new()
    };
    let mut provider = provider.with_delistings(&announced).cut_at_gaps(
        Duration::from_secs(a.max_gap_hours.saturating_mul(3600)),
        a.to,
    );
    if perp {
        provider = with_perp(provider, &dir, &ids, &a)?;
    } else if a.leverage.is_some() || a.clearance.is_some() {
        anyhow::bail!("--leverage and --clearance are for --venue binance-usdm");
    }
    if let Some(ratio) = a.earn_ratio {
        anyhow::ensure!(
            !perp,
            "--earn-ratio is spot only (Simple Earn holds spot cash)"
        );
        let apr = a.earn_apr.unwrap_or(Decimal::ZERO);
        anyhow::ensure!(
            ratio >= Decimal::ZERO && ratio < Decimal::ONE,
            "--earn-ratio must be in [0, 1)"
        );
        anyhow::ensure!(apr >= Decimal::ZERO, "--earn-apr must be >= 0");
        provider = provider.with_earn(backtest::EarnModel { ratio, apr });
    }
    for e in provider.events() {
        if let MarketEvent::Delisting {
            instrument,
            at: Some(at),
            ts,
        } = e
        {
            eprintln!(
                "note: {instrument} delists at {} (known from {})",
                date::format(*at),
                date::format(*ts)
            );
        }
    }
    for (id, n) in provider.warmup_bars() {
        if n < a.history_len {
            eprintln!(
                "warning: {id}: {n} of {} warm-up bars cached before --from; the first \
                 decisions see a shorter history than a live run would (fetch-data from earlier)",
                a.history_len
            );
        }
    }
    let mut strategy = strategy::build(&a.strategy, &params)?;
    let config = EngineConfig {
        whitelist: ids,
        history_len: a.history_len,
        delist_exit: Duration::from_secs(a.delist_exit_hours.saturating_mul(3600)),
    };
    let report = run_backtest(&provider, strategy.as_mut(), params, &config, a.interval).await?;
    println!("{report}");
    println!("(model output under the stated fill model; unverified against live)");
    Ok(())
}

/// A perpetual backtest's margin terms (cached brackets, `--leverage`,
/// `--margin`) and funding replay (cached history; missing marks stood in
/// by last price, noted).
fn with_perp(
    provider: BacktestProvider,
    dir: &DataDir,
    ids: &[InstrumentId],
    a: &BacktestArgs,
) -> anyhow::Result<BacktestProvider> {
    use bot_core::margin::MarginMode;
    let leverage = a
        .leverage
        .context("--venue binance-usdm needs --leverage")?;
    anyhow::ensure!(leverage >= 1, "--leverage must be >= 1");
    let mode = match a.margin {
        MarginArg::Isolated => MarginMode::Isolated,
        MarginArg::Cross => MarginMode::Cross,
    };
    let mut terms = std::collections::HashMap::new();
    let mut clearance = Decimal::ZERO;
    let mut events = Vec::new();
    for id in ids {
        let data = dir
            .read_perp(id)
            .with_context(|| format!("{id}: run fetch-data --venue binance-usdm"))?;
        let brackets = data.brackets.with_context(|| {
            format!("{id}: no margin brackets cached; fetch-data --venue binance-usdm with BINANCE_API_KEY/SECRET set")
        })?;
        clearance = clearance.max(data.liquidation_fee);
        terms.insert(
            id.clone(),
            exchange::paper::PerpTerms {
                leverage,
                mode,
                brackets,
            },
        );
        let rates = dir.read_funding(id)?;
        if rates.is_empty() {
            eprintln!("warning: {id}: no funding history cached: funding not modelled");
        }
        let (ev, estimated) =
            backtest::funding_events(id, &rates, provider.candles(), a.from, a.to);
        if estimated > 0 {
            eprintln!(
                "note: {id}: {estimated} funding settlements without a venue mark price valued at last price"
            );
        }
        events.extend(ev);
    }
    let clearance = a.clearance.unwrap_or(clearance);
    anyhow::ensure!(
        clearance >= Decimal::ZERO && clearance < Decimal::ONE,
        "--clearance must be in [0, 1)"
    );
    Ok(provider
        .with_perp(backtest::PerpSetup { terms, clearance })
        .with_events(events))
}

#[derive(Args)]
struct FetchArgs {
    /// Spot symbols, comma-separated (`BTCUSDT,ETHUSDT`).
    #[arg(long, value_delimiter = ',', required = true)]
    symbol: Vec<Symbol>,
    /// Bar width (`1s`, `1m`, `5m`, `15m`, `1h`, `4h`, `1d`). `1s` is 86,400
    /// bars per day per symbol: fetch short ranges.
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
    /// Venue to download from.
    #[arg(long, value_enum, default_value = "binance")]
    venue: Venue,
    /// REST host for public market data; default follows `--venue`.
    #[arg(long)]
    base_url: Option<String>,
    /// `binance-usdm`: whose host the account's margin brackets come from
    /// (signed; keys from `BINANCE_API_KEY` / `BINANCE_API_SECRET`, which
    /// belong to one network).
    #[arg(long, value_enum, default_value = "demo")]
    brackets_from: BracketsHost,
}

/// Which Binance futures host `fetch-data` reads margin brackets from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum BracketsHost {
    Live,
    Demo,
    Testnet,
}

fn parse_interval(s: &str) -> anyhow::Result<Interval> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .with_context(|| format!("interval {s:?}: want 1s, 1m, 5m, 15m, 1h, 4h or 1d"))
}

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    Timestamp::from_millis(ms)
}

async fn fetch_data(a: FetchArgs) -> anyhow::Result<()> {
    let base = a.base_url.unwrap_or_else(|| data_url(a.venue).to_owned());
    let client: Box<dyn HistorySource> = match a.venue {
        Venue::Binance => Box::new(PublicClient::new(base.clone())),
        Venue::Max => Box::new(MaxPublicClient::new(base.clone())),
        Venue::BinanceUsdm => Box::new(PublicClient::usdm(base.clone())),
    };
    let dir = DataDir::new(a.data_dir.clone());
    let to = a.to.unwrap_or_else(now);
    let venue = a.venue.id();
    for symbol in a.symbol {
        let id = InstrumentId::new(venue.clone(), symbol);
        let s = fetch(client.as_ref(), &dir, &id, a.interval, a.from, to)
            .await
            .with_context(|| format!("fetching {id}"))?;
        println!(
            "{id} {}: {} downloaded, {} cached",
            a.interval.code(),
            s.downloaded,
            s.cached
        );
        if a.venue == Venue::BinanceUsdm {
            fetch_perp_data(&base, &dir, &id, a.from, to, a.brackets_from).await?;
        }
    }
    Ok(())
}

/// Funding history, clearance fee and (with a key) margin brackets of a
/// perpetual. Brackets are the account's (`/fapi/v1/leverageBracket`,
/// signed) from `brackets_from`'s host; without `BINANCE_API_KEY` /
/// `BINANCE_API_SECRET`, or if that call fails, the cached ones are kept
/// (a warning) and a backtest without any refuses to run.
async fn fetch_perp_data(
    base: &str,
    dir: &DataDir,
    id: &InstrumentId,
    from: Timestamp,
    to: Timestamp,
    brackets_from: BracketsHost,
) -> anyhow::Result<()> {
    use exchange::binance::usdm;
    let client = PublicClient::usdm(base.to_owned());
    let f = backtest::fetch_funding(&client, dir, id, from, to)
        .await
        .with_context(|| format!("fetching {id} funding"))?;
    println!(
        "{id} funding: {} downloaded, {} cached",
        f.downloaded, f.cached
    );
    let info = client.perp_instrument(id.symbol()).await?;
    let mut brackets = dir.read_perp(id).ok().and_then(|p| p.brackets);
    let host = match brackets_from {
        BracketsHost::Live => usdm::LIVE_URL,
        BracketsHost::Demo => usdm::DEMO_URL,
        BracketsHost::Testnet => usdm::TESTNET_URL,
    };
    match Credentials::from_env() {
        Ok(creds) => {
            let signed = exchange::binance::SignedClient::new(
                host,
                Box::new(exchange::binance::ReqwestTransport::default()),
                creds,
                Box::new(exchange::binance::SystemClock),
            );
            match usdm::brackets(&signed, id.symbol()).await {
                Ok(b) => {
                    println!(
                        "{id} margin brackets: {} tiers from {host}",
                        b.iter().count()
                    );
                    brackets = Some(b);
                }
                Err(e) => eprintln!(
                    "warning: {id}: margin brackets from {host}: {e}; kept the cached ones"
                ),
            }
        }
        Err(_) => eprintln!(
            "warning: {id}: no BINANCE_API_KEY/SECRET: margin brackets not fetched (signed endpoint); kept the cached ones"
        ),
    }
    dir.write_perp(
        id,
        &backtest::PerpData {
            liquidation_fee: info.liquidation_fee,
            brackets,
        },
    )?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rust_log = std::env::var("RUST_LOG").ok();
    match cli.command {
        Command::Run(a) => {
            // Config first: it may set the level. Loading logs nothing, and
            // a bad config is reported as main's error. A live run must
            // show its orders and fills by default; sqlx logs every SQL
            // statement at info.
            let cfg = config::Config::load(a.config.as_deref())?;
            logging::init(
                &config::log_filter(rust_log, cfg.log.level.as_deref(), "info,sqlx=warn"),
                &cfg.log,
            );
            let ran = run_bot(a, cfg).await;
            // Also as a log line, so the log file (and whatever ships it)
            // says why the bot stopped, not only the terminal.
            if let Err(e) = &ran {
                tracing::error!(error = format!("{e:#}"), "run stopped");
            }
            ran
        }
        // A backtest logging every simulated order would bury its report.
        Command::FetchData(a) => {
            init_logging(&config::log_filter(rust_log, None, "warn"));
            fetch_data(a).await
        }
        Command::Backtest(a) => {
            init_logging(&config::log_filter(rust_log, None, "warn"));
            backtest(a).await
        }
    }
}

/// Text logs on the console with `filter` (already validated): for the
/// commands without a config file.
fn init_logging(filter: &str) {
    logging::init(filter, &config::LogSection::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venue_picks_ids_and_data_host() {
        assert_eq!(Venue::Binance.id().as_str(), "binance");
        assert_eq!(Venue::Max.id().as_str(), "max");
        assert_eq!(data_url(Venue::Binance), "https://data-api.binance.vision");
        assert_eq!(data_url(Venue::Max), "https://max-api.maicoin.com");
        let cli = Cli::try_parse_from([
            "cli",
            "fetch-data",
            "--venue",
            "max",
            "--symbol",
            "BTCTWD",
            "--from",
            "2024-01-01",
        ])
        .unwrap();
        let Command::FetchData(a) = cli.command else {
            panic!("not fetch-data");
        };
        assert_eq!((a.venue, a.base_url), (Venue::Max, None));
    }

    #[test]
    fn perpetual_venue_parses_with_margin_flags() {
        assert_eq!(Venue::BinanceUsdm.id().as_str(), "binance-usdm");
        assert_eq!(data_url(Venue::BinanceUsdm), "https://fapi.binance.com");
        let cli = Cli::try_parse_from([
            "cli",
            "fetch-data",
            "--venue",
            "binance-usdm",
            "--symbol",
            "BTCUSDT",
            "--from",
            "2024-01-01",
            "--brackets-from",
            "testnet",
        ])
        .unwrap();
        let Command::FetchData(a) = cli.command else {
            panic!("not fetch-data");
        };
        assert_eq!(
            (a.venue, a.brackets_from),
            (Venue::BinanceUsdm, BracketsHost::Testnet)
        );
        let cli = Cli::try_parse_from([
            "cli",
            "backtest",
            "--venue",
            "binance-usdm",
            "--strategy",
            "buy_and_hold",
            "--symbol",
            "BTCUSDT",
            "--from",
            "2024-01-01",
            "--to",
            "2024-02-01",
            "--slippage-bps",
            "2",
            "--leverage",
            "3",
            "--margin",
            "cross",
            "--clearance",
            "0.0125",
        ])
        .unwrap();
        let Command::Backtest(b) = cli.command else {
            panic!("not backtest");
        };
        assert_eq!((b.leverage, b.margin), (Some(3), MarginArg::Cross));
        assert_eq!(b.clearance, Some(Decimal::new(125, 4)));
    }

    /// A perpetual backtest without its margin data is refused with a hint,
    /// never run on made-up brackets.
    #[test]
    fn perpetual_backtest_needs_cached_brackets() {
        let dir = std::env::temp_dir().join(format!("tb-perp-cli-{}", std::process::id()));
        let data = DataDir::new(&dir);
        let id: InstrumentId = "binance-usdm:BTCUSDT".parse().unwrap();
        let cli = Cli::try_parse_from([
            "cli",
            "backtest",
            "--venue",
            "binance-usdm",
            "--strategy",
            "buy_and_hold",
            "--symbol",
            "BTCUSDT",
            "--from",
            "2024-01-01",
            "--to",
            "2024-02-01",
            "--slippage-bps",
            "2",
            "--leverage",
            "3",
        ])
        .unwrap();
        let Command::Backtest(a) = cli.command else {
            panic!("not backtest");
        };
        let provider = || {
            BacktestProvider::new(
                vec![],
                vec![],
                PaperConfig {
                    fees: RateFee::new(Decimal::ZERO, Decimal::ZERO, FeeAsset::Quote).unwrap(),
                    slippage: BpsSlippage::zero(),
                    initial_cash: Notional::new(Decimal::ONE),
                },
            )
        };
        let err = with_perp(provider(), &data, std::slice::from_ref(&id), &a)
            .err()
            .unwrap();
        assert!(
            format!("{err:#}").contains("fetch-data --venue binance-usdm"),
            "{err:#}"
        );
        data.write_perp(
            &id,
            &backtest::PerpData {
                liquidation_fee: Decimal::new(125, 4),
                brackets: None,
            },
        )
        .unwrap();
        let err = with_perp(provider(), &data, std::slice::from_ref(&id), &a)
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("no margin brackets"), "{err:#}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
