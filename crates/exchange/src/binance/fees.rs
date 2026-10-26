//! Valuing fees Binance charges in a third asset (BNB, when the account
//! pays fees with BNB).
//!
//! Position accounting needs every fee in the instrument's quote asset
//! (`Fee::Other::value`). `myTrades` reports only the BNB amount, not the
//! rate Binance converted at, so the fee is valued at the `close` of the
//! one-minute `{asset}{quote}` kline (e.g. BNBUSDT) that contains the
//! trade. Binance's REST klines include the minute still in progress, so a
//! fee valued during its minute (the usual live case) gets the latest price
//! at that moment, and one valued later (a restart's reconcile) the
//! minute's final close. Either way an approximation: at most a minute of
//! BNB price movement on a fee of a fraction of a percent, and not
//! reproducible to the digit. The multiplication itself is that pair's
//! `Instrument::notional` (invariant 4).
//!
//! Binance publishes a minute's kline over REST a few seconds after the
//! minute opens (about 5 s measured on demo trading, 2026-10-23), so a
//! trade early in a minute is often valued before its candle exists. An
//! empty reply is retried every [`CANDLE_RETRY`] for up to
//! [`CANDLE_ATTEMPTS`] tries; a later candle in its place, or none after
//! that, is an error, never a guessed price.

use super::public::{parse_exchange_info, parse_klines};
use super::signed::{Method, SignedClient};
use crate::ExchangeError;
use bot_core::{Fee, Fill, FillSpec, Instrument, InstrumentId, Interval, Symbol};
use std::collections::HashMap;

const MINUTE_MS: i64 = 60_000;
/// Pause between asks for a fee candle not yet published.
const CANDLE_RETRY: std::time::Duration = std::time::Duration::from_secs(2);
/// Asks for a fee candle before giving up: about a minute, well past the
/// measured publishing delay.
const CANDLE_ATTEMPTS: u32 = 30;

/// Fills in `value` on third-asset fees. Caches each fee pair's reference
/// data for the life of the run.
#[derive(Default)]
pub(super) struct FeeValuer {
    pairs: HashMap<InstrumentId, Instrument>,
}

