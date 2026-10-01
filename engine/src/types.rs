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
//! times a large quantity). The ones that can really be larger (a client's quantity before
//! the size check, a notional times a fee rate, the terms of a liquidation key) are computed
//! in `i128` and checked on the way back. The rest are plain `i64` products that RISK.md
//! 2.4's size limits keep in range, and overflow panics in every build of the engine rather
//! than wrapping (overflow checks, D-004).
//!
//! **The three units are types of their own.** `Price`, `Qty` and `Micros` are newtypes
//! over `i64`, so a price can't go where a quantity or an amount of money is expected, and
//! adding two different units doesn't compile. Values of one unit add and subtract (a
//! difference of two prices is a number of ticks, so a `Price`), and `Qty` and `Micros`,
//! which are signed (a short, a loss), negate. The one product between units is D-004's
//! identity: lots times ticks is micro-dollars, `qty * price` (or `price * qty`) is
//! `Micros`. Everything else, such as scaling by a rate in parts per million, dividing by a
//! leverage, or widening to `i128`, goes through the bare number explicitly: `.ticks()`,
//! `.lots()` or `.micros()` out, `::new` back in, `i128::from(value)` to widen. `Debug` and
//! `Display` print the bare number, exactly as an `i64` does, so every report, snapshot and
//! log reads as before.
//!
//! A type can't tell apart two values of the same unit: a cost basis and a collateral
//! amount are both `Micros`. The money formulas that read both take the slot whole instead
//! ([`crate::money::SlotMoney`], D-004), and a position change takes the position and the
//! change each whole ([`crate::money::Holding`]).
//!
//! **Identifiers are types of their own too.** [`MarketId`] (a `u16`), [`AccountId`] (a
//! `u32`), [`OrderId`] (a `u64`) and [`OrderSeq`], an order's sequence number within its
//! account (a `u32`), are newtypes. As bare integers they could pass for other numbers of
//! the same width: a market id for a leverage (both `u16`), an account id for a sequence
//! number or a count, an order id for a nonce or a timestamp. An id is a name, not a
//! number, so it has no arithmetic and no implicit conversion: `::new(n)` names one,
//! `.get()` reads the number back where a record, a message or a report carries it or where
//! something is derived from the number (a market's class is its id mod 3, an account's
//! gateway its id mod the number of gateways), and a market's or an account's `.index()` is
//! its position in a list kept by id. [`order_id`] builds an order id from its account and
//! sequence number, and [`account_of`] and [`sequence_of`] read them back. `Debug`,
//! `Display`, `Hash` and the ordering are the bare integer's, so every printout, hash and
//! sorted list is as before.

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Mul, Neg, Sub, SubAssign};

/// A price, in ticks. Always positive for a valid order. The difference of two prices is a
/// number of ticks, so it is a `Price` too.
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(i64);

impl Price {
    /// Zero ticks.
    pub const ZERO: Price = Price(0);

    /// One tick: the smallest move of a price.
    pub const ONE_TICK: Price = Price(1);

    /// A price of `ticks` ticks.
    pub const fn new(ticks: i64) -> Self {
        Price(ticks)
    }

    /// The number of ticks.
    pub const fn ticks(self) -> i64 {
        self.0
    }
}

/// A quantity, in lots. Always positive for a valid order; signed for positions
/// (positive = long, negative = short).
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Qty(i64);

impl Qty {
    /// No lots: a flat position, or nothing left to fill.
    pub const ZERO: Qty = Qty(0);

    /// A quantity of `lots` lots.
    pub const fn new(lots: i64) -> Self {
        Qty(lots)
    }

    /// The number of lots.
    pub const fn lots(self) -> i64 {
        self.0
    }

    /// The size of a signed quantity: a long's or a short's lots, as a positive number.
    pub const fn abs(self) -> Qty {
        Qty(self.0.abs())
    }
}

/// An amount of collateral, in micro-dollars (1 USDC = 1_000_000).
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Micros(i64);

impl Micros {
    /// No money.
    pub const ZERO: Micros = Micros(0);

    /// An amount of `micros` micro-dollars.
    pub const fn new(micros: i64) -> Self {
        Micros(micros)
    }

    /// The number of micro-dollars.
    pub const fn micros(self) -> i64 {
        self.0
    }

    /// The size of a signed amount, as a positive number.
    pub const fn abs(self) -> Micros {
        Micros(self.0.abs())
    }

    /// `self + other`, or `None` if the sum doesn't fit in `i64`: for the checks that
    /// reject an amount (or name what overflowed) rather than panic on a plain `+`.
    pub fn checked_add(self, other: Micros) -> Option<Micros> {
        self.0.checked_add(other.0).map(Micros)
    }
}

