//! The client commands, `PlaceOrder`, `CancelOrder` and `ModifyOrder` (`docs/RISK.md` 6.1
//! to 6.3), and the pre-trade check that places and replacing modifies share (section 5).
//!
//! **Contract.** Each command's checks run in the order RISK.md lists them, and the first
//! failure is the command's single `Reject`. The book's own checks come first, in the
//! book's order, so a book reject can never follow a top-up; then `NoMark`, the price
//! band, the size limit and the margin rule. Pure decreases (a cancel, a shrink in place,
//! a removal) skip every risk check: they add no fill volume.
//!
//! **Complexity.** O(1) in `Fast` apart from the book's matching; the naive modes read
//! open totals by walking the account's orders.

use crate::book::{OrderBook, RestingOrder};
use crate::command::{CancelOrder, ModifyOrder, PlaceOrder};
use crate::event::{CancelReason, Event, EventSink, RejectReason};
use crate::mode::Mode;
use crate::money::{initial_margin, is_strictly_reducing, narrow, worst_case_size};
use crate::state::{Market, Slot};
use crate::types::{AccountId, Micros, Price, Qty, Side, account_of, sequence_of};

use super::{Engine, FUND, reject};

/// What an accepted modify does to the order (RISK.md 6.3). `new_size` is the order's new
/// total size, so what is left to fill becomes `r' = new_size − filled` (D-008).
#[derive(Clone, Copy, Debug)]
enum ModifyKind {
    /// `r' <= 0`: nothing is left to fill, and the book removes the order
    /// (`Cancelled { SizeBelowFilled }`).
    Removal,
    /// Same price and `r' <= r`: the book updates the order in place, keeping its priority.
    /// `shrink_by = r − r'` comes off the side's open total.
    ShrinkInPlace { shrink_by: Qty },
    /// Anything else is cancel-and-replace, checked like a new order: `side_change = r' − r`
    /// goes onto the side's open total, and `top_up` is what the pre-trade check asked for.
    Replace { side_change: Qty, top_up: Micros },
}

impl<B: OrderBook, M: Mode> Engine<B, M> {
    // -----------------------------------------------------------------------------------
    // PlaceOrder (RISK.md 6.1).

    pub(super) fn place_order(&mut self, order: &PlaceOrder, events: &mut impl EventSink) {
        match self.check_place(order) {
            Ok(top_up) => self.accept_place(order, top_up, events),
            Err(reason) => reject(events, order.order_id, account_of(order.order_id), reason),
        }
    }

    /// Checks 1 to 10 of RISK.md 6.1, in order. Returns the top-up the order needs (0 if
    /// none).
    fn check_place(&self, order: &PlaceOrder) -> Result<Micros, RejectReason> {
        let market = self.find_market(order.market).ok_or(RejectReason::UnknownMarket)?;
        let owner = account_of(order.order_id);
        if owner == FUND {
            return Err(RejectReason::ReservedAccount);
        }
        if order.qty < Qty::new(1) {
            return Err(RejectReason::InvalidQty);
        }
        if !market.price_in_range(order.price) {
            return Err(RejectReason::InvalidPrice);
        }
        // Every resting order of the account has a lower sequence, so the book's own
        // duplicate check can never fire after this one.
        if u64::from(sequence_of(order.order_id).get()) < self.next_seq(owner) {
            return Err(RejectReason::Duplicate);
        }
        if order.post_only && would_cross(&market.book, order.side, order.price) {
            return Err(RejectReason::PostOnlyWouldCross);
        }
        self.check_exposure(market, owner, order.side, order.price, i128::from(order.qty))
    }

    /// The effects of an accepted place, in RISK.md 6.1's order.
    fn accept_place(&mut self, order: &PlaceOrder, top_up: Micros, events: &mut impl EventSink) {
        let (market_id, owner) = (order.market, account_of(order.order_id));
        self.start_command();
        self.market_mut(market_id).slot_or_create(owner);
        // The command's own slot comes first in the post-command pass.
        self.touch(market_id, owner);
        self.account_mut(owner).next_seq = u64::from(sequence_of(order.order_id).get()) + 1;
        // Counted in full before matching, an IOC too: its fills take their quantity off
        // again, and so does the book's cancel of whatever an IOC leaves unfilled.
        Self::add_open(self.market_mut(market_id).slot_mut(owner), order.side, order.qty);
        let top_up = self.move_to_slot(market_id, owner, top_up);

        self.call_book(market_id, |book, scratch| book.place(order, scratch));
        self.assert_first_book_event("Ack", |event| matches!(event, Event::Ack(_)));
        self.apply_book_events(market_id, top_up, events);
        self.run_post_command_pass(market_id, events);
        self.report_shortfall(events);
    }

