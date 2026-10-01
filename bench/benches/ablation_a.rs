//! Ablation A (`docs/RISK.md` 14.4): the O(1) pre-trade check against a naive loop over the
//! account's resting orders.
//!
//! `Fast` keeps each slot's open buys and sells as running totals. `NaiveTotals` recomputes
//! them by walking the account's resting orders in the book: once in the pre-trade check,
//! and once more when the post-command pass works out the release (RISK.md 14.2). Both keep
//! the liquidation index, so both re-key the account's slot after each order.
//!
//! **Measured:** the time per margin-checked place that rests without filling, and per
//! cancel, by an account with `k` resting orders, in a market with `n` positions, for `k`
//! in {1, 16, 256, 4,096} and `n` in {100, 10,000, 1,000,000}. Expected: `Fast` flat in `k`
//! and growing like log n with the re-key; `NaiveTotals` linear in `k`.
//!
//! **How** (`bench::risk`). For each `n`, `k` and mode, a new engine is built with
//! commands: `n` positions in all, 16 of them held by timed accounts that each rest `k`
//! bids. Filler accounts rest the other bids of 4,096 per timed account, so the book always
//! holds the same 65,536 bids in the same places, and `k` is the only thing that changes.
//! (Without them, a bigger `k` would also mean a bigger book, which costs even `Fast` more
//! cache misses in the book.) Likewise each timed account's position is large next to its
//! bids, so its liquidation key sits among the other positions' keys for every `k`. A timed
//! place is a bid of 1,000 lots that rests; its top-up moves the account's key, so the
//! re-key moves an index entry. Places are timed in rounds of 16, one per timed account,
//! and are cancelled after the clock stops; cancels are timed the same way, after untimed
//! places. So every timed command meets the same state, and no engine is ever cloned.
//!
//! Before timing, each engine prints what it holds (positions, how many have a key, slots,
//! resting orders) and what one timed place and cancel did (the top-up, and the account's
//! key without and with the bid), checked from their events.
//!
//! Run alone: `./dev cargo bench -p bench --locked --offline --bench ablation_a` (about
//! four minutes: 48 benchmarks of about 3 s, and 24 engines to build).

use std::time::Duration;

use bench::clock_read_cost;
use bench::risk::{MarketView, Scenario, ScenarioConfig};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use engine::mode::{Fast, Mode, NaiveTotals};

/// Positions per market (`n`).
const POSITIONS: [usize; 3] = [100, 10_000, 1_000_000];

/// Resting orders per timed account (`k`).
const ORDERS_PER_ACCOUNT: [usize; 4] = [1, 16, 256, 4_096];

/// Resting bids per timed account in the book, whatever `k` is: filler accounts rest those
/// the timed account doesn't. So the book always holds 16 × 4,096 = 65,536 bids, laid out
/// the same way, and only `k` changes.
const BOOK_DEPTH_PER_TIMED_ACCOUNT: usize = 4_096;

fn benches(c: &mut Criterion) {
    eprintln!(
        "ablation_a: timing an empty stretch of code measures {:?}; each round of 16 includes it once",
        clock_read_cost()
    );
    for n in POSITIONS {
        for k in ORDERS_PER_ACCOUNT {
            bench_mode::<Fast>(c, "Fast", n, k);
            bench_mode::<NaiveTotals>(c, "NaiveTotals", n, k);
        }
    }
}

/// Builds one engine in mode `M` with `n` positions and `k` resting orders per timed
/// account, prints what it holds, and times places and cancels on it.
fn bench_mode<M: Mode>(c: &mut Criterion, mode: &str, n: usize, k: usize) {
    let config = ScenarioConfig {
        positions: n,
        near_positions: 0,
        orders_per_timed_account: k,
        book_depth_per_timed_account: BOOK_DEPTH_PER_TIMED_ACCOUNT,
    };
    let mut scenario = Scenario::<M>::build(config);
    let view = MarketView::of(&scenario.engine);
    let probe = scenario.probe_round();
    eprintln!("ablation_a/{mode}/n={n}/k={k}: {view}; {probe}");

    let mut group = c.benchmark_group("ablation_a");
    group.warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(2));
    let parameter = format!("n={n}/k={k}");
    group.bench_function(BenchmarkId::new(format!("place/{mode}"), &parameter), |b| {
        b.iter_custom(|iterations| scenario.time_places(iterations))
    });
    group.bench_function(BenchmarkId::new(format!("cancel/{mode}"), &parameter), |b| {
        b.iter_custom(|iterations| scenario.time_cancels(iterations))
    });
    group.finish();
}

criterion_group!(ablation_a, benches);
criterion_main!(ablation_a);
