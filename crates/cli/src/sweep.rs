//! `trade-bot sweep`: walk-forward test of a parameter grid.
//!
//! A backtest tuned on the period it is scored on measures the tuning, not
//! the strategy: with enough combinations one always fits the past. So the
//! range `--from..--to` is cut into rolling windows: each *train* window
//! (`--train-days`) runs every combination and picks the best by
//! `--objective`; the *test* window right after it (`--test-days`, never
//! seen by the pick) runs that pick. Windows advance by the test length, so
//! test windows tile the range without overlap. Only test results count.
//!
//! Two reference points come with them:
//! - the *median* combination's test return in each window: if the pick
//!   doesn't beat it, choosing parameters added nothing over choosing at
//!   random;
//! - the pick's train score beside its test result: a large drop is the
//!   overfitting showing.
//!
//! Each window is its own backtest from `--initial-cash`, flat, warmed up on
//! the bars before it (as `backtest`): a holding at a window's end is
//! marked at the last close, not sold, and the next window starts flat
//! again (its entry costs are paid). The chained figure Π(1 + rᵢ) − 1 is
//! therefore an approximation of one continuous run, stated as such.
//!
//! The grid: the `--params` file with a `[sweep]` table whose keys are
//! top-level params and whose values are arrays; every combination of them
//! is tried over the rest of the file. A combination the strategy refuses
//! (`fast_period >= slow_period`) is skipped and counted.

use crate::{BacktestArgs, backtest_report, date};
use anyhow::Context;
use bot_core::{Decimal, Timestamp};
use clap::Args;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use strategy::Params;

const DAY_MS: i64 = 86_400_000;
/// Most combinations one sweep may run: each one costs a backtest per
/// window, twice, and more of them make a lucky fit likelier.
pub const DEFAULT_MAX_COMBOS: usize = 200;

#[derive(Args, Clone)]
pub struct SweepArgs {
    #[command(flatten)]
    pub backtest: BacktestArgs,
    /// Days each combination is scored on before a pick.
    #[arg(long)]
    pub train_days: u32,
    /// Days the pick is then tested on; windows advance by this much.
    #[arg(long)]
    pub test_days: u32,
    /// What "best" means on a train window.
    #[arg(long, value_enum, default_value = "sharpe")]
    pub objective: Objective,
    /// Refuse a grid with more combinations than this.
    #[arg(long, default_value_t = DEFAULT_MAX_COMBOS)]
    pub max_combos: usize,
}

/// How a train window ranks combinations. Higher is better; a run with no
/// score (Sharpe of a run that never moved) ranks below every scored one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Objective {
    /// Final equity / initial − 1. Favors whatever took the most risk.
    Return,
    /// Annualized Sharpe of per-bar equity returns (the report's).
    Sharpe,
    /// Return / max drawdown, drawdown floored at 1% (a run that never
    /// fell would otherwise divide by zero).
    Calmar,
}

/// The grid expanded: one params table per combination, and its label
/// (`k=v` of the swept keys only).
#[derive(Debug, PartialEq)]
pub struct Grid {
    pub combos: Vec<toml::Table>,
    pub labels: Vec<String>,
}

