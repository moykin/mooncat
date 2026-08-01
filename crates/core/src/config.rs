//! Startup configuration.
//!
//! Three sources, in decreasing precedence: **a TOML file, then the environment, then
//! built-in defaults.**
//!
//! The file wins deliberately. The core runs as a service on a VPS, and a service is
//! configured by a file an operator can read, diff and back up — not by an environment
//! block assembled inside a unit file where a typo is invisible until something misbehaves.
//! The environment stays supported because it is how the thing is run during development,
//! and because a secret is better passed through `LoadCredential` than written to disk.
//!
//! The core must be able to start with **no environment variables at all**. That is a test,
//! not an aspiration: `starting_needs_no_environment_at_all`.

use domain::MarketKind;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Where the certificate and key live. Absent means plaintext, which is only tolerated on
/// loopback — see [`Config::validate`].
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Tls {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    /// Where `/metrics` and `/health` are served. Separate from `bind` so the operational
    /// surface can stay on loopback while the wire faces the network.
    pub metrics_bind: Option<SocketAddr>,
    pub token: String,
    pub markets: Vec<MarketKind>,
    /// Tickers to stream, as the venue spells them.
    pub tickers: Vec<String>,
    pub tls: Option<Tls>,
}

/// The file, with every field optional so a partial file is legal.
///
/// `deny_unknown_fields` turns a typo into a startup error naming the key, instead of a
/// setting that silently never applies. For a file whose job is to hold a bind address and
/// a certificate path, silence is the wrong failure.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    bind: Option<SocketAddr>,
    metrics_bind: Option<SocketAddr>,
    token: Option<String>,
    markets: Option<Vec<MarketKind>>,
    tickers: Option<Vec<String>>,
    tls: Option<Tls>,
}

const DEFAULT_BIND: &str = "127.0.0.1:8787";
const DEFAULT_CONFIG_PATH: &str = "config.toml";
const MIN_TOKEN_LEN: usize = 16;

impl Config {
    /// Load from `--config <path>` / `MOON_CONFIG` / `./config.toml`, then the environment,
    /// then defaults. `args` are the positional arguments left after flags are consumed.
    pub fn load(args: Vec<String>) -> Result<Self, String> {
        let (path, tickers_from_args) = split_args(args)?;
        let (file, source) = load_file(path)?;
        Self::assemble(file, source, tickers_from_args)
    }

    fn assemble(
        file: FileConfig,
        source: Option<PathBuf>,
        tickers_from_args: Vec<String>,
    ) -> Result<Self, String> {
        let token = file.token.or_else(|| std::env::var("MOON_TOKEN").ok()).ok_or_else(|| {
            "no token: set `token` in the config file or MOON_TOKEN — a core without one \
                 would serve anyone"
                .to_string()
        })?;
        if token.len() < MIN_TOKEN_LEN {
            return Err(format!("token is shorter than {MIN_TOKEN_LEN} characters"));
        }

        let bind = match file.bind {
            Some(bind) => bind,
            None => parse_env_addr("MOON_BIND")?.unwrap_or_else(|| {
                DEFAULT_BIND.parse().expect("the default bind address is a literal and parses")
            }),
        };

        let metrics_bind = match file.metrics_bind {
            Some(addr) => Some(addr),
            None => parse_env_addr("MOON_METRICS_BIND")?,
        };

        // Command-line tickers beat the file: they are what someone types when looking at a
        // specific market right now, and that intent is more immediate than the file's.
        let tickers = if !tickers_from_args.is_empty() {
            tickers_from_args
        } else {
            file.tickers.unwrap_or_else(|| vec!["BTCUSDT".to_string()])
        };
        if tickers.is_empty() {
            return Err("no tickers: the core would connect and stream nothing".into());
        }

        let config = Self {
            bind,
            metrics_bind,
            token,
            markets: file.markets.unwrap_or_else(|| vec![MarketKind::Spot, MarketKind::LinearPerp]),
            tickers: tickers.into_iter().map(|t| t.to_uppercase()).collect(),
            tls: file.tls,
        };
        config.validate(source.as_deref())?;
        Ok(config)
    }

