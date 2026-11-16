//! `rhai`: a strategy written as a [Rhai](https://rhai.rs) script.
//!
//! ```toml
//! [strategy]
//! name = "rhai"
//! [strategy.params]
//! script = "strategies/sma_trend.rhai"   # relative to the working directory
//! coin = "binance:BTCUSDT"               # the rest is the script's own params
//! ```
//!
//! # Numbers
//! Built without floats: `1000`, `-3` are integers, and any number with a
//! decimal point (`0.001`, `1e-3`) is an exact `Decimal` (invariant 1).
//! Integers and decimals mix in arithmetic. Every price, quantity and
//! amount the host hands over is a `Decimal`; every money argument accepts
//! an integer or a `Decimal`. Params come as written in TOML: money there
//! is a string (`fee = "0.001"`), turned into a number with
//! `parse_decimal(params.fee)`. A TOML float is refused at build.
//!
//! # Hooks
//! Every hook is optional. Top-level statements are not run; only these
//! functions are called:
//! - `check_params(params)`: at build, before trading. `throw` to refuse.
//! - `on_start(ctx)`, `on_candle(ctx, candle)`, `on_fill(ctx, fill)`,
//!   `on_order(ctx, order)`, `on_rejected(ctx, signal, reason)`: as the
//!   [`Strategy`] hooks of the same name. Each returns `()`, a signal, or an
//!   array of signals.
//! - `restore(saved, ctx)`: on a restart, with the state the last run
//!   saved. Return `()` to take it (assign what you keep to `this`), or a
//!   string saying why it contradicts the books. A script without
//!   `restore` always starts fresh.
//!
//! # State
//! `this` in every hook is one map, empty at build, that the script keeps
//! its own state in (`this.held = true;`). After every hook it is saved
//! (live: on every event), so it may hold only `()`, booleans, integers,
//! decimals, strings, arrays and maps; anything else stops the run, in
//! backtest as in live. State is only handed back to the exact script
//! that saved it (same SHA-256).
//!
//! # ctx
//! A read-only snapshot of [`Context`]: `ctx.now` (ms, `()` before the
//! first event), `ctx.params` (map), `ctx.instruments` (ids),
//! `ctx.instrument(id)` (`()` outside the universe),
//! `ctx.position(id)`, `ctx.open_orders(id)`, `ctx.cancelling_orders(id)`,
//! `ctx.history(id)` (closed candles, oldest first, ending at the current
//! one), `ctx.last_candle(id)` (`()` if none). Ids are strings
//! (`"binance:BTCUSDT"`), sides `"buy"` / `"sell"`, times ms integers.
//!
//! - Candle: `instrument open_time close_time open high low close volume
//!   quote_volume`.
//! - Instrument: `id base quote tick_size lot_size min_qty min_notional`;
//!   `round_qty(q)`, `round_price(p, side)`, `market_price(reference,
//!   side)`, `notional(q, p)`, `qty_for(notional, p)` (`()` if it fails the
//!   filters), `order_ok(q, p)`, `market_ok(q, reference, side)`: the
//!   venue's math (invariant 4), never redone by hand.
//! - Position: `instrument qty cost_basis realized_pnl is_flat`.
//! - Order: `id instrument side kind` (`market` `limit` `stop_market`
//!   `stop_limit`) `qty quote price stop created_at` (`()` where not set).
//! - Fill: `order_id instrument side qty price liquidity ts fee_kind`
//!   (`base` `quote` `other`) `fee_amount fee_asset fee_value` (quote
//!   value; `()` when not known).
//! - Signal: `action` (`submit` / `cancel`) `instrument side order_id`.
//!
//! Signals: `market_buy(id, qty)`, `market_sell(id, qty)`,
//! `market_buy_quote(id, quote)` (spend at most `quote`, fees included),
//! `limit_buy(id, qty, price)`, `limit_sell(id, qty, price)` (GTC; a 4th
//! argument `"ioc"` / `"fok"` / `"gtc"` sets time in force),
//! `stop_buy(id, qty, stop)`, `stop_sell(id, qty, stop)` (stop-market),
//! `stop_limit_buy(id, qty, stop, limit)`, `stop_limit_sell(..)`,
//! `cancel(order_id)`. Risk rounds and checks them like any signal.
//! `instrument_id(s)` gives an id in the form the host prints it
//! (`"binance:btcusdt"` → `"binance:BTCUSDT"`), for comparing with
//! `candle.instrument`.
//!
//! # Sandbox
//! No file, network, clock, randomness, `eval` or `import`. `print` and
//! `debug` go to the log. Each hook call may run at most
//! [`MAX_OPERATIONS`] operations, nest 64 calls deep, and build strings,
//! arrays and maps within fixed bounds. An error anywhere (a `throw`, a
//! type mismatch, the budget) stops the run through
//! [`Strategy::fault`] before anything the failing call returned is
//! routed.

use super::{Source, load, split_params, unwrap_state, wrap_state};
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration};
use bot_core::{
    Candle, Decimal, Fee, Fill, Instrument, InstrumentId, Liquidity, Notional, Order, OrderId,
    OrderKind, Position, Price, Qty, Side, Signal, Size, TimeInForce, Timestamp,
};
use rhai::module_resolvers::DummyModuleResolver;
use rhai::{
    AST, Array, CallFnOptions, Dynamic, Engine, EvalAltResult, INT, ImmutableString, Map, Scope,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const NAME: &str = "rhai";

/// Operations one hook call may run before it is stopped. Generous for
/// candle-frequency work (a 200-bar moving average is a few thousand),
/// small enough that a runaway loop fails within a fraction of a second.
pub const MAX_OPERATIONS: u64 = 1_000_000;

/// JSON key marking an exact decimal in saved state: `{"$decimal": "1.5"}`.
const DECIMAL_TAG: &str = "$decimal";

type RhaiResult<T> = Result<T, Box<EvalAltResult>>;

/// A script strategy; see the module docs.
pub struct RhaiStrategy {
    engine: Engine,
    ast: AST,
    source: Source,
    /// Hooks the script defines, by (name, arity).
    hooks: HashSet<(String, usize)>,
    params: Arc<Map>,
    /// The script's state (`this`).
    this: Dynamic,
    /// `this` as JSON after the last hook: what `save_state` hands out.
    saved: Value,
    fault: Option<String>,
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: RhaiStrategy::build }
}

