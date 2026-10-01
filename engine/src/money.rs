//! Pure money arithmetic of the risk layer: rounding, margins, equity, the price band,
//! fees, position changes, collateral release and liquidation keys (`docs/RISK.md`
//! sections 2, 4, 5, 7.2, 7.3, 8.2, 9.1 and 10.3).
//!
//! **Contract.** Every function here is pure: integers in, integers out, no state. The
//! engine (`engine.rs`) calls them and makes every state change itself, so each formula can
//! be read, tested and checked against RISK.md's worked numbers on its own. Every mode of
//! the engine shares these functions (RISK.md 14.3), so the unit tests below are what
//! checks the formulas themselves.
//!
//! **Rounding** (RISK.md 2.3). One principle: the exchange rounds in its own favour, and
//! anything that makes money withdrawable is rounded down. Requirements (IM, MM, the
//! withdrawal reserve) round up. Fees round toward plus infinity, so a rebate gets smaller,
//! never larger. Band edges round toward the mark. The cost basis removed on a reduce rounds
//! up, so the realized PnL rounds down. Equity, unrealized PnL and notional are exact.
//! Rust's `/` rounds toward zero, which for a negative value is neither up nor down, so no
//! formula here uses `/` on a value that can be negative: they use [`floor_div`] and
//! [`ceil_div`].
//!
//! **Invariant (integer widths, RISK.md 2.4).** A market's prices are below 2^32 and a
//! slot's worst-case size is at most `max_qty = floor(2^53 / max_price)`, so at every
//! command boundary a slot's notional and cost basis are at most 2^53 in size, its
//! unrealized PnL at most 2^54, its locked collateral below 2^55 and its equity below 2^56.
//! So those are `i64`. What can be larger is computed in `i128`: a worst-case size before
//! the size check (a client can send any quantity), fee products, the products inside
//! [`apply_change`], liquidation keys, the band rules and the fund's values. An `i128`
//! result is narrowed back to `i64` with [`narrow`], which panics naming the value rather
//! than wrapping.
//!
//! **Complexity.** O(1) each; the tier lookup scans at most [`MAX_TIERS`] rows.

use crate::types::{Micros, Price, Qty, Side};

/// Every market's `max_price` must be below this (RISK.md 2.4).
pub const PRICE_LIMIT: Price = 1 << 32;

/// The largest worst-case notional of one slot: 2^53 micros, about $9.0 billion
/// (RISK.md 2.4).
pub const NOTIONAL_LIMIT: Micros = 1 << 53;

/// The most rows a tier table can have (Polymarket's instruments have up to 8).
pub const MAX_TIERS: usize = 8;

/// One million: fee rates and the price band are in parts per million of notional.
pub const PPM: i64 = 1_000_000;

// ---------------------------------------------------------------------------------------
// Rounding helpers (RISK.md 2.2). Every divisor in the spec is positive.

/// `a / b` rounded toward minus infinity. `b` must be positive.
pub fn floor_div(a: i64, b: i64) -> i64 {
    debug_assert!(b > 0, "divisor {b} is not positive");
    a.div_euclid(b)
}

/// `a / b` rounded toward plus infinity. `b` must be positive.
pub fn ceil_div(a: i64, b: i64) -> i64 {
    -floor_div(-a, b)
}

/// [`floor_div`] for `i128` operands.
pub fn floor_div_i128(a: i128, b: i128) -> i128 {
    debug_assert!(b > 0, "divisor {b} is not positive");
    a.div_euclid(b)
}

/// [`ceil_div`] for `i128` operands.
pub fn ceil_div_i128(a: i128, b: i128) -> i128 {
    -floor_div_i128(-a, b)
}

/// Narrows an `i128` result to `i64`. Panics, naming the value, if it doesn't fit: the
/// engine never wraps (RISK.md 2.4). A panic is deterministic, so replay reproduces it.
pub fn narrow(value: i128, what: &str) -> i64 {
    i64::try_from(value).unwrap_or_else(|_| panic!("{what} does not fit in i64: {value}"))
}

// ---------------------------------------------------------------------------------------
// Size limits (RISK.md 2.4).

/// The largest worst-case size a slot may reach in a market with this `max_price`:
/// `floor(2^53 / max_price)`. Every price in the market is at most `max_price`, so no
/// position or order total can be worth more than 2^53 micros at any of them.
pub fn max_qty(max_price: Price) -> Qty {
    floor_div(NOTIONAL_LIMIT, max_price)
}

// ---------------------------------------------------------------------------------------
// Equity and worst-case size (RISK.md 4.1, 4.2).

/// Unrealized PnL at `mark`: what closing the position at the mark would realize,
/// `pos × mark − cost`. Exact.
pub fn unrealized_pnl(pos: Qty, cost: Micros, mark: Price) -> Micros {
    pos * mark - cost
}

/// A slot's equity at `mark`: its locked collateral plus its unrealized PnL. Exact.
pub fn equity(pos: Qty, cost: Micros, locked: Micros, mark: Price) -> Micros {
    locked + unrealized_pnl(pos, cost, mark)
}

/// The worst-case size: the largest position the slot could reach if every resting order
/// on one side filled, `max(|pos + open_buys|, |pos − open_sells|)`. It is at least
/// `|pos|`, and fills never raise it (a buy fill moves `pos` up and `open_buys` down by the
/// same amount). In `i128`, because the pre-trade check adds a client's quantity, which can
/// be any `i64`.
pub fn worst_case_size(pos: i128, open_buys: i128, open_sells: i128) -> i128 {
    (pos + open_buys).abs().max((pos - open_sells).abs())
}

