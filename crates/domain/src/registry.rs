//! Short identifiers for instruments, and the epoch that makes reusing them safe.
//!
//! # What this saves
//!
//! `binance:linear_perp:BTCUSDT` is twenty-six bytes and rides on every book delta, every
//! print, every candle — thousands of times a second. Four bytes instead is not a
//! micro-optimisation at that rate; it is most of the payload on the hottest channel.
//!
//! # Why an epoch is not optional
//!
//! Small identifiers are only safe while both sides agree what they mean, and they are
//! assigned in the order instruments happen to be subscribed to. A core that restarts assigns
//! them again, in a different order. A terminal that kept the old mapping and reconnected
//! would apply BTCUSDT's deltas to whatever now holds id 3 — silently, with a book that looks
//! entirely plausible.
//!
//! So every mapping carries the `dict_epoch` it was issued under, and an id from a foreign
//! epoch is refused rather than resolved. The epoch changes whenever the mapping could have,
//! which in practice means on every core start.
//!
//! # Why ids are never recycled within an epoch
//!
//! Unsubscribing does not free an id. Frames for a symbol can still be in flight when the
//! unsubscribe is processed, and handing that number to a different instrument would mean the
//! terminal applies them to the wrong one. Four billion ids against a few thousand instruments
//! is not a budget worth managing.

use crate::ids::Symbol;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// An instrument's short form within one [`dict_epoch`](SymbolRegistry::epoch).
///
/// Deliberately not `From<u32>`: constructing one out of a bare number is exactly the mistake
/// the epoch exists to catch, and it should have to go through a registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SymbolId(pub u32);

impl std::fmt::Display for SymbolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sid:{}", self.0)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("symbol id {0} was issued under dictionary epoch {issued}, this core is at {current}", issued = .1, current = .2)]
    ForeignEpoch(SymbolId, u64, u64),
    #[error("symbol id {0} has never been issued")]
    Unknown(SymbolId),
}

/// The core's assignment of short ids to instruments.
///
/// One per core lifetime. The terminal holds a copy built from what the core told it, and both
/// sides tag it with the same epoch.
#[derive(Debug)]
pub struct SymbolRegistry {
    epoch: u64,
    /// Index is the id; the vector is append-only, which is what makes ids permanent.
    symbols: Vec<Symbol>,
    by_key: HashMap<String, SymbolId>,
}

impl SymbolRegistry {
    /// `epoch` must differ from every previous core lifetime. Deriving it from the start time
    /// is enough, and is what the core does.
    pub fn new(epoch: u64) -> Self {
        Self { epoch, symbols: Vec::new(), by_key: HashMap::new() }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    /// Id for a symbol, assigning one if it has none.
    ///
    /// Idempotent: the same symbol always comes back with the same id within an epoch, so a
    /// terminal resubscribing does not invalidate what it already holds.
    pub fn intern(&mut self, symbol: &Symbol) -> SymbolId {
        let key = symbol.key();
        if let Some(id) = self.by_key.get(&key) {
            return *id;
        }
        let id = SymbolId(self.symbols.len() as u32);
        self.symbols.push(symbol.clone());
        self.by_key.insert(key, id);
        id
    }

    /// Id for a symbol, without assigning one.
    pub fn lookup(&self, symbol: &Symbol) -> Option<SymbolId> {
        self.by_key.get(&symbol.key()).copied()
    }

    /// Resolve an id issued under `their_epoch`.
    ///
    /// The epoch is checked first and separately from existence, because the two failures need
    /// different answers: a foreign epoch means the terminal must throw away its whole mapping,
    /// an unknown id means it asked about something that was never issued.
    pub fn resolve(&self, id: SymbolId, their_epoch: u64) -> Result<&Symbol, ResolveError> {
        if their_epoch != self.epoch {
            return Err(ResolveError::ForeignEpoch(id, their_epoch, self.epoch));
        }
        self.symbols.get(id.0 as usize).ok_or(ResolveError::Unknown(id))
    }

    /// Everything issued so far, for the start-of-session handover.
    pub fn assignments(&self) -> Vec<(Symbol, SymbolId)> {
        self.symbols.iter().cloned().zip((0..).map(SymbolId)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ExchangeId, MarketKind};

    fn sym(raw: &str) -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, raw)
    }