impl FeeValuer {
    /// `fill` unchanged unless its fee is `Fee::Other` without a value; then
    /// the same fill with the fee valued in `inst`'s quote asset.
    pub async fn complete(
        &mut self,
        client: &SignedClient,
        inst: &Instrument,
        fill: Fill,
    ) -> Result<Fill, ExchangeError> {
        let Fee::Other {
            asset,
            amount,
            value: None,
        } = &fill.fee
        else {
            return Ok(fill);
        };
        let symbol = Symbol::new(format!("{asset}{}", inst.quote))
            .map_err(|e| ExchangeError::Decode(format!("fee pair: {e}")))?;
        let pair_id = InstrumentId::new(inst.id.venue().clone(), symbol.clone());
        if !self.pairs.contains_key(&pair_id) {
            let info = client
                .public(
                    Method::Get,
                    "/api/v3/exchangeInfo",
                    &[("symbol", symbol.to_string())],
                )
                .await
                .map_err(|e| {
                    ExchangeError::Decode(format!(
                        "can't value a fee in {asset}: no {symbol} pair to price it ({e})"
                    ))
                })?;
            let pair = parse_exchange_info(&info, inst.id.venue())?;
            self.pairs.insert(pair_id.clone(), pair);
        }
        let pair = &self.pairs[&pair_id];
        let minute = fill.ts.as_millis().div_euclid(MINUTE_MS) * MINUTE_MS;
        let mut attempt = 1;
        let candle = loop {
            let page = client
                .public(
                    Method::Get,
                    "/api/v3/klines",
                    &[
                        ("symbol", symbol.to_string()),
                        ("interval", Interval::M1.code().to_owned()),
                        ("startTime", minute.to_string()),
                        ("limit", "1".to_owned()),
                    ],
                )
                .await?;
            let first = parse_klines(&page, &pair_id, Interval::M1)?
                .into_iter()
                .next();
            match first {
                Some(c) if c.open_time.as_millis() == minute => break c,
                // Empty: the minute isn't published yet.
                None if attempt < CANDLE_ATTEMPTS => {
                    attempt += 1;
                    tokio::time::sleep(CANDLE_RETRY).await;
                }
                // A later minute in its place: ours will never come.
                _ => {
                    return Err(ExchangeError::Decode(format!(
                        "no {symbol} candle at the fee's minute"
                    )));
                }
            }
        };
        let value = pair.notional(*amount, candle.close);
        let spec = FillSpec {
            fee: Fee::Other {
                asset: asset.clone(),
                amount: *amount,
                value: Some(value),
            },
            ..fill.into()
        };
        Fill::new(spec).map_err(|e| ExchangeError::Decode(e.to_string()))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::binance::signed::Credentials;
    use crate::binance::signed::fake::{FakeTransport, FixedClock, param};
    use bot_core::{Asset, Liquidity, Notional, OrderId, Price, Qty, Side, Timestamp};
    use rust_decimal_macros::dec;

    /// BNBUSDT reference data, as exchangeInfo returns it.
    pub const BNBUSDT_INFO: &str = r#"{"symbols":[{"symbol":"BNBUSDT","status":"TRADING","baseAsset":"BNB","quoteAsset":"USDT","filters":[
        {"filterType":"PRICE_FILTER","tickSize":"0.01000000"},
        {"filterType":"LOT_SIZE","stepSize":"0.00100000","minQty":"0.00100000"},
        {"filterType":"NOTIONAL","minNotional":"5.00000000"}]}]}"#;

    /// One BNBUSDT 1m kline opening at `open_ms` and closing at `close`.
    pub fn bnb_kline(open_ms: i64, close: &str) -> String {
        format!(
            r#"[[{open_ms},"599","10000","1","{close}","10",{},"6000",5,"5","3000","0"]]"#,
            open_ms + 59_999
        )
    }

    fn btcusdt() -> Instrument {
        Instrument::new(bot_core::InstrumentSpec {
            id: "binance:BTCUSDT".parse().unwrap(),
            class: bot_core::AssetClass::Spot,
            base: Asset::new("BTC").unwrap(),
            quote: Asset::new("USDT").unwrap(),
            tick_size: Price::new(dec!(0.01)),
            lot_size: Qty::new(dec!(0.00001)),
            min_qty: Qty::new(dec!(0.00001)),
            min_notional: Notional::new(dec!(5)),
        })
        .unwrap()
    }

    fn fill(fee: Fee, ts: i64) -> Fill {
        Fill::new(FillSpec {
            order_id: OrderId::new("o-1").unwrap(),
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            qty: Qty::new(dec!(1)),
            price: Price::new(dec!(100)),
            fee,
            liquidity: Liquidity::Taker,
            ts: Timestamp::from_millis(ts),
        })
        .unwrap()
    }

    fn client(t: &FakeTransport) -> SignedClient {
        SignedClient::new(
            "x",
            Box::new(t.clone()),
            Credentials::new("k", "s"),
            Box::new(FixedClock(0)),
        )
    }

    fn bnb(amount: rust_decimal::Decimal) -> Fee {
        Fee::Other {
            asset: Asset::new("BNB").unwrap(),
            amount: Qty::new(amount),
            value: None,
        }
    }

    /// 0.0001 BNB at a BNBUSDT close of 600 is 0.06 USDT.
    #[tokio::test]
    async fn bnb_fee_is_valued_at_the_trades_minute() {
        let t = FakeTransport::default();
        t.reply(200, BNBUSDT_INFO);
        t.reply(200, &bnb_kline(120_000, "600.00"));
        let mut v = FeeValuer::default();
        let got = v
            .complete(&client(&t), &btcusdt(), fill(bnb(dec!(0.0001)), 150_000))
            .await
            .unwrap();
        assert_eq!(
            got.fee,
            Fee::Other {
                asset: Asset::new("BNB").unwrap(),
                amount: Qty::new(dec!(0.0001)),
                value: Some(Notional::new(dec!(0.06))),
            }
        );
        let sent = t.sent();
        assert_eq!(param(&sent[1].query, "symbol"), Some("BNBUSDT"));
        assert_eq!(param(&sent[1].query, "startTime"), Some("120000"));
        assert!(sent.iter().all(|r| r.api_key.is_none()), "public endpoints");

        // The pair's reference data is fetched once per run.
        t.reply(200, &bnb_kline(180_000, "610.00"));
        let again = v
            .complete(&client(&t), &btcusdt(), fill(bnb(dec!(0.0001)), 200_000))
            .await
            .unwrap();
        assert!(
            matches!(again.fee, Fee::Other { value: Some(v), .. } if v == Notional::new(dec!(0.061)))
        );
        assert_eq!(t.sent().len(), 3);
    }

    #[tokio::test]
    async fn base_and_quote_fees_need_no_rate() {
        let t = FakeTransport::default();
        let f = fill(Fee::Quote(Notional::new(dec!(0.1))), 0);
        let got = FeeValuer::default()
            .complete(&client(&t), &btcusdt(), f.clone())
            .await
            .unwrap();
        assert_eq!(got, f);
        assert!(t.sent().is_empty());
    }

    /// The trade's minute opened 1 s ago and isn't published yet: asked
    /// again 2 s later, still nothing; on the third ask it's there.
    #[tokio::test(start_paused = true)]
    async fn unpublished_candle_is_waited_for() {
        let t = FakeTransport::default();
        t.reply(200, BNBUSDT_INFO);
        t.reply(200, "[]");
        t.reply(200, "[]");
        t.reply(200, &bnb_kline(120_000, "600.00"));
        let start = tokio::time::Instant::now();
        let got = FeeValuer::default()
            .complete(&client(&t), &btcusdt(), fill(bnb(dec!(0.0001)), 121_000))
            .await
            .unwrap();
        assert!(
            matches!(got.fee, Fee::Other { value: Some(v), .. } if v == Notional::new(dec!(0.06)))
        );
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(4));
        assert_eq!(t.sent().len(), 4);
    }

    /// Never published: 30 asks over 58 s, then an error, not a guess.
    #[tokio::test(start_paused = true)]
    async fn missing_candle_is_an_error_not_a_guess() {
        let t = FakeTransport::default();
        t.reply(200, BNBUSDT_INFO);
        for _ in 0..30 {
            t.reply(200, "[]");
        }
        let start = tokio::time::Instant::now();
        let err = FeeValuer::default()
            .complete(&client(&t), &btcusdt(), fill(bnb(dec!(0.0001)), 150_000))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("BNBUSDT"), "{err}");
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(58));
        assert_eq!(t.sent().len(), 31);
    }

    /// A later minute where the trade's should be: it will never come, so
    /// no waiting.
    #[tokio::test(start_paused = true)]
    async fn later_candle_is_an_error_at_once() {
        let t = FakeTransport::default();
        t.reply(200, BNBUSDT_INFO);
        t.reply(200, &bnb_kline(180_000, "600.00"));
        let err = FeeValuer::default()
            .complete(&client(&t), &btcusdt(), fill(bnb(dec!(0.0001)), 150_000))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("BNBUSDT"), "{err}");
        assert_eq!(t.sent().len(), 2);
    }
}
