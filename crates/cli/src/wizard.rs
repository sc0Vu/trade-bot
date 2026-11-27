//! `config init` without `--strategy`: a run config built by questions.
//!
//! The strategy's embedded example ([`crate::configure::EXAMPLES`]) is
//! the template: every question's default is the example's value (or the
//! previous answer), every strategy param's help is the comment beside it
//! there, and the file written is that example edited in place
//! (`toml_edit`), so its comments survive. Strategy params are not
//! described by any schema; they are asked one by one in the example's
//! order and type (exact-decimal string, integer, bool, or a TOML list or
//! table). Changing the coins renames them in the params too (by position:
//! the example's i-th instrument becomes the i-th answered).
//!
//! The answers are only written once the config loads and its strategy
//! builds ([`crate::configure::check_loaded`]); otherwise every question
//! is asked again with the last answers as defaults.
//!
//! Paper only: the wizard never writes `mode = "live"` or `network =
//! "live"`. Going live stays two deliberate edits plus `--i-understand`
//! typed at each start (invariant 7). No key is asked for (invariant 6).

use crate::config::Config;
use crate::configure::{self, EXAMPLES, Finding};
use crate::preflight;
use anyhow::{Context, bail};
use bot_core::{Decimal, Interval};
use std::io::{BufRead, Write};
use std::path::Path;
use std::str::FromStr;
use toml_edit::{Array, DocumentMut, Item, Value, value};

/// Questions on `out`, answers from `input`, one line each; an empty line
/// takes the default. Also remembers, for the web page ([`step`]), the
/// last question asked, what was said before it, and which answers were
/// refused.
pub struct Prompt<'a> {
    input: &'a mut dyn BufRead,
    out: &'a mut dyn Write,
    /// The question waiting for (or last given) an answer.
    asked: Option<Question>,
    /// Lines said since the last question.
    notes: Vec<String>,
    /// Answers read so far.
    read: usize,
    /// Indices (in reading order) of answers refused, with why.
    refused: Vec<(usize, String)>,
    /// The input ran out.
    ended: bool,
}

/// One question, as the web page shows it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Question {
    pub text: String,
    /// What an empty answer means.
    pub default: String,
    /// The accepted answers, when it is a choice (empty: free text).
    pub options: Vec<String>,
    /// A label per option, if the options need one (strategy summaries).
    pub labels: Vec<String>,
    /// What was said before it (section headers, a failed check).
    pub notes: Vec<String>,
}

impl<'a> Prompt<'a> {
    pub fn new(input: &'a mut dyn BufRead, out: &'a mut dyn Write) -> Self {
        Self {
            input,
            out,
            asked: None,
            notes: Vec::new(),
            read: 0,
            refused: Vec::new(),
            ended: false,
        }
    }

    /// Prints a line (no question); the page shows it with the next one.
    fn say(&mut self, text: &str) -> anyhow::Result<()> {
        self.notes.push(text.trim().to_owned());
        self.print(text)
    }

    /// Prints a line for the terminal only (the page shows it otherwise).
    fn print(&mut self, text: &str) -> anyhow::Result<()> {
        writeln!(self.out, "{text}")?;
        Ok(())
    }

    /// One answer: the line typed, trimmed, or `default` if empty. Input
    /// ending is an error (nothing is written).
    fn ask(&mut self, q: Question) -> anyhow::Result<String> {
        let shown = if q.options.is_empty() || !q.labels.is_empty() {
            q.text.clone()
        } else {
            format!("{} ({})", q.text, q.options.join("/"))
        };
        if q.default.is_empty() {
            write!(self.out, "{shown}: ")?;
        } else {
            write!(self.out, "{shown} [{}]: ", q.default)?;
        }
        self.out.flush()?;
        let default = q.default.clone();
        let mut q = q;
        q.notes = std::mem::take(&mut self.notes);
        self.asked = Some(q);
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            self.ended = true;
            bail!("input ended; nothing written");
        }
        self.read += 1;
        let line = line.trim();
        Ok(if line.is_empty() {
            default
        } else {
            line.to_owned()
        })
    }

    /// Asks until `parse` accepts the answer; its error is shown and the
    /// question repeated.
    fn ask_valid<T>(
        &mut self,
        question: &str,
        default: &str,
        parse: impl Fn(&str) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.ask_question(free(question, default), parse)
    }

    fn ask_question<T>(
        &mut self,
        q: Question,
        parse: impl Fn(&str) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        loop {
            let answer = self.ask(q.clone())?;
            match parse(&answer) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let why = format!("{e:#}");
                    self.refused.push((self.read - 1, why.clone()));
                    self.print(&format!("  {why}"))?;
                }
            }
        }
    }

    /// One of `options`.
    fn choose(
        &mut self,
        question: &str,
        options: &[&str],
        default: &str,
    ) -> anyhow::Result<String> {
        let q = Question {
            options: options.iter().map(|o| (*o).to_owned()).collect(),
            ..free(question, default)
        };
        self.ask_question(q, |a| {
            options
                .iter()
                .find(|o| o.eq_ignore_ascii_case(a))
                .map(|o| (*o).to_owned())
                .with_context(|| format!("one of {}", options.join(", ")))
        })
    }

    fn yes(&mut self, question: &str, default: bool) -> anyhow::Result<bool> {
        let d = if default { "y" } else { "n" };
        let a = self.choose(question, &["y", "n"], d)?;
        Ok(a == "y")
    }
}

/// A free-text question.
fn free(text: &str, default: &str) -> Question {
    Question {
        text: text.to_owned(),
        default: default.to_owned(),
        options: Vec::new(),
        labels: Vec::new(),
        notes: Vec::new(),
    }
}

