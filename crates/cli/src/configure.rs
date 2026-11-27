//! `trade-bot config list | init | check`: help writing a run config.
//!
//! `list` names every strategy with a one-line summary; `init` writes a
//! new config from that strategy's example (embedded in the binary, so it
//! works away from the repo); `check` runs every refusal `run` makes
//! before it connects ([`crate::preflight`], the config loader, the key
//! and token lookups, the strategy build, the kill-switch marker) without
//! connecting, so a config is known good before anything is started.
//! Nothing here reads or prints a secret's value: keys are only looked up
//! by name (invariant 6).

use crate::config::{self, Config, Venue};
use crate::preflight;
use anyhow::{Context, bail};
use clap::Subcommand;
use std::path::{Path, PathBuf};
use strategy::Params;

/// The per-strategy examples of `config/examples/`, embedded. A test
/// keeps this list equal to the directory, which another keeps equal to
/// the strategy registry.
macro_rules! examples {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"), "/../../config/examples/", $name, ".toml"
        )))),*]
    };
}

/// `(strategy, example config text)`, sorted by name.
pub const EXAMPLES: &[(&str, &str)] = examples![
    "atr_breakout",
    "avwap",
    "bollinger_reversion",
    "buy_and_hold",
    "dca",
    "donchian_breakout",
    "dual_ma_cross",
    "funding_carry",
    "grid",
    "ichimoku",
    "long_short_momentum",
    "macd_cross",
    "momentum_rotation",
    "pair_reversion",
    "ratio_hop",
    "rebalance",
    "rhai",
    "risk_parity",
    "rsi_reversion",
    "sma_long_short",
    "sma_trend",
    "stochastic",
    "supertrend",
    "top_k_momentum",
    "trailing_stop",
    "tsmom",
    "tsmom_long_short",
    "turn_of_month",
    "vol_target",
    "volatility_breakout",
];

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// List the strategies a config can name, each with a one-line summary.
    List,
    /// Write a new paper config, then check it. Without `--strategy`, asks
    /// for everything (venue, coins, interval, costs, each strategy param,
    /// defaults from the example); with it, copies that example as is.
    Init {
        /// Strategy to start from (see `config list`). Omitted: asked.
        #[arg(long)]
        strategy: Option<String>,
        /// Ask the questions even with `--strategy` (which then skips only
        /// the strategy question).
        #[arg(long, short)]
        interactive: bool,
        /// Where to write it. Gitignored by default: keep real configs out
        /// of the repository.
        #[arg(long, default_value = config::DEFAULT_PATH)]
        out: PathBuf,
        /// Replace the file if it exists.
        #[arg(long)]
        force: bool,
    },
    /// Check a config as `run` would before connecting: loads, passes the
    /// live gate, has its keys in the environment, builds its strategy, no
    /// kill switch set. Connects to nothing; prints no secret.
    Check {
        /// Config file (default `BOT_CONFIG`, else config/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Check it as `run --live` (the config's two locks); the third,
        /// `--i-understand`, is typed at each run, never here.
        #[arg(long)]
        live: bool,
    },
}

/// Runs a `config` subcommand.
pub fn run(cmd: ConfigCommand) -> anyhow::Result<()> {
    let env = |name: &str| std::env::var(name).ok();
    let exists = |p: &Path| p.exists();
    match cmd {
        ConfigCommand::List => {
            print!("{}", list());
            Ok(())
        }
        ConfigCommand::Init {
            strategy,
            interactive,
            out,
            force,
        } => {
            let strategy = match strategy {
                Some(s) if !interactive => s,
                other => return ask(other.as_deref(), &out, force, &env, &exists),
            };
            init(&strategy, &out, force)?;
            println!(
                "wrote {} from the {strategy} example; edit it, then `trade-bot config check --config {}`",
                out.display(),
                out.display()
            );
            let found = check(Some(&out), false, &env, &exists);
            // Written either way: what's left to fix is the report's to say.
            if let Err(e) = report(&found, &out, false) {
                println!("{e}");
            }
            Ok(())
        }
        ConfigCommand::Check { config, live } => {
            let shown = config
                .clone()
                .or_else(|| std::env::var_os("BOT_CONFIG").map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from(config::DEFAULT_PATH));
            let found = check(config.as_deref(), live, &env, &exists);
            report(&found, &shown, live)
        }
    }
}

