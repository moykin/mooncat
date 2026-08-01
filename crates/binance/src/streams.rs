//! Translation between [`Subscription`] and Binance stream names.

use exchange::{Error, Result, Subscription};

/// Book updates at 100 ms. The 250 ms default is too coarse for a scalping book, and the
/// raw stream is only available on futures.
const DEPTH_SPEED: &str = "100ms";

pub fn stream_name(sub: &Subscription) -> Result<String> {
    Ok(match sub {
        Subscription::Trades(s) => format!("{}@aggTrade", s.raw.to_lowercase()),
        Subscription::Book(s) => format!("{}@depth@{DEPTH_SPEED}", s.raw.to_lowercase()),
        Subscription::Candles { symbol, interval_ms } => {
            format!("{}@kline_{}", symbol.raw.to_lowercase(), interval_name(*interval_ms)?)
        }
    })
}

/// Binance names intervals rather than taking a duration, and only publishes a fixed set.
fn interval_name(ms: i64) -> Result<&'static str> {
    Ok(match ms {
        60_000 => "1m",
        180_000 => "3m",
        300_000 => "5m",
        900_000 => "15m",
        1_800_000 => "30m",
        3_600_000 => "1h",
        7_200_000 => "2h",
        14_400_000 => "4h",
        21_600_000 => "6h",
        28_800_000 => "8h",
        43_200_000 => "12h",
        86_400_000 => "1d",
        _ => return Err(Error::Malformed(format!("binance publishes no candle interval of {ms} ms"))),
    })
}

/// A `SUBSCRIBE`/`UNSUBSCRIBE` control frame.
pub fn control_frame(method: &str, streams: &[String], id: u64) -> String {
    let params = streams.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(",");
    format!(r#"{{"method":"{method}","params":[{params}],"id":{id}}}"#)
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{ExchangeId, MarketKind, Symbol};

    fn perp() -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT")
    }

    #[test]
    fn stream_names_are_lowercased_as_binance_requires() {
        assert_eq!(stream_name(&Subscription::Trades(perp())).unwrap(), "btcusdt@aggTrade");
        assert_eq!(stream_name(&Subscription::Book(perp())).unwrap(), "btcusdt@depth@100ms");
    }

    #[test]
    fn candle_intervals_map_to_binance_names() {
        let sub = Subscription::Candles { symbol: perp(), interval_ms: 60_000 };
        assert_eq!(stream_name(&sub).unwrap(), "btcusdt@kline_1m");
    }

    #[test]
    fn an_interval_binance_does_not_publish_is_refused_up_front() {
        // Better a clear error here than a subscription the venue silently ignores.
        let sub = Subscription::Candles { symbol: perp(), interval_ms: 45_000 };
        assert!(stream_name(&sub).is_err());
    }

    #[test]
    fn control_frames_are_valid_json() {
        let frame = control_frame("SUBSCRIBE", &["btcusdt@aggTrade".into(), "btcusdt@depth@100ms".into()], 7);
        let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();

        assert_eq!(parsed["method"], "SUBSCRIBE");
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["params"][1], "btcusdt@depth@100ms");
    }

    #[test]
    fn an_empty_subscription_list_still_produces_valid_json() {
        let parsed: serde_json::Value = serde_json::from_str(&control_frame("SUBSCRIBE", &[], 1)).unwrap();
        assert_eq!(parsed["params"].as_array().unwrap().len(), 0);
    }
}
