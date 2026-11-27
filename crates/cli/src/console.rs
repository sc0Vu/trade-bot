//! `trade-bot console`: a local web page for writing script strategies,
//! backtesting any strategy against buy-and-hold, and writing a run config
//! by `config init`'s questions (`/api/config/step`, [`crate::wizard::step`]:
//! nothing written, no key asked).
//!
//! Self-hosted and single-user: it listens on loopback only (reach it from
//! elsewhere through an SSH tunnel), reads no secret and places no order.
//! What it can change is `strategies/*.rhai`, so it guards against other
//! web pages in the same browser: a request must name this server as its
//! `Host` (a DNS-rebinding page can't), an `Origin`, when sent, must be
//! this server, and every write is `application/json` (a cross-site form
//! can't send that without a CORS preflight, which is never granted).
//!
//! Backtests run through the same code as `trade-bot backtest`
//! ([`crate::backtest_report`]), on the cache `fetch-data` fills, one at a
//! time. Results are model output, labelled as such on the page.

use crate::config::Venue;
use crate::{BacktestArgs, MarginArg, backtest_report, date, parse_decimal, parse_interval};
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bot_core::{Decimal, Symbol};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use strategy::Params;
use tokio::sync::Mutex;

/// The page: one static file, no external scripts.
const PAGE: &str = include_str!("console.html");
/// Largest script the console saves.
const MAX_SCRIPT: usize = 256 * 1024;
/// Most points of an equity curve sent to the page (display only).
const MAX_POINTS: usize = 2000;

#[derive(clap::Args)]
pub(crate) struct ConsoleArgs {
    /// Address to serve on; must be loopback.
    #[arg(long, default_value = "127.0.0.1:9186")]
    listen: SocketAddr,
    /// Cached market data (`fetch-data`'s cache).
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,
    /// Where script strategies are read and saved.
    #[arg(long, default_value = "strategies")]
    scripts_dir: PathBuf,
    /// Example configs, for each strategy's starting params.
    #[arg(long, default_value = "config/examples")]
    examples_dir: PathBuf,
}

/// Shared by every request.
#[derive(Clone)]
pub(crate) struct Console {
    data_dir: PathBuf,
    scripts_dir: PathBuf,
    examples_dir: PathBuf,
    /// Hosts a request may name (`127.0.0.1:<port>`, `localhost:<port>`).
    hosts: Arc<Vec<String>>,
    /// One backtest at a time: they are CPU-bound.
    busy: Arc<Mutex<()>>,
}

/// Serves the console until the process ends.
pub(crate) async fn run(a: ConsoleArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        a.listen.ip().is_loopback(),
        "the console serves loopback only ({} is not); use an SSH tunnel to reach it",
        a.listen
    );
    let listener = tokio::net::TcpListener::bind(a.listen).await?;
    let addr = listener.local_addr()?;
    let console = Console::new(&a, addr.port());
    println!("console: http://{addr}/  (local only; places no orders; Ctrl-C stops)");
    axum::serve(listener, router(console)).await?;
    Ok(())
}

impl Console {
    fn new(a: &ConsoleArgs, port: u16) -> Self {
        Self {
            data_dir: a.data_dir.clone(),
            scripts_dir: a.scripts_dir.clone(),
            examples_dir: a.examples_dir.clone(),
            hosts: Arc::new(vec![
                format!("127.0.0.1:{port}"),
                format!("localhost:{port}"),
                format!("[::1]:{port}"),
            ]),
            busy: Arc::new(Mutex::new(())),
        }
    }
}

fn router(console: Console) -> Router {
    Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/api/catalog", get(catalog))
        .route("/api/scripts/{name}", get(read_script).put(save_script))
        .route("/api/examples/{strategy}", get(example_params))
        .route("/api/check", post(check))
        .route("/api/backtest", post(run_backtest))
        .route("/api/lookahead", post(lookahead))
        .route("/api/restart", post(restart))
        .route("/api/config/step", post(config_step))
        .layer(middleware::from_fn_with_state(console.clone(), guard))
        .with_state(console)
}

/// An error the page shows as is.
#[derive(Debug)]
struct Failure(StatusCode, String);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn bad(msg: impl ToString) -> Failure {
    Failure(StatusCode::BAD_REQUEST, msg.to_string())
}

