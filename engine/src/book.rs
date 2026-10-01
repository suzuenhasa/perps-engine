//! The order book interface, and the production book.
//!
//! **Contract.** An [`OrderBook`] holds one market's resting limit orders and matches
//! incoming orders against them by price-time priority. It knows nothing about accounts'
//! balances or margin: risk checks happen before an order reaches the book (Milestone 2).
//! Given the same calls in the same order, every implementation must emit exactly the same
//! events and end in the same state as [`crate::reference::ReferenceBook`], which is the
//! executable definition of the semantics (INFO.md section 4, "Order book").
//!
//! **Invariant.** After every call returns, the book is not crossed: the best bid is
//! strictly below the best ask. In debug builds [`Book`] checks this after every change,
//! along with its cached best prices; [`Book::assert_consistent`] checks every link and
//! index, for tests.
//!
//! **Structure** (Milestone 1; INFO.md section 4, "Order book"). [`Book`] keeps:
//! - **Price levels in a tick-indexed array**, one per tick of the market's price range:
//!   the level for price `p` is `levels[p - min_price]`, so finding it is one subtraction.
//!   A level is the head and tail of a list of orders, oldest first. The book is never
//!   crossed at rest, so a level only ever holds orders of one side.
//! - **A level index per side** (`level_index.rs`): a bitmap of which levels hold bids (or
//!   asks), with summary layers above it, so the next non-empty level is found without
//!   scanning the levels in between.
//! - **The best bid and ask, cached.** A new order can only improve its side's best price;
//!   when the best level empties, the level index finds the next one.
//! - **Orders in a slab**: a `Vec` of fixed-size slots, reused through a free list, so
//!   resting an order doesn't allocate. Each slot is linked into its level's list and into
//!   its account's list, both doubly linked, so any order unlinks in O(1). The links are
//!   `u32` slot numbers rather than pointers: plain safe Rust, with no `unsafe` and no
//!   `Rc<RefCell<..>>`.
//! - **An id map** from order id to slot: a `HashMap` with a seeded hash (`id_hash.rs`
//!   explains why seeded).
//! - **A list per account** of its resting orders, in the order they started resting. That
//!   is the order `cancel_account` must cancel them in, so it needs no sort and no scan of
//!   the book. The map from account to list uses the same hash, and holds an account only
//!   while it has orders resting.
//!
//! Matching takes the same steps as the reference book. The difference is how a step finds
//! the best resting order: it is the head of the best opposite level, found in O(1), rather
//! than by a scan over every order.
//!
//! **Complexity** (`L` = ticks in the price range; the maps' O(1) is expected O(1)):
//! - `place`: O(1) to check and rest, plus O(1) for each fill or self-trade cancel.
//! - `cancel`: O(1).
//! - `modify`: O(1) in place; a cancel-and-replace costs a cancel plus a place.
//! - `cancel_account`: O(k) for the account's k resting orders.
//! - `cancel_beyond`: O(1 + orders cancelled): it cancels from the best price inward and
//!   stops at the first order inside the limit.
//! - `order`: O(1), through the id map.
//! - `open_quantities`: O(k) for the account's k resting orders, walking its list.
//! - `best_bid`, `best_ask`: O(1), cached.
//! - Any removal that empties the best level also searches the level index: O(log64 L), at
//!   most 4 layers.
//! - `snapshot`: O(orders + non-empty levels), and allocates; tests only.
//!
//! **Memory.** 8 bytes per tick of the price range for the levels, about 2 bits per tick
//! for the two level indexes, and for each order of reserved capacity a 56-byte slot plus
//! about 70 to 140 bytes of room in the two maps (they reserve twice the capacity, see
//! "Allocation", and round their tables up to a power of two). Ranges wider than
//! [`MAX_LEVELS`] ticks (2^24, so 128 MiB of levels) are refused: they would need a sorted
//! level structure instead (INFO.md section 4), which is left for later.
//!
//! **Allocation.** [`Book::with_options`] reserves room for `order_capacity` resting orders
//! up front. Up to that many resting at once, no call allocates: freed slots are reused,
//! and neither map ever has to grow. For the maps, that takes two things:
//! - An account's list leaves the account map with its last resting order. So that map
//!   never holds more entries than there are resting orders, however many different
//!   accounts come and go.
//! - Each map reserves room for *twice* `order_capacity` entries. std's `HashMap` leaves a
//!   marker (a "tombstone") where it removes an entry, and the markers use up its free
//!   room. When that runs out, a table at most half full clears the markers in place, but
//!   a table more than half full allocates a bigger table and moves every entry into it.
//!   At twice the capacity, the table is never more than half full. This is how std's
//!   table (hashbrown) works, not a documented promise, so `engine/tests/no_alloc.rs`
//!   checks it.
//!
//! Clearing the markers in place doesn't allocate, but it rehashes the whole table, so
//! now and then one call costs O(order_capacity) rather than O(1). Past `order_capacity`,
//! the slab and the maps grow like any `Vec` or `HashMap` (amortised O(1), but the call
//! that grows them allocates and copies), so size it for the expected peak.

use crate::command::PlaceOrder;
use crate::event::{Ack, CancelReason, Cancelled, Event, EventSink, Fill, Modified, Reject, RejectReason};
use crate::id_hash::{IdBuildHasher, IdMap};
use crate::level_index::LevelIndex;
use crate::types::{AccountId, MarketId, OrderId, Price, Qty, Side, TimeInForce, account_of};

/// Fixed parameters of one market's book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookConfig {
    pub market: MarketId,
    /// Orders priced outside `min_price..=max_price` are rejected with `InvalidPrice`.
    pub min_price: Price,
    pub max_price: Price,
}

/// One resting order, as seen from outside the book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestingOrder {
    pub order_id: OrderId,
    pub side: Side,
    pub price: Price,
    /// Remaining quantity.
    pub qty: Qty,
    /// Quantity already filled. The order's total size is `qty + filled`.
    pub filled: Qty,
    pub post_only: bool,
}

/// Every resting order, each side in priority order (best price first, then oldest first).
///
/// Used by tests to compare two books; building one allocates, so it is never called on
/// the hot path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BookSnapshot {
    pub bids: Vec<RestingOrder>,
    pub asks: Vec<RestingOrder>,
}

impl BookSnapshot {
    /// True if the best bid is at or above the best ask, which must never happen.
    pub fn is_crossed(&self) -> bool {
        match (self.bids.first(), self.asks.first()) {
            (Some(bid), Some(ask)) => bid.price >= ask.price,
            _ => false,
        }
    }
}

