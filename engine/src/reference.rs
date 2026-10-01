//! The reference order book: slow, simple, and obviously correct.
//!
//! **Contract.** This book *defines* the order book's behaviour. The fast book (Milestone 1)
//! is checked against it on random command sequences, and must emit identical events and
//! end in an identical state. Every rule below is therefore part of the specification;
//! changing one is a `docs/DECISIONS.md` change (D-008).
//!
//! **Rules.**
//! - Price-time priority: best price first; at the same price, the oldest order first.
//! - Every fill happens at the resting (maker) order's price.
//! - Self-trade prevention: if the incoming order would match a resting order from the same
//!   account, that resting order is cancelled (`Cancelled { SelfTrade }`) and matching
//!   continues.
//! - Post-only: rejected (`PostOnlyWouldCross`) if its price reaches the best opposite
//!   price, whoever owns that order.
//! - IOC: fills what it can; the rest is cancelled (`Cancelled { IocRemainder }`).
//! - Checks on a new order, in this order: quantity > 0 (`InvalidQty`), price within the
//!   market's range (`InvalidPrice`), id not already resting (`Duplicate`), then post-only.
//! - Modify (INFO.md section 4, "Modify semantics"). The new quantity is the order's new
//!   *total* size, including what has already filled (`command::ModifyOrder`):
//!   - unknown id → `UnknownOrder`; then the same quantity and price checks as a new order;
//!   - new size at or below what has filled: the order is removed
//!     (`Cancelled { SizeBelowFilled }`, with the quantity that was still resting);
//!   - same price and a smaller or equal size: updated in place, keeps its priority;
//!   - otherwise it is a cancel-and-replace under the same id: it goes to the back of the
//!     queue and can match like a new order, and keeps counting what it has filled. A
//!     post-only order that would now cross is rejected, and the original stays untouched.
//! - Cancel of an id that isn't resting → `UnknownOrder`.
//! - Cancelling an account's orders removes them oldest first, by when each started
//!   resting (a cancel-and-replace counts as starting again).
//! - Cancelling beyond a limit removes every bid priced above it (or every ask priced below
//!   it), best price first, oldest first within a price; an order exactly at the limit
//!   stays.
//! - Every `Fill` carries the taker's side and every `Cancelled` the cancelled order's side.
//!
//! **Complexity.** Everything is a linear scan over all resting orders: O(n) per best-price
//! lookup, so matching is O(n) per fill. Fine for tests; never used on the hot path.

use std::cmp::Reverse;

use crate::book::{BookConfig, BookOptions, BookSnapshot, OrderBook, RestingOrder};
use crate::command::PlaceOrder;
use crate::event::{Ack, CancelReason, Cancelled, Event, EventSink, Fill, Modified, Reject, RejectReason};
use crate::types::{AccountId, OrderId, Price, Qty, Side, TimeInForce, account_of};

/// A resting order, plus the arrival number that sets its time priority.
#[derive(Clone, Copy, Debug)]
struct Entry {
    order_id: OrderId,
    side: Side,
    price: Price,
    /// Remaining quantity.
    qty: Qty,
    /// Quantity already filled, kept across cancel-and-replace modifies.
    filled: Qty,
    post_only: bool,
    /// Lower is older. Assigned when the order starts resting, and again when a modify
    /// sends it to the back of the queue.
    arrival: u64,
}

impl Entry {
    fn resting_order(&self) -> RestingOrder {
        RestingOrder {
            order_id: self.order_id,
            side: self.side,
            price: self.price,
            qty: self.qty,
            filled: self.filled,
            post_only: self.post_only,
        }
    }

    /// Sorts `entries` of one side into priority order: best price first (highest bid,
    /// lowest ask), then oldest first. `Reverse` rather than `-price`, which would overflow
    /// for a bid at `Price::MIN`.
    fn sort_by_priority(side: Side, entries: &mut [Entry]) {
        match side {
            Side::Buy => entries.sort_by_key(|e| (Reverse(e.price), e.arrival)),
            Side::Sell => entries.sort_by_key(|e| (e.price, e.arrival)),
        }
    }

    fn cancelled(&self, reason: CancelReason, market: u16) -> Event {
        Event::Cancelled(Cancelled {
            order_id: self.order_id,
            remaining: self.qty,
            market,
            reason,
            side: self.side,
        })
    }
}

/// See the module docs.
#[derive(Clone, Debug)]
pub struct ReferenceBook {
    config: BookConfig,
    /// All resting orders, in no particular order.
    entries: Vec<Entry>,
    next_arrival: u64,
}

