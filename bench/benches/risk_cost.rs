//! The risk layer's cost (`docs/RISK.md` 14.4): the deep-book flow through the whole engine,
//! against the same flow through the book alone.
//!
//! - `risk_cost/book`: the production `Book` alone, as in M1's `book/deep_flow`.
//! - `risk_cost/engine`: `Engine<Book, Fast>`. Every place and replacing modify is
//!   margin-checked and tops up, every fill charges fees and changes two positions, the
//!   post-command pass releases collateral and re-keys each touched slot, and every event
//!   of the ledger goes to the sink.
//!
//! **Setup.** The engine's market is `bench::risk`'s: its mark is the flow's mid (10,000)
//! and never moves, and its band (9,554 to 10,446) holds every price the flow sends. Each of
//! the flow's 1,000 accounts deposits $1 billion and sets leverage 10. So the risk checks
//! reject nothing and nothing is liquidated (both are checked after every timed stretch),
//! and on the first batch the engine acks, fills, cancels, modifies and rejects exactly
//! what the book alone does (checked, and printed).
//!
//! **Timing.** Both start with the same 100,000 warm-up commands, not timed. Then each
//! iteration is the next command of the same endless flow: commands are generated in chunks
//! of 10,000 before the clock starts, and each chunk is timed as a whole. Neither the book
//! nor the engine is ever cloned or rebuilt (a cloned `Vec` loses its spare capacity,
//! D-010); the flow's depth levels off after the warm-up (printed before and after the first
//! batch), so later batches are like the first. The book alone is timed the same way, so
//! the two numbers compare like for like, and their difference is what the risk layer adds
//! per command.
//!
//! Run alone: `./dev cargo bench -p bench --locked --offline --bench risk_cost` (about
//! 20 seconds).

use std::time::{Duration, Instant};

use bench::risk::{MARK, MARKET, MarketView, deposit, fund, market_params, open_market};
use bench::{CountingSink, Depth};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use engine::book::{Book, BookConfig, BookOptions};
use engine::command::Command;
use engine::engine::{Engine, EngineOptions, FUND};
use engine::event::Event;
use engine::mode::Fast;
use engine::types::AccountId;
use loadgen::{FlowConfig, SyntheticFlow};

/// Commands applied, untimed, before timing starts: as in `book/deep_flow`, enough for the
/// book's depth to level off.
const WARM_UP: usize = 100_000;

/// Commands generated before the clock starts, then timed as a whole; also the size of the
/// first batch, whose contents are printed.
const CHUNK: usize = 10_000;

/// Room for the deep flow's resting orders (about 9,150 at most) in both books.
const ORDER_CAPACITY: usize = 16_384;

/// Where the flow's commands go: the book alone, or the whole engine.
trait Target {
    fn apply(&mut self, command: &Command, sink: &mut CountingSink);
    /// Resting orders and levels on each side. Allocates: outside timing only.
    fn depth(&self) -> Depth;
}

impl Target for Book {
    fn apply(&mut self, command: &Command, sink: &mut CountingSink) {
        bench::apply(self, command, sink);
    }

    fn depth(&self) -> Depth {
        Depth::of(self)
    }
}

impl Target for Engine<Book, Fast> {
    fn apply(&mut self, command: &Command, sink: &mut CountingSink) {
        Engine::apply(self, command, sink);
    }

    fn depth(&self) -> Depth {
        MarketView::of(self).depth()
    }
}

/// One target and the flow that feeds it.
struct FlowRunner<T> {
    target: T,
    flow: SyntheticFlow,
    /// The next chunk of commands, generated before the clock starts.
    chunk: Vec<Command>,
    /// What every timed command did, so far.
    timed_events: CountingSink,
}

impl<T: Target> FlowRunner<T> {
    /// A target fed from the start of the deep flow, which it has taken through the warm-up.
    fn warmed_up(target: T) -> Self {
        let mut runner = FlowRunner {
            target,
            flow: SyntheticFlow::new(FlowConfig::deep()),
            chunk: Vec::with_capacity(CHUNK),
            timed_events: CountingSink::default(),
        };
        runner.run(WARM_UP);
        runner
    }

    /// Applies the flow's next `count` commands, untimed, and returns what they did.
    fn run(&mut self, count: usize) -> CountingSink {
        let mut counts = CountingSink::default();
        for command in self.flow.by_ref().take(count) {
            self.target.apply(&command, &mut counts);
        }
        counts
    }

