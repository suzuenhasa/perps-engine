//! How the book's events, the post-command pass and the insurance fund's totals change the
//! ledger (`docs/RISK.md` 7, 8, 9.3, 10.3 and 10.4), and three of the Mode seam's four
//! functions (14.2).
//!
//! **Contract.** The engine never hands its output sink to the book. For each book call it
//! clears a scratch buffer, lets the book write into it, and then walks it: each book event
//! is applied to the ledger and emitted, followed by the events it causes (7.1). The book
//! runs to completion first, so matching never sees a half-updated slot. After matching,
//! one pass over the touched slots liquidates, releases and re-keys them (8).
//!
//! **Invariant.** After each event block (a top-up, a fill with its two positions, a
//! release), positions sum to zero and money is conserved exactly (RISK.md 12, I2 and I3):
//! a fill updates both slots and the fees in one step.
//!
//! **Complexity.** O(1) per book event and per touched slot, plus one O(log n) re-key per
//! touched slot whose position or collateral changed. In the naive modes, reading a slot's
//! open totals walks its account's orders in the book.

use crate::book::OrderBook;
use crate::event::{CancelReason, Cancelled, Event, EventSink, Fill, InsuranceShortfall};
use crate::mode::Mode;
use crate::money::{fee, fund_unrealized_pnl, initial_margin, narrow, uncovered_bad_debt, worst_case_size};
use crate::state::{Market, Slot};
use crate::types::{AccountId, MarketId, Micros, Price, Qty, Side, account_of};

use super::{Engine, balance_event, position_event};

impl<B: OrderBook, M: Mode> Engine<B, M> {
    // -----------------------------------------------------------------------------------
    // The Mode seam (RISK.md 14.2): functions 1 to 3 of 4. The fourth, the SetMark
    // liquidation step, is in `liquidation.rs`.

    /// Mode function 1: the slot's open buys and open sells, as running totals or
    /// recomputed from the book (O(k) for the account's k resting orders).
    pub(super) fn open_totals(market: &Market<B>, account: AccountId, slot: &Slot) -> (Qty, Qty) {
        if M::RUNNING_TOTALS {
            (slot.open_buys, slot.open_sells)
        } else {
            market.book.open_quantities(account)
        }
    }

    /// Mode function 2: adds `change` lots to the slot's open total on `side`, or does
    /// nothing in the modes that recompute totals from the book.
    pub(super) fn add_open(slot: &mut Slot, side: Side, change: Qty) {
        if M::RUNNING_TOTALS {
            match side {
                Side::Buy => slot.open_buys += change,
                Side::Sell => slot.open_sells += change,
            }
        }
    }

    /// Mode function 3: brings the slot's entry in the liquidation index up to date
    /// (RISK.md 9.3), or does nothing in the modes without the index. Only if something the
    /// key depends on (`pos`, `cost`, `locked`) changed since the last re-key: then one
    /// `i128` division, and O(log n) if the key moved.
    pub(super) fn rekey(market: &mut Market<B>, account: AccountId) {
        if !M::LIQUIDATION_INDEX {
            return;
        }
        let max_leverage = market.params.max_leverage;
        // The fields, not `market.slot_mut()`, so that the index can change alongside.
        let slot = market.slots.get_mut(&account).expect("a touched account has a slot");
        if !slot.key_dirty {
            return;
        }
        slot.key_dirty = false;
        let key = slot.money().liquidation_key(max_leverage);
        debug_assert_key_is_exact(slot, key, market.mark, max_leverage);
        if key != slot.indexed_key {
            market.index.refile(account, slot.indexed_key, key);
            slot.indexed_key = key;
        }
    }

    /// The slot's worst-case size now (RISK.md 4.2), with its open totals read through the
    /// Mode. At most `max_qty` at a command boundary (I6), so it fits in `Qty`.
    pub(super) fn worst_case_size_of(market: &Market<B>, account: AccountId, slot: &Slot) -> Qty {
        let (open_buys, open_sells) = Self::open_totals(market, account, slot);
        let size = worst_case_size(slot.pos.into(), open_buys.into(), open_sells.into());
        Qty::new(narrow(size, "worst-case size"))
    }

