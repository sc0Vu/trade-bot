//! AI-drafted script strategies for the console: a description in plain
//! words goes to Claude with the script reference (`doc/scripting.md`),
//! and a Rhai script, its params and an explanation come back.
//!
//! A draft is a starting point, not a strategy: the console saves it and
//! runs the same checks any script gets (build and `check_params`,
//! lookahead, restart, a backtest against buy-and-hold). Nothing a draft
//! says about its own edge is taken on trust.
//!
//! Over raw HTTPS (there is no Anthropic SDK for Rust) to the Messages API:
//! `claude-opus-5-5` unless the console is started with another model,
//! adaptive thinking at `high` effort, JSON output constrained to a schema,
//! and server-side refusal fallback (`fallbacks: "default"`), so a policy
//! decline is retried on another model inside the same call. The key comes
//! from `ANTHROPIC_API_KEY` at each request and is never stored, logged or
//! sent to the page. Each draft is billed to that key.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The script reference the model writes against (embedded, so a release
/// binary carries it).
const REFERENCE: &str = include_str!("../../../doc/scripting.md");
const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Default model.
pub(crate) const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// Thinking at high effort on a hard task can take minutes.
const TIMEOUT: Duration = Duration::from_secs(600);

/// What the console asks for.
#[derive(Debug, Deserialize)]
pub(crate) struct DraftRequest {
    /// The strategy in plain words.
    pub description: String,
    /// Instruments it will trade (`etoro:AAPL.RTH`), for the params.
    #[serde(default)]
    pub instruments: Vec<String>,
    /// Bar interval (`1d`), for periods in bars.
    #[serde(default)]
    pub interval: String,
    /// A script to revise instead of starting fresh, with what to change.
    #[serde(default)]
    pub revise: Option<String>,
}

/// What comes back.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Draft {
    /// snake_case, for the file name.
    pub name: String,
    /// The Rhai script.
    pub script: String,
    /// Its params as TOML (money as strings).
    pub params_toml: String,
    /// What it does, why it might work after costs, and how it fails.
    pub explanation: String,
}

/// Why no draft came back.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DraftError {
    #[error("ANTHROPIC_API_KEY is not set: export it before starting the console")]
    NoKey,
    #[error("Anthropic API request failed: {0}")]
    Http(String),
    #[error("Anthropic API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("the model declined this request ({0})")]
    Refused(String),
    #[error("the draft didn't fit in the reply; describe a smaller strategy")]
    Truncated,
    #[error("unexpected reply: {0}")]
    Decode(String),
}

const SYSTEM: &str = "You write trading strategies as Rhai scripts for trade-bot, an \
event-driven bot whose backtests and live runs share one engine. Write against the \
reference below exactly: use only the functions, fields and hooks it lists, never \
invent one. The script decides on closed candles; orders fill at the next open. Keep \
it simple and stateless where you can (decide from ctx.position and ctx.history), \
write check_params validating every param it reads, never decide while an order on \
the coin is working, treat unsellable dust as cash (market_ok), and read money \
params with parse_decimal. Params must include fee and slippage_bps as strings. Use \
ctx.history(id, n) with the smallest n you need. In the explanation, say what the \
strategy bets on, why that might hold after fees and slippage, and how it fails; do \
not claim an edge you can't show. The user will backtest it against buy-and-hold \
and run lookahead and restart checks on it.\n\n# Script reference\n\n";

/// The Messages API request body for `req` on `model`.
pub(crate) fn request_body(req: &DraftRequest, model: &str) -> serde_json::Value {
    let mut user = format!("Strategy: {}\n", req.description.trim());
    if !req.instruments.is_empty() {
        user.push_str(&format!(
            "Instruments (use these ids in params): {}\n",
            req.instruments.join(", ")
        ));
    }
    if !req.interval.is_empty() {
        user.push_str(&format!("Bar interval: {}\n", req.interval));
    }
    if let Some(old) = &req.revise {
        user.push_str(&format!(
            "\nRevise this existing script as described above, keeping what works:\n\n{old}\n"
        ));
    }
    serde_json::json!({
        "model": model,
        "max_tokens": 16000,
        "thinking": { "type": "adaptive" },
        "output_config": {
            "effort": "high",
            "format": { "type": "json_schema", "schema": draft_schema() }
        },
        "fallbacks": "default",
        "system": format!("{SYSTEM}{REFERENCE}"),
        "messages": [{ "role": "user", "content": user }]
    })
}

fn draft_schema() -> serde_json::Value {
    let text = |what: &str| serde_json::json!({ "type": "string", "description": what });
    serde_json::json!({
        "type": "object",
        "properties": {
            "name": text("snake_case name for the file, lowercase letters, digits and _"),
            "script": text("the complete Rhai script"),
            "params_toml": text("the script's params as TOML, money as strings"),
            "explanation": text("what it bets on, why it might hold after costs, how it fails")
        },
        "required": ["name", "script", "params_toml", "explanation"],
        "additionalProperties": false
    })
}