    /// Applies the flow's next `iterations` commands and returns the time that took, not
    /// counting the time to generate them: each chunk is generated before the clock starts.
    fn time(&mut self, iterations: u64) -> Duration {
        let mut timed = Duration::ZERO;
        let mut left = iterations;
        while left > 0 {
            let count = left.min(CHUNK as u64);
            self.chunk.clear();
            self.chunk.extend(self.flow.by_ref().take(count as usize));
            let start = Instant::now();
            for command in &self.chunk {
                self.target.apply(command, &mut self.timed_events);
            }
            timed += start.elapsed();
            left -= count;
        }
        let events = &self.timed_events;
        assert_eq!(
            events.reject_reasons.other, 0,
            "the risk checks rejected an order: {}",
            events.reject_reasons
        );
        assert_eq!(events.liquidations, 0, "the flow liquidated a position");
        timed
    }

    /// Runs the first batch after the warm-up, untimed, and prints what it did and the depth
    /// before and after it. Returns its counts.
    fn first_batch(&mut self, name: &str) -> CountingSink {
        let depth_before = self.target.depth();
        let counts = self.run(CHUNK);
        eprintln!("risk_cost/{name}: first {CHUNK} commands -> {}", counts.summary(CHUNK));
        eprintln!("risk_cost/{name}: depth before them: {depth_before}");
        eprintln!("risk_cost/{name}: depth after them:  {}", self.target.depth());
        counts
    }
}

/// The book the engine builds for the market, built the same way.
fn book() -> Book {
    let params = market_params();
    let config = BookConfig { market: MARKET, min_price: params.min_price, max_price: params.max_price };
    Book::with_options(config, BookOptions { order_capacity: ORDER_CAPACITY, id_hash_seed: 0x5EED })
}

/// The engine with the market open, the insurance fund capitalised, and every account of
/// the flow funded at leverage 10.
fn engine() -> Engine<Book, Fast> {
    let options = EngineOptions {
        order_capacity: ORDER_CAPACITY,
        id_hash_seed: 0x5EED,
        scratch_capacity: 4_096,
        account_capacity: 2_048,
        slot_capacity: 2_048,
    };
    let mut engine = Engine::new(options);
    let mut setup = Vec::new();
    setup.extend(open_market());
    setup.push(deposit(FUND));
    for account in (1..=FlowConfig::deep().accounts).map(AccountId::new) {
        setup.extend(fund(account));
    }
    let mut events: Vec<Event> = Vec::new();
    for command in &setup {
        engine.apply(command, &mut events);
    }
    assert!(!events.iter().any(|event| matches!(event, Event::Reject(_))), "a setup command was rejected");
    engine
}

/// The book-layer counts of a batch, which the book alone and the engine must share.
fn book_counts(counts: &CountingSink) -> [u64; 5] {
    [counts.acks, counts.fills, counts.cancels, counts.modifies, counts.rejects]
}

fn benches(c: &mut Criterion) {
    let flow = FlowConfig::deep();
    assert_eq!(
        (flow.market, flow.mid),
        (MARKET, MARK),
        "the flow must trade the benchmarks' market at its mark"
    );

    let mut book = FlowRunner::warmed_up(book());
    let mut engine = FlowRunner::warmed_up(engine());
    let book_batch = book.first_batch("book");
    let engine_batch = engine.first_batch("engine");
    eprintln!("risk_cost/engine: first {CHUNK} commands, the ledger -> {}", engine_batch.ledger_summary());
    eprintln!("risk_cost/engine: after them: {}", MarketView::of(&engine.target));
    assert_eq!(
        book_counts(&engine_batch),
        book_counts(&book_batch),
        "the engine and the book did different things"
    );

    let mut group = c.benchmark_group("risk_cost");
    group.throughput(Throughput::Elements(1));
    group.warm_up_time(Duration::from_secs(2)).measurement_time(Duration::from_secs(5));
    group.bench_function("book", |b| b.iter_custom(|iterations| book.time(iterations)));
    group.bench_function("engine", |b| b.iter_custom(|iterations| engine.time(iterations)));
    group.finish();
}

criterion_group!(risk_cost, benches);
criterion_main!(risk_cost);