/// Expands `table`'s `[sweep]` section over the rest of it. Keys combine in
/// the table's order, first key slowest. Refuses a missing or empty
/// section, a value that isn't a non-empty array, a swept key that is a
/// table, and more than `max` combinations.
pub fn expand(mut table: toml::Table, max: usize) -> anyhow::Result<Grid> {
    let sweep = match table.remove("sweep") {
        Some(toml::Value::Table(t)) if !t.is_empty() => t,
        Some(_) => anyhow::bail!("[sweep] must be a table of param = [values]"),
        None => anyhow::bail!("the params file needs a [sweep] table: param = [values, ...]"),
    };
    let mut axes = Vec::new();
    let mut count: usize = 1;
    for (k, v) in sweep {
        let toml::Value::Array(values) = v else {
            anyhow::bail!("[sweep] {k}: want an array of values");
        };
        anyhow::ensure!(!values.is_empty(), "[sweep] {k}: no values");
        anyhow::ensure!(
            values.iter().all(|v| !v.is_table()),
            "[sweep] {k}: values must be plain (sweep top-level params only)"
        );
        count = count.saturating_mul(values.len());
        axes.push((k, values));
    }
    anyhow::ensure!(
        count <= max,
        "{count} combinations; at most {max} (--max-combos): every extra one costs a \
         backtest per window and makes a lucky fit likelier"
    );
    let mut combos = vec![(table, Vec::<String>::new())];
    for (k, values) in &axes {
        combos = combos
            .into_iter()
            .flat_map(|(t, label)| {
                values.iter().map(move |v| {
                    let mut t = t.clone();
                    t.insert(k.clone(), v.clone());
                    let mut label = label.clone();
                    label.push(format!("{k}={v}"));
                    (t, label)
                })
            })
            .collect();
    }
    let (combos, labels) = combos.into_iter().map(|(t, l)| (t, l.join(" "))).unzip();
    Ok(Grid { combos, labels })
}

/// One train window and the test window after it, `[start, end)` each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub train: (Timestamp, Timestamp),
    pub test: (Timestamp, Timestamp),
}

/// Rolling windows over `[from, to)`: train `train_days`, then test
/// `test_days`, advancing by `test_days`. A last test window that would end
/// after `to` is dropped (a shorter one would score a different horizon).
pub fn windows(
    from: Timestamp,
    to: Timestamp,
    train_days: u32,
    test_days: u32,
) -> anyhow::Result<Vec<Window>> {
    anyhow::ensure!(
        train_days > 0 && test_days > 0,
        "--train-days and --test-days must be > 0"
    );
    let (train, test) = (
        i64::from(train_days) * DAY_MS,
        i64::from(test_days) * DAY_MS,
    );
    let mut out = Vec::new();
    let mut start = from.as_millis();
    while start + train + test <= to.as_millis() {
        let split = start + train;
        out.push(Window {
            train: (Timestamp::from_millis(start), Timestamp::from_millis(split)),
            test: (
                Timestamp::from_millis(split),
                Timestamp::from_millis(split + test),
            ),
        });
        start += test;
    }
    anyhow::ensure!(
        !out.is_empty(),
        "--from..--to is shorter than one train + test window ({train_days} + {test_days} days)"
    );
    Ok(out)
}

/// One finished backtest, reduced to what the sweep compares.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outcome {
    /// Final equity / initial − 1.
    pub ret: Decimal,
    pub max_drawdown: Decimal,
    pub sharpe: Option<Decimal>,
}

impl Outcome {
    pub fn of(r: &backtest::Report) -> Self {
        let initial = r.initial_cash.get();
        Self {
            ret: if initial.is_zero() {
                Decimal::ZERO
            } else {
                r.final_equity.get() / initial - Decimal::ONE
            },
            max_drawdown: r.max_drawdown,
            sharpe: r.sharpe,
        }
    }

    /// The train-window score under `o`.
    pub fn score(&self, o: Objective) -> Option<Decimal> {
        match o {
            Objective::Return => Some(self.ret),
            Objective::Sharpe => self.sharpe,
            Objective::Calmar => Some(self.ret / self.max_drawdown.max(Decimal::new(1, 2))),
        }
    }
}

/// Index of the best score: highest, the earliest on a tie (grid order,
/// so a rerun picks the same), `None` scores never picked. `None` when
/// nothing scored.
pub fn pick(scores: &[Option<Decimal>]) -> Option<usize> {
    let mut best: Option<(usize, Decimal)> = None;
    for (i, s) in scores.iter().enumerate() {
        if let Some(s) = *s
            && best.is_none_or(|(_, b)| s > b)
        {
            best = Some((i, s));
        }
    }
    best.map(|(i, _)| i)
}

/// Median of `xs` (mean of the middle two when even); `None` if empty.
pub fn median(xs: &[Decimal]) -> Option<Decimal> {
    let mut v = xs.to_vec();
    v.sort();
    let n = v.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(v[n / 2]),
        _ => Some((v[n / 2 - 1] + v[n / 2]) / Decimal::TWO),
    }
}

