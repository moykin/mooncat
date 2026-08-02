//! Prices and quantities as integers, for the hot path only.
//!
//! # What this is not
//!
//! It is not a replacement for [`Decimal`]. Money stays decimal everywhere it is reasoned
//! about — in the order manager, in the risk engine, in reports — because that is where
//! getting it wrong costs money. This is a transport representation, used between the point a
//! book delta is built and the point the terminal turns it back into a decimal to display.
//!
//! # Why it is worth having at all
//!
//! A decimal on the wire is a string: `"63096.01"` is ten bytes and has to be parsed. A book
//! delta carries a hundred of them, and there are thousands of deltas a second. The same value
//! as `6309601` is four bytes and is already a number.
//!
//! # Why off-grid values are refused rather than rounded
//!
//! Every instrument has a tick size, and every legal price is a whole number of ticks. A value
//! that is not is either a bug in our own arithmetic or a venue sending something unexpected,
//! and both deserve to be noticed. Rounding it quietly would turn "the price is wrong" into
//! "the price is slightly different from what you asked for", which is the harder of the two
//! to debug and the one that reaches an exchange.
//!
//! Rounding on purpose is what [`Instrument::round_price`] is for, and it is a separate,
//! deliberate call.

use crate::instrument::Instrument;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A value expressed as a whole number of an instrument's ticks or steps.
///
/// Meaningless without the instrument it was scaled against, which is why it is a distinct
/// type: a bare `i64` would eventually be compared against one from a different instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scaled(pub i64);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScaleError {
    #[error("{value} is not a whole number of {increment} ticks")]
    OffGrid { value: Decimal, increment: Decimal },
    #[error("{value} scaled by {increment} does not fit in an i64")]
    OutOfRange { value: Decimal, increment: Decimal },
    #[error("increment is zero, so nothing can be scaled by it")]
    ZeroIncrement,
}

/// Convert a decimal into whole increments.
fn scale(value: Decimal, increment: Decimal) -> Result<Scaled, ScaleError> {
    if increment.is_zero() {
        return Err(ScaleError::ZeroIncrement);
    }
    let quotient = value / increment;
    // `Decimal` division is exact within its precision, so a value on the grid divides to a
    // whole number. Anything else is off-grid and must be seen rather than smoothed away.
    if quotient.fract() != Decimal::ZERO {
        return Err(ScaleError::OffGrid { value, increment });
    }
    quotient.to_i64().map(Scaled).ok_or(ScaleError::OutOfRange { value, increment })
}

fn unscale(scaled: Scaled, increment: Decimal) -> Decimal {
    Decimal::from(scaled.0) * increment
}

impl Instrument {
    /// A price as a whole number of ticks.
    pub fn scale_price(&self, price: Decimal) -> Result<Scaled, ScaleError> {
        scale(price, self.tick_size)
    }

    /// Back to a decimal. Exact by construction: it is a multiplication by the same increment.
    pub fn unscale_price(&self, scaled: Scaled) -> Decimal {
        unscale(scaled, self.tick_size)
    }

    /// A quantity as a whole number of steps.
    pub fn scale_qty(&self, qty: Decimal) -> Result<Scaled, ScaleError> {
        scale(qty, self.step_size)
    }

    pub fn unscale_qty(&self, scaled: Scaled) -> Decimal {
        unscale(scaled, self.step_size)
    }
}

/// One book level on the wire: two integers, no field names.
///
/// A tuple rather than a struct with named fields, because a map key costs more than the value
/// it labels when the value is four bytes and there are two hundred of them in a frame. The
/// order is (price, quantity) and it is fixed by this type rather than by convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScaledLevel(pub (i64, i64));

impl ScaledLevel {
    pub fn new(price: Scaled, qty: Scaled) -> Self {
        Self((price.0, qty.0))
    }

    pub fn price(self) -> Scaled {
        Scaled(self.0 .0)
    }