/// Refuses requests that didn't come from this console's own page: wrong
/// `Host` (DNS rebinding), foreign `Origin`, or a write that isn't JSON.
async fn guard(State(c): State<Console>, req: Request, next: Next) -> Response {
    match allowed(&c, req.method(), req.headers()) {
        Ok(()) => next.run(req).await,
        Err(f) => f.into_response(),
    }
}

fn allowed(c: &Console, method: &Method, h: &HeaderMap) -> Result<(), Failure> {
    let text = |name| h.get(name).and_then(|v| v.to_str().ok());
    let host = text(header::HOST).unwrap_or_default();
    if !c.hosts.iter().any(|ok| ok == host) {
        return Err(Failure(StatusCode::FORBIDDEN, "unexpected Host".into()));
    }
    if let Some(origin) = text(header::ORIGIN)
        && origin != format!("http://{host}")
    {
        return Err(Failure(StatusCode::FORBIDDEN, "foreign Origin".into()));
    }
    if (method == Method::POST || method == Method::PUT)
        && !text(header::CONTENT_TYPE).is_some_and(|t| t.starts_with("application/json"))
    {
        return Err(Failure(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "writes must be application/json".into(),
        ));
    }
    Ok(())
}

/// A script name: lowercase letters, digits, `_`, at most 64; the file is
/// `<scripts_dir>/<name>.rhai`. Nothing else can be read or written.
fn script_path(c: &Console, name: &str) -> Result<PathBuf, Failure> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !ok {
        return Err(bad(
            "a script name is lowercase letters, digits and _ (at most 64)",
        ));
    }
    Ok(c.scripts_dir.join(format!("{name}.rhai")))
}

#[derive(Serialize)]
struct Catalog {
    /// Built-in strategies (registered names, `rhai` excluded).
    strategies: Vec<String>,
    /// Script names in the scripts directory.
    scripts: Vec<String>,
    /// Whether this build runs scripts (cargo feature `rhai`).
    rhai: bool,
    /// Cached series a backtest can use.
    data: Vec<Series>,
}

#[derive(Serialize, PartialEq, Debug)]
struct Series {
    venue: String,
    symbol: String,
    interval: String,
}

async fn catalog(State(c): State<Console>) -> Json<Catalog> {
    let names = strategy::names();
    let mut scripts: Vec<String> = files(&c.scripts_dir)
        .into_iter()
        .filter_map(|f| f.strip_suffix(".rhai").map(str::to_owned))
        .collect();
    scripts.sort();
    Json(Catalog {
        strategies: names
            .iter()
            .filter(|n| **n != "rhai")
            .map(|n| (*n).to_owned())
            .collect(),
        scripts,
        rhai: names.contains(&"rhai"),
        data: cached_series(&c.data_dir),
    })
}

/// File names in `dir` (none if it can't be read).
fn files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Every `klines/<venue>/<SYMBOL>-<interval>.jsonl` under `data_dir`.
fn cached_series(data_dir: &Path) -> Vec<Series> {
    let root = data_dir.join("klines");
    let mut out = Vec::new();
    for venue in files(&root) {
        for f in files(&root.join(&venue)) {
            let Some(stem) = f.strip_suffix(".jsonl") else {
                continue;
            };
            if let Some((symbol, interval)) = stem.rsplit_once('-') {
                out.push(Series {
                    venue: venue.clone(),
                    symbol: symbol.to_owned(),
                    interval: interval.to_owned(),
                });
            }
        }
    }
    out.sort_by(|a, b| (&a.venue, &a.symbol, &a.interval).cmp(&(&b.venue, &b.symbol, &b.interval)));
    out
}

#[derive(Serialize, Deserialize)]
struct ScriptText {
    text: String,
}

async fn read_script(
    State(c): State<Console>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<ScriptText>, Failure> {
    let path = script_path(&c, &name)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|_| Failure(StatusCode::NOT_FOUND, format!("no script {name}")))?;
    Ok(Json(ScriptText { text }))
}

