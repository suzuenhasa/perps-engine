//! Events: the only output of the engine.
//!
//! **Contract.** For a given command stream the engine emits exactly one event stream.
//! The replay test (Milestone 3) relies on this: replaying the journal must reproduce the
//! same events, in the same order.
//!
//! **Layout.** Same scheme as `command.rs`: one `#[repr(C)]` struct per event and a
//! `#[repr(C, u8)]` enum over them. The largest event, [`Fill`], is 56 bytes, so an
//! [`Event`] is 64 bytes (checked at compile time below): one cache line when stored
//! 64-byte aligned, which the Milestone 3 ring's slot type will enforce. Fills carry only
//! order ids, not account ids: the owner is encoded in the order id, and dropping the two
//! account fields is what keeps a fill within 64 bytes. See D-005.
//!
//! Fields go largest first, and a field added later goes last (`docs/RISK.md` 15.1): with
//! `taker_side` first, `Fill` would be 64 bytes and `Event` 72. So each struct whose size
//! matters has its own compile-time size check, and a reordered field fails right there.
//!
//! **Order of events for one book call** (the reference book defines this; the real book
//! must match it exactly):
//! 1. `Ack` if the order is accepted, or a single `Reject` and nothing else.
//! 2. Then, in matching order, one `Fill` per match, with a `Cancelled { SelfTrade }` for
//!    each of the account's own resting orders removed along the way.
//! 3. Finally `Cancelled { IocRemainder }` if an IOC order has quantity left over.
//!
//! The engine adds the ledger's events around the book's (top-ups, positions, releases,
//! liquidations), in the order `docs/RISK.md` section 11 fixes for each command.

use crate::command::{SetMarketParams, SetRiskTier};
use crate::types::{AccountId, MarketId, Micros, OrderId, Price, Qty, Side};

/// Why a command was refused.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RejectReason {
    /// Price outside the market's accepted range.
    InvalidPrice,
    /// Quantity zero or negative.
    InvalidQty,
    /// From the engine: the order's sequence number is not above the account's last
    /// accepted one, in any market (`docs/RISK.md` 3.1). From a book on its own: an order
    /// with this id is already resting in this book (`docs/DECISIONS.md` D-008).
    Duplicate,
    /// No resting order with this id (never existed, already filled, or cancelled).
    UnknownOrder,
    /// A post-only order would have traded on arrival.
    PostOnlyWouldCross,
    /// No such market.
    UnknownMarket,
    /// The market has no mark price yet, so margin can't be computed.
    NoMark,
    /// The limit price is further through the mark than the market's price band.
    PriceBand,
    /// The position is in margin call and the order isn't strictly reducing.
    MarginCall,
    /// Not enough collateral for the initial margin.
    InsufficientMargin,
    /// The free balance is below the amount of a withdrawal.
    InsufficientBalance,
    /// Market parameters can only change while the market is empty.
    MarketNotEmpty,
    /// Leverage of zero, or above the market's maximum.
    InvalidLeverage,
    // The reasons below came with the risk layer (Milestone 2). New reasons go last, so
    // that existing ones keep their numbers.
    /// The insurance fund's id (`AccountId::MAX`) was used for an order, a withdrawal or a
    /// leverage change. The fund can't trade in v1 (`docs/RISK.md` 3.4).
    ReservedAccount,
    /// The order would take the slot's worst-case size above the market's `max_qty`, the
    /// limit that rules out overflow (`docs/RISK.md` 2.4).
    SizeLimit,
    /// A deposit or withdrawal amount below 1, or a deposit that would overflow the
    /// balance.
    InvalidAmount,
    /// `SetMarketParams` or `SetRiskTier` failed validation (`docs/RISK.md` 6.8, 6.9).
    InvalidParams,
    /// The withdrawal would leave free plus locked collateral below 10% of the account's
    /// open notional at mark (`docs/RISK.md` 6.5).
    WithdrawalReserve,
    /// `SetMark` for a market that has no committed tier table yet (`docs/RISK.md` 3.3).
    NoRiskTiers,
}

/// Why an order left the book without filling completely.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CancelReason {
    /// The owner asked for it.
    UserRequested,
    /// The unfilled part of an IOC order.
    IocRemainder,
    /// Self-trade prevention: an incoming order from the same account would have matched it.
    SelfTrade,
    /// The owner's position in this market was liquidated.
    Liquidation,
    /// A modify set the order's total size at or below what had already filled, so
    /// nothing was left to fill.
    SizeBelowFilled,
    /// A mark move left the order outside the market's price band (`docs/RISK.md` 5.4).
    PriceBand,
}

