//! The engine: markets with their order books, and the risk layer above them: accounts,
//! isolated slots, the insurance fund, the pre-trade check, fills and fees, collateral
//! release, liquidation, the price-band sweep and the operator commands (Milestone 2; the
//! specification is `docs/RISK.md`).
//!
//! **Contract.** [`Engine::apply`] applies one command completely before the next one
//! starts, and writes its events to the sink in the order RISK.md section 11 fixes. It is a
//! pure function of the command stream: no clock, no randomness, no I/O, and no iteration
//! over a hash map where the order could show.
//!
//! A rejected command emits exactly one `Reject` and changes no state at all: no account
//! or slot is created, no counter moves, no collateral moves. The code makes that
//! structural: each command has a `check_*` function that takes `&self` and returns either
//! the reject reason or what the command will do, and an `accept_*` part that takes
//! `&mut self` and does it. The engine makes every check the book would make before
//! calling the book (so the book never rejects on the engine's path) and asserts, in
//! release builds too, that the book's first event is the one it expects.
//!
//! **Invariants.** RISK.md section 12: the book is not crossed, positions sum to zero,
//! money is conserved to the micro, open totals match the book, no slot is below
//! maintenance margin (any slot that gets there is liquidated before the command ends),
//! sizes are within `max_qty`, the liquidation index is exact, every resting order is
//! inside the price band, and more. [`Engine::assert_invariants`] checks them for tests.
//!
//! **Complexity** (RISK.md 14.1). A risk-checked place costs the book's own work plus a
//! market lookup by index, a few `i64` multiplications, two `i128` fee products per fill
//! (maker and taker), a tier scan of at most 8 rows, and one O(log n) re-key per touched
//! slot whose position or collateral changed. In the `Fast` mode nothing loops over orders
//! or positions, with two stated exceptions: a liquidation cancels the account's k orders
//! in the market (O(k)), and `SetMark`'s band sweep costs O(1 + orders swept). `Withdraw`
//! loops over the markets (about 88), off the hot path.
//!
//! Accounts and slots live in hash maps, and each step looks up the entry it needs by id
//! rather than being handed a reference by the step before, which keeps the steps
//! independent of each other. So the same two entries are looked up several times: a place
//! that rests with a top-up makes 12 lookups (4 of the account, 8 of its slot), each fill 3
//! more, and the pass 3 for each maker (2 more for each slot it releases). A lookup of an
//! entry that is already in cache costs about 6 to 11 ns (RISK.md 18, P2).
//!
//! **Allocation.** The book's events go to a scratch buffer, and the slots a command
//! touched to a list, both reserved up front ([`EngineOptions`]) and cleared, never shrunk;
//! the liquidation index reserves room for `slot_capacity` entries per side
//! (`liquidation_index.rs`). So a command that fits in them, on accounts and slots that
//! already exist, allocates nothing, apart from, in the modes without the index, the list
//! of slots a `SetMark` has crossed (only when there are any).
//!
//! **Page faults** (Milestone 3). Reserving memory doesn't make it resident: the operating
//! system maps each page the first time it is written, so a run's first uses of reserved
//! room (a new slot, a new order id's bucket) each take a minor page fault on the core
//! thread. [`Engine::prefault`] and [`Engine::prefault_market`] write it all once, before a
//! run, and change nothing the engine does or emits (`prefault.rs`). Nothing in the engine
//! calls them; the pipeline's core thread does, before its first command and after each
//! accepted `SetMarketParams` (`docs/PIPELINE.md` 15.4).
//!
//! **Files.**
//! - this file: the state, [`EngineOptions`], `apply`, and helpers every handler uses;
//! - `engine/orders.rs`: `PlaceOrder`, `CancelOrder`, `ModifyOrder`, and the pre-trade
//!   check (RISK.md 5, 6.1 to 6.3);
//! - `engine/operator.rs`: `Deposit`, `Withdraw`, `SetLeverage`, `SetMark`,
//!   `SetMarketParams`, `SetRiskTier` (6.4 to 6.9);
//! - `engine/ledger.rs`: the book's events applied to the ledger (fills, fees, cancels),
//!   top-ups, the post-command pass and its release, re-keying, the Mode seam, and the
//!   insurance fund's running totals (7, 8, 9.3, 10.3, 10.4, 14.2);
//! - `engine/liquidation.rs`: liquidation and the insurance fund's takeover, the `SetMark`
//!   liquidation walk (the Mode seam's fourth function) and the band sweep (5.4, 9.4, 10.1,
//!   14.2);
//! - `engine/inspect.rs`: [`EngineSnapshot`] and [`Engine::assert_invariants`], for tests
//!   (12, 15.5).

mod inspect;
mod ledger;
mod liquidation;
mod operator;
mod orders;
#[cfg(test)]
mod tests;

pub use inspect::{AccountSnapshot, EngineSnapshot, MarketSnapshot, SlotSnapshot};

use std::fmt;
use std::marker::PhantomData;

