//! Bot binary: `run | backtest | fetch-data | migrate`.
//!
//! Safe by default: paper against testnet. Live trading requires
//! `mode = "live"`, `exchange.network = "live"`, and `--i-understand`.

mod config;
mod console;
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
#[command(
    name = "trade-bot",
    bin_name = "trade-bot",
    version,
    about = "Event-driven trading bot"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download public Binance klines and instrument filters into the cache.
    FetchData(FetchArgs),
    /// Replay cached klines through the engine with a paper broker.
    Backtest(Box<BacktestArgs>),
    /// Trade on Binance: testnet or demo (`--paper`), mainnet only behind
    /// the live gate.
    Run(RunArgs),
    /// Local web console: edit script strategies, backtest them against
    /// buy-and-hold. Loopback only; places no orders.
    Console(console::ConsoleArgs),
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
    /// Start although the kill switch stopped this strategy before
    /// (removes its marker under `data/kill-switch/`).
    #[arg(long)]
    clear_kill_switch: bool,
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
    // Spot and futures in one run aren't built: started, it would see one
    // leg's instrument and quietly do nothing.
    anyhow::ensure!(
        cfg.strategy.name != "funding_carry",
        "funding_carry trades Binance spot and USDⓈ-M together; a run trades one venue, \
         so it is backtest only for now (backtest --perp-symbol)"
    );
    if cfg.exchange.venue == Venue::BinanceUsdm {
        let venue_fills = cfg
            .perp
            .as_ref()
            .is_some_and(|p| p.fills == config::PerpFills::Venue);
        if venue_fills {
            // The same three locks as spot: --paper = testnet/demo only.
            let network = gate::decide(cfg.mode, cfg.exchange.network, a.live, a.i_understand)?;
            return run_usdm_orders(cfg, network, a.cancel_orphans).await;
        }
        anyhow::ensure!(
            !a.live,
            "perp.fills = \"local\" is a paper run; set perp.fills = \"venue\" for orders on Binance"
        );
        if a.cancel_orphans {
            tracing::warn!(
                "--cancel-orphans ignored: a local-fill paper run places no orders on Binance"
            );
        }
        return run_usdm_paper(cfg).await;
    }
    if cfg.exchange.venue == Venue::Etoro {
        // The same three locks: --paper = demo account only.
        let network = gate::decide(cfg.mode, cfg.exchange.network, a.live, a.i_understand)?;
        return run_etoro(cfg, network).await;
    }
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
    // Fail before connecting: a public API without a token is refused.
    config::api_token(&cfg.api, std::env::var(config::API_TOKEN_VAR).ok())?;
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
    let (mut observer, resume) = observe_run(&cfg, books.clone(), network).await?;
    let params = Params::new(cfg.strategy.params);
    let mut strategy = strategy::build(&cfg.strategy.name, &params)?;
    let engine_cfg = EngineConfig {
        whitelist: cfg.whitelist,
        history_len: cfg.engine.history_len,
        delist_exit: cfg.engine.delist_exit(),
        kill_switch: cfg.engine.kill_switch(),
    };
    if network == Network::Live {
        eprintln!("LIVE: sending real orders to Binance mainnet");
    }
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
    let killed = finish_run(outcome?);
    if let Some(saver) = observer.state.take() {
        saver.finish().await;
    }
    closed?;
    tripped(killed)
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
        kill_switch: cfg.engine.kill_switch(),
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
    let killed = finish_run(outcome?);
    tripped(killed)
}

