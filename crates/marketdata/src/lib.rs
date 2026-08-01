//! Venue-agnostic market-data state.
//!
//! Connectors translate wire formats; this crate holds what must be *maintained* — today
//! the order books, later candle aggregation and the tape.

pub mod books;
pub mod candles;

pub use books::{BookSet, Need, Outcome};
pub use candles::{CandleSet, Update};
