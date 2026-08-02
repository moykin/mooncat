//! What a venue can actually do.
//!
//! # The problem with pretending
//!
//! Venues differ in ways that matter. Binance spot cannot amend an order in place; USD-M can.
//! Some support hedge mode, some do not. Some let a stop be placed at the venue so it survives
//! the core dying, some make you hold it yourself.
//!
//! A connector faced with a call the venue cannot serve has two options, and only one of them
//! is honest. It can emulate — cancel and replace, to look like an amend — or it can say no.
//! Emulation is the tempting answer because it makes the code above simpler, and it is wrong:
//! a cancel-replace loses queue position, can partially fill in the gap between the two calls,
//! and turns one atomic operation into two that can each fail separately. The caller thinks it
//! moved an order and actually cancelled one and maybe placed another.
//!
//! So the rule is that a connector returns [`Error::Unsupported`](crate::Error::Unsupported),
//! and anything that wants the emulated behaviour builds it deliberately, where the trade-off
//! is visible.
//!
//! # Why a bitmask
//!
//! It is queried per order, on the hot path, and it crosses the wire to the terminal so that
//! a button can be greyed out rather than producing an error when pressed. Sixty-four flags in
//! eight bytes, tested with an `and`.

use crate::Error;
use domain::MarketKind;
use serde::{Deserialize, Serialize};

/// What a venue offers on one market.
///
/// Absent means unsupported. That direction is deliberate: a connector that forgets to declare
/// a capability loses it, rather than claiming something it cannot do — the failure lands at
/// the point of the omission instead of at the venue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Caps(pub u64);

impl Caps {
    // --- order types ---------------------------------------------------------------------
    /// A plain limit order.
    pub const LIMIT: Self = Self(1 << 0);
    /// A market order.
    pub const MARKET: Self = Self(1 << 1);
    /// Stop-market held at the venue.
    pub const STOP_MARKET: Self = Self(1 << 2);
    /// Stop-limit held at the venue.
    pub const STOP_LIMIT: Self = Self(1 << 3);
    /// Take-profit held at the venue.
    pub const TAKE_PROFIT: Self = Self(1 << 4);
    /// Trailing stop held at the venue, following the market without our involvement.
    pub const TRAILING_STOP: Self = Self(1 << 5);
    /// Post-only: refuse rather than cross the spread.
    pub const POST_ONLY: Self = Self(1 << 6);
    /// Immediate-or-cancel.
    pub const IOC: Self = Self(1 << 7);
    /// Fill-or-kill.
    pub const FOK: Self = Self(1 << 8);
    /// The order may only reduce an existing position.
    pub const REDUCE_ONLY: Self = Self(1 << 9);

    // --- order lifecycle -----------------------------------------------------------------
    /// Change price or quantity without losing the order. The one that matters most: without
    /// it an "amend" is a cancel and a replace, with everything that implies.
    pub const AMEND_IN_PLACE: Self = Self(1 << 10);
    /// Cancel everything on a symbol in one call rather than one at a time.
    pub const CANCEL_ALL: Self = Self(1 << 11);
    /// The venue accepts our own identifier, so an order can be found after a reconnect
    /// without having stored what the venue called it.
    pub const CLIENT_ORDER_ID: Self = Self(1 << 12);
    /// Several orders in one request.
    pub const BATCH_ORDERS: Self = Self(1 << 13);

    // --- positions and margin -------------------------------------------------------------
    /// Long and short at once on the same instrument.
    pub const HEDGE_MODE: Self = Self(1 << 14);
    /// Per-position margin as well as account-wide.
    pub const ISOLATED_MARGIN: Self = Self(1 << 15);
    /// Leverage can be set from the API rather than only in a web interface.
    pub const SET_LEVERAGE: Self = Self(1 << 16);
    /// The venue reports a liquidation price, so it need not be guessed at.
    pub const LIQUIDATION_PRICE: Self = Self(1 << 17);
    /// Funding is charged and reported.
    pub const FUNDING: Self = Self(1 << 18);