/// A USDⓈ-M paper run: live futures prices and funding, the local perp
/// broker with `[paper]` costs and `[perp]` margin; brackets read from
/// the account (signed, keys from env). Like MAX paper: no db, no API, no
/// state resume (the broker starts flat), nothing sent to Binance but
/// data requests.
async fn run_usdm_paper(cfg: config::Config) -> anyhow::Result<()> {
    use exchange::binance::usdm_paper::{UsdmPaperConfig, UsdmPaperProvider};
    let paper = cfg
        .paper
        .as_ref()
        .context("exchange.venue = \"binance-usdm\" needs a [paper] section")?;
    let perp = cfg
        .perp
        .as_ref()
        .context("exchange.venue = \"binance-usdm\" needs a [perp] section")?;
    let signed = usdm_signed(perp.brackets_from)
        .context("a perpetual paper run reads the account's margin brackets (signed)")?;
    let mut terms = std::collections::HashMap::new();
    for id in &cfg.whitelist {
        let brackets = exchange::binance::usdm::brackets(&signed, id.symbol())
            .await
            .with_context(|| format!("{id}: margin brackets"))?;
        terms.insert(
            id.clone(),
            exchange::paper::PerpTerms {
                leverage: perp.leverage,
                mode: perp.margin,
                brackets,
            },
        );
    }
    let provider = UsdmPaperProvider::new(
        UsdmPaperConfig {
            interval: cfg.exchange.interval,
            warmup: cfg.engine.history_len,
            max_outage: Duration::from_secs(cfg.exchange.max_outage_secs),
            http_timeout: Duration::from_secs(cfg.exchange.http_timeout_secs),
            fees: RateFee::new(paper.fee, paper.fee, FeeAsset::Quote)
                .context("paper.fee must be in [0, 1)")?,
            slippage: BpsSlippage::new(paper.slippage_bps)
                .context("paper.slippage_bps must be >= 0")?,
            initial_cash: paper.initial_cash,
            terms,
            clearance: perp.clearance,
        },
        cfg.whitelist.clone(),
    );
    if cfg.api.enabled {
        tracing::warn!(
            "api is not available on a perpetual paper run (no order database); not started"
        );
    }
    let params = Params::new(cfg.strategy.params);
    let mut strategy = strategy::build(&cfg.strategy.name, &params)?;
    let engine_cfg = EngineConfig {
        whitelist: cfg.whitelist,
        history_len: cfg.engine.history_len,
        delist_exit: cfg.engine.delist_exit(),
        kill_switch: cfg.engine.kill_switch(),
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
        leverage = perp.leverage,
        "starting: Binance futures prices and funding, local paper fills and margin, nothing sent to Binance"
    );
    let outcome = tokio::select! {
        r = engine::run_observed(&provider, strategy.as_mut(), params, &engine_cfg, &mut observer, None) => r.map(Some),
        s = shutdown::signal() => {
            eprintln!("{}: stopping", s?);
            Ok(None)
        }
    };
    let killed = finish_run(outcome?);
    tripped(killed)
}

/// What a run with books observes: saved strategy state (loaded for
/// resume under the same params, then written after every event), the
/// metrics exporter, and the read-only API when `[api]` enables it (a
/// bind failure is a warning; the API never stops trading).
async fn observe_run(
    cfg: &config::Config,
    books: Arc<SqlStore>,
    network: Network,
) -> anyhow::Result<(telemetry::RunObserver, Option<serde_json::Value>)> {
    let api_token = config::api_token(&cfg.api, std::env::var(config::API_TOKEN_VAR).ok())?;
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
            books,
            token: api_token,
        };
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
    Ok((
        telemetry::RunObserver {
            include_money: cfg.metrics.include_money,
            live,
            state: Some(saver),
        },
        resume,
    ))
}