impl RhaiStrategy {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let bad = |reason: String| StrategyError::Params {
            strategy: NAME.to_owned(),
            reason,
        };
        let (path, rest) = split_params(NAME, params)?;
        let source = load(NAME, &path)?;
        let script_params = toml_to_map(&rest).map_err(bad)?;
        let engine = engine();
        let ast = engine
            .compile(&source.text)
            .map_err(|e| bad(format!("{}: {e}", path.display())))?;
        let hooks = ast
            .iter_functions()
            .map(|f| (f.name.to_owned(), f.params.len()))
            .collect();
        let mut s = Self {
            engine,
            ast,
            source,
            hooks,
            params: Arc::new(script_params),
            this: Dynamic::from_map(Map::new()),
            saved: Value::Object(Default::default()),
            fault: None,
        };
        if s.has("check_params", 1) {
            let args = vec![Dynamic::from_map((*s.params).clone())];
            let opts = CallFnOptions::new()
                .eval_ast(false)
                .rewind_scope(true)
                .bind_this_ptr(&mut s.this);
            let _: Dynamic = s
                .engine
                .call_fn_with_options(opts, &mut Scope::new(), &s.ast, "check_params", args)
                .map_err(|e| bad(format!("{}: {e}", s.source.path.display())))?;
            s.saved = to_json(&s.this).map_err(|e| bad(format!("state `this`: {e}")))?;
        }
        Ok(Box::new(s))
    }

    fn has(&self, hook: &str, arity: usize) -> bool {
        self.hooks.contains(&(hook.to_owned(), arity))
    }

    fn fail(&mut self, reason: String) {
        let reason = format!("script {}: {reason}", self.source.path.display());
        tracing::error!(%reason, "script strategy failed");
        self.fault = Some(reason);
    }

    fn view(&self, ctx: &Context) -> Dynamic {
        Dynamic::from(ScriptContext::new(ctx, &self.params))
    }

    /// Calls `hook` if the script defines it and nothing has failed yet,
    /// with `this` bound, and saves `this` after. Any error is a fault.
    fn call(&mut self, hook: &str, args: Vec<Dynamic>) -> Option<Dynamic> {
        if self.fault.is_some() || !self.has(hook, args.len()) {
            return None;
        }
        let opts = CallFnOptions::new()
            .eval_ast(false)
            .rewind_scope(true)
            .bind_this_ptr(&mut self.this);
        let result = self.engine.call_fn_with_options::<Dynamic>(
            opts,
            &mut Scope::new(),
            &self.ast,
            hook,
            args,
        );
        match result {
            Ok(v) => match to_json(&self.this) {
                Ok(json) => {
                    self.saved = json;
                    Some(v)
                }
                Err(e) => {
                    self.fail(format!("{hook}: state `this` can't be saved: {e}"));
                    None
                }
            },
            Err(e) => {
                self.fail(format!("{hook}: {e}"));
                None
            }
        }
    }

    /// The signals a hook returned: `()`, one signal, or an array of them.
    fn signals(&mut self, hook: &str, returned: Option<Dynamic>) -> Vec<Signal> {
        let Some(v) = returned else {
            return Vec::new();
        };
        if v.is_unit() {
            return Vec::new();
        }
        if v.is::<Signal>() {
            return v.try_cast::<Signal>().into_iter().collect();
        }
        let type_name = v.type_name();
        if let Some(items) = v.try_cast::<Array>() {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let t = item.type_name();
                match item.try_cast::<Signal>() {
                    Some(s) => out.push(s),
                    None => {
                        self.fail(format!("{hook} returned an array holding {t}, not signals"));
                        return Vec::new();
                    }
                }
            }
            return out;
        }
        self.fail(format!(
            "{hook} returned {type_name}; expected (), a signal or an array of signals"
        ));
        Vec::new()
    }

    fn submit_from(&mut self, ctx: &mut Context, hook: &str, args: Vec<Dynamic>) {
        let returned = self.call(hook, args);
        for s in self.signals(hook, returned) {
            ctx.submit(s);
        }
    }
}

impl Strategy for RhaiStrategy {
    fn name(&self) -> &str {
        NAME
    }

    fn on_start(&mut self, ctx: &mut Context) {
        let args = vec![self.view(ctx)];
        self.submit_from(ctx, "on_start", args);
    }

    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let args = vec![self.view(ctx), Dynamic::from(c.clone())];
        let returned = self.call("on_candle", args);
        self.signals("on_candle", returned)
    }

    fn on_fill(&mut self, ctx: &mut Context, f: &Fill) {
        let args = vec![self.view(ctx), Dynamic::from(f.clone())];
        self.submit_from(ctx, "on_fill", args);
    }

    fn on_order(&mut self, ctx: &mut Context, o: &Order) {
        let args = vec![self.view(ctx), Dynamic::from(o.clone())];
        self.submit_from(ctx, "on_order", args);
    }

    fn on_rejected(&mut self, ctx: &mut Context, signal: &Signal, reason: &str) {
        let args = vec![
            self.view(ctx),
            Dynamic::from(signal.clone()),
            Dynamic::from(reason.to_owned()),
        ];
        self.submit_from(ctx, "on_rejected", args);
    }

    fn save_state(&self) -> Option<Value> {
        Some(wrap_state(&self.source.digest, self.saved.clone()))
    }

    fn restore_state(&mut self, state: Value, ctx: &Context) -> Result<(), StrategyError> {
        let refuse = |reason: String| StrategyError::State {
            strategy: NAME.to_owned(),
            reason,
        };
        let inner = unwrap_state(NAME, &self.source.digest, state)?;
        if !self.has("restore", 2) {
            return Err(refuse(
                "the script defines no restore(saved, ctx); starting fresh".to_owned(),
            ));
        }
        let saved = from_json(&inner).map_err(refuse)?;
        // All or nothing: the script works on a copy, kept only if it
        // accepts.
        let mut candidate = self.this.clone();
        let args = vec![saved, self.view(ctx)];
        let opts = CallFnOptions::new()
            .eval_ast(false)
            .rewind_scope(true)
            .bind_this_ptr(&mut candidate);
        let result = self.engine.call_fn_with_options::<Dynamic>(
            opts,
            &mut Scope::new(),
            &self.ast,
            "restore",
            args,
        );
        let verdict = match result {
            Ok(v) if v.is_unit() => None,
            Ok(v) if v.is_string() => {
                return Err(refuse(v.into_string().unwrap_or_default()));
            }
            Ok(v) => Some(format!(
                "restore returned {}; expected () or a reason string",
                v.type_name()
            )),
            Err(e) => Some(format!("restore: {e}")),
        };
        if let Some(reason) = verdict {
            self.fail(reason.clone());
            return Err(refuse(reason));
        }
        match to_json(&candidate) {
            Ok(json) => {
                self.this = candidate;
                self.saved = json;
                Ok(())
            }
            Err(e) => {
                let reason = format!("restore: state `this` can't be saved: {e}");
                self.fail(reason.clone());
                Err(refuse(reason))
            }
        }
    }

    fn fault(&self) -> Option<String> {
        self.fault.clone()
    }
}