/// The order was accepted.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ack {
    pub order_id: OrderId,
}

/// A command was refused. `order_id` is 0 for commands that don't refer to an order.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reject {
    pub order_id: OrderId,
    pub account: AccountId,
    pub reason: RejectReason,
}

/// A trade between a resting (maker) order and an incoming (taker) order.
///
/// The price is always the maker's price. Fees are filled in by the ledger; the order
/// book on its own reports them as zero. `taker_side` is the incoming order's side (the
/// maker traded the other one), so the ledger and consumers know who bought from the event
/// alone.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    pub maker_order: OrderId,
    pub taker_order: OrderId,
    pub price: Price,
    pub qty: Qty,
    pub maker_fee: Micros,
    pub taker_fee: Micros,
    pub market: MarketId,
    pub taker_side: Side,
}

/// An order left the book with `remaining` lots unfilled. `side` is the order's side, so
/// the ledger can take `remaining` off the right open total from the event alone.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cancelled {
    pub order_id: OrderId,
    pub remaining: Qty,
    pub market: MarketId,
    pub reason: CancelReason,
    pub side: Side,
}

/// A modify was accepted. `qty` is the new remaining quantity: the new total size minus
/// what had already filled.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Modified {
    pub order_id: OrderId,
    pub price: Price,
    pub qty: Qty,
    pub market: MarketId,
}

/// An account's slot in one market changed. It carries the slot's state after the change,
/// so the event stream alone rebuilds every position and every slot's collateral (M3
/// replay, private feeds).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionChanged {
    /// Signed: positive is long, negative is short.
    pub position: Qty,
    /// Signed cost basis in micros (see INFO.md section 4, "Types").
    pub cost_basis: Micros,
    /// Collateral held in the slot (`docs/RISK.md` 3.2); always 0 for the insurance fund.
    pub locked: Micros,
    pub account: AccountId,
    pub market: MarketId,
}

/// An account's free balance changed (for the insurance fund, its balance).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BalanceChanged {
    pub free: Micros,
    pub account: AccountId,
}

/// A new mark price was applied.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkPrice {
    pub price: Price,
    pub market: MarketId,
}

/// A position was liquidated (taken over by the insurance fund).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Liquidation {
    pub position: Qty,
    pub account: AccountId,
    pub market: MarketId,
}

/// What the insurance fund took on in a liquidation.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InsuranceAbsorb {
    pub position: Qty,
    pub cost_basis: Micros,
    pub collateral: Micros,
    pub market: MarketId,
}

/// The insurance fund's uncovered bad debt, `max(0, -fund equity)`, changed.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InsuranceShortfall {
    pub uncovered: Micros,
}

/// An account's chosen leverage for one market was set (echoes an accepted `SetLeverage`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeverageSet {
    pub account: AccountId,
    pub market: MarketId,
    pub leverage: u16,
}

/// Every event the engine emits.
#[repr(C, u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Ack(Ack),
    Reject(Reject),
    Fill(Fill),
    Cancelled(Cancelled),
    Modified(Modified),
    PositionChanged(PositionChanged),
    BalanceChanged(BalanceChanged),
    MarkPrice(MarkPrice),
    Liquidation(Liquidation),
    InsuranceAbsorb(InsuranceAbsorb),
    InsuranceShortfall(InsuranceShortfall),
    LeverageSet(LeverageSet),
    /// Echoes an accepted `SetMarketParams`.
    MarketParamsSet(SetMarketParams),
    /// Echoes an accepted `SetRiskTier` row; the row with `index == count - 1` committed the
    /// table.
    RiskTierSet(SetRiskTier),
}

// The structs that changed or came with the risk layer, each checked on its own (module
// docs, "Layout"). The two echo events are the commands' structs, checked in `command.rs`.
const _: () = assert!(size_of::<Fill>() == 56);
const _: () = assert!(size_of::<Cancelled>() == 24);
const _: () = assert!(size_of::<PositionChanged>() == 32);
const _: () = assert!(size_of::<LeverageSet>() == 8);
// 64 bytes per event: one cache line when stored 64-byte aligned (see D-005). `Fill` is
// still the largest, so this is a one-byte tag, padded to 8, before it.
const _: () = assert!(size_of::<Event>() == 64);

/// Where the engine writes its events.
///
/// The order book and the rest of the engine emit through this trait rather than
/// returning collections, so the hot path never allocates: in production the sink writes
/// straight into the output ring, and in tests it is simply a `Vec<Event>`.
pub trait EventSink {
    fn emit(&mut self, event: Event);
}

impl EventSink for Vec<Event> {
    fn emit(&mut self, event: Event) {
        self.push(event);
    }
}
