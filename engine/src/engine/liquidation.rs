//! Liquidation, the insurance fund's takeover, and what `SetMark` does after it has moved
//! the mark: the liquidation walk and the price-band sweep (`docs/RISK.md` 5.4, 9.4, 10.1,
//! 14.2; D-002, D-003, D-018, D-019).
//!
//! **Contract.**
//! - [`Engine::liquidate_slot`] takes over one slot whose equity is below maintenance
//!   margin: it cancels the account's orders in the market without releasing anything,
//!   then moves the slot's position, cost basis and all of its collateral to the insurance
//!   fund, which nets it into its own position (10.1). The post-command pass calls it for a
//!   touched slot (8.1), and the `SetMark` walk for a slot the new mark has crossed.
//! - [`Engine::liquidate_crossed_slots`] is the fourth function of the Mode seam (14.2):
//!   with the liquidation index it walks the index from its first elements (9.4); without
//!   it, it scans every slot in the market and finds each crossed slot's key by binary
//!   search. Both liquidate the same slots in the same order: longs first, highest key
//!   first, then shorts, lowest key first, ties to the lower account id.
//! - [`Engine::sweep_out_of_band_orders`] cancels every resting order the new band would
//!   reject (5.4), and puts each owner in the pass that follows, which releases the
//!   collateral that backed the order.
//!
//! **Invariants.** After a liquidation the account has no resting orders in the market and
//! its slot is all zeros (I12), and it is out of the index. Positions still sum to zero and
//! money is conserved exactly (I2, I3): the slot's `locked − cost` moves to the fund's
//! `fund_balance − fund_cost` unchanged, whatever `apply_change` rounds (RISK.md 10.1). After
//! `SetMark`, no slot is below maintenance margin (I5) and every resting order is inside the
//! band of the new mark (I16).
//!
//! **Complexity.** One liquidation costs O(k) for the account's k orders in the market, plus
//! O(log n) to leave the index. The walk costs O((1 + L) × log n) for L liquidations. The
//! sweep costs O(1 + orders cancelled), plus one release and re-key per owner: the book
//! walks from its best price inward and stops at the first level inside the band, so a
//! sweep that cancels nothing is two comparisons. The naive scan costs O(n) per `SetMark`
//! for the market's n slots, plus O(log |mark move|) per crossed slot; it allocates only
//! when something is crossed.

use std::cmp::Reverse;

use crate::book::OrderBook;
use crate::event::{CancelReason, Event, EventSink, InsuranceAbsorb, Liquidation};
use crate::mode::Mode;
use crate::money::{is_liquidatable, liquidation_key_by_search, narrow};
use crate::state::Absorbed;
use crate::types::{AccountId, MarketId, Price, Side};

use super::{Engine, FUND, balance_event, fund_position_event, position_event};

impl<B: OrderBook, M: Mode> Engine<B, M> {
    // -----------------------------------------------------------------------------------
    // Liquidating one slot (RISK.md 10.1).