    // -----------------------------------------------------------------------------------
    // Calling the book and walking its events (RISK.md 7.1).

    /// Clears the scratch buffer and lets `call` make its calls on the market's book into it
    /// (one, except the band sweep's two `cancel_beyond`s).
    pub(super) fn call_book(&mut self, market_id: MarketId, call: impl FnOnce(&mut B, &mut Vec<Event>)) {
        self.scratch.clear();
        let market = self.markets[market_id.index()].as_mut().expect("the command's market exists");
        call(&mut market.book, &mut self.scratch);
    }

    /// Panics unless the book's first event is the one the engine's checks promised. A
    /// mismatch is a bug in the engine's copy of the book's checks, and stopping,
    /// deterministically, beats a half-applied command. In release builds too.
    pub(super) fn assert_first_book_event(&self, expected: &str, is_expected: impl Fn(&Event) -> bool) {
        let first = self.scratch.first();
        assert!(
            first.is_some_and(is_expected),
            "the book disagreed with the engine's checks: expected {expected}, got {first:?}"
        );
    }

    /// Walks the scratch buffer in order, applying each book event to the ledger and
    /// emitting it, followed by the events it causes (RISK.md 7.1). `top_up` is the top-up
    /// the command applied before the book call, if any, whose events section 11 puts right
    /// after the book's first event (`Ack` or `Modified`).
    ///
    /// Walked by index, with each event copied out (`Event` is `Copy`), so the engine can
    /// update itself inside the loop without allocating or holding a borrow of the buffer.
    pub(super) fn apply_book_events(
        &mut self,
        market_id: MarketId,
        top_up: Option<TopUp>,
        events: &mut impl EventSink,
    ) {
        for i in 0..self.scratch.len() {
            let event = self.scratch[i];
            match event {
                Event::Ack(_) | Event::Modified(_) => {
                    events.emit(event);
                    if let Some(top_up) = top_up {
                        top_up.emit(events);
                    }
                }
                Event::Fill(fill) => self.apply_fill(market_id, fill, events),
                Event::Cancelled(cancelled) => self.apply_cancelled(market_id, cancelled, events),
                other => panic!("the book emitted {other:?}, which the engine's checks rule out"),
            }
        }
    }

    /// One fill (RISK.md 7.2 to 7.4): both fees, both slots' positions and collateral, both
    /// open totals and the market's fees, all in one step. Emits the fill with its fees,
    /// then the maker's position, then the taker's; the maker's slot joins the pass.
    fn apply_fill(&mut self, market_id: MarketId, fill: Fill, events: &mut impl EventSink) {
        let (maker, taker) = (account_of(fill.maker_order), account_of(fill.taker_order));
        let market = self.market_mut(market_id);
        let maker_fee = fee(fill.price, fill.qty, market.params.maker_fee_ppm);
        let taker_fee = fee(fill.price, fill.qty, market.params.taker_fee_ppm);
        let fees = maker_fee + taker_fee;
        market.fees_collected =
            market.fees_collected.checked_add(fees).expect("fees_collected overflows i64");

        // The taker's lots are signed by its side; the maker traded the other way. Each
        // side's order has `qty` fewer lots resting.
        let taker_lots = fill.taker_side.signed(fill.qty);
        let maker_slot = market.trade(maker, -taker_lots, fill.price, maker_fee);
        Self::add_open(maker_slot, fill.taker_side.opposite(), -fill.qty);
        let maker_position = position_event(market_id, maker, maker_slot);
        let taker_slot = market.trade(taker, taker_lots, fill.price, taker_fee);
        Self::add_open(taker_slot, fill.taker_side, -fill.qty);
        let taker_position = position_event(market_id, taker, taker_slot);

        events.emit(Event::Fill(Fill { maker_fee, taker_fee, ..fill }));
        events.emit(maker_position);
        events.emit(taker_position);
        self.touch(market_id, maker);
    }

