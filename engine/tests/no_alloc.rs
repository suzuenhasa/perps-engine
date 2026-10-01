//! The production book makes no heap allocation once it is warmed up.
//!
//! **Contract checked.** `book.rs` ("Allocation") promises that within the room reserved by
//! `Book::with_options`, no call allocates: freed order slots are reused and neither map has
//! to grow. INFO.md section 4 asks for no heap allocation per order on the hot path. This test
//! checks that directly on a busy book: after a warm-up, 10,000 mixed commands (places of
//! every kind, crossing and not, cancels, modifies of every kind, account cancels, cancels
//! beyond a price) must make no heap allocation at all, and neither may the two lookups the
//! engine makes (`order` and `open_quantities`).
//!
//! **How allocations are counted.** A test binary may replace the global allocator, which
//! every `Box`, `Vec` and `HashMap` in the process gets its memory from. This one forwards
//! to the system allocator and counts calls per thread, so the test harness's own threads
//! can't disturb the count.
//!
//! **Keeping the test itself from allocating.** The commands are generated before the
//! measured phase, against the reference book (which allocates freely), and the book
//! writes its events into a `Vec` with room reserved for all of them. So while the book
//! runs, nothing else on this thread allocates. The events must also equal the reference
//! book's, which is a free equivalence check on a book far deeper than the property tests
//! build.
//!
//! **Scope.** The book has room for 4,096 resting orders; the flow keeps 350 to 600 resting,
//! over 200 price levels, from eight accounts that all have orders in the warm-up. This is
//! the steady state the promise is for, not the edge of the reserved capacity.
//!
//! **At the edge of the reserved capacity.** Two regression tests after it take the book
//! there with plain place and cancel churn, one call at a time: 500,000 steps with 900 to
//! `order_capacity` orders resting (the id map once resized under this), and 10,000 accounts
//! that each rest one order and cancel it (the account map once kept every account it had
//! seen, and outgrew its room).
//!
//! **The engine** (`docs/RISK.md` 14.4, "Allocation"). The engine promises the same within
//! its reserved room (`engine.rs`, "Allocation"). A script of engine commands
//! (`no_alloc/engine_flow.rs`: places, cancels, modifies, fills, top-ups, releases, marks
//! that sweep orders but liquidate nothing, deposits, withdrawals, leverage changes) is
//! replayed after a warm-up that creates every account and slot it uses:
//! 1. on `Engine<Book, NaiveLiquidation>` (running totals, no index), which must make no
//!    heap allocation and no free at all;
//! 2. on `Engine<Book, Fast>`, which must make none either: the difference from step 1 is
//!    exactly the liquidation index's share. RISK.md 14.4 asked only for a report here, and
//!    a switch to D-018's fallback, an indexed heap, if `Fast` allocated more than once per
//!    1,000 commands. With std's `BTreeSet`, which allocates when a node splits and frees
//!    when two merge, it made 468 allocations and 468 frees in these 10,000 commands over
//!    256 accounts (RISK.md 18, P1). The heap reserves its room up front, so this is now an
//!    assertion, and a regression test for that switch.
//!
//! Both run with 8 and with 256 accounts: a B-tree of 8 keys per side fits in one node and
//! never split, so only the larger flow showed the problem. Frees are counted as well as
//! allocations, because a B-tree that merges two nodes frees one and allocates nothing.
//!
//! **`Engine::prefault`** (Milestone 3, `docs/PIPELINE.md` 15.4), which the pipeline's core
//! thread calls before a run so that reserved memory isn't first touched inside a measured
//! window, must change none of this: the same script, on an engine prefaulted where the core
//! thread does it (at the start and after each `SetMarketParams`) and once more after the
//! warm-up, when its maps hold entries and are rebuilt, must emit the same events, end in the
//! same state and still allocate nothing on the measured commands. Prefaulting a market that
//! `SetMarketParams` has just created allocates nothing either.