/// Runs the questions and writes `out` (asking before replacing it unless
/// `force`). `strategy` skips the strategy question. Returns the check of
/// the written file, or `None` when the user stopped without writing.
pub fn run(
    p: &mut Prompt,
    strategy: Option<&str>,
    out: &Path,
    force: bool,
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> anyhow::Result<Option<Vec<Finding>>> {
    let Some((text, found)) = build(p, strategy, out, env, exists)? else {
        return Ok(None);
    };
    if exists(out) && !force && !p.yes(&format!("{} exists; replace it?", out.display()), false)? {
        p.say("nothing written")?;
        return Ok(None);
    }
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(out, &text).with_context(|| format!("writing {}", out.display()))?;
    p.say(&format!("wrote {}", out.display()))?;
    Ok(Some(found))
}

/// The questions, until the answers make a config that loads, routes and
/// builds its strategy: its text (headed for `out`) and its check. `None`
/// when the user stopped after a failed check. Writes nothing.
fn build(
    p: &mut Prompt,
    strategy: Option<&str>,
    out: &Path,
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> anyhow::Result<Option<(String, Vec<Finding>)>> {
    p.print(
        "New run config. Enter keeps the [default]. Paper only: nothing here trades real money.",
    )?;
    let name = match strategy {
        Some(s) => s.to_owned(),
        None => pick_strategy(p)?,
    };
    let Some((_, example)) = EXAMPLES.iter().find(|(n, _)| *n == name) else {
        bail!("no strategy {name:?}; `trade-bot config list` shows them");
    };
    if name == "funding_carry" {
        bail!("funding_carry is backtest only: a run trades one venue (see its example)");
    }
    let mut doc: DocumentMut = example.parse().context("embedded example")?;
    loop {
        ask_all(p, &mut doc)?;
        // The example's usage line names its own path; point it here.
        let text = doc.to_string().replace(
            &format!("config/examples/{name}.toml"),
            &out.display().to_string(),
        );
        let blocking = blocking(&text);
        if blocking.is_empty() {
            let found = match Config::parse_str(&text) {
                Ok(cfg) => configure::check_loaded(&cfg, false, env, exists),
                Err(e) => vec![Finding::Fail(format!("{e:#}"))],
            };
            return Ok(Some((text, found)));
        }
        for b in &blocking {
            p.say(&format!("FAIL  {b}"))?;
        }
        if !p.yes("Answer again (your answers are the new defaults)?", true)? {
            p.say("nothing written")?;
            return Ok(None);
        }
    }
}

/// Where the questions stand after `answers` (the web page's state: it
/// keeps the answers and sends them all each time; the questions are
/// replayed, so the same answers always reach the same place).
#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Step {
    /// The next question. `error` says why the last answer was refused.
    Ask {
        question: Question,
        error: Option<String>,
        /// The answers that stood (refused ones dropped): the page's new state.
        answers: Vec<String>,
        /// How many questions answered so far.
        progress: usize,
    },
    /// The config, ready.
    Done {
        toml: String,
        findings: Vec<(String, String)>,
        answers: Vec<String>,
        /// What a backtest of it needs (the console's backtest form).
        backtest: BacktestHint,
    },
    /// The user declined to answer again after a failed check.
    Stopped { answers: Vec<String> },
}

/// A finished config's strategy, params and market, for a backtest.
#[derive(Debug, Default, serde::Serialize)]
pub struct BacktestHint {
    pub strategy: String,
    /// `[strategy.params]` as its own TOML table.
    pub params: String,
    pub venue: String,
    pub interval: String,
    /// Whitelisted symbols without the venue.
    pub symbols: Vec<String>,
    pub history_len: Option<i64>,
    pub fee: Option<String>,
    pub slippage_bps: Option<String>,
}

/// Most answers [`step`] takes (a full run is a few dozen).
pub const MAX_ANSWERS: usize = 400;

/// Replays `answers` through the questions ([`build`]) and says what comes
/// next. `env` and `exists` as for [`configure::check_loaded`]. `out` is
/// the path the config's usage comment names.
pub fn step(
    answers: &[String],
    out: &Path,
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> anyhow::Result<Step> {
    if answers.len() > MAX_ANSWERS {
        bail!("too many answers");
    }
    if answers
        .iter()
        .any(|a| a.contains(['\n', '\r']) || a.len() > 4096)
    {
        bail!("an answer is one line, at most 4096 bytes");
    }
    let text: String = answers.iter().map(|a| format!("{a}\n")).collect();
    let mut input = std::io::Cursor::new(text.into_bytes());
    let mut sink = std::io::sink();
    let mut p = Prompt::new(&mut input, &mut sink);
    let result = build(&mut p, None, out, env, exists);
    let stood: Vec<String> = answers
        .iter()
        .enumerate()
        .filter(|(i, _)| !p.refused.iter().any(|(r, _)| r == i))
        .map(|(_, a)| a.clone())
        .collect();
    match result {
        Ok(Some((toml, found))) => Ok(Step::Done {
            backtest: hint(&toml),
            toml,
            findings: found
                .into_iter()
                .map(|f| match f {
                    Finding::Ok(t) => ("ok".into(), t),
                    Finding::Note(t) => ("note".into(), t),
                    Finding::Fail(t) => ("fail".into(), t),
                })
                .collect(),
            answers: stood,
        }),
        Ok(None) => Ok(Step::Stopped { answers: stood }),
        Err(_) if p.ended => {
            let question = p.asked.take().context("no question asked")?;
            let error = p
                .refused
                .iter()
                .find(|(i, _)| i + 1 == answers.len())
                .map(|(_, why)| why.clone());
            Ok(Step::Ask {
                progress: stood.len(),
                question,
                error,
                answers: stood,
            })
        }
        Err(e) => Err(e),
    }
}

/// What the console's backtest form needs from a finished config.
fn hint(text: &str) -> BacktestHint {
    let Ok(t) = text.parse::<toml::Table>() else {
        return BacktestHint::default();
    };
    let get = |path: &[&str]| {
        let mut v = t.get(path[0])?;
        for k in &path[1..] {
            v = v.get(k)?;
        }
        Some(v)
    };
    let s = |path: &[&str]| get(path).and_then(|v| v.as_str()).map(str::to_owned);
    let params = get(&["strategy", "params"])
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default();
    BacktestHint {
        strategy: s(&["strategy", "name"]).unwrap_or_default(),
        params: toml::to_string(&params).unwrap_or_default(),
        venue: s(&["exchange", "venue"]).unwrap_or_else(|| "binance".into()),
        interval: s(&["exchange", "interval"]).unwrap_or_default(),
        symbols: get(&["whitelist"])
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|id| id.split_once(':').map_or(id, |(_, s)| s).to_owned())
                    .collect()
            })
            .unwrap_or_default(),
        history_len: get(&["engine", "history_len"]).and_then(|v| v.as_integer()),
        fee: s(&["strategy", "params", "fee"]).or_else(|| s(&["paper", "fee"])),
        slippage_bps: s(&["strategy", "params", "slippage_bps"])
            .or_else(|| s(&["paper", "slippage_bps"])),
    }
}

