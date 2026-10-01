//! Commands: the only input to the engine.
//!
//! **Contract.** The engine is a pure function of the ordered stream of commands (INFO.md
//! section 4, "Determinism rules"). The sequencer stamps each command with a sequence
//! number and timestamp and journals it before the core sees it, so replaying the journal
//! replays the engine exactly.
//!
//! **Layout.** Every command is a small fixed-size `#[repr(C)]` struct, and [`Command`] is
//! a `#[repr(C, u8)]` enum of them: a one-byte tag, then the largest variant. Fields are
//! ordered largest first to keep padding small. A whole command is at most 40 bytes
//! (checked at compile time below), so rings between threads move plain fixed-size records
//! with no heap allocation. `repr(C)` is for a predictable, checkable size and field order;
//! it is not a wire format. The records still contain padding bytes, so the journal
//! (Milestone 3) encodes each field explicitly instead of copying memory. See
//! `docs/DECISIONS.md` D-005.
//!
//! **Who sends what.**
//! - Signed by clients and checked by a gateway: [`PlaceOrder`], [`CancelOrder`],
//!   [`ModifyOrder`]. The gateway also checks that the signer owns the order: the owning
//!   account is encoded in the `OrderId` itself (see `types::account_of`), so these
//!   commands carry no separate account field.
//! - Operator commands through the sequencer's privileged queue: everything else.

use crate::types::{AccountId, MarketId, Micros, OrderId, Price, Qty, Side, TimeInForce};

/// A new limit order.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaceOrder {
    /// Chosen by the client; also identifies the owning account.
    pub order_id: OrderId,
    pub price: Price,
    pub qty: Qty,
    pub market: MarketId,
    pub side: Side,
    pub tif: TimeInForce,
    /// Reject instead of trading if the order would match on arrival.
    pub post_only: bool,
}

/// Remove a resting order.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CancelOrder {
    pub order_id: OrderId,
    pub market: MarketId,
}

/// Change a resting order's price and/or size.
///
/// `new_size` is the order's new *total* size, including what has already filled (the FIX
/// convention), not the new remaining quantity. So a modify signed before a fill arrives
/// can't grow the client's exposure past the size they asked for: what is left to fill
/// becomes `new_size - filled`, and if that is zero or less the order is removed. Whether
/// the order keeps its place in the queue is defined by the order book (INFO.md section 4,
/// "Modify semantics"; `docs/DECISIONS.md` D-008).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModifyOrder {
    pub order_id: OrderId,
    pub new_price: Price,
    pub new_size: Qty,
    pub market: MarketId,
}

/// Add collateral to an account's free balance.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deposit {
    pub amount: Micros,
    pub account: AccountId,
}

/// Remove collateral from an account's free balance (subject to the withdrawal rule).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Withdraw {
    pub amount: Micros,
    pub account: AccountId,
}

/// Choose an account's leverage for one market: 1 to the market's maximum; anything else is
/// rejected. Until it is set, a slot's leverage is 1 (`docs/RISK.md` 6.6).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetLeverage {
    pub account: AccountId,
    pub market: MarketId,
    pub leverage: u16,
}

/// The mark price for one market. In v1 this is a plain input; computing it is "Later".
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetMark {
    pub price: Price,
    pub market: MarketId,
}

/// A market's fixed parameters. Only accepted while the market has no resting orders and
/// no positions, because changing them on a live market would invalidate the book and
/// every indexed liquidation price. It clears the market's tier table and mark, so the
/// market then needs a tier table (`SetRiskTier`) and a `SetMark` before it takes orders
/// (`docs/RISK.md` 3.3, 6.8).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetMarketParams {
    /// Lowest and highest price the book accepts, in ticks.
    pub min_price: Price,
    pub max_price: Price,
    /// Fees in parts per million of notional. Negative means a rebate.
    pub maker_fee_ppm: i32,
    pub taker_fee_ppm: i32,
    /// How far through the mark a limit order may be priced, in parts per million.
    pub price_band_ppm: u32,
    pub market: MarketId,
    pub max_leverage: u16,
}

/// One row of a market's leverage tiers: a notional at or above `lower_bound` may use at
/// most `max_leverage`. Sent as one command per row so that every command stays small
/// (Polymarket markets have up to 8 tiers). A table is `count` rows, sent with indexes 0 to
/// `count - 1`; the rows are staged and take effect together when the last one is accepted
/// (`docs/RISK.md` 6.9).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetRiskTier {
    pub lower_bound: Micros,
    pub market: MarketId,
    pub max_leverage: u16,
    /// Row position, 0 = lowest bound.
    pub index: u8,
    /// Rows in the whole table, 1 to 8: the same in every row of one table.
    pub count: u8,
}

/// Every command the engine accepts.
#[repr(C, u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    PlaceOrder(PlaceOrder),
    CancelOrder(CancelOrder),
    ModifyOrder(ModifyOrder),
    Deposit(Deposit),
    Withdraw(Withdraw),
    SetLeverage(SetLeverage),
    SetMark(SetMark),
    SetMarketParams(SetMarketParams),
    SetRiskTier(SetRiskTier),
}

// Commands cross thread boundaries by the hundred thousand per second; keep them small.
// The two largest structs, and the enum: a one-byte tag, padded to 8, before the largest.
const _: () = assert!(size_of::<SetMarketParams>() == 32);
const _: () = assert!(size_of::<SetRiskTier>() == 16);
const _: () = assert!(size_of::<Command>() == 40);