    pub fn qty(self) -> Scaled {
        Scaled(self.0 .1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ExchangeId, MarketKind, Symbol};
    use rust_decimal_macros::dec;

    /// BTCUSDT on Binance USD-M: a cent tick and a milli-BTC step.
    fn btc() -> Instrument {
        Instrument {
            symbol: Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT"),
            base: "BTC".into(),
            quote: "USDT".into(),
            margin_asset: "USDT".into(),
            tick_size: dec!(0.01),
            step_size: dec!(0.001),
            min_qty: dec!(0.001),
            min_notional: dec!(5),
            trading: true,
        }
    }

    /// A cheap instrument, where the tick is small and the numbers are large.
    fn shib() -> Instrument {
        Instrument {
            symbol: Symbol::new(ExchangeId::Binance, MarketKind::Spot, "SHIBUSDT"),
            base: "SHIB".into(),
            quote: "USDT".into(),
            margin_asset: "USDT".into(),
            tick_size: dec!(0.00000001),
            step_size: dec!(1),
            min_qty: dec!(1),
            min_notional: dec!(5),
            trading: true,
        }
    }

    // --- the acceptance criterion (test 14, doc 11 §10.5) ----------------------------------

    #[test]
    fn every_price_on_the_tick_grid_survives_the_round_trip_exactly() {
        // The acceptance criterion. Checked across the grid rather than on one value, because
        // an off-by-one in the scaling shows up at some magnitudes and not others.
        for instrument in [btc(), shib()] {
            let tick = instrument.tick_size;
            let mut price = tick;
            for _ in 0..10_000 {
                let scaled = instrument.scale_price(price).expect("on the grid");
                assert_eq!(
                    instrument.unscale_price(scaled),
                    price,
                    "{price} did not survive on {}",
                    instrument.symbol
                );
                price += tick;
            }
        }
    }

    #[test]
    fn realistic_prices_and_quantities_round_trip() {
        let btc = btc();
        for price in [dec!(63096.01), dec!(0.01), dec!(120000.00), dec!(99999.99)] {
            let scaled = btc.scale_price(price).unwrap();
            assert_eq!(btc.unscale_price(scaled), price, "{price}");
        }
        for qty in [dec!(0.001), dec!(1.234), dec!(1000)] {
            let scaled = btc.scale_qty(qty).unwrap();
            assert_eq!(btc.unscale_qty(scaled), qty, "{qty}");
        }
    }

    #[test]
    fn a_satoshi_scale_tick_does_not_lose_precision() {
        // The case a float representation gets wrong, and the reason money is decimal in the
        // first place. Eight decimal places is where `f64` starts producing 0.000000009999.
        let shib = shib();
        let price = dec!(0.00002417);
        let scaled = shib.scale_price(price).unwrap();
        assert_eq!(scaled, Scaled(2_417));
        assert_eq!(shib.unscale_price(scaled), price);
    }

    // --- off-grid values are refused ------------------------------------------------------------

    #[test]
    fn a_price_off_the_tick_grid_is_refused_not_rounded() {
        // Rounding quietly turns "the price is wrong" into "the price is slightly different
        // from what you asked", which is harder to debug and reaches an exchange.
        let btc = btc();
        let err = btc.scale_price(dec!(63096.015)).unwrap_err();
        assert!(matches!(err, ScaleError::OffGrid { .. }), "got {err:?}");
        assert!(err.to_string().contains("63096.015"), "the message must name the value");
    }

    #[test]
    fn rounding_remains_available_but_has_to_be_asked_for() {
        // The deliberate path. Scaling refuses; `round_price` rounds; the two are different
        // calls so that a rounding cannot happen by accident on a transport boundary.
        let btc = btc();
        let rounded = btc.round_price(dec!(63096.015));
        assert!(btc.scale_price(rounded).is_ok(), "what was rounded is on the grid");
    }

    #[test]
    fn a_quantity_off_the_step_grid_is_refused() {
        let btc = btc();
        assert!(matches!(btc.scale_qty(dec!(0.0015)), Err(ScaleError::OffGrid { .. })));
        assert!(btc.scale_qty(dec!(0.002)).is_ok());
    }

    #[test]
    fn a_zero_increment_is_an_error_rather_than_a_division_by_zero() {
        // A malformed instrument from a venue must not panic a connector.
        let mut broken = btc();
        broken.tick_size = Decimal::ZERO;
        assert_eq!(broken.scale_price(dec!(1)), Err(ScaleError::ZeroIncrement));
    }

    #[test]
    fn a_value_too_large_for_an_i64_is_refused_rather_than_wrapping() {
        // A wrap would produce a plausible-looking price of the wrong sign.
        let shib = shib();
        let absurd = Decimal::from(i64::MAX) * dec!(0.00000001) * dec!(1000);
        assert!(matches!(shib.scale_price(absurd), Err(ScaleError::OutOfRange { .. })));
    }

    #[test]
    fn zero_is_a_legal_scaled_value() {
        // A book level with zero quantity is how a venue says "this level is gone", so it must
        // not be mistaken for an error.
        let btc = btc();
        assert_eq!(btc.scale_qty(Decimal::ZERO), Ok(Scaled(0)));
        assert_eq!(btc.unscale_qty(Scaled(0)), Decimal::ZERO);
    }

    #[test]
    fn a_negative_value_scales_symmetrically() {
        // Not a price, but a delta or a profit figure can be negative, and truncation towards
        // zero would make the two directions behave differently.
        let btc = btc();
        assert_eq!(btc.scale_price(dec!(-63096.01)), Ok(Scaled(-6_309_601)));
        assert_eq!(btc.unscale_price(Scaled(-6_309_601)), dec!(-63096.01));
    }

    // --- the size measurement ---------------------------------------------------------------------

    #[test]
    fn a_scaled_book_frame_fits_the_budget() {
        // The other half of the acceptance criterion: doc 11 §10.5 asks for a tape frame under
        // a kibibyte. Measured against the decimal form it replaces, because the ratio is what
        // justifies carrying a second representation at all.
        let btc = btc();
        let levels: Vec<ScaledLevel> = (0..100)
            .map(|i| {
                let price = dec!(63096.01) + Decimal::from(i) * btc.tick_size;
                ScaledLevel::new(btc.scale_price(price).unwrap(), btc.scale_qty(dec!(0.123)).unwrap())
            })
            .collect();

        let as_decimals: Vec<(Decimal, Decimal)> =
            levels.iter().map(|l| (btc.unscale_price(l.price()), btc.unscale_qty(l.qty()))).collect();

        let scaled_bytes = rmp_serde::to_vec_named(&levels).unwrap().len();
        let decimal_bytes = rmp_serde::to_vec_named(&as_decimals).unwrap().len();

        println!("100 levels: {scaled_bytes} bytes scaled, {decimal_bytes} as decimals");
        assert!(scaled_bytes < 1024, "a 100-level frame must fit a kibibyte, got {scaled_bytes}");
        assert!(
            scaled_bytes * 2 < decimal_bytes,
            "the scaled form must be worth having: {scaled_bytes} against {decimal_bytes}"
        );
    }

    #[test]
    fn a_level_carries_no_field_names() {
        // A map key costs more than the value it labels when the value is four bytes and there
        // are two hundred of them in a frame.
        let level = ScaledLevel::new(Scaled(6_309_601), Scaled(123));
        let encoded = rmp_serde::to_vec_named(&level).unwrap();
        assert!(encoded.len() <= 10, "encoded as {} bytes: {encoded:02x?}", encoded.len());

        let decoded: ScaledLevel = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(decoded, level);
        assert_eq!((decoded.price(), decoded.qty()), (Scaled(6_309_601), Scaled(123)));
    }

    #[test]
    fn a_scaled_value_is_meaningless_without_its_instrument() {
        // Stated as a property of the types rather than a comment: the same integer means
        // different things on different instruments, and mixing them is what a distinct type
        // is there to make awkward.
        let (btc, shib) = (btc(), shib());
        let scaled = btc.scale_price(dec!(63096.01)).unwrap();
        assert_ne!(
            shib.unscale_price(scaled),
            btc.unscale_price(scaled),
            "the same integer must not be read as the same price"
        );
    }
}
