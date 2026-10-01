//! The engine's state: accounts, their slots in each market, and markets (`docs/RISK.md`
//! section 3).
//!
//! **Contract.** Plain data, and helpers that change one market's own state: creating a
//! slot, counting positions and staging tier rows, and three that move money inside the
//! market. One side of a fill changes the slot's position and cost basis and puts the
//! realized PnL minus the fee into its `locked`; emptying a liquidated slot takes all of its
//! collateral out, and returns it; netting an absorbed position into the insurance fund's
//! returns the PnL that realizes. The engine (`engine.rs`) moves money between free
//! balances, slots and the fund's balance, adds up the fees, and emits every event.
//!
//! **Invariants** (RISK.md section 12), at every command boundary:
//! - a slot's `open_buys` and `open_sells` equal the remaining quantity of its account's
//!   resting orders on each side, in the modes that keep them (I4);
//! - a flat slot with no resting orders holds no collateral (I7);
//! - `nonzero_positions` counts the slots with a position, the fund's included;
//! - `accounts` lists every slot's account once, in the order the slots were created.

use crate::book::OrderBook;
use crate::command::{SetMarketParams, SetRiskTier};
use crate::id_hash::{IdBuildHasher, IdMap};
use crate::liquidation_index::LiquidationIndex;
use crate::money::{self, Holding, MAX_TIERS, SlotMoney, Tier};
use crate::types::{AccountId, Micros, Price, Qty, Side};

/// One account's engine-wide state (RISK.md 3.1). The insurance fund has none: its balance
/// is the engine's `fund_balance`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Account {
    /// The free balance, in micros. Never negative.
    pub free: Micros,
    /// The lowest order sequence number the account may still use: one above its last
    /// accepted order's. A `u64`, so that it still fits after sequence `u32::MAX`.
    pub next_seq: u64,
}

impl Account {
    /// A new account: no money, and every sequence number still available.
    pub const NEW: Account = Account { free: Micros::ZERO, next_seq: 0 };
}

/// One account's isolated position in one market (RISK.md 3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    /// Signed position in lots: positive is long.
    pub pos: Qty,
    /// Signed cost basis: the sum of `signed qty × price` of what is held. A long's is at
    /// least 0, a short's at most 0, a flat slot's 0.
    pub cost: Micros,
    /// Collateral held for this position and its orders: top-ups, realized PnL and fees.
    /// Signed: realized losses can take it below zero while the rest of the position
    /// shows a profit that keeps equity at or above maintenance margin.
    pub locked: Micros,
    /// Remaining quantity of the account's resting buys (sells) in this market. Kept only
    /// in modes with running totals; always 0 in the others (read them through the engine's
    /// `open_totals`).
    pub open_buys: Qty,
    pub open_sells: Qty,
    /// The chosen leverage: 1 until `SetLeverage`.
    pub leverage: u16,
    // Bookkeeping, not economic state (left out of the engine's snapshot):
    /// What the liquidation index holds for this slot.
    pub indexed_key: Option<(Side, Price)>,
    /// `pos`, `cost` or `locked` changed since the slot was last re-keyed.
    pub key_dirty: bool,
    /// The command that last added this slot to the engine's `touched` list. A new slot has
    /// 0, which no accepted command uses (RISK.md 3.5).
    pub touched_in: u64,
}

impl Slot {
    /// A new slot: flat, no collateral, no orders, leverage 1.
    pub const NEW: Slot = Slot {
        pos: Qty::ZERO,
        cost: Micros::ZERO,
        locked: Micros::ZERO,
        open_buys: Qty::ZERO,
        open_sells: Qty::ZERO,
        leverage: 1,
        indexed_key: None,
        key_dirty: false,
        touched_in: 0,
    };

    /// The slot's position, cost basis and collateral, for the formulas that read them
    /// together (`money::SlotMoney`).
    pub fn money(&self) -> SlotMoney {
        SlotMoney { pos: self.pos, cost: self.cost, locked: self.locked }
    }

    /// Equity at `mark`: collateral plus unrealized PnL.
    pub fn equity(&self, mark: Price) -> Micros {
        self.money().equity(mark)
    }
}

/// What a liquidation takes out of a slot and hands to the insurance fund (RISK.md 10.1): the
/// position with its cost basis, and all of the slot's collateral. Economically a takeover at
/// the bankruptcy price: the fund gains (or, if negative, loses) the slot's equity
/// `locked + pos × mark − cost` at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Absorbed {
    pub pos: Qty,
    pub cost: Micros,
    pub locked: Micros,
}