/// What every order book implementation provides. See the module docs for the contract.
pub trait OrderBook {
    /// An empty book. The engine builds one per market from `SetMarketParams`; `options`
    /// only tunes speed and memory, never behaviour.
    fn with_config(config: BookConfig, options: BookOptions) -> Self
    where
        Self: Sized;

    /// Accepts or rejects a new order, matches it, and rests any GTC remainder.
    fn place(&mut self, order: &PlaceOrder, events: &mut impl EventSink);

    /// Removes a resting order.
    fn cancel(&mut self, order_id: OrderId, events: &mut impl EventSink);

    /// Changes a resting order's price and/or total size (see `command::ModifyOrder`).
    fn modify(&mut self, order_id: OrderId, new_price: Price, new_size: Qty, events: &mut impl EventSink);

    /// Removes all of one account's resting orders, oldest first (the order in which they
    /// started resting), with a `Cancelled { reason }` for each. Liquidation uses this
    /// (Milestone 2).
    fn cancel_account(&mut self, account: AccountId, reason: CancelReason, events: &mut impl EventSink);

    /// Removes every bid priced above `limit` (for `side` Buy), or every ask priced below it
    /// (Sell), best price first and oldest first within a price, with a
    /// `Cancelled { reason }` for each. An order exactly at `limit` stays. The engine's
    /// price-band sweep uses this when the mark moves (`docs/RISK.md` 5.4).
    fn cancel_beyond(&mut self, side: Side, limit: Price, reason: CancelReason, events: &mut impl EventSink);

    /// One resting order, or `None` if no order with this id is resting. The engine uses it
    /// to check a cancel or modify before calling the book.
    fn order(&self, order_id: OrderId) -> Option<RestingOrder>;

    /// The remaining quantity of `account`'s resting buys and of its resting sells, in that
    /// order. Used only by the engine's naive modes, which recompute open totals instead of
    /// keeping them (`docs/RISK.md` 14.2).
    fn open_quantities(&self, account: AccountId) -> (Qty, Qty);

    fn best_bid(&self) -> Option<Price>;
    fn best_ask(&self) -> Option<Price>;

    /// All resting orders in priority order. Allocates; tests only.
    fn snapshot(&self) -> BookSnapshot;

    /// Writes the memory the book reserved once, so that its pages are resident before a
    /// run, and changes nothing the book holds or does (`Engine::prefault`). Does nothing
    /// by default.
    fn prefault(&mut self) {}
}

/// The widest price range [`Book`] accepts, in ticks: 2^24 levels, 128 MiB of levels.
pub const MAX_LEVELS: usize = 1 << 24;

/// A slot number meaning "none": the end of a list, or an empty level.
const NIL: u32 = u32::MAX;

/// Tuning for [`Book::with_options`]. Neither field changes what the book does, only how
/// fast it does it and how much memory it takes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BookOptions {
    /// Resting orders to reserve room for up front. Up to this many, no call allocates
    /// (module docs, "Allocation").
    pub order_capacity: usize,
    /// Seed for the order-id hash (`id_hash.rs`). Production uses a secret seed per
    /// deployment, recorded in the journal header (Milestone 3) so that replay rebuilds the
    /// same maps while clients can't predict them.
    pub id_hash_seed: u64,
}

/// Leaves the seed out, so logging the options can't leak it.
impl std::fmt::Debug for BookOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BookOptions").field("order_capacity", &self.order_capacity).finish_non_exhaustive()
    }
}

impl Default for BookOptions {
    /// For tests and benchmarks only: room for 1,024 orders, and hash seed 0. Everyone
    /// knows seed 0, so a client could compute order ids that all collide in the id map
    /// (hash flooding, `id_hash.rs`). A book that takes clients' order ids needs a secret
    /// seed.
    fn default() -> Self {
        BookOptions { order_capacity: 1024, id_hash_seed: 0 }
    }
}

/// One price level: a first-in, first-out list of resting orders, oldest at the head.
#[derive(Clone, Copy, Debug)]
struct Level {
    head: u32,
    tail: u32,
}

impl Level {
    const EMPTY: Level = Level { head: NIL, tail: NIL };
}

/// One account's resting orders in this market, oldest (first to start resting) at the head.
#[derive(Clone, Copy, Debug)]
struct AccountOrders {
    head: u32,
    tail: u32,
}

impl AccountOrders {
    const EMPTY: AccountOrders = AccountOrders { head: NIL, tail: NIL };
}

/// One slot of the slab: a resting order and its links. A free slot uses only `next`, to
/// link the free list; its other fields are left over from the order it last held.
#[derive(Clone, Copy, Debug)]
struct OrderSlot {
    order_id: OrderId,
    price: Price,
    /// Remaining quantity.
    qty: Qty,
    /// Quantity already filled, kept across cancel-and-replace modifies.
    filled: Qty,
    /// Neighbours in the price level's list: `prev` is older, `next` newer.
    prev: u32,
    next: u32,
    /// Neighbours in the account's list: `account_prev` started resting earlier.
    account_prev: u32,
    account_next: u32,
    side: Side,
    post_only: bool,
}

// The size the module docs quote.
const _: () = assert!(size_of::<OrderSlot>() == 56);

/// The production order book. See the module docs for how it is built and what each
/// operation costs.
#[derive(Clone, Debug)]
pub struct Book {
    config: BookConfig,
    /// One level per tick of the price range: `levels[price - min_price]`.
    levels: Vec<Level>,
    /// Which levels hold bids.
    bid_levels: LevelIndex,
    /// Which levels hold asks.
    ask_levels: LevelIndex,
    /// Cached best prices; `None` while that side is empty.
    best_bid: Option<Price>,
    best_ask: Option<Price>,
    /// The slab. Its length is the most orders that have rested at once: freed slots are
    /// reused before it grows.
    slots: Vec<OrderSlot>,
    /// The first free slot, or `NIL`. The rest of the free list is linked through
    /// `OrderSlot::next`.
    free_head: u32,
    /// The slot of every resting order.
    slot_of: IdMap<OrderId, u32>,
    /// The list of every account with orders resting here. An account's entry goes with
    /// its last resting order, so this never holds more entries than `slot_of`.
    account_orders: IdMap<AccountId, AccountOrders>,
}

impl Book {
    /// A book with the default [`BookOptions`]. For tests and benchmarks only: its hash
    /// seed is 0, which everyone knows (see [`BookOptions::default`]). Production uses
    /// [`Book::with_options`] with a secret seed.
    ///
    /// Panics if the price range is empty or wider than [`MAX_LEVELS`] ticks.
    pub fn new(config: BookConfig) -> Self {
        Book::with_options(config, BookOptions::default())
    }