/// `config init` by questions ([`crate::wizard`]) on the terminal.
fn ask(
    strategy: Option<&str>,
    out: &Path,
    force: bool,
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut prompt = crate::wizard::Prompt::new(&mut input, &mut stdout);
    if let Some(found) = crate::wizard::run(&mut prompt, strategy, out, force, env, exists)? {
        // Written; what's left to fix (keys, usually) is the report's to say.
        if let Err(e) = report(&found, out, false) {
            println!("{e}");
        }
    }
    Ok(())
}

/// Every strategy with its example's summary, one per line.
pub(crate) fn list() -> String {
    let width = EXAMPLES.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let built = strategy::names();
    let mut out = String::new();
    for (name, text) in EXAMPLES {
        let note = if built.contains(name) {
            ""
        } else {
            " (not in this build: needs a cargo feature)"
        };
        out.push_str(&format!("{name:width$}  {}{note}\n", summary(text)));
    }
    out
}

/// An example's opening comment, without the `name:` it starts with: its
/// first sentences up to [`SUMMARY_MIN`] characters (`one coin.` alone
/// says little), cut at a word to at most [`SUMMARY_MAX`].
fn summary(text: &str) -> String {
    let para: Vec<&str> = text
        .lines()
        .map_while(|l| l.strip_prefix('#'))
        .map(str::trim)
        .take_while(|l| !l.is_empty())
        .collect();
    let joined = para.join(" ");
    let body = joined
        .split_once(": ")
        .map_or(joined.as_str(), |(_, rest)| rest);
    let mut out = String::new();
    for sentence in body.split_inclusive(". ") {
        out.push_str(sentence);
        if out.chars().count() >= SUMMARY_MIN {
            break;
        }
    }
    let out = out.trim_end().trim_end_matches('.');
    if out.chars().count() <= SUMMARY_MAX {
        return out.to_owned();
    }
    let head: String = out.chars().take(SUMMARY_MAX - 1).collect();
    let cut = head.rfind(' ').map_or(head.as_str(), |i| &head[..i]);
    format!("{}…", cut.trim_end_matches([',', ';', ':']))
}

/// Shortest summary [`summary`] settles for.
const SUMMARY_MIN: usize = 30;
/// Longest summary, in characters, ellipsis included.
const SUMMARY_MAX: usize = 100;

/// Writes `strategy`'s example to `out` (its directory created), never
/// over an existing file unless `force`.
fn init(strategy: &str, out: &Path, force: bool) -> anyhow::Result<()> {
    let Some((_, text)) = EXAMPLES.iter().find(|(n, _)| *n == strategy) else {
        bail!("no strategy {strategy:?}; `trade-bot config list` shows them");
    };
    if out.exists() && !force {
        bail!(
            "{} exists; pick another --out, or --force to replace it",
            out.display()
        );
    }
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(out, text).with_context(|| format!("writing {}", out.display()))
}

/// One line of a check.
#[derive(Debug, PartialEq, Eq)]
pub enum Finding {
    /// Passed.
    Ok(String),
    /// `run` would refuse to start.
    Fail(String),
    /// Worth knowing; not a refusal.
    Note(String),
}