    /// One cancelled order (RISK.md 7.1, 7.4), whatever the reason: a user's cancel, a
    /// self-trade, an IOC remainder, a modify to at most the filled size, the price-band
    /// sweep or a liquidation.
    fn apply_cancelled(&mut self, market_id: MarketId, cancelled: Cancelled, events: &mut impl EventSink) {
        let owner = account_of(cancelled.order_id);
        // Its remaining quantity comes off its owner's open total on its side. Except after
        // a liquidation, which zeroes the slot's totals itself (RISK.md 10.1).
        if cancelled.reason != CancelReason::Liquidation {
            let slot = self.market_mut(market_id).slot_mut(owner);
            Self::add_open(slot, cancelled.side, -cancelled.remaining);
        }
        // An order the `SetMark` sweep cancelled puts its owner's slot in the pass, which may
        // release the collateral that backed it.
        if cancelled.reason == CancelReason::PriceBand {
            self.touch(market_id, owner);
        }
        events.emit(Event::Cancelled(cancelled));
    }

    // -----------------------------------------------------------------------------------
    // Top-ups (RISK.md 5.2).

    /// Moves `amount` from the account's free balance into its slot: a top-up, which the
    /// check made sure the free balance covers. Emits nothing, and returns the top-up's
    /// events for the caller to emit where section 11 puts them; `None` if `amount` is 0,
    /// which moves and reports nothing.
    pub(super) fn move_to_slot(
        &mut self,
        market_id: MarketId,
        account: AccountId,
        amount: Micros,
    ) -> Option<TopUp> {
        if amount == Micros::ZERO {
            return None;
        }
        let state = self.account_mut(account);
        debug_assert!(state.free >= amount, "a top-up of {amount} from a free balance of {}", state.free);
        state.free -= amount;
        let balance = balance_event(account, state.free);
        let slot = self.market_mut(market_id).slot_mut(account);
        slot.locked += amount;
        slot.key_dirty = true;
        Some(TopUp { balance, slot: position_event(market_id, account, slot) })
    }

    // -----------------------------------------------------------------------------------
    // The post-command pass (RISK.md 8).

    /// Visits every touched slot in order (the command's own slot first, then makers in the
    /// order of their first fill; for `SetMark`, the owners of swept orders): liquidates it
    /// if its equity is below maintenance margin, otherwise releases what it holds above
    /// its requirement, then re-keys it. Liquidating one slot changes no other slot's
    /// equity, so the order only decides the order of events.
    pub(super) fn run_post_command_pass(&mut self, market_id: MarketId, events: &mut impl EventSink) {
        for i in 0..self.touched.len() {
            let account = self.touched[i];
            if self.is_below_maintenance(market_id, account) {
                self.liquidate_slot(market_id, account, events);
            } else {
                self.release_excess(market_id, account, events);
            }
            Self::rekey(self.market_mut(market_id), account);
        }
    }

    /// The pass's direct check, `equity < MM` at the current mark (RISK.md 8.1). It equals
    /// the key test, and also catches a flat slot with negative collateral.
    pub(super) fn is_below_maintenance(&self, market_id: MarketId, account: AccountId) -> bool {
        let market = self.market(market_id);
        let mark = market.mark.expect("the pass runs only in a market with a mark");
        market.slot(account).money().is_liquidatable(mark, market.params.max_leverage)
    }