/// One market (RISK.md 3.3). It opens in three steps: `SetMarketParams`, then a tier table
/// (`SetRiskTier`), then `SetMark`. Orders need a mark and `SetMark` needs a tier table, so
/// no order is ever checked against a missing or half-written table.
#[derive(Debug)]
pub struct Market<B> {
    pub params: SetMarketParams,
    /// `floor(2^53 / max_price)`: the largest worst-case size of any slot (RISK.md 2.4).
    pub max_qty: Qty,
    /// The live tier table: its first `tier_count` rows. 0 rows until one is committed.
    pub tiers: [Tier; MAX_TIERS],
    pub tier_count: u8,
    /// A tier table being sent row by row: `staged_rows` of its `staged_count` rows have
    /// arrived. Invisible until the last one commits it.
    pub staged: [Tier; MAX_TIERS],
    pub staged_rows: u8,
    pub staged_count: u8,
    /// `None` until the first `SetMark`, and again after `SetMarketParams`.
    pub mark: Option<Price>,
    /// The price band's edges at the current mark, computed once per `SetMark` (5.3).
    pub upper: Price,
    pub lower: Price,
    pub book: B,
    pub slots: IdMap<AccountId, Slot>,
    /// Every account with a slot here, in the order the slots were created. Kept in every
    /// mode (so all modes' snapshots match); only the naive `SetMark` reads it.
    pub accounts: Vec<AccountId>,
    /// The insurance fund's netted position here, and its unrealized PnL at the current
    /// mark (`fund_pos × mark − fund_cost`).
    pub fund_pos: Qty,
    pub fund_cost: Micros,
    pub fund_upnl: i128,
    /// Net fees (fees minus rebates) from this market's fills. Never falls, because
    /// `maker_fee_ppm + taker_fee_ppm >= 0` (RISK.md 6.8).
    pub fees_collected: Micros,
    /// Slots here with a position, the fund's included. With both sides of the book
    /// empty, 0 means the market can be reconfigured.
    pub nonzero_positions: u32,
    /// Empty in the modes without the index (which reserve its room all the same).
    pub index: LiquidationIndex,
}

impl<B: OrderBook> Market<B> {
    /// A new market with no tier table, no mark and no slots.
    pub fn new(params: SetMarketParams, book: B, slot_capacity: usize, hasher: IdBuildHasher) -> Self {
        Market {
            params,
            max_qty: money::max_qty(params.max_price),
            tiers: [Tier::default(); MAX_TIERS],
            tier_count: 0,
            staged: [Tier::default(); MAX_TIERS],
            staged_rows: 0,
            staged_count: 0,
            mark: None,
            upper: Price::ZERO,
            lower: Price::ZERO,
            book,
            slots: IdMap::with_capacity_and_hasher(slot_capacity, hasher),
            accounts: Vec::with_capacity(slot_capacity),
            fund_pos: Qty::ZERO,
            fund_cost: Micros::ZERO,
            fund_upnl: 0,
            fees_collected: Micros::ZERO,
            nonzero_positions: 0,
            index: LiquidationIndex::with_capacity(slot_capacity, hasher),
        }
    }

    /// Makes the memory this market reserved resident, changing nothing it holds
    /// (`Engine::prefault`): the slots map and the list of accounts, the liquidation index
    /// and the book.
    pub fn prefault(&mut self) {
        crate::prefault::touch_map(&mut self.slots, |i| AccountId::new(i as u32), || Slot::NEW);
        crate::prefault::touch_spare(&mut self.accounts, AccountId::new(0));
        self.index.prefault();
        self.book.prefault();
    }

    /// New parameters for an empty market (RISK.md 6.8): a new empty book for the new price
    /// range, and no tier table and no mark, so the market must open again. The slots (flat,
    /// with their chosen leverage) and `fees_collected` stay.
    pub fn reconfigure(&mut self, params: SetMarketParams, book: B) {
        self.params = params;
        self.max_qty = money::max_qty(params.max_price);
        self.book = book;
        self.tier_count = 0;
        self.staged_rows = 0;
        self.mark = None;
    }

    /// The live tier table.
    pub fn live_tiers(&self) -> &[Tier] {
        &self.tiers[..usize::from(self.tier_count)]
    }

    pub fn price_in_range(&self, price: Price) -> bool {
        (self.params.min_price..=self.params.max_price).contains(&price)
    }

    /// True if nothing rests on the book and nobody, the fund included, holds a position:
    /// the condition for `SetMarketParams` (RISK.md 6.8). O(1).
    pub fn has_no_orders_or_positions(&self) -> bool {
        self.book.best_bid().is_none() && self.book.best_ask().is_none() && self.nonzero_positions == 0
    }

    /// The account's slot, if it has one here. For the checks.
    pub fn find_slot(&self, account: AccountId) -> Option<&Slot> {
        self.slots.get(&account)
    }

    /// The slot of an account that has one: every caller acts on an account whose order,
    /// fill or collateral is in this market, and its slot was created with the first of
    /// those.
    pub fn slot(&self, account: AccountId) -> &Slot {
        self.slots.get(&account).unwrap_or_else(|| panic!("account {account} has no slot here"))
    }