/// An engine with the sandbox and API of the module docs.
fn engine() -> Engine {
    let mut e = Engine::new();
    e.set_max_operations(MAX_OPERATIONS)
        .set_max_call_levels(64)
        .set_max_expr_depths(64, 64)
        .set_max_string_size(64 * 1024)
        .set_max_array_size(1_000_000)
        .set_max_map_size(10_000)
        .set_strict_variables(true)
        .set_module_resolver(DummyModuleResolver::new());
    e.disable_symbol("eval");
    e.on_print(|s| tracing::info!(target: "script", "{s}"));
    e.on_debug(|s, _, pos| tracing::debug!(target: "script", %pos, "{s}"));
    register_context(&mut e);
    register_values(&mut e);
    register_signals(&mut e);
    e
}

// ---------------------------------------------------------------- ctx

/// What a script sees of [`Context`], copied before each hook call: a
/// script holds no reference into the engine, and nothing it does to this
/// reaches the books.
#[derive(Clone)]
pub struct ScriptContext(Arc<View>);

struct View {
    now: Option<Timestamp>,
    params: Arc<Map>,
    instruments: Vec<Instrument>,
    positions: HashMap<InstrumentId, Position>,
    open: Vec<Order>,
    cancelling: Vec<Order>,
    history: HashMap<InstrumentId, Vec<Candle>>,
}

impl ScriptContext {
    fn new(ctx: &Context, params: &Arc<Map>) -> Self {
        let instruments: Vec<Instrument> = ctx.instruments().cloned().collect();
        let mut view = View {
            now: ctx.now(),
            params: params.clone(),
            positions: HashMap::new(),
            open: Vec::new(),
            cancelling: Vec::new(),
            history: HashMap::new(),
            instruments: Vec::new(),
        };
        for i in &instruments {
            view.positions.insert(i.id.clone(), ctx.position(&i.id));
            view.open.extend(ctx.open_orders(&i.id).cloned());
            view.cancelling
                .extend(ctx.cancelling_orders(&i.id).cloned());
            view.history
                .insert(i.id.clone(), ctx.history(&i.id).cloned().collect());
        }
        view.instruments = instruments;
        Self(Arc::new(view))
    }
}

fn instrument_id(s: &str) -> RhaiResult<InstrumentId> {
    s.parse()
        .map_err(|e| format!("bad instrument id {s:?}: {e}").into())
}

fn side(s: &str) -> RhaiResult<Side> {
    match s {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        _ => Err(format!("side must be \"buy\" or \"sell\", got {s:?}").into()),
    }
}

fn side_str(s: Side) -> ImmutableString {
    match s {
        Side::Buy => "buy".into(),
        Side::Sell => "sell".into(),
    }
}

/// An exact decimal from an integer or a decimal argument.
fn dec(v: &Dynamic) -> RhaiResult<Decimal> {
    if let Ok(d) = v.as_decimal() {
        return Ok(d);
    }
    if let Ok(i) = v.as_int() {
        return Ok(Decimal::from(i));
    }
    Err(format!("expected a number, got {}", v.type_name()).into())
}

fn ms(t: Timestamp) -> INT {
    t.as_millis()
}

fn orders_on(orders: &[Order], id: &InstrumentId) -> Array {
    orders
        .iter()
        .filter(|o| o.instrument == *id)
        .cloned()
        .map(Dynamic::from)
        .collect()
}

fn register_context(e: &mut Engine) {
    e.register_type_with_name::<ScriptContext>("Context")
        .register_get("now", |c: &mut ScriptContext| {
            c.0.now.map_or(Dynamic::UNIT, |t| Dynamic::from_int(ms(t)))
        })
        .register_get("params", |c: &mut ScriptContext| (*c.0.params).clone())
        .register_get("instruments", |c: &mut ScriptContext| -> Array {
            c.0.instruments
                .iter()
                .map(|i| Dynamic::from(i.id.to_string()))
                .collect()
        })
        .register_fn(
            "instrument",
            |c: &mut ScriptContext, id: &str| -> RhaiResult<Dynamic> {
                let id = instrument_id(id)?;
                Ok(c.0
                    .instruments
                    .iter()
                    .find(|i| i.id == id)
                    .map_or(Dynamic::UNIT, |i| Dynamic::from(i.clone())))
            },
        )
        .register_fn(
            "position",
            |c: &mut ScriptContext, id: &str| -> RhaiResult<Position> {
                let id = instrument_id(id)?;
                Ok(c.0
                    .positions
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| Position::flat(id)))
            },
        )
        .register_fn(
            "open_orders",
            |c: &mut ScriptContext, id: &str| -> RhaiResult<Array> {
                Ok(orders_on(&c.0.open, &instrument_id(id)?))
            },
        )
        .register_fn(
            "cancelling_orders",
            |c: &mut ScriptContext, id: &str| -> RhaiResult<Array> {
                Ok(orders_on(&c.0.cancelling, &instrument_id(id)?))
            },
        )
        .register_fn(
            "history",
            |c: &mut ScriptContext, id: &str| -> RhaiResult<Array> {
                let id = instrument_id(id)?;
                Ok(c.0.history.get(&id).map_or_else(Array::new, |h| {
                    h.iter().cloned().map(Dynamic::from).collect()
                }))
            },
        )
        .register_fn(
            "last_candle",
            |c: &mut ScriptContext, id: &str| -> RhaiResult<Dynamic> {
                let id = instrument_id(id)?;
                Ok(c.0
                    .history
                    .get(&id)
                    .and_then(|h| h.last())
                    .map_or(Dynamic::UNIT, |c| Dynamic::from(c.clone())))
            },
        );
}