    /// A book with room reserved for `options.order_capacity` resting orders.
    ///
    /// Panics if the price range is empty or wider than [`MAX_LEVELS`] ticks.
    pub fn with_options(config: BookConfig, options: BookOptions) -> Self {
        let (min, max) = (config.min_price, config.max_price);
        // In i128, so that even an absurd range can't overflow while being checked.
        let ticks = i128::from(max) - i128::from(min) + 1;
        assert!(ticks >= 1, "empty price range {min}..={max}");
        assert!(
            ticks <= MAX_LEVELS as i128,
            "price range {min}..={max} is {ticks} ticks wide, but the tick-indexed book takes at \
             most {MAX_LEVELS} (8 bytes per tick); a wider range needs a sorted level structure"
        );
        let level_count = ticks as usize;
        let hasher = IdBuildHasher::new(options.id_hash_seed);
        // Twice `order_capacity`, so that neither map ever grows while the book stays
        // within it (module docs, "Allocation").
        let map_capacity = options.order_capacity.saturating_mul(2);
        Book {
            config,
            levels: vec![Level::EMPTY; level_count],
            bid_levels: LevelIndex::new(level_count),
            ask_levels: LevelIndex::new(level_count),
            best_bid: None,
            best_ask: None,
            slots: Vec::with_capacity(options.order_capacity),
            free_head: NIL,
            slot_of: IdMap::with_capacity_and_hasher(map_capacity, hasher),
            account_orders: IdMap::with_capacity_and_hasher(map_capacity, hasher),
        }
    }

    fn price_in_range(&self, price: Price) -> bool {
        (self.config.min_price..=self.config.max_price).contains(&price)
    }

    /// The level number of a price that is in range.
    fn level_of(&self, price: Price) -> usize {
        (price - self.config.min_price) as usize
    }

    /// The price of a level number.
    fn price_of(&self, level: usize) -> Price {
        self.config.min_price + level as Price
    }

    fn best_price(&self, side: Side) -> Option<Price> {
        match side {
            Side::Buy => self.best_bid,
            Side::Sell => self.best_ask,
        }
    }

    fn best_price_mut(&mut self, side: Side) -> &mut Option<Price> {
        match side {
            Side::Buy => &mut self.best_bid,
            Side::Sell => &mut self.best_ask,
        }
    }

    fn level_index_mut(&mut self, side: Side) -> &mut LevelIndex {
        match side {
            Side::Buy => &mut self.bid_levels,
            Side::Sell => &mut self.ask_levels,
        }
    }

    /// The next level holding orders on `side`, moving away from the spread from `level`
    /// (not included): downwards for bids, upwards for asks.
    fn next_level_away(&self, side: Side, level: usize) -> Option<usize> {
        match side {
            Side::Buy => self.bid_levels.last_at_or_below(level.checked_sub(1)?),
            Side::Sell => self.ask_levels.first_at_or_above(level + 1),
        }
    }

    /// Would an order on `side` at `price` trade against the current best opposite order?
    fn would_cross(&self, side: Side, price: Price) -> bool {
        match side {
            Side::Buy => self.best_ask.is_some_and(|ask| price >= ask),
            Side::Sell => self.best_bid.is_some_and(|bid| price <= bid),
        }
    }

    fn reject(&self, order_id: OrderId, reason: RejectReason, events: &mut impl EventSink) {
        events.emit(Event::Reject(Reject { order_id, account: account_of(order_id), reason }));
    }

    /// Reports that `order`, which has just left the book, was cancelled with its remaining
    /// quantity.
    fn emit_cancelled(&self, order: &OrderSlot, reason: CancelReason, events: &mut impl EventSink) {
        events.emit(Event::Cancelled(Cancelled {
            order_id: order.order_id,
            remaining: order.qty,
            market: self.config.market,
            reason,
            side: order.side,
        }));
    }

    /// Matches an incoming order against the opposite side until it is filled or no longer
    /// crosses. Returns the quantity left over.
    ///
    /// The same loop as the reference book's, but each turn finds the best resting order in
    /// O(1): the head of the best opposite level.
    fn match_incoming(
        &mut self,
        taker: OrderId,
        side: Side,
        limit: Price,
        mut remaining: Qty,
        events: &mut impl EventSink,
    ) -> Qty {
        while remaining > 0 {
            let Some(best) = self.best_price(side.opposite()) else { break };
            let crosses = match side {
                Side::Buy => best <= limit,
                Side::Sell => best >= limit,
            };
            if !crosses {
                break;
            }
            let maker_slot = self.levels[self.level_of(best)].head;
            let maker = self.slots[maker_slot as usize];

            if account_of(maker.order_id) == account_of(taker) {
                // Self-trade prevention: drop our own resting order and keep going.
                self.remove(maker_slot);
                self.emit_cancelled(&maker, CancelReason::SelfTrade, events);
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
                self.remove(maker_slot);
            } else {
                // Partly filled: it stays at the head of its level.
                let maker = &mut self.slots[maker_slot as usize];
                maker.qty -= qty;
                maker.filled += qty;
            }
        }
        remaining
    }

    /// Puts an order on the book, behind every order already resting at its price and at
    /// the back of its account's list.
    fn rest(&mut self, order_id: OrderId, side: Side, price: Price, qty: Qty, filled: Qty, post_only: bool) {
        let slot = self.allocate_slot(OrderSlot {
            order_id,
            price,
            qty,
            filled,
            prev: NIL,
            next: NIL,
            account_prev: NIL,
            account_next: NIL,
            side,
            post_only,
        });
        self.slot_of.insert(order_id, slot);
        self.push_to_level(slot);
        self.push_to_account(slot);
    }

    /// Takes a resting order off the book completely, and returns what its slot held.
    fn remove(&mut self, slot: u32) -> OrderSlot {
        let order = self.slots[slot as usize];
        self.unlink_from_level(slot);
        self.unlink_from_account(slot);
        self.slot_of.remove(&order.order_id);
        self.free_slot(slot);
        order
    }

    /// Stores an order in a free slot if there is one, or in a new slot at the end of the
    /// slab, and returns the slot number.
    fn allocate_slot(&mut self, order: OrderSlot) -> u32 {
        if self.free_head != NIL {
            let slot = self.free_head;
            self.free_head = self.slots[slot as usize].next;
            self.slots[slot as usize] = order;
            slot
        } else {
            assert!(self.slots.len() < NIL as usize, "the book holds at most {NIL} orders");
            self.slots.push(order);
            (self.slots.len() - 1) as u32
        }
    }