    #[test]
    fn interning_is_idempotent_within_an_epoch() {
        // A terminal resubscribing must not invalidate ids it already holds.
        let mut registry = SymbolRegistry::new(1);
        let first = registry.intern(&sym("BTCUSDT"));
        assert_eq!(registry.intern(&sym("BTCUSDT")), first);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn different_instruments_get_different_ids() {
        let mut registry = SymbolRegistry::new(1);
        let btc = registry.intern(&sym("BTCUSDT"));
        let eth = registry.intern(&sym("ETHUSDT"));
        assert_ne!(btc, eth);

        // The same ticker on a different market is a different instrument, and confusing the
        // two would mean spot deltas applied to a futures book.
        let spot = registry.intern(&Symbol::new(ExchangeId::Binance, MarketKind::Spot, "BTCUSDT"));
        assert_ne!(spot, btc);
    }

    #[test]
    fn an_id_from_a_foreign_epoch_is_refused() {
        // The acceptance criterion for task 3.1, and the failure it prevents is silent: the
        // terminal would apply one instrument's deltas to another with a plausible-looking book.
        let mut registry = SymbolRegistry::new(7);
        let id = registry.intern(&sym("BTCUSDT"));

        assert!(registry.resolve(id, 7).is_ok());
        assert_eq!(registry.resolve(id, 6), Err(ResolveError::ForeignEpoch(id, 6, 7)));
    }

    #[test]
    fn the_epoch_is_checked_before_existence() {
        // The two failures need different answers: a foreign epoch means throw the whole
        // mapping away, an unknown id means one lookup went wrong.
        let registry = SymbolRegistry::new(2);
        assert!(matches!(registry.resolve(SymbolId(999), 1), Err(ResolveError::ForeignEpoch(..))));
        assert!(matches!(registry.resolve(SymbolId(999), 2), Err(ResolveError::Unknown(_))));
    }

    #[test]
    fn ids_are_stable_and_never_recycled() {
        // Frames for a symbol can still be in flight when it is unsubscribed. Handing its
        // number to something else would mean applying them to the wrong instrument.
        let mut registry = SymbolRegistry::new(1);
        let btc = registry.intern(&sym("BTCUSDT"));
        let eth = registry.intern(&sym("ETHUSDT"));

        // There is deliberately no way to remove one, so this is as much a statement about the
        // API as about the data.
        assert_eq!(registry.resolve(btc, 1).unwrap().raw, "BTCUSDT");
        assert_eq!(registry.resolve(eth, 1).unwrap().raw, "ETHUSDT");
        assert_eq!(registry.intern(&sym("BTCUSDT")), btc, "still the same after others joined");
    }

    #[test]
    fn a_symbol_still_describes_itself_in_full() {
        // The short form is for the wire. Logs, database rows and error messages keep the
        // readable key, because `sid:3` in a log at three in the morning says nothing.
        let mut registry = SymbolRegistry::new(1);
        let id = registry.intern(&sym("BTCUSDT"));
        assert_eq!(registry.resolve(id, 1).unwrap().key(), "binance:linear_perp:BTCUSDT");
        assert_eq!(id.to_string(), "sid:0", "and the id prints as obviously an id");
    }

    #[test]
    fn lookup_does_not_assign() {
        // A path that resolved by assigning would quietly grow the dictionary from a query.
        let mut registry = SymbolRegistry::new(1);
        assert!(registry.lookup(&sym("BTCUSDT")).is_none());
        assert!(registry.is_empty());

        registry.intern(&sym("BTCUSDT"));
        assert!(registry.lookup(&sym("BTCUSDT")).is_some());
    }

    #[test]
    fn the_assignment_list_is_ordered_by_id() {
        // It is sent at the start of a session, and a terminal building its own table from it
        // relies on position matching the id.
        let mut registry = SymbolRegistry::new(1);
        for raw in ["BTCUSDT", "ETHUSDT", "SOLUSDT"] {
            registry.intern(&sym(raw));
        }
        let assignments = registry.assignments();
        for (index, (symbol, id)) in assignments.iter().enumerate() {
            assert_eq!(*id, SymbolId(index as u32));
            assert_eq!(registry.resolve(*id, 1).unwrap(), symbol);
        }
    }

    #[test]
    fn an_id_is_four_bytes_on_the_wire_not_a_nested_value() {
        // The entire point. A map or a struct here would give back what the short form saves.
        let encoded = rmp_serde::to_vec_named(&SymbolId(70_000)).unwrap();
        assert!(encoded.len() <= 5, "encoded as {} bytes: {encoded:02x?}", encoded.len());
        assert_eq!(rmp_serde::from_slice::<SymbolId>(&encoded).unwrap(), SymbolId(70_000));
    }

    #[test]
    fn the_saving_against_a_full_key_is_what_it_is_supposed_to_be() {
        // Measured rather than asserted, because it is the whole justification for the epoch
        // machinery that comes with it.
        let symbol = sym("BTCUSDT");
        let long = rmp_serde::to_vec_named(&symbol.key()).unwrap().len();
        let short = rmp_serde::to_vec_named(&SymbolId(1)).unwrap().len();
        println!("{long} bytes as a key, {short} as an id");
        assert!(short * 4 < long, "the short form must be worth the epoch it requires");
    }
}
