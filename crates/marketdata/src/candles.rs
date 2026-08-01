//! Building candles from the trade tape.
//!
//! Not taken from the venue's own kline stream, for two reasons. A scalping chart wants
//! sub-minute buckets and Binance's smallest published interval is a minute; and a candle
//! built from the same prints the tape shows is a candle that cannot disagree with the tape.
//!
//! Nothing here fabricates data. A minute with no trades produces no candle rather than a
//! flat one — a gap on the chart is the truth, and inventing a bar to fill it hides exactly
//! the illiquidity a scalper is looking for.

use domain::{Candle, PublicTrade, Symbol, Timestamp};
use std::collections::{HashMap, VecDeque};

/// Candles retained per instrument.
pub const DEFAULT_CAPACITY: usize = 1_200;

/// What feeding a trade in produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// The forming candle changed and consumers should redraw it.
    Formed(Candle),
    /// A bucket ended. The closed candle comes first, the newly opened one second.
    Closed { closed: Candle, opened: Candle },
    /// Older than the candle currently forming; ignored.
    Ignored,
}

/// Live candle series, one per instrument.
#[derive(Debug)]
pub struct CandleSet {
    interval_ms: i64,
    capacity: usize,
    series: HashMap<String, VecDeque<Candle>>,
}

impl CandleSet {
    /// `interval_ms` must be positive; anything else would put every trade in one bucket.
    pub fn new(interval_ms: i64, capacity: usize) -> Self {
        Self { interval_ms: interval_ms.max(1), capacity: capacity.max(1), series: HashMap::new() }
    }

    pub fn interval_ms(&self) -> i64 {
        self.interval_ms
    }

    /// Fold one trade into its bucket.
    pub fn on_trade(&mut self, trade: &PublicTrade) -> Update {
        let open_time = self.bucket(trade.ts);
        let interval_ms = self.interval_ms;
        let capacity = self.capacity;
        let series = self.series.entry(trade.symbol.key()).or_default();

        match series.back().map(|c| c.open_time) {
            // A trade older than the forming candle would rewrite history that consumers
            // have already drawn. Venues replay prints after a reconnect; drop them.
            Some(current) if open_time < current => Update::Ignored,

            Some(current) if open_time == current => {
                let candle = series.back_mut().expect("checked above");
                candle.high = candle.high.max(trade.price);
                candle.low = candle.low.min(trade.price);
                candle.close = trade.price;
                candle.volume += trade.qty;
                Update::Formed(candle.clone())
            }

            _ => {
                let mut closed = None;
                if let Some(previous) = series.back_mut() {
                    previous.closed = true;
                    closed = Some(previous.clone());
                }

                let opened = open(trade, open_time, interval_ms);
                series.push_back(opened.clone());
                while series.len() > capacity {
                    series.pop_front();
                }

                match closed {
                    Some(closed) => Update::Closed { closed, opened },
                    None => Update::Formed(opened),
                }
            }
        }
    }

    /// Every candle for an instrument, oldest first.
    ///
    /// `DoubleEndedIterator` because the newest candle — the one still forming — is what a
    /// caller usually wants, and it lives at the back.
    pub fn iter(&self, key: &str) -> impl DoubleEndedIterator<Item = &Candle> {
        self.series.get(key).into_iter().flatten()
    }

    /// The candle currently forming, if any.
    pub fn forming(&self, key: &str) -> Option<&Candle> {
        self.series.get(key).and_then(|s| s.back())
    }

    pub fn len(&self, key: &str) -> usize {
        self.series.get(key).map_or(0, |s| s.len())
    }

    pub fn is_empty(&self, key: &str) -> bool {
        self.len(key) == 0
    }

    /// Instruments with at least one candle.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.series.keys()
    }

    /// Forget an instrument's history. Used when its book resyncs from scratch.
    pub fn clear(&mut self, symbol: &Symbol) {
        self.series.remove(&symbol.key());
    }

    fn bucket(&self, ts: Timestamp) -> Timestamp {
        Timestamp::from_millis(ts.millis() - ts.millis().rem_euclid(self.interval_ms))
    }
}