/// True if an order on `side` only shrinks the position (RISK.md 5.1): it adds to the side
/// that reduces the position, and that side's open total, counting the order in full, stays
/// within the position. A flat slot has no strictly reducing orders. These orders skip the
/// margin rule, so a trader in margin call with no free balance can still reduce or close.
pub fn is_strictly_reducing(pos: Qty, side: Side, open_buys_after: i128, open_sells_after: i128) -> bool {
    let pos = i128::from(pos);
    match side {
        Side::Sell => pos > 0 && open_sells_after <= pos,
        Side::Buy => pos < 0 && open_buys_after <= -pos,
    }
}

// ---------------------------------------------------------------------------------------
// Margin (RISK.md 4.3, 4.4, 5.2, 8.2).

/// One row of a market's leverage tiers: a notional (at the mark) of at least
/// `lower_bound` micros may use at most `max_leverage`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tier {
    pub lower_bound: Micros,
    pub max_leverage: u16,
}

/// The maximum leverage allowed for `notional`: that of the last tier whose lower bound is
/// at or below it, so a notional exactly at a bound is in that bound's tier. `tiers` is a
/// committed table: it starts at 0 and its bounds rise (RISK.md 6.9).
pub fn tier_max_leverage(tiers: &[Tier], notional: Micros) -> u16 {
    let tier = tiers.iter().take_while(|tier| tier.lower_bound <= notional).last();
    tier.expect("a committed tier table starts at 0").max_leverage
}

/// The leverage that sets the initial margin of `notional` (RISK.md 4.3): the slot's
/// chosen leverage, capped by the tier the notional falls in. The tier's rate applies to
/// the whole notional, not bracket by bracket (Polymarket FAQ).
pub fn effective_leverage(chosen: u16, tiers: &[Tier], notional: Micros) -> u16 {
    chosen.min(tier_max_leverage(tiers, notional))
}

/// Initial margin for `size` lots at `mark` (RISK.md 4.4): `ceil(size × mark / lev_eff)`,
/// with the tier picked by that same notional. `size` is the worst-case size for a
/// pre-trade check or a release, and the position's size for the margin state. It never
/// falls as `size` grows, because tiers only lower leverage as notional grows.
pub fn initial_margin(size: Qty, mark: Price, chosen_leverage: u16, tiers: &[Tier]) -> Micros {
    let notional = size * mark;
    let leverage = effective_leverage(chosen_leverage, tiers, notional);
    ceil_div(notional, i64::from(leverage))
}

/// Maintenance margin of a position (RISK.md 4.4): `ceil(|pos| × mark / (2 × max_leverage))`,
/// that is a rate of `0.5 / max_leverage`. Flat per market: no tiers, and the chosen
/// leverage plays no part (Polymarket FAQ).
pub fn maintenance_margin(pos: Qty, mark: Price, max_leverage: u16) -> Micros {
    ceil_div(pos.abs() * mark, 2 * i64::from(max_leverage))
}

/// True if a slot must be liquidated at `mark`: its equity is below maintenance margin
/// (RISK.md 4.5).
pub fn is_liquidatable(pos: Qty, cost: Micros, locked: Micros, mark: Price, max_leverage: u16) -> bool {
    equity(pos, cost, locked, mark) < maintenance_margin(pos, mark, max_leverage)
}

/// The top-up that brings `equity` up to `requirement`, or 0 if it is there already
/// (RISK.md 5.2). It is "just enough": afterwards equity equals the requirement exactly.
pub fn top_up_needed(requirement: Micros, equity: Micros) -> Micros {
    (requirement - equity).max(0)
}

/// What a slot can return to the free balance (RISK.md 8.2):
/// `max(0, min(locked, equity) − initial_margin)`, with `initial_margin` for the slot's
/// worst-case size.
///
/// Why `min(locked, equity)`: with an unrealized profit, equity is above `locked`, and the
/// minimum keeps the whole profit in the slot (only money that was deposited or realized
/// ever becomes free). With an unrealized loss, the minimum is equity, so afterwards equity
/// is exactly the initial margin, never below it. With `locked` negative, nothing goes.
pub fn release_amount(locked: Micros, equity: Micros, initial_margin: Micros) -> Micros {
    (locked.min(equity) - initial_margin).max(0)
}

/// What a withdrawal must leave behind (RISK.md 6.5): 10% of the account's open notional at
/// mark, rounded up. `open_notional` is a sum over markets, so it is `i128`.
pub fn withdrawal_reserve(open_notional: i128) -> i128 {
    ceil_div_i128(open_notional, 10)
}

// ---------------------------------------------------------------------------------------
// The price band (RISK.md 5.3).

/// The edges of the price band at one mark: buys priced above `upper` and sells priced
/// below `lower` are rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BandEdges {
    pub upper: Price,
    pub lower: Price,
}

/// The band's edges at `mark`: `floor(mark × (1 + band))` and `ceil(mark × (1 − band))`.
/// Each edge rounds toward the mark, so the allowed band is never wider than the exact
/// one. `band_ppm` is at most 450,000 (band rule 1), so `mark × (10^6 + band)` is below
/// `2^32 × 1,450,000 < 2^53`.
pub fn band_edges(mark: Price, band_ppm: u32) -> BandEdges {
    let band = i64::from(band_ppm);
    BandEdges { upper: floor_div(mark * (PPM + band), PPM), lower: ceil_div(mark * (PPM - band), PPM) }
}

/// Band rule 1 (D-015): `20 × Lmax × (band + fee) ≤ 9,000,000`, that is
/// `2 × (band + fee) ≤ 0.9 / Lmax`: INFO.md's bound with a 10% safety margin. `fee_ppm` is
/// the larger of the market's two fee rates.
pub fn band_rule_1_holds(max_leverage: u16, band_ppm: u32, fee_ppm: i32) -> bool {
    let (lmax, band, fee) = (i128::from(max_leverage), i128::from(band_ppm), i128::from(fee_ppm));
    20 * lmax * (band + fee) <= 9_000_000
}