/// A USDⓈ-M run with orders on `network`'s futures exchange (gated by
/// the caller): account set up to `[perp]`, reconciled at start, open
/// orders cancelled at exit (positions stay open on Binance). Books in
/// their own database (`data/<network>-usdm.db` unless `db.url`), so a
/// spot run never meets a futures journal row. Strategy state and the
/// API as a spot run ([`observe_run`]).
async fn run_usdm_orders(
    cfg: config::Config,
    network: Network,
    cancel_orphans: bool,
) -> anyhow::Result<()> {
    use exchange::binance::usdm_live::{UsdmConfig, UsdmProvider};
    let perp = cfg
        .perp
        .as_ref()
        .context("exchange.venue = \"binance-usdm\" needs a [perp] section")?;
    let creds = Credentials::from_env()?;
    let url = cfg.db.url.clone().unwrap_or_else(|| {
        format!(
            "sqlite://data/{}-usdm.db?mode=rwc",
            config::network_name(network)
        )
    });
    if cfg.db.url.is_none() {
        std::fs::create_dir_all("data").context("creating data/")?;
    }
    let books = SqlStore::open(&url).await.context("opening the database")?;
    books.bind_network(config::network_name(network)).await?;
    let books = Arc::new(books);
    let books_ref = books.clone();
    let provider = UsdmProvider::new(
        UsdmConfig {
            network,
            interval: cfg.exchange.interval,
            warmup: cfg.engine.history_len,
            max_outage: Duration::from_secs(cfg.exchange.max_outage_secs),
            http_timeout: Duration::from_secs(cfg.exchange.http_timeout_secs),
            leverage: perp.leverage,
            mode: perp.margin,
        },
        creds,
        cfg.whitelist.clone(),
        books,
    )
    .with_orphans(if cancel_orphans {
        Orphans::Cancel
    } else {
        Orphans::Refuse
    });
    let (mut observer, resume) = observe_run(&cfg, books_ref.clone(), network).await?;
    let params = Params::new(cfg.strategy.params);
    let mut strategy = strategy::build(&cfg.strategy.name, &params)?;
    let engine_cfg = EngineConfig {
        whitelist: cfg.whitelist,
        history_len: cfg.engine.history_len,
        delist_exit: cfg.engine.delist_exit(),
        kill_switch: cfg.engine.kill_switch(),
    };
    if network == Network::Live {
        eprintln!("LIVE: sending real orders to Binance USDⓈ-M futures");
    }
    tracing::info!(
        provider = provider.id(),
        network = config::network_name(network),
        strategy = %cfg.strategy.name,
        leverage = perp.leverage,
        run_tag = provider.run_tag(),
        "starting"
    );
    let outcome = tokio::select! {
        r = engine::run_observed(&provider, strategy.as_mut(), params, &engine_cfg, &mut observer, resume) => r.map(Some),
        s = shutdown::signal() => {
            eprintln!("{}: stopping; cancelling open orders (signal again to skip)", s?);
            Ok(None)
        }
    };
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
            "closed out open orders; positions stay open on Binance"
        ),
        Err(e) => eprintln!("could not close out open orders: {e}; check the account"),
    }
    let killed = finish_run(outcome?);
    closed?;
    tripped(killed)
}

/// An eToro run: the demo account for a fake-money network, the real one
/// for `live` (gated by the caller). Books in `data/<network>-etoro.db`
/// unless `db.url`; strategy state and the API as a spot run. At exit,
/// orders still in flight are settled and recorded; positions stay.
async fn run_etoro(cfg: config::Config, network: Network) -> anyhow::Result<()> {
    use exchange::etoro::feed::{EtoroConfig, EtoroProvider};
    use exchange::etoro::{Account, EtoroClient, EtoroKeys, REST_URL};
    let account = if network == Network::Live {
        Account::Real
    } else {
        Account::Demo
    };
    let client = EtoroClient::new(
        REST_URL,
        EtoroKeys::from_env()?,
        account,
        Duration::from_secs(cfg.exchange.http_timeout_secs),
    );
    let url = cfg.db.url.clone().unwrap_or_else(|| {
        format!(
            "sqlite://data/{}-etoro.db?mode=rwc",
            config::network_name(network)
        )
    });
    if cfg.db.url.is_none() {
        std::fs::create_dir_all("data").context("creating data/")?;
    }
    let books = SqlStore::open(&url).await.context("opening the database")?;
    books.bind_network(config::network_name(network)).await?;
    let books = Arc::new(books);
    let provider = EtoroProvider::new(
        client,
        EtoroConfig {
            interval: cfg.exchange.interval,
            warmup: cfg.engine.history_len,
            max_outage: Duration::from_secs(cfg.exchange.max_outage_secs),
            w8ben_signed: cfg.etoro.as_ref().is_some_and(|e| e.w8ben_signed),
        },
        cfg.whitelist.clone(),
        books.clone(),
    );
    let (mut observer, resume) = observe_run(&cfg, books, network).await?;
    let params = Params::new(cfg.strategy.params);
    let mut strategy = strategy::build(&cfg.strategy.name, &params)?;
    let engine_cfg = EngineConfig {
        whitelist: cfg.whitelist,
        history_len: cfg.engine.history_len,
        delist_exit: cfg.engine.delist_exit(),
        kill_switch: cfg.engine.kill_switch(),
    };
    if account == Account::Real {
        eprintln!("LIVE: sending real orders to eToro");
    }
    tracing::info!(
        provider = provider.id(),
        account = ?account,
        strategy = %cfg.strategy.name,
        run_tag = provider.run_tag(),
        "starting"
    );
    let outcome = tokio::select! {
        r = engine::run_observed(&provider, strategy.as_mut(), params, &engine_cfg, &mut observer, resume) => r.map(Some),
        s = shutdown::signal() => {
            eprintln!("{}: stopping; settling orders in flight (signal again to skip)", s?);
            Ok(None)
        }
    };
    let closed = match shutdown::close_out_unless(provider.close_out(), shutdown::signal()).await {
        shutdown::CloseOut::Done(r) => r.map_err(anyhow::Error::from),
        shutdown::CloseOut::Abandoned(s) => Err(anyhow::anyhow!(
            "{s} during close-out: abandoned; the next start settles orders in flight"
        )),
    };
    match &closed {
        Ok(n) => tracing::info!(
            fills_recovered = n,
            "settled orders in flight; positions stay on eToro"
        ),
        Err(e) => eprintln!("could not settle orders in flight: {e}; check the account"),
    }
    let killed = finish_run(outcome?);
    closed?;
    tripped(killed)
}

