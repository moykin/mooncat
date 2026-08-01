//! Startup configuration.
//!
//! Read from the environment, not a file committed next to the binary — the token is the
//! only thing standing between the open internet and an account, and files get copied into
//! repositories.

use domain::MarketKind;
use std::net::SocketAddr;

pub struct Config {
    pub bind: SocketAddr,
    pub token: String,
    pub markets: Vec<MarketKind>,
    /// Tickers to stream, as the venue spells them.
    pub tickers: Vec<String>,
}

const DEFAULT_BIND: &str = "127.0.0.1:8787";

impl Config {
    pub fn from_env(args: Vec<String>) -> Result<Self, String> {
        let token = std::env::var("MOON_TOKEN")
            .map_err(|_| "MOON_TOKEN is not set — a core without a token would serve anyone".to_string())?;
        if token.len() < 16 {
            return Err("MOON_TOKEN is shorter than 16 characters".into());
        }

        // Loopback by default. Binding to every interface is a decision that has to be made
        // out loud, once the operator has a firewall rule to match.
        let bind_raw = std::env::var("MOON_BIND").unwrap_or_else(|_| DEFAULT_BIND.into());
        let bind = bind_raw.parse().map_err(|e| format!("MOON_BIND `{bind_raw}` is not an address: {e}"))?;

        let tickers = if args.is_empty() { vec!["BTCUSDT".to_string()] } else { args };

        Ok(Self {
            bind,
            token,
            markets: vec![MarketKind::Spot, MarketKind::LinearPerp],
            tickers: tickers.into_iter().map(|t| t.to_uppercase()).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `std::env` is process-global, so these run under one lock rather than in parallel.
    ///
    /// Every variable the config reads is cleared first. Without that, a test that leaves
    /// `MOON_BIND` set to something invalid makes an unrelated test fail depending on the
    /// order they happen to run in — which is exactly how this flaked.
    fn with_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        for key in ["MOON_TOKEN", "MOON_BIND"] {
            std::env::remove_var(key);
        }
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        f()
    }

    #[test]
    fn a_missing_token_refuses_to_start() {
        // Defaulting to "no auth" would be the single worst failure this program could have.
        with_env(&[("MOON_TOKEN", None)], || {
            assert!(Config::from_env(vec![]).is_err());
        });
    }

    #[test]
    fn a_short_token_refuses_to_start() {
        with_env(&[("MOON_TOKEN", Some("hunter2"))], || {
            assert!(Config::from_env(vec![]).is_err());
        });
    }

    #[test]
    fn defaults_are_loopback_and_btcusdt_on_both_markets() {
        with_env(&[("MOON_TOKEN", Some("0123456789abcdef")), ("MOON_BIND", None)], || {
            let cfg = Config::from_env(vec![]).unwrap();
            assert_eq!(cfg.bind.to_string(), DEFAULT_BIND);
            assert_eq!(cfg.tickers, vec!["BTCUSDT"]);
            assert_eq!(cfg.markets, vec![MarketKind::Spot, MarketKind::LinearPerp]);
        });
    }

    #[test]
    fn tickers_come_from_the_command_line_and_are_normalised() {
        with_env(&[("MOON_TOKEN", Some("0123456789abcdef"))], || {
            let cfg = Config::from_env(vec!["ethusdt".into(), "SolUsdt".into()]).unwrap();
            assert_eq!(cfg.tickers, vec!["ETHUSDT", "SOLUSDT"]);
        });
    }

    #[test]
    fn a_malformed_bind_address_is_a_startup_error() {
        with_env(&[("MOON_TOKEN", Some("0123456789abcdef")), ("MOON_BIND", Some("not-an-address"))], || {
            assert!(Config::from_env(vec![]).is_err());
        });
    }
}
