//! The Mode seam: two compile-time switches that pick, inside one engine, between the fast
//! structures and their naive reference versions (`docs/RISK.md` 14.2, D-017).
//!
//! **Contract.** The engine is generic over a [`Mode`], and reads its two constants in
//! exactly four functions (the first three in `engine/ledger.rs`, next to each other, the
//! fourth in `engine/liquidation.rs`):
//! - `open_totals`: a slot's open buys and sells, from its running totals, or recomputed
//!   by walking the account's resting orders in the book (`OrderBook::open_quantities`);
//! - `add_open`: updates the running totals, or does nothing;
//! - `rekey`: brings the slot's entry in the liquidation index up to date, or does nothing;
//! - the `SetMark` liquidation step: walks the index, or scans every slot in the market.
//!
//! (The test-only `assert_invariants` reads them too, to skip the checks of structures a
//! mode doesn't keep.)
//!
//! Everything else, every money formula included, is shared by all modes. So when a
//! property test finds that every mode emits the same events, it has shown that the
//! running totals equal the book's and that the index picks the same slots in the same
//! order as a scan (RISK.md 14.3 says what that does and doesn't prove).
//!
//! **Cost.** The switches are constants, so the compiler removes the branch not taken:
//! [`Fast`] carries no trace of the naive paths.

/// See the module docs.
pub trait Mode {
    /// true: `open_buys`/`open_sells` are running totals kept on each slot, O(1) to read.
    /// false: they are recomputed from the book, O(k) for the account's k resting orders.
    const RUNNING_TOTALS: bool;
    /// true: `SetMark` walks the liquidation index (RISK.md 9).
    /// false: `SetMark` scans every slot in the market.
    const LIQUIDATION_INDEX: bool;
}

/// Production: running totals and the liquidation index.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fast;

/// The reference: neither. `Engine<ReferenceBook, Naive>` is the engine's executable
/// specification, as `ReferenceBook` is the book's.
#[derive(Clone, Copy, Debug, Default)]
pub struct Naive;

/// Ablation A (RISK.md 14.4): the liquidation index, but no running totals, so every
/// pre-trade check walks the account's orders.
#[derive(Clone, Copy, Debug, Default)]
pub struct NaiveTotals;

/// Ablation B: running totals, but no liquidation index, so every `SetMark` scans the
/// market's slots.
#[derive(Clone, Copy, Debug, Default)]
pub struct NaiveLiquidation;

impl Mode for Fast {
    const RUNNING_TOTALS: bool = true;
    const LIQUIDATION_INDEX: bool = true;
}

impl Mode for Naive {
    const RUNNING_TOTALS: bool = false;
    const LIQUIDATION_INDEX: bool = false;
}

impl Mode for NaiveTotals {
    const RUNNING_TOTALS: bool = false;
    const LIQUIDATION_INDEX: bool = true;
}

impl Mode for NaiveLiquidation {
    const RUNNING_TOTALS: bool = true;
    const LIQUIDATION_INDEX: bool = false;
}