impl ReferenceBook {
    pub fn new(config: BookConfig) -> Self {
        ReferenceBook { config, entries: Vec::new(), next_arrival: 0 }
    }

    fn position_of(&self, order_id: OrderId) -> Option<usize> {
        self.entries.iter().position(|e| e.order_id == order_id)
    }

    /// Index of the best resting order on `side`, if any.
    fn best_on(&self, side: Side) -> Option<usize> {
        let candidates = self.entries.iter().enumerate().filter(|(_, e)| e.side == side);
        match side {
            // Highest bid wins; ties go to the oldest. `Reverse` rather than `-price`, which
            // would overflow for a bid at `Price::MIN`.
            Side::Buy => candidates.min_by_key(|(_, e)| (Reverse(e.price), e.arrival)).map(|(i, _)| i),
            // Lowest ask wins; ties go to the oldest.
            Side::Sell => candidates.min_by_key(|(_, e)| (e.price, e.arrival)).map(|(i, _)| i),
        }
    }

    /// Would an order on `side` at `price` trade against the current best opposite order?
    fn would_cross(&self, side: Side, price: Price) -> bool {
        match side {
            Side::Buy => self.best_ask().is_some_and(|ask| price >= ask),
            Side::Sell => self.best_bid().is_some_and(|bid| price <= bid),
        }
    }

    fn price_in_range(&self, price: Price) -> bool {
        (self.config.min_price..=self.config.max_price).contains(&price)
    }

    fn reject(&self, order_id: OrderId, reason: RejectReason, events: &mut impl EventSink) {
        events.emit(Event::Reject(Reject { order_id, account: account_of(order_id), reason }));
    }

    /// Matches an incoming order against the opposite side until it is filled or no longer
    /// crosses. Returns the quantity left over.
    fn match_incoming(
        &mut self,
        taker: OrderId,
        side: Side,
        limit: Price,
        mut remaining: Qty,
        events: &mut impl EventSink,
    ) -> Qty {
        while remaining > 0 {
            let Some(i) = self.best_on(side.opposite()) else { break };
            let maker = self.entries[i];
            let crosses = match side {
                Side::Buy => maker.price <= limit,
                Side::Sell => maker.price >= limit,
            };
            if !crosses {
                break;
            }

            if account_of(maker.order_id) == account_of(taker) {
                // Self-trade prevention: drop our own resting order and keep going.
                self.entries.remove(i);
                events.emit(maker.cancelled(CancelReason::SelfTrade, self.config.market));
                continue;
            }

            let qty = remaining.min(maker.qty);
            events.emit(Event::Fill(Fill {
                maker_order: maker.order_id,
                taker_order: taker,
                price: maker.price,
                qty,
                maker_fee: 0,
                taker_fee: 0,
                market: self.config.market,
                taker_side: side,
            }));
            remaining -= qty;
            if qty == maker.qty {
                self.entries.remove(i);
            } else {
                self.entries[i].qty -= qty;
                self.entries[i].filled += qty;
            }
        }
        remaining
    }

    fn rest(&mut self, order_id: OrderId, side: Side, price: Price, qty: Qty, filled: Qty, post_only: bool) {
        let arrival = self.next_arrival;
        self.next_arrival += 1;
        self.entries.push(Entry { order_id, side, price, qty, filled, post_only, arrival });
    }

    fn sorted_side(&self, side: Side) -> Vec<RestingOrder> {
        let mut side_entries: Vec<Entry> = self.entries.iter().copied().filter(|e| e.side == side).collect();
        Entry::sort_by_priority(side, &mut side_entries);
        side_entries.iter().map(Entry::resting_order).collect()
    }
}

impl OrderBook for ReferenceBook {
    /// The options only tune the fast book's memory and speed, so this book ignores them.
    fn with_config(config: BookConfig, _options: BookOptions) -> Self {
        ReferenceBook::new(config)
    }

    fn place(&mut self, order: &PlaceOrder, events: &mut impl EventSink) {
        let id = order.order_id;
        if order.qty <= 0 {
            return self.reject(id, RejectReason::InvalidQty, events);
        }
        if !self.price_in_range(order.price) {
            return self.reject(id, RejectReason::InvalidPrice, events);
        }
        if self.position_of(id).is_some() {
            return self.reject(id, RejectReason::Duplicate, events);
        }
        if order.post_only && self.would_cross(order.side, order.price) {
            return self.reject(id, RejectReason::PostOnlyWouldCross, events);
        }

        events.emit(Event::Ack(Ack { order_id: id }));
        let remaining = self.match_incoming(id, order.side, order.price, order.qty, events);
        if remaining > 0 {
            match order.tif {
                TimeInForce::Gtc => {
                    self.rest(id, order.side, order.price, remaining, order.qty - remaining, order.post_only)
                }
                TimeInForce::Ioc => events.emit(Event::Cancelled(Cancelled {
                    order_id: id,
                    remaining,
                    market: self.config.market,
                    reason: CancelReason::IocRemainder,
                    side: order.side,
                })),
            }
        }
    }