// ------------------------------------------------------------- values

fn opt_dec(d: Option<Decimal>) -> Dynamic {
    d.map_or(Dynamic::UNIT, Dynamic::from_decimal)
}

fn register_values(e: &mut Engine) {
    e.register_type_with_name::<Candle>("Candle")
        .register_get("instrument", |c: &mut Candle| c.instrument.to_string())
        .register_get("open_time", |c: &mut Candle| ms(c.open_time))
        .register_get("close_time", |c: &mut Candle| ms(c.close_time))
        .register_get("open", |c: &mut Candle| c.open.get())
        .register_get("high", |c: &mut Candle| c.high.get())
        .register_get("low", |c: &mut Candle| c.low.get())
        .register_get("close", |c: &mut Candle| c.close.get())
        .register_get("volume", |c: &mut Candle| c.volume.get())
        .register_get("quote_volume", |c: &mut Candle| c.quote_volume.get());

    e.register_type_with_name::<Position>("Position")
        .register_get("instrument", |p: &mut Position| p.instrument().to_string())
        .register_get("qty", |p: &mut Position| p.qty().get())
        .register_get("cost_basis", |p: &mut Position| p.cost_basis().get())
        .register_get("realized_pnl", |p: &mut Position| p.realized_pnl().get())
        .register_get("is_flat", |p: &mut Position| p.is_flat());

    e.register_type_with_name::<Instrument>("Instrument")
        .register_get("id", |i: &mut Instrument| i.id.to_string())
        .register_get("base", |i: &mut Instrument| i.base.to_string())
        .register_get("quote", |i: &mut Instrument| i.quote.to_string())
        .register_get("tick_size", |i: &mut Instrument| i.tick_size.get())
        .register_get("lot_size", |i: &mut Instrument| i.lot_size.get())
        .register_get("min_qty", |i: &mut Instrument| i.min_qty.get())
        .register_get("min_notional", |i: &mut Instrument| i.min_notional.get())
        .register_fn(
            "round_qty",
            |i: &mut Instrument, q: Dynamic| -> RhaiResult<Decimal> {
                Ok(i.round_qty(Qty::new(dec(&q)?)).get())
            },
        )
        .register_fn(
            "round_price",
            |i: &mut Instrument, p: Dynamic, s: &str| -> RhaiResult<Decimal> {
                Ok(i.round_price(Price::new(dec(&p)?), side(s)?).get())
            },
        )
        .register_fn(
            "market_price",
            |i: &mut Instrument, r: Dynamic, s: &str| -> RhaiResult<Decimal> {
                Ok(i.market_price(Price::new(dec(&r)?), side(s)?).get())
            },
        )
        .register_fn(
            "notional",
            |i: &mut Instrument, q: Dynamic, p: Dynamic| -> RhaiResult<Decimal> {
                Ok(i.notional(Qty::new(dec(&q)?), Price::new(dec(&p)?)).get())
            },
        )
        .register_fn(
            "qty_for",
            |i: &mut Instrument, n: Dynamic, p: Dynamic| -> RhaiResult<Dynamic> {
                Ok(opt_dec(
                    i.qty_for(Notional::new(dec(&n)?), Price::new(dec(&p)?))
                        .ok()
                        .map(Qty::get),
                ))
            },
        )
        .register_fn(
            "order_ok",
            |i: &mut Instrument, q: Dynamic, p: Dynamic| -> RhaiResult<bool> {
                Ok(i.check_order(Qty::new(dec(&q)?), Price::new(dec(&p)?))
                    .is_ok())
            },
        )
        .register_fn(
            "market_ok",
            |i: &mut Instrument, q: Dynamic, r: Dynamic, s: &str| -> RhaiResult<bool> {
                Ok(
                    i.check_market(Qty::new(dec(&q)?), Price::new(dec(&r)?), side(s)?)
                        .is_ok(),
                )
            },
        );

    e.register_type_with_name::<Order>("Order")
        .register_get("id", |o: &mut Order| o.id.to_string())
        .register_get("instrument", |o: &mut Order| o.instrument.to_string())
        .register_get("side", |o: &mut Order| side_str(o.side))
        .register_get("kind", |o: &mut Order| -> ImmutableString {
            match o.kind {
                OrderKind::Market => "market".into(),
                OrderKind::Limit { .. } => "limit".into(),
                OrderKind::StopMarket { .. } => "stop_market".into(),
                OrderKind::StopLimit { .. } => "stop_limit".into(),
            }
        })
        .register_get("qty", |o: &mut Order| match o.size {
            Size::Base(q) => Dynamic::from_decimal(q.get()),
            Size::Quote(_) => Dynamic::UNIT,
        })
        .register_get("quote", |o: &mut Order| match o.size {
            Size::Quote(n) => Dynamic::from_decimal(n.get()),
            Size::Base(_) => Dynamic::UNIT,
        })
        .register_get("price", |o: &mut Order| match o.kind {
            OrderKind::Limit { price, .. } => Dynamic::from_decimal(price.get()),
            OrderKind::StopLimit { limit, .. } => Dynamic::from_decimal(limit.get()),
            _ => Dynamic::UNIT,
        })
        .register_get("stop", |o: &mut Order| match o.kind {
            OrderKind::StopMarket { stop } | OrderKind::StopLimit { stop, .. } => {
                Dynamic::from_decimal(stop.get())
            }
            _ => Dynamic::UNIT,
        })
        .register_get("created_at", |o: &mut Order| ms(o.created_at));

    e.register_type_with_name::<Fill>("Fill")
        .register_get("order_id", |f: &mut Fill| f.order_id.to_string())
        .register_get("instrument", |f: &mut Fill| f.instrument.to_string())
        .register_get("side", |f: &mut Fill| side_str(f.side))
        .register_get("qty", |f: &mut Fill| f.qty.get())
        .register_get("price", |f: &mut Fill| f.price.get())
        .register_get("liquidity", |f: &mut Fill| -> ImmutableString {
            match f.liquidity {
                Liquidity::Maker => "maker".into(),
                Liquidity::Taker => "taker".into(),
            }
        })
        .register_get("ts", |f: &mut Fill| ms(f.ts))
        .register_get("fee_kind", |f: &mut Fill| -> ImmutableString {
            match f.fee {
                Fee::Base(_) => "base".into(),
                Fee::Quote(_) => "quote".into(),
                Fee::Other { .. } => "other".into(),
            }
        })
        .register_get("fee_amount", |f: &mut Fill| match &f.fee {
            Fee::Base(q) => q.get(),
            Fee::Quote(n) => n.get(),
            Fee::Other { amount, .. } => amount.get(),
        })
        .register_get("fee_asset", |f: &mut Fill| -> Dynamic {
            match &f.fee {
                Fee::Other { asset, .. } => Dynamic::from(asset.to_string()),
                _ => Dynamic::UNIT,
            }
        })
        .register_get("fee_value", |f: &mut Fill| match &f.fee {
            Fee::Quote(n) => Dynamic::from_decimal(n.get()),
            Fee::Other { value, .. } => opt_dec(value.map(Notional::get)),
            Fee::Base(_) => Dynamic::UNIT,
        });
}

