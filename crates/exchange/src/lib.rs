//! The contract every venue connector fulfils.
//!
//! Splitting market data from trading is deliberate: a core can run a data-only connector
//! with no API keys at all, and the risk engine can revoke [`TradingVenue`] access without
//! disturbing the feed.
//!
//! Connectors do not return streams. They push [`domain::Event`] into a sink handed to them
//! at construction, which is what makes one connector serve many consumers — the terminal,
//! a strategy, the tick recorder — without any of them knowing about the others.

use async_trait::async_trait;
use domain::{
    Balance, ClientOrderId, Decimal, Event, ExchangeId, Instrument, MarketKind, NewOrder, Order, OrderBook,
    Position, Symbol,
};

pub mod sink;

pub use sink::EventSink;

/// What a connector can fail with.
///
/// Callers must be able to distinguish "the venue said no" from "the network blinked":
/// the first is final, the second is worth retrying. Conflating them either spams the venue
/// or silently drops orders.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport failure. Retryable.
    #[error("transport: {0}")]
    Transport(String),

    /// The venue rejected the request on its merits. Not retryable as-is.
    #[error("venue rejected ({code}): {message}")]
    Rejected { code: String, message: String },

    /// Rate limit hit. Retryable after `retry_after_ms`, if the venue told us.
    #[error("rate limited (retry after {retry_after_ms:?} ms)")]
    RateLimited { retry_after_ms: Option<u64> },

    /// Credentials missing, wrong, or lacking the permission the call needs.
    #[error("auth: {0}")]
    Auth(String),

    /// The venue sent something we could not interpret. Never retry blindly — a parse
    /// failure on an order update means local state is already suspect.
    #[error("malformed venue response: {0}")]
    Malformed(String),

    /// The connector does not offer this on this market.
    #[error("{what} is not supported on {market}")]
    Unsupported { what: &'static str, market: MarketKind },
}

impl Error {
    /// Whether retrying the same request unchanged could succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::RateLimited { .. })
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// API credentials for one venue account. Never leave the core process.
#[derive(Clone)]
pub struct Credentials {
    pub api_key: String,
    pub api_secret: String,
}

impl std::fmt::Debug for Credentials {
    /// Hand-written so a stray `{:?}` in a log cannot leak the account.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("api_key", &"<redacted>")
            .field("api_secret", &"<redacted>")
            .finish()
    }
}

/// What market data to stream for an instrument.
///
/// Serialisable because a terminal asks for these across the wire.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Subscription {
    /// Public trade tape.
    Trades(Symbol),
    /// Incremental order book. The connector owns snapshot fetching and gap resync.
    Book(Symbol),
    Candles {
        symbol: Symbol,
        interval_ms: i64,
    },
}

impl Subscription {
    /// The instrument this subscription concerns. Used to route a request to the connector
    /// that owns its market, and to decide which sessions an event belongs to.
    pub fn symbol(&self) -> &Symbol {
        match self {
            Self::Trades(s) | Self::Book(s) => s,
            Self::Candles { symbol, .. } => symbol,
        }
    }
}

/// Public market data. Requires no credentials.
#[async_trait]
pub trait MarketDataSource: Send + Sync {
    /// Tradable instruments with their tick and step grids.
    async fn instruments(&self, market: MarketKind) -> Result<Vec<Instrument>>;

    /// Start streaming. Events arrive on the sink given at construction.
    async fn subscribe(&self, subs: &[Subscription]) -> Result<()>;

    async fn unsubscribe(&self, subs: &[Subscription]) -> Result<()>;

    /// Fetch a fresh book snapshot. Used to recover from a sequence gap.
    async fn book_snapshot(&self, symbol: &Symbol, depth: u32) -> Result<OrderBook>;
}

/// Order entry and account state. Requires credentials.
#[async_trait]
pub trait TradingVenue: Send + Sync {
    async fn place_order(&self, order: &NewOrder) -> Result<Order>;

    async fn cancel_order(&self, symbol: &Symbol, client_id: &ClientOrderId) -> Result<()>;

    /// Change price and/or quantity in place where the venue supports it. Connectors that
    /// cannot must return [`Error::Unsupported`] rather than emulating it with
    /// cancel-and-replace — that decision belongs to the OMS, which knows the risk.
    async fn amend_order(
        &self,
        symbol: &Symbol,
        client_id: &ClientOrderId,
        new_price: Option<Decimal>,
        new_qty: Option<Decimal>,
    ) -> Result<Order>;

    /// Everything currently working. The source of truth for startup reconciliation.
    async fn open_orders(&self, market: MarketKind) -> Result<Vec<Order>>;

    async fn balances(&self, market: MarketKind) -> Result<Vec<Balance>>;

    /// Open positions. Empty on spot, where [`MarketKind::has_positions`] is false.
    async fn positions(&self, market: MarketKind) -> Result<Vec<Position>>;

    async fn set_leverage(&self, symbol: &Symbol, leverage: Decimal) -> Result<()>;

    /// Subscribe to the private order/fill/balance stream.
    async fn subscribe_account(&self, market: MarketKind) -> Result<()>;
}

/// A fully capable connector.
pub trait Exchange: MarketDataSource + TradingVenue {
    fn id(&self) -> ExchangeId;

    /// Markets this connector implements. Callers check here before asking for a market.
    fn markets(&self) -> &[MarketKind];

    fn supports(&self, market: MarketKind) -> bool {
        self.markets().contains(&market)
    }
}

/// Convenience for connectors: build the sink-pushing half of a connector.
pub fn event_channel() -> (EventSink, tokio::sync::mpsc::UnboundedReceiver<Event>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (EventSink::new(tx), rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_transient_failures_are_retryable() {
        assert!(Error::Transport("reset".into()).is_retryable());
        assert!(Error::RateLimited { retry_after_ms: Some(500) }.is_retryable());

        // A rejection retried unchanged just gets rejected again.
        assert!(
            !Error::Rejected { code: "-2010".into(), message: "insufficient balance".into() }.is_retryable()
        );
        assert!(!Error::Auth("bad signature".into()).is_retryable());
        // A malformed order update means local state is already suspect: resync, do not retry.
        assert!(!Error::Malformed("unknown status".into()).is_retryable());
    }

    #[test]
    fn credentials_never_render_their_secret() {
        let creds = Credentials { api_key: "AK_LIVE_1234".into(), api_secret: "SUPERSECRET".into() };
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("SUPERSECRET"));
        assert!(!rendered.contains("AK_LIVE_1234"));
    }
}