/// A signed client of `network`'s USDⓈ-M host, keys from env.
fn usdm_signed(network: Network) -> anyhow::Result<exchange::binance::SignedClient> {
    use exchange::binance::usdm;
    let host = match network {
        Network::Live => usdm::LIVE_URL,
        Network::Demo => usdm::DEMO_URL,
        Network::Testnet => usdm::TESTNET_URL,
    };
    Ok(exchange::binance::SignedClient::new(
        host,
        Box::new(exchange::binance::ReqwestTransport::default()),
        Credentials::from_env()?,
        Box::new(exchange::binance::SystemClock),
    ))
}

/// Prints the summary of a run that ended on its own (`None`: stopped by
/// a signal); the kill switch's reason if it tripped.
fn finish_run(report: Option<engine::RunReport>) -> Option<String> {
    let r = report?;
    print_summary(&r);
    r.killed
}

/// A run ended by its kill switch (`[engine] kill_drawdown`): an error,
/// so the process exits non-zero, and `run` leaves a marker that refuses
/// the next start (a supervisor restarting it must not trade on).
#[derive(Debug)]
struct KillSwitchTripped(String);

impl std::fmt::Display for KillSwitchTripped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "kill switch tripped: {}; every position closed, strategy stopped",
            self.0
        )
    }
}

impl std::error::Error for KillSwitchTripped {}

/// `Err` when the kill switch ended the run.
fn tripped(killed: Option<String>) -> anyhow::Result<()> {
    match killed {
        Some(why) => Err(KillSwitchTripped(why).into()),
        None => Ok(()),
    }
}

/// Where a tripped kill switch is remembered for this venue, network and
/// strategy (`data/kill-switch/<venue>-<network>-<strategy>.txt`).
fn kill_marker(cfg: &config::Config) -> PathBuf {
    PathBuf::from("data/kill-switch").join(format!(
        "{}-{}-{}.txt",
        cfg.exchange.venue.id().as_str(),
        format!("{:?}", cfg.exchange.network).to_lowercase(),
        cfg.strategy.name
    ))
}

/// Refuses to start while `marker` exists, unless `clear` (which removes
/// it): a kill switch stays tripped until a person says otherwise.
fn check_kill_marker(marker: &std::path::Path, clear: bool) -> anyhow::Result<()> {
    let Ok(text) = std::fs::read_to_string(marker) else {
        return Ok(());
    };
    if clear {
        std::fs::remove_file(marker).with_context(|| format!("removing {}", marker.display()))?;
        tracing::warn!(marker = %marker.display(), "kill switch cleared by --clear-kill-switch");
        return Ok(());
    }
    anyhow::bail!(
        "the kill switch stopped this strategy earlier ({}); review it, then start with \
         --clear-kill-switch (or delete {})",
        text.trim(),
        marker.display()
    )
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
        Venue::Etoro => exchange::etoro::REST_URL,
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
    /// Default 48; 120 on eToro (stock markets close over weekends and
    /// holidays).
    #[arg(long)]
    max_gap_hours: Option<u64>,
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
    /// Kill switch, as `[engine] kill_drawdown` live: once equity falls
    /// this fraction below its peak (from `--initial-cash`), the strategy
    /// is stopped and everything closed (`0.25` = 25%).
    #[arg(long, value_parser = parse_decimal)]
    kill_drawdown: Option<Decimal>,
    /// Hedge: USDⓈ-M perpetuals traded beside the spot `--symbol`s
    /// (`--venue binance`), in their own wallet (`--perp-cash`), with
    /// `--leverage`, `--margin` and `--perp-fee`; funding and mark prices
    /// from the `binance-usdm` cache.
    #[arg(long, value_delimiter = ',')]
    perp_symbol: Vec<Symbol>,
    /// The futures wallet of a hedge, in USDT, apart from `--initial-cash`
    /// (spot): Binance keeps them apart and the bot never transfers.
    #[arg(long, value_parser = parse_decimal)]
    perp_cash: Option<Decimal>,
    /// A hedge's futures fee rate, in the quote (default Binance's 0.05%
    /// taker).
    #[arg(long, default_value = "0.0005", value_parser = parse_decimal)]
    perp_fee: Decimal,
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
    let (report, notes) = backtest_report(&a, params).await?;
    for n in notes {
        eprintln!("{n}");
    }
    println!("{report}");
    println!("(model output under the stated fill model; unverified against live)");
    Ok(())
}