    /// See [`Market::slot`].
    pub fn slot_mut(&mut self, account: AccountId) -> &mut Slot {
        self.slots.get_mut(&account).unwrap_or_else(|| panic!("account {account} has no slot here"))
    }

    /// The account's slot, created if it has none (RISK.md 3.2). The only place a slot is
    /// created, so it is also where `accounts` grows. Slots are never deleted: the chosen
    /// leverage survives a liquidation.
    pub fn slot_or_create(&mut self, account: AccountId) -> &mut Slot {
        let accounts = &mut self.accounts;
        self.slots.entry(account).or_insert_with(|| {
            accounts.push(account);
            Slot::NEW
        })
    }

    /// Applies one side of a fill to `account`'s slot (RISK.md 7.3): the position and cost
    /// basis change by `lots` (signed) at `price`, and the realized PnL minus the fee goes
    /// to `locked`, so a trader with no free balance can still close. Keeps
    /// `nonzero_positions` in step. Returns the slot, so that the caller can finish its side
    /// of the fill without looking it up again.
    pub fn trade(&mut self, account: AccountId, lots: Qty, price: Price, fee: Micros) -> &mut Slot {
        // The field, not `slot_mut()`, so that the count of positions can change alongside.
        let slot =
            self.slots.get_mut(&account).unwrap_or_else(|| panic!("account {account} has no slot here"));
        let was_open = slot.pos != Qty::ZERO;
        let held = Holding { pos: slot.pos, cost: slot.cost };
        let change = held.apply_change(Holding { pos: lots, cost: lots * price });
        slot.pos = change.pos;
        slot.cost = change.cost;
        slot.locked += change.realized - fee;
        slot.key_dirty = true;
        count_position_change(&mut self.nonzero_positions, was_open, slot.pos != Qty::ZERO);
        slot
    }

    /// Empties a liquidated slot (RISK.md 10.1, step 3): its position, cost basis and
    /// collateral go to zero, and are returned for the insurance fund to take over. The slot
    /// itself stays, with its chosen leverage. Its orders must be gone already (step 1), so
    /// its open totals are 0 too. Keeps `nonzero_positions` in step, and marks the key stale:
    /// a flat slot has none, so the next re-key takes it out of the index.
    pub fn empty_slot(&mut self, account: AccountId) -> Absorbed {
        let slot = self.slot_mut(account);
        let taken = Absorbed { pos: slot.pos, cost: slot.cost, locked: slot.locked };
        (slot.pos, slot.cost, slot.locked) = (Qty::ZERO, Micros::ZERO, Micros::ZERO);
        slot.key_dirty = true;
        count_position_change(&mut self.nonzero_positions, taken.pos != Qty::ZERO, false);
        taken
    }

    /// Nets an absorbed position into the insurance fund's position here (RISK.md 10.1,
    /// step 4), with the same `apply_change` as a fill: the absorbed cost basis plays the
    /// part of the fill's `qty × price`. Returns the PnL that realizes, which goes to the
    /// fund's balance. Keeps `nonzero_positions` in step (the fund's position counts).
    pub fn net_into_fund(&mut self, pos: Qty, cost: Micros) -> Micros {
        let was_open = self.fund_pos != Qty::ZERO;
        let fund = Holding { pos: self.fund_pos, cost: self.fund_cost };
        let change = fund.apply_change(Holding { pos, cost });
        self.fund_pos = change.pos;
        self.fund_cost = change.cost;
        count_position_change(&mut self.nonzero_positions, was_open, change.pos != Qty::ZERO);
        change.realized
    }

    /// Stages one accepted tier row, and commits the table if it was the last row
    /// (RISK.md 6.9). Row 0 starts a new table, dropping any rows staged before it.
    pub fn stage_tier_row(&mut self, row: &SetRiskTier) {
        let index = usize::from(row.index);
        self.staged[index] = Tier { lower_bound: row.lower_bound, max_leverage: row.max_leverage };
        if index == 0 {
            self.staged_count = row.count;
        }
        self.staged_rows = row.index + 1;
        if self.staged_rows == self.staged_count {
            self.tiers = self.staged;
            self.tier_count = self.staged_count;
            self.staged_rows = 0;
        }
    }
}

/// Updates a market's count of positions (`Market::nonzero_positions`) when one, a slot's or
/// the fund's, opens or closes. It takes the count, not the market, so that it can run while
/// one of the market's slots is borrowed.
pub fn count_position_change(nonzero_positions: &mut u32, was_open: bool, is_open: bool) {
    match (was_open, is_open) {
        (false, true) => *nonzero_positions += 1,
        (true, false) => *nonzero_positions -= 1,
        _ => {}
    }
}