/// What keeps `text` from being a config `run` could start (besides keys
/// and a kill switch, which the written file's check reports): it doesn't
/// load, its run is refused, or its strategy doesn't build.
fn blocking(text: &str) -> Vec<String> {
    let cfg = match Config::parse_str(text) {
        Ok(c) => c,
        Err(e) => return vec![format!("{e:#}")],
    };
    let mut out = Vec::new();
    if let Err(e) = preflight::route(&cfg, false, false) {
        out.push(format!("{e:#}"));
    }
    let params = strategy::Params::new(cfg.strategy.params.clone());
    if let Err(e) = strategy::build(&cfg.strategy.name, &params) {
        out.push(format!("strategy {}: {e}", cfg.strategy.name));
    }
    out
}

/// The strategy, by number or name from the list.
fn pick_strategy(p: &mut Prompt) -> anyhow::Result<String> {
    let names: Vec<&str> = EXAMPLES
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| *n != "funding_carry")
        .collect();
    let listing = configure::list();
    let lines: Vec<&str> = listing
        .lines()
        .filter(|l| !l.starts_with("funding_carry "))
        .collect();
    for (i, line) in lines.iter().enumerate() {
        p.print(&format!("{:>3}  {line}", i + 1))?;
    }
    let q = Question {
        options: names.iter().map(|n| (*n).to_owned()).collect(),
        labels: lines
            .iter()
            .map(|l| {
                let (name, summary) = l.split_once(' ').unwrap_or((l, ""));
                format!("{name} — {}", summary.trim())
            })
            .collect(),
        ..free("Strategy (number or name)", "sma_trend")
    };
    p.ask_question(q, |a| {
        if let Ok(i) = a.parse::<usize>() {
            return names
                .get(i.wrapping_sub(1))
                .map(|n| (*n).to_owned())
                .with_context(|| format!("1 to {}", names.len()));
        }
        names
            .iter()
            .find(|n| n.eq_ignore_ascii_case(a))
            .map(|n| (*n).to_owned())
            .context("not a strategy in the list")
    })
}

const VENUES: [&str; 4] = ["binance", "binance-usdm", "max", "etoro"];

/// Default tickers for an eToro stock config: broad ETFs, then large caps
/// (the set `make etoro-stocks-data` caches), none near a split.
const ETORO_STOCKS: [&str; 6] = ["SPY", "QQQ", "AAPL", "MSFT", "AMZN", "META"];

/// eToro crypto symbols the wizard recognises as crypto, to keep a crypto
/// config crypto when its questions are asked again.
const ETORO_CRYPTO: [&str; 8] = ["BTC", "ETH", "SOL", "XRP", "ADA", "DOGE", "BNB", "LTC"];