    // --- market data -----------------------------------------------------------------------
    /// Incremental book updates, as opposed to snapshots only.
    pub const BOOK_DELTAS: Self = Self(1 << 19);
    /// A book snapshot can be requested at a chosen depth.
    pub const BOOK_DEPTH_CHOICE: Self = Self(1 << 20);
    /// Liquidations are published.
    pub const LIQUIDATION_FEED: Self = Self(1 << 21);
    /// Historical candles can be fetched, not merely streamed.
    pub const CANDLE_HISTORY: Self = Self(1 << 22);
    /// Historical trades can be fetched.
    pub const TRADE_HISTORY: Self = Self(1 << 23);
    /// A mark price distinct from the last traded price.
    pub const MARK_PRICE: Self = Self(1 << 24);

    // --- account -----------------------------------------------------------------------------
    /// Balances can be moved between the venue's own wallets.
    pub const WALLET_TRANSFER: Self = Self(1 << 25);
    /// A private stream of account updates, rather than polling.
    pub const USER_STREAM: Self = Self(1 << 26);
    /// The venue reports when the API key expires.
    pub const KEY_EXPIRY: Self = Self(1 << 27);
    /// A test environment exists.
    pub const TESTNET: Self = Self(1 << 28);

    pub const NONE: Self = Self(0);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Guard for a connector: return `Unsupported` unless the capability is present.
    ///
    /// The shape is what makes the honest path shorter than the emulating one — a connector
    /// writes one line and is correct, where emulation would take twenty and be subtly wrong.
    pub fn require(self, needed: Self, what: &'static str, market: MarketKind) -> Result<(), Error> {
        if self.contains(needed) {
            Ok(())
        } else {
            Err(Error::Unsupported { what, market })
        }
    }

    /// Names of the flags present, for logs, diagnostics and the terminal's own display.
    pub fn names(self) -> Vec<&'static str> {
        const ALL: &[(Caps, &str)] = &[
            (Caps::LIMIT, "limit"),
            (Caps::MARKET, "market"),
            (Caps::STOP_MARKET, "stop_market"),
            (Caps::STOP_LIMIT, "stop_limit"),
            (Caps::TAKE_PROFIT, "take_profit"),
            (Caps::TRAILING_STOP, "trailing_stop"),
            (Caps::POST_ONLY, "post_only"),
            (Caps::IOC, "ioc"),
            (Caps::FOK, "fok"),
            (Caps::REDUCE_ONLY, "reduce_only"),
            (Caps::AMEND_IN_PLACE, "amend_in_place"),
            (Caps::CANCEL_ALL, "cancel_all"),
            (Caps::CLIENT_ORDER_ID, "client_order_id"),
            (Caps::BATCH_ORDERS, "batch_orders"),
            (Caps::HEDGE_MODE, "hedge_mode"),
            (Caps::ISOLATED_MARGIN, "isolated_margin"),
            (Caps::SET_LEVERAGE, "set_leverage"),
            (Caps::LIQUIDATION_PRICE, "liquidation_price"),
            (Caps::FUNDING, "funding"),
            (Caps::BOOK_DELTAS, "book_deltas"),
            (Caps::BOOK_DEPTH_CHOICE, "book_depth_choice"),
            (Caps::LIQUIDATION_FEED, "liquidation_feed"),
            (Caps::CANDLE_HISTORY, "candle_history"),
            (Caps::TRADE_HISTORY, "trade_history"),
            (Caps::MARK_PRICE, "mark_price"),
            (Caps::WALLET_TRANSFER, "wallet_transfer"),
            (Caps::USER_STREAM, "user_stream"),
            (Caps::KEY_EXPIRY, "key_expiry"),
            (Caps::TESTNET, "testnet"),
        ];
        ALL.iter().filter(|(flag, _)| self.contains(*flag)).map(|(_, name)| *name).collect()
    }

    /// Every flag this build knows about, for the coverage test and for diagnostics.
    pub fn all_known() -> Self {
        Self(Self::NONE.0)
            .union(Self::LIMIT)
            .union(Self::MARKET)
            .union(Self::STOP_MARKET)
            .union(Self::STOP_LIMIT)
            .union(Self::TAKE_PROFIT)
            .union(Self::TRAILING_STOP)
            .union(Self::POST_ONLY)
            .union(Self::IOC)
            .union(Self::FOK)
            .union(Self::REDUCE_ONLY)
            .union(Self::AMEND_IN_PLACE)
            .union(Self::CANCEL_ALL)
            .union(Self::CLIENT_ORDER_ID)
            .union(Self::BATCH_ORDERS)
            .union(Self::HEDGE_MODE)
            .union(Self::ISOLATED_MARGIN)
            .union(Self::SET_LEVERAGE)
            .union(Self::LIQUIDATION_PRICE)
            .union(Self::FUNDING)
            .union(Self::BOOK_DELTAS)
            .union(Self::BOOK_DEPTH_CHOICE)
            .union(Self::LIQUIDATION_FEED)
            .union(Self::CANDLE_HISTORY)
            .union(Self::TRADE_HISTORY)
            .union(Self::MARK_PRICE)
            .union(Self::WALLET_TRANSFER)
            .union(Self::USER_STREAM)
            .union(Self::KEY_EXPIRY)
            .union(Self::TESTNET)
    }
}