/// Runs the backtest `a` describes: the report, and the notes and warnings
/// about the data it ran on (shorter warm-up, delistings replayed, funding
/// estimated), each a line for the user. `backtest` prints them; the
/// console shows them beside the result.
async fn backtest_report(
    a: &BacktestArgs,
    params: Params,
) -> anyhow::Result<(backtest::Report, Vec<String>)> {
    let mut notes = Vec::new();
    let venue = a.venue.id();
    let mut ids: Vec<InstrumentId> = a
        .symbol
        .iter()
        .map(|s| InstrumentId::new(venue.clone(), s.clone()))
        .collect();
    let perp = a.venue == Venue::BinanceUsdm;
    // A hedge: Binance spot symbols plus USDⓈ-M ones, two wallets.
    let hedged: Vec<InstrumentId> = a
        .perp_symbol
        .iter()
        .map(|s| InstrumentId::new(Venue::BinanceUsdm.id(), s.clone()))
        .collect();
    if !hedged.is_empty() {
        anyhow::ensure!(
            a.venue == Venue::Binance,
            "--perp-symbol hedges Binance spot: use --venue binance"
        );
        anyhow::ensure!(
            a.perp_cash.is_some(),
            "--perp-symbol needs --perp-cash (the futures wallet, apart from --initial-cash)"
        );
        ids.extend(hedged.iter().cloned());
    } else if a.perp_cash.is_some() {
        anyhow::bail!("--perp-cash is for --perp-symbol");
    }
    // Futures charge every fee in the quote (margin) asset.
    // Futures and eToro charge in the quote (USD).
    let fee_asset = if perp || a.venue == Venue::Etoro {
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
        Duration::from_secs(
            a.max_gap_hours
                .unwrap_or(if a.venue == Venue::Etoro { 120 } else { 48 })
                .saturating_mul(3600),
        ),
        a.to,
    );
    if a.venue == Venue::Etoro {
        // eToro's resting limit is market-if-touched; IOC and stops refused.
        provider = provider.with_touch_fills();
    }
    if perp {
        let (setup, events) = perp_setup(&provider, &dir, &ids, a, &mut notes)?;
        provider = provider.with_perp(setup).with_events(events);
    } else if !hedged.is_empty() {
        let (setup, events) = perp_setup(&provider, &dir, &hedged, a, &mut notes)?;
        let fees = RateFee::new(a.perp_fee, a.perp_fee, FeeAsset::Quote)
            .context("--perp-fee must be in [0, 1)")?;
        provider = provider
            .with_hedge(backtest::HedgeSetup {
                perp: setup,
                wallet: Notional::new(a.perp_cash.unwrap_or(Decimal::ZERO)),
                fees,
            })
            .with_events(events);
    } else if a.leverage.is_some() || a.clearance.is_some() {
        anyhow::bail!("--leverage and --clearance are for --venue binance-usdm or --perp-symbol");
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
            notes.push(format!(
                "note: {instrument} delists at {} (known from {})",
                date::format(*at),
                date::format(*ts)
            ));
        }
    }
    for (id, n) in provider.warmup_bars() {
        if n < a.history_len {
            notes.push(format!(
                "warning: {id}: {n} of {} warm-up bars cached before --from; the first \
                 decisions see a shorter history than a live run would (fetch-data from earlier)",
                a.history_len
            ));
        }
    }
    notes.extend(suspect_notes(&provider));
    let mut strategy = strategy::build(&a.strategy, &params)?;
    let config = EngineConfig {
        whitelist: ids,
        history_len: a.history_len,
        delist_exit: Duration::from_secs(a.delist_exit_hours.saturating_mul(3600)),
        kill_switch: a.kill_drawdown.map(|d| engine::KillSwitch {
            capital: Notional::new(a.initial_cash),
            max_drawdown: d,
        }),
    };
    let report = run_backtest(&provider, strategy.as_mut(), params, &config, a.interval).await?;
    Ok((report, notes))
}