    fn cancel(&mut self, order_id: OrderId, events: &mut impl EventSink) {
        let Some(i) = self.position_of(order_id) else {
            return self.reject(order_id, RejectReason::UnknownOrder, events);
        };
        let entry = self.entries.remove(i);
        events.emit(entry.cancelled(CancelReason::UserRequested, self.config.market));
    }

    fn modify(&mut self, order_id: OrderId, new_price: Price, new_size: Qty, events: &mut impl EventSink) {
        let Some(i) = self.position_of(order_id) else {
            return self.reject(order_id, RejectReason::UnknownOrder, events);
        };
        if new_size <= 0 {
            return self.reject(order_id, RejectReason::InvalidQty, events);
        }
        if !self.price_in_range(new_price) {
            return self.reject(order_id, RejectReason::InvalidPrice, events);
        }

        let entry = self.entries[i];
        let new_remaining = new_size - entry.filled;

        // Sized at or below what already filled (typically a modify signed before a fill):
        // nothing is left to fill.
        if new_remaining <= 0 {
            self.entries.remove(i);
            events.emit(entry.cancelled(CancelReason::SizeBelowFilled, self.config.market));
            return;
        }

        let modified = Event::Modified(Modified {
            order_id,
            price: new_price,
            qty: new_remaining,
            market: self.config.market,
        });

        // Shrinking in place keeps the order's place in the queue.
        if new_price == entry.price && new_remaining <= entry.qty {
            self.entries[i].qty = new_remaining;
            events.emit(modified);
            return;
        }

        // Anything else is cancel-and-replace: back of the queue, and it may trade.
        if entry.post_only && self.would_cross(entry.side, new_price) {
            return self.reject(order_id, RejectReason::PostOnlyWouldCross, events);
        }
        self.entries.remove(i);
        events.emit(modified);
        let left = self.match_incoming(order_id, entry.side, new_price, new_remaining, events);
        if left > 0 {
            let filled = entry.filled + (new_remaining - left);
            self.rest(order_id, entry.side, new_price, left, filled, entry.post_only);
        }
    }

    fn cancel_account(&mut self, account: AccountId, reason: CancelReason, events: &mut impl EventSink) {
        let mut own: Vec<Entry> =
            self.entries.iter().copied().filter(|e| account_of(e.order_id) == account).collect();
        own.sort_by_key(|e| e.arrival);
        self.entries.retain(|e| account_of(e.order_id) != account);
        for e in own {
            events.emit(e.cancelled(reason, self.config.market));
        }
    }

    fn cancel_beyond(&mut self, side: Side, limit: Price, reason: CancelReason, events: &mut impl EventSink) {
        let is_beyond = |e: &Entry| {
            e.side == side
                && match side {
                    Side::Buy => e.price > limit,
                    Side::Sell => e.price < limit,
                }
        };
        let mut beyond: Vec<Entry> = self.entries.iter().copied().filter(is_beyond).collect();
        Entry::sort_by_priority(side, &mut beyond);
        self.entries.retain(|e| !is_beyond(e));
        for e in beyond {
            events.emit(e.cancelled(reason, self.config.market));
        }
    }

    fn order(&self, order_id: OrderId) -> Option<RestingOrder> {
        self.position_of(order_id).map(|i| self.entries[i].resting_order())
    }

    fn open_quantities(&self, account: AccountId) -> (Qty, Qty) {
        let own = || self.entries.iter().filter(move |e| account_of(e.order_id) == account);
        let buys = own().filter(|e| e.side == Side::Buy).map(|e| e.qty).sum();
        let sells = own().filter(|e| e.side == Side::Sell).map(|e| e.qty).sum();
        (buys, sells)
    }

    fn best_bid(&self) -> Option<Price> {
        self.best_on(Side::Buy).map(|i| self.entries[i].price)
    }

    fn best_ask(&self) -> Option<Price> {
        self.best_on(Side::Sell).map(|i| self.entries[i].price)
    }

    fn snapshot(&self) -> BookSnapshot {
        BookSnapshot { bids: self.sorted_side(Side::Buy), asks: self.sorted_side(Side::Sell) }
    }
}

