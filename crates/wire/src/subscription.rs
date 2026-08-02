//! What a session asked for, and what it therefore receives.
//!
//! # Filtering belongs on the core
//!
//! A terminal watching one instrument out of fifty should not be paying to decode the other
//! forty-nine. That means the core must not serialise them for that session at all — a filter
//! applied after encoding saves the network nothing and the client's CPU nothing, since the
//! decode has already happened by the time anyone can tell it was unwanted.
//!
//! # The ordering rule, made structural
//!
//! A frame carrying [`SymbolId`] 7 is meaningless to a terminal that has not been told what 7
//! is. The obvious approach — send `Subscribed`, then start streaming — depends on nobody ever
//! reordering those two writes, and that discipline is exactly what breaks under a refactor.
//!
//! So a subscription is inert until [`Subscriptions::activate`] is called, and
//! [`Subscriptions::wants`] answers `false` for anything it covers until then. The caller
//! physically cannot leak a frame early, because the filter itself refuses.

use domain::{Event, Payload, Symbol, SymbolId, SymbolRegistry};
use exchange::Subscription;
use std::collections::{HashMap, HashSet};

/// The result of a subscribe request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assigned {
    /// Instruments now subscribed, with the id their frames will carry.
    pub assigned: Vec<(Symbol, SymbolId)>,
    /// Instruments refused, with why. Refusing one must not fail the rest: a terminal
    /// restoring twenty tabs after a reconnect should get nineteen rather than nothing.
    pub rejected: Vec<(Symbol, String)>,
    /// Instruments the session stopped receiving because `replace` was set.
    pub dropped: Vec<Symbol>,
}

/// One session's subscriptions.
#[derive(Debug, Default)]
pub struct Subscriptions {
    /// What has been asked for and acknowledged.
    active: HashSet<Subscription>,
    /// Asked for, assigned an id, but not yet announced to the terminal.
    pending: HashSet<Subscription>,
    ids: HashMap<String, SymbolId>,
}

