//! # bench
//!
//! Shared helpers for the benchmarks in `benches/`, and the end-to-end harness:
//! - this file: a counting event sink, book depth, and applying commands to a bare book;
//! - [`risk`]: engine states for the risk-layer benchmarks (Milestone 2), and the timed
//!   loops that run on them;
//! - [`e2e`]: the end-to-end harness of Milestone 3 (open-loop load, a histogram per stage,
//!   sweeps, searches, ablations, machine probes and report generation), driven by the
//!   `e2e` binary (`src/bin/e2e.rs`).
//!
//! Every number that goes into `docs/BENCHMARKS.md` states its layer, machine and commit
//! (INFO.md section 10, "Honest numbers").

pub mod e2e;
pub mod risk;

use std::fmt;
use std::time::{Duration, Instant};

use engine::book::{BookSnapshot, OrderBook, RestingOrder};
use engine::command::Command;
use engine::event::{Event, EventSink, RejectReason};

/// An event sink that only counts events by kind, so benchmarks measure the book rather
/// than a `Vec` growing, and can report what the flow actually did (INFO.md section 7:
/// rejects are reported, not hidden).
#[derive(Debug, Default)]
pub struct CountingSink {
    pub acks: u64,
    pub fills: u64,
    pub cancels: u64,
    pub modifies: u64,
    pub rejects: u64,
    /// The ledger's events, which only the engine emits (Milestone 2): slots and free
    /// balances that changed, and liquidations.
    pub positions: u64,
    pub balances: u64,
    pub liquidations: u64,
    pub other: u64,
    /// The same rejects as `rejects`, split by reason.
    pub reject_reasons: RejectCounts,
}

impl CountingSink {
    pub fn total(&self) -> u64 {
        let book = self.acks + self.fills + self.cancels + self.modifies + self.rejects;
        book + self.positions + self.balances + self.liquidations + self.other
    }

    /// What a batch of `commands` commands did, in one line for the benchmark output.
    pub fn summary(&self, commands: usize) -> String {
        format!(
            "{} acks, {} fills, {} cancels, {} modifies, {} rejects ({:.1}% of commands: {})",
            self.acks,
            self.fills,
            self.cancels,
            self.modifies,
            self.rejects,
            100.0 * self.rejects as f64 / commands as f64,
            self.reject_reasons,
        )
    }

    /// What the engine's ledger did, in one line: e.g. "8,532 position changes, 3,920
    /// balance changes, 0 liquidations, 0 other".
    pub fn ledger_summary(&self) -> String {
        format!(
            "{} position changes, {} balance changes, {} liquidations, {} other",
            self.positions, self.balances, self.liquidations, self.other
        )
    }
}

impl EventSink for CountingSink {
    fn emit(&mut self, event: Event) {
        let counter = match std::hint::black_box(event) {
            Event::Ack(_) => &mut self.acks,
            Event::Fill(_) => &mut self.fills,
            Event::Cancelled(_) => &mut self.cancels,
            Event::Modified(_) => &mut self.modifies,
            Event::Reject(reject) => {
                self.reject_reasons.count(reject.reason);
                &mut self.rejects
            }
            Event::PositionChanged(_) => &mut self.positions,
            Event::BalanceChanged(_) => &mut self.balances,
            Event::Liquidation(_) => &mut self.liquidations,
            _ => &mut self.other,
        };
        *counter += 1;
    }
}

/// Rejects split by the reasons an order book can give. Any other reason (the risk
/// checks', from Milestone 2) is counted in `other`.
#[derive(Debug, Default)]
pub struct RejectCounts {
    pub unknown_order: u64,
    pub post_only_would_cross: u64,
    pub duplicate: u64,
    pub invalid_price: u64,
    pub invalid_qty: u64,
    pub other: u64,
}

impl RejectCounts {
    fn count(&mut self, reason: RejectReason) {
        let counter = match reason {
            RejectReason::UnknownOrder => &mut self.unknown_order,
            RejectReason::PostOnlyWouldCross => &mut self.post_only_would_cross,
            RejectReason::Duplicate => &mut self.duplicate,
            RejectReason::InvalidPrice => &mut self.invalid_price,
            RejectReason::InvalidQty => &mut self.invalid_qty,
            _ => &mut self.other,
        };
        *counter += 1;
    }
}

/// The non-zero counts, e.g. "3064 unknown order, 12 post-only would cross", or "none".
impl fmt::Display for RejectCounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let named = [
            (self.unknown_order, "unknown order"),
            (self.post_only_would_cross, "post-only would cross"),
            (self.duplicate, "duplicate"),
            (self.invalid_price, "invalid price"),
            (self.invalid_qty, "invalid qty"),
            (self.other, "other"),
        ];
        let parts: Vec<String> = named
            .iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, what)| format!("{count} {what}"))
            .collect();
        if parts.is_empty() { write!(f, "none") } else { write!(f, "{}", parts.join(", ")) }
    }
}

