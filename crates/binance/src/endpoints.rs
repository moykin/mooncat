//! Per-market endpoints.
//!
//! Binance runs spot and USD-M futures as separate hosts with separate path prefixes. All
//! of that divergence is confined here, so the rest of the connector is one code path
//! rather than two — which is the whole point of carrying [`MarketKind`] on the identifier.

use domain::MarketKind;
use exchange::{Error, Result};

pub struct Endpoints {
    pub rest: &'static str,
    pub ws: &'static str,
    pub exchange_info: &'static str,
    pub depth: &'static str,
}

pub const SPOT: Endpoints = Endpoints {
    rest: "https://api.binance.com",
    ws: "wss://stream.binance.com:9443/ws",
    exchange_info: "/api/v3/exchangeInfo",
    depth: "/api/v3/depth",
};

pub const USD_M: Endpoints = Endpoints {
    rest: "https://fapi.binance.com",
    ws: "wss://fstream.binance.com/ws",
    exchange_info: "/fapi/v1/exchangeInfo",
    depth: "/fapi/v1/depth",
};

impl Endpoints {
    pub fn for_market(market: MarketKind) -> Result<&'static Endpoints> {
        match market {
            MarketKind::Spot => Ok(&SPOT),
            MarketKind::LinearPerp => Ok(&USD_M),
            // COIN-M is a third host and a different contract model; it is not implemented
            // rather than silently routed to the USD-M endpoints.
            MarketKind::InversePerp => Err(Error::Unsupported { what: "coin-margined futures", market }),
        }
    }

    pub fn exchange_info_url(&self) -> String {
        format!("{}{}", self.rest, self.exchange_info)
    }

    pub fn depth_url(&self, symbol: &str, limit: u32) -> String {
        format!("{}{}?symbol={}&limit={}", self.rest, self.depth, symbol, limit)
    }
}

/// Binance depth snapshots only accept a fixed set of limits; anything else is a 400.
/// Round the caller's request up to the next legal value rather than failing.
pub fn legal_depth_limit(requested: u32) -> u32 {
    const LIMITS: [u32; 5] = [5, 10, 20, 50, 100];
    const DEEP: [u32; 3] = [500, 1000, 5000];

    LIMITS.iter().chain(DEEP.iter()).copied().find(|&l| l >= requested).unwrap_or(5000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_market_gets_its_own_host() {
        assert_eq!(Endpoints::for_market(MarketKind::Spot).unwrap().rest, "https://api.binance.com");
        assert_eq!(Endpoints::for_market(MarketKind::LinearPerp).unwrap().rest, "https://fapi.binance.com");
    }

    #[test]
    fn coin_margined_is_refused_not_misrouted() {
        // Silently sending COIN-M requests to the USD-M host would return plausible-looking
        // data for the wrong contract.
        assert!(Endpoints::for_market(MarketKind::InversePerp).is_err());
    }

    #[test]
    fn depth_limits_snap_up_to_a_value_binance_accepts() {
        assert_eq!(legal_depth_limit(1), 5);
        assert_eq!(legal_depth_limit(20), 20);
        assert_eq!(legal_depth_limit(21), 50);
        assert_eq!(legal_depth_limit(101), 500);
        assert_eq!(legal_depth_limit(99_999), 5000);
    }

    #[test]
    fn urls_are_built_per_market() {
        let usdm = Endpoints::for_market(MarketKind::LinearPerp).unwrap();
        assert_eq!(usdm.exchange_info_url(), "https://fapi.binance.com/fapi/v1/exchangeInfo");
        assert_eq!(
            usdm.depth_url("BTCUSDT", 100),
            "https://fapi.binance.com/fapi/v1/depth?symbol=BTCUSDT&limit=100"
        );
    }
}