impl Subscriptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Take a subscribe request, assigning ids but not yet delivering anything.
    ///
    /// `replace` swaps the whole set rather than adding to it. A terminal switching tabs wants
    /// replacement; one opening a second chart wants addition, and guessing wrong either leaks
    /// a stream nobody is watching or silently closes one somebody is.
    pub fn subscribe(
        &mut self,
        requested: &[Subscription],
        replace: bool,
        registry: &mut SymbolRegistry,
        is_tradable: impl Fn(&Symbol) -> Result<(), String>,
    ) -> Assigned {
        let mut dropped = Vec::new();
        if replace {
            let keep: HashSet<_> = requested.iter().cloned().collect();
            for gone in self.active.difference(&keep) {
                dropped.push(gone.symbol().clone());
            }
            self.active.retain(|s| keep.contains(s));
            self.pending.retain(|s| keep.contains(s));
        }

        let (mut assigned, mut rejected) = (Vec::new(), Vec::new());
        for sub in requested {
            let symbol = sub.symbol();
            if let Err(why) = is_tradable(symbol) {
                rejected.push((symbol.clone(), why));
                continue;
            }
            let id = registry.intern(symbol);
            self.ids.insert(symbol.key(), id);

            // Already active means already announced; re-announcing would be harmless but
            // would make a terminal think its ids had changed.
            if !self.active.contains(sub) {
                self.pending.insert(sub.clone());
            }
            assigned.push((symbol.clone(), id));
        }
        Assigned { assigned, rejected, dropped }
    }

    /// Mark everything pending as announced.
    ///
    /// Called by the caller **after** `Subscribed` has been written to the socket. Until then
    /// [`wants`](Self::wants) refuses the traffic, so an id cannot arrive before its meaning.
    pub fn activate(&mut self) {
        self.active.extend(self.pending.drain());
    }

    pub fn unsubscribe(&mut self, requested: &[Subscription]) -> Vec<Symbol> {
        let mut removed = Vec::new();
        for sub in requested {
            if self.active.remove(sub) || self.pending.remove(sub) {
                removed.push(sub.symbol().clone());
            }
        }
        removed
    }

    /// The id a symbol's frames will carry, if it has one.
    pub fn id_of(&self, symbol: &Symbol) -> Option<SymbolId> {
        self.ids.get(&symbol.key()).copied()
    }

    /// Whether this session should be sent `event`.
    ///
    /// Events that do not belong to a single instrument — connection state, instrument lists,
    /// account updates — always pass: they are about the session, not about a market.
    pub fn wants(&self, event: &Event) -> bool {
        let Payload::Market(market) = &event.payload else { return true };
        let Some(symbol) = market.symbol() else { return true };
        self.active.iter().any(|s| s.symbol() == symbol)
    }

    pub fn active(&self) -> Vec<Subscription> {
        let mut subs: Vec<_> = self.active.iter().cloned().collect();
        subs.sort_by_key(|s| s.symbol().key());
        subs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{ExchangeId, MarketEvent, MarketKind, PublicTrade, Side, Timestamp};
    use rust_decimal_macros::dec;

    fn sym(raw: &str) -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, raw)
    }

    fn trade(raw: &str) -> Event {
        Event::market(
            Timestamp::from_millis(1),
            MarketEvent::Trade(PublicTrade {
                symbol: sym(raw),
                price: dec!(1),
                qty: dec!(1),
                taker_side: Side::Buy,
                ts: Timestamp::from_millis(1),
                id: 1,
            }),
        )
    }

    fn anything(symbols: &[&str]) -> Vec<Subscription> {
        symbols.iter().map(|s| Subscription::Trades(sym(s))).collect()
    }

    fn all_tradable(_: &Symbol) -> Result<(), String> {
        Ok(())
    }

    fn fixture() -> (Subscriptions, SymbolRegistry) {
        (Subscriptions::new(), SymbolRegistry::new(1))
    }

    // --- the ordering invariant (10, doc 11 §10.5) -----------------------------------------

    #[test]
    fn nothing_is_delivered_before_the_terminal_has_been_told_the_id() {
        // The acceptance invariant, made structural rather than left to discipline: a frame
        // carrying id 7 is meaningless to a terminal that has not been told what 7 is.
        let (mut subs, mut registry) = fixture();
        let assigned = subs.subscribe(&anything(&["BTCUSDT"]), false, &mut registry, all_tradable);

        assert_eq!(assigned.assigned.len(), 1, "an id was assigned");
        assert!(
            !subs.wants(&trade("BTCUSDT")),
            "but nothing may be delivered until Subscribed has been written"
        );

        subs.activate();
        assert!(subs.wants(&trade("BTCUSDT")), "and after that it flows");
    }

    #[test]
    fn the_filter_itself_refuses_so_the_order_cannot_be_got_wrong() {
        // A refactor that reordered the two writes would leak a frame under any scheme that
        // relied on the caller. Here the filter is the thing that says no.
        let (mut subs, mut registry) = fixture();
        subs.subscribe(&anything(&["BTCUSDT"]), false, &mut registry, all_tradable);
        assert_eq!(subs.pending_count(), 1);
        assert_eq!(subs.active_count(), 0, "pending is not active");
    }

    // --- what is delivered ----------------------------------------------------------------------

    #[test]
    fn an_unsubscribed_instrument_is_never_serialised_for_this_session() {
        let (mut subs, mut registry) = fixture();
        subs.subscribe(&anything(&["BTCUSDT"]), false, &mut registry, all_tradable);
        subs.activate();

        assert!(subs.wants(&trade("BTCUSDT")));
        assert!(!subs.wants(&trade("ETHUSDT")), "a terminal watching one must not decode fifty");
    }

    #[test]
    fn events_that_are_not_about_a_market_always_pass() {
        // Connection state, instrument lists and account updates are about the session, not
        // about an instrument, and filtering them by subscription would hide them entirely.
        let (subs, _) = fixture();
        let connection = Event::connection(Timestamp::from_millis(1), domain::ConnectionEvent::Ready);
        assert!(subs.wants(&connection), "with no subscriptions at all");
    }

    #[test]
    fn subscribing_to_one_of_fifty_saves_what_it_is_supposed_to() {
        // The measurement from doc 11 §10.5. Encoded size rather than event count, because the
        // saving being claimed is bytes on a socket and CPU in a decoder.
        let (mut subs, mut registry) = fixture();
        let universe: Vec<String> = (0..50).map(|i| format!("SYM{i}USDT")).collect();

        subs.subscribe(&[Subscription::Trades(sym(&universe[0]))], false, &mut registry, all_tradable);
        subs.activate();

        let (mut sent, mut total) = (0usize, 0usize);
        for raw in &universe {
            let event = trade(raw);
            let size = rmp_serde::to_vec_named(&event).unwrap().len();
            total += size;
            if subs.wants(&event) {
                sent += size;
            }
        }
        let saving = total as f64 / sent.max(1) as f64;
        println!("{sent} bytes of {total} — {saving:.0}× saving");
        assert!(saving >= 40.0, "expected at least 40x, got {saving:.1}x");
    }

    // --- replace against add -----------------------------------------------------------------------

    #[test]
    fn replace_swaps_the_set_and_reports_what_was_dropped() {
        // A terminal switching tabs wants replacement. Reporting what went is what lets it
        // free the state rather than keeping a book that will never update again.
        let (mut subs, mut registry) = fixture();
        subs.subscribe(&anything(&["BTCUSDT", "ETHUSDT"]), false, &mut registry, all_tradable);
        subs.activate();

        let outcome = subs.subscribe(&anything(&["SOLUSDT"]), true, &mut registry, all_tradable);
        subs.activate();

        assert_eq!(outcome.dropped.len(), 2);
        assert!(subs.wants(&trade("SOLUSDT")));
        assert!(!subs.wants(&trade("BTCUSDT")), "replaced away");
    }

    #[test]
    fn adding_keeps_what_was_already_there() {
        // Opening a second chart must not close the first.
        let (mut subs, mut registry) = fixture();
        subs.subscribe(&anything(&["BTCUSDT"]), false, &mut registry, all_tradable);
        subs.activate();
        let outcome = subs.subscribe(&anything(&["ETHUSDT"]), false, &mut registry, all_tradable);
        subs.activate();

        assert!(outcome.dropped.is_empty());
        assert!(subs.wants(&trade("BTCUSDT")) && subs.wants(&trade("ETHUSDT")));
    }

    #[test]
    fn resubscribing_to_something_already_active_does_not_disturb_it() {
        // Re-announcing would be harmless on the wire but would make a terminal think its ids
        // had changed and rebuild state for no reason.
        let (mut subs, mut registry) = fixture();
        subs.subscribe(&anything(&["BTCUSDT"]), false, &mut registry, all_tradable);
        subs.activate();

        let again = subs.subscribe(&anything(&["BTCUSDT"]), false, &mut registry, all_tradable);
        assert_eq!(subs.pending_count(), 0, "nothing became pending again");
        assert_eq!(again.assigned[0].1, subs.id_of(&sym("BTCUSDT")).unwrap(), "same id");
        assert!(subs.wants(&trade("BTCUSDT")), "and it never stopped flowing");
    }

    // --- rejection -----------------------------------------------------------------------------------

    #[test]
    fn one_bad_instrument_does_not_fail_the_whole_request() {
        // A terminal restoring twenty tabs after a reconnect should get nineteen rather than
        // nothing, and should be told which one it lost.
        let (mut subs, mut registry) = fixture();
        let outcome =
            subs.subscribe(&anything(&["BTCUSDT", "DELISTED", "ETHUSDT"]), false, &mut registry, |s| {
                if s.raw == "DELISTED" {
                    Err("not trading".into())
                } else {
                    Ok(())
                }
            });
        subs.activate();

        assert_eq!(outcome.assigned.len(), 2);
        assert_eq!(outcome.rejected.len(), 1);
        assert_eq!(outcome.rejected[0].0.raw, "DELISTED");
        assert!(outcome.rejected[0].1.contains("not trading"), "the reason must be carried");
        assert!(subs.wants(&trade("BTCUSDT")), "the good ones went through");
    }

    #[test]
    fn a_rejected_instrument_gets_no_id() {
        // Assigning one would put a symbol in the dictionary that can never be delivered, and
        // the dictionary is what the terminal builds its mapping from.
        let (mut subs, mut registry) = fixture();
        subs.subscribe(&anything(&["DELISTED"]), false, &mut registry, |_| Err("no".into()));
        assert!(subs.id_of(&sym("DELISTED")).is_none());
        assert_eq!(registry.len(), 0, "the dictionary must not grow from a refusal");
    }

    // --- unsubscribe -----------------------------------------------------------------------------------

    #[test]
    fn unsubscribing_stops_delivery_and_reports_what_stopped() {
        let (mut subs, mut registry) = fixture();
        subs.subscribe(&anything(&["BTCUSDT", "ETHUSDT"]), false, &mut registry, all_tradable);
        subs.activate();

        let removed = subs.unsubscribe(&anything(&["BTCUSDT"]));
        assert_eq!(removed.len(), 1);
        assert!(!subs.wants(&trade("BTCUSDT")));
        assert!(subs.wants(&trade("ETHUSDT")), "the other is untouched");
    }

    #[test]
    fn unsubscribing_from_something_not_held_reports_nothing() {
        let (mut subs, _) = fixture();
        assert!(subs.unsubscribe(&anything(&["BTCUSDT"])).is_empty());
    }

    #[test]
    fn an_id_survives_unsubscribing_and_resubscribing() {
        // Ids are never recycled within an epoch, so a terminal that kept its mapping across
        // a tab close and reopen is still correct.
        let (mut subs, mut registry) = fixture();
        subs.subscribe(&anything(&["BTCUSDT"]), false, &mut registry, all_tradable);
        subs.activate();
        let first = subs.id_of(&sym("BTCUSDT")).unwrap();

        subs.unsubscribe(&anything(&["BTCUSDT"]));
        subs.subscribe(&anything(&["BTCUSDT"]), false, &mut registry, all_tradable);
        subs.activate();

        assert_eq!(subs.id_of(&sym("BTCUSDT")), Some(first));
    }

    #[test]
    fn the_active_list_is_stable_for_comparison() {
        // It answers `SubscriptionsGet`, and a terminal diffing it against its own state needs
        // an order that does not depend on hashing.
        let (mut subs, mut registry) = fixture();
        subs.subscribe(&anything(&["SOLUSDT", "BTCUSDT", "ETHUSDT"]), false, &mut registry, all_tradable);
        subs.activate();

        let keys: Vec<String> = subs.active().iter().map(|s| s.symbol().raw.clone()).collect();
        assert_eq!(keys, vec!["BTCUSDT", "ETHUSDT", "SOLUSDT"]);
    }
}