/// Every question, defaults read from `doc`, answers written into it.
fn ask_all(p: &mut Prompt, doc: &mut DocumentMut) -> anyhow::Result<()> {
    let old_venue = str_at(doc, &["exchange", "venue"])
        .unwrap_or("binance")
        .to_owned();
    let old_ids = ids(doc, &old_venue);

    let venue = p.choose("Venue", &VENUES, &old_venue)?;
    let old_symbols: Vec<&str> = old_ids
        .iter()
        .map(|id| id.split_once(':').map_or(id.as_str(), |(_, s)| s))
        .collect();
    // eToro lists stocks, ETFs and crypto; which one decides the tickers,
    // the trading hours and a sensible bar.
    let mut rth = false;
    let mut stocks_from_elsewhere = false;
    let default_symbols: Vec<String> = if venue == "etoro" {
        let was_etoro = old_venue == "etoro";
        let crypto_now = was_etoro && old_symbols.iter().all(|s| ETORO_CRYPTO.contains(s));
        let market = p.choose(
            "eToro market: US stocks and ETFs, or crypto",
            &["stocks", "crypto"],
            if crypto_now { "crypto" } else { "stocks" },
        )?;
        if market == "stocks" {
            stocks_from_elsewhere = !was_etoro || crypto_now;
            let regular = !was_etoro || old_symbols.iter().all(|s| s.ends_with(".RTH"));
            rth = p.choose(
                "Trading hours: regular (9:30-16:00 New York) or extended (pre- and after-market too)",
                &["regular", "extended"],
                if regular { "regular" } else { "extended" },
            )? == "regular";
            if was_etoro && !crypto_now {
                old_symbols
                    .iter()
                    .map(|s| s.trim_end_matches(".RTH").to_owned())
                    .collect()
            } else {
                // As many tickers as the example has instruments.
                ETORO_STOCKS
                    .iter()
                    .cycle()
                    .take(old_symbols.len().clamp(1, ETORO_STOCKS.len()))
                    .map(|s| (*s).to_owned())
                    .collect()
            }
        } else if crypto_now {
            old_symbols.iter().map(|s| (*s).to_owned()).collect()
        } else {
            // eToro names crypto by coin (etoro:BTC), not by pair.
            old_symbols
                .iter()
                .map(|s| s.strip_suffix("USDT").unwrap_or(s).to_owned())
                .collect()
        }
    } else {
        old_symbols
            .iter()
            .map(|s| s.trim_end_matches(".RTH").to_owned())
            .collect()
    };
    let example = if venue == "etoro" { "SPY" } else { "BTCUSDT" };
    let symbols = p.ask_valid(
        "Instruments (comma-separated symbols)",
        &default_symbols.join(","),
        |a| {
            let v: Vec<String> = a
                .split(',')
                .map(|s| s.trim().to_ascii_uppercase())
                .filter(|s| !s.is_empty())
                .collect();
            if v.is_empty() {
                bail!("at least one symbol");
            }
            if v.iter().any(|s| s.contains(':') || s.contains(' ')) {
                bail!("symbols only, e.g. {example} (the venue is the one chosen)");
            }
            Ok(v)
        },
    )?;
    let symbols: Vec<String> = symbols
        .into_iter()
        .map(|s| {
            // The hours answer decides the suffix, whatever was typed.
            let bare = s.trim_end_matches(".RTH").to_owned();
            if rth { format!("{bare}.RTH") } else { bare }
        })
        .collect();
    // Stocks trade one session a day: daily bars by default when a crypto
    // example becomes a stock config.
    if stocks_from_elsewhere {
        table(doc, "exchange")["interval"] = value("1d");
    }

    let new_ids: Vec<String> = symbols.iter().map(|s| format!("{venue}:{s}")).collect();
    doc["whitelist"] = value(Array::from_iter(new_ids.iter().map(String::as_str)));

    let ex = table(doc, "exchange");
    ex["venue"] = value(&venue);
    let interval = str_at(doc, &["exchange", "interval"])
        .unwrap_or("1h")
        .to_owned();
    let no_1s = venue != "binance";
    let interval = p.ask_valid("Candle interval (1m, 5m, 1h, 4h, 1d, ...)", &interval, |a| {
        let i: Interval = toml::Value::String(a.to_owned())
            .try_into()
            .map_err(|_| anyhow::anyhow!("not an interval Binance names (1s 1m 3m 5m 15m 30m 1h 2h 4h 6h 8h 12h 1d 3d 1w 1M)"))?;
        if no_1s && i == Interval::S1 {
            bail!("only Binance spot serves 1s bars");
        }
        Ok(a.to_owned())
    })?;
    table(doc, "exchange")["interval"] = value(interval);

    let usdm = venue == "binance-usdm";
    let fills = if usdm {
        let d = str_at(doc, &["perp", "fills"])
            .unwrap_or("local")
            .to_owned();
        p.choose(
            "Futures fills: local paper broker, or orders on the futures testnet/demo",
            &["local", "venue"],
            &d,
        )?
    } else {
        String::new()
    };
    match venue.as_str() {
        "binance" => ask_network(p, doc, "Binance network (Demo Trading or the spot testnet)")?,
        "binance-usdm" if fills == "venue" => ask_network(p, doc, "Futures network")?,
        "etoro" => {
            // Both fake-money networks are eToro's demo account.
            table(doc, "exchange")["network"] = value("demo");
            p.say("  eToro paper = the demo account (keys from it)")?;
        }
        _ => {
            // MAX and local perp fills ignore it: nothing is sent.
            table(doc, "exchange").remove("network");
        }
    }

    let paper = venue == "max" || (usdm && fills == "local");
    let fee_default = match venue.as_str() {
        "binance-usdm" => "0.0005",
        "max" => "0.0015",
        _ => "0.001",
    };
    let mut paper_costs = None;
    if paper {
        p.say("Paper broker (local fills, money as exact decimals):")?;
        let cash = str_at(doc, &["paper", "initial_cash"])
            .unwrap_or("1000")
            .to_owned();
        let fee = str_at(doc, &["paper", "fee"])
            .unwrap_or(fee_default)
            .to_owned();
        let slip = str_at(doc, &["paper", "slippage_bps"])
            .unwrap_or("5")
            .to_owned();
        let cash = p.ask_valid("  initial cash (quote)", &cash, positive)?;
        let fee = p.ask_valid("  fee rate per fill, e.g. 0.001 = 0.1%", &fee, rate)?;
        let slip = p.ask_valid("  slippage, basis points", &slip, non_negative)?;
        let t = table(doc, "paper");
        t["initial_cash"] = value(&cash);
        t["fee"] = value(&fee);
        t["slippage_bps"] = value(&slip);
        paper_costs = Some((fee, slip));
    } else {
        doc.remove("paper");
    }
    if usdm {
        let lev = int_at(doc, &["perp", "leverage"]).unwrap_or(1);
        let margin = str_at(doc, &["perp", "margin"])
            .unwrap_or("isolated")
            .to_owned();
        let brackets = str_at(doc, &["perp", "brackets_from"])
            .unwrap_or("demo")
            .to_owned();
        let lev = p.ask_valid("Leverage", &lev.to_string(), |a| {
            let n: i64 = a.parse().context("a whole number")?;
            if !(1..=125).contains(&n) {
                bail!("1 to 125");
            }
            Ok(n)
        })?;
        let margin = p.choose("Margin", &["isolated", "cross"], &margin)?;
        let t = table(doc, "perp");
        t["leverage"] = value(lev);
        t["margin"] = value(&margin);
        t["fills"] = value(&fills);
        if fills == "local" {
            let b = p.choose(
                "Network your API key belongs to (margin brackets are read signed)",
                &["demo", "testnet"],
                &brackets,
            )?;
            table(doc, "perp")["brackets_from"] = value(b);
        } else {
            table(doc, "perp").remove("brackets_from");
        }
    } else {
        doc.remove("perp");
    }

    let hist = int_at(doc, &["engine", "history_len"]).unwrap_or(500);
    let hist = p.ask_valid(
        "History kept, in bars (must cover the strategy's longest lookback)",
        &hist.to_string(),
        |a| {
            let n: i64 = a.parse().context("a whole number")?;
            if n < 1 {
                bail!("at least 1");
            }
            Ok(n)
        },
    )?;
    table(doc, "engine")["history_len"] = value(hist);

    ask_params(p, doc, &old_ids, &new_ids, paper_costs.as_ref())?;

    let name = doc
        .get("name")
        .and_then(Item::as_str)
        .unwrap_or(NO_NAME)
        .to_owned();
    let name = p.ask_valid(
        &format!(
            "Bot name: its own books and kill switch, for several bots on one machine \
             (a-z 0-9 _ -; {NO_NAME:?} for none)"
        ),
        &name,
        |a| {
            if a == NO_NAME {
                return Ok(None);
            }
            let ok = a.len() <= 64
                && a.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
                });
            if !ok {
                bail!("lowercase letters, digits, _ and - only, at most 64");
            }
            Ok(Some(a.to_owned()))
        },
    )?;
    match name {
        Some(n) => doc["name"] = value(n),
        None => {
            doc.remove("name");
        }
    }
    Ok(())
}