#[cfg(test)]
mod tests {
    //! Hand-written scenarios, one per rule in the module docs. These are the readable
    //! version of the specification; the property tests cover the combinations.
    use super::*;
    use crate::types::order_id;

    const MARKET: u16 = 1;
    const ALICE: u32 = 1;
    const BOB: u32 = 2;
    const CAROL: u32 = 3;

    fn book() -> ReferenceBook {
        ReferenceBook::new(BookConfig { market: MARKET, min_price: 1, max_price: 1_000 })
    }

    fn order(account: u32, seq: u32, side: Side, price: Price, qty: Qty) -> PlaceOrder {
        PlaceOrder {
            order_id: order_id(account, seq),
            price,
            qty,
            market: MARKET,
            side,
            tif: TimeInForce::Gtc,
            post_only: false,
        }
    }

    fn place(book: &mut ReferenceBook, order: PlaceOrder) -> Vec<Event> {
        let mut events = Vec::new();
        book.place(&order, &mut events);
        events
    }

    fn ack(id: OrderId) -> Event {
        Event::Ack(Ack { order_id: id })
    }

    /// A fill of `taker`'s order, whose side is `taker_side`, against `maker`'s.
    fn fill(maker: OrderId, taker: &PlaceOrder, price: Price, qty: Qty) -> Event {
        Event::Fill(Fill {
            maker_order: maker,
            taker_order: taker.order_id,
            price,
            qty,
            maker_fee: 0,
            taker_fee: 0,
            market: MARKET,
            taker_side: taker.side,
        })
    }

    fn cancelled(o: &PlaceOrder, remaining: Qty, reason: CancelReason) -> Event {
        Event::Cancelled(Cancelled { order_id: o.order_id, remaining, market: MARKET, reason, side: o.side })
    }

    fn resting(o: &PlaceOrder, qty: Qty, filled: Qty) -> RestingOrder {
        RestingOrder {
            order_id: o.order_id,
            side: o.side,
            price: o.price,
            qty,
            filled,
            post_only: o.post_only,
        }
    }

    fn reject(id: OrderId, reason: RejectReason) -> Event {
        Event::Reject(Reject { order_id: id, account: account_of(id), reason })
    }

    fn resting_ids(book: &ReferenceBook) -> (Vec<OrderId>, Vec<OrderId>) {
        let s = book.snapshot();
        (s.bids.iter().map(|o| o.order_id).collect(), s.asks.iter().map(|o| o.order_id).collect())
    }

    #[test]
    fn a_crossing_order_fills_at_the_makers_price() {
        let mut b = book();
        let ask = order(ALICE, 1, Side::Sell, 100, 5);
        let buy = order(BOB, 1, Side::Buy, 105, 5);
        place(&mut b, ask);
        assert_eq!(place(&mut b, buy), vec![ack(buy.order_id), fill(ask.order_id, &buy, 100, 5)]);
        assert_eq!(b.snapshot(), BookSnapshot::default());
    }

    #[test]
    fn best_price_first_then_oldest_first() {
        let mut b = book();
        let old_101 = order(ALICE, 1, Side::Sell, 101, 1);
        // The older order at 100 has the *larger* id, so only arrival order can explain
        // why it fills first.
        let new_100 = order(BOB, 1, Side::Sell, 100, 1);
        let newer_100 = order(ALICE, 2, Side::Sell, 100, 1);
        for o in [old_101, new_100, newer_100] {
            place(&mut b, o);
        }
        let buy = order(CAROL, 1, Side::Buy, 101, 3);
        assert_eq!(
            place(&mut b, buy),
            vec![
                ack(buy.order_id),
                fill(new_100.order_id, &buy, 100, 1),
                fill(newer_100.order_id, &buy, 100, 1),
                fill(old_101.order_id, &buy, 101, 1),
            ]
        );
    }

    #[test]
    fn a_partial_fill_leaves_the_gtc_remainder_resting() {
        let mut b = book();
        place(&mut b, order(ALICE, 1, Side::Sell, 100, 2));
        let buy = order(BOB, 1, Side::Buy, 100, 5);
        place(&mut b, buy);
        let s = b.snapshot();
        assert_eq!(
            s.bids,
            vec![RestingOrder {
                order_id: buy.order_id,
                side: Side::Buy,
                price: 100,
                qty: 3,
                filled: 2,
                post_only: false
            }]
        );
        assert!(s.asks.is_empty());
    }