async fn save_script(
    State(c): State<Console>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<ScriptText>,
) -> Result<StatusCode, Failure> {
    let path = script_path(&c, &name)?;
    if body.text.len() > MAX_SCRIPT {
        return Err(bad("script larger than 256 KiB"));
    }
    std::fs::create_dir_all(&c.scripts_dir)
        .and_then(|()| std::fs::write(&path, body.text))
        .map_err(|e| Failure(StatusCode::INTERNAL_SERVER_ERROR, format!("saving: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct ExampleParams {
    /// The example's `[strategy.params]` as TOML; empty without one.
    params: String,
}

/// A built-in's params from `config/examples/<strategy>.toml`, as a
/// starting point the page edits.
async fn example_params(
    State(c): State<Console>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<ExampleParams>, Failure> {
    if !name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return Err(bad("not a strategy name"));
    }
    let path = c.examples_dir.join(format!("{name}.toml"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Json(ExampleParams {
            params: String::new(),
        }));
    };
    let table: toml::Table = text.parse().map_err(|e| bad(format!("{name}.toml: {e}")))?;
    let params = table
        .get("strategy")
        .and_then(|s| s.get("params"))
        .and_then(toml::Value::as_table)
        .map(|t| toml::to_string(t).unwrap_or_default())
        .unwrap_or_default();
    Ok(Json(ExampleParams { params }))
}

/// Which strategy a request means: a built-in by name, or a script by
/// name (run as `rhai` with `script` set to its file).
#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    script: Option<String>,
    /// The strategy's params as TOML (money as strings).
    #[serde(default)]
    params: String,
}

fn resolve(c: &Console, choice: &Choice) -> Result<(String, Params), Failure> {
    resolve_in(c, choice, &[])
}

/// [`resolve`], with bare symbols in the params naming instruments of
/// `universe` (`crate::symbols`).
fn resolve_in(
    c: &Console,
    choice: &Choice,
    universe: &[bot_core::InstrumentId],
) -> Result<(String, Params), Failure> {
    let (name, text) = match (&choice.strategy, &choice.script) {
        (Some(s), None) => (s.clone(), choice.params.clone()),
        (None, Some(script)) => {
            let path = script_path(c, script)?;
            let path = toml::Value::String(path.display().to_string());
            (
                "rhai".to_owned(),
                format!("script = {path}\n{}", choice.params),
            )
        }
        _ => return Err(bad("name a strategy or a script")),
    };
    let mut table: toml::Table = text.parse().map_err(bad)?;
    crate::symbols::qualify_params(&mut table, universe).map_err(|e| bad(format!("{e:#}")))?;
    Ok((name, Params::new(table)))
}

#[derive(Serialize)]
struct Checked {
    ok: bool,
    error: Option<String>,
}

#[derive(Deserialize)]
struct CheckRequest {
    #[serde(flatten)]
    choice: Choice,
    /// The venue and symbols picked on the page, so bare symbols in the
    /// params resolve as a backtest would.
    #[serde(default)]
    venue: Option<Venue>,
    #[serde(default)]
    symbols: Vec<String>,
}

/// Builds the strategy from its params (a script: compiles it and runs
/// its `check_params`), as a run would at start.
async fn check(
    State(c): State<Console>,
    Json(r): Json<CheckRequest>,
) -> Result<Json<Checked>, Failure> {
    let universe = match r.venue {
        Some(v) => {
            crate::symbols::qualify(&r.symbols, &v.id()).map_err(|e| bad(format!("{e:#}")))?
        }
        None => Vec::new(),
    };
    let (name, params) = resolve_in(&c, &r.choice, &universe)?;
    Ok(Json(match strategy::build(&name, &params) {
        Ok(_) => Checked {
            ok: true,
            error: None,
        },
        Err(e) => Checked {
            ok: false,
            error: Some(e.to_string()),
        },
    }))
}

#[derive(Deserialize)]
struct BacktestRequest {
    #[serde(flatten)]
    choice: Choice,
    venue: Venue,
    symbols: Vec<String>,
    interval: String,
    /// `YYYY-MM-DD` (UTC), as the CLI.
    from: String,
    to: String,
    /// Money and rates as strings (invariant 1).
    initial_cash: String,
    fee: String,
    slippage_bps: String,
    #[serde(default)]
    history_len: Option<usize>,
    /// Buy-and-hold this symbol with the same cash and costs, as the
    /// yardstick; `None`: no benchmark.
    #[serde(default)]
    benchmark: Option<String>,
    /// `binance-usdm` only.
    #[serde(default)]
    leverage: Option<u32>,
}

/// One run's figures for the page; money and ratios as exact strings.
#[derive(Serialize)]
struct Outcome {
    final_equity: String,
    return_pct: String,
    max_drawdown_pct: String,
    sharpe: Option<String>,
    fills: usize,
    closed_trades: usize,
    win_rate_pct: Option<String>,
    rejected_signals: usize,
    /// (close time ms, equity), thinned to at most `MAX_POINTS`.
    curve: Vec<(i64, String)>,
    notes: Vec<String>,
}

#[derive(Serialize)]
struct BacktestReply {
    strategy: Outcome,
    benchmark: Option<Outcome>,
    /// Why the benchmark couldn't run (the strategy's result stands).
    benchmark_error: Option<String>,
}

async fn run_backtest(
    State(c): State<Console>,
    Json(r): Json<BacktestRequest>,
) -> Result<Json<BacktestReply>, Failure> {
    let Ok(_one) = c.busy.try_lock() else {
        return Err(Failure(
            StatusCode::CONFLICT,
            "a backtest is already running".into(),
        ));
    };
    let (name, params) = resolve(&c, &r.choice)?;
    let args = |strategy: String, symbols: &[String]| -> Result<BacktestArgs, Failure> {
        Ok(BacktestArgs {
            strategy,
            venue: r.venue,
            params: None,
            symbol: symbols
                .iter()
                .map(|s| Symbol::new(s).map_err(bad))
                .collect::<Result<_, _>>()?,
            interval: parse_interval(&r.interval).map_err(bad)?,
            from: date::parse(&r.from).map_err(bad)?,
            to: date::parse(&r.to).map_err(bad)?,
            initial_cash: parse_decimal(&r.initial_cash).map_err(|e| bad(format!("{e:#}")))?,
            fee: parse_decimal(&r.fee).map_err(|e| bad(format!("{e:#}")))?,
            slippage_bps: parse_decimal(&r.slippage_bps).map_err(|e| bad(format!("{e:#}")))?,
            delistings: None,
            max_gap_hours: None,
            earn_ratio: None,
            earn_apr: None,
            delist_exit_hours: 24,
            kill_drawdown: None,
            perp_symbol: Vec::new(),
            perp_cash: None,
            perp_fee: Decimal::new(5, 4),
            history_len: r.history_len.unwrap_or(500),
            data_dir: c.data_dir.clone(),
            leverage: r.leverage,
            margin: MarginArg::Isolated,
            cfd: false,
            clearance: None,
        })
    };
    if r.symbols.is_empty() {
        return Err(bad("pick at least one symbol"));
    }
    let main = args(name, &r.symbols)?;
    let (report, notes) = backtest_report(&main, params)
        .await
        .map_err(|e| bad(format!("{e:#}")))?;
    let strategy = outcome(&report, notes);
    let (benchmark, benchmark_error) = match &r.benchmark {
        None => (None, None),
        Some(sym) => {
            let b = args("buy_and_hold".into(), std::slice::from_ref(sym))?;
            // 1% of the cash left over, as the examples do: a quote-budget
            // buy plus its fee must fit.
            let budget = (b.initial_cash * Decimal::new(99, 2)).round_dp(2);
            let text = format!(
                "coin = {coin}\nquote_amount = \"{budget}\"\nfee = \"{fee}\"\nslippage_bps = \"{slip}\"\n",
                coin = toml::Value::String(format!("{}:{sym}", r.venue.id())),
                fee = b.fee,
                slip = b.slippage_bps,
            );
            match Params::from_toml(&text) {
                Err(e) => (None, Some(e.to_string())),
                Ok(p) => match backtest_report(&b, p).await {
                    Ok((rep, notes)) => (Some(outcome(&rep, notes)), None),
                    Err(e) => (None, Some(format!("{e:#}"))),
                },
            }
        }
    };
    Ok(Json(BacktestReply {
        strategy,
        benchmark,
        benchmark_error,
    }))
}

/// Cut points the console's property checks try (a backtest or two each).
const CHECK_CUTS: usize = 10;

/// The outcome of a property check (lookahead or restart).
#[derive(Serialize)]
struct CheckReply {
    passed: bool,
    /// Cut points compared.
    cuts: usize,
    /// Whether the pass tested anything: lookahead, the rewrite changed a
    /// later fill; restart, a cut was taken while holding a position.
    meaningful: bool,
    /// What failed.
    error: Option<String>,
}

/// What both property checks run on: the request's strategy, its cached
/// decision bars (no warm-up), costs and engine settings.
struct CheckInput {
    name: String,
    params: strategy::Params,
    instruments: Vec<bot_core::Instrument>,
    candles: Vec<bot_core::Candle>,
    paper: backtest::PaperConfig,
    config: engine::EngineConfig,
}

fn check_input(c: &Console, r: &BacktestRequest) -> Result<CheckInput, Failure> {
    let venue = r.venue.id();
    let ids: Vec<bot_core::InstrumentId> = r
        .symbols
        .iter()
        .map(|s| {
            Ok(bot_core::InstrumentId::new(
                venue.clone(),
                Symbol::new(s).map_err(bad)?,
            ))
        })
        .collect::<Result<_, Failure>>()?;
    if ids.is_empty() {
        return Err(bad("pick at least one symbol"));
    }
    let (name, params) = resolve_in(c, &r.choice, &ids)?;
    let fee = parse_decimal(&r.fee).map_err(|e| bad(format!("{e:#}")))?;
    let quote_fee = matches!(r.venue, Venue::Etoro | Venue::BinanceUsdm);
    let fees = exchange::paper::RateFee::new(
        fee,
        fee,
        if quote_fee {
            exchange::paper::FeeAsset::Quote
        } else {
            exchange::paper::FeeAsset::Received
        },
    )
    .ok_or_else(|| bad("fee must be in [0, 1)"))?;
    let paper = backtest::PaperConfig {
        fees,
        slippage: exchange::paper::BpsSlippage::new(
            parse_decimal(&r.slippage_bps).map_err(|e| bad(format!("{e:#}")))?,
        )
        .ok_or_else(|| bad("slippage must be >= 0"))?,
        initial_cash: bot_core::Notional::new(
            parse_decimal(&r.initial_cash).map_err(|e| bad(format!("{e:#}")))?,
        ),
    };
    let cached = backtest::BacktestProvider::from_cache(
        &backtest::DataDir::new(c.data_dir.clone()),
        &ids,
        parse_interval(&r.interval).map_err(bad)?,
        date::parse(&r.from).map_err(bad)?,
        date::parse(&r.to).map_err(bad)?,
        paper,
    )
    .map_err(bad)?;
    Ok(CheckInput {
        name,
        params,
        instruments: cached.instruments().to_vec(),
        candles: cached.candles().to_vec(),
        paper,
        config: engine::EngineConfig {
            whitelist: ids,
            history_len: r.history_len.unwrap_or(500),
            ..Default::default()
        },
    })
}

/// `backtest::check_lookahead` on the request (see [`check_input`]) at
/// `CHECK_CUTS` cut points.
async fn lookahead(
    State(c): State<Console>,
    Json(r): Json<BacktestRequest>,
) -> Result<Json<CheckReply>, Failure> {
    let Ok(_one) = c.busy.try_lock() else {
        return Err(Failure(
            StatusCode::CONFLICT,
            "a backtest is already running".into(),
        ));
    };
    let i = check_input(&c, &r)?;
    let checked = backtest::check_lookahead(
        &i.instruments,
        &i.candles,
        i.paper,
        &|_| strategy::build(&i.name, &i.params),
        &i.params,
        &i.config,
        CHECK_CUTS,
    )
    .await;
    Ok(Json(match checked {
        Ok(rep) => CheckReply {
            passed: true,
            cuts: rep.cuts,
            meaningful: rep.future_mattered,
            error: None,
        },
        Err(backtest::LookaheadError::Changed { cut, what }) => CheckReply {
            passed: false,
            cuts: 0,
            meaningful: true,
            error: Some(format!(
                "{what} up to {} changed when only later candles did: the strategy reads ahead",
                date::format(cut)
            )),
        },
        Err(e) => return Err(bad(e)),
    }))
}

/// `backtest::check_restart` on the request (see [`check_input`]) at up to
/// `CHECK_CUTS` cut points.
async fn restart(
    State(c): State<Console>,
    Json(r): Json<BacktestRequest>,
) -> Result<Json<CheckReply>, Failure> {
    let Ok(_one) = c.busy.try_lock() else {
        return Err(Failure(
            StatusCode::CONFLICT,
            "a backtest is already running".into(),
        ));
    };
    let i = check_input(&c, &r)?;
    let checked = backtest::check_restart(
        &i.instruments,
        &i.candles,
        i.paper,
        &|_| strategy::build(&i.name, &i.params),
        &i.params,
        &i.config,
        CHECK_CUTS,
    )
    .await;
    Ok(Json(match checked {
        Ok(rep) => CheckReply {
            passed: true,
            cuts: rep.cuts,
            meaningful: rep.while_holding > 0,
            error: None,
        },
        Err(backtest::RestartError::Changed { cut, .. }) => CheckReply {
            passed: false,
            cuts: 0,
            meaningful: true,
            error: Some(format!(
                "restarting after {} from saved state changed the fills: save_state / restore \
                 loses something the decisions depend on",
                date::format(cut)
            )),
        },
        Err(backtest::RestartError::NoIdleCut) => CheckReply {
            passed: false,
            cuts: 0,
            meaningful: false,
            error: Some("no cut point without a working order: nothing comparable".into()),
        },
        Err(e) => return Err(bad(e)),
    }))
}

fn outcome(r: &backtest::Report, notes: Vec<String>) -> Outcome {
    let hundred = Decimal::ONE_HUNDRED;
    let initial = r.initial_cash.get();
    let ret = if initial.is_zero() {
        Decimal::ZERO
    } else {
        (r.final_equity.get() / initial - Decimal::ONE) * hundred
    };
    let step = r.equity_curve.len().div_ceil(MAX_POINTS).max(1);
    let mut curve: Vec<(i64, String)> = r
        .equity_curve
        .iter()
        .step_by(step)
        .map(|(t, e)| (t.as_millis(), e.get().round_dp(2).to_string()))
        .collect();
    if let Some((t, e)) = r.equity_curve.last()
        && curve.last().is_none_or(|(lt, _)| *lt != t.as_millis())
    {
        curve.push((t.as_millis(), e.get().round_dp(2).to_string()));
    }
    Outcome {
        final_equity: format!("{:.2}", r.final_equity.get()),
        return_pct: format!("{ret:.2}"),
        max_drawdown_pct: format!("{:.2}", r.max_drawdown * hundred),
        sharpe: r.sharpe.map(|s| format!("{s:.2}")),
        fills: r.fills,
        closed_trades: r.closed_trades,
        win_rate_pct: r.win_rate.map(|w| format!("{:.1}", w * hundred)),
        rejected_signals: r.rejected_signals,
        curve,
        notes,
    }
}

#[derive(Deserialize)]
struct StepRequest {
    /// Every answer so far, in order; the questions are replayed.
    answers: Vec<String>,
}

/// One step of the run-config questions (`config init`'s, [`crate::wizard`]):
/// the next question, or the finished config with its check. Nothing is
/// written: the page offers the text to copy or download. Keys are never
/// asked for here; the check reports which ones the server will need,
/// without looking at this process's environment.
async fn config_step(Json(r): Json<StepRequest>) -> Result<Json<crate::wizard::Step>, Failure> {
    let no_keys = |_: &str| None;
    let exists = |p: &Path| p.exists();
    let mut step = crate::wizard::step(
        &r.answers,
        Path::new("config/config.toml"),
        &no_keys,
        &exists,
    )
    .map_err(|e| bad(format!("{e:#}")))?;
    if let crate::wizard::Step::Done { findings, .. } = &mut step {
        for (level, text) in findings.iter_mut() {
            if let Some(rest) = text.strip_prefix("keys: ")
                && level == "fail"
            {
                *level = "note".into();
                let names = rest.split(" not set").next().unwrap_or(rest);
                *text =
                    format!("keys: the server needs {names} in its env file (never entered here)");
            }
        }
    }
    Ok(Json(step))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use bot_core::{Notional, Timestamp};
    use rust_decimal_macros::dec;

    fn console(dir: &Path) -> Console {
        Console::new(
            &ConsoleArgs {
                listen: "127.0.0.1:9186".parse().unwrap(),
                data_dir: dir.join("data"),
                scripts_dir: dir.join("strategies"),
                examples_dir: dir.join("examples"),
            },
            9186,
        )
    }

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.clone(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("console-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Own page: allowed. Another site's Host (DNS rebinding) or Origin,
    /// or a write that isn't JSON (a cross-site form): refused.
    #[test]
    fn only_the_consoles_own_page_gets_through() {
        let c = console(Path::new("."));
        let json = (header::CONTENT_TYPE, "application/json");
        let host = (header::HOST, "127.0.0.1:9186");
        assert!(allowed(&c, &Method::GET, &headers(std::slice::from_ref(&host))).is_ok());
        assert!(
            allowed(
                &c,
                &Method::GET,
                &headers(&[(header::HOST, "localhost:9186")])
            )
            .is_ok()
        );
        assert!(
            allowed(
                &c,
                &Method::PUT,
                &headers(&[
                    host.clone(),
                    json.clone(),
                    (header::ORIGIN, "http://127.0.0.1:9186")
                ])
            )
            .is_ok()
        );
        let refused = |h: HeaderMap, m: Method| allowed(&c, &m, &h).unwrap_err().0;
        assert_eq!(
            refused(headers(&[(header::HOST, "evil.example:9186")]), Method::GET),
            StatusCode::FORBIDDEN
        );
        assert_eq!(refused(headers(&[]), Method::GET), StatusCode::FORBIDDEN);
        assert_eq!(
            refused(
                headers(&[host.clone(), json, (header::ORIGIN, "http://evil.example")]),
                Method::POST
            ),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            refused(
                headers(&[host, (header::CONTENT_TYPE, "text/plain")]),
                Method::PUT
            ),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }

    #[test]
    fn scripts_stay_in_their_directory() {
        let c = console(Path::new("/tmp/x"));
        assert_eq!(
            script_path(&c, "my_trend2").unwrap(),
            PathBuf::from("/tmp/x/strategies/my_trend2.rhai")
        );
        for bad in ["", "../evil", "a/b", "A", "x.rhai", "a b", &"a".repeat(65)] {
            assert!(script_path(&c, bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn cached_series_are_listed_from_the_kline_cache() {
        let d = temp("series");
        let k = d.join("data/klines");
        std::fs::create_dir_all(k.join("etoro")).unwrap();
        std::fs::create_dir_all(k.join("binance")).unwrap();
        for f in [
            "etoro/AAPL.RTH-1d.jsonl",
            "binance/BTCUSDT-1h.jsonl",
            "binance/README",
        ] {
            std::fs::write(k.join(f), "").unwrap();
        }
        let s = |v: &str, sym: &str, iv: &str| Series {
            venue: v.into(),
            symbol: sym.into(),
            interval: iv.into(),
        };
        assert_eq!(
            cached_series(&d.join("data")),
            vec![s("binance", "BTCUSDT", "1h"), s("etoro", "AAPL.RTH", "1d")]
        );
    }

    /// A script runs as `rhai` with `script` pointing at its file, before
    /// the user's own params; naming both or neither is refused.
    #[test]
    fn a_script_choice_runs_as_rhai_with_its_file() {
        let c = console(Path::new("/tmp/x"));
        let (name, params) = resolve(
            &c,
            &Choice {
                strategy: None,
                script: Some("mine".into()),
                params: "coin = \"binance:BTCUSDT\"".into(),
            },
        )
        .unwrap();
        assert_eq!(name, "rhai");
        let table: toml::Table = params.parse("test").unwrap();
        assert_eq!(
            table.get("script").and_then(toml::Value::as_str),
            Some("/tmp/x/strategies/mine.rhai")
        );
        assert_eq!(
            table.get("coin").and_then(toml::Value::as_str),
            Some("binance:BTCUSDT")
        );
        let both = Choice {
            strategy: Some("sma_trend".into()),
            script: Some("mine".into()),
            params: String::new(),
        };
        assert!(resolve(&c, &both).is_err());
    }

    /// 10,000 → 12,500 is +25.00%; drawdown 0.1 is 10.00% (fixed decimals, so
    /// the page's columns line up); a 5,000-point curve is
    /// thinned to every 3rd point (⌈5000 / 2000⌉) plus the last.
    #[test]
    fn outcomes_are_percentages_and_a_thinned_curve() {
        let curve: Vec<(Timestamp, Notional)> = (0..5000)
            .map(|i| (Timestamp::from_millis(i), Notional::new(dec!(10000))))
            .collect();
        let r = backtest::Report {
            initial_cash: Notional::new(dec!(10000)),
            final_equity: Notional::new(dec!(12500)),
            realized_pnl: Notional::ZERO,
            min_cash: Notional::ZERO,
            max_drawdown: dec!(0.1),
            fills: 4,
            closed_trades: 2,
            win_rate: Some(dec!(0.5)),
            sharpe: None,
            rejected_signals: 0,
            earn: None,
            earn_interest: Notional::ZERO,
            funding: Notional::ZERO,
            equity_curve: curve,
            killed: None,
        };
        let o = outcome(&r, vec![]);
        assert_eq!(
            (o.return_pct.as_str(), o.max_drawdown_pct.as_str()),
            ("25.00", "10.00")
        );
        assert_eq!(o.win_rate_pct.as_deref(), Some("50.0"));
        assert_eq!(o.curve.len(), 1668);
        assert_eq!(o.curve.first().unwrap().0, 0);
        assert_eq!(o.curve.last().unwrap().0, 4999);
    }

    async fn step(answers: &[&str]) -> serde_json::Value {
        let r = StepRequest {
            answers: answers.iter().map(|a| (*a).to_owned()).collect(),
        };
        let Json(step) = config_step(Json(r)).await.unwrap();
        serde_json::to_value(step).unwrap()
    }

    /// The page walks the same questions as `config init`: first the
    /// strategy (a choice with summaries), then one at a time; a refused
    /// answer comes back as an error with the same question and is dropped
    /// from the answers; enough answers give the config, its backtest
    /// form, and a check that names the keys without asking for them.
    #[tokio::test]
    async fn the_config_page_walks_the_wizard() {
        let first = step(&[]).await;
        assert_eq!(first["kind"], "ask");
        assert_eq!(first["question"]["default"], "sma_trend");
        let options = first["question"]["options"].as_array().unwrap();
        assert!(options.iter().any(|o| o == "grid"));
        assert!(!options.iter().any(|o| o == "funding_carry"));
        assert_eq!(
            options.len(),
            first["question"]["labels"].as_array().unwrap().len()
        );
        let labels = first["question"]["labels"].as_array().unwrap();
        assert!(
            labels.iter().any(|l| l
                == "buy_and_hold — the benchmark. Buys one coin once on its first candle and holds"),
            "{labels:?}"
        );

        let refused = step(&["sma_trend", "kraken"]).await;
        assert_eq!(refused["kind"], "ask");
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("one of binance")
        );
        assert_eq!(refused["answers"], serde_json::json!(["sma_trend"]));
        assert!(
            refused["question"]["text"]
                .as_str()
                .unwrap()
                .starts_with("Venue")
        );

        // strategy, venue … history, 6 params, bot name
        let mut all = vec!["sma_trend", "", "ETHUSDT", "1h", "", ""];
        all.extend([""; 7]);
        let done = step(&all).await;
        assert_eq!(done["kind"], "done", "{done}");
        assert!(
            done["toml"]
                .as_str()
                .unwrap()
                .contains("whitelist = [\"binance:ETHUSDT\"]")
        );
        let hint = &done["backtest"];
        assert_eq!(hint["strategy"], "sma_trend");
        assert_eq!(hint["symbols"], serde_json::json!(["ETHUSDT"]));
        assert_eq!(hint["interval"], "1h");
        assert_eq!(hint["fee"], "0.001");
        assert!(
            hint["params"]
                .as_str()
                .unwrap()
                .contains("sma_period = 200")
        );
        let findings = done["findings"].as_array().unwrap();
        assert!(!findings.iter().any(|f| f[0] == "fail"), "{findings:?}");
        assert!(findings.iter().any(|f| {
            f[1].as_str()
                .unwrap()
                .contains("BINANCE_API_KEY, BINANCE_API_SECRET in its env file")
        }));

        // A section header said before a question travels with it.
        let mut max = vec!["buy_and_hold", "max", "", ""];
        max.truncate(4);
        let at_paper = step(&max).await;
        assert_eq!(
            at_paper["question"]["notes"][0],
            "Paper broker (local fills, money as exact decimals):"
        );

        assert!(
            config_step(Json(StepRequest {
                answers: vec!["a\nb".into()]
            }))
            .await
            .is_err()
        );
    }
}