    /// Rules that must hold before a socket is opened.
    fn validate(&self, source: Option<&Path>) -> Result<(), String> {
        if self.markets.is_empty() {
            return Err("`markets` is empty: the core would subscribe to nothing".into());
        }

        // The token is a bearer credential. On a public interface without TLS it crosses the
        // network in the clear, and whoever reads it owns the core — which owns the API keys.
        // Refusing here is the whole point of the task: the failure has to happen at startup,
        // in front of the operator, not silently at the first connection.
        if !self.bind.ip().is_loopback() && self.tls.is_none() {
            let where_to_put_it = source.map_or_else(
                || format!("create {DEFAULT_CONFIG_PATH} with a [tls] section"),
                |p| format!("add a [tls] section to {}", p.display()),
            );
            return Err(format!(
                "refusing to serve {} without TLS: the token would cross the network in the \
                 clear. Either bind to loopback, or {where_to_put_it}",
                self.bind
            ));
        }

        if let Some(tls) = &self.tls {
            for (what, path) in [("certificate", &tls.cert), ("private key", &tls.key)] {
                if !path.exists() {
                    return Err(format!("TLS {what} not found: {}", path.display()));
                }
            }
        }
        Ok(())
    }
}

/// Splits `--config <path>` out of the arguments; the rest are tickers.
fn split_args(args: Vec<String>) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let mut path = None;
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                let value = it.next().ok_or("--config needs a path")?;
                path = Some(PathBuf::from(value));
            }
            other if other.starts_with("--config=") => {
                path = Some(PathBuf::from(&other["--config=".len()..]));
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown flag `{other}`"));
            }
            ticker => rest.push(ticker.to_string()),
        }
    }
    Ok((path, rest))
}