    fn free_slot(&mut self, slot: u32) {
        self.slots[slot as usize].next = self.free_head;
        self.free_head = slot;
    }

    /// Appends a slot to the back of its price level. If the level was empty, marks it in
    /// the level index; if the price beats the side's best, it becomes the best.
    fn push_to_level(&mut self, slot: u32) {
        let OrderSlot { price, side, .. } = self.slots[slot as usize];
        let level = self.level_of(price);
        let tail = self.levels[level].tail;
        self.slots[slot as usize].prev = tail;
        self.slots[slot as usize].next = NIL;
        if tail == NIL {
            self.levels[level].head = slot;
            self.level_index_mut(side).set(level);
        } else {
            self.slots[tail as usize].next = slot;
        }
        self.levels[level].tail = slot;

        if self.best_price(side).is_none_or(|best| is_better(side, price, best)) {
            *self.best_price_mut(side) = Some(price);
        }
    }

    /// Takes a slot out of its price level. If that empties the level, unmarks it in the
    /// level index, and if it was the best level, finds the next best.
    fn unlink_from_level(&mut self, slot: u32) {
        let OrderSlot { price, side, prev, next, .. } = self.slots[slot as usize];
        let level = self.level_of(price);
        if prev == NIL {
            self.levels[level].head = next;
        } else {
            self.slots[prev as usize].next = next;
        }
        if next == NIL {
            self.levels[level].tail = prev;
        } else {
            self.slots[next as usize].prev = prev;
        }

        if self.levels[level].head == NIL {
            self.level_index_mut(side).clear(level);
            if self.best_price(side) == Some(price) {
                let next_best = self.next_level_away(side, level).map(|l| self.price_of(l));
                *self.best_price_mut(side) = next_best;
            }
        }
    }

    /// Appends a slot to the back of its account's list, creating the list if the account
    /// has no other order resting.
    fn push_to_account(&mut self, slot: u32) {
        let account = account_of(self.slots[slot as usize].order_id);
        let list = self.account_orders.entry(account).or_insert(AccountOrders::EMPTY);
        let tail = list.tail;
        if tail == NIL {
            list.head = slot;
        } else {
            self.slots[tail as usize].account_next = slot;
        }
        list.tail = slot;
        self.slots[slot as usize].account_prev = tail;
        self.slots[slot as usize].account_next = NIL;
    }

    /// Takes a slot out of its account's list. The account's last order takes the list
    /// with it, so the account map holds only accounts with orders resting.
    fn unlink_from_account(&mut self, slot: u32) {
        let OrderSlot { order_id, account_prev, account_next, .. } = self.slots[slot as usize];
        let account = account_of(order_id);
        if account_prev == NIL && account_next == NIL {
            self.account_orders.remove(&account);
            return;
        }
        let list = self.account_orders.get_mut(&account).expect("a resting order's account has a list");
        if account_prev == NIL {
            list.head = account_next;
        } else {
            self.slots[account_prev as usize].account_next = account_next;
        }
        if account_next == NIL {
            list.tail = account_prev;
        } else {
            self.slots[account_next as usize].account_prev = account_prev;
        }
    }

    /// The order in a slot, as seen from outside the book.
    fn resting_order(&self, slot: u32) -> RestingOrder {
        let o = &self.slots[slot as usize];
        RestingOrder {
            order_id: o.order_id,
            side: o.side,
            price: o.price,
            qty: o.qty,
            filled: o.filled,
            post_only: o.post_only,
        }
    }

    /// One side's resting orders in priority order, for [`OrderBook::snapshot`].
    fn resting_orders(&self, side: Side) -> Vec<RestingOrder> {
        let mut orders = Vec::new();
        let mut level = self.best_price(side).map(|price| self.level_of(price));
        while let Some(l) = level {
            let mut slot = self.levels[l].head;
            while slot != NIL {
                orders.push(self.resting_order(slot));
                slot = self.slots[slot as usize].next;
            }
            level = self.next_level_away(side, l);
        }
        orders
    }

    /// The best bid and ask according to the level indexes, rather than the cache: the
    /// highest level holding bids and the lowest holding asks. O(log64 L).
    fn best_prices_from_level_indexes(&self) -> (Option<Price>, Option<Price>) {
        let top = self.levels.len() - 1;
        let bid = self.bid_levels.last_at_or_below(top).map(|l| self.price_of(l));
        let ask = self.ask_levels.first_at_or_above(0).map(|l| self.price_of(l));
        (bid, ask)
    }

    /// The cheap invariant checks, run after every change in debug builds (release builds
    /// skip them): the book is not crossed, and the cached best prices agree with the level
    /// indexes. O(log64 L).
    fn debug_check_invariants(&self) {
        if let (Some(bid), Some(ask)) = (self.best_bid, self.best_ask) {
            debug_assert!(bid < ask, "book crossed: best bid {bid} >= best ask {ask}");
        }
        debug_assert_eq!(
            (self.best_bid, self.best_ask),
            self.best_prices_from_level_indexes(),
            "the cached best prices are stale"
        );
    }