    // -----------------------------------------------------------------------------------
    // CancelOrder (RISK.md 6.2).

    pub(super) fn cancel_order(&mut self, cancel: &CancelOrder, events: &mut impl EventSink) {
        let owner = account_of(cancel.order_id);
        if let Err(reason) = self.check_cancel(cancel) {
            return reject(events, cancel.order_id, owner, reason);
        }
        // Accepted in any margin state. Ownership is the gateway's check (D-005).
        self.start_command();
        self.touch(cancel.market, owner);
        self.call_book(cancel.market, |book, scratch| book.cancel(cancel.order_id, scratch));
        self.assert_first_book_event(
            "Cancelled { UserRequested }",
            |event| matches!(event, Event::Cancelled(c) if c.reason == CancelReason::UserRequested),
        );
        self.apply_book_events(cancel.market, None, events);
        self.run_post_command_pass(cancel.market, events);
    }

    /// Checks 1 and 2 of RISK.md 6.2: the market exists and the order rests in its book.
    fn check_cancel(&self, cancel: &CancelOrder) -> Result<(), RejectReason> {
        let market = self.find_market(cancel.market).ok_or(RejectReason::UnknownMarket)?;
        market.book.order(cancel.order_id).ok_or(RejectReason::UnknownOrder)?;
        Ok(())
    }

    // -----------------------------------------------------------------------------------
    // ModifyOrder (RISK.md 6.3).

    pub(super) fn modify_order(&mut self, modify: &ModifyOrder, events: &mut impl EventSink) {
        match self.check_modify(modify) {
            Ok((side, kind)) => self.accept_modify(modify, side, kind, events),
            Err(reason) => reject(events, modify.order_id, account_of(modify.order_id), reason),
        }
    }

    /// Checks 1 to 4 of RISK.md 6.3, then sorts the modify into one of its three kinds; a
    /// replace also goes through checks 5 to 9. Returns the order's side and the kind.
    fn check_modify(&self, modify: &ModifyOrder) -> Result<(Side, ModifyKind), RejectReason> {
        let market = self.find_market(modify.market).ok_or(RejectReason::UnknownMarket)?;
        let order: RestingOrder = market.book.order(modify.order_id).ok_or(RejectReason::UnknownOrder)?;
        if modify.new_size < Qty::new(1) {
            return Err(RejectReason::InvalidQty);
        }
        if !market.price_in_range(modify.new_price) {
            return Err(RejectReason::InvalidPrice);
        }

        let remaining = order.qty;
        let new_remaining = modify.new_size - order.filled;
        if new_remaining <= Qty::ZERO {
            return Ok((order.side, ModifyKind::Removal));
        }
        if modify.new_price == order.price && new_remaining <= remaining {
            return Ok((order.side, ModifyKind::ShrinkInPlace { shrink_by: remaining - new_remaining }));
        }

        if order.post_only && would_cross(&market.book, order.side, modify.new_price) {
            return Err(RejectReason::PostOnlyWouldCross);
        }
        let side_change = new_remaining - remaining;
        let owner = account_of(modify.order_id);
        let top_up =
            self.check_exposure(market, owner, order.side, modify.new_price, i128::from(side_change))?;
        Ok((order.side, ModifyKind::Replace { side_change, top_up }))
    }

