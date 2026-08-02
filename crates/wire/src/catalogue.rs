//! The venue's instrument list, and how it reaches a terminal.
//!
//! # What this fixes
//!
//! The terminal's "Instruments" tab currently shows the books of whatever happens to be
//! subscribed — reverse-engineering report 09 §3.4 records it as a defect. Those are two
//! different things: an instrument list is what the venue offers, a subscription list is what
//! this session asked for, and only the first can answer "what can I trade?".
//!
//! # Why it is paged
//!
//! Binance lists a few thousand instruments per market. As one message that is megabytes,
//! which blocks the socket long enough to threaten the heartbeat and cannot be shown
//! incrementally. Two thousand at a time is a few hundred kilobytes: large enough that the
//! paging overhead is negligible, small enough that nothing else waits behind it.
//!
//! # Why the cursor is a name and not an offset
//!
//! An index-based cursor assumes the list does not change between pages, and the list is
//! precisely the thing that changes — a listing, a delisting, a symbol going to maintenance.
//! With an offset, an instrument removed while paging silently skips the one after it. The
//! cursor here is the last name delivered, so the next page starts after that name whatever
//! happened in between.

use domain::{ExchangeId, Instrument, MarketKind, Symbol};
use std::collections::HashMap;

/// Instruments in one page. Sized so a page is a few hundred kilobytes rather than megabytes.
pub const INSTRUMENTS_PAGE: usize = 2_000;

/// What changed when a venue's list was refreshed.
///
/// Sent before the page that reflects it, so a terminal can react to a new listing without
/// diffing two large lists itself — and so an alert on a new listing has something to fire on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changed {
    pub added: Vec<Symbol>,
    pub removed: Vec<Symbol>,
    /// Still listed, but something about it moved — a tick size, a minimum, a halt.
    pub altered: Vec<Symbol>,
}

impl Changed {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.altered.is_empty()
    }
}

/// One page of the catalogue.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub exchange: ExchangeId,
    pub market: MarketKind,
    pub items: Vec<Instrument>,
    /// Where to resume. `None` means this was the last page.
    pub cursor: Option<String>,
    /// Revision of the list this page was cut from. A terminal that sees it change mid-paging
    /// knows its pages no longer describe one consistent list.
    pub rev: u64,
}

/// Every instrument the core knows about, by market.
#[derive(Debug, Default)]
pub struct Catalogue {
    /// Sorted by `raw`, which is what makes a name-based cursor work.
    by_market: HashMap<(ExchangeId, MarketKind), Vec<Instrument>>,
    rev: u64,
}

impl Catalogue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Revision, bumped whenever anything changes. Carried on every page.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn len(&self, exchange: ExchangeId, market: MarketKind) -> usize {
        self.by_market.get(&(exchange, market)).map_or(0, Vec::len)
    }

    pub fn is_empty(&self) -> bool {
        self.by_market.values().all(Vec::is_empty)
    }

    /// Replace one market's list, reporting what moved.
    ///
    /// Replacement rather than merge: the venue's answer is the truth, and an instrument
    /// missing from it has been delisted. A merge would keep delisted symbols alive forever
    /// because nothing ever explicitly says they are gone.
    pub fn replace(
        &mut self,
        exchange: ExchangeId,
        market: MarketKind,
        mut incoming: Vec<Instrument>,
    ) -> Changed {
        incoming.sort_by(|a, b| a.symbol.raw.cmp(&b.symbol.raw));
        incoming.dedup_by(|a, b| a.symbol.raw == b.symbol.raw);

        let previous = self.by_market.entry((exchange, market)).or_default();
        let before: HashMap<&str, &Instrument> =
            previous.iter().map(|i| (i.symbol.raw.as_str(), i)).collect();

        let mut changed = Changed::default();
        for item in &incoming {
            match before.get(item.symbol.raw.as_str()) {
                None => changed.added.push(item.symbol.clone()),
                Some(old) if !same_terms(old, item) => changed.altered.push(item.symbol.clone()),
                Some(_) => {}
            }
        }
        let now: HashMap<&str, ()> = incoming.iter().map(|i| (i.symbol.raw.as_str(), ())).collect();
        for old in previous.iter() {
            if !now.contains_key(old.symbol.raw.as_str()) {
                changed.removed.push(old.symbol.clone());
            }
        }

        *previous = incoming;
        if !changed.is_empty() {
            self.rev += 1;
        }
        changed
    }

    /// One page, starting after `cursor`.
    ///
    /// Returns `None` only for a market that was never populated — an empty market yields an
    /// empty final page, so a terminal can tell "nothing listed" from "never asked".
    pub fn page(&self, exchange: ExchangeId, market: MarketKind, cursor: Option<&str>) -> Option<Page> {
        let all = self.by_market.get(&(exchange, market))?;

        // Strictly after the cursor, by name. An instrument removed since the previous page
        // cannot make this skip one, because the position is recomputed from the name.
        let start = match cursor {
            None => 0,
            Some(after) => all.partition_point(|i| i.symbol.raw.as_str() <= after),
        };
        let end = (start + INSTRUMENTS_PAGE).min(all.len());
        let items = all[start..end].to_vec();

        let next = (end < all.len()).then(|| items.last().map(|i| i.symbol.raw.clone())).flatten();
        Some(Page { exchange, market, items, cursor: next, rev: self.rev })
    }

    pub fn get(&self, symbol: &Symbol) -> Option<&Instrument> {
        let all = self.by_market.get(&(symbol.exchange, symbol.market))?;
        all.binary_search_by(|i| i.symbol.raw.cmp(&symbol.raw)).ok().map(|at| &all[at])
    }

    /// Whether an instrument may be subscribed to, for the subscription gate.
    pub fn is_tradable(&self, symbol: &Symbol) -> Result<(), String> {
        match self.get(symbol) {
            None => Err(format!("{} is not listed on {}", symbol.raw, symbol.exchange)),
            Some(i) if !i.trading => Err(format!("{} is halted or delisted", symbol.raw)),
            Some(_) => Ok(()),
        }
    }
}