    /// Checks every internal link and index against the others, and panics at the first
    /// disagreement. Walks every level and every order, so it is for tests only (the
    /// equivalence test runs it after every step); the book itself runs only the cheap
    /// checks, in debug builds.
    pub fn assert_consistent(&self) {
        // Every level's list: links agree in both directions, every order in it has this
        // level's price, one side only, and an id-map entry pointing back at its slot. The
        // level indexes mark exactly the non-empty levels.
        let mut resting = 0;
        for (l, level) in self.levels.iter().enumerate() {
            let price = self.price_of(l);
            let mut level_side = None;
            let mut prev = NIL;
            let mut slot = level.head;
            while slot != NIL {
                let o = &self.slots[slot as usize];
                let id = o.order_id;
                assert_eq!(o.prev, prev, "broken backward link at price {price}");
                assert_eq!(o.price, price, "order {id:#x} is in the wrong level");
                assert!(o.qty > 0, "order {id:#x} rests with nothing left to fill");
                assert_eq!(self.slot_of.get(&id), Some(&slot), "the id map is wrong for {id:#x}");
                assert!(level_side.is_none_or(|s| s == o.side), "price {price} holds both bids and asks");
                level_side = Some(o.side);
                resting += 1;
                assert!(resting <= self.slots.len(), "a level's list loops");
                prev = slot;
                slot = o.next;
            }
            assert_eq!(level.tail, prev, "wrong tail at price {price}");
            let (has_bids, has_asks) = (level_side == Some(Side::Buy), level_side == Some(Side::Sell));
            assert_eq!(self.bid_levels.contains(l), has_bids, "bid level index wrong at {price}");
            assert_eq!(self.ask_levels.contains(l), has_asks, "ask level index wrong at {price}");
        }
        assert_eq!(self.slot_of.len(), resting, "the id map has entries for orders not on the book");

        // Every account's list holds only that account's resting orders, correctly linked,
        // and together the lists hold every resting order. No list is empty: an account
        // leaves the map with its last order. (A test may iterate the map: the order of
        // iteration can't change a count.)
        let mut listed = 0;
        for (&account, list) in &self.account_orders {
            assert_ne!(list.head, NIL, "account {account} has an empty list in the map");
            let mut prev = NIL;
            let mut slot = list.head;
            while slot != NIL {
                let o = &self.slots[slot as usize];
                let id = o.order_id;
                assert_eq!(account_of(id), account, "order {id:#x} is in the wrong account's list");
                assert_eq!(o.account_prev, prev, "broken backward link in account {account}'s list");
                assert_eq!(self.slot_of.get(&id), Some(&slot), "account {account} lists a freed slot");
                listed += 1;
                assert!(listed <= self.slots.len(), "an account's list loops");
                prev = slot;
                slot = o.account_next;
            }
            assert_eq!(list.tail, prev, "wrong tail in account {account}'s list");
        }
        assert_eq!(listed, resting, "account lists and levels hold different numbers of orders");

        // Every slot is either resting or on the free list.
        let mut free = 0;
        let mut slot = self.free_head;
        while slot != NIL {
            free += 1;
            assert!(free <= self.slots.len(), "the free list loops");
            slot = self.slots[slot as usize].next;
        }
        assert_eq!(resting + free, self.slots.len(), "slots lost from both the book and the free list");

        // The level indexes were checked above, so this checks the cache.
        let from_indexes = self.best_prices_from_level_indexes();
        assert_eq!((self.best_bid, self.best_ask), from_indexes, "the cached best prices are stale");
        if let (Some(bid), Some(ask)) = (self.best_bid, self.best_ask) {
            assert!(bid < ask, "book crossed: best bid {bid} >= best ask {ask}");
        }
    }
}

/// True if `a` is a better price than `b` for an order on `side`: higher for a bid, lower
/// for an ask.
fn is_better(side: Side, a: Price, b: Price) -> bool {
    match side {
        Side::Buy => a > b,
        Side::Sell => a < b,
    }
}

impl OrderBook for Book {
    fn with_config(config: BookConfig, options: BookOptions) -> Self {
        Book::with_options(config, options)
    }

    /// The slab's spare room, both id maps and both level indexes (`crate::prefault`).
    /// The levels themselves are written when the book is built (`Level::EMPTY` is not
    /// zero), so they are resident already.
    fn prefault(&mut self) {
        let filler = OrderSlot {
            order_id: 0,
            price: 0,
            qty: 0,
            filled: 0,
            prev: NIL,
            next: NIL,
            account_prev: NIL,
            account_next: NIL,
            side: Side::Buy,
            post_only: false,
        };
        crate::prefault::touch_spare(&mut self.slots, filler);
        crate::prefault::touch_map(&mut self.slot_of, |i| i as OrderId, || 0);
        crate::prefault::touch_map(&mut self.account_orders, |i| i as AccountId, || AccountOrders::EMPTY);
        self.bid_levels.prefault();
        self.ask_levels.prefault();
    }

    fn place(&mut self, order: &PlaceOrder, events: &mut impl EventSink) {
        let id = order.order_id;
        if order.qty <= 0 {
            return self.reject(id, RejectReason::InvalidQty, events);
        }
        if !self.price_in_range(order.price) {
            return self.reject(id, RejectReason::InvalidPrice, events);
        }
        if self.slot_of.contains_key(&id) {
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
        self.debug_check_invariants();
    }

    fn cancel(&mut self, order_id: OrderId, events: &mut impl EventSink) {
        let Some(&slot) = self.slot_of.get(&order_id) else {
            return self.reject(order_id, RejectReason::UnknownOrder, events);
        };
        let order = self.remove(slot);
        self.emit_cancelled(&order, CancelReason::UserRequested, events);
        self.debug_check_invariants();
    }

    fn modify(&mut self, order_id: OrderId, new_price: Price, new_size: Qty, events: &mut impl EventSink) {
        let Some(&slot) = self.slot_of.get(&order_id) else {
            return self.reject(order_id, RejectReason::UnknownOrder, events);
        };
        if new_size <= 0 {
            return self.reject(order_id, RejectReason::InvalidQty, events);
        }
        if !self.price_in_range(new_price) {
            return self.reject(order_id, RejectReason::InvalidPrice, events);
        }

        let order = self.slots[slot as usize];
        let new_remaining = new_size - order.filled;

        // Sized at or below what already filled (typically a modify signed before a fill):
        // nothing is left to fill.
        if new_remaining <= 0 {
            self.remove(slot);
            self.emit_cancelled(&order, CancelReason::SizeBelowFilled, events);
            self.debug_check_invariants();
            return;
        }

        let modified = Event::Modified(Modified {
            order_id,
            price: new_price,
            qty: new_remaining,
            market: self.config.market,
        });

        // Shrinking in place keeps the order's place in the queue.
        if new_price == order.price && new_remaining <= order.qty {
            self.slots[slot as usize].qty = new_remaining;
            events.emit(modified);
            self.debug_check_invariants();
            return;
        }

        // Anything else is cancel-and-replace: back of the queue, and it may trade. The
        // order leaves the book before matching, exactly as in the reference book.
        if order.post_only && self.would_cross(order.side, new_price) {
            return self.reject(order_id, RejectReason::PostOnlyWouldCross, events);
        }
        self.remove(slot);
        events.emit(modified);
        let left = self.match_incoming(order_id, order.side, new_price, new_remaining, events);
        if left > 0 {
            let filled = order.filled + (new_remaining - left);
            self.rest(order_id, order.side, new_price, left, filled, order.post_only);
        }
        self.debug_check_invariants();
    }

    fn cancel_account(&mut self, account: AccountId, reason: CancelReason, events: &mut impl EventSink) {
        let Some(list) = self.account_orders.get(&account) else { return };
        // Walk the account's list from its oldest order. Each removal unlinks the current
        // head, so remember the next one first.
        let mut slot = list.head;
        while slot != NIL {
            let next = self.slots[slot as usize].account_next;
            let order = self.remove(slot);
            self.emit_cancelled(&order, reason, events);
            slot = next;
        }
        self.debug_check_invariants();
    }

    fn cancel_beyond(&mut self, side: Side, limit: Price, reason: CancelReason, events: &mut impl EventSink) {
        // "Beyond the limit" is "better than the limit" for this side: a higher bid, a
        // lower ask. The best order is the head of the best level, so each turn cancels it;
        // `remove` moves the cached best price inward when a level empties. The loop stops
        // at the first order inside the limit, so it never looks at the rest of the book.
        while let Some(best) = self.best_price(side) {
            if !is_better(side, best, limit) {
                break;
            }
            let head = self.levels[self.level_of(best)].head;
            let order = self.remove(head);
            self.emit_cancelled(&order, reason, events);
        }
        self.debug_check_invariants();
    }

    fn order(&self, order_id: OrderId) -> Option<RestingOrder> {
        let &slot = self.slot_of.get(&order_id)?;
        Some(self.resting_order(slot))
    }

    fn open_quantities(&self, account: AccountId) -> (Qty, Qty) {
        let (mut buys, mut sells) = (0, 0);
        let Some(list) = self.account_orders.get(&account) else { return (0, 0) };
        let mut slot = list.head;
        while slot != NIL {
            let order = &self.slots[slot as usize];
            match order.side {
                Side::Buy => buys += order.qty,
                Side::Sell => sells += order.qty,
            }
            slot = order.account_next;
        }
        (buys, sells)
    }

    fn best_bid(&self) -> Option<Price> {
        self.best_bid
    }

    fn best_ask(&self) -> Option<Price> {
        self.best_ask
    }

    fn snapshot(&self) -> BookSnapshot {
        BookSnapshot { bids: self.resting_orders(Side::Buy), asks: self.resting_orders(Side::Sell) }
    }
}

#[cfg(test)]
mod tests {
    //! Cases the equivalence test (`engine/tests/book_equivalence.rs`) can miss: its prices
    //! span only 21 levels, it never outgrows the reserved capacity, and it can't see which
    //! slot an order uses. Where possible these also run the reference book and compare.
    use super::*;
    use crate::reference::ReferenceBook;
    use crate::types::order_id;

