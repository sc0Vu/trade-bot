//! `ichimoku`: one coin, long above the Ichimoku cloud, cash below it.
//!
//! Ichimoku Kinko Hyo (Hosoda): Tenkan = high-low midpoint of the last
//! `tenkan` bars, Kijun of the last `kijun`; the cloud at a bar spans the
//! two Senkou lines computed `displacement` bars earlier (span A = the
//! mean of Tenkan and Kijun, span B = midpoint of `senkou_b` bars), as
//! charts draw them
//! shifted forward ([`indicator::ichimoku`]): only past data. Flat, the
//! close above the cloud and Tenkan above Kijun → buy `quote_amount`; long
//! and the close below the cloud → sell everything; inside the cloud,
//! nothing. Needs `displacement + max(tenkan, kijun, senkou_b)` closed
//! bars; `history_len` must be at least that.
//!
//! What it bets: a trend confirmed twice (price clear of the cloud, the
//! fast midpoint over the slow), exiting only when price falls through the
//! whole cloud: few trades, late entries and exits, a wide cloud giving
//! back much of a move. The textbook 9 / 26 / 52 periods come from a
//! six-day trading week; on a 24/7 market they are just numbers. Expected
//! edge is unverified.

use super::long_flat;
use crate::indicator;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, Signal};
use serde::Deserialize;

const NAME: &str = "ichimoku";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IchimokuParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Bars in Tenkan (`9`). At least 1.
    tenkan: usize,
    /// Bars in Kijun (`26`). At least 1.
    kijun: usize,
    /// Bars in Senkou span B (`52`). At least 1.
    senkou_b: usize,
    /// How far back the cloud's spans are computed (`26`, Kijun's period,
    /// when unset).
    #[serde(default)]
    displacement: Option<usize>,
    /// Quote spent on each buy (`"1000"` USDT), as a quote-budget market
    /// order.
    quote_amount: Notional,
    /// Fee per trade assumed (`"0.001"`). Required so the config states the
    /// costs the backtest runs with; decisions don't use it.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed, in basis points (`"5"`).
    /// Required and unused, as `fee`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct Ichimoku {
    p: IchimokuParams,
}

impl Ichimoku {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: IchimokuParams = params.parse(NAME)?;
        if p.tenkan == 0 || p.kijun == 0 || p.senkou_b == 0 {
            return Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: "tenkan, kijun and senkou_b must be >= 1".to_owned(),
            });
        }
        long_flat::check_common(NAME, p.quote_amount, p.fee, p.slippage_bps)?;
        Ok(Box::new(Self { p }))
    }

    fn displacement(&self) -> usize {
        self.p.displacement.unwrap_or(self.p.kijun)
    }

    fn needed(&self) -> usize {
        self.displacement() + self.p.tenkan.max(self.p.kijun).max(self.p.senkou_b)
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: Ichimoku::build }
}

impl Strategy for Ichimoku {
    fn name(&self) -> &str {
        NAME
    }

    /// Decisions come from history and positions alone, both rebuilt on a
    /// restart: only the format version is saved.
    fn save_state(&self) -> Option<serde_json::Value> {
        super::stateless_save()
    }

    fn restore_state(
        &mut self,
        state: serde_json::Value,
        _ctx: &Context,
    ) -> Result<(), StrategyError> {
        super::stateless_restore(NAME, &state)
    }

    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let coin = &self.p.coin;
        if !long_flat::deciding(ctx, coin, c) {
            return Vec::new();
        }
        let Some(bars) = long_flat::newest(ctx, coin, self.needed()) else {
            return Vec::new();
        };
        let Some((tenkan, kijun, bottom, top)) = indicator::ichimoku(
            &bars,
            self.p.tenkan,
            self.p.kijun,
            self.p.senkou_b,
            self.displacement(),
        ) else {
            return Vec::new();
        };
        let close = c.close.get();
        long_flat::decide(
            ctx,
            coin,
            c,
            self.p.quote_amount,
            close > top && tenkan > kijun,
            close < bottom,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{OrderKind, Position, Qty, Side, Size};
    use rust_decimal_macros::dec;

    /// The indicator test's setting: tenkan 2, kijun 3, senkou_b 4,
    /// displacement 2 (6 bars needed).
    fn params() -> Params {
        Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               tenkan = 2
               kijun = 3
               senkou_b = 4
               displacement = 2
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap()
    }

    fn feed(s: &mut dyn Strategy, ctx: &mut Context, prices: &[&str]) -> Vec<Signal> {
        let mut out = Vec::new();
        for (i, p) in prices.iter().enumerate() {
            let c = candle(i as i64, p);
            driver::set_now(ctx, c.close_time);
            driver::push_candle(ctx, c.clone());
            out = s.on_candle(ctx, &c);
        }
        out
    }

    #[test]
    fn self_registers_and_checks_params() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params()).unwrap().name(), NAME);
        let zero = Params::from_toml(
            "coin = \"binance:BTCUSDT\"\ntenkan = 0\nkijun = 3\nsenkou_b = 4\n\
             quote_amount = \"1000\"\nfee = \"0.001\"\nslippage_bps = \"10\"",
        )
        .unwrap();
        assert!(build(NAME, &zero).is_err());
    }

    /// The indicator test's climb 10..15: close 15 above the cloud (11.5 ..
    /// 12.25), Tenkan 14.5 above Kijun 14 → buy at bar 5 (not before: 6
    /// bars needed).
    #[test]
    fn buys_above_the_cloud_with_tenkan_over_kijun() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], params(), 10);
        assert!(feed(s.as_mut(), &mut ctx, &["10", "11", "12", "13", "14"]).is_empty());
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], params(), 10);
        assert_eq!(
            feed(s.as_mut(), &mut ctx, &["10", "11", "12", "13", "14", "15"]),
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Buy,
                size: Size::Quote(Notional::new(dec!(1000))),
                kind: OrderKind::Market,
            }]
        );
    }

    /// Long through the climb, then closes 15, 14, 13, 12, 11, 10 after
    /// it: at the last bar the cloud spans come from two bars back (end at
    /// close 12): tenkan mid(13, 12) = 12.5, kijun mid(14..12) = 13 → span
    /// A 12.75; span B mid(15..12) = 13.5. Close 10 is below 12.75: sell.
    #[test]
    fn sells_below_the_cloud() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], params(), 20);
        driver::set_position(
            &mut ctx,
            Position::from_parts(
                btcusdt().id.clone(),
                Qty::new(dec!(5)),
                Notional::new(dec!(500)),
                Notional::ZERO,
            )
            .unwrap(),
        );
        let got = feed(
            s.as_mut(),
            &mut ctx,
            &[
                "10", "11", "12", "13", "14", "15", "14", "13", "12", "11", "10",
            ],
        );
        assert_eq!(
            got,
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Sell,
                size: Size::Base(Qty::new(dec!(5))),
                kind: OrderKind::Market,
            }]
        );
    }

    #[test]
    fn saves_only_its_version() {
        let mut s = build(NAME, &params()).unwrap();
        let state = s.save_state().unwrap();
        assert_eq!(state, serde_json::json!({ "v": 1 }));
        let ctx = driver::new(vec![btcusdt()], params(), 10);
        assert!(s.restore_state(state, &ctx).is_ok());
    }
}