/// Band rule 2: the exact condition the solvency argument of RISK.md 5.3 needs, including
/// the fee charged on a price at the band's edge and fee rounding (up to 2 micros per lot).
/// With `S = 10^12 − 2 × Lmax × (10^6 × (band + fee) + fee × band)`: `S > 0` and
/// `min_price × S ≥ 2 × Lmax × 10^12`. In practice a floor of about `20 × Lmax` ticks on
/// `min_price`. `fee_ppm` is the larger of the two fee rates.
pub fn band_rule_2_holds(min_price: Price, max_leverage: u16, band_ppm: u32, fee_ppm: i32) -> bool {
    const TRILLION: i128 = 1_000_000_000_000;
    let (lmax, band, fee) = (i128::from(max_leverage), i128::from(band_ppm), i128::from(fee_ppm));
    let slack = TRILLION - 2 * lmax * (1_000_000 * (band + fee) + fee * band);
    slack > 0 && i128::from(min_price) * slack >= 2 * lmax * TRILLION
}

// ---------------------------------------------------------------------------------------
// Fees and position changes (RISK.md 7.2, 7.3).

/// The fee on a fill of `qty` lots at `price` at a rate of `rate_ppm`:
/// `ceil(price × qty × rate / 10^6)`. A negative fee is a rebate; rounding toward plus
/// infinity makes a rebate smaller, never larger. The product is `i128` (up to about
/// 2^53 × 2^31); band rule 1 caps rates at 45%, so the fee fits in `i64`.
pub fn fee(price: Price, qty: Qty, rate_ppm: i32) -> Micros {
    let product = i128::from(price) * i128::from(qty) * i128::from(rate_ppm);
    narrow(ceil_div_i128(product, i128::from(PPM)), "fee")
}

/// A position after a change, and the PnL the change realized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionChange {
    pub pos: Qty,
    pub cost: Micros,
    pub realized: Micros,
}

/// Adds `lots` (signed: a buy is positive) whose signed cost is `lots_cost` to the position
/// `(pos, cost)` (RISK.md 7.3). A fill uses it with `lots_cost = lots × price`; the
/// insurance fund's netting uses it with an absorbed slot's position and cost basis
/// (RISK.md 10.1).
///
/// - **Opening or increasing:** the cost basis grows by `lots_cost`; nothing is realized.
/// - **Reducing or closing:** the cost basis shrinks in proportion to the lots removed,
///   rounded up (`removed`), so the realized PnL rounds down.
/// - **Flipping:** close all of `pos` at the change's own average price, open the rest.
///
/// In every case `realized − (cost' − cost) = −lots_cost` exactly. So the change moves a
/// slot's `locked − cost` by exactly `−lots_cost` once the realized PnL is added to
/// `locked`, whatever the rounding: conservation is exact, and the rounding only decides
/// how that is split between realized PnL (which can be released) and the remaining cost
/// basis (which cannot). Products are in `i128` (`cost × lots` is below 2^106).
pub fn apply_change(pos: Qty, cost: Micros, lots: Qty, lots_cost: Micros) -> PositionChange {
    let (pos, cost, lots, lots_cost) =
        (i128::from(pos), i128::from(cost), i128::from(lots), i128::from(lots_cost));
    let (new_pos, new_cost, realized) = if pos == 0 || lots.signum() == pos.signum() {
        (pos + lots, cost + lots_cost, 0)
    } else if lots.abs() <= pos.abs() {
        let removed = ceil_div_i128(cost * lots.abs(), pos.abs());
        (pos + lots, cost - removed, -lots_cost - removed)
    } else {
        let close_cost = ceil_div_i128(lots_cost * pos.abs(), lots.abs());
        (pos + lots, lots_cost - close_cost, -close_cost - cost)
    };
    PositionChange {
        pos: narrow(new_pos, "position"),
        cost: narrow(new_cost, "cost basis"),
        realized: narrow(realized, "realized PnL"),
    }
}

// ---------------------------------------------------------------------------------------
// Liquidation keys (RISK.md 9.1).

/// A slot's liquidation key: the side of its position (`Buy` for a long, `Sell` for a
/// short) and the tick at which it becomes liquidatable, or `None` if it never does (a flat
/// slot, or a long that stays at or above MM at every positive price).
///
/// A slot is liquidatable at mark `x` when `E(x) < MM(x) = ceil(|pos| × x / 2L)`, with
/// `L = max_leverage`. For an integer `E` and a rational `y`, `E < ceil(y)` exactly when
/// `E < y`, so the check is exactly `2L × E(x) < |pos| × x`, which is linear in `x`. So the
/// boundary comes out of one division, with no estimate to correct, and "the key is at or
/// through the mark" is exactly "equity is below MM".
pub fn liquidation_key(pos: Qty, cost: Micros, locked: Micros, max_leverage: u16) -> Option<(Side, Price)> {
    if pos > 0 {
        long_liquidation_key(pos, cost, locked, max_leverage).map(|key| (Side::Buy, key))
    } else if pos < 0 {
        Some((Side::Sell, short_liquidation_key(pos, cost, locked, max_leverage)))
    } else {
        None
    }
}

/// A long (`pos > 0`) is liquidatable exactly when `x < n / d`, with
/// `n = 2L × (cost − locked)` and `d = pos × (2L − 1)`: the highest such tick is
/// `ceil(n / d) − 1`. Below 1, the long never gets there, and has no key.
fn long_liquidation_key(pos: Qty, cost: Micros, locked: Micros, max_leverage: u16) -> Option<Price> {
    let two_l = 2 * i128::from(max_leverage);
    let n = two_l * (i128::from(cost) - i128::from(locked));
    let d = i128::from(pos) * (two_l - 1);
    let key = ceil_div_i128(n, d) - 1;
    (key >= 1).then(|| narrow(key, "long liquidation key"))
}