    const MARKET: u16 = 1;
    const ALICE: u32 = 1;
    const BOB: u32 = 2;
    const CAROL: u32 = 3;
    const CONFIG: BookConfig = BookConfig { market: MARKET, min_price: 1, max_price: 1_000 };

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

    fn resting(o: PlaceOrder, qty: Qty, filled: Qty) -> RestingOrder {
        RestingOrder {
            order_id: o.order_id,
            side: o.side,
            price: o.price,
            qty,
            filled,
            post_only: o.post_only,
        }
    }

    fn cancelled(o: PlaceOrder, remaining: Qty, reason: CancelReason) -> Event {
        Event::Cancelled(Cancelled { order_id: o.order_id, remaining, market: MARKET, reason, side: o.side })
    }

    fn reject(id: OrderId, reason: RejectReason) -> Event {
        Event::Reject(Reject { order_id: id, account: account_of(id), reason })
    }

    /// One call on a book, so the same sequence can be applied to several books.
    #[derive(Clone, Copy, Debug)]
    enum Call {
        Place(PlaceOrder),
        Cancel(OrderId),
        Modify(OrderId, Price, Qty),
        CancelAccount(AccountId),
        CancelBeyond(Side, Price),
    }

    fn apply(book: &mut impl OrderBook, call: Call) -> Vec<Event> {
        let mut events = Vec::new();
        match call {
            Call::Place(order) => book.place(&order, &mut events),
            Call::Cancel(id) => book.cancel(id, &mut events),
            Call::Modify(id, price, size) => book.modify(id, price, size, &mut events),
            Call::CancelAccount(account) => {
                book.cancel_account(account, CancelReason::Liquidation, &mut events)
            }
            Call::CancelBeyond(side, limit) => {
                book.cancel_beyond(side, limit, CancelReason::PriceBand, &mut events)
            }
        }
        events
    }

    /// Applies a call to the book and to the reference, checks that both emit the same
    /// events and that the book is consistent, and returns the events.
    fn apply_both(book: &mut Book, reference: &mut ReferenceBook, call: Call) -> Vec<Event> {
        let events = apply(book, call);
        assert_eq!(events, apply(reference, call), "{call:?}");
        assert_eq!(book.snapshot(), reference.snapshot(), "{call:?}");
        book.assert_consistent();
        events
    }

    #[test]
    fn a_reused_slot_is_not_confused_with_the_order_that_left_it() {
        let (mut b, mut r) = (Book::new(CONFIG), ReferenceBook::new(CONFIG));
        let old = order(ALICE, 1, Side::Buy, 100, 5);
        let new = order(BOB, 1, Side::Sell, 105, 3);
        apply_both(&mut b, &mut r, Call::Place(old));
        apply_both(&mut b, &mut r, Call::Cancel(old.order_id));
        apply_both(&mut b, &mut r, Call::Place(new));
        assert_eq!(b.slots.len(), 1, "the new order took the freed slot");

        // The old id refers to nothing now, even though its slot is in use again.
        let unknown = vec![reject(old.order_id, RejectReason::UnknownOrder)];
        assert_eq!(apply_both(&mut b, &mut r, Call::Cancel(old.order_id)), unknown);
        assert_eq!(apply_both(&mut b, &mut r, Call::Modify(old.order_id, 105, 9)), unknown);
        assert_eq!(b.snapshot().asks, vec![resting(new, 3, 0)]);

        // A maker's slot is freed when it fills completely, and reused the same way.
        apply_both(&mut b, &mut r, Call::Place(order(ALICE, 2, Side::Buy, 105, 3)));
        let next = order(ALICE, 3, Side::Sell, 110, 1);
        apply_both(&mut b, &mut r, Call::Place(next));
        assert_eq!(b.slots.len(), 1, "the next order took the filled maker's slot");
        let unknown = vec![reject(new.order_id, RejectReason::UnknownOrder)];
        assert_eq!(apply_both(&mut b, &mut r, Call::Cancel(new.order_id)), unknown);
        assert_eq!(b.snapshot().asks, vec![resting(next, 1, 0)]);
    }