/// `Debug` and `Display` for a newtype over an integer (the units and the identifiers):
/// both print the bare number, exactly as the integer does, with any width, padding, sign
/// or `{:#?}` flag passed on. So whatever prints a value reads as it did when the type was
/// the integer itself: the reports, the logs, `keys.txt` and the snapshots compared by
/// replay.
macro_rules! prints_as_its_number {
    ($type:ident) => {
        impl fmt::Debug for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.0, f)
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

/// The trait implementations the three units share, written once for each of them:
/// - `Debug` and `Display` print the bare number, exactly as an `i64` does
///   (`prints_as_its_number!`);
/// - `+`, `-`, `+=` and `-=` between two values of the same unit, never of two units;
/// - `i128::from(value)`, for the products and sums that are computed in `i128`.
macro_rules! unit_traits {
    ($unit:ident) => {
        prints_as_its_number!($unit);

        impl Add for $unit {
            type Output = $unit;
            fn add(self, other: $unit) -> $unit {
                $unit(self.0 + other.0)
            }
        }

        impl Sub for $unit {
            type Output = $unit;
            fn sub(self, other: $unit) -> $unit {
                $unit(self.0 - other.0)
            }
        }

        impl AddAssign for $unit {
            fn add_assign(&mut self, other: $unit) {
                self.0 += other.0;
            }
        }

        impl SubAssign for $unit {
            fn sub_assign(&mut self, other: $unit) {
                self.0 -= other.0;
            }
        }

        impl From<$unit> for i128 {
            fn from(value: $unit) -> i128 {
                i128::from(value.0)
            }
        }
    };
}

unit_traits!(Price);
unit_traits!(Qty);
unit_traits!(Micros);

/// What the two signed units, `Qty` and `Micros`, have on top: `-value` (a sell's lots, a
/// rebate, a loss) and `.sum()` of an iterator of values.
macro_rules! signed_unit_traits {
    ($unit:ident) => {
        impl Neg for $unit {
            type Output = $unit;
            fn neg(self) -> $unit {
                $unit(-self.0)
            }
        }

        impl Sum for $unit {
            fn sum<I: Iterator<Item = $unit>>(values: I) -> $unit {
                values.fold($unit::ZERO, |total, value| total + value)
            }
        }
    };
}

signed_unit_traits!(Qty);
signed_unit_traits!(Micros);

/// D-004's identity, the only product between two units: one lot at one tick is one
/// micro-dollar, so `qty × price` is a notional in micros, with no scaling. A plain `i64`
/// product: RISK.md 2.4's size limits keep every one the engine forms below 2^53 in size,
/// and if a limit were ever broken it would panic rather than wrap (module docs). For a
/// client's values, which nothing has limited yet, use [`notional`].
impl Mul<Price> for Qty {
    type Output = Micros;
    fn mul(self, price: Price) -> Micros {
        Micros(self.0 * price.0)
    }
}

/// The same product, written the other way round: `price × qty`.
impl Mul<Qty> for Price {
    type Output = Micros;
    fn mul(self, qty: Qty) -> Micros {
        qty * self
    }
}

/// Identifies a market (Polymarket calls these instruments). The engine keeps its markets
/// in a list, each at the position of its id ([`MarketId::index`]).
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MarketId(u16);

impl MarketId {
    /// Market number `n`.
    pub const fn new(n: u16) -> Self {
        MarketId(n)
    }

    /// The number itself: for the records, messages and reports that carry it, and for what
    /// is derived from it (a flow's market class, start price and random streams).
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The market's position in a list kept by market id, such as the engine's markets.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Identifies an account.
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(u32);

impl AccountId {
    /// The highest id, which no client may hold: the engine's insurance fund, and the
    /// account named in the reject of an operator command (`engine::FUND`).
    pub const MAX: AccountId = AccountId(u32::MAX);

    /// Account number `n`.
    pub const fn new(n: u32) -> Self {
        AccountId(n)
    }

    /// The number itself: for the records, messages and reports that carry it, and for what
    /// is derived from it (the gateway that serves the account, the cohort it belongs to).
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The account's position in a list kept by account id, such as a client's last
    /// sequence number per account. A `u32` always fits in a `usize` on the 64-bit
    /// machines this runs on.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Identifies an order. Globally unique: the owning account sits in the high 32 bits and
/// a per-account sequence number in the low 32 bits, so the owner can be read straight off
/// the id (see [`order_id`] and [`account_of`]). Clients choose the sequence, which lets
/// pre-signed cancels and modifies refer to orders before the engine has seen them.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderId(u64);

impl OrderId {
    /// The order whose id is `n`: for the records and messages that carry an id whole, and
    /// for the fixed ids that stand for no real order (the engine's `NO_ORDER`, a quote's
    /// placeholder before it is placed, the book's prefault fillers). An order's real id is
    /// built with [`order_id`].
    pub const fn new(n: u64) -> Self {
        OrderId(n)
    }

    /// The 64 bits of the id: for the records, messages and reports that carry it, and for
    /// [`account_of`] and [`sequence_of`], which read its two halves.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// An order's sequence number within its account: the low 32 bits of its id. A type of its
/// own rather than a bare `u32`, so that [`order_id`] can't be handed the account and the
/// sequence number the wrong way round.
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderSeq(u32);

impl OrderSeq {
    /// Sequence number `n`.
    pub const fn new(n: u32) -> Self {
        OrderSeq(n)
    }

    /// The number itself.
    pub const fn get(self) -> u32 {
        self.0
    }
}

prints_as_its_number!(MarketId);
prints_as_its_number!(AccountId);
prints_as_its_number!(OrderId);
prints_as_its_number!(OrderSeq);

/// Builds an order id from its owning account and that account's sequence number.
pub const fn order_id(account: AccountId, seq: OrderSeq) -> OrderId {
    OrderId::new(((account.get() as u64) << 32) | seq.get() as u64)
}

/// The account that owns an order.
pub const fn account_of(id: OrderId) -> AccountId {
    AccountId::new((id.get() >> 32) as u32)
}

/// An order's sequence number within its account: the low 32 bits of its id. The engine
/// accepts an account's orders only with rising sequence numbers (`docs/RISK.md` 3.1).
pub const fn sequence_of(id: OrderId) -> OrderSeq {
    OrderSeq::new(id.get() as u32)
}

/// Notional value of `qty` lots at `price` ticks, in micro-dollars, for values of any size.
///
/// Returns `None` on overflow instead of wrapping or panicking. Because one tick times one
/// lot is one micro-dollar (module docs), this is just the product, as `qty * price` is for
/// the values the engine has already limited.
pub fn notional(price: Price, qty: Qty) -> Option<Micros> {
    let product = i128::from(price) * i128::from(qty);
    i64::try_from(product).ok().map(Micros::new)
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
    pub fn signed(self, qty: Qty) -> Qty {
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
        let id = order_id(AccountId::new(7), OrderSeq::new(42));
        assert_eq!(id.get(), 7 << 32 | 42, "the account in the high 32 bits");
        assert_eq!(account_of(id), AccountId::new(7));
        assert_eq!(sequence_of(id), OrderSeq::new(42));
        let last = order_id(AccountId::MAX, OrderSeq::new(u32::MAX));
        assert_eq!(last, OrderId::new(u64::MAX));
        assert_eq!((account_of(last), sequence_of(last).get()), (AccountId::MAX, u32::MAX));
    }

    #[test]
    fn the_identifiers_print_hash_and_sort_as_their_bare_numbers() {
        // Whatever prints an id, with any format, reads as it did as a bare integer: the
        // reports, `keys.txt` and the snapshots compared by replay.
        fn every_format(value: impl fmt::Debug + fmt::Display) -> String {
            format!("{0:?} {0} {0:#?} {0:>12} {0:<12}| {0:+} {0:x?} {0:08}", value)
        }
        // A hash map keyed by an id hashes it as it hashed the integer (the engine's maps,
        // `id_hash.rs`), so the same keys land in the same buckets.
        fn hash(value: impl std::hash::Hash) -> u64 {
            use std::hash::BuildHasher;
            std::hash::BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default()
                .hash_one(value)
        }
        for n in [0, 7, u16::MAX] {
            assert_eq!(every_format(MarketId::new(n)), every_format(n));
            assert_eq!(hash(MarketId::new(n)), hash(n));
        }
        for n in [0, 7, 4_000_000_000, u32::MAX] {
            assert_eq!(every_format(AccountId::new(n)), every_format(n));
            assert_eq!(every_format(OrderSeq::new(n)), every_format(n));
            assert_eq!(hash(AccountId::new(n)), hash(n));
            assert_eq!(hash(OrderSeq::new(n)), hash(n));
        }
        for n in [0, 7, 7 << 32 | 42, u64::MAX] {
            assert_eq!(every_format(OrderId::new(n)), every_format(n));
            assert_eq!(hash(OrderId::new(n)), hash(n));
        }
        // Inside a derived `Debug`, as in a snapshot or an event, an id is its number.
        let fields = (MarketId::new(3), Some(AccountId::new(9)), [OrderId::new(5)]);
        assert_eq!(format!("{fields:?}"), format!("{:?}", (3, Some(9), [5])));
        assert_eq!(format!("{fields:#?}"), format!("{:#?}", (3, Some(9), [5])));
        // Ids sort as their numbers, so a list sorted by id is in the same order as before.
        assert!(AccountId::new(9) < AccountId::new(10) && AccountId::new(10) < AccountId::MAX);
        assert!(OrderId::new(u64::MAX - 1) < OrderId::new(u64::MAX));
        assert_eq!((MarketId::default(), AccountId::default()), (MarketId::new(0), AccountId::new(0)));
        assert_eq!((MarketId::new(66).index(), AccountId::new(4_000_000_000).index()), (66, 4_000_000_000));
    }

    #[test]
    fn the_units_print_as_their_bare_numbers() {
        // Whatever prints a unit, with any format, reads as it did as an `i64`: the reports,
        // the logs and the snapshots compared by replay.
        fn every_format(value: impl fmt::Debug + fmt::Display) -> String {
            format!("{0:?} {0} {0:#?} {0:>14} {0:<14}| {0:+} {0:x?}", value)
        }
        for n in [0, 7, -7_502_400_000, i64::MAX, i64::MIN] {
            let expected = every_format(n);
            assert_eq!(every_format(Price::new(n)), expected);
            assert_eq!(every_format(Qty::new(n)), expected);
            assert_eq!(every_format(Micros::new(n)), expected);
        }
        // Inside a derived `Debug`, as in a snapshot, a unit is its number.
        let fields = (Price::new(75_024), Some(Qty::new(-3)), [Micros::new(5)]);
        assert_eq!(format!("{fields:?}"), format!("{:?}", (75_024, Some(-3), [5])));
        assert_eq!(format!("{fields:#?}"), format!("{:#?}", (75_024, Some(-3), [5])));
    }

    #[test]
    fn lots_times_ticks_is_micros() {
        // D-004: 1 unit of SP500 at 7,502.4 is 75,024 ticks of 0.1 and 100,000 lots of
        // 0.00001, which is $7,502.40.
        let (price, qty) = (Price::new(75_024), Qty::new(100_000));
        assert_eq!(qty * price, Micros::new(7_502_400_000));
        assert_eq!(price * qty, Micros::new(7_502_400_000));
        // A short's notional is negative, as its cost basis is.
        assert_eq!(-qty * price, Micros::new(-7_502_400_000));
    }

    #[test]
    #[should_panic(expected = "overflow")]
    fn a_product_that_overflows_panics_rather_than_wraps() {
        let _ = Qty::new(i64::MAX) * Price::new(2);
    }

    #[test]
    fn values_of_one_unit_compare_add_and_sum_as_their_numbers_do() {
        assert_eq!((Price::ZERO, Qty::ZERO, Micros::ZERO), (Price::new(0), Qty::new(0), Micros::new(0)));
        assert_eq!(Price::default(), Price::ZERO);
        assert!(Price::new(-1) < Price::ZERO && Price::ZERO < Price::new(1));
        assert_eq!(Qty::new(5).max(Qty::new(-9)), Qty::new(5));
        assert_eq!(Micros::new(3).min(Micros::ZERO), Micros::ZERO);
        assert_eq!(Price::new(100_500) - Price::new(100_000), Price::new(500), "a number of ticks");
        let mut locked = Micros::new(5_000_000);
        locked += Micros::new(25);
        locked -= Micros::new(5_000_000);
        assert_eq!(locked, Micros::new(25));
        assert_eq!((-Qty::new(4)).abs(), Qty::new(4));
        assert_eq!(Micros::new(-12).abs(), Micros::new(12));
        let open: Qty = [2, 7, 4].into_iter().map(Qty::new).sum();
        assert_eq!(open, Qty::new(13));
        let fees: Micros = [Micros::new(40_400), Micros::new(-4_975)].into_iter().sum();
        assert_eq!(fees, Micros::new(35_425));
        assert_eq!(Micros::new(i64::MAX - 1).checked_add(Micros::new(1)), Some(Micros::new(i64::MAX)));
        assert_eq!(Micros::new(i64::MAX).checked_add(Micros::new(1)), None);
        assert_eq!(i128::from(Micros::new(i64::MIN)), i128::from(i64::MIN));
        assert_eq!(i128::from(Price::new(7)) * i128::from(Qty::new(-3)), -21);
    }

    #[test]
    fn a_side_signs_a_quantity_as_a_position_change() {
        assert_eq!(Side::Buy.signed(Qty::new(5)), Qty::new(5));
        assert_eq!(Side::Sell.signed(Qty::new(5)), Qty::new(-5));
    }

    #[test]
    fn notional_is_price_times_qty_and_reports_overflow() {
        // 1 unit of SP500 at 7,502.4: 75,024 ticks of 0.1 and 100,000 lots of 0.00001.
        let price = Price::new(75_024);
        assert_eq!(notional(price, Qty::new(100_000)), Some(Micros::new(7_502_400_000))); // $7,502.40
        assert_eq!(notional(Price::new(i64::MAX), Qty::new(2)), None);
    }
}