/// The answer that leaves a config without a bot name.
const NO_NAME: &str = "none";

/// The fake-money network of a Binance run.
fn ask_network(p: &mut Prompt, doc: &mut DocumentMut, question: &str) -> anyhow::Result<()> {
    let d = str_at(doc, &["exchange", "network"])
        .filter(|n| *n != "live")
        .unwrap_or("testnet")
        .to_owned();
    let n = p.choose(question, &["testnet", "demo"], &d)?;
    table(doc, "exchange")["network"] = value(n);
    Ok(())
}

/// Each `[strategy.params]` entry in the example's order and type; its
/// comment is the help. Instruments renamed first (`old` → `new` by
/// position), the paper broker's fee and slippage as the cost defaults.
fn ask_params(
    p: &mut Prompt,
    doc: &mut DocumentMut,
    old: &[String],
    new: &[String],
    costs: Option<&(String, String)>,
) -> anyhow::Result<()> {
    let name = str_at(doc, &["strategy", "name"]).unwrap_or("?").to_owned();
    let Some(params) = doc["strategy"]["params"].as_table_like_mut() else {
        return Ok(());
    };
    p.say(&format!("{name} params (comments from its example):"))?;
    let keys: Vec<String> = params.iter().map(|(k, _)| k.to_owned()).collect();
    for key in keys {
        let Some(v) = params.get_mut(&key).and_then(Item::as_value_mut) else {
            continue;
        };
        if key != "script" {
            rename(v, old, new);
        }
        if let (Some((fee, slip)), Value::String(_)) = (costs, &*v) {
            let d = match key.as_str() {
                "fee" => Some(fee),
                "slippage_bps" => Some(slip),
                _ => None,
            };
            if let Some(d) = d {
                set_keep_decor(v, Value::from(d.as_str()));
            }
        }
        let help = v
            .decor()
            .suffix()
            .and_then(|s| s.as_str())
            .map(|s| s.trim().trim_start_matches('#').trim().to_owned())
            .filter(|s| !s.is_empty())
            .or_else(|| common_help(&key).map(str::to_owned));
        let question = match &help {
            Some(h) => format!("  {key} ({h})"),
            None => format!("  {key}"),
        };
        let current = v.clone();
        let default = shown(&current);
        let answer = p.ask_valid(&question, &default, |a| parse_like(&current, a))?;
        set_keep_decor(v, answer);
    }
    Ok(())
}

/// Help for params many strategies share, where the example has none.
fn common_help(key: &str) -> Option<&'static str> {
    Some(match key {
        "fee" => "fee rate per fill the strategy assumes, e.g. 0.001 = 0.1%; match the venue",
        "slippage_bps" => "slippage per market fill the strategy assumes, basis points",
        _ => return None,
    })
}

/// `answer` as a value of `like`'s kind: an exact decimal where the
/// example has a decimal string, else any string; an integer; a bool; a
/// TOML list or table written as in the file.
fn parse_like(like: &Value, answer: &str) -> anyhow::Result<Value> {
    Ok(match like {
        Value::String(s) => {
            let text = answer.trim_matches('"');
            if Decimal::from_str(s.value()).is_ok() {
                Decimal::from_str(text).context("an exact decimal, e.g. 0.001")?;
            }
            Value::from(text)
        }
        Value::Integer(_) => Value::from(answer.parse::<i64>().context("a whole number")?),
        Value::Boolean(_) => Value::from(match answer {
            "true" => true,
            "false" => false,
            _ => bail!("true or false"),
        }),
        Value::Array(_) | Value::InlineTable(_) => {
            let doc: DocumentMut = format!("v = {answer}")
                .parse()
                .context("TOML, written as shown in the default")?;
            let v = doc["v"].as_value().context("a value")?.clone();
            if std::mem::discriminant(&v) != std::mem::discriminant(like) {
                bail!("same shape as the default ({})", shown(like));
            }
            if contains_float(&v) {
                bail!("money and ratios go in quotes (\"0.5\"): a bare decimal is a float");
            }
            v
        }
        Value::Float(_) => bail!("floats are not accepted (invariant 1)"),
        Value::Datetime(_) => Value::from(answer),
    })
}

fn contains_float(v: &Value) -> bool {
    match v {
        Value::Float(_) => true,
        Value::Array(a) => a.iter().any(contains_float),
        Value::InlineTable(t) => t.iter().any(|(_, v)| contains_float(v)),
        _ => false,
    }
}

/// A value as typed at the prompt: strings bare, the rest as TOML.
fn shown(v: &Value) -> String {
    match v {
        Value::String(s) => s.value().clone(),
        other => {
            let mut c = other.clone();
            c.decor_mut().clear();
            c.to_string().trim().to_owned()
        }
    }
}

/// Replaces `v`, keeping its comment.
fn set_keep_decor(v: &mut Value, mut new: Value) {
    *new.decor_mut() = v.decor().clone();
    *v = new;
}