// ------------------------------------------------------------ signals

fn submit(id: &str, s: Side, size: Size, kind: OrderKind) -> RhaiResult<Signal> {
    Ok(Signal::Submit {
        instrument: instrument_id(id)?,
        side: s,
        size,
        kind,
    })
}

fn base(q: &Dynamic) -> RhaiResult<Size> {
    Ok(Size::Base(Qty::new(dec(q)?)))
}

fn price(p: &Dynamic) -> RhaiResult<Price> {
    Ok(Price::new(dec(p)?))
}

fn tif(s: &str) -> RhaiResult<TimeInForce> {
    match s {
        "gtc" => Ok(TimeInForce::Gtc),
        "ioc" => Ok(TimeInForce::Ioc),
        "fok" => Ok(TimeInForce::Fok),
        _ => Err(format!("time in force must be gtc, ioc or fok, got {s:?}").into()),
    }
}

fn register_signals(e: &mut Engine) {
    e.register_type_with_name::<Signal>("Signal")
        .register_get("action", |s: &mut Signal| -> ImmutableString {
            match s {
                Signal::Submit { .. } => "submit".into(),
                Signal::Cancel(_) => "cancel".into(),
            }
        })
        .register_get("instrument", |s: &mut Signal| match s {
            Signal::Submit { instrument, .. } => Dynamic::from(instrument.to_string()),
            Signal::Cancel(_) => Dynamic::UNIT,
        })
        .register_get("side", |s: &mut Signal| match s {
            Signal::Submit { side, .. } => Dynamic::from(side_str(*side)),
            Signal::Cancel(_) => Dynamic::UNIT,
        })
        .register_get("order_id", |s: &mut Signal| match s {
            Signal::Cancel(id) => Dynamic::from(id.to_string()),
            Signal::Submit { .. } => Dynamic::UNIT,
        });

    e.register_fn("market_buy", |id: &str, q: Dynamic| {
        submit(id, Side::Buy, base(&q)?, OrderKind::Market)
    })
    .register_fn("market_sell", |id: &str, q: Dynamic| {
        submit(id, Side::Sell, base(&q)?, OrderKind::Market)
    })
    .register_fn("market_buy_quote", |id: &str, n: Dynamic| {
        submit(
            id,
            Side::Buy,
            Size::Quote(Notional::new(dec(&n)?)),
            OrderKind::Market,
        )
    });
    for (name, s) in [("limit_buy", Side::Buy), ("limit_sell", Side::Sell)] {
        e.register_fn(name, move |id: &str, q: Dynamic, p: Dynamic| {
            let kind = OrderKind::Limit {
                price: price(&p)?,
                tif: TimeInForce::Gtc,
            };
            submit(id, s, base(&q)?, kind)
        })
        .register_fn(name, move |id: &str, q: Dynamic, p: Dynamic, t: &str| {
            let kind = OrderKind::Limit {
                price: price(&p)?,
                tif: tif(t)?,
            };
            submit(id, s, base(&q)?, kind)
        });
    }
    for (name, s) in [("stop_buy", Side::Buy), ("stop_sell", Side::Sell)] {
        e.register_fn(name, move |id: &str, q: Dynamic, stop: Dynamic| {
            let kind = OrderKind::StopMarket {
                stop: price(&stop)?,
            };
            submit(id, s, base(&q)?, kind)
        });
    }
    for (name, s) in [
        ("stop_limit_buy", Side::Buy),
        ("stop_limit_sell", Side::Sell),
    ] {
        e.register_fn(
            name,
            move |id: &str, q: Dynamic, stop: Dynamic, limit: Dynamic| {
                let kind = OrderKind::StopLimit {
                    stop: price(&stop)?,
                    limit: price(&limit)?,
                    tif: TimeInForce::Gtc,
                };
                submit(id, s, base(&q)?, kind)
            },
        );
    }
    e.register_fn("instrument_id", |id: &str| -> RhaiResult<ImmutableString> {
        Ok(instrument_id(id)?.to_string().into())
    });
    e.register_fn("cancel", |id: &str| -> RhaiResult<Signal> {
        OrderId::new(id)
            .map(Signal::Cancel)
            .map_err(|e| format!("bad order id {id:?}: {e}").into())
    });
}