// Test-only exception to the workspace's `unsafe_code = "deny"`. Counting allocations
// needs a global allocator, and `GlobalAlloc` is an unsafe trait: an implementation
// promises to hand out valid memory. Ours only forwards to `std::alloc::System` and bumps
// a counter. This file is its own test binary, so none of this reaches the engine, which
// stays free of `unsafe`.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use engine::book::{Book, BookConfig, BookOptions, OrderBook, RestingOrder};
use engine::command::{Command, PlaceOrder};
use engine::event::{CancelReason, Event, EventSink};
use engine::reference::ReferenceBook;
use engine::types::{AccountId, OrderId, Price, Qty, Side, TimeInForce, order_id};

thread_local! {
    /// Heap allocations made by this thread so far.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    /// Heap frees made by this thread so far.
    static FREES: Cell<u64> = const { Cell::new(0) };
}

/// The system allocator, plus a count of allocations and frees. `realloc` and `alloc_zeroed`
/// keep the trait's default implementations, which call `alloc` (and `realloc` then
/// `dealloc`), so they are counted too.
struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `try_with`: a thread that is shutting down may have lost its thread-locals, and
        // an allocator must not panic.
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        // SAFETY: our caller upholds `GlobalAlloc::alloc`'s contract; we pass it on as is.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _ = FREES.try_with(|count| count.set(count.get() + 1));
        // SAFETY: `ptr` came from `System.alloc` (through `alloc` above) with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// How many heap allocations `f` makes on this thread.
fn allocations_during(f: impl FnOnce()) -> u64 {
    allocations_and_frees_during(f).0
}

/// How many heap allocations and frees `f` makes on this thread.
fn allocations_and_frees_during(f: impl FnOnce()) -> (u64, u64) {
    let before = (ALLOCATIONS.with(Cell::get), FREES.with(Cell::get));
    f();
    (ALLOCATIONS.with(Cell::get) - before.0, FREES.with(Cell::get) - before.1)
}

const MARKET: u16 = 1;
const CONFIG: BookConfig = BookConfig { market: MARKET, min_price: 1, max_price: 10_000 };
/// The middle of the market. Passive orders rest within 100 ticks of it on their own side.
const MID: Price = 5_000;
const ACCOUNTS: u64 = 8;
/// Room reserved in the book, well above what the flow ever keeps resting.
const ORDER_CAPACITY: usize = 4_096;
/// When this many orders rest, the flow cancels instead of placing. The current mix never
/// gets there; this keeps the book well within its capacity if the mix changes.
const MAX_RESTING: usize = 1_000;

/// One call on a book, so the same sequence can be applied to both books.
#[derive(Clone, Copy, Debug)]
enum Call {
    Place(PlaceOrder),
    Cancel(OrderId),
    Modify(OrderId, Price, Qty),
    CancelAccount(AccountId),
    CancelBeyond(Side, Price),
}

impl Call {
    fn apply(&self, book: &mut impl OrderBook, events: &mut Vec<Event>) {
        match *self {
            Call::Place(order) => book.place(&order, events),
            Call::Cancel(id) => book.cancel(id, events),
            Call::Modify(id, price, size) => book.modify(id, price, size, events),
            Call::CancelAccount(account) => book.cancel_account(account, CancelReason::Liquidation, events),
            Call::CancelBeyond(side, limit) => {
                book.cancel_beyond(side, limit, CancelReason::PriceBand, events)
            }
        }
    }
}

/// A tiny deterministic random number generator (xorshift64, as in `book.rs`'s tests), so
/// the test always runs the same commands and needs no dependency.
#[derive(Debug)]
struct Random(u64);

impl Random {
    /// A number in `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }

    /// A number in `low..=high`.
    fn between(&mut self, low: i64, high: i64) -> i64 {
        low + self.below((high - low + 1) as u64) as i64
    }