    #[test]
    fn the_best_price_moves_across_bitmap_word_and_layer_boundaries() {
        // 300,000 levels, so four bitmap layers. Orders sit on both sides of the boundaries
        // of a word (64 levels), a layer-1 word (4,096) and a layer-2 word (262,144), and at
        // both ends of the range.
        let config = BookConfig { market: MARKET, min_price: 1, max_price: 300_000 };
        let levels = [0, 63, 64, 4_095, 4_096, 262_143, 262_144, 299_999];
        let prices = levels.map(|level| config.min_price + level as Price);
        let mut b = Book::new(config);

        // Asks: cancelling the best one each time moves the best ask up to the next level.
        for (seq, &price) in prices.iter().enumerate() {
            apply(&mut b, Call::Place(order(ALICE, seq as u32, Side::Sell, price, 1)));
        }
        for (seq, &price) in prices.iter().enumerate() {
            assert_eq!(b.best_ask(), Some(price));
            apply(&mut b, Call::Cancel(order_id(ALICE, seq as u32)));
            b.assert_consistent();
        }
        assert_eq!(b.best_ask(), None);

        // Bids: the same, from the top down.
        for (seq, &price) in prices.iter().enumerate() {
            apply(&mut b, Call::Place(order(BOB, seq as u32, Side::Buy, price, 1)));
        }
        for (seq, &price) in prices.iter().enumerate().rev() {
            assert_eq!(b.best_bid(), Some(price));
            apply(&mut b, Call::Cancel(order_id(BOB, seq as u32)));
            b.assert_consistent();
        }
        assert_eq!(b.best_bid(), None);

        // Matching crosses the same boundaries: one buy sweeps every ask, lowest first.
        for (seq, &price) in prices.iter().enumerate() {
            apply(&mut b, Call::Place(order(ALICE, 100 + seq as u32, Side::Sell, price, 1)));
        }
        let sweep = order(BOB, 100, Side::Buy, config.max_price, prices.len() as Qty);
        let fill_prices: Vec<Price> = apply(&mut b, Call::Place(sweep))
            .into_iter()
            .filter_map(|event| match event {
                Event::Fill(fill) => Some(fill.price),
                _ => None,
            })
            .collect();
        assert_eq!(fill_prices, prices);
        assert_eq!((b.best_bid(), b.best_ask()), (None, None));
        b.assert_consistent();
    }

    #[test]
    fn cancel_account_follows_resting_order_through_fills_and_requeues() {
        let (mut b, mut r) = (Book::new(CONFIG), ReferenceBook::new(CONFIG));
        let a1 = order(ALICE, 1, Side::Buy, 100, 2);
        let a2 = order(ALICE, 2, Side::Sell, 105, 2);
        let bob = order(BOB, 1, Side::Buy, 99, 1);
        let a3 = order(ALICE, 3, Side::Buy, 98, 3);
        for o in [a1, a2, bob, a3] {
            apply_both(&mut b, &mut r, Call::Place(o));
        }
        // A partial fill keeps a1's place: 1 filled, 1 left.
        apply_both(&mut b, &mut r, Call::Place(order(BOB, 2, Side::Sell, 100, 1)));
        // Growing to a total of 5 (4 left) is cancel-and-replace: a1 starts resting again.
        apply_both(&mut b, &mut r, Call::Modify(a1.order_id, 100, 5));
        // Shrinking in place keeps a2's place.
        apply_both(&mut b, &mut r, Call::Modify(a2.order_id, 105, 1));
        // A new price is cancel-and-replace: a3 starts resting again, after a1.
        apply_both(&mut b, &mut r, Call::Modify(a3.order_id, 97, 3));

        assert_eq!(
            apply_both(&mut b, &mut r, Call::CancelAccount(ALICE)),
            vec![
                cancelled(a2, 1, CancelReason::Liquidation),
                cancelled(a1, 4, CancelReason::Liquidation),
                cancelled(a3, 3, CancelReason::Liquidation),
            ]
        );
        assert_eq!(b.snapshot(), BookSnapshot { bids: vec![resting(bob, 1, 0)], asks: vec![] });
        assert!(!b.account_orders.contains_key(&ALICE), "the list goes with the account's last order");

        // The account's next order starts a new list.
        let a4 = order(ALICE, 4, Side::Sell, 110, 1);
        apply_both(&mut b, &mut r, Call::Place(a4));
        assert_eq!(
            apply_both(&mut b, &mut r, Call::CancelAccount(ALICE)),
            vec![cancelled(a4, 1, CancelReason::Liquidation)]
        );
        assert!(apply_both(&mut b, &mut r, Call::CancelAccount(ALICE)).is_empty());
    }

    #[test]
    fn cancel_beyond_walks_from_the_best_price_inward_and_stops_at_the_limit() {
        let (mut b, mut r) = (Book::new(CONFIG), ReferenceBook::new(CONFIG));
        // Bids at 103 (three), 102, 101 and 100; an ask at 110.
        let first_103 = order(BOB, 1, Side::Buy, 103, 1);
        let second_103 = order(ALICE, 1, Side::Buy, 103, 2);
        let third_103 = order(BOB, 2, Side::Buy, 103, 3);
        let at_102 = order(ALICE, 2, Side::Buy, 102, 3);
        let at_101 = order(BOB, 3, Side::Buy, 101, 4);
        let at_100 = order(ALICE, 3, Side::Buy, 100, 5);
        let ask = order(BOB, 4, Side::Sell, 110, 1);
        for o in [first_103, second_103, third_103, at_102, at_101, at_100, ask] {
            apply_both(&mut b, &mut r, Call::Place(o));
        }
        // Carol sells 2 at 103: the first bid fills, and the second keeps 1 of its 2.
        apply_both(&mut b, &mut r, Call::Place(order(CAROL, 1, Side::Sell, 103, 2)));

        // Bids above 101: the highest price first, oldest first within 103, a partly filled
        // bid with what is left of it; 101 itself stays.
        assert_eq!(
            apply_both(&mut b, &mut r, Call::CancelBeyond(Side::Buy, 101)),
            vec![
                cancelled(second_103, 1, CancelReason::PriceBand),
                cancelled(third_103, 3, CancelReason::PriceBand),
                cancelled(at_102, 3, CancelReason::PriceBand),
            ]
        );
        assert_eq!(b.best_bid(), Some(101));
        // Nothing is beyond the best price itself, and asks are untouched by a bid limit.
        assert!(apply_both(&mut b, &mut r, Call::CancelBeyond(Side::Buy, 101)).is_empty());
        assert_eq!(b.best_ask(), Some(110));

        // Asks below a limit, the mirror image; a limit past every order empties the side.
        assert!(apply_both(&mut b, &mut r, Call::CancelBeyond(Side::Sell, 110)).is_empty());
        assert_eq!(
            apply_both(&mut b, &mut r, Call::CancelBeyond(Side::Sell, 111)),
            vec![cancelled(ask, 1, CancelReason::PriceBand)]
        );
        assert_eq!(b.best_ask(), None);
        assert_eq!(
            apply_both(&mut b, &mut r, Call::CancelBeyond(Side::Buy, 0)),
            vec![
                cancelled(at_101, 4, CancelReason::PriceBand),
                cancelled(at_100, 5, CancelReason::PriceBand)
            ]
        );
        assert_eq!(b.snapshot(), BookSnapshot::default());
        // On an empty side it does nothing.
        assert!(apply_both(&mut b, &mut r, Call::CancelBeyond(Side::Buy, 0)).is_empty());
    }

