//! The core's shared read-model.
//!
//! A broadcast only reaches whoever is already listening. Book snapshots are published once,
//! when the book is built, and then never again while it stays healthy — so a terminal that
//! connects a minute later would receive a stream of deltas for a book it does not have and
//! would sit blank forever. That is precisely what happened the first time this was wired up.
//!
//! So the core keeps the current book itself. A session gets the state on subscribe and the
//! stream thereafter. Deltas already queued for that session are older than the state it was
//! handed, and [`OrderBook::apply`] discards them as stale — the overlap resolves itself.

use domain::{Candle, Event, MarketEvent, OrderBook, Payload, PublicTrade};
use exchange::Subscription;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

/// Candles retained per instrument for late subscribers.
///
/// A terminal that connects an hour in must get a chart with history on it, not a single bar
/// that grows from the right edge.
const HISTORY: usize = 600;

/// Prints retained per instrument.
///
/// The chart draws a mark per trade, so a terminal joining late needs the prints themselves,
/// not just the bars they were folded into.
const TAPE_HISTORY: usize = 4_000;

#[derive(Clone, Default)]
pub struct MarketState {
    books: Arc<RwLock<HashMap<String, OrderBook>>>,
    candles: Arc<RwLock<HashMap<String, VecDeque<Candle>>>>,
    tape: Arc<RwLock<HashMap<String, VecDeque<PublicTrade>>>>,
}

impl MarketState {
    /// Fold one event into the read-model. Called for every event before it is broadcast.
    pub fn apply(&self, event: &Event) {
        let Payload::Market(market) = &event.payload else { return };

        if let MarketEvent::Candle(candle) = market {
            self.fold_candle(candle);
            return;
        }
        if let MarketEvent::Trade(trade) = market {
            let mut tape = self.tape.write().unwrap_or_else(|e| e.into_inner());
            let series = tape.entry(trade.symbol.key()).or_default();
            series.push_back(trade.clone());
            while series.len() > TAPE_HISTORY {
                series.pop_front();
            }
            return;
        }

        let mut books = self.books.write().unwrap_or_else(|e| e.into_inner());
        match market {
            MarketEvent::BookSnapshot(book) => {
                if let Some(symbol) = &book.symbol {
                    tracing::debug!(%symbol, synced = book.synced, "read-model: snapshot in");
                    books.insert(symbol.key(), book.clone());
                }
            }
            MarketEvent::BookDelta(delta) => {
                if let Some(book) = books.get_mut(&delta.symbol.key()) {
                    // The connector only forwards updates that chain, so a gap here would
                    // mean the core disagrees with itself. Drop the book rather than serve
                    // a torn one; the connector's next snapshot repairs it.
                    if let domain::ApplyOutcome::Gap { expected, got } = book.apply(delta) {
                        tracing::debug!(symbol = %delta.symbol, expected, got, "read-model: torn, dropping");
                        books.remove(&delta.symbol.key());
                    }
                }
            }
            _ => {}
        }
    }

    /// Replace the matching bar, or append a new one.
    ///
    /// The connector republishes the forming bar on every print, so this is an update far
    /// more often than an insert; matching on open time keeps one bar per bucket.
    fn fold_candle(&self, candle: &Candle) {
        let mut candles = self.candles.write().unwrap_or_else(|e| e.into_inner());
        let series = candles.entry(candle.symbol.key()).or_default();

        match series.back_mut() {
            Some(last) if last.open_time == candle.open_time => *last = candle.clone(),
            Some(last) if last.open_time > candle.open_time => {}
            _ => {
                series.push_back(candle.clone());
                while series.len() > HISTORY {
                    series.pop_front();
                }
            }
        }
    }

    /// Candle history for the instruments a session just subscribed to, oldest first.
    pub fn candles_for(&self, subs: &[Subscription]) -> Vec<Candle> {
        let candles = self.candles.read().unwrap_or_else(|e| e.into_inner());
        let mut keys: Vec<String> = subs.iter().map(|s| s.symbol().key()).collect();
        keys.sort();
        keys.dedup();

        keys.iter().filter_map(|k| candles.get(k)).flatten().cloned().collect()
    }

