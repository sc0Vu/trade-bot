//! Keeping a share of the account's quote cash in Binance Simple Earn.
//!
//! With `[earn] enabled`, a live run keeps `ratio` of its quote cash (the
//! spot wallet's free balance of the quote asset plus what it holds in
//! Simple Earn) in an Earn product, and the rest in spot for trading. The
//! share counts the account's whole quote cash, money the bot didn't put
//! there included: Binance can't tell them apart. [`EarnSweep`] runs from
//! the broker's settle every `every` (an hour by default) while no buy of
//! ours is working, reads the spot balance and the Earn positions afresh,
//! and moves the difference ([`plan`]), outside a dead band of `band` of
//! the fund so it doesn't churn:
//!
//! - **Flexible** (`/sapi/v1/simple-earn/flexible/*`): tops up and redeems.
//!   Binance doesn't say how fast a redemption reaches spot, so a strategy
//!   never counts on it: its buys spend only spot cash, and the paper
//!   broker models the share as unspendable likewise.
//! - **Locked** (`/sapi/v1/simple-earn/locked/*`, a `days` term): only tops
//!   up; never redeemed early (that forfeits interest). Matured positions
//!   come back to spot by themselves (`redeemTo` SPOT, no auto-renew), and
//!   while the fund shrinks the locked share can exceed `ratio`.
//!
//! Everything is read fresh on each sweep, so a request that failed with an
//! unknown outcome (HTTP 5xx: Binance says it may have succeeded) is
//! simply seen in the next balances, never blindly retried. Simple Earn is
//! on the live host only (demo and testnet answer 404); the config refuses
//! it elsewhere. A failed sweep is a warning: the cash is safe either way.

use super::{Method, SignedClient};
use crate::ExchangeError;
use bot_core::{Asset, Decimal};
use serde_json::Value;
use std::str::FromStr;
use std::time::{Duration, Instant};

/// Which Simple Earn product holds the share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EarnProduct {
    Flexible,
    /// A locked product of this many days.
    Locked {
        days: u32,
    },
}

/// `[earn]` from the config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EarnConfig {
    /// Share of the quote cash kept in Earn, in (0, 1).
    pub ratio: Decimal,
    /// Dead band: move only when off by more than this share of the fund.
    pub band: Decimal,
    pub product: EarnProduct,
    /// Time between sweeps.
    pub every: Duration,
}

/// What a sweep does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Subscribe(Decimal),
    Redeem(Decimal),
    Nothing,
}

/// The move that brings `earning` to `ratio` of `spot + earning`, if it's
/// off by more than `band` of that and the move is allowed: a subscription
/// of at least `min_purchase`, never more than the spot cash; a redemption
/// only when `can_redeem` (flexible). Amounts truncated to 8 decimals,
/// Binance's precision.
pub fn plan(
    spot: Decimal,
    earning: Decimal,
    cfg: &EarnConfig,
    min_purchase: Decimal,
    can_redeem: bool,
) -> Action {
    let fund = spot + earning;
    let diff = cfg.ratio * fund - earning;
    let dead = cfg.band * fund;
    if diff > dead {
        let amount = diff.min(spot).trunc_with_scale(8);
        if amount >= min_purchase && amount.is_sign_positive() && !amount.is_zero() {
            return Action::Subscribe(amount);
        }
    } else if -diff > dead && can_redeem {
        let amount = (-diff).trunc_with_scale(8);
        if !amount.is_zero() {
            return Action::Redeem(amount);
        }
    }
    Action::Nothing
}

/// The product a sweep moves money into, found on the first sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    /// `productId` (flexible) or `projectId` (locked).
    id: String,
    min_purchase: Decimal,
}

/// See the module docs.
pub struct EarnSweep {
    cfg: EarnConfig,
    asset: Asset,
    target: Option<Target>,
    last: Option<Instant>,
}

fn dec(v: &Value, what: &str) -> Result<Decimal, ExchangeError> {
    let s = v
        .as_str()
        .ok_or_else(|| ExchangeError::Decode(format!("simple earn: {what}")))?;
    Decimal::from_str(s).map_err(|e| ExchangeError::Decode(format!("simple earn: {what}: {e}")))
}

fn rows(v: &Value) -> Result<&Vec<Value>, ExchangeError> {
    v["rows"]
        .as_array()
        .ok_or_else(|| ExchangeError::Decode("simple earn: rows".into()))
}