/// A short (`pos < 0`, `s = −pos`) is liquidatable exactly when `x > n / d`, with
/// `n = 2L × (locked − cost)` and `d = s × (2L + 1)`: the lowest such tick is
/// `floor(n / d) + 1`. It always exists.
fn short_liquidation_key(pos: Qty, cost: Micros, locked: Micros, max_leverage: u16) -> Price {
    let two_l = 2 * i128::from(max_leverage);
    let n = two_l * (i128::from(locked) - i128::from(cost));
    let d = -i128::from(pos) * (two_l + 1);
    narrow(floor_div_i128(n, d) + 1, "short liquidation key")
}

/// The same key found without the closed form: a binary search with the direct check
/// `equity < MM`, as the naive `SetMark` does (RISK.md 14.2). `safe_mark` is a mark at which
/// the slot is not liquidatable and `crossed_mark` one at which it is; for a long the safe
/// mark is the higher one, for a short the lower. Returns the tick on the crossed side of
/// the boundary that is next to it, which is the slot's key.
///
/// Why it works: the check is linear in the mark (see [`liquidation_key`]), so it changes
/// its answer exactly once between the two marks, and halving the interval finds where. It
/// only ever tries marks strictly between the two, so never one outside the market's
/// price range. O(log |safe_mark − crossed_mark|). Panics unless the slot really is safe at
/// one mark and liquidatable at the other: the naive `SetMark` relies on every slot having
/// been safe at the previous mark (invariant I5).
pub fn liquidation_key_by_search(
    pos: Qty,
    cost: Micros,
    locked: Micros,
    max_leverage: u16,
    safe_mark: Price,
    crossed_mark: Price,
) -> Price {
    let liquidatable = |mark: Price| is_liquidatable(pos, cost, locked, mark, max_leverage);
    assert!(!liquidatable(safe_mark), "the slot is liquidatable at {safe_mark}, which should be safe");
    assert!(liquidatable(crossed_mark), "the slot is not liquidatable at {crossed_mark}");
    // Kept true throughout: safe at `safe`, liquidatable at `crossed`.
    let (mut safe, mut crossed) = (safe_mark, crossed_mark);
    while (crossed - safe).abs() > 1 {
        // Strictly between the two, whichever is higher (both are positive).
        let middle = floor_div(safe + crossed, 2);
        if liquidatable(middle) {
            crossed = middle;
        } else {
            safe = middle;
        }
    }
    crossed
}

// ---------------------------------------------------------------------------------------
// The insurance fund (RISK.md 10.3).

/// The fund's unrealized PnL in one market at `mark`. In `i128`: the fund's position is a
/// sum of absorbed positions, so it has no per-slot bound.
pub fn fund_unrealized_pnl(fund_pos: Qty, fund_cost: Micros, mark: Price) -> i128 {
    i128::from(fund_pos) * i128::from(mark) - i128::from(fund_cost)
}

/// Uncovered bad debt: how far the fund's equity (balance plus unrealized PnL over all
/// markets) is below zero, or 0.
pub fn uncovered_bad_debt(fund_balance: Micros, fund_upnl_total: i128) -> i128 {
    (-(i128::from(fund_balance) + fund_upnl_total)).max(0)
}

#[cfg(test)]
mod tests {
    //! Each formula against RISK.md's worked numbers, its rounding direction at the exact
    //! boundary, and its limits.
    use super::*;

    /// SP500's live tier table (Polymarket, 2026-09-29): 50x from $0, 25x from $500,000,
    /// ..., 1x from $100M. Bounds in micros.
    const SP500_TIERS: [Tier; 8] = [
        Tier { lower_bound: 0, max_leverage: 50 },
        Tier { lower_bound: 500_000_000_000, max_leverage: 25 },
        Tier { lower_bound: 1_000_000_000_000, max_leverage: 20 },
        Tier { lower_bound: 5_000_000_000_000, max_leverage: 15 },
        Tier { lower_bound: 10_000_000_000_000, max_leverage: 10 },
        Tier { lower_bound: 25_000_000_000_000, max_leverage: 5 },
        Tier { lower_bound: 50_000_000_000_000, max_leverage: 2 },
        Tier { lower_bound: 100_000_000_000_000, max_leverage: 1 },
    ];

    /// A one-row table, as every unit-test market has.
    fn flat(max_leverage: u16) -> [Tier; 1] {
        [Tier { lower_bound: 0, max_leverage }]
    }

    #[test]
    fn floor_and_ceil_round_toward_minus_and_plus_infinity() {
        assert_eq!((floor_div(7, 2), ceil_div(7, 2)), (3, 4));
        assert_eq!((floor_div(-7, 2), ceil_div(-7, 2)), (-4, -3), "not toward zero like `/`");
        assert_eq!((floor_div(6, 2), ceil_div(6, 2)), (3, 3), "exact divisions don't round");
        assert_eq!((floor_div(0, 5), ceil_div(0, 5)), (0, 0));
        assert_eq!((floor_div_i128(-7, 2), ceil_div_i128(-7, 2)), (-4, -3));
        let big = i128::from(i64::MAX) * 1_000;
        assert_eq!(ceil_div_i128(big + 1, 1_000), i128::from(i64::MAX) + 1);
    }

    #[test]
    #[should_panic(expected = "cost basis does not fit in i64")]
    fn narrowing_names_the_value_that_overflowed() {
        narrow(i128::from(i64::MAX) + 1, "cost basis");
    }