    #[test]
    fn ioc_cancels_whatever_does_not_fill() {
        let mut b = book();
        let ask = order(ALICE, 1, Side::Sell, 100, 2);
        place(&mut b, ask);
        let buy = PlaceOrder { tif: TimeInForce::Ioc, ..order(BOB, 1, Side::Buy, 100, 5) };
        assert_eq!(
            place(&mut b, buy),
            vec![
                ack(buy.order_id),
                fill(ask.order_id, &buy, 100, 2),
                cancelled(&buy, 3, CancelReason::IocRemainder),
            ]
        );
        assert_eq!(b.snapshot(), BookSnapshot::default());
    }

    #[test]
    fn post_only_is_rejected_if_it_would_trade_and_rests_otherwise() {
        let mut b = book();
        place(&mut b, order(ALICE, 1, Side::Sell, 100, 1));
        let crossing = PlaceOrder { post_only: true, ..order(BOB, 1, Side::Buy, 100, 1) };
        assert_eq!(
            place(&mut b, crossing),
            vec![reject(crossing.order_id, RejectReason::PostOnlyWouldCross)]
        );
        let passive = PlaceOrder { post_only: true, ..order(BOB, 2, Side::Buy, 99, 1) };
        assert_eq!(place(&mut b, passive), vec![ack(passive.order_id)]);
        assert_eq!(
            b.snapshot().bids,
            vec![RestingOrder {
                order_id: passive.order_id,
                side: Side::Buy,
                price: 99,
                qty: 1,
                filled: 0,
                post_only: true
            }]
        );
    }

    #[test]
    fn post_only_is_rejected_even_against_the_accounts_own_order() {
        let mut b = book();
        let own_ask = order(ALICE, 1, Side::Sell, 100, 1);
        place(&mut b, own_ask);
        let crossing = PlaceOrder { post_only: true, ..order(ALICE, 2, Side::Buy, 100, 1) };
        assert_eq!(
            place(&mut b, crossing),
            vec![reject(crossing.order_id, RejectReason::PostOnlyWouldCross)]
        );
        assert_eq!(resting_ids(&b).1, vec![own_ask.order_id], "the own ask is not cancelled");
    }

    #[test]
    fn orders_rest_and_match_at_the_edges_of_the_price_range() {
        let mut b = book();
        let lowest = order(ALICE, 1, Side::Buy, 1, 1);
        let highest = order(ALICE, 2, Side::Sell, 1_000, 1);
        assert_eq!(place(&mut b, lowest), vec![ack(lowest.order_id)]);
        assert_eq!(place(&mut b, highest), vec![ack(highest.order_id)]);
        let sell = order(BOB, 1, Side::Sell, 1, 1);
        assert_eq!(place(&mut b, sell), vec![ack(sell.order_id), fill(lowest.order_id, &sell, 1, 1)]);
        let buy = order(BOB, 2, Side::Buy, 1_000, 1);
        assert_eq!(place(&mut b, buy), vec![ack(buy.order_id), fill(highest.order_id, &buy, 1_000, 1)]);
        assert_eq!(
            place(&mut b, order(BOB, 3, Side::Buy, 0, 1)),
            vec![reject(order_id(BOB, 3), RejectReason::InvalidPrice)]
        );
        assert_eq!(
            place(&mut b, order(BOB, 4, Side::Sell, 1_001, 1)),
            vec![reject(order_id(BOB, 4), RejectReason::InvalidPrice)]
        );
    }

    #[test]
    fn self_trade_cancels_the_resting_order_and_keeps_matching() {
        let mut b = book();
        let own_ask = order(ALICE, 1, Side::Sell, 100, 1);
        let bobs_ask = order(BOB, 1, Side::Sell, 101, 1);
        place(&mut b, own_ask);
        place(&mut b, bobs_ask);
        let buy = order(ALICE, 2, Side::Buy, 101, 1);
        assert_eq!(
            place(&mut b, buy),
            vec![
                ack(buy.order_id),
                cancelled(&own_ask, 1, CancelReason::SelfTrade),
                fill(bobs_ask.order_id, &buy, 101, 1),
            ]
        );
    }