impl EarnSweep {
    /// Keeps `cfg.ratio` of the `asset` cash (the universe's quote) in Earn.
    pub fn new(cfg: EarnConfig, asset: Asset) -> Self {
        Self {
            cfg,
            asset,
            target: None,
            last: None,
        }
    }

    /// Whether a sweep is due.
    pub fn due(&self) -> bool {
        self.last.is_none_or(|t| t.elapsed() >= self.cfg.every)
    }

    fn asset_param(&self) -> Vec<(&'static str, String)> {
        vec![("asset", self.asset.to_string()), ("size", "100".into())]
    }

    async fn spot_free(&self, client: &SignedClient) -> Result<Decimal, ExchangeError> {
        let acct = client
            .signed(
                Method::Get,
                "/api/v3/account",
                &[("omitZeroBalances", "true".into())],
            )
            .await?;
        let balances = acct["balances"]
            .as_array()
            .ok_or_else(|| ExchangeError::Decode("account: balances".into()))?;
        match balances
            .iter()
            .find(|b| b["asset"].as_str() == Some(self.asset.as_str()))
        {
            Some(b) => dec(&b["free"], "free"),
            None => Ok(Decimal::ZERO),
        }
    }

    /// (flexible amount, locked amount) held in Earn for the asset.
    async fn earning(&self, client: &SignedClient) -> Result<(Decimal, Decimal), ExchangeError> {
        let p = self.asset_param();
        let flex = client
            .signed(Method::Get, "/sapi/v1/simple-earn/flexible/position", &p)
            .await?;
        let flexible = rows(&flex)?
            .iter()
            .map(|r| dec(&r["totalAmount"], "totalAmount"))
            .sum::<Result<Decimal, _>>()?;
        let locked = match self.cfg.product {
            EarnProduct::Flexible => Decimal::ZERO,
            EarnProduct::Locked { .. } => {
                let v = client
                    .signed(Method::Get, "/sapi/v1/simple-earn/locked/position", &p)
                    .await?;
                rows(&v)?
                    .iter()
                    .map(|r| dec(&r["amount"], "amount"))
                    .sum::<Result<Decimal, _>>()?
            }
        };
        Ok((flexible, locked))
    }

    async fn target(&mut self, client: &SignedClient) -> Result<Target, ExchangeError> {
        if let Some(t) = &self.target {
            return Ok(t.clone());
        }
        let p = self.asset_param();
        let t = match self.cfg.product {
            EarnProduct::Flexible => {
                let v = client
                    .signed(Method::Get, "/sapi/v1/simple-earn/flexible/list", &p)
                    .await?;
                let row = rows(&v)?
                    .iter()
                    .find(|r| r["canPurchase"].as_bool() == Some(true))
                    .ok_or_else(|| {
                        ExchangeError::Decode(format!(
                            "no flexible product open for {}",
                            self.asset
                        ))
                    })?;
                Target {
                    id: row["productId"]
                        .as_str()
                        .ok_or_else(|| ExchangeError::Decode("productId".into()))?
                        .to_owned(),
                    min_purchase: dec(&row["minPurchaseAmount"], "minPurchaseAmount")?,
                }
            }
            EarnProduct::Locked { days } => {
                let v = client
                    .signed(Method::Get, "/sapi/v1/simple-earn/locked/list", &p)
                    .await?;
                let row = rows(&v)?
                    .iter()
                    .find(|r| {
                        r["detail"]["duration"].as_u64() == Some(u64::from(days))
                            && r["detail"]["isSoldOut"].as_bool() != Some(true)
                    })
                    .ok_or_else(|| {
                        ExchangeError::Decode(format!(
                            "no {days}-day locked product open for {}",
                            self.asset
                        ))
                    })?;
                Target {
                    id: row["projectId"]
                        .as_str()
                        .ok_or_else(|| ExchangeError::Decode("projectId".into()))?
                        .to_owned(),
                    min_purchase: dec(&row["quota"]["minimum"], "quota.minimum")?,
                }
            }
        };
        self.target = Some(t.clone());
        Ok(t)
    }