fn open(trade: &PublicTrade, open_time: Timestamp, interval_ms: i64) -> Candle {
    Candle {
        symbol: trade.symbol.clone(),
        open_time,
        interval_ms,
        open: trade.price,
        high: trade.price,
        low: trade.price,
        close: trade.price,
        volume: trade.qty,
        closed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{ExchangeId, MarketKind, Side};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn sym() -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::Spot, "BTCUSDT")
    }

    fn trade(ts: i64, price: Decimal, qty: Decimal) -> PublicTrade {
        PublicTrade {
            symbol: sym(),
            price,
            qty,
            taker_side: Side::Buy,
            ts: Timestamp::from_millis(ts),
            id: ts as u64,
        }
    }

    fn set() -> CandleSet {
        CandleSet::new(1_000, DEFAULT_CAPACITY)
    }

    #[test]
    fn the_first_trade_opens_a_candle_on_the_bucket_boundary() {
        let mut candles = set();
        let Update::Formed(candle) = candles.on_trade(&trade(1_700_000_000_123, dec!(100), dec!(2))) else {
            panic!("expected a forming candle");
        };

        assert_eq!(candle.open_time.millis(), 1_700_000_000_000, "snapped down to the bucket");
        assert_eq!(
            (candle.open, candle.high, candle.low, candle.close),
            (dec!(100), dec!(100), dec!(100), dec!(100))
        );
        assert_eq!(candle.volume, dec!(2));
        assert!(!candle.closed);
    }

    #[test]
    fn trades_in_one_bucket_extend_the_same_candle() {
        let mut candles = set();
        candles.on_trade(&trade(1_000, dec!(100), dec!(1)));
        candles.on_trade(&trade(1_400, dec!(105), dec!(1)));
        candles.on_trade(&trade(1_800, dec!(97), dec!(3)));

        let candle = candles.iter(&sym().key()).next_back().unwrap();
        assert_eq!(candle.open, dec!(100));
        assert_eq!(candle.high, dec!(105));
        assert_eq!(candle.low, dec!(97));
        assert_eq!(candle.close, dec!(97), "the last print is the close");
        assert_eq!(candle.volume, dec!(5));
        assert_eq!(candles.len(&sym().key()), 1);
    }

    #[test]
    fn crossing_a_boundary_closes_the_previous_candle_and_opens_the_next() {
        let mut candles = set();
        candles.on_trade(&trade(1_500, dec!(100), dec!(1)));

        let Update::Closed { closed, opened } = candles.on_trade(&trade(2_100, dec!(102), dec!(4))) else {
            panic!("expected a close");
        };

        assert!(closed.closed, "consumers must be told the bar is final");
        assert_eq!(closed.open_time.millis(), 1_000);
        assert_eq!(closed.close, dec!(100));

        assert!(!opened.closed);
        assert_eq!(opened.open_time.millis(), 2_000);
        assert_eq!(opened.open, dec!(102));
        assert_eq!(candles.len(&sym().key()), 2);
    }

    #[test]
    fn a_replayed_trade_cannot_rewrite_a_drawn_candle() {
        // Venues resend prints after a reconnect. Folding one into a finished bucket would
        // change a bar the chart has already shown.
        let mut candles = set();
        candles.on_trade(&trade(1_500, dec!(100), dec!(1)));
        candles.on_trade(&trade(2_100, dec!(102), dec!(1)));

        assert_eq!(candles.on_trade(&trade(1_600, dec!(999), dec!(50))), Update::Ignored);

        let first = candles.iter(&sym().key()).next().unwrap();
        assert_eq!(first.high, dec!(100), "untouched");
        assert_eq!(candles.len(&sym().key()), 2);
    }

    #[test]
    fn a_quiet_stretch_leaves_a_gap_rather_than_inventing_bars() {
        // Ten seconds of silence must not become ten flat candles: the emptiness is the
        // information.
        let mut candles = set();
        candles.on_trade(&trade(1_000, dec!(100), dec!(1)));
        candles.on_trade(&trade(11_000, dec!(101), dec!(1)));

        assert_eq!(candles.len(&sym().key()), 2);
        let times: Vec<i64> = candles.iter(&sym().key()).map(|c| c.open_time.millis()).collect();
        assert_eq!(times, vec![1_000, 11_000]);
    }

    #[test]
    fn the_ring_drops_the_oldest_candles() {
        let mut candles = CandleSet::new(1_000, 3);
        for second in 0..6 {
            candles.on_trade(&trade(second * 1_000, dec!(100), dec!(1)));
        }

        assert_eq!(candles.len(&sym().key()), 3);
        let times: Vec<i64> = candles.iter(&sym().key()).map(|c| c.open_time.millis()).collect();
        assert_eq!(times, vec![3_000, 4_000, 5_000]);
    }

    #[test]
    fn instruments_do_not_share_a_series() {
        let mut candles = set();
        let other = Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT");
        let mut perp_trade = trade(1_000, dec!(200), dec!(1));
        perp_trade.symbol = other.clone();

        candles.on_trade(&trade(1_000, dec!(100), dec!(1)));
        candles.on_trade(&perp_trade);

        assert_eq!(candles.len(&sym().key()), 1);
        assert_eq!(candles.len(&other.key()), 1);
        assert_eq!(candles.iter(&other.key()).next().unwrap().open, dec!(200));
    }

    #[test]
    fn clearing_forgets_one_instrument_only() {
        let mut candles = set();
        candles.on_trade(&trade(1_000, dec!(100), dec!(1)));
        assert!(!candles.is_empty(&sym().key()));

        candles.clear(&sym());
        assert!(candles.is_empty(&sym().key()));
    }

    #[test]
    fn a_zero_interval_is_clamped_rather_than_dividing_by_zero() {
        let mut candles = CandleSet::new(0, 10);
        assert_eq!(candles.interval_ms(), 1);
        candles.on_trade(&trade(1_500, dec!(100), dec!(1)));
        assert_eq!(candles.len(&sym().key()), 1);
    }

    #[test]
    fn an_unknown_instrument_reports_empty_rather_than_panicking() {
        let candles = set();
        assert!(candles.is_empty("binance:spot:NOPE"));
        assert_eq!(candles.iter("binance:spot:NOPE").count(), 0);
        assert!(candles.forming("binance:spot:NOPE").is_none());
    }
}