/// Whether the terms a trader cares about are unchanged.
///
/// Compared field by field rather than by equality on the whole struct so that a field added
/// later has to be considered: if it belongs here it goes in the list, and if it does not, its
/// absence was a decision rather than an oversight.
fn same_terms(a: &Instrument, b: &Instrument) -> bool {
    a.tick_size == b.tick_size
        && a.step_size == b.step_size
        && a.min_qty == b.min_qty
        && a.min_notional == b.min_notional
        && a.trading == b.trading
        && a.base == b.base
        && a.quote == b.quote
        && a.margin_asset == b.margin_asset
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn instrument(raw: &str) -> Instrument {
        Instrument {
            symbol: Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, raw),
            base: raw.trim_end_matches("USDT").to_string(),
            quote: "USDT".into(),
            margin_asset: "USDT".into(),
            tick_size: dec!(0.01),
            step_size: dec!(0.001),
            min_qty: dec!(0.001),
            min_notional: dec!(5),
            trading: true,
        }
    }

    fn many(count: usize) -> Vec<Instrument> {
        (0..count).map(|i| instrument(&format!("SYM{i:05}USDT"))).collect()
    }

    fn populated(count: usize) -> Catalogue {
        let mut catalogue = Catalogue::new();
        catalogue.replace(ExchangeId::Binance, MarketKind::LinearPerp, many(count));
        catalogue
    }

    fn page(catalogue: &Catalogue, cursor: Option<&str>) -> Page {
        catalogue.page(ExchangeId::Binance, MarketKind::LinearPerp, cursor).expect("populated")
    }

    /// Walk every page, returning the names in order.
    fn walk(catalogue: &Catalogue) -> Vec<String> {
        let mut names = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = page(catalogue, cursor.as_deref());
            names.extend(page.items.iter().map(|i| i.symbol.raw.clone()));
            match page.cursor {
                Some(next) => cursor = Some(next),
                None => return names,
            }
        }
    }

    // --- what the tab is supposed to show -----------------------------------------------

    #[test]
    fn the_catalogue_holds_what_the_venue_lists_not_what_is_subscribed() {
        // The defect from report 09 §3.4: the two are different things, and only the first
        // answers "what can I trade?".
        let catalogue = populated(3_500);
        assert_eq!(catalogue.len(ExchangeId::Binance, MarketKind::LinearPerp), 3_500);
        assert_eq!(catalogue.len(ExchangeId::Binance, MarketKind::Spot), 0, "markets are separate");
    }

    #[test]
    fn an_unpopulated_market_is_distinguishable_from_an_empty_one() {
        // "Nothing listed" and "never asked" need different answers on screen.
        let mut catalogue = Catalogue::new();
        assert!(catalogue.page(ExchangeId::Binance, MarketKind::Spot, None).is_none());

        catalogue.replace(ExchangeId::Binance, MarketKind::Spot, vec![]);
        let page = catalogue.page(ExchangeId::Binance, MarketKind::Spot, None).expect("populated");
        assert!(page.items.is_empty() && page.cursor.is_none());
    }

    // --- paging -----------------------------------------------------------------------------

    #[test]
    fn a_large_list_arrives_in_pages_and_all_of_it_arrives() {
        let catalogue = populated(5_001);
        let names = walk(&catalogue);
        assert_eq!(names.len(), 5_001, "every instrument must be delivered exactly once");

        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "an instrument arrived twice");
    }

    #[test]
    fn a_page_is_capped_and_the_last_one_says_so() {
        let catalogue = populated(2_500);
        let first = page(&catalogue, None);
        assert_eq!(first.items.len(), INSTRUMENTS_PAGE);
        assert!(first.cursor.is_some(), "more to come");

        let second = page(&catalogue, first.cursor.as_deref());
        assert_eq!(second.items.len(), 500);
        assert!(second.cursor.is_none(), "the last page must not ask for another");
    }

    #[test]
    fn a_list_that_fits_in_one_page_has_no_cursor() {
        let catalogue = populated(10);
        let only = page(&catalogue, None);
        assert_eq!(only.items.len(), 10);
        assert!(only.cursor.is_none());
    }

    #[test]
    fn a_delisting_between_pages_does_not_skip_the_next_instrument() {
        // The failure an offset cursor produces, and the reason this one is a name. With an
        // index, removing an item shifts everything after it and one is silently missed.
        let mut catalogue = populated(4_000);
        let first = page(&catalogue, None);
        let cursor = first.cursor.clone().expect("more pages");

        // An instrument from the first page is delisted while the terminal is paging.
        let mut remaining = many(4_000);
        remaining.retain(|i| i.symbol.raw != "SYM00005USDT");
        catalogue.replace(ExchangeId::Binance, MarketKind::LinearPerp, remaining);

        let second = page(&catalogue, Some(&cursor));
        let expected_first = format!("SYM{:05}USDT", INSTRUMENTS_PAGE);
        assert_eq!(
            second.items[0].symbol.raw, expected_first,
            "the page after the cursor must start where it should, not one along"
        );
    }

    #[test]
    fn a_cursor_naming_something_no_longer_listed_still_works() {
        // It was valid when it was issued. Refusing it would make a terminal restart paging
        // from the beginning because of a delisting it does not care about.
        let mut catalogue = populated(3_000);
        catalogue.replace(
            ExchangeId::Binance,
            MarketKind::LinearPerp,
            many(3_000).into_iter().filter(|i| i.symbol.raw != "SYM01999USDT").collect(),
        );

        let next = page(&catalogue, Some("SYM01999USDT"));
        assert_eq!(next.items[0].symbol.raw, "SYM02000USDT", "paging continues past the gap");
    }

    #[test]
    fn every_page_carries_the_revision_it_was_cut_from() {
        // A terminal that sees it change mid-paging knows its pages no longer describe one
        // consistent list, and can start again rather than assembling a mixture.
        let mut catalogue = populated(3_000);
        let before = page(&catalogue, None).rev;

        catalogue.replace(ExchangeId::Binance, MarketKind::LinearPerp, many(3_001));
        assert!(page(&catalogue, None).rev > before, "the revision must move when the list does");
    }

    #[test]
    fn pages_come_back_in_a_stable_order() {
        // Hash order would make two consecutive full walks return different sequences, and a
        // name cursor meaningless.
        let catalogue = populated(100);
        assert_eq!(walk(&catalogue), walk(&catalogue));

        let names = walk(&catalogue);
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "the order must be by name");
    }

    // --- change detection ----------------------------------------------------------------------

    #[test]
    fn a_new_listing_is_reported_as_added() {
        // What an alert on a new listing fires from, and what saves a terminal diffing two
        // lists of several thousand entries itself.
        let mut catalogue = populated(10);
        let mut next = many(10);
        next.push(instrument("NEWCOINUSDT"));

        let changed = catalogue.replace(ExchangeId::Binance, MarketKind::LinearPerp, next);
        assert_eq!(changed.added.len(), 1);
        assert_eq!(changed.added[0].raw, "NEWCOINUSDT");
        assert!(changed.removed.is_empty() && changed.altered.is_empty());
    }

    #[test]
    fn a_delisting_is_reported_as_removed() {
        // Replacement rather than merge: an instrument missing from the venue's answer has
        // been delisted, and a merge would keep it alive forever.
        let mut catalogue = populated(10);
        let fewer: Vec<_> = many(10).into_iter().filter(|i| i.symbol.raw != "SYM00003USDT").collect();

        let changed = catalogue.replace(ExchangeId::Binance, MarketKind::LinearPerp, fewer);
        assert_eq!(changed.removed.len(), 1);
        assert_eq!(changed.removed[0].raw, "SYM00003USDT");
        assert!(catalogue
            .get(&Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "SYM00003USDT"))
            .is_none());
    }

    #[test]
    fn a_changed_tick_size_is_reported_as_altered() {
        // A trader with a resting order at a price that is no longer on the grid needs to know
        // before the venue rejects the next amend.
        let mut catalogue = populated(5);
        let mut next = many(5);
        next[2].tick_size = dec!(0.1);

        let changed = catalogue.replace(ExchangeId::Binance, MarketKind::LinearPerp, next);
        assert_eq!(changed.altered.len(), 1);
        assert!(changed.added.is_empty() && changed.removed.is_empty());
    }

    #[test]
    fn a_halt_is_reported_as_altered_not_as_a_delisting() {
        // They are different: a halt ends, a delisting does not, and a terminal that removed
        // the tab would lose the position still open on it.
        let mut catalogue = populated(5);
        let mut next = many(5);
        next[1].trading = false;

        let changed = catalogue.replace(ExchangeId::Binance, MarketKind::LinearPerp, next);
        assert_eq!(changed.altered.len(), 1);
        assert!(changed.removed.is_empty(), "a halted instrument is still listed");
    }

    #[test]
    fn an_unchanged_refresh_reports_nothing_and_does_not_move_the_revision() {
        // The common case: the catalogue is refreshed on a timer, and almost every refresh is
        // identical. Reporting it would flood the channel and invalidate every cursor.
        let mut catalogue = populated(1_000);
        let rev = catalogue.rev();

        let changed = catalogue.replace(ExchangeId::Binance, MarketKind::LinearPerp, many(1_000));
        assert!(changed.is_empty());
        assert_eq!(catalogue.rev(), rev, "an identical refresh must not invalidate cursors");
    }

    #[test]
    fn a_duplicate_in_the_venues_answer_is_collapsed() {
        // Malformed input from a venue must not produce two entries that disagree, and must
        // not break the binary search that lookup depends on.
        let mut catalogue = Catalogue::new();
        let doubled = vec![instrument("BTCUSDT"), instrument("BTCUSDT"), instrument("ETHUSDT")];

        catalogue.replace(ExchangeId::Binance, MarketKind::LinearPerp, doubled);
        assert_eq!(catalogue.len(ExchangeId::Binance, MarketKind::LinearPerp), 2);
    }

    // --- the subscription gate -----------------------------------------------------------------------

    #[test]
    fn an_unlisted_symbol_cannot_be_subscribed_to() {
        // This is what the subscription gate calls, so the message reaches an operator who
        // typed a ticker that does not exist.
        let catalogue = populated(5);
        let unknown = Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "NOSUCHUSDT");
        let refusal = catalogue.is_tradable(&unknown).unwrap_err();
        assert!(refusal.contains("NOSUCHUSDT") && refusal.contains("not listed"), "got: {refusal}");
    }

    #[test]
    fn a_halted_symbol_is_refused_with_a_different_reason() {
        // "Halted" and "does not exist" call for different actions: wait, or check the spelling.
        let mut catalogue = Catalogue::new();
        let mut halted = instrument("BTCUSDT");
        halted.trading = false;
        catalogue.replace(ExchangeId::Binance, MarketKind::LinearPerp, vec![halted]);

        let refusal = catalogue
            .is_tradable(&Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT"))
            .unwrap_err();
        assert!(refusal.contains("halted"), "got: {refusal}");
    }

    #[test]
    fn a_listed_and_trading_symbol_passes() {
        let catalogue = populated(5);
        let ok = Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "SYM00000USDT");
        assert!(catalogue.is_tradable(&ok).is_ok());
    }

    #[test]
    fn lookup_finds_an_instrument_without_scanning_the_list() {
        // Called per order to check the tick grid, so it has to be a search rather than a
        // walk of several thousand entries.
        let catalogue = populated(3_000);
        let target = Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "SYM02999USDT");
        assert_eq!(catalogue.get(&target).map(|i| i.symbol.raw.as_str()), Some("SYM02999USDT"));
        assert_eq!(catalogue.get(&Symbol::new(ExchangeId::Binance, MarketKind::Spot, "SYM00001USDT")), None);
    }

    #[test]
    fn the_terms_comparison_covers_every_field_a_trader_would_notice() {
        // Field by field rather than whole-struct equality, so that a field added later has to
        // be considered rather than silently ignored.
        /// A named change to one field, so a failure says which field went unnoticed.
        type Mutation = (&'static str, fn(&mut Instrument));

        let base = instrument("BTCUSDT");
        let mutations: [Mutation; 6] = [
            ("tick_size", |i| i.tick_size = dec!(1)),
            ("step_size", |i| i.step_size = dec!(1)),
            ("min_qty", |i| i.min_qty = dec!(9)),
            ("min_notional", |i| i.min_notional = Decimal::ZERO),
            ("trading", |i| i.trading = false),
            ("margin_asset", |i| i.margin_asset = "BUSD".into()),
        ];
        for (what, mutate) in mutations {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert!(!same_terms(&base, &changed), "a change to {what} went unnoticed");
        }
        assert!(same_terms(&base, &base.clone()));
    }
}
