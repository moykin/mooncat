//! Binance JSON payloads and their translation into [`domain`] types.
//!
//! Kept free of I/O so the tricky parts — the aggressor-side inversion and the two
//! different book-sequencing schemes — are testable against captured payloads.

use domain::{BookDelta, BookLevel, Instrument, MarketKind, OrderBook, PublicTrade, Side, Symbol, Timestamp};
use exchange::{Error, Result};
use rust_decimal::Decimal;
use serde::Deserialize;

/// `["64000.10", "0.235"]` — Binance sends numbers as strings throughout.
#[derive(Debug, Deserialize)]
pub struct RawLevel(
    #[serde(with = "rust_decimal::serde::str")] pub Decimal,
    #[serde(with = "rust_decimal::serde::str")] pub Decimal,
);

impl RawLevel {
    fn into_level(self) -> BookLevel {
        BookLevel { price: self.0, qty: self.1 }
    }
}

fn levels(raw: Vec<RawLevel>) -> Vec<BookLevel> {
    raw.into_iter().map(RawLevel::into_level).collect()
}

// ---------------------------------------------------------------- trade stream

#[derive(Debug, Deserialize)]
pub struct AggTrade {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "a")]
    pub agg_id: u64,
    #[serde(rename = "p", with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(rename = "q", with = "rust_decimal::serde::str")]
    pub qty: Decimal,
    /// Trade time.
    #[serde(rename = "T")]
    pub trade_time: i64,
    /// True when the *buyer* was the maker.
    #[serde(rename = "m")]
    pub buyer_is_maker: bool,
}

impl AggTrade {
    pub fn into_domain(self, symbol: Symbol) -> PublicTrade {
        PublicTrade {
            symbol,
            price: self.price,
            qty: self.qty,
            // Binance reports who *made* the market, we store who *took* it. If the buyer
            // was the maker then the seller crossed the spread. Getting this backwards
            // silently inverts every delta/CVD indicator built on the tape.
            taker_side: if self.buyer_is_maker { Side::Sell } else { Side::Buy },
            ts: Timestamp::from_millis(self.trade_time),
            id: self.agg_id,
        }
    }
}

// ----------------------------------------------------------------- book stream

#[derive(Debug, Deserialize)]
pub struct DepthUpdate {
    #[serde(rename = "s")]
    pub symbol: String,
    /// First update id in this event.
    #[serde(rename = "U")]
    pub first_update_id: u64,
    /// Final update id in this event.
    #[serde(rename = "u")]
    pub final_update_id: u64,
    /// Final update id of the *previous* event. USD-M futures only.
    #[serde(rename = "pu")]
    pub prev_final_update_id: Option<u64>,
    #[serde(rename = "b")]
    pub bids: Vec<RawLevel>,
    #[serde(rename = "a")]
    pub asks: Vec<RawLevel>,
    /// Event time.
    #[serde(rename = "E")]
    pub event_time: i64,
}

impl DepthUpdate {
    pub fn into_domain(self, symbol: Symbol) -> BookDelta {
        // The two markets sequence their books differently, and the difference is exactly
        // what gap detection hangs on:
        //   * USD-M futures state the previous event's final id outright in `pu`;
        //   * spot does not, but guarantees `U == previous u + 1`, so `U - 1` is that id.
        // Treating spot's `U` as if it were `pu` would flag a gap on every single event.
        let prev = self.prev_final_update_id.unwrap_or_else(|| self.first_update_id.saturating_sub(1));

        BookDelta {
            symbol,
            prev_update_id: prev,
            last_update_id: self.final_update_id,
            bids: levels(self.bids),
            asks: levels(self.asks),
            ts: Timestamp::from_millis(self.event_time),
        }
    }
}

// ------------------------------------------------------------- REST: book snapshot

#[derive(Debug, Deserialize)]
pub struct DepthSnapshot {
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: u64,
    pub bids: Vec<RawLevel>,
    pub asks: Vec<RawLevel>,
}

impl DepthSnapshot {
    pub fn into_domain(self, symbol: Symbol, ts: Timestamp) -> OrderBook {
        OrderBook {
            symbol: Some(symbol),
            bids: levels(self.bids),
            asks: levels(self.asks),
            last_update_id: self.last_update_id,
            ts,
            // Not yet joined to the delta stream; see `OrderBook::apply`.
            synced: false,
        }
    }
}

// -------------------------------------------------------------- REST: exchangeInfo

#[derive(Debug, Deserialize)]
pub struct ExchangeInfo {
    pub symbols: Vec<SymbolInfo>,
}

#[derive(Debug, Deserialize)]
pub struct SymbolInfo {
    pub symbol: String,
    pub status: Option<String>,
    #[serde(rename = "contractStatus")]
    pub contract_status: Option<String>,
    #[serde(rename = "baseAsset")]
    pub base_asset: String,
    #[serde(rename = "quoteAsset")]
    pub quote_asset: String,
    #[serde(rename = "marginAsset")]
    pub margin_asset: Option<String>,
    pub filters: Vec<Filter>,
}