impl std::ops::BitOr for Caps {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl std::fmt::Display for Caps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        f.write_str(&self.names().join(" | "))
    }
}

/// Binance spot, from its documented behaviour.
///
/// The absence of [`Caps::AMEND_IN_PLACE`] is the point of the example: spot has no amend
/// endpoint, so an amend must be refused rather than turned into a cancel and a replace.
pub const BINANCE_SPOT: Caps = Caps(
    Caps::LIMIT.0
        | Caps::MARKET.0
        | Caps::STOP_LIMIT.0
        | Caps::POST_ONLY.0
        | Caps::IOC.0
        | Caps::FOK.0
        | Caps::CANCEL_ALL.0
        | Caps::CLIENT_ORDER_ID.0
        | Caps::BOOK_DELTAS.0
        | Caps::BOOK_DEPTH_CHOICE.0
        | Caps::CANDLE_HISTORY.0
        | Caps::TRADE_HISTORY.0
        | Caps::WALLET_TRANSFER.0
        | Caps::USER_STREAM.0
        | Caps::TESTNET.0,
);

/// Binance USD-margined perpetuals.
pub const BINANCE_USDM: Caps = Caps(
    BINANCE_SPOT.0
        | Caps::AMEND_IN_PLACE.0
        | Caps::STOP_MARKET.0
        | Caps::TAKE_PROFIT.0
        | Caps::TRAILING_STOP.0
        | Caps::REDUCE_ONLY.0
        | Caps::BATCH_ORDERS.0
        | Caps::HEDGE_MODE.0
        | Caps::ISOLATED_MARGIN.0
        | Caps::SET_LEVERAGE.0
        | Caps::LIQUIDATION_PRICE.0
        | Caps::FUNDING.0
        | Caps::LIQUIDATION_FEED.0
        | Caps::MARK_PRICE.0,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_has_at_least_twenty_flags_and_no_collisions() {
        // Collisions are the failure mode of a hand-numbered bitmask, and they are silent: two
        // capabilities sharing a bit means declaring one grants the other.
        let all = Caps::all_known();
        let count = all.names().len();
        assert!(count >= 20, "only {count} capabilities declared");
        assert_eq!(count, all.0.count_ones() as usize, "two flags share a bit");
    }

    #[test]
    fn absent_means_unsupported_rather_than_unknown() {
        // The direction that makes an omission safe: a connector that forgets to declare a
        // capability loses it, instead of claiming something it cannot do.
        let nothing = Caps::default();
        assert!(nothing.is_empty());
        assert!(!nothing.contains(Caps::LIMIT));
        assert_eq!(nothing.to_string(), "none");
    }

    // --- the acceptance criterion ------------------------------------------------------------

    #[test]
    fn amending_on_a_venue_without_it_is_unsupported_not_emulated() {
        // The acceptance criterion for task 3.4. A cancel-replace loses queue position, can
        // partially fill in the gap between the two calls, and turns one atomic operation into
        // two that can each fail separately.
        let refusal =
            BINANCE_SPOT.require(Caps::AMEND_IN_PLACE, "amend_order", MarketKind::Spot).unwrap_err();

        assert!(matches!(refusal, Error::Unsupported { what: "amend_order", market: MarketKind::Spot }));
        assert!(!refusal.is_retryable(), "retrying will not make the venue grow the endpoint");

        // And where it exists, the same call is allowed.
        assert!(BINANCE_USDM.require(Caps::AMEND_IN_PLACE, "amend_order", MarketKind::LinearPerp).is_ok());
    }

    #[test]
    fn the_refusal_names_the_call_and_the_market() {
        // It surfaces to an operator who pressed a button, so it has to say what was refused
        // and where rather than "unsupported".
        let message = BINANCE_SPOT
            .require(Caps::HEDGE_MODE, "set_position_mode", MarketKind::Spot)
            .unwrap_err()
            .to_string();
        assert!(message.contains("set_position_mode"), "got: {message}");
        assert!(message.contains("spot"), "got: {message}");
    }

    // --- the declared profiles ------------------------------------------------------------------

    #[test]
    fn spot_and_futures_differ_where_they_actually_differ() {
        // These are the differences that cost a debugging session if got wrong.
        assert!(!BINANCE_SPOT.contains(Caps::AMEND_IN_PLACE), "spot has no amend endpoint");
        assert!(BINANCE_USDM.contains(Caps::AMEND_IN_PLACE));

        assert!(!BINANCE_SPOT.contains(Caps::HEDGE_MODE), "spot has no positions at all");
        assert!(BINANCE_USDM.contains(Caps::HEDGE_MODE));

        assert!(!BINANCE_SPOT.contains(Caps::FUNDING));
        assert!(BINANCE_USDM.contains(Caps::FUNDING));

        assert!(!BINANCE_SPOT.contains(Caps::LIQUIDATION_PRICE), "nothing to liquidate on spot");
        assert!(BINANCE_USDM.contains(Caps::LIQUIDATION_PRICE));
    }

    #[test]
    fn futures_offer_everything_spot_does() {
        // Not a law of nature, but true here, and a violation would more likely be a typo in
        // the mask than a real venue difference.
        assert!(
            BINANCE_USDM.contains(BINANCE_SPOT),
            "USD-M is missing {:?}",
            BINANCE_SPOT.without(BINANCE_USDM).names()
        );
    }

    #[test]
    fn both_profiles_declare_the_basics() {
        for (name, caps) in [("spot", BINANCE_SPOT), ("usdm", BINANCE_USDM)] {
            for required in [Caps::LIMIT, Caps::MARKET, Caps::CLIENT_ORDER_ID, Caps::USER_STREAM] {
                assert!(caps.contains(required), "{name} is missing {}", required.names()[0]);
            }
        }
    }

    // --- the operations ------------------------------------------------------------------------------

    #[test]
    fn contains_requires_all_the_bits_asked_for() {
        // A multi-flag query must be an "and", not an "or" — asking whether a venue supports
        // both a trailing stop and hedge mode and getting yes for one of them would be worse
        // than not asking.
        let both = Caps::LIMIT | Caps::MARKET;
        assert!(both.contains(Caps::LIMIT));
        assert!(both.contains(both));
        assert!(!Caps::LIMIT.contains(both));
    }

    #[test]
    fn a_capability_set_survives_the_wire_as_one_number() {
        // It goes to the terminal so a button can be greyed out rather than producing an error
        // when pressed, and it must not cost a map to do so.
        let encoded = rmp_serde::to_vec_named(&BINANCE_USDM).unwrap();
        assert!(encoded.len() <= 9, "encoded as {} bytes", encoded.len());
        assert_eq!(rmp_serde::from_slice::<Caps>(&encoded).unwrap(), BINANCE_USDM);
    }

    #[test]
    fn the_display_form_is_readable_and_complete() {
        let text = BINANCE_SPOT.to_string();
        assert!(text.contains("limit"));
        assert!(!text.contains("amend_in_place"), "must not claim what is absent");
        assert_eq!(text.split(" | ").count(), BINANCE_SPOT.names().len());
    }

    #[test]
    fn an_unknown_bit_does_not_break_the_display() {
        // A newer core may declare a capability this build has never heard of; it must be
        // ignored rather than crashing a terminal that is merely rendering a list.
        let future = BINANCE_SPOT.union(Caps(1 << 63));
        assert_eq!(future.names(), BINANCE_SPOT.names(), "the unknown bit is skipped");
        assert!(future.contains(Caps::LIMIT), "and the known ones still resolve");
    }
}