use crate::book::OrderBook;
use crate::command::Command;
use crate::event::{BalanceChanged, Event, EventSink, PositionChanged, Reject, RejectReason};
use crate::id_hash::{IdBuildHasher, IdMap};
use crate::mode::Mode;
use crate::state::{Account, Market, Slot};
use crate::types::{AccountId, MarketId, Micros, OrderId, Price, Qty};

/// The insurance fund's account id (RISK.md 3.4). No client may use it: the fund can't
/// place orders, withdraw or set leverage (`ReservedAccount`). It is capitalised by an
/// ordinary `Deposit`.
pub const FUND: AccountId = AccountId::MAX;

/// `Reject.account` for a rejected market-level command (`SetMark`, `SetMarketParams`,
/// `SetRiskTier`): an id no client can hold, which consumers read as "the operator".
const OPERATOR: AccountId = AccountId::MAX;

/// `Reject.order_id` for a rejected command that names no order (a deposit, a withdrawal,
/// a leverage change or a market-level command): 0.
const NO_ORDER: OrderId = OrderId::new(0);

/// Sizes to reserve up front. None of them changes what the engine does, only how often it
/// allocates (RISK.md 15.5).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EngineOptions {
    /// Resting orders each market's book reserves room for (`BookOptions`).
    pub order_capacity: usize,
    /// Seed for the books' and the engine's id maps (D-011). A secret in production.
    pub id_hash_seed: u64,
    /// Book events one book call can emit before the scratch buffer grows. Also the
    /// capacity of the list of touched slots, which never holds more entries than the
    /// scratch buffer (RISK.md 3.5).
    pub scratch_capacity: usize,
    /// Accounts to reserve room for.
    pub account_capacity: usize,
    /// Slots each market reserves room for.
    pub slot_capacity: usize,
}

/// Leaves the seed out, so logging the options can't leak it.
impl fmt::Debug for EngineOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineOptions")
            .field("order_capacity", &self.order_capacity)
            .field("scratch_capacity", &self.scratch_capacity)
            .field("account_capacity", &self.account_capacity)
            .field("slot_capacity", &self.slot_capacity)
            .finish_non_exhaustive()
    }
}

impl Default for EngineOptions {
    /// For tests and benchmarks only: small capacities, and hash seed 0, which everyone
    /// knows (see `BookOptions::default`).
    fn default() -> Self {
        EngineOptions {
            order_capacity: 1_024,
            id_hash_seed: 0,
            scratch_capacity: 4_096,
            account_capacity: 1_024,
            slot_capacity: 1_024,
        }
    }
}

/// The engine, generic over its order book `B` and its [`Mode`] `M` (`crate::mode`).
/// `Engine<Book, Fast>` is production; `Engine<ReferenceBook, Naive>` is the executable
/// reference. See the module docs.
#[derive(Debug)]
pub struct Engine<B, M> {
    /// Indexed by `MarketId`; grown by `SetMarketParams`.
    markets: Vec<Option<Market<B>>>,
    /// Every account that has been created (by a `Deposit` or `SetLeverage`). The fund has
    /// no entry.
    accounts: IdMap<AccountId, Account>,
    /// The insurance fund's balance, and its unrealized PnL summed over all markets, kept
    /// as a running total (RISK.md 10.3).
    fund_balance: Micros,
    fund_upnl_total: i128,
    /// The value in the last `InsuranceShortfall` (0 at start).
    last_reported_uncovered: Micros,
    /// Sum of deposits minus sum of withdrawals, for the conservation check (I3).
    net_deposits: i128,
    /// Moves by one as the first step of every accepted command (RISK.md 3.5).
    command_counter: u64,
    /// The book's events for the current book call, walked by index (RISK.md 7.1).
    scratch: Vec<Event>,
    /// The slots, in the current command's market, that the post-command pass visits: the
    /// command's own slot first, then makers in the order of their first fill (8.1).
    touched: Vec<AccountId>,
    options: EngineOptions,
    mode: PhantomData<M>,
}

impl<B: OrderBook, M: Mode> Engine<B, M> {
    /// An engine with no markets and no accounts.
    pub fn new(options: EngineOptions) -> Self {
        let hasher = IdBuildHasher::new(options.id_hash_seed);
        Engine {
            markets: Vec::new(),
            accounts: IdMap::with_capacity_and_hasher(options.account_capacity, hasher),
            fund_balance: Micros::ZERO,
            fund_upnl_total: 0,
            last_reported_uncovered: Micros::ZERO,
            net_deposits: 0,
            command_counter: 0,
            scratch: Vec::with_capacity(options.scratch_capacity),
            touched: Vec::with_capacity(options.scratch_capacity),
            options,
            mode: PhantomData,
        }
    }

    /// Writes, once, all the memory the engine and its markets have reserved (module docs,
    /// "Page faults"), so that none of it is first touched by a later command. Changes
    /// nothing the engine holds or emits. It allocates only to rebuild id maps that
    /// already hold entries (a replayed engine's, `prefault.rs`). O(the reserved memory):
    /// call it before measuring, never on a command's path.
    pub fn prefault(&mut self) {
        crate::prefault::touch_map(&mut self.accounts, |i| AccountId::new(i as u32), || Account::NEW);
        let filler =
            Event::MarkPrice(crate::event::MarkPrice { price: Price::ZERO, market: MarketId::new(0) });
        crate::prefault::touch_spare(&mut self.scratch, filler);
        crate::prefault::touch_spare(&mut self.touched, AccountId::new(0));
        for market in self.markets.iter_mut().flatten() {
            market.prefault();
        }
    }

