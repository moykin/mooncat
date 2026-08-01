//! Building candles from the trade tape.
//!
//! Not taken from the venue's own kline stream, for two reasons. A scalping chart wants
//! sub-minute buckets and Binance's smallest published interval is a minute; and a candle
//! built from the same prints the tape shows is a candle that cannot disagree with the tape.
//!
//! Nothing here fabricates data. A minute with no trades produces no candle rather than a
//! flat one — a gap on the chart is the truth, and inventing a bar to fill it hides exactly
//! the illiquidity a scalper is looking for.

use domain::{Bucketing, Candle, PublicTrade, Symbol};
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
    bucketing: Bucketing,
    capacity: usize,
    series: HashMap<String, VecDeque<Candle>>,
}

impl CandleSet {
    /// Degenerate bucketing — a non-positive interval or a zero tick count — is clamped
    /// rather than rejected: either would otherwise fold the whole session into one bar.
    pub fn new(bucketing: Bucketing, capacity: usize) -> Self {
        let bucketing = match bucketing {
            Bucketing::Time { interval_ms } => Bucketing::Time { interval_ms: interval_ms.max(1) },
            Bucketing::Ticks { count } => Bucketing::Ticks { count: count.max(1) },
        };
        Self { bucketing, capacity: capacity.max(1), series: HashMap::new() }
    }

    pub fn bucketing(&self) -> Bucketing {
        self.bucketing
    }

    /// Fold one trade into its bar.
    pub fn on_trade(&mut self, trade: &PublicTrade) -> Update {
        match self.bucketing {
            Bucketing::Time { interval_ms } => self.fold_time(trade, interval_ms),
            Bucketing::Ticks { count } => self.fold_ticks(trade, count),
        }
    }

    /// Time bars: the clock decides where one bar ends and the next begins.
    fn fold_time(&mut self, trade: &PublicTrade, interval_ms: i64) -> Update {
        let millis = trade.ts.millis();
        let open_time = domain::Timestamp::from_millis(millis - millis.rem_euclid(interval_ms));
        let (bucketing, capacity) = (self.bucketing, self.capacity);
        let series = self.series.entry(trade.symbol.key()).or_default();

        match series.back().map(|c| c.open_time) {
            // A trade older than the forming bar would rewrite history consumers have
            // already drawn. Venues replay prints after a reconnect; drop them.
            Some(current) if open_time < current => Update::Ignored,
            Some(current) if open_time == current => Update::Formed(extend(series, trade)),
            _ => roll(series, trade, open_time, bucketing, capacity),
        }
    }

