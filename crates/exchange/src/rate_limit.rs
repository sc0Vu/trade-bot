//! Client-side rate limiting: a sliding-window log per limit.
//!
//! Venues count requests in windows: Binance allows so much request weight
//! per IP per minute and so many orders per account per 10 seconds and per
//! day, and bans an IP that keeps exceeding them. The limiter makes a
//! request wait until it fits every limit it is counted against.
//!
//! Why a sliding-window log rather than a leaky or token bucket: Binance's
//! windows are fixed (the minute resets on the minute). A bucket holding
//! the full budget can spend it at :59 and refill by :00 and spend again,
//! close to twice the budget inside one venue minute; making it safe means
//! halving its capacity. The log keeps `(time, cost)` for the last window
//! and admits a request only if the window's total plus the request stays
//! within budget, so *every* span of that length is within budget, fixed
//! ones included. Requests are few (a handful per candle), so the log stays
//! tiny.
//!
//! The venue's own count wins when it is higher (another program on the
//! same IP, or a restart that forgot recent requests): [`observe_used`]
//! raises the window's total to what the venue reported. A `Retry-After`
//! from the venue blocks every request until it passes ([`block_for`]).
//!
//! [`observe_used`]: RateLimiter::observe_used
//! [`block_for`]: RateLimiter::block_for

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;
use thiserror::Error;
use tokio::time::Instant;

/// What a limit counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Request weight (Binance `REQUEST_WEIGHT`).
    Weight,
    /// Orders placed (Binance `ORDERS`).
    Orders,
}

/// One limit: at most `budget` of `kind` in any `window`. The budget is
/// what the limiter allows, already below the venue's own limit if headroom
/// is wanted (see [`Limit::with_headroom`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub kind: Kind,
    pub window: Duration,
    pub budget: u64,
}

impl Limit {
    /// A limit allowing `fraction` (0..=1) of the venue's `limit`, rounded
    /// down, so other programs sharing the IP or account keep some room.
    pub fn with_headroom(kind: Kind, window: Duration, limit: u64, fraction: f64) -> Self {
        let budget = ((limit as f64) * fraction.clamp(0.0, 1.0)).floor() as u64;
        Self {
            kind,
            window,
            budget,
        }
    }
}

/// A request can never fit: its cost alone exceeds a limit's budget.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("request costs {cost} {kind:?} but the budget is {budget} per {window:?}")]
pub struct TooLarge {
    pub kind: Kind,
    pub cost: u64,
    pub budget: u64,
    pub window: Duration,
}

struct Window {
    limit: Limit,
    /// Oldest first.
    log: VecDeque<(Instant, u64)>,
    total: u64,
}

impl Window {
    fn expire(&mut self, now: Instant) {
        while let Some(&(t, c)) = self.log.front() {
            if now.duration_since(t) < self.limit.window {
                break;
            }
            self.log.pop_front();
            self.total -= c;
        }
    }

    /// When `cost` more would fit, given the log as it stands.
    fn fits_at(&self, now: Instant, cost: u64) -> Instant {
        let over = (self.total + cost).saturating_sub(self.limit.budget);
        if over == 0 {
            return now;
        }
        let mut freed = 0;
        for &(t, c) in &self.log {
            freed += c;
            if freed >= over {
                return t + self.limit.window;
            }
        }
        // Unreachable when cost <= budget: the whole log frees enough.
        now + self.limit.window
    }
}

struct State {
    windows: Vec<Window>,
    blocked_until: Option<Instant>,
}

/// Admits requests within every configured limit; see the module docs.
/// Share one per venue host (one IP budget) across all its clients.
pub struct RateLimiter {
    state: Mutex<State>,
}