    /// Print history for the instruments a session just subscribed to, oldest first.
    pub fn tape_for(&self, subs: &[Subscription]) -> Vec<PublicTrade> {
        let tape = self.tape.read().unwrap_or_else(|e| e.into_inner());
        let mut keys: Vec<String> = subs.iter().map(|s| s.symbol().key()).collect();
        keys.sort();
        keys.dedup();

        keys.iter().filter_map(|k| tape.get(k)).flatten().cloned().collect()
    }

    /// Current books for the instruments a session just subscribed to.
    ///
    /// Only synced books are returned: an unattached one would show a spread from a moment
    /// that has already passed.
    pub fn snapshots_for(&self, subs: &[Subscription]) -> Vec<OrderBook> {
        let books = self.books.read().unwrap_or_else(|e| e.into_inner());
        let mut keys: Vec<String> = subs.iter().map(|s| s.symbol().key()).collect();
        keys.sort();
        keys.dedup();

        tracing::debug!(held = books.len(), ?keys, "read-model: serving");
        keys.iter().filter_map(|k| books.get(k)).filter(|b| b.synced).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{BookDelta, BookLevel, ConnectionEvent, ExchangeId, MarketKind, Symbol, Timestamp};
    use rust_decimal_macros::dec;

    fn sym(market: MarketKind) -> Symbol {
        Symbol::new(ExchangeId::Binance, market, "BTCUSDT")
    }

    fn snapshot(market: MarketKind, id: u64, bid: rust_decimal::Decimal) -> Event {
        Event::market(
            Timestamp::from_millis(1),
            MarketEvent::BookSnapshot(OrderBook {
                symbol: Some(sym(market)),
                bids: vec![BookLevel { price: dec!(100), qty: bid }],
                asks: vec![BookLevel { price: dec!(101), qty: dec!(1) }],
                last_update_id: id,
                ts: Timestamp::from_millis(1),
                synced: true,
            }),
        )
    }

    fn delta(market: MarketKind, prev: u64, last: u64, bid: rust_decimal::Decimal) -> Event {
        Event::market(
            Timestamp::from_millis(2),
            MarketEvent::BookDelta(BookDelta {
                symbol: sym(market),
                prev_update_id: prev,
                last_update_id: last,
                bids: vec![BookLevel { price: dec!(100), qty: bid }],
                asks: vec![],
                ts: Timestamp::from_millis(2),
            }),
        )
    }

    fn book_sub(market: MarketKind) -> Subscription {
        Subscription::Book(sym(market))
    }

    #[test]
    fn a_late_subscriber_gets_the_book_as_it_stands_now() {
        // The bug this exists for: the snapshot was broadcast before anyone was listening.
        let state = MarketState::default();
        state.apply(&snapshot(MarketKind::Spot, 1_000, dec!(1)));
        state.apply(&delta(MarketKind::Spot, 1_000, 1_010, dec!(5)));

        let served = state.snapshots_for(&[book_sub(MarketKind::Spot)]);
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].last_update_id, 1_010, "deltas since the snapshot are folded in");
        assert_eq!(served[0].best_bid().unwrap().qty, dec!(5));
    }

    #[test]
    fn nothing_is_served_for_an_instrument_never_seen() {
        let state = MarketState::default();
        assert!(state.snapshots_for(&[book_sub(MarketKind::Spot)]).is_empty());
    }

    #[test]
    fn markets_are_tracked_separately_under_the_same_ticker() {
        let state = MarketState::default();
        state.apply(&snapshot(MarketKind::Spot, 1_000, dec!(1)));
        state.apply(&snapshot(MarketKind::LinearPerp, 9_000, dec!(7)));

        let served = state.snapshots_for(&[book_sub(MarketKind::Spot), book_sub(MarketKind::LinearPerp)]);
        assert_eq!(served.len(), 2);

        let spot_only = state.snapshots_for(&[book_sub(MarketKind::Spot)]);
        assert_eq!(spot_only.len(), 1);
        assert_eq!(spot_only[0].symbol.as_ref().unwrap().market, MarketKind::Spot);
    }

    #[test]
    fn a_torn_book_is_dropped_rather_than_served() {
        let state = MarketState::default();
        state.apply(&snapshot(MarketKind::Spot, 1_000, dec!(1)));
        // Does not chain onto 1_000.
        state.apply(&delta(MarketKind::Spot, 5_000, 5_010, dec!(9)));

        assert!(state.snapshots_for(&[book_sub(MarketKind::Spot)]).is_empty());
    }

    #[test]
    fn asking_twice_for_the_same_instrument_serves_it_once() {
        let state = MarketState::default();
        state.apply(&snapshot(MarketKind::Spot, 1_000, dec!(1)));

        // A terminal subscribing to both trades and book must not receive two snapshots.
        let served = state.snapshots_for(&[
            Subscription::Trades(sym(MarketKind::Spot)),
            Subscription::Book(sym(MarketKind::Spot)),
        ]);
        assert_eq!(served.len(), 1);
    }

    fn candle(open_time: i64, close: rust_decimal::Decimal, closed: bool) -> Event {
        Event::market(
            Timestamp::from_millis(open_time),
            MarketEvent::Candle(Candle {
                symbol: sym(MarketKind::Spot),
                open_time: Timestamp::from_millis(open_time),
                bucketing: domain::Bucketing::Ticks { count: 25 },
                trades: 1,
                open: dec!(100),
                high: close.max(dec!(100)),
                low: close.min(dec!(100)),
                close,
                volume: dec!(1),
                closed,
            }),
        )
    }

    #[test]
    fn a_late_subscriber_gets_chart_history_not_a_single_bar() {
        let state = MarketState::default();
        state.apply(&candle(1_000, dec!(101), true));
        state.apply(&candle(2_000, dec!(102), true));
        state.apply(&candle(3_000, dec!(103), false));

        let served = state.candles_for(&[book_sub(MarketKind::Spot)]);
        assert_eq!(served.len(), 3);
        assert_eq!(served.first().unwrap().open_time.millis(), 1_000, "oldest first");
        assert_eq!(served.last().unwrap().close, dec!(103));
    }

    #[test]
    fn the_forming_bar_is_replaced_rather_than_appended() {
        // The connector republishes the forming bar on every print; appending each one would
        // turn a single second into hundreds of bars.
        let state = MarketState::default();
        for close in [dec!(101), dec!(102), dec!(103)] {
            state.apply(&candle(1_000, close, false));
        }

        let served = state.candles_for(&[book_sub(MarketKind::Spot)]);
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].close, dec!(103), "the latest print wins");
    }

    #[test]
    fn a_candle_older_than_the_last_one_is_dropped() {
        // A replayed print must not reopen a bar the chart has already finished.
        let state = MarketState::default();
        state.apply(&candle(2_000, dec!(102), true));
        state.apply(&candle(1_000, dec!(999), true));

        let served = state.candles_for(&[book_sub(MarketKind::Spot)]);
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].open_time.millis(), 2_000);
    }

    #[test]
    fn a_late_subscriber_gets_the_prints_the_chart_draws() {
        let state = MarketState::default();
        for id in 0..3u64 {
            state.apply(&Event::market(
                Timestamp::from_millis(id as i64),
                MarketEvent::Trade(domain::PublicTrade {
                    symbol: sym(MarketKind::Spot),
                    price: dec!(100),
                    qty: dec!(1),
                    taker_side: domain::Side::Buy,
                    ts: Timestamp::from_millis(id as i64),
                    id,
                }),
            ));
        }

        let served = state.tape_for(&[book_sub(MarketKind::Spot)]);
        assert_eq!(served.len(), 3);
        assert_eq!(served.first().unwrap().id, 0, "oldest first");
        assert!(state.tape_for(&[book_sub(MarketKind::LinearPerp)]).is_empty());
    }

    #[test]
    fn candles_and_books_are_served_per_instrument() {
        let state = MarketState::default();
        state.apply(&candle(1_000, dec!(101), true));

        assert_eq!(state.candles_for(&[book_sub(MarketKind::Spot)]).len(), 1);
        assert!(state.candles_for(&[book_sub(MarketKind::LinearPerp)]).is_empty());
    }

    #[test]
    fn non_market_events_leave_the_read_model_alone() {
        let state = MarketState::default();
        state.apply(&snapshot(MarketKind::Spot, 1_000, dec!(1)));
        state.apply(&Event::connection(Timestamp::from_millis(3), ConnectionEvent::Ready));

        assert_eq!(state.snapshots_for(&[book_sub(MarketKind::Spot)]).len(), 1);
    }
}
