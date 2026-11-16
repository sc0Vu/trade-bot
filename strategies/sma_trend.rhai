// sma_trend as a Rhai script: the same decisions as the built-in
// `sma_trend` (crates/strategy/src/builtin/sma_trend.rs), so a backtest of
// either gives the same fills. A worked example of the script API; see
// crates/strategy/src/script/rhai.rs for the reference.
//
// Long while the close is above the mean of the last `sma_period` closes
// (current bar included) by more than `band_pct`; cash below it by more
// than `band_pct`. Buys spend `quote_amount` (quote budget, market);
// sells sell the whole lot-rounded holding.
//
// Params (strategy.params, beside `script`):
//   coin = "binance:BTCUSDT"   sma_period = 20   quote_amount = "1000"
//   band_pct = "0.01" (optional)   fee = "0.001"   slippage_bps = "5"
// fee and slippage_bps are required like the built-in's, so the config
// states its costs; decisions don't use them.

fn band_pct(p) {
    if "band_pct" in p { parse_decimal(p.band_pct) } else { 0.0 }
}

fn check_params(p) {
    instrument_id(p.coin);
    if type_of(p.sma_period) != "i64" || p.sma_period < 1 {
        throw "sma_period must be an integer >= 1";
    }
    if parse_decimal(p.quote_amount) <= 0 { throw "quote_amount must be positive"; }
    let band = band_pct(p);
    if band < 0 || band >= 1 { throw "band_pct must be in [0, 1)"; }
    let fee = parse_decimal(p.fee);
    if fee < 0 || fee >= 1 { throw "fee must be in [0, 1)"; }
    let slip = parse_decimal(p.slippage_bps);
    if slip < 0 || slip >= 10000 { throw "slippage_bps must be in [0, 10000)"; }
}

fn on_candle(ctx, c) {
    let p = ctx.params;
    let coin = instrument_id(p.coin);
    // An order still working would be doubled by deciding again.
    if c.instrument != coin || ctx.open_orders(coin).len() > 0 { return; }

    let hist = ctx.history(coin);
    let n = p.sma_period;
    if hist.len() < n { return; }
    let sum = 0;
    for i in 0..n { sum += hist[hist.len() - 1 - i].close; }
    let sma = sum / n;

    // Units held, if a sell of them passes the venue's filters; otherwise
    // cash (flat, or dust a base fee left behind).
    let inst = ctx.instrument(coin);
    let long = false;
    let units = 0;
    if inst != () {
        units = inst.round_qty(ctx.position(coin).qty);
        long = inst.market_ok(units, c.close, "sell");
    }

    let band = band_pct(p);
    if !long && c.close > sma * (1 + band) {
        return market_buy_quote(coin, parse_decimal(p.quote_amount));
    }
    if long && c.close < sma * (1 - band) {
        return market_sell(coin, units);
    }
}

// Stateless: decisions come from history and the books, both rebuilt on
// a restart, so any saved state is accepted as is.
fn restore(saved, ctx) {}