// ----------------------------------------------------- params & state

/// The script's params as a Rhai map. TOML floats are refused: they'd
/// reach the script through f64 (invariant 1).
fn toml_to_map(table: &toml::Table) -> Result<Map, String> {
    table
        .iter()
        .map(|(k, v)| Ok((k.as_str().into(), toml_value(k, v)?)))
        .collect()
}

fn toml_value(key: &str, v: &toml::Value) -> Result<Dynamic, String> {
    Ok(match v {
        toml::Value::String(s) => Dynamic::from(s.clone()),
        toml::Value::Integer(i) => Dynamic::from_int(*i),
        toml::Value::Boolean(b) => Dynamic::from_bool(*b),
        toml::Value::Array(a) => Dynamic::from_array(
            a.iter()
                .map(|x| toml_value(key, x))
                .collect::<Result<_, _>>()?,
        ),
        toml::Value::Table(t) => Dynamic::from_map(toml_to_map(t)?),
        toml::Value::Float(_) => {
            return Err(format!(
                "`{key}`: write non-integer numbers as strings (\"0.001\"); a TOML float \
                 passes through f64"
            ));
        }
        toml::Value::Datetime(_) => return Err(format!("`{key}`: datetimes aren't supported")),
    })
}

/// `this` as JSON. Decimals become `{"$decimal": "…"}` so they come back
/// as decimals, exactly.
fn to_json(v: &Dynamic) -> Result<Value, String> {
    if v.is_unit() {
        return Ok(Value::Null);
    }
    if let Ok(b) = v.as_bool() {
        return Ok(Value::Bool(b));
    }
    if let Ok(i) = v.as_int() {
        return Ok(Value::from(i));
    }
    if let Ok(d) = v.as_decimal() {
        return Ok(serde_json::json!({ DECIMAL_TAG: d.to_string() }));
    }
    if v.is_string() {
        return Ok(Value::String(v.clone().into_string().unwrap_or_default()));
    }
    if v.is_array() {
        let a = v.read_lock::<Array>().ok_or("array is locked")?;
        return a
            .iter()
            .map(to_json)
            .collect::<Result<_, _>>()
            .map(Value::Array);
    }
    if v.is_map() {
        let m = v.read_lock::<Map>().ok_or("map is locked")?;
        if m.len() == 1 && m.contains_key(DECIMAL_TAG) {
            return Err(format!("a map whose only key is {DECIMAL_TAG:?}"));
        }
        return m
            .iter()
            .map(|(k, x)| Ok((k.to_string(), to_json(x)?)))
            .collect::<Result<serde_json::Map<_, _>, String>>()
            .map(Value::Object);
    }
    Err(format!("a value of type {}", v.type_name()))
}

