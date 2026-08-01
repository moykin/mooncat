//! Venue, market and instrument identity.
//!
//! `MarketKind` is threaded through every type from day one: spot is modelled as the
//! reduced case of a derivatives market, never the other way round. Adding a venue or a
//! market kind must not require touching order, book or position types.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A trading venue we hold a connector for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExchangeId {
    Binance,
    Bybit,
}

impl ExchangeId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Bybit => "bybit",
        }
    }
}

impl fmt::Display for ExchangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which market of a venue an instrument belongs to.
///
/// The account model differs per kind: spot has balances only, perpetuals add positions,
/// leverage and a margin mode. Code that must branch does so on this enum rather than on
/// the venue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketKind {
    /// Spot market. No positions, no leverage.
    Spot,
    /// USDT/USDC-margined perpetual. Binance calls this USD-M.
    LinearPerp,
    /// Coin-margined perpetual. Binance calls this COIN-M.
    InversePerp,
}

impl MarketKind {
    /// Whether the market carries positions, leverage and a margin mode.
    pub const fn has_positions(self) -> bool {
        matches!(self, Self::LinearPerp | Self::InversePerp)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::LinearPerp => "linear_perp",
            Self::InversePerp => "inverse_perp",
        }
    }
}

impl fmt::Display for MarketKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A fully qualified instrument: venue + market + the venue's own ticker.
///
/// `raw` is kept exactly as the venue spells it (`BTCUSDT`), because it is what goes back
/// out on the wire. Normalisation for display lives in [`crate::instrument::Instrument`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Symbol {
    pub exchange: ExchangeId,
    pub market: MarketKind,
    pub raw: String,
}

impl Symbol {
    pub fn new(exchange: ExchangeId, market: MarketKind, raw: impl Into<String>) -> Self {
        Self { exchange, market, raw: raw.into() }
    }

    /// Stable key for maps, logs and the wire protocol: `binance:linear_perp:BTCUSDT`.
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.exchange, self.market, self.raw)
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.exchange, self.market, self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_has_no_positions_derivatives_do() {
        assert!(!MarketKind::Spot.has_positions());
        assert!(MarketKind::LinearPerp.has_positions());
        assert!(MarketKind::InversePerp.has_positions());
    }

    #[test]
    fn symbol_key_is_stable_and_qualified() {
        let s = Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT");
        assert_eq!(s.key(), "binance:linear_perp:BTCUSDT");
        // The same ticker on two markets must never collide.
        let spot = Symbol::new(ExchangeId::Binance, MarketKind::Spot, "BTCUSDT");
        assert_ne!(s.key(), spot.key());
    }
}
