//! Shared numeric types and small enums used by every other module.
//!
//! **Contract.** All engine arithmetic is on integers. There are no floats anywhere in
//! `engine/`, so every result is exact and identical on every machine, which is what makes
//! replay deterministic.
//!
//! **Units.**
//! - `Price` is a count of ticks and `Qty` a count of lots, both per market.
//! - `Micros` is collateral in millionths of a dollar (USDC has 6 decimals).
//! - One tick times one lot is exactly one micro-dollar. That holds for all 88 Polymarket
//!   Perps instruments (their `price_decimals + quantity_decimals` is always 6), so the
//!   notional of a fill in micros is simply `price * qty`, with no scaling or rounding.
//!   See `docs/DECISIONS.md` D-004.
//!
//! **Invariants.** Products of two `i64` values can overflow `i64` (e.g. a large price
//! times a large quantity), so they are computed in `i128` and checked on the way back.
//!
//! These are type aliases, not newtypes: arithmetic stays readable, and unit mix-ups are
//! caught by the equivalence and invariant tests instead (D-004 records the trade-off).

/// A price, in ticks. Always positive for a valid order.
pub type Price = i64;

/// A quantity, in lots. Always positive for a valid order; signed for positions
/// (positive = long, negative = short).
pub type Qty = i64;

/// An amount of collateral, in micro-dollars (1 USDC = 1_000_000).
pub type Micros = i64;

/// Identifies a market (Polymarket calls these instruments).
pub type MarketId = u16;

/// Identifies an account.
pub type AccountId = u32;

/// Identifies an order. Globally unique: the owning account sits in the high 32 bits and
/// a per-account sequence number in the low 32 bits, so the owner can be read straight off
/// the id (see [`order_id`] and [`account_of`]). Clients choose the sequence, which lets
/// pre-signed cancels and modifies refer to orders before the engine has seen them.
pub type OrderId = u64;

/// Builds an order id from its owning account and that account's sequence number.
pub const fn order_id(account: AccountId, seq: u32) -> OrderId {
    ((account as u64) << 32) | seq as u64
}

/// The account that owns an order.
pub const fn account_of(id: OrderId) -> AccountId {
    (id >> 32) as AccountId
}

/// An order's sequence number within its account: the low 32 bits of its id. The engine
/// accepts an account's orders only with rising sequence numbers (`docs/RISK.md` 3.1).
pub const fn sequence_of(id: OrderId) -> u32 {
    id as u32
}

/// Notional value of `qty` lots at `price` ticks, in micro-dollars.
///
/// Returns `None` on overflow instead of wrapping. Because one tick times one lot is one
/// micro-dollar (module docs), this is just the product.
pub fn notional(price: Price, qty: Qty) -> Option<Micros> {
    let product = (price as i128) * (qty as i128);
    Micros::try_from(product).ok()
}

/// Which side of the book an order is on.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Buy = 0,
    Sell = 1,
}

impl Side {
    /// The side this order trades against.
    pub const fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    /// `qty` lots traded on this side, as a signed change of position: positive for a buy,
    /// negative for a sell (a long position is positive).
    pub const fn signed(self, qty: Qty) -> Qty {
        match self {
            Side::Buy => qty,
            Side::Sell => -qty,
        }
    }
}

/// How long an order may rest.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimeInForce {
    /// Good 'til cancelled: whatever doesn't fill immediately rests on the book.
    Gtc = 0,
    /// Immediate or cancel: fill what's possible now, cancel the rest. Never rests.
    Ioc = 1,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_id_round_trips_the_account() {
        let id = order_id(7, 42);
        assert_eq!(account_of(id), 7);
        assert_eq!(sequence_of(id), 42);
        let last = order_id(AccountId::MAX, u32::MAX);
        assert_eq!((account_of(last), sequence_of(last)), (AccountId::MAX, u32::MAX));
    }

    #[test]
    fn a_side_signs_a_quantity_as_a_position_change() {
        assert_eq!(Side::Buy.signed(5), 5);
        assert_eq!(Side::Sell.signed(5), -5);
    }

    #[test]
    fn notional_is_price_times_qty_and_reports_overflow() {
        // 1 unit of SP500 at 7,502.4: 75,024 ticks of 0.1 and 100,000 lots of 0.00001.
        assert_eq!(notional(75_024, 100_000), Some(7_502_400_000)); // $7,502.40
        assert_eq!(notional(i64::MAX, 2), None);
    }
}