/// Π(1 + rᵢ) − 1: window returns chained as if reinvested.
pub fn chain(returns: impl IntoIterator<Item = Decimal>) -> Decimal {
    returns
        .into_iter()
        .fold(Decimal::ONE, |acc, r| acc * (Decimal::ONE + r))
        - Decimal::ONE
}

/// One window's results: every combination on train and on test (`None` =
/// refused or failed), and the pick.
#[derive(Debug)]
pub struct WindowResult {
    pub window: Window,
    pub train: Vec<Option<Outcome>>,
    pub test: Vec<Option<Outcome>>,
    pub picked: Option<usize>,
}

impl WindowResult {
    /// The pick's test outcome; `None` when nothing was picked (counted as
    /// staying in cash, 0%) or its test run failed.
    pub fn picked_test(&self) -> Option<Outcome> {
        self.picked.and_then(|i| self.test[i])
    }

    /// Median test return over the combinations that ran.
    pub fn median_test(&self) -> Option<Decimal> {
        median(
            &self
                .test
                .iter()
                .flatten()
                .map(|o| o.ret)
                .collect::<Vec<_>>(),
        )
    }
}

/// The whole sweep's results.
#[derive(Debug)]
pub struct Sweep {
    pub labels: Vec<String>,
    pub objective: Objective,
    pub windows: Vec<WindowResult>,
    /// Distinct reasons combinations were refused, and data notes.
    pub notes: Vec<String>,
}

/// Runs the sweep `a` describes: every combination on every train window
/// and every test window. Fails if no combination runs at all (a data
/// error then, not a bad combination).
pub async fn run(a: &SweepArgs) -> anyhow::Result<Sweep> {
    let path = a
        .backtest
        .params
        .as_ref()
        .context("sweep needs --params: the params file with a [sweep] table")?;
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let table: toml::Table =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let grid = expand(table, a.max_combos)?;
    let windows = windows(a.backtest.from, a.backtest.to, a.train_days, a.test_days)?;
    let mut notes = BTreeSet::new();
    let mut last_error = None;
    let total = windows.len() * grid.combos.len() * 2;
    let mut done = 0;
    let mut results = Vec::new();
    for w in &windows {
        let mut outcomes = [Vec::new(), Vec::new()];
        for (span, out) in [w.train, w.test].into_iter().zip(outcomes.iter_mut()) {
            for combo in &grid.combos {
                let mut b = a.backtest.clone();
                (b.from, b.to) = span;
                done += 1;
                eprint!("\rsweep: {done}/{total} backtests");
                match backtest_report(&b, Params::new(combo.clone())).await {
                    Ok((report, n)) => {
                        notes.extend(n);
                        out.push(Some(Outcome::of(&report)));
                    }
                    Err(e) => {
                        notes.insert(format!("refused: {e:#}"));
                        last_error = Some(e);
                        out.push(None);
                    }
                }
            }
        }
        let [train, test] = outcomes;
        let scores: Vec<_> = train
            .iter()
            .map(|o| o.and_then(|o| o.score(a.objective)))
            .collect();
        results.push(WindowResult {
            window: *w,
            picked: pick(&scores),
            train,
            test,
        });
    }
    eprintln!();
    let ran = results
        .iter()
        .any(|r| r.train.iter().chain(&r.test).any(Option::is_some));
    if !ran && let Some(e) = last_error {
        return Err(e.context("no combination ran"));
    }
    Ok(Sweep {
        labels: grid.labels,
        objective: a.objective,
        windows: results,
        notes: notes.into_iter().collect(),
    })
}

/// `YYYY-MM-DD` of a window bound (always midnight UTC).
fn day(t: Timestamp) -> String {
    date::format(t).chars().take(10).collect()
}

fn pct(x: Decimal) -> String {
    format!("{:+.2}%", x * Decimal::ONE_HUNDRED)
}