    /// Tick bars: the print count decides, and the clock says nothing.
    ///
    /// This is the difference the name promises. A quiet minute yields no bar at all, and a
    /// burst yields several inside one second — the chart stretches where the activity is.
    fn fold_ticks(&mut self, trade: &PublicTrade, count: u32) -> Update {
        let (bucketing, capacity) = (self.bucketing, self.capacity);
        let series = self.series.entry(trade.symbol.key()).or_default();

        let room = series.back().is_some_and(|c| !c.closed && c.trades < count);
        if room {
            let candle = extend(series, trade);
            // Filling the last slot ends the bar there and then, so consumers never see a
            // finished bar still marked as forming.
            if candle.trades >= count {
                let closed = series.back_mut().expect("just extended");
                closed.closed = true;
                return Update::Formed(closed.clone());
            }
            return Update::Formed(candle);
        }

        roll(series, trade, trade.ts, bucketing, capacity)
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
}

/// Fold a print into the bar currently forming.
fn extend(series: &mut VecDeque<Candle>, trade: &PublicTrade) -> Candle {
    let candle = series.back_mut().expect("caller checked there is one");
    candle.high = candle.high.max(trade.price);
    candle.low = candle.low.min(trade.price);
    candle.close = trade.price;
    candle.volume += trade.qty;
    candle.trades += 1;
    candle.clone()
}

/// Close whatever was forming and start a new bar on this print.
fn roll(
    series: &mut VecDeque<Candle>,
    trade: &PublicTrade,
    open_time: domain::Timestamp,
    bucketing: Bucketing,
    capacity: usize,
) -> Update {
    let mut closed = None;
    if let Some(previous) = series.back_mut() {
        if !previous.closed {
            previous.closed = true;
        }
        closed = Some(previous.clone());
    }

    let mut opened = Candle {
        symbol: trade.symbol.clone(),
        open_time,
        bucketing,
        trades: 1,
        open: trade.price,
        high: trade.price,
        low: trade.price,
        close: trade.price,
        volume: trade.qty,
        closed: false,
    };
    // A one-tick bar is complete the instant it exists; leaving it "forming" would have
    // consumers redraw a bar that can never change again.
    if let Bucketing::Ticks { count } = bucketing {
        opened.closed = opened.trades >= count;
    }
    series.push_back(opened.clone());
    while series.len() > capacity {
        series.pop_front();
    }

    match closed {
        Some(closed) => Update::Closed { closed, opened },
        None => Update::Formed(opened),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{ExchangeId, MarketKind, Side, Timestamp};
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
        CandleSet::new(Bucketing::Time { interval_ms: 1_000 }, DEFAULT_CAPACITY)
    }

    fn ticks(count: u32) -> CandleSet {
        CandleSet::new(Bucketing::Ticks { count }, DEFAULT_CAPACITY)
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
        let mut candles = CandleSet::new(Bucketing::Time { interval_ms: 1_000 }, 3);
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
        let mut candles = CandleSet::new(Bucketing::Time { interval_ms: 0 }, 10);
        assert_eq!(candles.bucketing(), Bucketing::Time { interval_ms: 1 });
        candles.on_trade(&trade(1_500, dec!(100), dec!(1)));
        assert_eq!(candles.len(&sym().key()), 1);
    }

    #[test]
    fn a_tick_bar_closes_on_the_print_count_not_the_clock() {
        // The whole point of the name: five prints make a bar whether they land in one
        // millisecond or across a minute.
        let mut candles = ticks(5);
        for n in 0..4 {
            assert!(matches!(candles.on_trade(&trade(1_000 + n, dec!(100), dec!(1))), Update::Formed(_)));
        }
        assert_eq!(candles.len(&sym().key()), 1);

        let Update::Formed(fifth) = candles.on_trade(&trade(9_999_999, dec!(101), dec!(1))) else {
            panic!("the fifth print completes the bar");
        };
        assert!(fifth.closed, "a full bar is final the moment it fills");
        assert_eq!(fifth.trades, 5);

        // The sixth opens the next one, however long the wait was.
        let Update::Closed { opened, .. } = candles.on_trade(&trade(9_999_999, dec!(102), dec!(1))) else {
            panic!("expected a roll");
        };
        assert_eq!(opened.trades, 1);
        assert_eq!(candles.len(&sym().key()), 2);
    }

    #[test]
    fn tick_bars_compress_quiet_stretches_and_expand_bursts() {
        // Six prints inside one millisecond make two bars; an hour of silence adds none.
        // A one-second time chart would have produced one bar and then sixty empty seconds.
        let mut candles = ticks(3);
        for _ in 0..6 {
            candles.on_trade(&trade(1_000, dec!(100), dec!(1)));
        }
        assert_eq!(candles.len(&sym().key()), 2, "activity, not elapsed time, cut these");

        let times: Vec<i64> = candles.iter(&sym().key()).map(|c| c.open_time.millis()).collect();
        assert_eq!(times, vec![1_000, 1_000], "both bars began in the same millisecond");
    }

    #[test]
    fn a_tick_bar_records_how_many_prints_it_holds() {
        let mut candles = ticks(10);
        candles.on_trade(&trade(1_000, dec!(100), dec!(1)));
        candles.on_trade(&trade(1_001, dec!(101), dec!(2)));

        let bar = candles.forming(&sym().key()).unwrap();
        assert_eq!(bar.trades, 2);
        assert_eq!(bar.volume, dec!(3));
        assert_eq!(bar.bucketing, Bucketing::Ticks { count: 10 });
    }

    #[test]
    fn a_tick_count_of_one_makes_every_print_its_own_bar() {
        let mut candles = ticks(1);
        for n in 0..3 {
            candles.on_trade(&trade(1_000 + n, dec!(100), dec!(1)));
        }
        assert_eq!(candles.len(&sym().key()), 3);
        assert!(candles.iter(&sym().key()).all(|c| c.closed && c.trades == 1));
    }

    #[test]
    fn a_zero_tick_count_is_clamped_rather_than_swallowing_the_session() {
        let candles = CandleSet::new(Bucketing::Ticks { count: 0 }, 10);
        assert_eq!(candles.bucketing(), Bucketing::Ticks { count: 1 });
    }

    #[test]
    fn time_bars_also_count_their_prints() {
        let mut candles = set();
        candles.on_trade(&trade(1_000, dec!(100), dec!(1)));
        candles.on_trade(&trade(1_500, dec!(101), dec!(1)));
        assert_eq!(candles.forming(&sym().key()).unwrap().trades, 2);
    }

    #[test]
    fn an_unknown_instrument_reports_empty_rather_than_panicking() {
        let candles = set();
        assert!(candles.is_empty("binance:spot:NOPE"));
        assert_eq!(candles.iter("binance:spot:NOPE").count(), 0);
        assert!(candles.forming("binance:spot:NOPE").is_none());
    }
}