/// Inverse of [`to_json`].
fn from_json(v: &Value) -> Result<Dynamic, String> {
    Ok(match v {
        Value::Null => Dynamic::UNIT,
        Value::Bool(b) => Dynamic::from_bool(*b),
        Value::Number(n) => Dynamic::from_int(
            n.as_i64()
                .ok_or_else(|| format!("number {n} is not an integer"))?,
        ),
        Value::String(s) => Dynamic::from(s.clone()),
        Value::Array(a) => Dynamic::from_array(a.iter().map(from_json).collect::<Result<_, _>>()?),
        Value::Object(o) => {
            if let (1, Some(Value::String(s))) = (o.len(), o.get(DECIMAL_TAG)) {
                return Decimal::from_str_exact(s)
                    .map(Dynamic::from_decimal)
                    .map_err(|e| format!("decimal {s:?}: {e}"));
            }
            Dynamic::from_map(
                o.iter()
                    .map(|(k, x)| Ok((k.as_str().into(), from_json(x)?)))
                    .collect::<Result<Map, String>>()?,
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use rust_decimal_macros::dec;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Writes `body` to a fresh temp file; returns its path.
    fn script(body: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "tb-rhai-{}-{}.rhai",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn params(path: &std::path::Path, extra: &str) -> Params {
        Params::from_toml(&format!(
            "script = {:?}\n{extra}",
            path.display().to_string()
        ))
        .unwrap()
    }

    fn make(body: &str) -> Box<dyn Strategy> {
        make_with(body, "")
    }

    /// `extra`: the script's own params (TOML lines).
    fn make_with(body: &str, extra: &str) -> Box<dyn Strategy> {
        build(NAME, &params(&script(body), extra)).unwrap()
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt()], Params::default(), 10)
    }

    /// Feeds candle `i` at `price` as the engine does; returns the signals.
    fn bar(s: &mut dyn Strategy, ctx: &mut Context, i: i64, price: &str) -> Vec<Signal> {
        let c = candle(i, price);
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        let mut out = s.on_candle(ctx, &c);
        out.extend(driver::take_signals(ctx));
        out
    }

    fn btc() -> InstrumentId {
        btcusdt().id.clone()
    }

    fn state(s: &dyn Strategy) -> Value {
        s.save_state().unwrap()["state"].clone()
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(make("").name(), NAME);
    }

    #[test]
    fn build_refuses_bad_scripts_and_params() {
        // Syntax error.
        assert!(build(NAME, &params(&script("fn on_candle(ctx, c) {"), "")).is_err());
        // check_params throws.
        let p = script(r#"fn check_params(p) { if p.n < 1 { throw "n must be >= 1"; } }"#);
        assert!(build(NAME, &params(&p, "n = 1")).is_ok());
        let e = build(NAME, &params(&p, "n = 0")).err().unwrap();
        assert!(e.to_string().contains("n must be >= 1"), "{e}");
        // A TOML float would reach the script through f64 (invariant 1).
        let e = build(NAME, &params(&script(""), "fee = 0.001"))
            .err()
            .unwrap();
        assert!(e.to_string().contains("fee"), "{e}");
    }

    /// Non-integer numbers are exact decimals, literals included: 0.1 + 0.2
    /// is 0.3, not 0.30000000000000004, and survives a save as a decimal.
    #[test]
    fn money_is_exact() {
        let mut s = make(
            r#"fn on_candle(ctx, c) {
                   this.sum = 0.1 + 0.2;
                   this.close = c.close * 3;
                   this.qty = ctx.instrument(c.instrument).round_qty(0.123456789);
               }"#,
        );
        let mut ctx = ctx();
        bar(s.as_mut(), &mut ctx, 0, "100.01");
        assert_eq!(s.fault(), None);
        assert_eq!(
            state(s.as_ref()),
            json!({
                "sum": {"$decimal": "0.3"},
                "close": {"$decimal": "300.03"},
                "qty": {"$decimal": "0.12345"},
            })
        );
    }

    /// No file, OS, clock, `eval` or `import`.
    #[test]
    fn the_sandbox_holds() {
        // `eval` is not even a word the parser accepts.
        assert!(
            build(
                NAME,
                &params(&script(r#"fn on_start(ctx) { eval("1") }"#), "")
            )
            .is_err()
        );
        for body in [
            r#"fn on_start(ctx) { import "std" as m; }"#,
            "fn on_start(ctx) { timestamp() }",
            r#"fn on_start(ctx) { open_file("/etc/passwd") }"#,
            r#"fn on_start(ctx) { system("ls") }"#,
        ] {
            let mut s = make(body);
            s.on_start(&mut ctx());
            assert!(s.fault().is_some(), "{body} ran");
        }
    }

    #[test]
    fn a_runaway_loop_hits_the_budget() {
        let mut s = make("fn on_candle(ctx, c) { loop { } }");
        let out = bar(s.as_mut(), &mut ctx(), 0, "100");
        assert!(out.is_empty());
        let fault = s.fault().unwrap();
        assert!(fault.to_lowercase().contains("operations"), "{fault}");
    }

    /// An error is a fault, nothing from the failing call is returned, and
    /// no later hook runs.
    #[test]
    fn script_errors_are_faults() {
        let mut s = make(
            r#"fn on_candle(ctx, c) {
                   this.calls = (this.calls ?? 0) + 1;
                   if this.calls == 2 { throw "boom"; }
                   [market_buy(c.instrument, 1)]
               }"#,
        );
        let mut ctx = ctx();
        assert_eq!(bar(s.as_mut(), &mut ctx, 0, "100").len(), 1);
        assert!(bar(s.as_mut(), &mut ctx, 1, "100").is_empty());
        assert!(s.fault().unwrap().contains("boom"));
        assert!(bar(s.as_mut(), &mut ctx, 2, "100").is_empty());
        // What is saved is the state after the last call that succeeded.
        assert_eq!(state(s.as_ref()), json!({"calls": 1}));

        for body in [
            "fn on_candle(ctx, c) { 42 }",
            "fn on_candle(ctx, c) { [1] }",
            // State that can't be saved fails in backtest as in live.
            "fn on_candle(ctx, c) { this.bar = c; }",
            r#"fn on_candle(ctx, c) { market_buy(c.instrument, "lots") }"#,
            r#"fn on_candle(ctx, c) { market_buy("nonsense", 1) }"#,
        ] {
            let mut s = make(body);
            assert!(bar(s.as_mut(), &mut ctx, 3, "100").is_empty(), "{body}");
            assert!(s.fault().is_some(), "{body}");
        }
    }

    #[test]
    fn the_script_sees_the_context() {
        let mut s = make_with(
            r#"fn on_candle(ctx, c) {
                   let h = ctx.history(c.instrument);
                   this.seen = #{
                       now: ctx.now, n: h.len(), first: h[0].close, last: ctx.last_candle(c.instrument).close,
                       held: ctx.position(c.instrument).qty, flat: ctx.position(c.instrument).is_flat,
                       ids: ctx.instruments, coin: ctx.params.coin, other: ctx.instrument("binance:ETHUSDT"),
                       open: ctx.open_orders(c.instrument).len(),
                   };
               }"#,
            "coin = \"x\"",
        );
        let mut ctx = ctx();
        bar(s.as_mut(), &mut ctx, 0, "100");
        driver::set_position(
            &mut ctx,
            Position::from_parts(
                btc(),
                Qty::new(dec!(0.5)),
                Notional::new(dec!(50)),
                Notional::ZERO,
            )
            .unwrap(),
        );
        bar(s.as_mut(), &mut ctx, 1, "101");
        assert_eq!(s.fault(), None);
        assert_eq!(
            state(s.as_ref())["seen"],
            json!({
                "now": 7_199_999, "n": 2, "first": {"$decimal": "100"}, "last": {"$decimal": "101"},
                "held": {"$decimal": "0.5"}, "flat": false, "ids": ["binance:BTCUSDT"],
                "coin": "x", "other": null, "open": 0,
            })
        );
    }

    #[test]
    fn signal_constructors_build_the_signals_they_name() {
        let mut s = make(
            r#"fn on_candle(ctx, c) {
                   let id = "binance:btcusdt";
                   [market_buy(id, 1), market_sell(id, 0.5), market_buy_quote(id, 100),
                    limit_buy(id, 1, 99.5), limit_sell(id, 1, 101, "ioc"),
                    stop_buy(id, 1, 105), stop_limit_sell(id, 1, 95, 94.9), cancel("o-3")]
               }"#,
        );
        let out = bar(s.as_mut(), &mut ctx(), 0, "100");
        assert_eq!(s.fault(), None);
        let sub = |side, size, kind| Signal::Submit {
            instrument: btc(),
            side,
            size,
            kind,
        };
        let q = |d| Size::Base(Qty::new(d));
        let p = Price::new;
        assert_eq!(
            out,
            vec![
                sub(Side::Buy, q(dec!(1)), OrderKind::Market),
                sub(Side::Sell, q(dec!(0.5)), OrderKind::Market),
                sub(
                    Side::Buy,
                    Size::Quote(Notional::new(dec!(100))),
                    OrderKind::Market
                ),
                sub(
                    Side::Buy,
                    q(dec!(1)),
                    OrderKind::Limit {
                        price: p(dec!(99.5)),
                        tif: TimeInForce::Gtc
                    }
                ),
                sub(
                    Side::Sell,
                    q(dec!(1)),
                    OrderKind::Limit {
                        price: p(dec!(101)),
                        tif: TimeInForce::Ioc
                    }
                ),
                sub(
                    Side::Buy,
                    q(dec!(1)),
                    OrderKind::StopMarket { stop: p(dec!(105)) }
                ),
                sub(
                    Side::Sell,
                    q(dec!(1)),
                    OrderKind::StopLimit {
                        stop: p(dec!(95)),
                        limit: p(dec!(94.9)),
                        tif: TimeInForce::Gtc
                    }
                ),
                Signal::Cancel(OrderId::new("o-3").unwrap()),
            ]
        );
    }

    /// Signals from hooks without a return path go through `ctx.submit`.
    #[test]
    fn start_fill_and_rejection_hooks_submit() {
        let mut s = make(
            r#"fn on_start(ctx) { market_buy("binance:BTCUSDT", 1) }
               fn on_rejected(ctx, sig, reason) { this.why = `${sig.action} ${sig.side}: ${reason}`; }"#,
        );
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        assert_eq!(driver::take_signals(&mut ctx).len(), 1);
        let sig = Signal::Submit {
            instrument: btc(),
            side: Side::Buy,
            size: Size::Base(Qty::new(dec!(1))),
            kind: OrderKind::Market,
        };
        s.on_rejected(&mut ctx, &sig, "too small");
        assert_eq!(state(s.as_ref()), json!({"why": "submit buy: too small"}));
    }

    const COUNTER: &str = r#"
        fn on_candle(ctx, c) { this.n = (this.n ?? 0) + 1; this.px = c.close; }
        fn restore(saved, ctx) {
            if saved.n > 5 { return "too many"; }
            this.n = saved.n;
            this.px = saved.px;
        }"#;

    #[test]
    fn state_round_trips_through_restore() {
        let path = script(COUNTER);
        let mut s1 = build(NAME, &params(&path, "")).unwrap();
        let mut c = ctx();
        bar(s1.as_mut(), &mut c, 0, "100.5");
        bar(s1.as_mut(), &mut c, 1, "101.25");
        let saved = s1.save_state().unwrap();

        let mut s2 = build(NAME, &params(&path, "")).unwrap();
        s2.restore_state(saved, &c).unwrap();
        assert_eq!(
            state(s2.as_ref()),
            json!({"n": 2, "px": {"$decimal": "101.25"}})
        );
        bar(s2.as_mut(), &mut c, 2, "99");
        assert_eq!(state(s2.as_ref())["n"], json!(3));
    }

    #[test]
    fn restore_is_all_or_nothing() {
        let path = script(COUNTER);
        let mut s = build(NAME, &params(&path, "")).unwrap();
        let c = ctx();
        let refused = wrap_state(&load(NAME, &path).unwrap().digest, json!({"n": 9, "px": 1}));
        assert!(s.restore_state(refused, &c).is_err());
        assert_eq!(state(s.as_ref()), json!({}));
        assert_eq!(s.fault(), None, "a refusal is not a fault");

        // Saved by another script (an edit changes the digest).
        let other = wrap_state("0000", json!({"n": 1, "px": 1}));
        assert!(s.restore_state(other, &c).is_err());

        // No `restore`: saved state is never taken blind.
        let mut plain = make("fn on_candle(ctx, c) { this.n = 1; }");
        let saved = plain.save_state().unwrap();
        assert!(plain.restore_state(saved, &c).is_err());
    }

    #[test]
    fn json_keeps_types_exact() {
        let v = json!({"a": [null, true, 3, {"$decimal": "-0.000000001"}, "s"], "m": {}});
        assert_eq!(to_json(&from_json(&v).unwrap()).unwrap(), v);
        assert!(from_json(&json!(1.5)).is_err());
        assert!(from_json(&json!({"$decimal": "x"})).is_err());
    }

    /// The shipped `strategies/sma_trend.rhai` decides like the built-in
    /// `sma_trend` on the same bars (the backtest test compares fills).
    #[test]
    fn shipped_sma_trend_script_matches_the_builtin() {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../strategies/sma_trend.rhai"
        ));
        let common = "coin = \"binance:BTCUSDT\"\nsma_period = 3\nquote_amount = \"1000\"\n\
                      band_pct = \"0.01\"\nfee = \"0.001\"\nslippage_bps = \"10\"";
        let mut script = build(NAME, &params(&path, common)).unwrap();
        let mut native = build("sma_trend", &Params::from_toml(common).unwrap()).unwrap();
        let (mut c1, mut c2) = (ctx(), ctx());
        let prices = [
            "100", "100", "100", "103", "104", "104", "101", "99", "98", "100", "106",
        ];
        let mut decisions = 0;
        for (i, px) in prices.iter().enumerate() {
            let i = i as i64;
            let a = bar(script.as_mut(), &mut c1, i, px);
            let b = bar(native.as_mut(), &mut c2, i, px);
            assert_eq!(a, b, "bar {i}");
            decisions += a.len();
            // Fill whatever was decided at the price, so both see a holding.
            for s in a {
                if let Signal::Submit { side, .. } = s {
                    let held = match side {
                        Side::Buy => dec!(9.5),
                        Side::Sell => dec!(0),
                    };
                    for c in [&mut c1, &mut c2] {
                        driver::set_position(
                            c,
                            Position::from_parts(
                                btc(),
                                Qty::new(held),
                                Notional::new(held * dec!(100)),
                                Notional::ZERO,
                            )
                            .unwrap(),
                        );
                    }
                }
            }
        }
        assert_eq!(script.fault(), None);
        assert_eq!(decisions, 3, "buy, sell, buy");
        let e = build(
            NAME,
            &params(&path, &common.replace("sma_period = 3", "sma_period = 0")),
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("sma_period"), "{e}");
    }
}