/// Only the filters that constrain an order. Everything else is skipped rather than
/// modelled, so a new Binance filter type cannot break parsing.
#[derive(Debug, Deserialize)]
#[serde(tag = "filterType")]
pub enum Filter {
    #[serde(rename = "PRICE_FILTER")]
    Price {
        #[serde(rename = "tickSize", with = "rust_decimal::serde::str")]
        tick_size: Decimal,
    },
    #[serde(rename = "LOT_SIZE")]
    LotSize {
        #[serde(rename = "stepSize", with = "rust_decimal::serde::str")]
        step_size: Decimal,
        #[serde(rename = "minQty", with = "rust_decimal::serde::str")]
        min_qty: Decimal,
    },
    /// Spot spells it `minNotional`; USD-M futures spells the same idea `notional`.
    #[serde(rename = "MIN_NOTIONAL")]
    MinNotional {
        #[serde(rename = "minNotional", default, deserialize_with = "opt_decimal_str::deserialize")]
        min_notional: Option<Decimal>,
        #[serde(default, deserialize_with = "opt_decimal_str::deserialize")]
        notional: Option<Decimal>,
    },
    #[serde(rename = "NOTIONAL")]
    Notional {
        #[serde(rename = "minNotional", default, deserialize_with = "opt_decimal_str::deserialize")]
        min_notional: Option<Decimal>,
    },
    #[serde(other)]
    Other,
}

mod opt_decimal_str {
    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer};
    use std::str::FromStr;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Decimal>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        Ok(raw.and_then(|s| Decimal::from_str(&s).ok()))
    }
}