impl std::fmt::Display for Sweep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "{:<3} {:<22} {:<22} {:>11} {:>10} {:>9} {:>10}  pick",
            "#", "train", "test", "train score", "test ret", "test dd", "median ret"
        );
        let span = |(a, b): (Timestamp, Timestamp)| format!("{}..{}", day(a), day(b));
        let mut picked_rets = Vec::new();
        let mut median_rets = Vec::new();
        let mut beat = 0;
        let mut compared = 0;
        let mut train_rets = Vec::new();
        let mut chosen = std::collections::BTreeMap::<&str, usize>::new();
        for (n, w) in self.windows.iter().enumerate() {
            let score = w
                .picked
                .and_then(|i| w.train[i])
                .and_then(|o| o.score(self.objective));
            let test = w.picked_test();
            let med = w.median_test();
            // Nothing picked = stayed in cash.
            let ret = test.map_or(Decimal::ZERO, |o| o.ret);
            picked_rets.push(ret);
            if let Some(m) = med {
                median_rets.push(m);
                compared += 1;
                if ret > m {
                    beat += 1;
                }
            }
            if let Some(o) = w.picked.and_then(|i| w.train[i]) {
                train_rets.push(o.ret);
            }
            let label = w
                .picked
                .map_or("(nothing scored: cash)", |i| &self.labels[i]);
            *chosen.entry(label).or_default() += 1;
            let _ = writeln!(
                s,
                "{:<3} {:<22} {:<22} {:>11} {:>10} {:>9} {:>10}  {}",
                n + 1,
                span(w.window.train),
                span(w.window.test),
                score.map_or("–".into(), |x| format!("{x:.3}")),
                test.map_or("–".into(), |o| pct(o.ret)),
                test.map_or("–".into(), |o| pct(-o.max_drawdown)),
                med.map_or("–".into(), pct),
                label
            );
        }
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "out of sample, chained:  pick {}   median combination {}",
            pct(chain(picked_rets.iter().copied())),
            pct(chain(median_rets.iter().copied()))
        );
        let _ = writeln!(
            s,
            "pick beat the median in {beat} of {compared} test windows"
        );
        if let (Some(tr), Some(te)) = (mean(&train_rets), mean(&picked_rets)) {
            let _ = writeln!(
                s,
                "pick's mean return: train {}  →  test {} (the gap is the fit to the past)",
                pct(tr),
                pct(te)
            );
        }
        let _ = writeln!(s, "picks:");
        let mut chosen: Vec<_> = chosen.into_iter().collect();
        chosen.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        for (label, k) in chosen {
            let _ = writeln!(s, "  {k:>3} × {label}");
        }
        f.write_str(&s)
    }
}

