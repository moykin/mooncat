//! The event bus vocabulary.
//!
//! This is the decision that lets manual trading and automated strategies coexist without a
//! rewrite: the core publishes events, and *every* consumer is just a subscriber. The
//! terminal is one. A strategy is another. Neither knows about the other, and adding the
//! strategy engine later touches no existing code.

use crate::{
    account::{Balance, Position},
    ids::Symbol,
    instrument::Instrument,
    market::{BookDelta, Candle, OrderBook, PublicTrade},
    order::{Fill, Order},
    time::Timestamp,
};
use serde::{Deserialize, Serialize};

/// Public data, identical for every consumer and safe to fan out widely.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketEvent {
    Trade(PublicTrade),
    /// Full book state. Also emitted after a resync, so consumers must treat it as a reset.
    BookSnapshot(OrderBook),
    BookDelta(BookDelta),
    Candle(Candle),
    /// Instrument metadata refreshed; cached tick/step sizes should be re-read.
    Instruments {
        exchange_market: Symbol,
        instruments: Vec<Instrument>,
    },
}

/// Private data. Never leaves the core except over an authenticated connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountEvent {
    OrderUpdate(Order),
    Fill(Fill),
    Balances(Vec<Balance>),
    Positions(Vec<Position>),
}

/// Connector health. The terminal renders it; the risk engine acts on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionEvent {
    Connecting,
    /// Streams are live and state has been reconciled.
    Ready,
    /// Book sequence gap or stream drop; a resync is under way and data is stale until
    /// the next `BookSnapshot`.
    Resyncing {
        reason: String,
    },
    Disconnected {
        reason: String,
    },
}

/// Everything the core publishes, stamped with when it was produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub at: Timestamp,
    pub payload: Payload,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Payload {
    Market(MarketEvent),
    Account(AccountEvent),
    Connection(ConnectionEvent),
}

impl Event {
    pub fn market(at: Timestamp, event: MarketEvent) -> Self {
        Self { at, payload: Payload::Market(event) }
    }

    pub fn account(at: Timestamp, event: AccountEvent) -> Self {
        Self { at, payload: Payload::Account(event) }
    }

    pub fn connection(at: Timestamp, event: ConnectionEvent) -> Self {
        Self { at, payload: Payload::Connection(event) }
    }

    /// True for events that must not be sent to an unauthenticated consumer.
    pub fn is_private(&self) -> bool {
        matches!(self.payload, Payload::Account(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::OrderBook;

    #[test]
    fn account_events_are_private_market_events_are_not() {
        let book = Event::market(Timestamp::from_millis(1), MarketEvent::BookSnapshot(OrderBook::default()));
        assert!(!book.is_private());

        let balances = Event::account(Timestamp::from_millis(1), AccountEvent::Balances(vec![]));
        assert!(balances.is_private());
    }
}