impl SymbolInfo {
    pub fn into_domain(self, exchange_market: MarketKind) -> Result<Instrument> {
        let mut tick_size = None;
        let mut step_size = None;
        let mut min_qty = None;
        let mut min_notional = Decimal::ZERO;

        for filter in &self.filters {
            match filter {
                Filter::Price { tick_size: t } => tick_size = Some(*t),
                Filter::LotSize { step_size: s, min_qty: q } => {
                    step_size = Some(*s);
                    min_qty = Some(*q);
                }
                Filter::MinNotional { min_notional: a, notional: b } => {
                    min_notional = a.or(*b).unwrap_or(Decimal::ZERO);
                }
                Filter::Notional { min_notional: a } => {
                    min_notional = a.unwrap_or(Decimal::ZERO);
                }
                Filter::Other => {}
            }
        }

        let missing = |what: &str| Error::Malformed(format!("{} has no {what} filter", self.symbol));
        let quote = self.quote_asset;

        Ok(Instrument {
            symbol: Symbol::new(domain::ExchangeId::Binance, exchange_market, &self.symbol),
            base: self.base_asset,
            margin_asset: self.margin_asset.unwrap_or_else(|| quote.clone()),
            quote,
            tick_size: tick_size.ok_or_else(|| missing("PRICE_FILTER"))?,
            step_size: step_size.ok_or_else(|| missing("LOT_SIZE"))?,
            min_qty: min_qty.ok_or_else(|| missing("LOT_SIZE"))?,
            min_notional,
            // Spot reports `status`, futures `contractStatus`; both use TRADING.
            trading: self.status.as_deref().or(self.contract_status.as_deref()) == Some("TRADING"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::ExchangeId;
    use rust_decimal_macros::dec;

    fn perp() -> Symbol {
        Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT")
    }

    #[test]
    fn taker_side_is_the_inverse_of_binance_maker_flag() {
        let raw = r#"{"e":"aggTrade","E":1,"s":"BTCUSDT","a":99,"p":"64000.10","q":"0.235","f":1,"l":2,"T":1700000000000,"m":true}"#;
        let trade: AggTrade = serde_json::from_str(raw).unwrap();
        let out = trade.into_domain(perp());

        // buyer_is_maker = true means the SELLER crossed the spread.
        assert_eq!(out.taker_side, Side::Sell);
        assert_eq!(out.price, dec!(64000.10));
        assert_eq!(out.qty, dec!(0.235));
        assert_eq!(out.id, 99);

        let raw = raw.replace("\"m\":true", "\"m\":false");
        let trade: AggTrade = serde_json::from_str(&raw).unwrap();
        assert_eq!(trade.into_domain(perp()).taker_side, Side::Buy);
    }

    #[test]
    fn futures_book_sequencing_uses_pu_verbatim() {
        let raw = r#"{"e":"depthUpdate","E":1700000000000,"T":1,"s":"BTCUSDT","U":157,"u":160,"pu":149,"b":[["64000.1","10"]],"a":[["64001.0","5"]]}"#;
        let delta: DepthUpdate = serde_json::from_str(raw).unwrap();
        let out = delta.into_domain(perp());

        assert_eq!(out.prev_update_id, 149);
        assert_eq!(out.last_update_id, 160);
        assert_eq!(out.bids[0].price, dec!(64000.1));
    }

    #[test]
    fn spot_book_sequencing_derives_the_previous_id_from_u() {
        // Spot omits `pu` entirely but guarantees U == previous u + 1.
        let raw = r#"{"e":"depthUpdate","E":1700000000000,"s":"BTCUSDT","U":157,"u":160,"b":[],"a":[]}"#;
        let delta: DepthUpdate = serde_json::from_str(raw).unwrap();
        let out = delta.into_domain(Symbol::new(ExchangeId::Binance, MarketKind::Spot, "BTCUSDT"));

        assert_eq!(out.prev_update_id, 156, "spot's previous final id is U - 1");
    }

    #[test]
    fn spot_and_futures_deltas_chain_without_false_gaps() {
        // The regression this guards: reading spot's `U` as `pu` flags a gap every event.
        let mut book = OrderBook { last_update_id: 156, ..Default::default() };
        let raw = r#"{"e":"depthUpdate","E":1,"s":"BTCUSDT","U":157,"u":160,"b":[["100","1"]],"a":[]}"#;
        let delta: DepthUpdate = serde_json::from_str(raw).unwrap();

        let outcome =
            book.apply(&delta.into_domain(Symbol::new(ExchangeId::Binance, MarketKind::Spot, "BTCUSDT")));
        assert_eq!(outcome, domain::ApplyOutcome::Applied);
    }

    #[test]
    fn spot_instrument_carries_both_grids_and_the_notional() {
        let raw = r#"{
            "symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT",
            "filters":[
                {"filterType":"PRICE_FILTER","tickSize":"0.01000000","minPrice":"0.01","maxPrice":"1000"},
                {"filterType":"LOT_SIZE","stepSize":"0.00001000","minQty":"0.00001000","maxQty":"9000"},
                {"filterType":"NOTIONAL","minNotional":"5.00000000"},
                {"filterType":"ICEBERG_PARTS","limit":10}
            ]}"#;
        let info: SymbolInfo = serde_json::from_str(raw).unwrap();
        let out = info.into_domain(MarketKind::Spot).unwrap();

        assert_eq!(out.tick_size, dec!(0.01));
        assert_eq!(out.step_size, dec!(0.00001));
        assert_eq!(out.min_notional, dec!(5));
        assert_eq!(out.margin_asset, "USDT", "spot margins in the quote asset");
        assert!(out.trading);
    }

    #[test]
    fn futures_instrument_reads_contract_status_and_the_notional_alias() {
        // USD-M spells the minimum "notional" and the state "contractStatus".
        let raw = r#"{
            "symbol":"BTCUSDT","contractStatus":"TRADING","baseAsset":"BTC","quoteAsset":"USDT","marginAsset":"USDT",
            "filters":[
                {"filterType":"PRICE_FILTER","tickSize":"0.10"},
                {"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001"},
                {"filterType":"MIN_NOTIONAL","notional":"5"}
            ]}"#;
        let info: SymbolInfo = serde_json::from_str(raw).unwrap();
        let out = info.into_domain(MarketKind::LinearPerp).unwrap();

        assert_eq!(out.min_notional, dec!(5));
        assert_eq!(out.tick_size, dec!(0.10));
        assert!(out.trading);
    }

    #[test]
    fn a_halted_instrument_is_marked_not_trading_rather_than_dropped() {
        let raw = r#"{"symbol":"XYZUSDT","status":"BREAK","baseAsset":"XYZ","quoteAsset":"USDT",
            "filters":[{"filterType":"PRICE_FILTER","tickSize":"0.01"},
                       {"filterType":"LOT_SIZE","stepSize":"1","minQty":"1"}]}"#;
        let info: SymbolInfo = serde_json::from_str(raw).unwrap();
        assert!(!info.into_domain(MarketKind::Spot).unwrap().trading);
    }

    #[test]
    fn a_symbol_without_grids_is_a_parse_error_not_a_silent_default() {
        let raw = r#"{"symbol":"BROKEN","status":"TRADING","baseAsset":"A","quoteAsset":"B","filters":[]}"#;
        let info: SymbolInfo = serde_json::from_str(raw).unwrap();
        assert!(info.into_domain(MarketKind::Spot).is_err());
    }

    #[test]
    fn depth_snapshot_keeps_the_sequence_id_for_resync() {
        let raw = r#"{"lastUpdateId":1027024,"bids":[["64000.1","1.5"]],"asks":[["64001.2","2.0"]]}"#;
        let snap: DepthSnapshot = serde_json::from_str(raw).unwrap();
        let book = snap.into_domain(perp(), Timestamp::from_millis(7));

        assert_eq!(book.last_update_id, 1027024);
        assert_eq!(book.best_bid().unwrap().price, dec!(64000.1));
        assert_eq!(book.spread(), Some(dec!(1.1)));
    }
}
