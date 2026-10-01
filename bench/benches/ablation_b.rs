//! Ablation B (`docs/RISK.md` 14.4): the liquidation index against a full rescan.
//!
//! `Fast` keeps each market's positions in the liquidation index, ordered by the mark at
//! which each must be liquidated, so a `SetMark` walks from the index's first entries and
//! stops at the first one the mark hasn't reached (9.4). `NaiveLiquidation` keeps no index:
//! every `SetMark` checks every slot in the market, and finds the key of each crossed one by
//! binary search (14.2). The index costs something on every order instead: a re-key of the
//! slot, O(log n), whenever its collateral or position changes.
//!
//! **Measured**, for `n` in {1,000, 100,000, 1,000,000} positions:
//! - the time per `SetMark` for marks that liquidate 0, 1 and 100 positions;
//! - the time per order (a margin-checked place that rests, and its cancel) in the same
//!   books, so the trade is shown both ways: O(log n) per order against O(n) per mark.
//!
//! **How** (`bench::risk`). For each `n` and mode, one engine is built with commands: `n`
//! positions, all longs but the maker's short, of which 100 "near" longs have the highest
//! keys, all different.
//! The mark that liquidates `L` of them is the `L`-th highest key, read from the engine's
//! snapshot and checked against the direct check on every slot. A `SetMark` that liquidates
//! nothing is timed going there and back, both counted. One that liquidates is timed on its
//! own, then undone with the clock stopped: the mark goes back and the liquidated longs are
//! opened again at the same prices, so they get the same keys. The orders are timed as in
//! ablation A, by 16 timed accounts with one resting bid each.
//!
//! Before timing, each engine prints what it holds, and each mark how many slots it
//! liquidates and how many slots each algorithm reads to find them. The engine has no
//! counter for the latter; the numbers follow from the two algorithms (RISK.md 9.4, 14.2).
//!
//! Run alone: `./dev cargo bench -p bench --locked --offline --bench ablation_b` (about
//! three minutes).

use std::time::Duration;

use bench::clock_read_cost;
use bench::risk::{MARK, MarketView, Scenario, ScenarioConfig};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use engine::mode::{Fast, Mode, NaiveLiquidation};

/// Positions per market (`n`).
const POSITIONS: [usize; 3] = [1_000, 100_000, 1_000_000];

/// How many positions each timed `SetMark` liquidates.
const LIQUIDATIONS: [usize; 3] = [0, 1, 100];

/// Near longs: at least the most any `SetMark` here liquidates.
const NEAR_POSITIONS: usize = 100;

fn benches(c: &mut Criterion) {
    eprintln!(
        "ablation_b: timing an empty stretch of code measures {:?}; each liquidating SetMark is timed on its \
         own and includes it once",
        clock_read_cost()
    );
    for n in POSITIONS {
        bench_mode::<Fast>(c, "Fast", n);
        bench_mode::<NaiveLiquidation>(c, "NaiveLiquidation", n);
    }
}

/// Builds one engine in mode `M` with `n` positions, prints what it holds, and times
/// `SetMark`s and orders on it.
fn bench_mode<M: Mode>(c: &mut Criterion, mode: &str, n: usize) {
    let config = ScenarioConfig {
        positions: n,
        near_positions: NEAR_POSITIONS,
        orders_per_timed_account: 1,
        book_depth_per_timed_account: 1,
    };
    let mut scenario = Scenario::<M>::build(config);
    let view = MarketView::of(&scenario.engine);
    eprintln!("ablation_b/{mode}/n={n}: {view}");

    let mut marks = c.benchmark_group("ablation_b");
    // Criterion switches to "flat" sampling by itself when a `SetMark` is too slow for its
    // default "linear" one (the scan of a million slots).
    marks.warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3)).sample_size(20);
    for liquidations in LIQUIDATIONS {
        let mark = view.mark_liquidating(liquidations);
        let examined = slots_examined(M::LIQUIDATION_INDEX, &view, liquidations);
        eprintln!("ablation_b/{mode}/n={n}: SetMark {MARK} -> {mark} liquidates {liquidations}; {examined}");
        let id = BenchmarkId::new(format!("set_mark/{mode}"), format!("n={n}/liquidations={liquidations}"));
        marks.bench_function(id, |b| {
            b.iter_custom(|iterations| {
                if liquidations == 0 {
                    scenario.time_quiet_marks(mark, iterations)
                } else {
                    scenario.time_liquidating_marks(mark, liquidations, iterations)
                }
            })
        });
    }
    marks.finish();

    let probe = scenario.probe_round();
    eprintln!("ablation_b/{mode}/n={n}: {probe}");
    let mut orders = c.benchmark_group("ablation_b");
    orders.warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(2));
    orders.bench_function(BenchmarkId::new(format!("place/{mode}"), format!("n={n}")), |b| {
        b.iter_custom(|iterations| scenario.time_places(iterations))
    });
    orders.bench_function(BenchmarkId::new(format!("cancel/{mode}"), format!("n={n}")), |b| {
        b.iter_custom(|iterations| scenario.time_cancels(iterations))
    });
    orders.finish();
}

/// How many slots a `SetMark` that liquidates `liquidations` reads to find them.
/// - The index walk (RISK.md 9.4) reads the first long once per liquidation and once more
///   to stop, then the first short once: `liquidations + 2` entries, whatever `n` is.
/// - The scan (14.2) reads every slot in the market, checks each one with a position, and
///   binary-searches the key of each crossed one between the old and the new mark.
fn slots_examined(index: bool, view: &MarketView, liquidations: usize) -> String {
    if index {
        format!("the index walk reads {} index entries", liquidations + 2)
    } else {
        format!(
            "the scan reads {} slots, checks the {} with a position, and binary-searches {} keys",
            view.slots(),
            view.positions(),
            liquidations
        )
    }
}

criterion_group!(ablation_b, benches);
criterion_main!(ablation_b);