    #[test]
    fn invalid_orders_are_rejected_in_a_fixed_order() {
        // The order is: quantity, price, duplicate id, post-only.
        let mut b = book();
        let o = order(ALICE, 1, Side::Buy, 100, 1);
        let bad_both = PlaceOrder { qty: 0, price: 5_000, ..o };
        assert_eq!(place(&mut b, bad_both), vec![reject(o.order_id, RejectReason::InvalidQty)]);
        let bad_price = PlaceOrder { price: 5_000, ..o };
        assert_eq!(place(&mut b, bad_price), vec![reject(o.order_id, RejectReason::InvalidPrice)]);

        place(&mut b, o);
        place(&mut b, order(BOB, 1, Side::Sell, 101, 1));
        // A resting id with a bad price: price is checked before the duplicate check.
        assert_eq!(place(&mut b, bad_price), vec![reject(o.order_id, RejectReason::InvalidPrice)]);
        // A resting id that is also a crossing post-only: duplicate is checked first.
        let dup_crossing = PlaceOrder { post_only: true, price: 101, ..o };
        assert_eq!(place(&mut b, dup_crossing), vec![reject(o.order_id, RejectReason::Duplicate)]);
    }

    #[test]
    fn a_used_id_is_accepted_again_once_it_no_longer_rests() {
        // The book only rejects ids that are resting right now. Refusing any reuse of an
        // account's sequence number is the engine's job, before the book (D-008).
        let mut b = book();
        let o = order(ALICE, 1, Side::Buy, 100, 1);
        place(&mut b, o);
        let mut events = Vec::new();
        b.cancel(o.order_id, &mut events);
        assert_eq!(place(&mut b, o), vec![ack(o.order_id)]);
    }

    #[test]
    fn modify_checks_the_id_then_quantity_then_price() {
        let mut b = book();
        let o = order(ALICE, 1, Side::Buy, 100, 5);
        let mut events = Vec::new();
        b.modify(o.order_id, 5_000, 0, &mut events);
        assert_eq!(events, vec![reject(o.order_id, RejectReason::UnknownOrder)]);

        place(&mut b, o);
        let before = b.snapshot();
        let mut events = Vec::new();
        b.modify(o.order_id, 5_000, 0, &mut events);
        b.modify(o.order_id, 5_000, 3, &mut events);
        assert_eq!(
            events,
            vec![
                reject(o.order_id, RejectReason::InvalidQty),
                reject(o.order_id, RejectReason::InvalidPrice)
            ]
        );
        assert_eq!(b.snapshot(), before, "a rejected modify leaves the order untouched");
    }

    #[test]
    fn cancel_removes_a_resting_order_and_rejects_an_unknown_one() {
        let mut b = book();
        let o = order(ALICE, 1, Side::Buy, 100, 4);
        place(&mut b, o);
        let mut events = Vec::new();
        b.cancel(o.order_id, &mut events);
        b.cancel(o.order_id, &mut events);
        assert_eq!(
            events,
            vec![
                cancelled(&o, 4, CancelReason::UserRequested),
                reject(o.order_id, RejectReason::UnknownOrder)
            ]
        );
    }

    #[test]
    fn shrinking_in_place_keeps_priority() {
        let mut b = book();
        let first = order(ALICE, 1, Side::Buy, 100, 5);
        let second = order(BOB, 1, Side::Buy, 100, 5);
        place(&mut b, first);
        place(&mut b, second);
        let mut events = Vec::new();
        b.modify(first.order_id, 100, 2, &mut events);
        assert_eq!(resting_ids(&b).0, vec![first.order_id, second.order_id]);
        assert_eq!(b.snapshot().bids[0].qty, 2);
    }

    #[test]
    fn a_size_increase_goes_to_the_back_of_the_queue() {
        let mut b = book();
        let first = order(ALICE, 1, Side::Buy, 100, 5);
        let second = order(BOB, 1, Side::Buy, 100, 5);
        place(&mut b, first);
        place(&mut b, second);
        let mut events = Vec::new();
        b.modify(first.order_id, 100, 6, &mut events);
        assert_eq!(resting_ids(&b).0, vec![second.order_id, first.order_id]);
    }

    #[test]
    fn a_price_change_goes_to_the_back_of_the_new_level() {
        let mut b = book();
        let first = order(ALICE, 1, Side::Buy, 100, 5);
        let second = order(BOB, 1, Side::Buy, 101, 5);
        place(&mut b, first);
        place(&mut b, second);
        let mut events = Vec::new();
        b.modify(first.order_id, 101, 5, &mut events);
        assert_eq!(resting_ids(&b).0, vec![second.order_id, first.order_id]);
    }