/// Moves above this between bars are reported before a backtest.
const SUSPECT_MOVE: Decimal = Decimal::from_parts(3, 0, 0, false, 1);
/// Suspect bars listed per symbol; the rest are counted.
const SUSPECT_SHOWN: usize = 3;

/// Warnings for bars that moved more than 30% (warm-up included): a split
/// the history doesn't adjust for, or a bad print. Reported, never
/// corrected; the strategy trades them as real.
fn suspect_notes(provider: &BacktestProvider) -> Vec<String> {
    let bars: Vec<bot_core::Candle> = provider.all_candles().cloned().collect();
    let moves = backtest::suspect_moves(&bars, SUSPECT_MOVE);
    let mut notes = Vec::new();
    for chunk in moves.chunk_by(|a, b| a.instrument == b.instrument) {
        for m in chunk.iter().take(SUSPECT_SHOWN) {
            notes.push(format!(
                "warning: {}: {} → {} on {} ({:+.1}%): a split, or a bad bar? \
                 The backtest trades it as real; check the series or pick a window without it",
                m.instrument,
                m.from,
                m.to,
                date::format(m.at),
                m.change() * Decimal::ONE_HUNDRED
            ));
        }
        if chunk.len() > SUSPECT_SHOWN {
            notes.push(format!(
                "warning: {}: {} more bars moved over 30%",
                chunk[0].instrument,
                chunk.len() - SUSPECT_SHOWN
            ));
        }
    }
    notes
}