    /// Liquidates one slot whose equity is below maintenance margin, in RISK.md 10.1's four
    /// steps, with the events section 11 lists:
    /// `Cancelled { Liquidation }*, Liquidation, PositionChanged(slot), InsuranceAbsorb,
    /// PositionChanged(FUND), BalanceChanged(FUND)`.
    ///
    /// Doesn't add the slot to the touched list, and leaves it re-keyed (out of the index),
    /// so the pass can visit it and the `SetMark` walk can move on to the next first element.
    pub(super) fn liquidate_slot(
        &mut self,
        market_id: MarketId,
        account: AccountId,
        events: &mut impl EventSink,
    ) {
        debug_assert!(self.is_below_maintenance(market_id, account), "liquidating a slot that is above MM");

        // 1. Cancel the account's orders here, oldest first, so they can't reopen a position
        //    with no collateral behind it. No release: all of `locked` stays in the slot for
        //    step 3, consistent with the equity the check used. The scratch walk leaves the
        //    ledger alone for a `Cancelled { Liquidation }`; the open totals are zeroed here
        //    (in the modes that recompute them from the book they are 0 already).
        self.call_book(market_id, |book, scratch| {
            book.cancel_account(account, CancelReason::Liquidation, scratch);
        });
        self.apply_book_events(market_id, None, events);
        let market = self.market_mut(market_id);
        let slot = market.slot_mut(account);
        (slot.open_buys, slot.open_sells) = (0, 0);

        // 2. Say what is being liquidated.
        events.emit(Event::Liquidation(Liquidation { position: slot.pos, account, market: market_id }));

        // 3. Take everything out of the slot. A flat slot has no key, so re-keying it takes
        //    it out of the index (in the modes that have one).
        let absorbed = market.empty_slot(account);
        Self::rekey(market, account);
        events.emit(position_event(market_id, account, market.slot(account)));
        events.emit(Event::InsuranceAbsorb(InsuranceAbsorb {
            position: absorbed.pos,
            cost_basis: absorbed.cost,
            collateral: absorbed.locked,
            market: market_id,
        }));

        // 4. The fund takes it over.
        self.absorb_into_fund(market_id, absorbed, events);
    }

    /// Step 4 of RISK.md 10.1: nets the absorbed position into the fund's position with the
    /// fills' `apply_change`, and credits the fund's balance with the slot's collateral plus
    /// the PnL the netting realized. Then revalues the fund's position at the current mark,
    /// which keeps its running PnL total exact (10.3). Emits the fund's position, then its
    /// balance.
    ///
    /// Netting itself doesn't change the fund's equity; the absorb changes it by exactly the
    /// slot's equity at the mark, which may be negative (bad debt, reported at the end of
    /// the command by the shortfall report).
    fn absorb_into_fund(&mut self, market_id: MarketId, absorbed: Absorbed, events: &mut impl EventSink) {
        let market = self.market_mut(market_id);
        let realized = market.net_into_fund(absorbed.pos, absorbed.cost);
        let (fund_pos, fund_cost) = (market.fund_pos, market.fund_cost);
        let balance = i128::from(self.fund_balance) + i128::from(absorbed.locked) + i128::from(realized);
        self.fund_balance = narrow(balance, "the insurance fund's balance");
        self.revalue_fund_position(market_id);
        events.emit(fund_position_event(market_id, fund_pos, fund_cost));
        events.emit(balance_event(FUND, self.fund_balance));
    }

    // -----------------------------------------------------------------------------------
    // The SetMark liquidation step: Mode function 4 (RISK.md 9.4, 14.2).

    /// Mode function 4: liquidates every slot the new mark has crossed, longs first (highest
    /// key first), then shorts (lowest key first), ties to the lower account id. With the
    /// index, a walk from its first elements; without it, a scan of every slot in the market.
    /// `previous_mark` is the mark before this `SetMark` (`None` for the market's first),
    /// which only the scan needs.
    pub(super) fn liquidate_crossed_slots(
        &mut self,
        market_id: MarketId,
        previous_mark: Option<Price>,
        events: &mut impl EventSink,
    ) {
        if M::LIQUIDATION_INDEX {
            self.liquidate_by_walking_the_index(market_id, events);
        } else {
            self.liquidate_by_scanning_every_slot(market_id, previous_mark, events);
        }
    }

    /// The index walk (RISK.md 9.4). Each liquidation takes the slot out of the index, so each
    /// loop always looks at the current first element, and stops at the first one that the
    /// mark hasn't reached. A key at or through the mark means exactly `equity < MM` (9.1).
    fn liquidate_by_walking_the_index(&mut self, market_id: MarketId, events: &mut impl EventSink) {
        let mark = self.market(market_id).mark.expect("SetMark has just set the mark");
        // A falling mark reaches the longs with the highest keys first.
        while let Some((_, account)) =
            self.market(market_id).index.first_long().filter(|&(key, _)| key >= mark)
        {
            self.liquidate_slot(market_id, account, events);
        }
        // A rising mark reaches the shorts with the lowest keys first.
        while let Some((_, account)) =
            self.market(market_id).index.first_short().filter(|&(key, _)| key <= mark)
        {
            self.liquidate_slot(market_id, account, events);
        }
    }

