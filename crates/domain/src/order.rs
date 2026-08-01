//! Orders and their state machine.
//!
//! The core owns order state; the terminal only renders it. Two rules drive the design:
//! every order carries a client-generated id so a retry can never double-fill, and state
//! only ever moves forward, so an out-of-order venue message cannot resurrect a dead order.

use crate::{ids::Symbol, time::Timestamp};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub const fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }

    /// Signed multiplier for position maths: +1 to buy, -1 to sell.
    pub fn sign(self) -> Decimal {
        match self {
            Self::Buy => Decimal::ONE,
            Self::Sell => Decimal::NEGATIVE_ONE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Limit,
    Market,
    StopMarket,
    StopLimit,
    TakeProfitMarket,
    TakeProfitLimit,
}

impl OrderType {
    pub const fn needs_price(self) -> bool {
        matches!(self, Self::Limit | Self::StopLimit | Self::TakeProfitLimit)
    }

    pub const fn needs_trigger_price(self) -> bool {
        matches!(self, Self::StopMarket | Self::StopLimit | Self::TakeProfitMarket | Self::TakeProfitLimit)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    /// Good till cancelled.
    #[default]
    Gtc,
    /// Immediate or cancel.
    Ioc,
    /// Fill or kill.
    Fok,
    /// Post only: rejected rather than taking liquidity.
    PostOnly,
}

/// Which leg of a hedged position an order applies to.
///
/// `Both` is one-way mode, where a single net position is held per instrument.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionSide {
    #[default]
    Both,
    Long,
    Short,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Handed to the connector, no venue acknowledgement yet. Local-only state: an order
    /// that dies here may still be live on the venue and must be reconciled at startup.
    Pending,
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
    Expired,
}

impl OrderStatus {
    /// Terminal states never change again, so they can be archived out of the hot map.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Filled | Self::Canceled | Self::Rejected | Self::Expired)
    }

    pub const fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::New | Self::PartiallyFilled)
    }

    /// Rank used to reject out-of-order venue updates.
    const fn rank(self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::New => 1,
            Self::PartiallyFilled => 2,
            Self::Filled | Self::Canceled | Self::Rejected | Self::Expired => 3,
        }
    }

    /// Whether `next` is a legal successor.
    ///
    /// Venues deliver updates out of order after a reconnect; without this guard a stale
    /// `New` arriving after a `Filled` would reopen a closed order.
    pub const fn can_advance_to(self, next: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        next.rank() > self.rank() || (next.rank() == self.rank() && next.rank() == 2)
    }
}

/// Client-generated order id, unique per core instance.
///
/// Sent to the venue so a retried placement is deduplicated on their side rather than
/// producing a second position.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientOrderId(pub String);

impl fmt::Display for ClientOrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An order request, before the venue has seen it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewOrder {
    pub client_id: ClientOrderId,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub qty: Decimal,
    /// Required when [`OrderType::needs_price`].
    pub price: Option<Decimal>,
    /// Required when [`OrderType::needs_trigger_price`].
    pub trigger_price: Option<Decimal>,
    pub tif: TimeInForce,
    pub position_side: PositionSide,
    /// Derivatives only: the order may only shrink an existing position.
    pub reduce_only: bool,
}

/// A live or historical order as the core understands it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub client_id: ClientOrderId,
    /// Venue id, absent until the venue acknowledges the order.
    pub venue_id: Option<String>,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub status: OrderStatus,
    pub qty: Decimal,
    pub filled_qty: Decimal,
    pub price: Option<Decimal>,
    pub trigger_price: Option<Decimal>,
    /// Volume-weighted average fill price; zero until the first fill.
    pub avg_price: Decimal,
    pub tif: TimeInForce,
    pub position_side: PositionSide,
    pub reduce_only: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Order {
    /// Quantity still working on the venue.
    pub fn remaining_qty(&self) -> Decimal {
        (self.qty - self.filled_qty).max(Decimal::ZERO)
    }

    pub fn is_open(&self) -> bool {
        self.status.is_open()
    }
}

/// A single execution against an order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    pub client_id: ClientOrderId,
    pub symbol: Symbol,
    /// Venue execution id, used to drop duplicates replayed after a reconnect.
    pub trade_id: String,
    pub side: Side,
    pub price: Decimal,
    pub qty: Decimal,
    pub fee: Decimal,
    pub fee_asset: String,
    /// True when the fill added liquidity.
    pub is_maker: bool,
    pub ts: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn side_sign_drives_position_maths() {
        assert_eq!(Side::Buy.sign(), dec!(1));
        assert_eq!(Side::Sell.sign(), dec!(-1));
        assert_eq!(Side::Buy.opposite(), Side::Sell);
    }

    #[test]
    fn a_terminal_order_can_never_be_reopened() {
        // The scenario this exists for: a stale `New` arriving after the fill.
        assert!(!OrderStatus::Filled.can_advance_to(OrderStatus::New));
        assert!(!OrderStatus::Canceled.can_advance_to(OrderStatus::PartiallyFilled));
        assert!(!OrderStatus::Rejected.can_advance_to(OrderStatus::New));
    }

    #[test]
    fn state_only_moves_forward() {
        assert!(OrderStatus::Pending.can_advance_to(OrderStatus::New));
        assert!(OrderStatus::New.can_advance_to(OrderStatus::PartiallyFilled));
        assert!(OrderStatus::PartiallyFilled.can_advance_to(OrderStatus::Filled));
        assert!(!OrderStatus::PartiallyFilled.can_advance_to(OrderStatus::New));
    }

    #[test]
    fn successive_partial_fills_are_allowed() {
        // The one legal same-rank transition: partial -> partial as more volume fills.
        assert!(OrderStatus::PartiallyFilled.can_advance_to(OrderStatus::PartiallyFilled));
        assert!(!OrderStatus::New.can_advance_to(OrderStatus::New));
    }

    #[test]
    fn remaining_qty_never_goes_negative() {
        let mut order = Order {
            client_id: ClientOrderId("c-1".into()),
            venue_id: None,
            symbol: crate::ids::Symbol::new(
                crate::ids::ExchangeId::Binance,
                crate::ids::MarketKind::LinearPerp,
                "BTCUSDT",
            ),
            side: Side::Buy,
            order_type: OrderType::Limit,
            status: OrderStatus::PartiallyFilled,
            qty: dec!(1),
            filled_qty: dec!(0.4),
            price: Some(dec!(64000)),
            trigger_price: None,
            avg_price: dec!(64000),
            tif: TimeInForce::Gtc,
            position_side: PositionSide::Both,
            reduce_only: false,
            created_at: Timestamp::from_millis(1),
            updated_at: Timestamp::from_millis(2),
        };
        assert_eq!(order.remaining_qty(), dec!(0.6));

        // Venues occasionally report a filled quantity a hair above the order size.
        order.filled_qty = dec!(1.0001);
        assert_eq!(order.remaining_qty(), Decimal::ZERO);
    }

    #[test]
    fn order_type_declares_the_fields_it_needs() {
        assert!(OrderType::Limit.needs_price());
        assert!(!OrderType::Market.needs_price());
        assert!(OrderType::StopLimit.needs_trigger_price());
        assert!(!OrderType::Limit.needs_trigger_price());
    }
}