    /// The effects of an accepted modify (RISK.md 6.3). A rejected modify never gets here,
    /// so it leaves the original order untouched.
    fn accept_modify(
        &mut self,
        modify: &ModifyOrder,
        side: Side,
        kind: ModifyKind,
        events: &mut impl EventSink,
    ) {
        let (market_id, owner) = (modify.market, account_of(modify.order_id));
        self.start_command();
        self.touch(market_id, owner);
        let top_up = match kind {
            // The book's `Cancelled { SizeBelowFilled }` takes the remaining quantity off.
            ModifyKind::Removal => None,
            ModifyKind::ShrinkInPlace { shrink_by } => {
                Self::add_open(self.market_mut(market_id).slot_mut(owner), side, -shrink_by);
                None
            }
            ModifyKind::Replace { side_change, top_up } => {
                Self::add_open(self.market_mut(market_id).slot_mut(owner), side, side_change);
                self.move_to_slot(market_id, owner, top_up)
            }
        };

        self.call_book(market_id, |book, scratch| {
            book.modify(modify.order_id, modify.new_price, modify.new_size, scratch)
        });
        match kind {
            ModifyKind::Removal => self.assert_first_book_event(
                "Cancelled { SizeBelowFilled }",
                |event| matches!(event, Event::Cancelled(c) if c.reason == CancelReason::SizeBelowFilled),
            ),
            _ => self.assert_first_book_event("Modified", |event| matches!(event, Event::Modified(_))),
        }
        // A replace matches like a new order: its fills and self-trade cancels follow.
        self.apply_book_events(market_id, top_up, events);
        self.run_post_command_pass(market_id, events);
        if let ModifyKind::Replace { .. } = kind {
            self.report_shortfall(events);
        }
    }

    // -----------------------------------------------------------------------------------
    // The pre-trade check (RISK.md 5).

    /// Checks 7 to 10 of RISK.md 6.1 (6 to 9 of 6.3, for a replace): the market has a mark,
    /// the price is inside the band, the slot's worst-case size stays within `max_qty`, and
    /// the order is strictly reducing or passes the margin rule. `side_change` is what the
    /// order adds to its side's open total: a new order's quantity, or a replace's
    /// `r' − r`. Returns the top-up the order needs (0 if none).
    fn check_exposure(
        &self,
        market: &Market<B>,
        account: AccountId,
        side: Side,
        price: Price,
        side_change: i128,
    ) -> Result<Micros, RejectReason> {
        if market.mark.is_none() {
            return Err(RejectReason::NoMark);
        }
        let outside_band = match side {
            Side::Buy => price > market.upper,
            Side::Sell => price < market.lower,
        };
        if outside_band {
            return Err(RejectReason::PriceBand);
        }

        // An account's first order in this market is checked against an empty slot.
        let slot = market.find_slot(account).copied().unwrap_or(Slot::NEW);
        let (open_buys, open_sells) = Self::open_totals(market, account, &slot);
        // As if the order were resting in full. In i128: a client can send any quantity.
        let (buys_after, sells_after) = match side {
            Side::Buy => (i128::from(open_buys) + side_change, i128::from(open_sells)),
            Side::Sell => (i128::from(open_buys), i128::from(open_sells) + side_change),
        };
        let size_after = worst_case_size(slot.pos.into(), buys_after, sells_after);
        if size_after > i128::from(market.max_qty) {
            return Err(RejectReason::SizeLimit);
        }
        if is_strictly_reducing(slot.pos, side, buys_after, sells_after) {
            return Ok(Micros::ZERO);
        }
        self.check_margin_rule(market, account, &slot, Qty::new(narrow(size_after, "worst-case size")))
    }

    /// The margin rule (RISK.md 5.2), for an order that would take the slot's worst-case
    /// size to `size_after`, valued at the mark with unrealized profit counting. If the
    /// free balance covers the shortfall to `IM(size_after)`, the order is accepted with
    /// that top-up; this also cures a margin call (the owner's answer to Q1). Otherwise the
    /// reason says whether the slot is in margin call already.
    fn check_margin_rule(
        &self,
        market: &Market<B>,
        account: AccountId,
        slot: &Slot,
        size_after: Qty,
    ) -> Result<Micros, RejectReason> {
        let mark = market.mark.expect("the caller checked that the market has a mark");
        let tiers = market.live_tiers();
        let requirement = initial_margin(size_after, mark, slot.leverage, tiers);
        let need = slot.money().top_up_needed(mark, requirement);
        if need <= self.free_balance(account) {
            Ok(need)
        } else if slot.equity(mark) < initial_margin(slot.pos.abs(), mark, slot.leverage, tiers) {
            Err(RejectReason::MarginCall)
        } else {
            Err(RejectReason::InsufficientMargin)
        }
    }
}

/// Would an order on `side` at `price` trade against the best opposite order? The book's
/// own post-only test, from its best prices.
fn would_cross(book: &impl OrderBook, side: Side, price: Price) -> bool {
    match side {
        Side::Buy => book.best_ask().is_some_and(|ask| price >= ask),
        Side::Sell => book.best_bid().is_some_and(|bid| price <= bid),
    }
}