    #[test]
    fn max_qty_keeps_a_slot_under_the_notional_limit_at_the_highest_price() {
        assert_eq!(max_qty(150_000), 60_047_995_031, "RISK.md 2.4's example");
        let (max_price, qty) = (1_000_000, max_qty(1_000_000));
        assert!(i128::from(qty) * i128::from(max_price) <= i128::from(NOTIONAL_LIMIT));
        assert!(i128::from(qty + 1) * i128::from(max_price) > i128::from(NOTIONAL_LIMIT));
        assert_eq!(max_qty(PRICE_LIMIT - 1), 2_097_152, "still at least 2^21 lots");
    }

    #[test]
    fn equity_is_collateral_plus_unrealized_pnl() {
        // RISK.md 4.1: long 1,000 lots bought at 100,000, locked 5,000,000, mark 98,000.
        assert_eq!(unrealized_pnl(1_000, 100_000_000, 98_000), -2_000_000);
        assert_eq!(equity(1_000, 100_000_000, 5_000_000, 98_000), 3_000_000);
        // A short gains as the mark falls; a flat slot's equity is its collateral.
        assert_eq!(unrealized_pnl(-1_000, -100_000_000, 98_000), 2_000_000);
        assert_eq!(equity(0, 0, 7, 12_345), 7);
    }

    #[test]
    fn worst_case_size_is_the_larger_side_and_never_overflows() {
        assert_eq!(worst_case_size(0, 0, 0), 0);
        assert_eq!(worst_case_size(10, 5, 0), 15);
        assert_eq!(worst_case_size(10, 0, 25), 15, "a flip: sells take a long of 10 to short 15");
        assert_eq!(worst_case_size(10, 0, 20), 10, "a flip that leaves W unchanged");
        assert_eq!(worst_case_size(-10, 4, 3), 13);
        let huge = i128::from(i64::MAX);
        assert_eq!(worst_case_size(huge, huge, 0), 2 * huge);
    }

    #[test]
    fn only_orders_that_shrink_the_position_within_it_are_strictly_reducing() {
        // Long 1,000 (RISK.md 5.1, T3 and T5).
        assert!(is_strictly_reducing(1_000, Side::Sell, 0, 1_000), "closes exactly");
        assert!(!is_strictly_reducing(1_000, Side::Sell, 0, 1_001), "one lot past flat");
        assert!(!is_strictly_reducing(1_000, Side::Sell, 0, 2_000), "a flip (T5)");
        assert!(!is_strictly_reducing(1_000, Side::Buy, 10, 0), "adds to the long");
        // Short 1,000: the mirror image.
        assert!(is_strictly_reducing(-1_000, Side::Buy, 1_000, 0));
        assert!(!is_strictly_reducing(-1_000, Side::Buy, 1_001, 0));
        assert!(!is_strictly_reducing(-1_000, Side::Sell, 0, 1));
        // A flat slot has none.
        assert!(!is_strictly_reducing(0, Side::Buy, 1, 0));
        assert!(!is_strictly_reducing(0, Side::Sell, 0, 1));
    }

    #[test]
    fn a_notional_exactly_at_a_tier_bound_is_in_that_tier() {
        assert_eq!(tier_max_leverage(&SP500_TIERS, 0), 50);
        assert_eq!(tier_max_leverage(&SP500_TIERS, 499_999_999_999), 50);
        assert_eq!(tier_max_leverage(&SP500_TIERS, 500_000_000_000), 25);
        assert_eq!(tier_max_leverage(&SP500_TIERS, 99_999_999_999_999), 2);
        assert_eq!(tier_max_leverage(&SP500_TIERS, 100_000_000_000_000), 1);
        assert_eq!(tier_max_leverage(&SP500_TIERS, i64::MAX), 1);
        assert_eq!(tier_max_leverage(&flat(20), i64::MAX), 20);
        assert_eq!(effective_leverage(50, &SP500_TIERS, 500_000_000_000), 25, "the tier binds");
        assert_eq!(effective_leverage(20, &SP500_TIERS, 500_000_000_000), 20, "the choice binds");
    }

    #[test]
    fn initial_margin_matches_the_worked_examples() {
        // RISK.md 4.4, SP500's real tiers at mark 75,024 with chosen leverage 50.
        assert_eq!(initial_margin(6_000_000, 75_024, 50, &SP500_TIERS), 9_002_880_000);
        assert_eq!(initial_margin(8_000_000, 75_024, 50, &SP500_TIERS), 24_007_680_000, "25x, 4%");
        // RISK.md 13, the tier boundary at mark 100,000: exactly $500,000 is in the 25x tier.
        assert_eq!(initial_margin(5_000_000, 100_000, 50, &SP500_TIERS), 20_000_000_000);
        assert_eq!(initial_margin(4_999_999, 100_000, 50, &SP500_TIERS), 9_999_998_000);
        assert_eq!(initial_margin(5_000_000, 100_000, 20, &SP500_TIERS), 25_000_000_000);
        // RISK.md 5.2: 1,000 lots at 100,000 at 20x.
        assert_eq!(initial_margin(1_000, 100_000, 20, &flat(20)), 5_000_000);
        assert_eq!(initial_margin(0, 100_000, 20, &flat(20)), 0);
    }

    #[test]
    fn requirements_round_up() {
        // 7 lots at 3 ticks is 21 micros; at 20x that is 1.05 micros of IM.
        assert_eq!(initial_margin(7, 3, 20, &flat(20)), 2);
        // MM at 20x is 2.5%: 21 / 40 = 0.525 micros.
        assert_eq!(maintenance_margin(7, 3, 20), 1);
        assert_eq!(maintenance_margin(-7, 3, 20), 1, "a short's MM is on |pos|");
        assert_eq!(withdrawal_reserve(100_000_001), 10_000_001);
    }