    #[test]
    fn a_modify_that_crosses_trades_like_a_new_order() {
        let mut b = book();
        let ask = order(ALICE, 1, Side::Sell, 102, 3);
        let bid = order(BOB, 1, Side::Buy, 100, 5);
        place(&mut b, ask);
        place(&mut b, bid);
        let mut events = Vec::new();
        b.modify(bid.order_id, 102, 5, &mut events);
        assert_eq!(
            events,
            vec![
                Event::Modified(Modified { order_id: bid.order_id, price: 102, qty: 5, market: MARKET }),
                fill(ask.order_id, &bid, 102, 3),
            ]
        );
        assert_eq!(
            b.snapshot().bids,
            vec![RestingOrder {
                order_id: bid.order_id,
                side: Side::Buy,
                price: 102,
                qty: 2,
                filled: 3,
                post_only: false
            }]
        );
    }

    #[test]
    fn a_modify_sets_the_total_size_so_what_is_left_counts_the_fills() {
        // Buy 10, 4 fill, then "modify to 8": 8 - 4 = 4 left, a decrease from 6, so it
        // keeps its place.
        let mut b = book();
        let bid = order(ALICE, 1, Side::Buy, 100, 10);
        let behind = order(CAROL, 1, Side::Buy, 100, 1);
        place(&mut b, bid);
        place(&mut b, behind);
        place(&mut b, order(BOB, 1, Side::Sell, 100, 4));
        let mut events = Vec::new();
        b.modify(bid.order_id, 100, 8, &mut events);
        assert_eq!(
            events,
            vec![Event::Modified(Modified { order_id: bid.order_id, price: 100, qty: 4, market: MARKET })]
        );
        assert_eq!(resting_ids(&b).0, vec![bid.order_id, behind.order_id]);
        assert_eq!(b.snapshot().bids[0].filled, 4);
    }

    #[test]
    fn a_stale_modify_at_or_below_the_filled_quantity_removes_the_order() {
        // The client signs "modify to 5" on a buy of 10; before it arrives, 7 fill. The
        // order is removed rather than growing back to 5 more.
        let mut b = book();
        let bid = order(ALICE, 1, Side::Buy, 100, 10);
        place(&mut b, bid);
        place(&mut b, order(BOB, 1, Side::Sell, 100, 7));
        let mut events = Vec::new();
        b.modify(bid.order_id, 100, 5, &mut events);
        assert_eq!(events, vec![cancelled(&bid, 3, CancelReason::SizeBelowFilled)]);
        assert!(b.snapshot().bids.is_empty());

        // Exactly the filled quantity counts as nothing left, too.
        let bid = order(ALICE, 2, Side::Buy, 100, 10);
        place(&mut b, bid);
        place(&mut b, order(BOB, 2, Side::Sell, 100, 7));
        let mut events = Vec::new();
        b.modify(bid.order_id, 100, 7, &mut events);
        assert_eq!(events, vec![cancelled(&bid, 3, CancelReason::SizeBelowFilled)]);
    }

    #[test]
    fn a_requeued_order_keeps_counting_its_fills() {
        // Buy 5 at 100, 2 fill; modify to 6 at 101: 4 left, back of the queue, filled
        // still 2, and it matches an ask at 101 like a new order would.
        let mut b = book();
        let bid = order(ALICE, 1, Side::Buy, 100, 5);
        place(&mut b, bid);
        place(&mut b, order(BOB, 1, Side::Sell, 100, 2));
        let ask = order(BOB, 2, Side::Sell, 101, 1);
        place(&mut b, ask);
        let mut events = Vec::new();
        b.modify(bid.order_id, 101, 6, &mut events);
        assert_eq!(
            events,
            vec![
                Event::Modified(Modified { order_id: bid.order_id, price: 101, qty: 4, market: MARKET }),
                fill(ask.order_id, &bid, 101, 1),
            ]
        );
        assert_eq!(
            b.snapshot().bids,
            vec![RestingOrder {
                order_id: bid.order_id,
                side: Side::Buy,
                price: 101,
                qty: 3,
                filled: 3,
                post_only: false
            }]
        );
    }

    #[test]
    fn cancel_account_removes_only_that_accounts_orders_oldest_first() {
        let mut b = book();
        let a1 = order(ALICE, 1, Side::Buy, 100, 1);
        let a2 = order(ALICE, 2, Side::Sell, 105, 2);
        let bob = order(BOB, 1, Side::Buy, 99, 1);
        let a3 = order(ALICE, 3, Side::Buy, 98, 3);
        for o in [a1, a2, bob, a3] {
            place(&mut b, o);
        }
        // A cancel-and-replace starts resting again, so a1 now counts as the newest.
        let mut events = Vec::new();
        b.modify(a1.order_id, 97, 1, &mut events);
        let mut events = Vec::new();
        b.cancel_account(ALICE, CancelReason::Liquidation, &mut events);
        assert_eq!(
            events,
            vec![
                cancelled(&a2, 2, CancelReason::Liquidation),
                cancelled(&a3, 3, CancelReason::Liquidation),
                cancelled(&a1, 1, CancelReason::Liquidation),
            ]
        );
        assert_eq!(resting_ids(&b), (vec![bob.order_id], vec![]));
        let mut events = Vec::new();
        b.cancel_account(ALICE, CancelReason::Liquidation, &mut events);
        assert!(events.is_empty(), "an account with no orders is a no-op");
    }