    #[test]
    fn order_looks_up_one_resting_order_in_both_books() {
        let (mut b, mut r) = (Book::new(CONFIG), ReferenceBook::new(CONFIG));
        let bid = order(ALICE, 1, Side::Buy, 100, 5);
        apply_both(&mut b, &mut r, Call::Place(bid));
        apply_both(&mut b, &mut r, Call::Place(order(BOB, 1, Side::Sell, 100, 2)));
        assert_eq!(b.order(bid.order_id), Some(resting(bid, 3, 2)));
        assert_eq!(r.order(bid.order_id), b.order(bid.order_id));
        // Filled, cancelled or never placed: not resting.
        assert_eq!(b.order(order_id(BOB, 1)), None);
        assert_eq!(r.order(order_id(BOB, 1)), None);
        apply_both(&mut b, &mut r, Call::Cancel(bid.order_id));
        assert_eq!((b.order(bid.order_id), r.order(bid.order_id)), (None, None));
    }

    #[test]
    fn open_quantities_sum_what_is_left_of_an_accounts_orders_on_each_side() {
        let (mut b, mut r) = (Book::new(CONFIG), ReferenceBook::new(CONFIG));
        for o in [
            order(ALICE, 1, Side::Buy, 100, 5),
            order(ALICE, 2, Side::Buy, 99, 7),
            order(ALICE, 3, Side::Sell, 110, 4),
            order(BOB, 1, Side::Buy, 100, 50),
        ] {
            apply_both(&mut b, &mut r, Call::Place(o));
        }
        // Carol sells 3 at 100 into Alice's bid there, the older one: 2 of it are left.
        apply_both(&mut b, &mut r, Call::Place(order(CAROL, 1, Side::Sell, 100, 3)));
        assert_eq!(b.open_quantities(ALICE), (2 + 7, 4));
        assert_eq!(b.open_quantities(BOB), (50, 0));
        assert_eq!(b.open_quantities(CAROL), (0, 0), "no orders resting");
        for account in [ALICE, BOB, CAROL] {
            assert_eq!(r.open_quantities(account), b.open_quantities(account));
        }
    }

    #[test]
    fn with_config_builds_an_empty_book_either_way() {
        let options = BookOptions { order_capacity: 16, id_hash_seed: 7 };
        let b = Book::with_config(CONFIG, options);
        let r = ReferenceBook::with_config(CONFIG, options);
        assert_eq!(b.snapshot(), BookSnapshot::default());
        assert_eq!(r.snapshot(), BookSnapshot::default());
        assert_eq!(b.slots.capacity(), 16, "the options reach the book");
    }

    /// A fixed, pseudo-random mix of places (some IOC, some post-only), cancels, modifies,
    /// account cancels and cancels beyond a price, over five accounts and eleven prices so
    /// that orders interact.
    fn mixed_calls(count: u32) -> Vec<Call> {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        // xorshift64: tiny and deterministic, so the test needs no dependency.
        let mut random = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let mut placed = Vec::new();
        let mut calls = Vec::new();
        for seq in 0..count {
            let roll = random(100);
            let call = if roll < 55 || placed.is_empty() {
                let account = 1 + random(5) as AccountId;
                let side = if random(2) == 0 { Side::Buy } else { Side::Sell };
                let (price, qty) = (95 + random(11) as Price, 1 + random(10) as Qty);
                let tif = if random(10) == 0 { TimeInForce::Ioc } else { TimeInForce::Gtc };
                let post_only = random(10) == 0;
                let o = PlaceOrder { tif, post_only, ..order(account, seq, side, price, qty) };
                placed.push(o.order_id);
                Call::Place(o)
            } else {
                let target = placed[random(placed.len() as u64) as usize];
                match roll {
                    55..80 => Call::Cancel(target),
                    80..97 => Call::Modify(target, 95 + random(11) as Price, 1 + random(10) as Qty),
                    97..98 => {
                        let side = if random(2) == 0 { Side::Buy } else { Side::Sell };
                        Call::CancelBeyond(side, 95 + random(11) as Price)
                    }
                    _ => Call::CancelAccount(1 + random(5) as AccountId),
                }
            };
            calls.push(call);
        }
        calls
    }

    #[test]
    fn capacity_and_hash_seed_change_nothing_but_speed() {
        // Capacities of 0 and 1 make the slab and both maps grow again and again; different
        // seeds put the ids in different buckets. None of it may change a single event.
        let mut books = [
            Book::new(CONFIG),
            Book::with_options(CONFIG, BookOptions { order_capacity: 0, id_hash_seed: 0 }),
            Book::with_options(CONFIG, BookOptions { order_capacity: 1, id_hash_seed: 0xDEAD_BEEF }),
            Book::with_options(CONFIG, BookOptions { order_capacity: 10_000, id_hash_seed: u64::MAX }),
        ];
        let mut reference = ReferenceBook::new(CONFIG);
        for (step, call) in mixed_calls(5_000).into_iter().enumerate() {
            let expected = apply(&mut reference, call);
            for book in &mut books {
                assert_eq!(apply(book, call), expected, "step {step}: {call:?}");
            }
        }
        let expected = reference.snapshot();
        assert!(expected.bids.len() + expected.asks.len() > 10, "the mix should leave orders resting");
        for book in &books {
            assert_eq!(book.snapshot(), expected);
            book.assert_consistent();
        }
    }

    #[test]
    #[should_panic(expected = "ticks wide")]
    fn a_price_range_one_tick_wider_than_max_levels_is_refused() {
        Book::new(BookConfig { market: MARKET, min_price: 1, max_price: MAX_LEVELS as Price + 1 });
    }

    #[test]
    #[should_panic(expected = "ticks wide")]
    fn the_widest_possible_price_range_is_refused_without_overflowing() {
        Book::new(BookConfig { market: MARKET, min_price: Price::MIN, max_price: Price::MAX });
    }

    #[test]
    #[should_panic(expected = "empty price range")]
    fn an_empty_price_range_is_refused() {
        Book::new(BookConfig { market: MARKET, min_price: 10, max_price: 9 });
    }
}