/// Strings (and table keys) naming `old[i]` (full id, or its bare symbol)
/// become `new[i]`; the whole old list becomes the whole new one.
fn rename(v: &mut Value, old: &[String], new: &[String]) {
    let map = |s: &str| -> Option<String> {
        let i = old.iter().position(|o| {
            o.eq_ignore_ascii_case(s)
                || (!s.contains(':')
                    && o.split_once(':')
                        .is_some_and(|(_, sym)| sym.eq_ignore_ascii_case(s)))
        })?;
        new.get(i).cloned()
    };
    match v {
        Value::String(s) => {
            if let Some(n) = map(s.value()) {
                set_keep_decor(v, Value::from(n));
            }
        }
        Value::Array(a) => {
            let all: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect();
            let whole = all.len() == a.len()
                && all.len() == old.len()
                && all
                    .iter()
                    .enumerate()
                    .all(|(i, s)| map(s) == new.get(i).cloned());
            if whole && new.len() != old.len() {
                let decor = a.decor().clone();
                let mut fresh = Array::from_iter(new.iter().map(String::as_str));
                *fresh.decor_mut() = decor;
                *a = fresh;
            } else {
                for x in a.iter_mut() {
                    rename(x, old, new);
                }
            }
        }
        Value::InlineTable(t) => {
            let pairs: Vec<(String, Value)> =
                t.iter().map(|(k, v)| (k.to_owned(), v.clone())).collect();
            t.clear();
            for (k, mut v) in pairs {
                rename(&mut v, old, new);
                let key = map(&k).unwrap_or(k);
                t.insert(&key, v);
            }
        }
        _ => {}
    }
}

/// The example's whitelist as full ids (bare ones on `venue`).
fn ids(doc: &DocumentMut, venue: &str) -> Vec<String> {
    doc.get("whitelist")
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| {
                    if s.contains(':') {
                        s.to_owned()
                    } else {
                        format!("{venue}:{s}")
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The top-level table `name`, created at the end if missing.
fn table<'d>(doc: &'d mut DocumentMut, name: &str) -> &'d mut toml_edit::Table {
    if !doc.contains_table(name) {
        doc[name] = toml_edit::table();
    }
    doc[name]
        .as_table_mut()
        .unwrap_or_else(|| unreachable!("just made a table"))
}

fn at<'d>(doc: &'d DocumentMut, path: &[&str]) -> Option<&'d Item> {
    let mut item = doc.as_item();
    for k in path {
        item = item.get(k)?;
    }
    Some(item)
}

fn str_at<'d>(doc: &'d DocumentMut, path: &[&str]) -> Option<&'d str> {
    at(doc, path)?.as_str()
}

fn int_at(doc: &DocumentMut, path: &[&str]) -> Option<i64> {
    at(doc, path)?.as_integer()
}

fn positive(a: &str) -> anyhow::Result<String> {
    let d = Decimal::from_str(a).context("an exact decimal")?;
    if d <= Decimal::ZERO {
        bail!("above 0");
    }
    Ok(a.to_owned())
}

fn rate(a: &str) -> anyhow::Result<String> {
    let d = Decimal::from_str(a).context("an exact decimal, e.g. 0.001")?;
    if d < Decimal::ZERO || d >= Decimal::ONE {
        bail!("in [0, 1): 0.001 is 0.1%");
    }
    Ok(a.to_owned())
}

fn non_negative(a: &str) -> anyhow::Result<String> {
    let d = Decimal::from_str(a).context("an exact decimal")?;
    if d < Decimal::ZERO {
        bail!("0 or more");
    }
    Ok(a.to_owned())
}