    #[test]
    fn maintenance_margin_is_flat_per_market() {
        // RISK.md 4.4: a 20x market, long 100,000 lots at mark 73,100.
        assert_eq!(maintenance_margin(100_000, 73_100, 20), 182_750_000);
        assert_eq!(maintenance_margin(0, 73_100, 20), 0);
        // T1's boundary (RISK.md 9.2): liquidatable at 73,100, not at 73,101.
        let (pos, cost, locked) = (100_000, 7_502_400_000, 375_120_000);
        assert!(is_liquidatable(pos, cost, locked, 73_100, 20));
        assert!(!is_liquidatable(pos, cost, locked, 73_101, 20));
    }

    #[test]
    fn a_top_up_is_just_enough_and_a_release_never_goes_below_im() {
        // RISK.md 5.2: IM 5,000,000 on a flat slot.
        assert_eq!(top_up_needed(5_000_000, 0), 5_000_000);
        assert_eq!(top_up_needed(5_000_000, 6_000_000), 0);
        // RISK.md 8.2 (long 1,000 at 100,000, locked 7,500,000; W falls to 1,000).
        // Mark 102,000: the minimum is `locked`, so the 2,000,000 profit stays in the slot.
        let equity_up = equity(1_000, 100_000_000, 7_500_000, 102_000);
        assert_eq!(release_amount(7_500_000, equity_up, 5_100_000), 2_400_000);
        // Mark 98,000: the minimum is equity, which ends exactly at IM.
        let equity_down = equity(1_000, 100_000_000, 7_500_000, 98_000);
        assert_eq!(release_amount(7_500_000, equity_down, 4_900_000), 600_000);
        // Negative collateral releases nothing, whatever the equity.
        assert_eq!(release_amount(-5, 1_000_000, 0), 0);
        assert_eq!(release_amount(3_000_000, 2_000_000, 2_500_000), 0);
    }

    #[test]
    fn fees_round_toward_plus_infinity_so_rebates_round_down_in_size() {
        // RISK.md 7.2 (taker 400 ppm, maker 125 ppm).
        assert_eq!(fee(101_000, 1_000, 400), 40_400);
        assert_eq!(fee(101_000, 1_000, 125), 12_625);
        assert_eq!(fee(99_500, 1_000, 400), 39_800);
        assert_eq!(fee(99_500, 1_000, 125), 12_438, "12,437.5 rounds up");
        assert_eq!(fee(99_500, 1_000, -50), -4_975, "an exact rebate");
        assert_eq!(fee(12_345, 1, -50), 0, "a rebate of 0.61725 rounds to none");
        assert_eq!(fee(12_345, 1, 0), 0);
        // The largest fill a market allows, at the largest rate band rule 1 allows.
        let largest = fee(1_000_000, max_qty(1_000_000), 450_000);
        assert!(largest > 0 && largest < NOTIONAL_LIMIT / 2);
    }

    #[test]
    fn band_edges_round_toward_the_mark() {
        // RISK.md 5.3: SP500 at 75,024 with a band of 8,600 ppm.
        assert_eq!(band_edges(75_024, 8_600), BandEdges { upper: 75_669, lower: 74_379 });
        // T3, T5, T6: 2% at 100,000 is exact; at 98,040 the upper edge is exactly 100,000.
        assert_eq!(band_edges(100_000, 20_000), BandEdges { upper: 102_000, lower: 98_000 });
        assert_eq!(band_edges(98_040, 20_000).upper, 100_000);
        assert_eq!(band_edges(98_039, 20_000), BandEdges { upper: 99_999, lower: 96_079 });
        // The largest band at the highest price stays exact.
        let top = band_edges(PRICE_LIMIT - 1, 450_000);
        assert_eq!((top.upper, top.lower), (6_227_702_577, 2_362_232_013));
    }

    #[test]
    fn band_rule_1_is_the_owners_bound_with_a_ten_percent_margin() {
        assert!(band_rule_1_holds(50, 8_600, 400), "the largest band at 50x (RISK.md 5.3)");
        assert!(!band_rule_1_holds(50, 8_601, 400));
        assert!(band_rule_1_holds(20, 22_100, 400));
        assert!(!band_rule_1_holds(20, 22_101, 400));
        assert!(band_rule_1_holds(1, 225_000, 225_000), "exactly at the bound (review L1)");
        // Every field at its type's extreme: no overflow in i128.
        assert!(!band_rule_1_holds(u16::MAX, u32::MAX, i32::MAX));
    }

    #[test]
    fn band_rule_2_puts_a_floor_on_min_price() {
        // RISK.md 5.3's table: 50x at 8,600 needs 1,004; 20x at 22,100 needs 402.
        assert!(band_rule_2_holds(1_004, 50, 8_600, 400));
        assert!(!band_rule_2_holds(1_003, 50, 8_600, 400));
        assert!(band_rule_2_holds(402, 20, 22_100, 400));
        assert!(!band_rule_2_holds(401, 20, 22_100, 400));
        // The unit tests' markets (20x, band 20,000, fee 400) need 218.
        assert!(band_rule_2_holds(218, 20, 20_000, 400));
        assert!(!band_rule_2_holds(217, 20, 20_000, 400));
        // Review L1: rule 1 lets this through, rule 2 doesn't (S < 0).
        assert!(!band_rule_2_holds(i64::MAX, 1, 225_000, 225_000));
        assert!(!band_rule_2_holds(i64::MAX, u16::MAX, u32::MAX, i32::MAX));
    }