    #[test]
    fn cancel_beyond_removes_bids_above_the_limit_best_price_first_then_oldest_first() {
        let mut b = book();
        let older_103 = order(ALICE, 1, Side::Buy, 103, 1);
        let newer_103 = order(BOB, 1, Side::Buy, 103, 2);
        let at_102 = order(CAROL, 1, Side::Buy, 102, 3);
        let at_101 = order(ALICE, 2, Side::Buy, 101, 4);
        let ask = order(BOB, 2, Side::Sell, 110, 5);
        // Placed out of priority order, so only the sort can explain the events.
        for o in [at_102, older_103, at_101, newer_103, ask] {
            place(&mut b, o);
        }
        let mut events = Vec::new();
        b.cancel_beyond(Side::Buy, 101, CancelReason::PriceBand, &mut events);
        assert_eq!(
            events,
            vec![
                cancelled(&older_103, 1, CancelReason::PriceBand),
                cancelled(&newer_103, 2, CancelReason::PriceBand),
                cancelled(&at_102, 3, CancelReason::PriceBand),
            ]
        );
        assert_eq!(resting_ids(&b), (vec![at_101.order_id], vec![ask.order_id]), "101 is at the limit");
    }

    #[test]
    fn cancel_beyond_removes_asks_below_the_limit_and_leaves_bids_alone() {
        let mut b = book();
        let bid = order(ALICE, 1, Side::Buy, 100, 1);
        let at_105 = order(BOB, 1, Side::Sell, 105, 2);
        let at_104 = order(CAROL, 1, Side::Sell, 104, 3);
        for o in [bid, at_105, at_104] {
            place(&mut b, o);
        }
        let mut events = Vec::new();
        b.cancel_beyond(Side::Sell, 106, CancelReason::PriceBand, &mut events);
        assert_eq!(
            events,
            vec![
                cancelled(&at_104, 3, CancelReason::PriceBand),
                cancelled(&at_105, 2, CancelReason::PriceBand)
            ]
        );
        assert_eq!(resting_ids(&b), (vec![bid.order_id], vec![]));
    }

    #[test]
    fn order_shows_one_resting_order_with_its_side_and_fills() {
        let mut b = book();
        let ask = order(ALICE, 1, Side::Sell, 100, 5);
        place(&mut b, ask);
        place(&mut b, order(BOB, 1, Side::Buy, 100, 2));
        assert_eq!(b.order(ask.order_id), Some(resting(&ask, 3, 2)));
        assert_eq!(b.order(order_id(BOB, 1)), None, "filled in full, so not resting");
        assert_eq!(b.order(order_id(CAROL, 9)), None, "never placed");
    }

    #[test]
    fn open_quantities_sum_what_is_left_of_an_accounts_orders_by_side() {
        let mut b = book();
        for o in [
            order(ALICE, 1, Side::Buy, 100, 5),
            order(ALICE, 2, Side::Buy, 98, 1),
            order(ALICE, 3, Side::Sell, 105, 7),
            order(BOB, 1, Side::Sell, 106, 9),
        ] {
            place(&mut b, o);
        }
        place(&mut b, order(CAROL, 1, Side::Sell, 100, 4));
        assert_eq!(b.open_quantities(ALICE), (1 + 1, 7), "4 of the bid at 100 filled");
        assert_eq!(b.open_quantities(BOB), (0, 9));
        assert_eq!(b.open_quantities(CAROL), (0, 0));
    }

    #[test]
    fn a_post_only_modify_that_would_cross_is_rejected_and_the_original_stays() {
        let mut b = book();
        place(&mut b, order(ALICE, 1, Side::Sell, 102, 1));
        let bid = PlaceOrder { post_only: true, ..order(BOB, 1, Side::Buy, 100, 1) };
        place(&mut b, bid);
        let before = b.snapshot();
        let mut events = Vec::new();
        b.modify(bid.order_id, 102, 1, &mut events);
        assert_eq!(events, vec![reject(bid.order_id, RejectReason::PostOnlyWouldCross)]);
        assert_eq!(b.snapshot(), before);
    }
}