/// Reads the config file, if there is one to read.
///
/// An explicitly named file that is missing is an error — someone asked for it. The default
/// `./config.toml` is optional, because the core has to start without any file at all.
fn load_file(explicit: Option<PathBuf>) -> Result<(FileConfig, Option<PathBuf>), String> {
    let (path, required) = match explicit {
        Some(path) => (path, true),
        None => match std::env::var("MOON_CONFIG") {
            Ok(path) => (PathBuf::from(path), true),
            Err(_) => (PathBuf::from(DEFAULT_CONFIG_PATH), false),
        },
    };

    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let parsed = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok((parsed, Some(path)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Ok((FileConfig::default(), None)),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn parse_env_addr(key: &str) -> Result<Option<SocketAddr>, String> {
    match std::env::var(key) {
        Err(_) => Ok(None),
        Ok(raw) => raw.parse().map(Some).map_err(|e| format!("{key} `{raw}` is not an address: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_TOKEN: &str = "0123456789abcdef";

    /// `std::env` is process-global, so these run under one lock rather than in parallel.
    ///
    /// Every variable the config reads is cleared first. Without that, a test that leaves
    /// `MOON_BIND` set to something invalid makes an unrelated test fail depending on the
    /// order they happen to run in — which is exactly how this flaked.
    fn with_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        for key in ["MOON_TOKEN", "MOON_BIND", "MOON_METRICS_BIND", "MOON_CONFIG"] {
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

    /// A file that deletes itself, so a failing assertion cannot leave litter behind that
    /// makes the *next* run behave differently.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str, contents: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("moon-cfg-{}-{n}-{name}", std::process::id()));
            std::fs::write(&path, contents).expect("temp file is writable");
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn from_file(text: &str) -> Result<Config, String> {
        let file: FileConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        Config::assemble(file, None, vec![])
    }

    #[test]
    fn starting_needs_no_environment_at_all() {
        // The acceptance criterion for task 0.7. A config file is enough on its own.
        with_env(&[], || {
            let cfg = from_file(&format!("token = \"{GOOD_TOKEN}\"")).unwrap();
            assert_eq!(cfg.bind.to_string(), DEFAULT_BIND);
            assert_eq!(cfg.tickers, vec!["BTCUSDT"]);
        });
    }

    #[test]
    fn file_config_beats_env_and_env_beats_default() {
        with_env(
            &[("MOON_TOKEN", Some("env-token-0123456789")), ("MOON_BIND", Some("127.0.0.1:9999"))],
            || {
                // env over default
                let from_env = from_file("").unwrap();
                assert_eq!(from_env.bind.to_string(), "127.0.0.1:9999");
                assert_eq!(from_env.token, "env-token-0123456789");

                // file over env
                let from_file_cfg =
                    from_file(&format!("bind = \"127.0.0.1:7777\"\ntoken = \"{GOOD_TOKEN}\"")).unwrap();
                assert_eq!(from_file_cfg.bind.to_string(), "127.0.0.1:7777");
                assert_eq!(from_file_cfg.token, GOOD_TOKEN);
            },
        );
    }

    #[test]
    fn a_missing_token_refuses_to_start() {
        // Defaulting to "no auth" would be the single worst failure this program could have.
        with_env(&[("MOON_TOKEN", None)], || {
            assert!(from_file("").is_err());
        });
    }

    #[test]
    fn a_short_token_refuses_to_start() {
        with_env(&[("MOON_TOKEN", Some("hunter2"))], || {
            assert!(from_file("").is_err());
        });
    }

    #[test]
    fn tickers_from_the_command_line_beat_the_file_and_are_normalised() {
        with_env(&[("MOON_TOKEN", Some(GOOD_TOKEN))], || {
            let file: FileConfig = toml::from_str("tickers = [\"BTCUSDT\"]").unwrap();
            let cfg = Config::assemble(file, None, vec!["ethusdt".into(), "SolUsdt".into()]).unwrap();
            assert_eq!(cfg.tickers, vec!["ETHUSDT", "SOLUSDT"]);
        });
    }

    #[test]
    fn a_malformed_bind_address_is_a_startup_error() {
        with_env(&[("MOON_TOKEN", Some(GOOD_TOKEN)), ("MOON_BIND", Some("not-an-address"))], || {
            assert!(from_file("").is_err());
        });
    }

    #[test]
    fn a_typo_in_the_file_names_the_offending_key() {
        // `tickerz` would otherwise be a setting that silently never applies.
        with_env(&[("MOON_TOKEN", Some(GOOD_TOKEN))], || {
            let err = from_file("tickerz = [\"BTCUSDT\"]").unwrap_err();
            assert!(err.contains("tickerz"), "error should name the key, got: {err}");
        });
    }

    #[test]
    fn an_explicitly_named_missing_file_is_an_error() {
        with_env(&[("MOON_TOKEN", Some(GOOD_TOKEN))], || {
            let missing = std::env::temp_dir().join("moon-cfg-does-not-exist.toml");
            assert!(load_file(Some(missing)).is_err());
        });
    }

    #[test]
    fn the_default_file_is_optional() {
        with_env(&[("MOON_TOKEN", Some(GOOD_TOKEN))], || {
            // Runs from a directory that has no config.toml; absence must not be fatal.
            let (file, source) = load_file(None).expect("a missing default file is fine");
            assert!(source.is_none() || source.is_some_and(|p| p.exists()));
            let _ = file;
        });
    }

    #[test]
    fn moon_config_env_points_at_a_file() {
        let f = TempFile::new("env.toml", &format!("token = \"{GOOD_TOKEN}\"\nbind = \"127.0.0.1:6001\""));
        let path = f.0.to_string_lossy().to_string();
        with_env(&[("MOON_CONFIG", Some(&path))], || {
            let cfg = Config::load(vec![]).unwrap();
            assert_eq!(cfg.bind.to_string(), "127.0.0.1:6001");
        });
    }

    #[test]
    fn markets_can_be_named_in_the_file() {
        with_env(&[("MOON_TOKEN", Some(GOOD_TOKEN))], || {
            let cfg = from_file("markets = [\"spot\"]").unwrap();
            assert_eq!(cfg.markets, vec![MarketKind::Spot]);
        });
    }

    #[test]
    fn an_empty_market_list_is_refused() {
        with_env(&[("MOON_TOKEN", Some(GOOD_TOKEN))], || {
            assert!(from_file("markets = []").is_err());
        });
    }

    #[test]
    fn the_config_flag_is_accepted_in_both_spellings() {
        let (path, tickers) =
            split_args(vec!["--config".into(), "/tmp/a.toml".into(), "btcusdt".into()]).unwrap();
        assert_eq!(path, Some(PathBuf::from("/tmp/a.toml")));
        assert_eq!(tickers, vec!["btcusdt"]);

        let (path, _) = split_args(vec!["--config=/tmp/b.toml".into()]).unwrap();
        assert_eq!(path, Some(PathBuf::from("/tmp/b.toml")));
    }

    #[test]
    fn an_unknown_flag_is_not_mistaken_for_a_ticker() {
        // Without this, `--tickers BTCUSDT` would subscribe to a market called "--tickers".
        assert!(split_args(vec!["--tickers".into()]).is_err());
    }

    // --- TLS: the token must never cross a network in the clear (task 0.3) ---------------

    #[test]
    fn plain_bind_to_public_iface_is_refused() {
        with_env(&[("MOON_TOKEN", Some(GOOD_TOKEN))], || {
            for addr in ["0.0.0.0:8787", "1.2.3.4:8787", "[::]:8787"] {
                let err = from_file(&format!("bind = \"{addr}\"")).unwrap_err();
                assert!(
                    err.contains("without TLS"),
                    "binding {addr} in plaintext must be refused, got: {err}"
                );
                // The message has to say what to do, not only what went wrong: this fires on
                // a VPS at deploy time, where the operator cannot read the source.
                assert!(err.contains("[tls]"), "the error must name the fix, got: {err}");
            }
        });
    }

    #[test]
    fn loopback_without_tls_is_fine() {
        // Development runs on 127.0.0.1 and must stay frictionless, or the rule gets bypassed.
        with_env(&[("MOON_TOKEN", Some(GOOD_TOKEN))], || {
            for addr in ["127.0.0.1:8787", "[::1]:8787"] {
                assert!(from_file(&format!("bind = \"{addr}\"")).is_ok(), "{addr} should be allowed");
            }
        });
    }

    #[test]
    fn a_public_bind_with_tls_is_allowed() {
        let cert = TempFile::new("cert.pem", "not a real certificate");
        let key = TempFile::new("key.pem", "not a real key");
        with_env(&[("MOON_TOKEN", Some(GOOD_TOKEN))], || {
            let cfg =
                from_file(&format!("bind = \"0.0.0.0:8787\"\n[tls]\ncert = {:?}\nkey = {:?}", cert.0, key.0))
                    .unwrap();
            assert!(cfg.tls.is_some());
        });
    }

    #[test]
    fn tls_paths_are_checked_at_startup_not_at_first_connection() {
        // A missing certificate discovered when the first terminal connects means the core
        // looked healthy for hours and then was not.
        with_env(&[("MOON_TOKEN", Some(GOOD_TOKEN))], || {
            let err = from_file(
                "bind = \"0.0.0.0:8787\"\n[tls]\ncert = \"/nope/cert.pem\"\nkey = \"/nope/key.pem\"",
            )
            .unwrap_err();
            assert!(err.contains("certificate not found"), "got: {err}");
        });
    }
}
