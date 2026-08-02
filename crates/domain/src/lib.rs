//! Exchange-agnostic trading vocabulary.
//!
//! Everything in the workspace speaks these types: the connectors translate venue JSON into
//! them, the core reasons in them, the wire protocol serialises them, the terminal renders
//! them. Nothing here knows about any particular venue.
//!
//! Two conventions are load-bearing and worth stating once:
//!
//! * **Money is [`rust_decimal::Decimal`], never `f64`.** Exchange prices and quantities sit
//!   on decimal grids; binary floats round off the grid and the venue rejects the order.
//! * **Spot is the reduced case of a derivatives market.** [`ids::MarketKind`] is present on
//!   every identifier from the start, so adding perpetuals never reshapes these types.

pub mod account;
pub mod event;
pub mod ids;
pub mod instrument;
pub mod market;
pub mod order;
pub mod registry;
pub mod scaled;
pub mod time;

pub use account::{Balance, MarginMode, Position};
pub use event::{AccountEvent, ConnectionEvent, Event, MarketEvent, Payload};
pub use ids::{ExchangeId, MarketKind, Symbol};
pub use instrument::{Instrument, OrderCheck};
pub use market::{ApplyOutcome, BookDelta, BookLevel, Bucketing, Candle, OrderBook, PublicTrade};
pub use order::{
    ClientOrderId, Fill, NewOrder, Order, OrderStatus, OrderType, PositionSide, Side, TimeInForce,
};
pub use registry::{ResolveError, SymbolId, SymbolRegistry};
pub use scaled::{ScaleError, Scaled, ScaledLevel};
pub use time::Timestamp;

pub use rust_decimal::Decimal;
