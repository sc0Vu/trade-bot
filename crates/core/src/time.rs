use serde::{Deserialize, Serialize};
use std::fmt;

/// UTC instant in milliseconds since the Unix epoch.
///
/// Same unit Binance uses, so there is no conversion loss on the wire.
/// Totally ordered, so the engine can reject an event stream that goes
/// backwards in time (a lookahead or replay bug).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Wraps milliseconds since the Unix epoch (UTC).
    pub const fn from_millis(ms: i64) -> Self {
        Self(ms)
    }

    /// Milliseconds since the Unix epoch (UTC).
    pub const fn as_millis(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}