    /// One sweep: read, plan, move. Returns what it did.
    pub async fn sweep(&mut self, client: &SignedClient) -> Result<Action, ExchangeError> {
        self.last = Some(Instant::now());
        let target = self.target(client).await?;
        let spot = self.spot_free(client).await?;
        let (flexible, locked) = self.earning(client).await?;
        let flexible_product = self.cfg.product == EarnProduct::Flexible;
        let action = plan(
            spot,
            flexible + locked,
            &self.cfg,
            target.min_purchase,
            flexible_product,
        );
        // A redemption can't exceed what the flexible product holds.
        let action = match action {
            Action::Redeem(a) => Action::Redeem(a.min(flexible)),
            other => other,
        };
        match &action {
            Action::Subscribe(amount) => {
                let (path, id_key, mut params) = match self.cfg.product {
                    EarnProduct::Flexible => (
                        "/sapi/v1/simple-earn/flexible/subscribe",
                        "productId",
                        vec![("autoSubscribe", "false".to_owned())],
                    ),
                    EarnProduct::Locked { .. } => (
                        "/sapi/v1/simple-earn/locked/subscribe",
                        "projectId",
                        vec![
                            ("autoSubscribe", "false".to_owned()),
                            ("redeemTo", "SPOT".to_owned()),
                        ],
                    ),
                };
                params.push((id_key, target.id.clone()));
                params.push(("amount", amount.normalize().to_string()));
                params.push(("sourceAccount", "SPOT".to_owned()));
                client.signed(Method::Post, path, &params).await?;
                tracing::info!(asset = %self.asset, "simple earn: subscribed");
                tracing::debug!(asset = %self.asset, %amount, "simple earn: subscribe amount");
            }
            Action::Redeem(amount) => {
                client
                    .signed(
                        Method::Post,
                        "/sapi/v1/simple-earn/flexible/redeem",
                        &[
                            ("productId", target.id.clone()),
                            ("amount", amount.normalize().to_string()),
                            ("destAccount", "SPOT".to_owned()),
                        ],
                    )
                    .await?;
                tracing::info!(asset = %self.asset, "simple earn: redeemed");
                tracing::debug!(asset = %self.asset, %amount, "simple earn: redeem amount");
            }
            Action::Nothing => {}
        }
        Ok(action)
    }
}

#[cfg(test)]
mod tests {
    use super::super::signed::fake::{FakeTransport, FixedClock, param};
    use super::*;
    use crate::binance::Credentials;
    use rust_decimal_macros::dec;

    fn cfg(product: EarnProduct) -> EarnConfig {
        EarnConfig {
            ratio: dec!(0.1),
            band: dec!(0.01),
            product,
            every: Duration::ZERO,
        }
    }

    /// Fund 1000, ratio 10%, band 1% (10). Nothing in Earn: subscribe
    /// 100. 95 in Earn: off by 5, inside the band: nothing. 50 in Earn
    /// (fund 1000): subscribe 50. 200 in Earn: redeem 100 (flexible), not
    /// locked. A top-up under the product minimum, or with no spot cash,
    /// is nothing.
    #[test]
    fn plans_the_move_back_to_the_ratio() {
        let f = cfg(EarnProduct::Flexible);
        assert_eq!(
            plan(dec!(1000), dec!(0), &f, dec!(1), true),
            Action::Subscribe(dec!(100))
        );
        assert_eq!(
            plan(dec!(905), dec!(95), &f, dec!(1), true),
            Action::Nothing
        );
        assert_eq!(
            plan(dec!(950), dec!(50), &f, dec!(1), true),
            Action::Subscribe(dec!(50))
        );
        assert_eq!(
            plan(dec!(800), dec!(200), &f, dec!(1), true),
            Action::Redeem(dec!(100))
        );
        assert_eq!(
            plan(dec!(800), dec!(200), &f, dec!(1), false),
            Action::Nothing
        );
        assert_eq!(
            plan(dec!(1000), dec!(0), &f, dec!(150), true),
            Action::Nothing
        );
        assert_eq!(plan(dec!(0), dec!(0), &f, dec!(1), true), Action::Nothing);
        // 1/3 of a fund of 1 truncates to 8 decimals, never rounds up.
        let third = EarnConfig {
            ratio: dec!(1) / dec!(3),
            band: dec!(0),
            ..f
        };
        assert_eq!(
            plan(dec!(1), dec!(0), &third, dec!(0), true),
            Action::Subscribe(dec!(0.33333333))
        );
    }

    fn client(t: &FakeTransport) -> SignedClient {
        SignedClient::new(
            "https://api.binance.com",
            Box::new(FakeTransport {
                sent: t.sent.clone(),
                replies: t.replies.clone(),
            }),
            Credentials::new("k", "s"),
            Box::new(FixedClock(1_000)),
        )
    }