/// A perpetual backtest's margin terms (cached brackets, `--leverage`,
/// `--margin`) and the funding and mark-price events to replay (cached
/// history; missing marks stood in by last price, noted), for `ids`.
fn perp_setup(
    provider: &BacktestProvider,
    dir: &DataDir,
    ids: &[InstrumentId],
    a: &BacktestArgs,
    notes: &mut Vec<String>,
) -> anyhow::Result<(backtest::PerpSetup, Vec<MarketEvent>)> {
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
            notes.push(format!(
                "warning: {id}: no funding history cached: funding not modelled"
            ));
        }
        let (ev, estimated) =
            backtest::funding_events(id, &rates, provider.candles(), a.from, a.to);
        if estimated > 0 {
            notes.push(format!(
                "note: {id}: {estimated} funding settlements without a venue mark price valued at last price"
            ));
        }
        events.extend(ev);
        let marks = dir.read_marks(id, a.interval)?;
        if marks.is_empty() {
            notes.push(format!(
                "warning: {id}: no mark-price bars cached: liquidation checked on last price (fetch-data again)"
            ));
        }
        events.extend(backtest::mark_events(marks, a.from, a.to));
    }
    let clearance = a.clearance.unwrap_or(clearance);
    anyhow::ensure!(
        clearance >= Decimal::ZERO && clearance < Decimal::ONE,
        "--clearance must be in [0, 1)"
    );
    Ok((backtest::PerpSetup { terms, clearance }, events))
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
        // eToro serves data only to a keyed account; a demo key will do.
        Venue::Etoro => Box::new(exchange::etoro::EtoroClient::new(
            base.clone(),
            exchange::etoro::EtoroKeys::from_env()?,
            exchange::etoro::Account::Demo,
            Duration::from_secs(30),
        )),
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
            fetch_perp_data(&base, &dir, &id, a.interval, a.from, to, a.brackets_from).await?;
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
    interval: Interval,
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
    let m = backtest::fetch_marks(&client, dir, id, interval, from, to)
        .await
        .with_context(|| format!("fetching {id} mark price"))?;
    println!(
        "{id} mark price {}: {} downloaded, {} cached",
        interval.code(),
        m.downloaded,
        m.cached
    );
    let info = client.perp_instrument(id.symbol()).await?;
    let mut brackets = dir.read_perp(id).ok().and_then(|p| p.brackets);
    let network = match brackets_from {
        BracketsHost::Live => Network::Live,
        BracketsHost::Demo => Network::Demo,
        BracketsHost::Testnet => Network::Testnet,
    };
    let host = network;
    match usdm_signed(network) {
        Ok(signed) => match usdm::brackets(&signed, id.symbol()).await {
            Ok(b) => {
                println!(
                    "{id} margin brackets: {} tiers from {host:?}",
                    b.iter().count()
                );
                brackets = Some(b);
            }
            Err(e) => {
                eprintln!("warning: {id}: margin brackets from {host:?}: {e}; kept the cached ones")
            }
        },
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
        Command::Console(a) => {
            init_logging(&config::log_filter(rust_log, None, "info,sqlx=warn"));
            console::run(a).await
        }
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
            let marker = kill_marker(&cfg);
            check_kill_marker(&marker, a.clear_kill_switch)?;
            let ran = run_bot(a, cfg).await;
            if let Err(e) = &ran
                && let Some(k) = e.downcast_ref::<KillSwitchTripped>()
            {
                let stamp = date::format(Timestamp::from_millis(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0),
                ));
                if let Err(w) = std::fs::create_dir_all("data/kill-switch")
                    .and_then(|()| std::fs::write(&marker, format!("{stamp}: {}\n", k.0)))
                {
                    tracing::error!(error = %w, marker = %marker.display(), "could not record the kill switch");
                }
            }
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
            backtest(*a).await
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
        let err = perp_setup(
            &provider(),
            &data,
            std::slice::from_ref(&id),
            &a,
            &mut Vec::new(),
        )
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
        let err = perp_setup(
            &provider(),
            &data,
            std::slice::from_ref(&id),
            &a,
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(format!("{err:#}").contains("no margin brackets"), "{err:#}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A marker refuses the start and names its reason; `--clear-kill-switch`
    /// removes it and starts; no marker, no question.
    #[test]
    fn a_tripped_kill_switch_refuses_the_next_start() {
        let dir = std::env::temp_dir().join(format!("kill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("binance-demo-sma_trend.txt");
        let _ = std::fs::remove_file(&marker);
        assert!(check_kill_marker(&marker, false).is_ok());
        std::fs::write(&marker, "2026-11-18: equity 700 fell\n").unwrap();
        let err = check_kill_marker(&marker, false).unwrap_err().to_string();
        assert!(
            err.contains("equity 700 fell") && err.contains("--clear-kill-switch"),
            "{err}"
        );
        assert!(check_kill_marker(&marker, true).is_ok());
        assert!(!marker.exists());
        let e = tripped(Some("why".into())).unwrap_err();
        assert!(e.downcast_ref::<KillSwitchTripped>().is_some());
        assert!(tripped(None).is_ok());
    }

    /// A hedge is Binance spot plus USDⓈ-M, with its own wallet: another
    /// venue, or no --perp-cash, is refused before any data is read.
    #[tokio::test]
    async fn hedge_flags_are_checked_first() {
        let args = |extra: &[&str]| {
            let mut v = vec![
                "cli",
                "backtest",
                "--strategy",
                "funding_carry",
                "--symbol",
                "BTCUSDT",
                "--from",
                "2024-01-01",
                "--to",
                "2024-02-01",
                "--slippage-bps",
                "2",
                "--data-dir",
                "/nonexistent",
            ];
            v.extend_from_slice(extra);
            let Command::Backtest(a) = Cli::parse_from(v).command else {
                panic!("not backtest");
            };
            a
        };
        let err = |a: Box<BacktestArgs>| async move {
            format!(
                "{:#}",
                backtest_report(&a, Params::default()).await.unwrap_err()
            )
        };
        let e = err(args(&[
            "--venue",
            "max",
            "--perp-symbol",
            "BTCUSDT",
            "--perp-cash",
            "500",
        ]))
        .await;
        assert!(e.contains("--venue binance"), "{e}");
        let e = err(args(&["--perp-symbol", "BTCUSDT"])).await;
        assert!(e.contains("--perp-cash"), "{e}");
        let e = err(args(&["--perp-cash", "500"])).await;
        assert!(e.contains("--perp-cash is for --perp-symbol"), "{e}");
    }
}
