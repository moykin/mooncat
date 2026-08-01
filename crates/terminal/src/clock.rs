//! Wall-clock formatting for the chrome.
//!
//! Hand-rolled rather than pulling a date library in: the terminal needs a clock in the
//! header and a stamp on log lines, and nothing else. Everything here is UTC — a trading
//! log that silently follows the laptop's timezone is a log you cannot compare with the
//! venue's.

use std::time::{SystemTime, UNIX_EPOCH};

const MS_PER_DAY: i64 = 86_400_000;

/// Milliseconds since the Unix epoch, now.
pub fn now_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// `HH:MM:SS` in UTC.
pub fn hms(millis: i64) -> String {
    let (h, m, s, _) = parts(millis);
    format!("{h:02}:{m:02}:{s:02}")
}

/// `HH:MM:SS.mmm` in UTC, for log lines where ordering within a second matters.
pub fn hms_millis(millis: i64) -> String {
    let (h, m, s, ms) = parts(millis);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// Split a Unix millisecond stamp into UTC time-of-day parts.
///
/// `rem_euclid` rather than `%` so stamps before the epoch — which a venue should never
/// send, but a corrupt frame might — wrap into a valid time instead of producing negatives.
fn parts(millis: i64) -> (i64, i64, i64, i64) {
    let of_day = millis.rem_euclid(MS_PER_DAY);
    let seconds = of_day / 1_000;
    (seconds / 3_600, (seconds / 60) % 60, seconds % 60, of_day % 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_time_of_day_in_utc() {
        // 2023-11-14T22:13:20Z
        assert_eq!(hms(1_700_000_000_000), "22:13:20");
        assert_eq!(hms_millis(1_700_000_000_123), "22:13:20.123");
    }

    #[test]
    fn midnight_and_the_last_second_of_the_day_both_render() {
        assert_eq!(hms(0), "00:00:00");
        assert_eq!(hms(MS_PER_DAY - 1_000), "23:59:59");
        assert_eq!(hms(MS_PER_DAY), "00:00:00", "the next day starts over");
    }

    #[test]
    fn a_stamp_before_the_epoch_wraps_rather_than_going_negative() {
        // A corrupt frame must not put `-1:-3:-20` in the log.
        let rendered = hms(-1_000);
        assert_eq!(rendered, "23:59:59");
        assert!(!rendered.contains('-'));
    }

    #[test]
    fn the_clock_advances() {
        let first = now_millis();
        assert!(first > 1_700_000_000_000, "a plausible wall clock");
        assert!(now_millis() >= first);
    }
}