    #[test]
    fn apply_change_matches_the_worked_examples() {
        let change = |pos, cost, realized| PositionChange { pos, cost, realized };
        // RISK.md 7.3. Long 3, cost 1,000; sell 1 at 400: removed 334, realized 66 (66.67).
        assert_eq!(apply_change(3, 1_000, -1, -400), change(2, 666, 66));
        // Short 3, cost -1,000; buy 1 at 300: removed -333, realized 33 (33.33).
        assert_eq!(apply_change(-3, -1_000, 1, 300), change(-2, -667, 33));
        // Flip: long 1,000 at 100,000; sell 2,000 at 98,000.
        assert_eq!(
            apply_change(1_000, 100_000_000, -2_000, -196_000_000),
            change(-1_000, -98_000_000, -2_000_000)
        );
        // Opening, increasing and closing exactly.
        assert_eq!(apply_change(0, 0, 5, 500), change(5, 500, 0));
        assert_eq!(apply_change(5, 500, 5, 600), change(10, 1_100, 0));
        assert_eq!(apply_change(10, 1_100, -10, -1_200), change(0, 0, 100));
        // RISK.md 10.2, the fund's netting: reducing, then flipping.
        assert_eq!(apply_change(1_000, 100_000_000, -400, -39_000_000), change(600, 60_000_000, -1_000_000));
        assert_eq!(apply_change(300, 30_000_000, -500, -49_000_000), change(-200, -19_600_000, -600_000));
    }

    #[test]
    fn apply_change_conserves_money_exactly_whatever_the_rounding() {
        // realized − (cost' − cost) = −lots_cost, over every small case of all three kinds.
        for pos in -7i64..=7 {
            for average in [1i64, 3, 97] {
                let cost = pos * average + pos.signum() * (average / 3);
                for lots in -9i64..=9 {
                    for price in [1i64, 2, 5, 101] {
                        let c = apply_change(pos, cost, lots, lots * price);
                        assert_eq!(c.pos, pos + lots);
                        assert_eq!(
                            c.realized - (c.cost - cost),
                            -lots * price,
                            "{pos} {cost} {lots} {price}"
                        );
                        assert!(c.pos != 0 || c.cost == 0, "a flat position has no cost basis");
                        assert!(c.pos.signum() * c.cost >= 0, "the cost basis has the position's sign");
                    }
                }
            }
        }
    }

    #[test]
    fn liquidation_keys_match_the_sp500_example() {
        // RISK.md 9.2: a 20x long of 100,000 lots at 75,024, fees 0, on a 20x market.
        let (cost, locked) = (7_502_400_000, 375_120_000);
        assert_eq!(liquidation_key(100_000, cost, locked, 20), Some((Side::Buy, 73_100)));
        // The maker on the other side, short 100,000 with the same collateral.
        assert_eq!(liquidation_key(-100_000, -cost, locked, 20), Some((Side::Sell, 76_854)));
        // The same long on SP500's live max leverage of 50 (section 16, C26).
        assert_eq!(liquidation_key(100_000, cost, locked, 50), Some((Side::Buy, 71_992)));
        // A flat slot and a fully collateralised 1x long have none.
        assert_eq!(liquidation_key(0, 0, 5, 20), None);
        assert_eq!(liquidation_key(1_000, 100_000_000, 100_000_000, 1), None);
    }

    #[test]
    fn a_liquidation_key_is_exactly_the_last_safe_tick_plus_one() {
        // Against the direct check `equity < MM`, over a grid of small slots: a long is
        // liquidatable at its key and not one tick above; a short at its key and not one
        // below; and a long with no key is safe at every tick.
        for max_leverage in [1u16, 2, 3, 20, 50] {
            for pos in [-9i64, -4, -1, 1, 3, 10] {
                for cost in [pos * 50, pos * 99, pos * 101] {
                    for locked in [-30i64, 0, 7, 100, 400, 1_000] {
                        let liquidatable = |x: Price| is_liquidatable(pos, cost, locked, x, max_leverage);
                        match liquidation_key(pos, cost, locked, max_leverage) {
                            Some((Side::Buy, key)) => {
                                assert!(liquidatable(key) && !liquidatable(key + 1), "{pos} {cost} {locked}");
                            }
                            Some((Side::Sell, key)) => {
                                assert!(liquidatable(key) && !liquidatable(key - 1), "{pos} {cost} {locked}");
                            }
                            None => assert!((1..2_000).all(|x| !liquidatable(x)), "{pos} {cost} {locked}"),
                        }
                    }
                }
            }
        }
    }

    /// A small deterministic pseudo-random generator (xorshift64, Marsaglia 2003), so the key
    /// tests below cover many slots without a new dependency. The same seed gives the same
    /// slots on every run.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// A number in `low..=high`. The modulo makes it very slightly uneven, which doesn't
        /// matter here.
        fn range(&mut self, low: i64, high: i64) -> i64 {
            let width = (high - low + 1) as u64;
            low + (self.next() % width) as i64
        }

        /// True one time in `n`, on average.
        fn one_in(&mut self, n: u64) -> bool {
            self.next().is_multiple_of(n)
        }

