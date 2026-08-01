//! Instrument metadata and the rounding rules that come with it.
//!
//! Every venue rejects orders whose price is off the tick grid or whose quantity is off the
//! step grid. Rounding therefore belongs next to the instrument, not scattered through the
//! connectors: one place to get right, one place to test.

use crate::ids::Symbol;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};

/// Static (slow-changing) description of a tradable instrument.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instrument {
    pub symbol: Symbol,
    /// Asset being traded, e.g. `BTC`.
    pub base: String,
    /// Asset the price is quoted in, e.g. `USDT`.
    pub quote: String,
    /// Asset that margin is posted in. Equals `quote` on spot and linear perps.
    pub margin_asset: String,
    /// Minimum price increment.
    pub tick_size: Decimal,
    /// Minimum quantity increment.
    pub step_size: Decimal,
    pub min_qty: Decimal,
    /// Smallest `price * qty` the venue accepts. Zero when the venue imposes none.
    pub min_notional: Decimal,
    /// False while the venue has the instrument halted or delisted.
    pub trading: bool,
}

/// Why an order would be rejected by the venue before it is worth sending.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OrderCheck {
    #[error("instrument is not trading")]
    NotTrading,
    #[error("quantity is below the venue minimum")]
    QtyBelowMin,
    #[error("quantity is not a multiple of the step size")]
    QtyOffStep,
    #[error("price is not a multiple of the tick size")]
    PriceOffTick,
    #[error("price * qty is below the venue minimum notional")]
    NotionalBelowMin,
}

impl Instrument {
    /// Snap a price down to the tick grid.
    ///
    /// Always rounds toward zero so a rounded buy never ends up more aggressive than asked.
    pub fn round_price(&self, price: Decimal) -> Decimal {
        round_to_grid(price, self.tick_size)
    }

    /// Snap a quantity down to the step grid, so the rounded order never exceeds intent.
    pub fn round_qty(&self, qty: Decimal) -> Decimal {
        round_to_grid(qty, self.step_size)
    }

    /// Reject locally what the venue would reject remotely.
    ///
    /// Cheaper than a round trip and, more importantly, keeps a rejected order out of the
    /// order-state machine entirely.
    pub fn check(&self, price: Decimal, qty: Decimal) -> Result<(), OrderCheck> {
        if !self.trading {
            return Err(OrderCheck::NotTrading);
        }
        if qty < self.min_qty {
            return Err(OrderCheck::QtyBelowMin);
        }
        if !self.step_size.is_zero() && !(qty % self.step_size).is_zero() {
            return Err(OrderCheck::QtyOffStep);
        }
        if !self.tick_size.is_zero() && !(price % self.tick_size).is_zero() {
            return Err(OrderCheck::PriceOffTick);
        }
        if !self.min_notional.is_zero() && price * qty < self.min_notional {
            return Err(OrderCheck::NotionalBelowMin);
        }
        Ok(())
    }
}

/// Round `value` down to the nearest multiple of `grid`. A zero grid means no constraint.
fn round_to_grid(value: Decimal, grid: Decimal) -> Decimal {
    if grid.is_zero() {
        return value;
    }
    (value / grid).round_dp_with_strategy(0, RoundingStrategy::ToZero) * grid
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ExchangeId, MarketKind};
    use rust_decimal_macros::dec;

    fn btc_perp() -> Instrument {
        Instrument {
            symbol: Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT"),
            base: "BTC".into(),
            quote: "USDT".into(),
            margin_asset: "USDT".into(),
            tick_size: dec!(0.10),
            step_size: dec!(0.001),
            min_qty: dec!(0.001),
            min_notional: dec!(5),
            trading: true,
        }
    }

    #[test]
    fn rounding_never_makes_an_order_more_aggressive() {
        let i = btc_perp();
        assert_eq!(i.round_price(dec!(64123.4567)), dec!(64123.4));
        assert_eq!(i.round_qty(dec!(0.0019)), dec!(0.001));
    }

    #[test]
    fn zero_grid_leaves_the_value_untouched() {
        let mut i = btc_perp();
        i.tick_size = Decimal::ZERO;
        assert_eq!(i.round_price(dec!(64123.4567)), dec!(64123.4567));
    }

    #[test]
    fn check_catches_what_the_venue_would_reject() {
        let i = btc_perp();
        assert_eq!(i.check(dec!(64123.4), dec!(0.001)), Ok(()));
        assert_eq!(i.check(dec!(64123.45), dec!(0.001)), Err(OrderCheck::PriceOffTick));
        assert_eq!(i.check(dec!(64123.4), dec!(0.0015)), Err(OrderCheck::QtyOffStep));
        assert_eq!(i.check(dec!(64123.4), dec!(0.0005)), Err(OrderCheck::QtyBelowMin));

        // On the tick and step grids, but too small to be worth sending.
        let mut cheap = btc_perp();
        cheap.min_qty = dec!(0.001);
        assert_eq!(cheap.check(dec!(1000), dec!(0.001)), Err(OrderCheck::NotionalBelowMin));
    }

    #[test]
    fn halted_instrument_is_rejected_before_anything_else() {
        let mut i = btc_perp();
        i.trading = false;
        assert_eq!(i.check(dec!(64123.4), dec!(0.001)), Err(OrderCheck::NotTrading));
    }
}