    /// The release rule (RISK.md 8.2): returns `max(0, min(locked, E) − IM(W))` to the free
    /// balance, with `W` and `E` at the current mark after the command. Emits the release,
    /// slot first, then the free balance, only if something was released.
    fn release_excess(&mut self, market_id: MarketId, account: AccountId, events: &mut impl EventSink) {
        let market = self.market(market_id);
        let mark = market.mark.expect("the pass runs only in a market with a mark");
        let slot = market.slot(account);
        let size = Self::worst_case_size_of(market, account, slot);
        let requirement = initial_margin(size, mark, slot.leverage, market.live_tiers());
        let release = slot.money().release_amount(mark, requirement);
        if release == Micros::ZERO {
            return;
        }

        let slot = self.market_mut(market_id).slot_mut(account);
        slot.locked -= release;
        slot.key_dirty = true;
        events.emit(position_event(market_id, account, slot));
        let state = self.account_mut(account);
        state.free = state.free.checked_add(release).expect("a free balance overflows i64");
        events.emit(balance_event(account, state.free));
    }

    // -----------------------------------------------------------------------------------
    // The insurance fund's running totals (RISK.md 10.3, 10.4).

    /// Recomputes the fund's unrealized PnL in one market at its current mark, and moves the
    /// running total over all markets by the difference: O(1), whatever the number of
    /// markets. After every `SetMark`, and after every absorb.
    pub(super) fn revalue_fund_position(&mut self, market_id: MarketId) {
        let market = self.market_mut(market_id);
        let mark = market.mark.expect("revalued at a mark");
        let revalued = fund_unrealized_pnl(market.fund_pos, market.fund_cost, mark);
        let change = revalued - market.fund_upnl;
        market.fund_upnl = revalued;
        self.fund_upnl_total += change;
    }

    /// The shortfall report (RISK.md 10.4): at the end of a command that can change the
    /// fund, emits `InsuranceShortfall` if the uncovered bad debt differs from the last
    /// value reported. So at most once per command, after all its other events, and also
    /// when it goes back to 0.
    pub(super) fn report_shortfall(&mut self, events: &mut impl EventSink) {
        let uncovered = uncovered_bad_debt(self.fund_balance, self.fund_upnl_total);
        let uncovered = Micros::new(narrow(uncovered, "uncovered bad debt"));
        if uncovered != self.last_reported_uncovered {
            self.last_reported_uncovered = uncovered;
            events.emit(Event::InsuranceShortfall(InsuranceShortfall { uncovered }));
        }
    }
}

/// The two events of a top-up, taken when it was applied: the free balance, then the slot,
/// source first (RISK.md 11). A command that tops up before its book call reports them after
/// the book's first event; the slot's state is the same then, because the book doesn't touch
/// the ledger.
#[derive(Clone, Copy, Debug)]
pub(super) struct TopUp {
    balance: Event,
    slot: Event,
}

impl TopUp {
    pub(super) fn emit(self, events: &mut impl EventSink) {
        events.emit(self.balance);
        events.emit(self.slot);
    }
}

/// In debug builds, checks a new key the way INFO.md's "correct it by a tick" would have
/// (RISK.md 9.1): the slot is liquidatable at its key and not one tick further from it; and
/// the key is on the far side of the current mark, because the pass has just liquidated
/// every slot that was at or through it. Release builds skip it: the formula is exact.
fn debug_assert_key_is_exact(
    slot: &Slot,
    key: Option<(Side, Price)>,
    mark: Option<Price>,
    max_leverage: u16,
) {
    let liquidatable = |x: Price| slot.money().is_liquidatable(x, max_leverage);
    match (key, mark) {
        (Some((Side::Buy, key)), Some(mark)) => {
            debug_assert!(
                liquidatable(key) && !liquidatable(key + Price::ONE_TICK),
                "long key {key} is not the boundary"
            );
            debug_assert!(key < mark, "long key {key} is at or above the mark {mark}");
        }
        (Some((Side::Sell, key)), Some(mark)) => {
            debug_assert!(
                liquidatable(key) && !liquidatable(key - Price::ONE_TICK),
                "short key {key} is not the boundary"
            );
            debug_assert!(key > mark, "short key {key} is at or below the mark {mark}");
        }
        _ => {}
    }
}
