//! Account state: balances on every market, positions on derivatives only.

use crate::{ids::Symbol, order::PositionSide, time::Timestamp};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    pub asset: String,
    /// Available to trade.
    pub free: Decimal,
    /// Reserved by working orders or posted as margin.
    pub locked: Decimal,
}

impl Balance {
    pub fn total(&self) -> Decimal {
        self.free + self.locked
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarginMode {
    #[default]
    Cross,
    Isolated,
}

/// An open derivatives position.
///
/// `qty` is always non-negative; direction lives in `side`. Netting maths uses
/// [`crate::order::Side::sign`] rather than a signed quantity, so the sign convention is
/// stated in exactly one place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub symbol: Symbol,
    /// `Both` in one-way mode; `Long`/`Short` when the account is hedged.
    pub side: PositionSide,
    pub qty: Decimal,
    pub entry_price: Decimal,
    /// Venue mark price, the basis for unrealised PnL and liquidation.
    pub mark_price: Decimal,
    pub unrealized_pnl: Decimal,
    pub leverage: Decimal,
    pub margin_mode: MarginMode,
    /// Absent when the venue does not publish one, or the position is flat.
    pub liquidation_price: Option<Decimal>,
    pub updated_at: Timestamp,
}

impl Position {
    pub fn is_flat(&self) -> bool {
        self.qty.is_zero()
    }

    /// Position value at the mark, in quote currency.
    pub fn notional(&self) -> Decimal {
        self.qty * self.mark_price
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ExchangeId, MarketKind};
    use rust_decimal_macros::dec;

    #[test]
    fn balance_total_includes_what_orders_reserved() {
        let b = Balance { asset: "USDT".into(), free: dec!(100), locked: dec!(25) };
        assert_eq!(b.total(), dec!(125));
    }

    #[test]
    fn flat_position_has_no_notional() {
        let p = Position {
            symbol: Symbol::new(ExchangeId::Binance, MarketKind::LinearPerp, "BTCUSDT"),
            side: PositionSide::Long,
            qty: Decimal::ZERO,
            entry_price: Decimal::ZERO,
            mark_price: dec!(64000),
            unrealized_pnl: Decimal::ZERO,
            leverage: dec!(10),
            margin_mode: MarginMode::Cross,
            liquidation_price: None,
            updated_at: Timestamp::ZERO,
        };
        assert!(p.is_flat());
        assert_eq!(p.notional(), Decimal::ZERO);
    }
}