fn mean(xs: &[Decimal]) -> Option<Decimal> {
    (!xs.is_empty()).then(|| xs.iter().sum::<Decimal>() / Decimal::from(xs.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn table(s: &str) -> toml::Table {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn grid_is_every_combination_first_key_slowest() {
        let g = expand(
            table(
                r#"coin = "BTCUSDT"
                   [sweep]
                   a = [1, 2]
                   b = ["x", "y", "z"]"#,
            ),
            200,
        )
        .unwrap();
        assert_eq!(g.combos.len(), 6);
        assert_eq!(
            g.labels,
            [
                r#"a=1 b="x""#,
                r#"a=1 b="y""#,
                r#"a=1 b="z""#,
                r#"a=2 b="x""#,
                r#"a=2 b="y""#,
                r#"a=2 b="z""#
            ]
        );
        // Base params kept, the swept ones set, [sweep] gone.
        let last = &g.combos[5];
        assert_eq!(last["coin"].as_str(), Some("BTCUSDT"));
        assert_eq!(last["a"].as_integer(), Some(2));
        assert_eq!(last["b"].as_str(), Some("z"));
        assert!(!last.contains_key("sweep"));
    }

    #[test]
    fn a_bad_grid_is_refused() {
        assert!(expand(table("a = 1"), 200).is_err(), "no [sweep]");
        assert!(expand(table("[sweep]"), 200).is_err(), "empty");
        assert!(
            expand(table("[sweep]\na = 1"), 200).is_err(),
            "not an array"
        );
        assert!(expand(table("[sweep]\na = []"), 200).is_err(), "no values");
        assert!(
            expand(table("[sweep]\na = [{x = 1}]"), 200).is_err(),
            "nested"
        );
        // 3 × 3 = 9 > 8.
        let e = expand(table("[sweep]\na = [1, 2, 3]\nb = [1, 2, 3]"), 8).unwrap_err();
        assert!(e.to_string().contains("9 combinations"), "{e}");
    }

    #[test]
    fn windows_roll_by_the_test_length_and_drop_a_short_tail() {
        let d = |n: i64| Timestamp::from_millis(n * DAY_MS);
        // 10 days, train 4, test 2: tests at 4-6, 6-8, 8-10; a fourth would end at 12.
        let w = windows(d(0), d(10), 4, 2).unwrap();
        assert_eq!(
            w,
            [
                Window {
                    train: (d(0), d(4)),
                    test: (d(4), d(6))
                },
                Window {
                    train: (d(2), d(6)),
                    test: (d(6), d(8))
                },
                Window {
                    train: (d(4), d(8)),
                    test: (d(8), d(10))
                },
            ]
        );
        // 11 days: the same three, day 10-11 too short for a test window.
        assert_eq!(windows(d(0), d(11), 4, 2).unwrap(), w);
        assert!(
            windows(d(0), d(5), 4, 2).is_err(),
            "shorter than one window"
        );
        assert!(windows(d(0), d(10), 0, 2).is_err());
    }

    #[test]
    fn pick_is_the_highest_earliest_on_a_tie_never_unscored() {
        assert_eq!(
            pick(&[Some(dec!(1)), Some(dec!(3)), Some(dec!(3))]),
            Some(1)
        );
        assert_eq!(pick(&[None, Some(dec!(-2)), None]), Some(1));
        assert_eq!(pick(&[None, None]), None);
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn scores_median_and_chain() {
        let o = Outcome {
            ret: dec!(0.10),
            max_drawdown: dec!(0.05),
            sharpe: None,
        };
        assert_eq!(o.score(Objective::Return), Some(dec!(0.10)));
        assert_eq!(o.score(Objective::Sharpe), None);
        assert_eq!(o.score(Objective::Calmar), Some(dec!(2)));
        // Drawdown below 1% counts as 1%.
        let flat = Outcome {
            max_drawdown: dec!(0),
            ..o
        };
        assert_eq!(flat.score(Objective::Calmar), Some(dec!(10)));
        assert_eq!(median(&[dec!(3), dec!(1), dec!(2)]), Some(dec!(2)));
        assert_eq!(
            median(&[dec!(4), dec!(1), dec!(2), dec!(3)]),
            Some(dec!(2.5))
        );
        assert_eq!(median(&[]), None);
        // +10% then −10%: 1.1 × 0.9 = 0.99.
        assert_eq!(chain([dec!(0.1), dec!(-0.1)]), dec!(-0.01));
        assert_eq!(chain([]), dec!(0));
    }

    /// End to end on a cached fixture: three windows, and each window's
    /// pick and test result are exactly what plain backtests of the same
    /// combinations on the same spans give (the sweep only composes them).
    #[tokio::test]
    async fn the_sweep_composes_plain_backtests() {
        use crate::{MarginArg, Venue};
        use bot_core::{
            Asset, AssetClass, Candle, CandleSpec, Instrument, InstrumentSpec, Interval, Notional,
            Price, Qty,
        };
        let dir = std::env::temp_dir().join(format!("sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = backtest::DataDir::new(dir.clone());
        let id: bot_core::InstrumentId = "binance:BTCUSDT".parse().unwrap();
        data.write_instrument(
            &Instrument::new(InstrumentSpec {
                id: id.clone(),
                class: AssetClass::Spot,
                base: Asset::new("BTC").unwrap(),
                quote: Asset::new("USDT").unwrap(),
                tick_size: Price::new(dec!(0.01)),
                lot_size: Qty::new(dec!(0.00001)),
                min_qty: Qty::new(dec!(0.00001)),
                min_notional: Notional::new(dec!(5)),
                max_open_orders: None,
                max_open_stops: None,
                price_band: None,
            })
            .unwrap(),
        )
        .unwrap();
        // 11 days of 1h bars: a triangle wave (period 40 bars, ±20 around 100).
        let hour = 3_600_000;
        let closes: Vec<Decimal> = (0..=264)
            .map(|i: i64| {
                let k = i % 40;
                dec!(80) + Decimal::from(if k < 20 { k * 2 } else { (40 - k) * 2 })
            })
            .collect();
        let bars: Vec<Candle> = (1..closes.len())
            .map(|i| {
                let (o, c) = (closes[i - 1], closes[i]);
                Candle::new(CandleSpec {
                    instrument: id.clone(),
                    interval: Interval::H1,
                    open_time: Timestamp::from_millis(i as i64 * hour),
                    close_time: Timestamp::from_millis(i as i64 * hour + hour - 1),
                    open: Price::new(o),
                    high: Price::new(o.max(c) + dec!(0.5)),
                    low: Price::new(o.min(c) - dec!(0.5)),
                    close: Price::new(c),
                    volume: Qty::new(dec!(1)),
                    quote_volume: Notional::new(c),
                })
                .unwrap()
            })
            .collect();
        data.write_klines(&id, Interval::H1, &bars).unwrap();
        let params = dir.join("grid.toml");
        std::fs::write(
            &params,
            r#"coin = "BTCUSDT"
               band_pct = "0"
               quote_amount = "100"
               fee = "0.001"
               slippage_bps = "5"
               [sweep]
               sma_period = [3, 8, 15]"#,
        )
        .unwrap();
        let day = |n: i64| Timestamp::from_millis(n * DAY_MS);
        let a = SweepArgs {
            backtest: BacktestArgs {
                strategy: "sma_trend".into(),
                venue: Venue::Binance,
                params: Some(params),
                symbol: vec![bot_core::Symbol::new("BTCUSDT").unwrap()],
                interval: Interval::H1,
                from: day(1),
                to: day(10),
                initial_cash: dec!(1000),
                fee: dec!(0.001),
                slippage_bps: dec!(5),
                delistings: None,
                max_gap_hours: None,
                earn_ratio: None,
                earn_apr: None,
                delist_exit_hours: 24,
                kill_drawdown: None,
                perp_symbol: Vec::new(),
                perp_cash: None,
                perp_fee: dec!(0.0005),
                history_len: 20,
                data_dir: dir.clone(),
                leverage: None,
                margin: MarginArg::Isolated,
                cfd: false,
                clearance: None,
            },
            train_days: 3,
            test_days: 2,
            objective: Objective::Return,
            max_combos: 10,
        };
        let s = run(&a).await.unwrap();
        // Days 1..10, train 3, test 2: tests 4-6, 6-8, 8-10.
        assert_eq!(s.windows.len(), 3);
        assert_eq!(s.windows[2].window.test, (day(8), day(10)));
        let grid = expand(
            toml::from_str(&std::fs::read_to_string(a.backtest.params.as_ref().unwrap()).unwrap())
                .unwrap(),
            10,
        )
        .unwrap();
        for w in &s.windows {
            let mut direct = Vec::new();
            for span in [w.window.train, w.window.test] {
                let mut row = Vec::new();
                for combo in &grid.combos {
                    let mut b = a.backtest.clone();
                    (b.from, b.to) = span;
                    let (r, _) = backtest_report(&b, Params::new(combo.clone()))
                        .await
                        .unwrap();
                    row.push(Some(Outcome::of(&r)));
                }
                direct.push(row);
            }
            assert_eq!(w.train, direct[0]);
            assert_eq!(w.test, direct[1]);
            let best = direct[0].iter().map(|o| o.unwrap().ret).max().unwrap();
            let first_best = direct[0].iter().position(|o| o.unwrap().ret == best);
            assert_eq!(w.picked, first_best);
        }
        // The strategy traded: not a vacuous comparison of flat runs.
        assert!(
            s.windows
                .iter()
                .flat_map(|w| &w.train)
                .any(|o| o.unwrap().ret != Decimal::ZERO)
        );
        let text = s.to_string();
        assert!(text.contains("pick beat the median in"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
