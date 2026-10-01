//! Order-book throughput: book layer only (no risk, no pipeline). Every batch is 10,000
//! commands, and each flow runs on both books: the naive `ReferenceBook`, which is the
//! specification and scans every resting order, and the production `Book` (Milestone 1).
//!
//! - `book/synthetic_flow`: the M0 flow (`FlowConfig::default()`), on a fresh book. It
//!   keeps the book shallow, a few hundred resting orders, which is kind to a linear scan.
//!   The flow and the timing are unchanged since M0, so the numbers compare like for like
//!   with the M0 baseline in `docs/BENCHMARKS.md`.
//! - `book/deep_flow`: the deep-book flow (`FlowConfig::deep()`). A warm-up of 100,000
//!   commands, not timed, brings the book to its steady depth of thousands of orders. Each
//!   iteration then applies the next 10,000 commands to a copy of that book. Depth makes
//!   every reference scan longer. The fast book's steps don't depend on depth, though its
//!   bigger working set costs it some cache misses.
//!
//! Before timing, each flow prints what its batch does (acks, fills, cancels, modifies,
//! rejects by reason), since a rejected cancel costs much less than a fill. The deep flow
//! also prints the book's depth before and after the batch. Both books must print the same
//! counts, since they implement the same specification.
//!
//! Run: `./dev cargo bench -p bench --locked --offline`

use std::time::Duration;

use bench::{CountingSink, Depth, apply};
use criterion::{BatchSize, Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use engine::book::{Book, BookConfig, OrderBook};
use engine::command::Command;
use engine::reference::ReferenceBook;
use engine::types::Price;
use loadgen::{FlowConfig, SyntheticFlow};

/// Commands in each timed batch.
const COMMANDS: usize = 10_000;

/// Commands applied, untimed, before the deep flow's timed batch. The generator reaches its
/// 10,000 live orders after about 12,000 commands. After that, orders that filled without
/// it knowing build up on its list until the book's depth levels off; the depth printed
/// before and after the batch shows that it has.
const DEEP_WARM_UP: usize = 100_000;

fn book_config(flow: &FlowConfig) -> BookConfig {
    BookConfig { market: flow.market, min_price: Price::new(1), max_price: Price::new(flow.mid.ticks() * 2) }
}

fn run_flow<B: OrderBook>(c: &mut Criterion, name: &str, new_book: impl Fn() -> B) {
    let flow = FlowConfig::default();
    let commands: Vec<Command> = SyntheticFlow::new(flow).take(COMMANDS).collect();

    // What this flow does to a book, printed once so the throughput can be read honestly:
    // a rejected cancel is much cheaper than a fill.
    let mut probe = new_book();
    let mut counts = CountingSink::default();
    for command in &commands {
        apply(&mut probe, command, &mut counts);
    }
    eprintln!("synthetic_flow/{name}: {COMMANDS} commands -> {}", counts.summary(COMMANDS));

    let mut group = c.benchmark_group("book/synthetic_flow");
    group.throughput(Throughput::Elements(COMMANDS as u64));
    group.bench_function(name, |b| {
        b.iter_batched(
            &new_book,
            |mut book| {
                let mut sink = CountingSink::default();
                for command in &commands {
                    apply(&mut book, command, &mut sink);
                }
                sink.total()
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

/// How criterion samples one book on the deep flow.
#[derive(Clone, Copy, Debug)]
enum Sampling {
    /// Criterion's defaults: a 3 s warm-up, then 100 samples in about 5 s.
    Default,
    /// For a book that needs hundreds of milliseconds per batch: criterion's minimum of 10
    /// samples, each of the same number of iterations ("flat" sampling), in about 5 s
    /// after a 1 s warm-up. It keeps the whole benchmark to about two minutes.
    Few,
}

/// Times the deep flow's batch on a copy of `empty_book` that the warm-up has filled.
fn run_deep_flow<B: OrderBook + Clone>(c: &mut Criterion, name: &str, empty_book: B, sampling: Sampling) {
    let mut flow = SyntheticFlow::new(FlowConfig::deep());
    let warm_up: Vec<Command> = flow.by_ref().take(DEEP_WARM_UP).collect();
    let batch: Vec<Command> = flow.take(COMMANDS).collect();

    // Bring the book to its steady depth. Not timed, and not reported.
    let mut prefilled = empty_book;
    let mut ignored = CountingSink::default();
    for command in &warm_up {
        apply(&mut prefilled, command, &mut ignored);
    }

    // What the batch does, and how deep the book is before and after it.
    let mut probe = prefilled.clone();
    let mut counts = CountingSink::default();
    for command in &batch {
        apply(&mut probe, command, &mut counts);
    }
    eprintln!("deep_flow/{name}: {COMMANDS} commands -> {}", counts.summary(COMMANDS));
    eprintln!("deep_flow/{name}: depth before the batch: {}", Depth::of(&prefilled));
    eprintln!("deep_flow/{name}: depth after the batch:  {}", Depth::of(&probe));

    let mut group = c.benchmark_group("book/deep_flow");
    group.throughput(Throughput::Elements(COMMANDS as u64));
    if let Sampling::Few = sampling {
        group
            .sample_size(10)
            .sampling_mode(SamplingMode::Flat)
            .warm_up_time(Duration::from_secs(1))
            .measurement_time(Duration::from_secs(5));
    }
    group.bench_function(name, |b| {
        b.iter_batched(
            // Every iteration gets its own copy of the prefilled book, made before the
            // clock starts. A copied `Vec` has room for its length and no more, so `Book`'s
            // slab has room for the most orders the warm-up ever held (9,150). The batch
            // peaks lower (9,121; both checked when this was written), so no timed command
            // has to grow it.
            || prefilled.clone(),
            |mut book| {
                let mut sink = CountingSink::default();
                for command in &batch {
                    apply(&mut book, command, &mut sink);
                }
                // Returning the book, rather than dropping it here, makes criterion free it
                // after the clock stops.
                (book, sink.total())
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

fn benches(c: &mut Criterion) {
    let shallow = FlowConfig::default();
    run_flow(c, "reference", || ReferenceBook::new(book_config(&shallow)));
    run_flow(c, "book", || Book::new(book_config(&shallow)));

    let deep = FlowConfig::deep();
    run_deep_flow(c, "reference", ReferenceBook::new(book_config(&deep)), Sampling::Few);
    run_deep_flow(c, "book", Book::new(book_config(&deep)), Sampling::Default);
}

criterion_group!(book, benches);
criterion_main!(book);