    /// The naive `SetMark` step (RISK.md 14.2), the executable reference for the index walk:
    /// checks every slot in the market, in creation order, with the direct check
    /// `equity < MM` at the new mark. For each crossed slot it finds the key by binary
    /// search, not with the closed form of 9.1. Then it sorts, and liquidates in the walk's
    /// order: the same slots in the same order, derived without the index or its formula.
    ///
    /// The search is bounded by the two marks: every slot was safe at the previous mark (I5
    /// at the last command boundary), so a long's key lies between the new mark and one tick
    /// below the old one, a short's between one tick above the old mark and the new one.
    /// `liquidation_key_by_search` asserts that. With no previous mark there can be no
    /// position (positions need a mark), which this asserts.
    fn liquidate_by_scanning_every_slot(
        &mut self,
        market_id: MarketId,
        previous_mark: Option<Price>,
        events: &mut impl EventSink,
    ) {
        let market = self.market(market_id);
        let mark = market.mark.expect("SetMark has just set the mark");
        let max_leverage = market.params.max_leverage;
        // `Reverse` so that sorting puts the highest long key first, as the walk does.
        let mut longs: Vec<(Reverse<Price>, AccountId)> = Vec::new();
        let mut shorts: Vec<(Price, AccountId)> = Vec::new();
        for &account in &market.accounts {
            let slot = market.slot(account);
            if slot.pos == 0 {
                continue;
            }
            let previous_mark = previous_mark.expect("a market with a position had a mark before this one");
            if !is_liquidatable(slot.pos, slot.cost, slot.locked, mark, max_leverage) {
                continue;
            }
            let key = liquidation_key_by_search(
                slot.pos,
                slot.cost,
                slot.locked,
                max_leverage,
                previous_mark,
                mark,
            );
            if slot.pos > 0 {
                longs.push((Reverse(key), account));
            } else {
                shorts.push((key, account));
            }
        }
        // Account ids are unique, so the order is fully determined; unstable sorts don't
        // allocate.
        longs.sort_unstable();
        shorts.sort_unstable();
        for (_, account) in longs {
            self.liquidate_slot(market_id, account, events);
        }
        for (_, account) in shorts {
            self.liquidate_slot(market_id, account, events);
        }
    }

    // -----------------------------------------------------------------------------------
    // The band sweep (RISK.md 5.4).

    /// Cancels every resting bid above the new `upper` edge, highest price first, then every
    /// ask below the new `lower` edge, lowest price first (oldest first within a price),
    /// each with `Cancelled { PriceBand }`. The scratch walk takes each order's remaining
    /// quantity off its owner's open total and puts the owner's slot in the pass, in the
    /// order of its first swept order; the pass then releases what the slot no longer needs
    /// and re-keys it.
    ///
    /// Runs after the walk, so a liquidated account's orders are cancelled as `Liquidation`,
    /// not `PriceBand`, and nothing is released from a slot about to be liquidated. If the
    /// mark fell, only bids can be out of band; if it rose, only asks.
    pub(super) fn sweep_out_of_band_orders(&mut self, market_id: MarketId, events: &mut impl EventSink) {
        let market = self.market(market_id);
        let (upper, lower) = (market.upper, market.lower);
        self.call_book(market_id, |book, scratch| {
            book.cancel_beyond(Side::Buy, upper, CancelReason::PriceBand, scratch);
            book.cancel_beyond(Side::Sell, lower, CancelReason::PriceBand, scratch);
        });
        self.apply_book_events(market_id, None, events);
    }
}