/// The draft in a Messages API reply, or why there is none.
pub(crate) fn parse_reply(status: u16, body: &str) -> Result<Draft, DraftError> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| DraftError::Decode(e.to_string()))?;
    if !(200..300).contains(&status) {
        let message = v["error"]["message"]
            .as_str()
            .unwrap_or("no message")
            .to_owned();
        return Err(DraftError::Api { status, message });
    }
    match v["stop_reason"].as_str() {
        Some("refusal") => {
            let why = v["stop_details"]["category"]
                .as_str()
                .unwrap_or("no category given");
            return Err(DraftError::Refused(why.to_owned()));
        }
        Some("max_tokens") => return Err(DraftError::Truncated),
        _ => {}
    }
    let text: String = v["content"]
        .as_array()
        .ok_or_else(|| DraftError::Decode("no content".into()))?
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    let mut draft: Draft =
        serde_json::from_str(&text).map_err(|e| DraftError::Decode(format!("draft: {e}")))?;
    draft.name = file_name(&draft.name);
    Ok(draft)
}

/// `draft_` + the name reduced to what a script file may be called.
fn file_name(name: &str) -> String {
    let clean: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let clean = clean.trim_matches('_');
    let clean = if clean.is_empty() { "strategy" } else { clean };
    format!("draft_{clean}").chars().take(64).collect()
}

/// Asks the model for a draft.
pub(crate) async fn draft(req: &DraftRequest, model: &str) -> Result<Draft, DraftError> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .ok_or(DraftError::NoKey)?;
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| DraftError::Http(e.to_string()))?;
    let reply = client
        .post(API_URL)
        .header("x-api-key", key)
        .header("anthropic-version", API_VERSION)
        .header("anthropic-beta", FALLBACK_BETA)
        .header("content-type", "application/json")
        .body(request_body(req, model).to_string())
        .send()
        .await
        .map_err(|e| DraftError::Http(e.to_string()))?;
    let status = reply.status().as_u16();
    let body = reply
        .text()
        .await
        .map_err(|e| DraftError::Http(e.to_string()))?;
    parse_reply(status, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> DraftRequest {
        DraftRequest {
            description: "buy when the 20-day average crosses above the 50-day".into(),
            instruments: vec!["etoro:AAPL.RTH".into()],
            interval: "1d".into(),
            revise: None,
        }
    }

    /// The request carries the model, adaptive thinking at high effort, the
    /// schema-constrained output, the default fallback, the reference, and
    /// the user's description with its instruments and interval.
    #[test]
    fn the_request_asks_for_a_schema_bound_draft_with_fallback() {
        let b = request_body(&req(), DEFAULT_MODEL);
        assert_eq!(b["model"], "claude-opus-5-5");
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert_eq!(b["output_config"]["effort"], "high");
        assert_eq!(b["output_config"]["format"]["type"], "json_schema");
        assert_eq!(
            b["output_config"]["format"]["schema"]["additionalProperties"],
            false
        );
        assert_eq!(b["fallbacks"], "default");
        assert!(
            b["system"]
                .as_str()
                .unwrap()
                .contains("# Writing a script strategy")
        );
        let user = b["messages"][0]["content"].as_str().unwrap();
        assert!(user.contains("20-day") && user.contains("etoro:AAPL.RTH") && user.contains("1d"));
        assert!(
            b.get("temperature").is_none(),
            "sampling params are rejected"
        );
    }

    /// A reply: a thinking block (empty, display omitted) then the JSON text.
    #[test]
    fn a_reply_yields_the_draft_with_a_safe_file_name() {
        let draft = serde_json::json!({
            "name": "MA Cross / 20-50!",
            "script": "fn on_candle(ctx, c) {}",
            "params_toml": "coin = \"etoro:AAPL.RTH\"",
            "explanation": "trend"
        });
        let body = serde_json::json!({
            "stop_reason": "end_turn",
            "content": [
                { "type": "thinking", "thinking": "" },
                { "type": "text", "text": draft.to_string() }
            ]
        });
        let d = parse_reply(200, &body.to_string()).unwrap();
        assert_eq!(d.name, "draft_ma_cross___20_50");
        assert_eq!(d.script, "fn on_candle(ctx, c) {}");
    }

    #[test]
    fn refusals_truncation_and_api_errors_are_errors() {
        let refused = serde_json::json!({
            "stop_reason": "refusal", "stop_details": { "category": "cyber" }, "content": []
        });
        assert!(matches!(
            parse_reply(200, &refused.to_string()),
            Err(DraftError::Refused(c)) if c == "cyber"
        ));
        let cut = serde_json::json!({ "stop_reason": "max_tokens", "content": [] });
        assert!(matches!(
            parse_reply(200, &cut.to_string()),
            Err(DraftError::Truncated)
        ));
        let err = serde_json::json!({ "error": { "type": "authentication_error", "message": "invalid x-api-key" } });
        assert!(matches!(
            parse_reply(401, &err.to_string()),
            Err(DraftError::Api { status: 401, message }) if message == "invalid x-api-key"
        ));
    }
}