        fn pick<T: Copy>(&mut self, values: &[T]) -> T {
            values[self.next() as usize % values.len()]
        }
    }

    /// A random slot within RISK.md 2.4's bounds, for a market whose prices go up to
    /// `max_price`: `(pos, cost, locked)`. The cost basis has the position's sign and is at
    /// most `|pos| × max_price`, with some rounding left over from reduces; the collateral
    /// runs from a quarter of the notional below zero to twice the notional.
    fn random_slot(rng: &mut XorShift, max_size: i64, max_price: Price) -> (Qty, Micros, Micros) {
        let size = rng.range(1, max_size);
        let pos = if rng.one_in(2) { size } else { -size };
        let average = rng.range(1, max_price - 1);
        let rounding = rng.range(0, size - 1);
        let cost = pos * average + pos.signum() * rounding;
        let notional = size * average;
        let locked = rng.range(-notional / 4, 2 * notional);
        (pos, cost, locked)
    }

    #[test]
    fn a_liquidation_key_matches_a_brute_force_scan_of_every_tick() {
        // For random small slots, check every tick from 1 to 4,000 with the direct check: a
        // long is liquidatable exactly at the ticks at or below its key (at none if it has
        // no key), a short exactly at the ticks at or above its key. So the key is exactly
        // the boundary, and liquidatability really is one-sided, as the walk assumes.
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for _ in 0..2_000 {
            let max_leverage = rng.pick(&[1u16, 2, 3, 5, 10, 20, 50, 100]);
            let (pos, cost, locked) = random_slot(&mut rng, 40, 1_000);
            let key = liquidation_key(pos, cost, locked, max_leverage);
            for tick in 1..=4_000 {
                let expected = match key {
                    Some((Side::Buy, key)) => tick <= key,
                    Some((Side::Sell, key)) => tick >= key,
                    None => false,
                };
                assert_eq!(
                    is_liquidatable(pos, cost, locked, tick, max_leverage),
                    expected,
                    "slot ({pos}, {cost}, {locked}) at {max_leverage}x, key {key:?}, tick {tick}"
                );
            }
            // Every position has a side: a short always has a key, a long may not.
            assert!(pos > 0 || matches!(key, Some((Side::Sell, _))));
        }
    }

    #[test]
    fn a_liquidation_key_matches_the_naive_binary_search_at_full_scale() {
        // Random slots at every scale RISK.md 2.4 allows (prices up to 2^32, notionals up to
        // 2^53, any maximum leverage): the closed form's key is the boundary of the direct
        // check, and a binary search from a random safe mark and a random crossed mark, as
        // the naive `SetMark` does, finds the same tick.
        let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
        for _ in 0..100_000 {
            let max_price = rng.range(2, PRICE_LIMIT - 1);
            let bits = rng.range(0, 53);
            let max_size = (1i64 << bits).min(max_qty(max_price));
            let max_leverage = if rng.one_in(4) { rng.range(1, 65_535) as u16 } else { 50 };
            let (pos, cost, locked) = random_slot(&mut rng, max_size, max_price);
            let liquidatable = |x: Price| is_liquidatable(pos, cost, locked, x, max_leverage);
            let search =
                |safe, crossed| liquidation_key_by_search(pos, cost, locked, max_leverage, safe, crossed);
            let what = format!("slot ({pos}, {cost}, {locked}) at {max_leverage}x, max price {max_price}");
            match liquidation_key(pos, cost, locked, max_leverage) {
                Some((Side::Buy, key)) => {
                    assert!(liquidatable(key) && !liquidatable(key + 1), "{what}: long key {key}");
                    if key < max_price {
                        let (crossed, safe) = (rng.range(1, key), rng.range(key + 1, max_price));
                        assert_eq!(search(safe, crossed), key, "{what}: from {safe} and {crossed}");
                    }
                }
                Some((Side::Sell, key)) => {
                    assert!(liquidatable(key) && !liquidatable(key - 1), "{what}: short key {key}");
                    if (2..=max_price).contains(&key) {
                        let (safe, crossed) = (rng.range(1, key - 1), rng.range(key, max_price));
                        assert_eq!(search(safe, crossed), key, "{what}: from {safe} and {crossed}");
                    }
                }
                None => {
                    assert!(pos > 0, "{what}: only a long can have no key");
                    assert!(!liquidatable(1) && !liquidatable(max_price), "{what}: no key, but liquidatable");
                }
            }
        }
    }

    #[test]
    fn the_naive_search_finds_the_sp500_keys_from_either_side() {
        // RISK.md 9.2's long (key 73,100) and short (key 76,854), searched from the mark at
        // which they opened toward a mark that crosses them, and from the far side too.
        let (cost, locked) = (7_502_400_000, 375_120_000);
        assert_eq!(liquidation_key_by_search(100_000, cost, locked, 20, 75_024, 70_000), 73_100);
        assert_eq!(liquidation_key_by_search(100_000, cost, locked, 20, 73_101, 73_100), 73_100);
        assert_eq!(liquidation_key_by_search(100_000, cost, locked, 20, 200_000, 1), 73_100);
        assert_eq!(liquidation_key_by_search(-100_000, -cost, locked, 20, 75_024, 80_000), 76_854);
        assert_eq!(liquidation_key_by_search(-100_000, -cost, locked, 20, 76_853, 76_854), 76_854);
    }

    #[test]
    #[should_panic(expected = "which should be safe")]
    fn the_naive_search_refuses_a_slot_that_was_not_safe_at_the_old_mark() {
        // T1's long is liquidatable at 73,000 already: a mark that crossed it earlier.
        liquidation_key_by_search(100_000, 7_502_400_000, 375_120_000, 20, 73_000, 72_000);
    }

    #[test]
    fn uncovered_bad_debt_is_the_funds_negative_equity() {
        assert_eq!(fund_unrealized_pnl(100_000, 7_502_400_000, 73_100), -192_400_000);
        assert_eq!(fund_unrealized_pnl(-1_000, -98_000_000, 100_000), -2_000_000);
        assert_eq!(uncovered_bad_debt(1_000, -999), 0);
        assert_eq!(uncovered_bad_debt(1_000, -1_000), 0);
        assert_eq!(uncovered_bad_debt(1_000, -1_001), 1);
        assert_eq!(uncovered_bad_debt(0, 0), 0);
    }
}
