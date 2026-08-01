//! Exchange time.
//!
//! Every venue stamps in milliseconds since the Unix epoch, so that is the storage form.
//! A newtype rather than a bare `i64` because mixing up an exchange timestamp with a local
//! one is a class of bug that costs real money in reconciliation.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub const ZERO: Self = Self(0);

    pub const fn from_millis(ms: i64) -> Self {
        Self(ms)
    }

    pub const fn millis(self) -> i64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Signed age against a reference clock; negative means the stamp is in the future,
    /// which happens routinely when local time drifts ahead of the venue.
    pub const fn age_millis_from(self, now: Timestamp) -> i64 {
        now.0 - self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_is_signed_so_clock_drift_stays_visible() {
        let stamp = Timestamp::from_millis(1_000);
        assert_eq!(stamp.age_millis_from(Timestamp::from_millis(1_500)), 500);
        assert_eq!(stamp.age_millis_from(Timestamp::from_millis(400)), -600);
    }
}
