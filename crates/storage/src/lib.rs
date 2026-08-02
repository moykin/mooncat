//! Durable state.
//!
//! What survives a restart: closed deals, the orders that made them, fills, candles and the
//! audit trail. Not the book — that is rebuilt from the venue in seconds and storing it would
//! be storing something already stale.
//!
//! The one thing to read before changing anything here is [`store`]: its writer discipline is
//! copied wholesale from MoonTerminal, where it was arrived at the expensive way.

pub mod read;
pub mod retention;
pub mod schema;
pub mod store;

pub use read::{PeriodSummary, Reader};
pub use retention::{sweep, Policy, Swept};
pub use schema::{MigrationError, Migrator};
pub use store::{Store, StoreError, Value, Write, WriteAck};