#[cfg(test)]
// figment's Jail API returns its own large error type; not ours to shrink.
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;
    use crate::preflight::Route;
    use exchange::binance::Network;
    use figment::Jail;
    use std::path::PathBuf;

    struct Run {
        result: anyhow::Result<Option<Vec<Finding>>>,
        shown: String,
        out: PathBuf,
        dir: PathBuf,
    }

    impl Run {
        fn text(&self) -> String {
            std::fs::read_to_string(&self.out).unwrap()
        }
        /// The written file as `run` loads it (in a jail: no `BOT_*`).
        fn cfg(&self) -> Config {
            let text = self.text();
            let mut out = None;
            Jail::expect_with(|_| {
                out = Some(Config::parse_str(&text).unwrap());
                Ok(())
            });
            out.unwrap()
        }
    }

    impl Drop for Run {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The wizard on `answers` (one line each; "" = Enter), writing into
    /// a fresh temp dir; `existing` pre-creates the output file. Inside a
    /// jail, so the shell's `BOT_*` can't change what loads.
    fn drive(tag: &str, strategy: Option<&str>, answers: &[&str], existing: bool) -> Run {
        let dir = std::env::temp_dir().join(format!("wizard-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("config.toml");
        if existing {
            std::fs::write(&out, "mine").unwrap();
        }
        let mut input = answers.iter().map(|a| format!("{a}\n")).collect::<String>();
        let mut shown = Vec::new();
        let mut result = None;
        Jail::expect_with(|_| {
            let mut reader = std::io::Cursor::new(std::mem::take(&mut input).into_bytes());
            let mut p = Prompt::new(&mut reader, &mut shown);
            let keys = |n: &str| n.starts_with("BINANCE_").then(|| "k".to_owned());
            result = Some(run(&mut p, strategy, &out, false, &keys, &|p: &Path| {
                p.exists()
            }));
            Ok(())
        });
        Run {
            result: result.unwrap(),
            shown: String::from_utf8(shown).unwrap(),
            out,
            dir,
        }
    }

    fn route(c: &Config) -> Route {
        preflight::route(c, false, false).unwrap()
    }

    fn param<'c>(c: &'c Config, key: &str) -> &'c toml::Value {
        c.strategy.params.get(key).unwrap()
    }

    /// Enter on every question reproduces the example: same instruments,
    /// network, interval, params; comments kept; mode stays paper.
    #[test]
    fn all_defaults_reproduce_the_example() {
        // strategy, venue, symbols, interval, network, history, 6 params
        let r = drive("defaults", None, &[""; 13], false);
        let found = r.result.as_ref().unwrap().as_ref().unwrap();
        assert!(
            !found.iter().any(|f| matches!(f, Finding::Fail(_))),
            "{found:?}"
        );
        let c = r.cfg();
        assert_eq!(c.strategy.name, "sma_trend");
        assert_eq!(c.whitelist[0].to_string(), "binance:BTCUSDT");
        assert_eq!(route(&c), Route::Spot(Network::Testnet));
        assert_eq!(c.exchange.interval, Interval::H4);
        assert_eq!(c.engine.history_len, 500);
        assert_eq!(param(&c, "sma_period").as_integer(), Some(200));
        assert_eq!(param(&c, "band_pct").as_str(), Some("0.01"));
        let text = r.text();
        assert!(
            text.contains("# closes in the average; <= engine.history_len"),
            "{text}"
        );
        assert!(text.contains("mode = \"paper\""), "{text}");
        assert!(
            text.contains(&format!("--config {}", r.out.display())),
            "{text}"
        );
        // Help shown: the example's comment, else the shared one.
        assert!(
            r.shown.contains("sma_period (closes in the average"),
            "{}",
            r.shown
        );
        assert!(r.shown.contains("fee (fee rate per fill"), "{}", r.shown);
    }

    /// New coins replace the example's in the params: a single coin, a
    /// list that changes length, and a weights table's keys.
    #[test]
    fn coins_follow_the_instruments_answered() {
        let mut a = vec!["sma_trend", "", "ethusdt", "", "", ""];
        a.extend([""; 7]);
        let r = drive("one", None, &a, false);
        assert_eq!(param(&r.cfg(), "coin").as_str(), Some("binance:ETHUSDT"));

        // momentum_rotation: 4 example coins, answered 2.
        let mut a = vec!["momentum_rotation", "", "BTCUSDT, SOLUSDT", "", "", ""];
        a.extend([""; 7]);
        let r = drive("list", None, &a, false);
        let c = r.cfg();
        let coins: Vec<&str> = param(&c, "coins")
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(coins, ["binance:BTCUSDT", "binance:SOLUSDT"]);

        let mut a = vec!["rebalance", "", "ETHUSDT", "", "", ""];
        a.extend([""; 6]);
        let r = drive("table", None, &a, false);
        let c = r.cfg();
        let w = param(&c, "weights").as_table().unwrap();
        assert_eq!(
            w.get("binance:ETHUSDT").and_then(|v| v.as_str()),
            Some("0.5")
        );
        assert_eq!(w.len(), 1);
    }

    /// MAX: a [paper] broker asked for, no network written, the paper
    /// fee and slippage as the params' defaults.
    #[test]
    fn max_asks_for_a_paper_broker() {
        let mut a = vec![
            "buy_and_hold",
            "max",
            "BTCTWD",
            "1h",
            "30000",
            "0.0015",
            "20",
            "",
        ];
        a.extend([""; 5]); // coin, quote_amount, fee, slippage_bps, name
        let r = drive("max", None, &a, false);
        let c = r.cfg();
        assert_eq!(route(&c), Route::MaxPaper);
        assert_eq!(c.whitelist[0].to_string(), "max:BTCTWD");
        assert_eq!(param(&c, "coin").as_str(), Some("max:BTCTWD"));
        let p = c.paper.as_ref().unwrap();
        assert_eq!(p.initial_cash.to_string(), "30000");
        assert_eq!(param(&c, "fee").as_str(), Some("0.0015"));
        assert_eq!(param(&c, "slippage_bps").as_str(), Some("20"));
        assert!(!r.text().contains("network"), "{}", r.text());
    }

    /// The perp example keeps its sections on USDⓈ-M; moved to spot, they
    /// go (spot refuses [perp]).
    #[test]
    fn perp_sections_come_and_go_with_the_venue() {
        // sma_long_short: venue, symbols, interval, fills, cash, fee, slip,
        // leverage, margin, brackets, history, 6 params
        let mut a = vec!["sma_long_short"];
        a.extend(["", "", "", "", "", "", "", "3", "cross", "", ""]);
        a.extend([""; 7]);
        let r = drive("usdm", None, &a, false);
        let c = r.cfg();
        assert_eq!(route(&c), Route::UsdmPaper);
        let perp = c.perp.as_ref().unwrap();
        assert_eq!(perp.leverage, 3);
        assert_eq!(perp.margin, bot_core::margin::MarginMode::Cross);

        let mut a = vec!["sma_long_short", "binance", "", "", "demo", ""];
        a.extend([""; 7]);
        let r = drive("to-spot", None, &a, false);
        let c = r.cfg();
        assert_eq!(route(&c), Route::Spot(Network::Demo));
        assert!(c.perp.is_none() && c.paper.is_none());
        assert_eq!(param(&c, "coin").as_str(), Some("binance:BTCUSDT"));
    }

    /// eToro asks which market. Crypto: coins by name (BTCUSDT → BTC).
    /// Stocks: US tickers by default (as many as the example has coins),
    /// daily bars, and the hours answer sets the `.RTH` suffix whatever
    /// was typed. The demo account either way; no network asked.
    #[test]
    fn etoro_asks_which_market() {
        // venue, market, symbols, interval, history, 6 params
        let mut a = vec!["sma_trend", "etoro", "crypto", "", "", ""];
        a.extend([""; 7]);
        let r = drive("etoro-crypto", None, &a, false);
        let c = r.cfg();
        assert_eq!(c.whitelist[0].to_string(), "etoro:BTC");
        assert_eq!(param(&c, "coin").as_str(), Some("etoro:BTC"));
        assert_eq!(c.exchange.interval, Interval::H4, "the example's bar kept");
        assert_eq!(route(&c), Route::Etoro(Network::Demo));

        // venue, market, hours, symbols, interval, history, 6 params
        let mut a = vec!["sma_trend", "etoro", "", "", "", "", ""];
        a.extend([""; 7]);
        let r = drive("etoro-stocks", None, &a, false);
        let c = r.cfg();
        assert_eq!(c.whitelist[0].to_string(), "etoro:SPY.RTH");
        assert_eq!(param(&c, "coin").as_str(), Some("etoro:SPY.RTH"));
        assert_eq!(c.exchange.interval, Interval::D1);

        let mut a = vec!["top_k_momentum", "etoro", "stocks", "extended"];
        a.extend(["aapl.rth, msft, qqq", "", ""]);
        a.extend([""; 20]);
        let r = drive("etoro-ext", None, &a, false);
        let c = r.cfg();
        let ids: Vec<String> = c.whitelist.iter().map(ToString::to_string).collect();
        assert_eq!(ids, ["etoro:AAPL", "etoro:MSFT", "etoro:QQQ"]);
    }

    /// A bot name goes into the config (and so its books and kill switch);
    /// "none" leaves it out; a name unsafe in a file name is asked again.
    #[test]
    fn a_bot_name_is_asked_last() {
        let mut a = vec![""; 11]; // venue … history, 6 params
        a.extend(["BTC 1", "btc-1"]);
        let r = drive("named", Some("sma_trend"), &a, false);
        assert!(r.shown.contains("lowercase letters"), "{}", r.shown);
        let c = r.cfg();
        assert_eq!(c.name.as_deref(), Some("btc-1"));
        assert_eq!(
            preflight::kill_marker(&c),
            std::path::PathBuf::from("data/kill-switch/binance-testnet-btc-1.txt")
        );
        assert!(r.text().contains("name = \"btc-1\""), "{}", r.text());
        let unnamed = drive("unnamed", Some("sma_trend"), &[""; 12], false);
        assert!(unnamed.cfg().name.is_none(), "{}", unnamed.text());
    }

    /// A wrong answer is explained and asked again; nothing else moves.
    #[test]
    fn bad_answers_are_asked_again() {
        let a = [
            "nope",
            "99",
            "sma_trend", // strategy: unknown, out of range, ok
            "kraken",
            "", // venue
            "binance:BTCUSDT",
            "", // venue prefix refused
            "2x",
            "1h", // interval
            "live",
            "", // network: live is not offered
            "0",
            "", // history
            "",
            "abc",
            "50", // coin, sma_period
            "1%",
            "",
            "",
            "0.1.1",
            "",
            "", // band_pct, quote, fee, slippage
            "", // name
        ];
        let r = drive("bad", None, &a, false);
        r.result.as_ref().unwrap().as_ref().unwrap();
        let c = r.cfg();
        assert_eq!(param(&c, "sma_period").as_integer(), Some(50));
        assert_eq!(c.exchange.interval, Interval::H1);
        assert_eq!(route(&c), Route::Spot(Network::Testnet));
        for said in [
            "not a strategy",
            "1 to 29",
            "one of binance",
            "symbols only",
            "not an interval",
            "one of testnet, demo",
            "at least 1",
            "a whole number",
            "an exact decimal",
        ] {
            assert!(r.shown.contains(said), "{said}: {}", r.shown);
        }
    }

    /// A float typed into a money table is refused, not converted.
    #[test]
    fn floats_in_tables_are_refused() {
        let mut a = vec!["rebalance", "", "", "", "", ""];
        a.extend([
            r#"{ "binance:BTCUSDT" = 0.5 }"#,
            r#"{ "binance:BTCUSDT" = "0.4" }"#,
        ]);
        a.extend([""; 5]);
        let r = drive("float", None, &a, false);
        assert!(r.shown.contains("go in quotes"), "{}", r.shown);
        let c = r.cfg();
        let w = param(&c, "weights").as_table().unwrap();
        assert_eq!(
            w.get("binance:BTCUSDT").and_then(|v| v.as_str()),
            Some("0.4")
        );
    }

    /// Params the strategy refuses: shown, asked again with the answers as
    /// defaults; declining writes nothing.
    #[test]
    fn a_strategy_that_wont_build_is_never_written() {
        let mut a = vec!["sma_trend", "", "", "", "", "", "", "0", "", "", "", "", ""];
        a.push("y");
        a.extend(["", "", "", "", "", "", "20", "", "", "", "", ""]); // no strategy question the second time
        let r = drive("again", None, &a, false);
        assert!(r.shown.contains("FAIL  strategy sma_trend"), "{}", r.shown);
        assert_eq!(param(&r.cfg(), "sma_period").as_integer(), Some(20));

        let mut a = vec!["sma_trend", "", "", "", "", "", "", "0", "", "", "", "", ""];
        a.push("n");
        let r = drive("quit", None, &a, false);
        assert!(r.result.as_ref().unwrap().is_none());
        assert!(!r.out.exists());
    }

    /// Input ending mid-way writes nothing; an existing file is replaced
    /// only when the user says so.
    #[test]
    fn nothing_written_by_accident() {
        let r = drive("eof", Some("sma_trend"), &["", "", ""], false);
        assert!(
            r.result
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("input ended")
        );
        assert!(!r.out.exists());

        let mut a = vec![""; 12];
        a.push("");
        let r = drive("keep", Some("sma_trend"), &a, true);
        assert!(r.result.as_ref().unwrap().is_none());
        assert_eq!(r.text(), "mine");

        let mut a = vec![""; 12];
        a.push("y");
        let r = drive("replace", Some("sma_trend"), &a, true);
        assert!(r.result.as_ref().unwrap().is_some());
        assert!(r.text().contains("name = \"sma_trend\""));
    }

    /// funding_carry can't run, so it is neither listed nor taken.
    #[test]
    fn backtest_only_strategy_refused() {
        let r = drive("carry", Some("funding_carry"), &[], false);
        assert!(
            r.result
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("backtest only")
        );
    }
}