    /// Flexible: find the product, read 950 USDT free and 50 in Earn
    /// (fund 1000), subscribe 50 to USDT001 from spot. The next sweep
    /// reuses the product id (no list call).
    #[tokio::test]
    async fn a_flexible_sweep_subscribes_the_difference() {
        let t = FakeTransport::default();
        t.reply(200, r#"{"rows":[{"productId":"USDT001","canPurchase":true,"minPurchaseAmount":"0.1"}],"total":1}"#);
        t.reply(
            200,
            r#"{"balances":[{"asset":"USDT","free":"950","locked":"0"}],"canTrade":true}"#,
        );
        t.reply(
            200,
            r#"{"rows":[{"totalAmount":"50","productId":"USDT001"}],"total":1}"#,
        );
        t.reply(200, r#"{"purchaseId":1,"success":true}"#);
        let c = client(&t);
        let mut s = EarnSweep::new(cfg(EarnProduct::Flexible), Asset::new("USDT").unwrap());
        assert_eq!(s.sweep(&c).await.unwrap(), Action::Subscribe(dec!(50)));
        let sent = t.sent();
        let sub = sent.last().unwrap();
        assert_eq!(sub.path, "/sapi/v1/simple-earn/flexible/subscribe");
        assert_eq!(param(&sub.query, "productId"), Some("USDT001"));
        assert_eq!(param(&sub.query, "amount"), Some("50"));
        assert_eq!(param(&sub.query, "sourceAccount"), Some("SPOT"));
        assert!(param(&sub.query, "signature").is_some());

        t.reply(
            200,
            r#"{"balances":[{"asset":"USDT","free":"900","locked":"0"}],"canTrade":true}"#,
        );
        t.reply(
            200,
            r#"{"rows":[{"totalAmount":"100","productId":"USDT001"}],"total":1}"#,
        );
        assert_eq!(s.sweep(&c).await.unwrap(), Action::Nothing);
        assert!(
            t.sent()[4..]
                .iter()
                .all(|r| r.path != "/sapi/v1/simple-earn/flexible/list")
        );
    }

    /// Locked 30 days: picks the 30-day project, counts flexible and
    /// locked holdings, subscribes to the lock with redeemTo SPOT; over
    /// the ratio it never redeems.
    #[tokio::test]
    async fn a_locked_sweep_tops_up_and_never_redeems() {
        let t = FakeTransport::default();
        t.reply(200, r#"{"rows":[{"projectId":"USDT*7","detail":{"duration":7,"isSoldOut":false},"quota":{"minimum":"1"}},{"projectId":"USDT*30","detail":{"duration":30,"isSoldOut":false},"quota":{"minimum":"1"}}],"total":2}"#);
        t.reply(
            200,
            r#"{"balances":[{"asset":"USDT","free":"900","locked":"0"}],"canTrade":true}"#,
        );
        t.reply(200, r#"{"rows":[],"total":0}"#);
        t.reply(
            200,
            r#"{"rows":[{"amount":"40","projectId":"USDT*30"}],"total":1}"#,
        );
        t.reply(200, r#"{"purchaseId":1,"positionId":"9","success":true}"#);
        let c = client(&t);
        let mut s = EarnSweep::new(
            cfg(EarnProduct::Locked { days: 30 }),
            Asset::new("USDT").unwrap(),
        );
        assert_eq!(s.sweep(&c).await.unwrap(), Action::Subscribe(dec!(54)));
        let sub = t.sent().last().unwrap().clone();
        assert_eq!(sub.path, "/sapi/v1/simple-earn/locked/subscribe");
        // URL-encoded (`*` → `%2A`), and signed as sent.
        assert_eq!(param(&sub.query, "projectId"), Some("USDT%2A30"));
        assert_eq!(param(&sub.query, "redeemTo"), Some("SPOT"));

        t.reply(
            200,
            r#"{"balances":[{"asset":"USDT","free":"500","locked":"0"}],"canTrade":true}"#,
        );
        t.reply(200, r#"{"rows":[],"total":0}"#);
        t.reply(
            200,
            r#"{"rows":[{"amount":"300","projectId":"USDT*30"}],"total":1}"#,
        );
        assert_eq!(s.sweep(&c).await.unwrap(), Action::Nothing);
    }
}