    /// [`Engine::prefault`] for one market, if it exists: after a `SetMarketParams` has
    /// created it, or given it a new book. Does nothing for a market that doesn't exist.
    /// Like `prefault`, it rebuilds any of the market's maps that hold entries (a
    /// reconfigured market's slots), so it belongs after an accepted `SetMarketParams`,
    /// not after a rejected one on a live market.
    pub fn prefault_market(&mut self, market: MarketId) {
        if let Some(Some(market)) = self.markets.get_mut(market.index()) {
            market.prefault();
        }
    }

    /// Applies one command completely, writing its events to `events`.
    pub fn apply(&mut self, command: &Command, events: &mut impl EventSink) {
        match command {
            Command::PlaceOrder(order) => self.place_order(order, events),
            Command::CancelOrder(cancel) => self.cancel_order(cancel, events),
            Command::ModifyOrder(modify) => self.modify_order(modify, events),
            Command::Deposit(deposit) => self.deposit(deposit, events),
            Command::Withdraw(withdraw) => self.withdraw(withdraw, events),
            Command::SetLeverage(leverage) => self.set_leverage(leverage, events),
            Command::SetMark(mark) => self.set_mark(mark, events),
            Command::SetMarketParams(params) => self.set_market_params(params, events),
            Command::SetRiskTier(row) => self.set_risk_tier(row, events),
        }
    }

    // -----------------------------------------------------------------------------------
    // Helpers every handler uses.

    /// A market, if `SetMarketParams` has created it. For the checks.
    fn find_market(&self, id: MarketId) -> Option<&Market<B>> {
        self.markets.get(id.index()).and_then(Option::as_ref)
    }

    /// The market of a command that passed its checks, which include the market existing.
    fn market(&self, id: MarketId) -> &Market<B> {
        self.find_market(id).unwrap_or_else(|| panic!("market {id} was checked to exist"))
    }

    /// See [`Engine::market`].
    fn market_mut(&mut self, id: MarketId) -> &mut Market<B> {
        let market = self.markets.get_mut(id.index()).and_then(Option::as_mut);
        market.unwrap_or_else(|| panic!("market {id} was checked to exist"))
    }

    /// The account's free balance: 0 for an account the engine hasn't seen.
    fn free_balance(&self, account: AccountId) -> Micros {
        self.accounts.get(&account).map_or(Micros::ZERO, |state| state.free)
    }

    /// The lowest sequence number the account may use: 0 for an account the engine hasn't
    /// seen.
    fn next_seq(&self, account: AccountId) -> u64 {
        self.accounts.get(&account).map_or(0, |state| state.next_seq)
    }

    /// An account that exists. Every caller acts on an account that has a slot holding
    /// collateral or orders, and that collateral came from its free balance.
    fn account_mut(&mut self, account: AccountId) -> &mut Account {
        self.accounts.get_mut(&account).unwrap_or_else(|| panic!("account {account} was expected to exist"))
    }

    /// The first step of every accepted command (RISK.md 3.5): the counter moves on, and the
    /// list of touched slots starts empty.
    fn start_command(&mut self) {
        self.command_counter += 1;
        self.touched.clear();
    }

    /// Adds the account's slot in this market to the touched list, once per command
    /// (RISK.md 8.1). The check is O(1): the slot remembers the last command that added it.
    fn touch(&mut self, market_id: MarketId, account: AccountId) {
        let counter = self.command_counter;
        let slot = self.market_mut(market_id).slot_mut(account);
        if slot.touched_in != counter {
            slot.touched_in = counter;
            self.touched.push(account);
        }
    }
}

/// Emits the single event of a rejected command.
fn reject(events: &mut impl EventSink, order_id: OrderId, account: AccountId, reason: RejectReason) {
    events.emit(Event::Reject(Reject { order_id, account, reason }));
}

/// A slot's state after a change, as an event.
fn position_event(market: MarketId, account: AccountId, slot: &Slot) -> Event {
    Event::PositionChanged(PositionChanged {
        position: slot.pos,
        cost_basis: slot.cost,
        locked: slot.locked,
        account,
        market,
    })
}

/// The insurance fund's position in one market after a change, as an event. The fund keeps
/// no collateral in a slot (its money is its balance), so `locked` is always 0.
fn fund_position_event(market: MarketId, fund_pos: Qty, fund_cost: Micros) -> Event {
    Event::PositionChanged(PositionChanged {
        position: fund_pos,
        cost_basis: fund_cost,
        locked: Micros::ZERO,
        account: FUND,
        market,
    })
}

/// A free balance (or the fund's balance) after a change, as an event.
fn balance_event(account: AccountId, free: Micros) -> Event {
    Event::BalanceChanged(BalanceChanged { free, account })
}
