//! Looking inside the engine, for tests: [`EngineSnapshot`] and
//! [`Engine::assert_invariants`] (`docs/RISK.md` 12 and 15.5).
//!
//! **Contract.** The snapshot holds every economic value in a fixed order (sorted by id,
//! never in hash-map order), so two engines can be compared field by field, and a snapshot
//! before a rejected command must equal the one after it (I15). It leaves out what differs
//! between modes or is bookkeeping: the index and each slot's `indexed_key`, `key_dirty`
//! and `touched_in`, the command counter, and the scratch and touched lists. Open totals are
//! read through the Mode, never from the raw fields, which the naive modes don't keep.
//!
//! **Complexity.** Both walk everything and allocate: tests only, never on the hot path.

use std::cmp::Ordering;

use crate::book::{BookSnapshot, OrderBook};
use crate::command::SetMarketParams;
use crate::mode::Mode;
use crate::money::{SlotMoney, Tier, fund_unrealized_pnl, uncovered_bad_debt, worst_case_size};
use crate::state::{Market, Slot};
use crate::types::{AccountId, Micros, Price, Qty, Side, account_of, sequence_of};

use super::Engine;

/// Every economic value in the engine, in a fixed order. See the module docs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineSnapshot {
    /// Sorted by account. An account's existence is part of the snapshot, so a rejected
    /// command that created one would show.
    pub accounts: Vec<AccountSnapshot>,
    /// Sorted by market.
    pub markets: Vec<MarketSnapshot>,
    pub fund_balance: Micros,
    pub fund_upnl_total: i128,
    pub last_reported_uncovered: Micros,
    pub net_deposits: i128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountSnapshot {
    pub account: AccountId,
    pub free: Micros,
    pub next_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketSnapshot {
    pub params: SetMarketParams,
    pub max_qty: Qty,
    /// The live tier table.
    pub tiers: Vec<Tier>,
    /// The rows of a table being staged.
    pub staged: Vec<Tier>,
    pub staged_rows: u8,
    pub staged_count: u8,
    pub mark: Option<Price>,
    pub upper: Price,
    pub lower: Price,
    pub fund_pos: Qty,
    pub fund_cost: Micros,
    pub fund_upnl: i128,
    pub fees_collected: Micros,
    pub nonzero_positions: u32,
    /// In creation order.
    pub accounts: Vec<AccountId>,
    pub book: BookSnapshot,
    /// Sorted by account.
    pub slots: Vec<SlotSnapshot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotSnapshot {
    pub account: AccountId,
    pub pos: Qty,
    pub cost: Micros,
    pub locked: Micros,
    pub leverage: u16,
    pub open_buys: Qty,
    pub open_sells: Qty,
}

impl SlotSnapshot {
    /// The slot's position, cost basis and collateral, for the money formulas, as
    /// `Slot::money()` gives them for a live slot.
    pub fn money(&self) -> SlotMoney {
        SlotMoney { pos: self.pos, cost: self.cost, locked: self.locked }
    }
}

impl<B: OrderBook, M: Mode> Engine<B, M> {
    /// Every economic value, in a fixed order (RISK.md 15.5). Allocates; tests only.
    pub fn snapshot(&self) -> EngineSnapshot {
        let mut accounts: Vec<AccountSnapshot> = self
            .accounts
            .iter()
            .map(|(&account, state)| AccountSnapshot { account, free: state.free, next_seq: state.next_seq })
            .collect();
        accounts.sort_by_key(|snapshot| snapshot.account);
        EngineSnapshot {
            accounts,
            markets: self.markets.iter().flatten().map(Self::market_snapshot).collect(),
            fund_balance: self.fund_balance,
            fund_upnl_total: self.fund_upnl_total,
            last_reported_uncovered: self.last_reported_uncovered,
            net_deposits: self.net_deposits,
        }
    }

    fn market_snapshot(market: &Market<B>) -> MarketSnapshot {
        let mut slots: Vec<SlotSnapshot> = market
            .slots
            .iter()
            .map(|(&account, slot)| {
                let (open_buys, open_sells) = Self::open_totals(market, account, slot);
                SlotSnapshot {
                    account,
                    pos: slot.pos,
                    cost: slot.cost,
                    locked: slot.locked,
                    leverage: slot.leverage,
                    open_buys,
                    open_sells,
                }
            })
            .collect();
        slots.sort_by_key(|snapshot| snapshot.account);
        MarketSnapshot {
            params: market.params,
            max_qty: market.max_qty,
            tiers: market.live_tiers().to_vec(),
            staged: market.staged[..usize::from(market.staged_rows)].to_vec(),
            staged_rows: market.staged_rows,
            staged_count: market.staged_count,
            mark: market.mark,
            upper: market.upper,
            lower: market.lower,
            fund_pos: market.fund_pos,
            fund_cost: market.fund_cost,
            fund_upnl: market.fund_upnl,
            fees_collected: market.fees_collected,
            nonzero_positions: market.nonzero_positions,
            accounts: market.accounts.clone(),
            book: market.book.snapshot(),
            slots,
        }
    }

    /// Checks the state invariants of RISK.md section 12 that hold at every command
    /// boundary, and panics, naming the invariant, at the first that fails. Walks
    /// everything and allocates; tests only.
    ///
    /// Checked: I1 to I11 and I16, with I4 only in modes with running totals and I10 only in
    /// modes with the index. The command properties (I12, I13, I14, I15, I17 to I20) need the
    /// state before the command and its events, so the tests check them, with their own
    /// formulas (`engine/src/engine/tests/command_properties.rs`).
    pub fn assert_invariants(&self) {
        let mut conserved = i128::from(self.fund_balance);
        for (&account, state) in &self.accounts {
            assert!(state.free >= Micros::ZERO, "I8: account {account} has a negative free balance");
            conserved += i128::from(state.free);
        }
        let mut fund_upnl_total = 0;
        for market in self.markets.iter().flatten() {
            self.assert_market_invariants(market);
            let slots = market.slots.values();
            let locked_minus_cost: i128 =
                slots.map(|slot| i128::from(slot.locked) - i128::from(slot.cost)).sum();
            conserved += locked_minus_cost + i128::from(market.fees_collected) - i128::from(market.fund_cost);
            fund_upnl_total += market.fund_upnl;
        }
        assert_eq!(conserved, self.net_deposits, "I3: money is not conserved");
        assert_eq!(fund_upnl_total, self.fund_upnl_total, "I9: the fund's running PnL total");
        let uncovered = uncovered_bad_debt(self.fund_balance, self.fund_upnl_total);
        assert_eq!(i128::from(self.last_reported_uncovered), uncovered, "I9: the last reported shortfall");
    }

    /// The invariants of one market and each of its slots.
    fn assert_market_invariants(&self, market: &Market<B>) {
        let id = market.params.market;
        let book = market.book.snapshot();
        assert!(!book.is_crossed(), "I1: market {id}'s book is crossed");

        let positions: i128 = market.slots.values().map(|slot| i128::from(slot.pos)).sum();
        assert_eq!(
            positions + i128::from(market.fund_pos),
            0,
            "I2: market {id}'s positions don't sum to zero"
        );
        assert_cost_basis_signs(market.fund_pos, market.fund_cost, "I8: the fund's position");
        if let Some(mark) = market.mark {
            let fund_upnl = fund_unrealized_pnl(market.fund_pos, market.fund_cost, mark);
            assert_eq!(market.fund_upnl, fund_upnl, "I9: the fund's PnL in market {id}");
        }

        let open = market.slots.values().filter(|slot| slot.pos != Qty::ZERO).count()
            + usize::from(market.fund_pos != Qty::ZERO);
        assert_eq!(market.nonzero_positions as usize, open, "market {id}'s count of positions");
        let mut listed = market.accounts.clone();
        listed.sort_unstable();
        let mut with_slots: Vec<AccountId> = market.slots.keys().copied().collect();
        with_slots.sort_unstable();
        assert_eq!(listed, with_slots, "market {id}'s account list doesn't match its slots");

        if let Some(mark) = market.mark {
            // I16: the `SetMark` sweep keeps every resting order inside the band of the
            // current mark, which the band's solvency argument needs (RISK.md 5.3, 5.4).
            for bid in &book.bids {
                assert!(
                    bid.price <= market.upper,
                    "I16: a bid at {} above the band at mark {mark}",
                    bid.price
                );
            }
            for ask in &book.asks {
                assert!(
                    ask.price >= market.lower,
                    "I16: an ask at {} below the band at mark {mark}",
                    ask.price
                );
            }
        }
        for order in book.bids.iter().chain(&book.asks) {
            let owner = account_of(order.order_id);
            let sequence = u64::from(sequence_of(order.order_id).get());
            assert!(
                sequence < self.next_seq(owner),
                "I11: order {:#x} is not below its account's next_seq",
                order.order_id.get()
            );
        }
        for (&account, slot) in &market.slots {
            Self::assert_slot_invariants(market, account, slot, &book);
        }
        if M::LIQUIDATION_INDEX {
            Self::assert_index_matches_slots(market);
        }
    }

    /// I4 to I8, and I10's per-slot part, for one slot.
    fn assert_slot_invariants(market: &Market<B>, account: AccountId, slot: &Slot, book: &BookSnapshot) {
        let what = format!("account {account} in market {}", market.params.market);
        if M::RUNNING_TOTALS {
            let own = |side| {
                let orders = book.bids.iter().chain(&book.asks);
                orders
                    .filter(|o| account_of(o.order_id) == account && o.side == side)
                    .map(|o| o.qty)
                    .sum::<Qty>()
            };
            let from_book = (own(Side::Buy), own(Side::Sell));
            assert_eq!((slot.open_buys, slot.open_sells), from_book, "I4: open totals of {what}");
        }

        let (open_buys, open_sells) = Self::open_totals(market, account, slot);
        let size = worst_case_size(slot.pos.into(), open_buys.into(), open_sells.into());
        assert!(i128::from(slot.pos.abs()) <= size, "I6: {what}");
        assert!(size <= i128::from(market.max_qty), "I6: {what} is above max_qty");
        if slot.pos == Qty::ZERO && open_buys == Qty::ZERO && open_sells == Qty::ZERO {
            assert_eq!(slot.locked, Micros::ZERO, "I7: {what} is flat with no orders but holds collateral");
        }
        assert_cost_basis_signs(slot.pos, slot.cost, &format!("I8: {what}"));
        let most_cost = i128::from(slot.pos.abs()) * i128::from(market.params.max_price);
        assert!(
            i128::from(slot.cost.abs()) <= most_cost,
            "I8: {what}'s cost basis is above |pos| x max_price"
        );

        let max_leverage = market.params.max_leverage;
        if let Some(mark) = market.mark {
            let below_mm = slot.money().is_liquidatable(mark, max_leverage);
            assert!(!below_mm, "I5: {what} is below maintenance margin");
        }
        if M::LIQUIDATION_INDEX {
            let key = slot.money().liquidation_key(max_leverage);
            assert_eq!(slot.indexed_key, key, "I10: {what} is indexed at a stale key");
            // The key is the boundary: liquidatable there, and not one tick back toward safety.
            let liquidatable = |x: Price| slot.money().is_liquidatable(x, max_leverage);
            let boundary = match key {
                Some((Side::Buy, key)) => liquidatable(key) && !liquidatable(key + Price::ONE_TICK),
                Some((Side::Sell, key)) => liquidatable(key) && !liquidatable(key - Price::ONE_TICK),
                None => true,
            };
            assert!(boundary, "I10: {what}'s key {key:?} is not its liquidation boundary");
        }
    }

    /// I10: the index's heaps are in order and agree with their position maps; it holds
    /// exactly one entry per slot with a key, at that key; and nothing in it is at or
    /// through the mark, so the first long is below it and the first short above it (what
    /// the `SetMark` walk relies on to stop).
    fn assert_index_matches_slots(market: &Market<B>) {
        let id = market.params.market;
        market.index.assert_consistent();
        let mut expected: Vec<(Side, Price, AccountId)> = market
            .slots
            .iter()
            .filter_map(|(&account, slot)| slot.indexed_key.map(|(side, key)| (side, key, account)))
            .collect();
        let mut actual = market.index.entries();
        expected.sort_unstable_by_key(|&(side, key, account)| (side as u8, key, account));
        actual.sort_unstable_by_key(|&(side, key, account)| (side as u8, key, account));
        assert_eq!(actual, expected, "I10: market {id}'s index doesn't match its slots' keys");
        if let Some(mark) = market.mark {
            let first_long = market.index.first_long();
            let first_short = market.index.first_short();
            assert!(first_long.is_none_or(|(key, _)| key < mark), "I10: a long at or through the mark");
            assert!(first_short.is_none_or(|(key, _)| key > mark), "I10: a short at or through the mark");
        }
    }

    /// Sets one slot's position, cost basis and collateral directly, for the one test whose
    /// state commands can't reach (RISK.md 10.1: a flat slot with negative collateral). The
    /// account and slot are created if needed, and `nonzero_positions` and the index follow.
    /// The change in the slot's `locked − cost` is booked to `net_deposits`, as if that much
    /// had been deposited or withdrawn, so money is still conserved (I3). Nothing else
    /// follows: positions summing to zero (I2) and the rest are up to the test.
    #[cfg(test)]
    pub(crate) fn set_slot_for_test(
        &mut self,
        market_id: crate::types::MarketId,
        account: AccountId,
        pos: Qty,
        cost: Micros,
        locked: Micros,
    ) {
        self.accounts.entry(account).or_insert(crate::state::Account::NEW);
        let market = self.market_mut(market_id);
        let slot = market.slot_or_create(account);
        let was_open = slot.pos != Qty::ZERO;
        let value_change =
            (i128::from(locked) - i128::from(cost)) - (i128::from(slot.locked) - i128::from(slot.cost));
        (slot.pos, slot.cost, slot.locked) = (pos, cost, locked);
        slot.key_dirty = true;
        crate::state::count_position_change(&mut market.nonzero_positions, was_open, pos != Qty::ZERO);
        Self::rekey(market, account);
        self.net_deposits += value_change;
    }
}

/// I8's sign rule for a position and its cost basis: flat has no cost basis, a long's is at
/// least 0 and a short's at most 0.
fn assert_cost_basis_signs(pos: Qty, cost: Micros, what: &str) {
    let signs_agree = match pos.cmp(&Qty::ZERO) {
        Ordering::Equal => cost == Micros::ZERO,
        Ordering::Greater => cost >= Micros::ZERO,
        Ordering::Less => cost <= Micros::ZERO,
    };
    assert!(signs_agree, "{what}: position {pos} with cost basis {cost}");
}