/// Loads the config at `path` (as `run`: else `BOT_CONFIG`, else the
/// default, then `BOT_*` env) and checks it ([`check_loaded`]).
pub fn check(
    path: Option<&Path>,
    live: bool,
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> Vec<Finding> {
    match Config::load(path) {
        Ok(cfg) => check_loaded(&cfg, live, env, exists),
        Err(e) => vec![Finding::Fail(format!("{e:#}"))],
    }
}

/// The checks `run` makes before connecting, each reported (not stopping
/// at the first), on a loaded config. `env` looks up an environment
/// variable (only whether it is set is reported); `exists` a file.
pub fn check_loaded(
    cfg: &Config,
    live: bool,
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> Vec<Finding> {
    let mut found = vec![Finding::Ok(format!(
        "loads: {} on {} ({} bars), {} instrument(s)",
        cfg.strategy.name,
        cfg.exchange.venue.id().as_str(),
        cfg.exchange.interval.code(),
        cfg.whitelist.len()
    ))];
    // `run` passes --i-understand itself; here the config's locks only.
    let route = preflight::route(cfg, live, live);
    match &route {
        Ok(r) => {
            found.push(Finding::Ok(format!("run: {}", r.describe())));
            if r.is_live() {
                found.push(Finding::Note(
                    "REAL MONEY: `run --live` also needs --i-understand, typed at each start"
                        .into(),
                ));
            }
            match r.books_url(&cfg.db) {
                Some(url) => found.push(Finding::Note(format!("books: {url}"))),
                None => found.push(Finding::Note(
                    "books: in memory (a local paper run starts flat every time)".into(),
                )),
            }
        }
        Err(e) => found.push(Finding::Fail(format!("{e:#}"))),
    }
    let set = |name: &str| env(name).is_some_and(|v| !v.trim().is_empty());
    let keys = keys_for(cfg.exchange.venue);
    let missing: Vec<&str> = keys.iter().copied().filter(|k| !set(k)).collect();
    if keys.is_empty() {
        found.push(Finding::Ok("keys: none needed".into()));
    } else if missing.is_empty() {
        found.push(Finding::Ok(format!("keys: {} set", keys.join(", "))));
    } else {
        found.push(Finding::Fail(format!(
            "keys: {} not set (keys come from the environment only, e.g. an --env-file)",
            missing.join(", ")
        )));
    }
    // `run` asks for the token when it serves the API or keeps books.
    let with_books = route.as_ref().is_ok_and(|r| r.books_url(&cfg.db).is_some());
    if cfg.api.enabled || with_books {
        match config::api_token(&cfg.api, env(config::API_TOKEN_VAR)) {
            Ok(_) if cfg.api.enabled => {
                found.push(Finding::Ok(format!("api: on {}", cfg.api.listen)))
            }
            Ok(_) => {}
            Err(e) => found.push(Finding::Fail(format!("{e:#}"))),
        }
    }
    match strategy::build(
        &cfg.strategy.name,
        &Params::new(cfg.strategy.params.clone()),
    ) {
        Ok(_) => found.push(Finding::Ok(format!(
            "strategy: {} builds from its params",
            cfg.strategy.name
        ))),
        Err(e) => found.push(Finding::Fail(format!(
            "strategy {}: {e}",
            cfg.strategy.name
        ))),
    }
    let marker = preflight::kill_marker(cfg);
    if exists(&marker) {
        found.push(Finding::Fail(format!(
            "kill switch: tripped earlier ({}); review, then `run --clear-kill-switch`",
            marker.display()
        )));
    }
    found.push(Finding::Note(
        "not checked (needs the network): key validity, symbols listed, filters, balance".into(),
    ));
    found
}

/// The environment variables a run on `venue` reads its keys from.
fn keys_for(venue: Venue) -> &'static [&'static str] {
    use exchange::binance::signed::{API_KEY_VAR, API_SECRET_VAR};
    use exchange::etoro::client::{API_KEY_VAR as ETORO_KEY, USER_KEY_VAR as ETORO_USER};
    match venue {
        // USDⓈ-M paper runs read margin brackets signed, so they need keys too.
        Venue::Binance | Venue::BinanceUsdm => &[API_KEY_VAR, API_SECRET_VAR],
        Venue::Etoro => &[ETORO_KEY, ETORO_USER],
        Venue::Max => &[],
    }
}

/// Prints the findings; `Err` if any would refuse the run, else how to
/// start it.
fn report(found: &[Finding], path: &Path, live: bool) -> anyhow::Result<()> {
    let mut failed = 0;
    for f in found {
        match f {
            Finding::Ok(t) => println!("ok    {t}"),
            Finding::Note(t) => println!("note  {t}"),
            Finding::Fail(t) => {
                failed += 1;
                println!("FAIL  {t}");
            }
        }
    }
    if failed > 0 {
        bail!("{failed} problem(s): `run` would refuse this config");
    }
    let flags = if live {
        "--live --i-understand"
    } else {
        "--paper"
    };
    println!("start: trade-bot run {flags} --config {}", path.display());
    Ok(())
}

#[cfg(test)]
// figment's Jail API returns its own large error type; not ours to shrink.
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;
    use figment::Jail;

    fn parsed(text: &str) -> Config {
        let mut out = None;
        Jail::expect_with(|_| {
            out = Some(Config::parse_str(text).unwrap_or_else(|e| panic!("{e:#}")));
            Ok(())
        });
        out.unwrap()
    }

    fn fails(found: &[Finding]) -> Vec<&str> {
        found
            .iter()
            .filter_map(|f| match f {
                Finding::Fail(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    const KEY: &str = "k-SECRET-VALUE";

    fn binance_keys(name: &str) -> Option<String> {
        (name.starts_with("BINANCE_")).then(|| KEY.to_owned())
    }
    fn no_env(_: &str) -> Option<String> {
        None
    }
    fn nothing(_: &Path) -> bool {
        false
    }

    /// The embedded list is exactly `config/examples/*.toml`, sorted
    /// (a new example needs an entry; the registry test covers the rest).
    #[test]
    fn embedded_examples_match_the_directory() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/examples");
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .map(|p| p.file_stem().unwrap().to_str().unwrap().to_owned())
            .collect();
        on_disk.sort();
        let embedded: Vec<&str> = EXAMPLES.iter().map(|(n, _)| *n).collect();
        assert_eq!(embedded, on_disk);
        for name in strategy::names() {
            assert!(embedded.contains(&name), "{name} has no embedded example");
        }
    }

    /// Sentences until the summary says something, cut at a word when long.
    #[test]
    fn summary_reads_on_from_a_short_first_sentence() {
        let text =
            "# grid: one coin. Splits [lower,\n# upper] into levels. More.\n#\n# later\nmode = 1\n";
        assert_eq!(summary(text), "one coin. Splits [lower, upper] into levels");
        let wrapped = "# rhai: a script, loaded at startup, no\n# recompile. More.\n";
        assert_eq!(
            summary(wrapped),
            "a script, loaded at startup, no recompile"
        );
        assert_eq!(
            summary("# x: no period at all\n\nmode = 1"),
            "no period at all"
        );
        let long = format!("# x: {}.\n", "word, ".repeat(40));
        let s = summary(&long);
        assert!(
            s.ends_with("word…") && s.chars().count() <= SUMMARY_MAX,
            "{s}"
        );
        let listed = list();
        assert_eq!(listed.lines().count(), EXAMPLES.len());
        assert!(
            listed.lines().any(|l| l.starts_with("buy_and_hold ")
                && l.ends_with("the benchmark. Buys one coin once on its first candle and holds")),
            "{listed}"
        );
    }

    /// Every embedded example passes the check with keys set (except the
    /// backtest-only one, refused as `run` refuses it), and none prints
    /// a key's value.
    #[test]
    fn every_example_checks_clean_with_keys_set() {
        for (name, text) in EXAMPLES {
            if !strategy::names().contains(name) {
                continue; // behind a cargo feature this build lacks
            }
            let mut cfg = parsed(text);
            // A script path is relative to the repo root, where `run` starts.
            if let Some(toml::Value::String(s)) = cfg.strategy.params.get_mut("script") {
                *s = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join(&*s)
                    .display()
                    .to_string();
            }
            let found = check_loaded(&cfg, false, &binance_keys, &nothing);
            let shown = format!("{found:?}");
            assert!(!shown.contains(KEY), "{name}: a key value was printed");
            let f = fails(&found);
            if *name == "funding_carry" {
                assert_eq!(f.len(), 1, "{name}: {f:?}");
                assert!(f[0].contains("backtest only"), "{f:?}");
            } else {
                assert!(f.is_empty(), "{name}: {f:?}");
            }
        }
    }

    const SPOT: &str = r#"
        whitelist = ["BTCUSDT"]
        [exchange]
        network = "demo"
        [strategy]
        name = "sma_trend"
        params = { coin = "BTCUSDT", sma_period = 10, band_pct = "0.01", quote_amount = "100", fee = "0.001", slippage_bps = "5" }
    "#;

    /// Every problem is reported, not only the first: missing keys by
    /// name, a strategy that won't build, a set kill switch.
    #[test]
    fn reports_every_refusal_at_once() {
        let bad = SPOT.replace("sma_period = 10", "sma_period = 0");
        let cfg = parsed(&bad);
        let marker = |p: &Path| p.ends_with("binance-demo-sma_trend.txt");
        let found = check_loaded(&cfg, false, &no_env, &marker);
        let f = fails(&found);
        assert_eq!(f.len(), 3, "{f:?}");
        assert!(
            f[0].contains("BINANCE_API_KEY, BINANCE_API_SECRET not set"),
            "{f:?}"
        );
        assert!(f[1].starts_with("strategy sma_trend:"), "{f:?}");
        assert!(f[2].contains("--clear-kill-switch"), "{f:?}");
        assert!(found.contains(&Finding::Note(
            "books: sqlite://data/demo.db?mode=rwc".into()
        )));
        // A blank key counts as missing, as `run` reads it.
        let blank = |n: &str| n.starts_with("BINANCE_").then(|| " ".to_owned());
        assert_eq!(
            fails(&check_loaded(&parsed(SPOT), false, &blank, &nothing)).len(),
            1
        );
    }

    /// `--live` checks the config's two locks; a live config passes with
    /// the real-money note, and a paper config is told what's missing.
    #[test]
    fn live_checks_the_config_locks() {
        let live_cfg = format!(
            "mode = \"live\"\n{}",
            SPOT.replace("network = \"demo\"", "network = \"live\"")
        );
        let found = check_loaded(&parsed(&live_cfg), true, &binance_keys, &nothing);
        assert!(fails(&found).is_empty(), "{found:?}");
        assert!(found.contains(&Finding::Ok("run: orders on Binance spot live".into())));
        assert!(
            found
                .iter()
                .any(|f| matches!(f, Finding::Note(t) if t.starts_with("REAL MONEY")))
        );
        let f = check_loaded(&parsed(SPOT), true, &binance_keys, &nothing);
        let f = fails(&f);
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("mode = \"live\" in config") && !f[0].contains("--i-understand"));
        // And without --live, a live config is refused, not downgraded.
        let f = check_loaded(&parsed(&live_cfg), false, &binance_keys, &nothing);
        assert!(fails(&f)[0].contains("--live was not given"), "{f:?}");
    }

    /// MAX needs no key; eToro names its own two.
    #[test]
    fn keys_by_venue() {
        assert!(keys_for(Venue::Max).is_empty());
        assert_eq!(
            keys_for(Venue::Etoro),
            ["ETORO_PUBLIC_KEY", "ETORO_PRIVATE_KEY"]
        );
        assert_eq!(
            keys_for(Venue::BinanceUsdm),
            ["BINANCE_API_KEY", "BINANCE_API_SECRET"]
        );
    }

    /// `init` writes the example byte for byte, refuses to overwrite
    /// without --force, and refuses an unknown strategy.
    #[test]
    fn init_writes_the_example_and_never_overwrites_by_accident() {
        let dir = std::env::temp_dir().join(format!("cfg-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("nested/config.toml");
        init("dca", &out, false).unwrap();
        let want = EXAMPLES.iter().find(|(n, _)| *n == "dca").unwrap().1;
        assert_eq!(std::fs::read_to_string(&out).unwrap(), want);
        std::fs::write(&out, "mine").unwrap();
        let err = init("dca", &out, false).unwrap_err().to_string();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "mine");
        init("dca", &out, true).unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), want);
        let err = init("nope", &out, true).unwrap_err().to_string();
        assert!(err.contains("config list"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