/// How deep a book is: its resting orders and non-empty price levels on each side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Depth {
    pub bid_orders: usize,
    pub bid_levels: usize,
    pub ask_orders: usize,
    pub ask_levels: usize,
}

impl Depth {
    /// Reads the depth from the book's snapshot, which allocates: call it outside timing.
    pub fn of(book: &impl OrderBook) -> Depth {
        Depth::of_snapshot(&book.snapshot())
    }

    /// The depth of a book snapshot, such as the one inside an engine's snapshot (the
    /// engine doesn't hand out its books).
    pub fn of_snapshot(snapshot: &BookSnapshot) -> Depth {
        Depth {
            bid_orders: snapshot.bids.len(),
            bid_levels: count_levels(&snapshot.bids),
            ask_orders: snapshot.asks.len(),
            ask_levels: count_levels(&snapshot.asks),
        }
    }
}

/// The number of distinct prices among one side's orders. A snapshot lists each side in
/// price order, so orders at the same price are next to each other and `dedup` leaves one
/// per level.
fn count_levels(orders: &[RestingOrder]) -> usize {
    let mut prices: Vec<_> = orders.iter().map(|o| o.price).collect();
    prices.dedup();
    prices.len()
}

impl fmt::Display for Depth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bids {} orders on {} levels, asks {} orders on {} levels",
            self.bid_orders, self.bid_levels, self.ask_orders, self.ask_levels
        )
    }
}

/// What timing nothing measures: `Instant::now()` followed at once by `elapsed()`, averaged
/// over a million tries. Every timed stretch of code includes about this much on top of its
/// own cost. A benchmark that has to time each command on its own (because it undoes each
/// one, untimed, before the next) includes it once per command, and prints it so the reader
/// can allow for it.
pub fn clock_read_cost() -> Duration {
    const READS: u32 = 1_000_000;
    let mut total = Duration::ZERO;
    for _ in 0..READS {
        let start = Instant::now();
        total += std::hint::black_box(start.elapsed());
    }
    total / READS
}

/// Applies order commands to a book. Non-order commands are ignored: the book layer
/// doesn't handle them.
pub fn apply(book: &mut impl OrderBook, command: &Command, sink: &mut impl EventSink) {
    match command {
        Command::PlaceOrder(order) => book.place(order, sink),
        Command::CancelOrder(cancel) => book.cancel(cancel.order_id, sink),
        Command::ModifyOrder(modify) => book.modify(modify.order_id, modify.new_price, modify.new_size, sink),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::book::BookConfig;
    use engine::command::PlaceOrder;
    use engine::reference::ReferenceBook;
    use engine::types::{AccountId, MarketId, OrderSeq, Price, Qty, Side, TimeInForce, order_id};

    /// A one-lot GTC order at `price` ticks.
    fn place(book: &mut ReferenceBook, sink: &mut CountingSink, seq: u32, side: Side, price: i64) {
        let order = PlaceOrder {
            order_id: order_id(AccountId::new(1), OrderSeq::new(seq)),
            price: Price::new(price),
            qty: Qty::new(1),
            market: MarketId::new(1),
            side,
            tif: TimeInForce::Gtc,
            post_only: seq == 99,
        };
        book.place(&order, sink);
    }

    #[test]
    fn depth_counts_orders_and_distinct_prices_on_each_side() {
        let mut book = ReferenceBook::new(BookConfig {
            market: MarketId::new(1),
            min_price: Price::new(1),
            max_price: Price::new(1_000),
        });
        let mut sink = CountingSink::default();
        assert_eq!(Depth::of(&book), Depth { bid_orders: 0, bid_levels: 0, ask_orders: 0, ask_levels: 0 });
        place(&mut book, &mut sink, 1, Side::Buy, 100);
        place(&mut book, &mut sink, 2, Side::Buy, 99);
        place(&mut book, &mut sink, 3, Side::Buy, 100);
        place(&mut book, &mut sink, 4, Side::Sell, 105);
        assert_eq!(Depth::of(&book), Depth { bid_orders: 3, bid_levels: 2, ask_orders: 1, ask_levels: 1 });
    }

    #[test]
    fn the_sink_splits_rejects_by_reason() {
        let mut book = ReferenceBook::new(BookConfig {
            market: MarketId::new(1),
            min_price: Price::new(1),
            max_price: Price::new(1_000),
        });
        let mut sink = CountingSink::default();
        place(&mut book, &mut sink, 1, Side::Sell, 100);
        place(&mut book, &mut sink, 99, Side::Buy, 100); // post-only, would cross
        book.cancel(order_id(AccountId::new(1), OrderSeq::new(7)), &mut sink); // never placed
        book.cancel(order_id(AccountId::new(1), OrderSeq::new(8)), &mut sink);
        assert_eq!((sink.acks, sink.rejects), (1, 3));
        assert_eq!(sink.reject_reasons.to_string(), "2 unknown order, 1 post-only would cross");
        assert_eq!(RejectCounts::default().to_string(), "none");
    }
}