impl RateLimiter {
    pub fn new(limits: impl IntoIterator<Item = Limit>) -> Self {
        Self {
            state: Mutex::new(State {
                windows: limits
                    .into_iter()
                    .map(|limit| Window {
                        limit,
                        log: VecDeque::new(),
                        total: 0,
                    })
                    .collect(),
                blocked_until: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding the lock leaves plain counters behind;
        // carrying on with them is safe.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Waits until a request costing `weight` and placing `orders` orders
    /// fits every limit, records it, and returns how long it waited.
    pub async fn acquire(&self, weight: u64, orders: u64) -> Result<Duration, TooLarge> {
        let cost = |kind: Kind| match kind {
            Kind::Weight => weight,
            Kind::Orders => orders,
        };
        let started = Instant::now();
        loop {
            let wake = {
                let mut s = self.lock();
                let now = Instant::now();
                let mut wake = s.blocked_until.filter(|b| *b > now).unwrap_or(now);
                for w in &mut s.windows {
                    let c = cost(w.limit.kind);
                    if c > w.limit.budget {
                        return Err(TooLarge {
                            kind: w.limit.kind,
                            cost: c,
                            budget: w.limit.budget,
                            window: w.limit.window,
                        });
                    }
                    w.expire(now);
                    wake = wake.max(w.fits_at(now, c));
                }
                if wake <= now {
                    for w in &mut s.windows {
                        let c = cost(w.limit.kind);
                        if c > 0 {
                            w.log.push_back((now, c));
                            w.total += c;
                        }
                    }
                    return Ok(now.duration_since(started));
                }
                wake
            };
            tokio::time::sleep_until(wake).await;
        }
    }

    /// The venue reports `used` of `kind` in its current `window`. If that
    /// is more than this limiter has counted, the difference is recorded
    /// now: someone else is spending the same budget.
    pub fn observe_used(&self, kind: Kind, window: Duration, used: u64) {
        let mut s = self.lock();
        let now = Instant::now();
        for w in s
            .windows
            .iter_mut()
            .filter(|w| w.limit.kind == kind && w.limit.window == window)
        {
            w.expire(now);
            if used > w.total {
                w.log.push_back((now, used - w.total));
                w.total = used;
            }
        }
    }

    /// Holds every request for `d` (the venue's `Retry-After`).
    pub fn block_for(&self, d: Duration) {
        let mut s = self.lock();
        let until = Instant::now() + d;
        s.blocked_until = Some(s.blocked_until.map_or(until, |b| b.max(until)));
    }

    /// What has been spent of `kind` in the last `window`, by this
    /// limiter's count (venue reports included).
    pub fn used(&self, kind: Kind, window: Duration) -> Option<u64> {
        let mut s = self.lock();
        let now = Instant::now();
        s.windows
            .iter_mut()
            .find(|w| w.limit.kind == kind && w.limit.window == window)
            .map(|w| {
                w.expire(now);
                w.total
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    fn weight(budget: u64) -> Limit {
        Limit {
            kind: Kind::Weight,
            window: MIN,
            budget,
        }
    }

    #[test]
    fn headroom_rounds_down() {
        let l = Limit::with_headroom(Kind::Weight, MIN, 6000, 0.8);
        assert_eq!(l.budget, 4800);
        assert_eq!(Limit::with_headroom(Kind::Orders, MIN, 7, 0.5).budget, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn within_budget_does_not_wait() {
        let l = RateLimiter::new([weight(10)]);
        assert_eq!(l.acquire(6, 0).await, Ok(Duration::ZERO));
        assert_eq!(l.acquire(4, 0).await, Ok(Duration::ZERO));
        assert_eq!(l.used(Kind::Weight, MIN), Some(10));
    }

    /// 6 at t=0, 6 more would make 12 > 10: it waits until the first 6
    /// leave the window, at t=60 s exactly.
    #[tokio::test(start_paused = true)]
    async fn over_budget_waits_for_the_window_to_slide() {
        let l = RateLimiter::new([weight(10)]);
        l.acquire(6, 0).await.unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(l.acquire(6, 0).await, Ok(Duration::from_secs(50)));
        assert_eq!(l.used(Kind::Weight, MIN), Some(6));
    }

    /// Only as much of the log as needed must expire: with 3 at t=0, 3 at
    /// t=20 and 3 at t=40, a cost of 4 (budget 10) needs 2 freed, so it
    /// waits for the first entry only, until t=60.
    #[tokio::test(start_paused = true)]
    async fn waits_only_for_enough_to_expire() {
        let l = RateLimiter::new([weight(10)]);
        for _ in 0..3 {
            l.acquire(3, 0).await.unwrap();
            tokio::time::advance(Duration::from_secs(20)).await;
        }
        // Now t=60: the t=0 entry has just left; 6 used, 4 fits at once.
        assert_eq!(l.acquire(4, 0).await, Ok(Duration::ZERO));
        // 10 used; 3 more waits for the t=20 entry, 20 s away.
        assert_eq!(l.acquire(3, 0).await, Ok(Duration::from_secs(20)));
    }

    /// The property that matters against Binance's fixed minutes: however
    /// requests arrive, no 60 s span (so no fixed minute) holds more than
    /// the budget.
    #[tokio::test(start_paused = true)]
    async fn no_window_ever_exceeds_the_budget() {
        let l = RateLimiter::new([weight(100)]);
        let start = Instant::now();
        let mut sent: Vec<(Duration, u64)> = Vec::new();
        // A fixed pseudo-random pattern of costs and gaps.
        let mut x: u64 = 7;
        for _ in 0..400 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let cost = 1 + (x >> 33) % 20;
            let gap = Duration::from_millis((x >> 45) % 3000);
            tokio::time::advance(gap).await;
            l.acquire(cost, 0).await.unwrap();
            sent.push((Instant::now().duration_since(start), cost));
        }
        for (i, (t0, _)) in sent.iter().enumerate() {
            let in_window: u64 = sent[i..]
                .iter()
                .take_while(|(t, _)| *t < *t0 + MIN)
                .map(|(_, c)| c)
                .sum();
            assert!(in_window <= 100, "{in_window} in the 60 s from {t0:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_larger_than_the_budget_is_refused() {
        let l = RateLimiter::new([weight(10)]);
        assert!(l.acquire(11, 0).await.is_err());
        assert_eq!(l.used(Kind::Weight, MIN), Some(0), "nothing recorded");
    }

    /// Orders count only against order limits, weight only against weight.
    #[tokio::test(start_paused = true)]
    async fn each_kind_has_its_own_limits() {
        let tens = Duration::from_secs(10);
        let l = RateLimiter::new([
            weight(1000),
            Limit {
                kind: Kind::Orders,
                window: tens,
                budget: 2,
            },
        ]);
        l.acquire(1, 1).await.unwrap();
        l.acquire(1, 1).await.unwrap();
        // A non-order request isn't held back by the full order window...
        assert_eq!(l.acquire(20, 0).await, Ok(Duration::ZERO));
        // ...but a third order waits for the first to age out.
        assert_eq!(l.acquire(1, 1).await, Ok(tens));
    }

    /// The venue says 9 of 10 are used though we spent 2: the rest belongs
    /// to someone else, and the next 2 must wait.
    #[tokio::test(start_paused = true)]
    async fn the_venues_higher_count_wins() {
        let l = RateLimiter::new([weight(10)]);
        l.acquire(2, 0).await.unwrap();
        l.observe_used(Kind::Weight, MIN, 9);
        assert_eq!(l.used(Kind::Weight, MIN), Some(9));
        assert!(l.acquire(2, 0).await.unwrap() > Duration::ZERO);
        // A lower venue count changes nothing.
        let l = RateLimiter::new([weight(10)]);
        l.acquire(5, 0).await.unwrap();
        l.observe_used(Kind::Weight, MIN, 1);
        assert_eq!(l.used(Kind::Weight, MIN), Some(5));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_holds_every_request() {
        let l = RateLimiter::new([weight(10)]);
        l.block_for(Duration::from_secs(30));
        assert_eq!(l.acquire(1, 0).await, Ok(Duration::from_secs(30)));
        assert_eq!(
            l.acquire(1, 0).await,
            Ok(Duration::ZERO),
            "block has passed"
        );
    }
}