    /// True `percent`% of the time.
    fn percent(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// Every order resting in `book`, bids first.
fn resting_orders(book: &ReferenceBook) -> Vec<RestingOrder> {
    let snapshot = book.snapshot();
    snapshot.bids.into_iter().chain(snapshot.asks).collect()
}

/// Generates a busy market's commands, each chosen from the reference book's state so that
/// cancels and modifies go to resting orders and every modify rule is taken.
#[derive(Debug)]
struct Flow {
    random: Random,
    /// The last sequence number used. One counter for all accounts keeps every id unique.
    seq: u32,
}

impl Flow {
    fn new() -> Self {
        Flow { random: Random(0x9E37_79B9_7F4A_7C15), seq: 0 }
    }

    fn account(&mut self) -> AccountId {
        1 + self.random.below(ACCOUNTS) as AccountId
    }

    fn new_id(&mut self) -> OrderId {
        self.seq += 1;
        order_id(self.account(), self.seq)
    }

    fn pick(&mut self, orders: &[RestingOrder]) -> RestingOrder {
        orders[self.random.below(orders.len() as u64) as usize]
    }

    /// On `side`'s own side of `MID`, up to 100 ticks away, so an order there usually rests.
    fn passive_price(&mut self, side: Side) -> Price {
        let distance = self.random.between(1, 100);
        match side {
            Side::Buy => MID - distance,
            Side::Sell => MID + distance,
        }
    }

    /// Up to 20 ticks through `MID`, so an order there trades with what rests near it.
    fn aggressive_price(&mut self, side: Side) -> Price {
        let distance = self.random.between(0, 20);
        match side {
            Side::Buy => MID + distance,
            Side::Sell => MID - distance,
        }
    }

    /// A new order from a random account, passive or aggressive; 15% IOC, 15% post-only.
    fn new_order(&mut self, passive: bool) -> PlaceOrder {
        let side = if self.random.percent(50) { Side::Buy } else { Side::Sell };
        let price = if passive { self.passive_price(side) } else { self.aggressive_price(side) };
        PlaceOrder {
            order_id: self.new_id(),
            price,
            qty: self.random.between(1, 20),
            market: MARKET,
            side,
            tif: if self.random.percent(15) { TimeInForce::Ioc } else { TimeInForce::Gtc },
            post_only: self.random.percent(15),
        }
    }

    /// The next command, and a label for the case it was generated to exercise.
    fn next(&mut self, book: &ReferenceBook) -> (&'static str, Call) {
        let resting = resting_orders(book);
        if resting.is_empty() {
            return ("place", Call::Place(self.new_order(true)));
        }
        // Out of 1,000: 45% places (a fifth of them aggressive), 3% malformed or duplicate
        // places, 14% cancels, 37.6% modifies, 0.2% cancels beyond a price near the top of
        // one side, and 0.2% account cancels, which remove an eighth of the book each.
        match self.random.below(1_000) {
            // A full book gets a cancel instead of a new order.
            0..450 if resting.len() >= MAX_RESTING => ("cancel", Call::Cancel(self.pick(&resting).order_id)),
            0..450 => {
                let passive = self.random.percent(80);
                ("place", Call::Place(self.new_order(passive)))
            }
            450..480 => {
                let order = self.new_order(true);
                let malformed = match self.random.below(3) {
                    0 => PlaceOrder { qty: 0, ..order },
                    1 => PlaceOrder { price: CONFIG.max_price + 1, ..order },
                    _ => PlaceOrder { order_id: self.pick(&resting).order_id, ..order },
                };
                ("place: malformed or duplicate", Call::Place(malformed))
            }
            480..600 => ("cancel", Call::Cancel(self.pick(&resting).order_id)),
            // An id that is never used.
            600..620 => ("cancel", Call::Cancel(order_id(1, u32::MAX))),
            620..996 => self.modify(book, &resting),
            996..998 => {
                // Passive orders rest up to 100 ticks from MID, so this takes the orders
                // within 1 to 10 ticks of it on one side, as a mark move would.
                let distance = self.random.between(1, 10);
                let call = if self.random.percent(50) {
                    Call::CancelBeyond(Side::Buy, MID - distance)
                } else {
                    Call::CancelBeyond(Side::Sell, MID + distance)
                };
                ("cancel beyond", call)
            }
            _ => ("cancel account", Call::CancelAccount(self.account())),
        }
    }

    /// A modify of one of the eight kinds below, with sizes as *total* sizes (what has
    /// filled plus what is left to fill).
    fn modify(&mut self, book: &ReferenceBook, resting: &[RestingOrder]) -> (&'static str, Call) {
        let order = self.pick(resting);
        let (id, total) = (order.order_id, order.filled + order.qty);
        match self.random.below(8) {
            0 => {
                // At or up to 2 below what has filled: removed. Only an order that has
                // filled something can be sized down to its fills; with none resting, the
                // size is 0, which is rejected.
                let partly_filled: Vec<RestingOrder> =
                    resting.iter().copied().filter(|o| o.filled > 0).collect();
                let call = if partly_filled.is_empty() {
                    Call::Modify(id, order.price, 0)
                } else {
                    let o = self.pick(&partly_filled);
                    Call::Modify(o.order_id, o.price, (o.filled - self.random.between(0, 2)).max(1))
                };
                ("modify: at or below filled", call)
            }
            1 => {
                let left = (order.qty - self.random.between(1, 5)).max(1);
                ("modify: shrink", Call::Modify(id, order.price, order.filled + left))
            }
            2 => ("modify: same size", Call::Modify(id, order.price, total)),
            3 => ("modify: grow", Call::Modify(id, order.price, total + self.random.between(1, 5))),
            4 => ("modify: new price", Call::Modify(id, self.passive_price(order.side), total)),
            5 => {
                // At or through the best opposite price: it trades, or, if post-only, is
                // rejected.
                let through = self.random.between(0, 3);
                let price = match order.side {
                    Side::Buy => book.best_ask().map_or(order.price, |ask| ask + through),
                    Side::Sell => book.best_bid().map_or(order.price, |bid| bid - through),
                };
                ("modify: cross", Call::Modify(id, price, total))
            }
            6 => {
                let call = if self.random.percent(50) {
                    Call::Modify(id, order.price, 0)
                } else {
                    Call::Modify(id, CONFIG.max_price + 1, total)
                };
                ("modify: invalid", call)
            }
            _ => ("modify: unknown id", Call::Modify(order_id(1, u32::MAX), order.price, total)),
        }
    }
}

/// A short name for an event: its type, and the reason for a reject or cancel.
fn event_name(event: &Event) -> String {
    match event {
        Event::Reject(reject) => format!("Reject {:?}", reject.reason),
        Event::Cancelled(cancelled) => format!("Cancelled {:?}", cancelled.reason),
        Event::Ack(_) => "Ack".to_string(),
        Event::Fill(_) => "Fill".to_string(),
        Event::Modified(_) => "Modified".to_string(),
        other => format!("{other:?}"),
    }
}

/// What the measured commands did, from the reference book's events: how often each kind
/// of command had each first event, and every event name that appeared at all.
#[derive(Debug, Default)]
struct Tally {
    first_events: BTreeMap<(&'static str, String), usize>,
    all_events: BTreeSet<String>,
}

impl Tally {
    fn record(&mut self, label: &'static str, events: &[Event]) {
        let first = events.first().map_or("no events".to_string(), event_name);
        *self.first_events.entry((label, first)).or_default() += 1;
        self.all_events.extend(events.iter().map(event_name));
    }

    /// Panics unless the commands took every path this test is meant to cover.
    fn assert_covers_every_path(&self) {
        let must_happen = [
            ("place", "Ack"),
            ("place", "Reject PostOnlyWouldCross"),
            ("place: malformed or duplicate", "Reject InvalidQty"),
            ("place: malformed or duplicate", "Reject InvalidPrice"),
            ("place: malformed or duplicate", "Reject Duplicate"),
            ("cancel", "Cancelled UserRequested"),
            ("cancel", "Reject UnknownOrder"),
            ("modify: at or below filled", "Cancelled SizeBelowFilled"),
            ("modify: shrink", "Modified"),
            ("modify: same size", "Modified"),
            ("modify: grow", "Modified"),
            ("modify: new price", "Modified"),
            ("modify: cross", "Modified"),
            ("modify: cross", "Reject PostOnlyWouldCross"),
            ("modify: invalid", "Reject InvalidQty"),
            ("modify: invalid", "Reject InvalidPrice"),
            ("modify: unknown id", "Reject UnknownOrder"),
            ("cancel account", "Cancelled Liquidation"),
            ("cancel beyond", "Cancelled PriceBand"),
        ];
        for (label, first) in must_happen {
            let count = self.first_events.get(&(label, first.to_string())).copied().unwrap_or(0);
            assert!(count > 0, "no `{label}` command started with {first}");
        }
        for name in ["Fill", "Cancelled SelfTrade", "Cancelled IocRemainder"] {
            assert!(self.all_events.contains(name), "no {name} event");
        }
    }
}

#[test]
fn a_warmed_up_book_makes_no_heap_allocation() {
    let options = BookOptions { order_capacity: ORDER_CAPACITY, id_hash_seed: 0x5EED };
    let mut book = Book::with_options(CONFIG, options);
    let mut reference = ReferenceBook::new(CONFIG);
    let mut flow = Flow::new();

    // Warm-up, not measured: fill the book with 800 passive orders, then run the mixed flow
    // for 2,000 commands, so every account has had orders resting and every kind of
    // command has run. Both books get the same calls, and must agree.
    for n in 0..2_800 {
        let call = if n < 800 { Call::Place(flow.new_order(true)) } else { flow.next(&reference).1 };
        let (mut expected, mut actual) = (Vec::new(), Vec::new());
        call.apply(&mut reference, &mut expected);
        call.apply(&mut book, &mut actual);
        assert_eq!(actual, expected, "warm-up: {call:?}");
    }
    assert_eq!(book.snapshot(), reference.snapshot(), "after the warm-up");

    // The measured commands, generated against the reference book, with its events.
    let mut script = Vec::new();
    let mut expected = Vec::new();
    let mut tally = Tally::default();
    for _ in 0..10_000 {
        let (label, call) = flow.next(&reference);
        let first = expected.len();
        call.apply(&mut reference, &mut expected);
        tally.record(label, &expected[first..]);
        script.push(call);
    }

    // Measured: the book runs the script into a sink with room for every event.
    let mut actual: Vec<Event> = Vec::with_capacity(expected.len());
    let allocations = allocations_during(|| {
        for call in &script {
            call.apply(&mut book, &mut actual);
        }
    });

    // The events first: extra events would have grown the sink, and that is the clearer
    // failure. (Compared without printing them: there are tens of thousands.)
    assert!(actual == expected, "the book's events differ from the reference book's");
    assert_eq!(allocations, 0, "the book allocated {allocations} times in 10,000 commands");
    assert_eq!(book.snapshot(), reference.snapshot());
    book.assert_consistent();
    tally.assert_covers_every_path();

    // The engine's lookups on the same deep book: every resting order and every account.
    let ids: Vec<OrderId> = resting_orders(&reference).iter().map(|o| o.order_id).collect();
    let lookups = allocations_during(|| {
        for &id in &ids {
            assert!(std::hint::black_box(book.order(id)).is_some());
        }
        for account in 1..=ACCOUNTS as AccountId {
            std::hint::black_box(book.open_quantities(account));
        }
    });
    assert_eq!(lookups, 0, "`order` or `open_quantities` allocated");
}

/// Room reserved by the two tests at the edge of the reserved capacity.
const SMALL_CAPACITY: usize = 1_024;

/// Drops every event. The tests at the edge run too many commands to keep their events,
/// and a growing `Vec` would allocate.
#[derive(Debug)]
struct DiscardingSink;

impl EventSink for DiscardingSink {
    fn emit(&mut self, _event: Event) {}
}

/// A GTC bid of one lot. The tests at the edge place only bids, so nothing ever trades.
fn bid(id: OrderId, price: Price) -> PlaceOrder {
    PlaceOrder {
        order_id: id,
        price,
        qty: 1,
        market: MARKET,
        side: Side::Buy,
        tif: TimeInForce::Gtc,
        post_only: false,
    }
}

/// Regression: the id map, reserved at `order_capacity`, resized under churn within it.
#[test]
fn place_and_cancel_churn_up_to_the_reserved_capacity_makes_no_heap_allocation() {
    let options = BookOptions { order_capacity: SMALL_CAPACITY, id_hash_seed: 0x5EED };
    let mut book = Book::with_options(CONFIG, options);
    let mut sink = DiscardingSink;
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    // The ids resting now. Reserved up front and never longer than SMALL_CAPACITY, so the
    // test's own bookkeeping doesn't allocate.
    let mut resting: Vec<OrderId> = Vec::with_capacity(SMALL_CAPACITY);

    // Fill to 900 resting orders, then place and cancel at random, keeping between 900 and
    // SMALL_CAPACITY resting. Eight accounts, which always have orders resting, so the
    // account map never changes and only the id map churns.
    let mut seq = 0;
    for step in 0..500_000 {
        let place = resting.len() < 900 || (resting.len() < SMALL_CAPACITY && random.below(2) == 0);
        let allocations = if place {
            let account = 1 + random.below(8) as AccountId;
            let id = order_id(account, seq);
            seq += 1;
            let price = 1 + random.below(1_000) as Price;
            resting.push(id);
            allocations_during(|| book.place(&bid(id, price), &mut sink))
        } else {
            let id = resting.swap_remove(random.below(resting.len() as u64) as usize);
            allocations_during(|| book.cancel(id, &mut sink))
        };
        assert_eq!(
            allocations,
            0,
            "step {step}: a {} allocated with {} orders resting, within order_capacity {SMALL_CAPACITY}",
            if place { "place" } else { "cancel" },
            resting.len()
        );
    }
}

/// Regression: the account map kept every account ever seen, so it outgrew its reserved room.
#[test]
fn many_accounts_coming_and_going_make_no_heap_allocation() {
    let options = BookOptions { order_capacity: SMALL_CAPACITY, id_hash_seed: 0x5EED };
    let mut book = Book::with_options(CONFIG, options);
    let mut sink = DiscardingSink;
    // Each account rests one order and cancels it: never more than one order resting, but
    // almost ten times as many accounts as SMALL_CAPACITY, more than even the maps' doubled
    // room would hold.
    for account in 1..=10_000 {
        let id = order_id(account, 1);
        let allocations = allocations_during(|| {
            book.place(&bid(id, 100), &mut sink);
            book.cancel(id, &mut sink);
        });
        assert_eq!(
            allocations, 0,
            "account {account}: placing and cancelling one order allocated, with order_capacity {SMALL_CAPACITY}"
        );
    }
}

#[test]
fn the_allocation_counter_sees_an_allocation_and_a_free() {
    // Guards the tests here against passing only because nothing is being counted.
    let counts = allocations_and_frees_during(|| drop(std::hint::black_box(vec![1u8; 10])));
    assert_eq!(counts, (1, 1));
}

// ---------------------------------------------------------------------------------------
// The engine (module docs, "The engine").

// A test file is its own crate root, so its modules are named by path.
#[path = "no_alloc/engine_flow.rs"]
mod engine_flow;

use engine::engine::Engine;
use engine::mode::{Fast, Mode, NaiveLiquidation};
use engine_flow::{MEASURED_COMMANDS, Script};

/// The flow's sizes: as many accounts as the book test, and a market with many positions.
const ENGINE_FLOW_ACCOUNTS: [u32; 2] = [8, 256];

/// Replays the script on a fresh `Engine<Book, M>`: the warm-up unmeasured, then the measured
/// commands into a sink with room for all of their events. Checks the events against the
/// script's and returns the measured part's heap allocations and frees.
fn replay<M: Mode>(script: &Script) -> (u64, u64) {
    let mut engine: Engine<Book, M> = Engine::new(engine_flow::options());
    for command in &script.warm_up {
        engine.apply(command, &mut DiscardingSink);
    }
    let mut events: Vec<Event> = Vec::with_capacity(script.expected.len());
    let counts = allocations_and_frees_during(|| {
        for command in &script.measured {
            engine.apply(command, &mut events);
        }
    });
    // Compared without printing them: there are tens of thousands.
    assert!(events == script.expected, "the engine's events differ from the script's");
    counts
}

/// Replays the engine flow in mode `M` over each number of accounts, and asserts that the
/// measured commands made no heap allocation and no free.
fn assert_engine_makes_no_heap_allocation<M: Mode>(mode: &str) {
    for accounts in ENGINE_FLOW_ACCOUNTS {
        let script = engine_flow::script(accounts);
        script.assert_covers_the_flow();
        let (allocations, frees) = replay::<M>(&script);
        assert_eq!(
            (allocations, frees),
            (0, 0),
            "Engine<Book, {mode}> allocated {allocations} and freed {frees} times in \
             {MEASURED_COMMANDS} commands over {accounts} accounts"
        );
    }
}

#[test]
fn a_warmed_up_engine_without_the_liquidation_index_makes_no_heap_allocation() {
    assert_engine_makes_no_heap_allocation::<NaiveLiquidation>("NaiveLiquidation");
}

/// Regression (RISK.md 18, P1): the `BTreeSet` index made 468 allocations and 468 frees here
/// over 256 accounts, 47 allocations per 1,000 commands against D-018's threshold of 1.
#[test]
fn a_warmed_up_engine_with_the_liquidation_index_makes_no_heap_allocation() {
    assert_engine_makes_no_heap_allocation::<Fast>("Fast");
}

/// `Engine::prefault` changes nothing the engine emits or holds, and the engine it leaves
/// still allocates nothing on the measured commands (module docs).
#[test]
fn a_prefaulted_engine_emits_the_same_events_and_still_allocates_nothing() {
    let script = engine_flow::script(256);
    let mut plain: Engine<Book, Fast> = Engine::new(engine_flow::options());
    let mut prefaulted: Engine<Book, Fast> = Engine::new(engine_flow::options());
    prefaulted.prefault();
    for command in &script.warm_up {
        plain.apply(command, &mut DiscardingSink);
        prefaulted.apply(command, &mut DiscardingSink);
        if let Command::SetMarketParams(params) = command {
            let allocations = allocations_during(|| prefaulted.prefault_market(params.market));
            assert_eq!(allocations, 0, "a new market's maps are empty: nothing to rebuild");
        }
    }
    prefaulted.prefault(); // every map holds entries now: rebuilt
    assert!(plain.snapshot() == prefaulted.snapshot(), "the same state after the warm-up");

    let mut events: Vec<Event> = Vec::with_capacity(script.expected.len());
    let (allocations, frees) = allocations_and_frees_during(|| {
        for command in &script.measured {
            prefaulted.apply(command, &mut events);
        }
    });
    assert!(events == script.expected, "the prefaulted engine's events differ from the script's");
    assert_eq!((allocations, frees), (0, 0), "the prefaulted engine allocated on the measured commands");
    for command in &script.measured {
        plain.apply(command, &mut DiscardingSink);
    }
    assert!(plain.snapshot() == prefaulted.snapshot(), "the same state at the end");
    prefaulted.assert_invariants();
}
