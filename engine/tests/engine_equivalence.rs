//! Property test of the engine (`docs/RISK.md` 14.3): random command streams applied to
//! every engine mode, which must agree event for event and state for state, while an
//! independent checker and a shadow ledger check the money.
//!
//! **Five engines.** `Engine<Book, Fast>` (production), `Engine<ReferenceBook, Naive>` (the
//! executable reference, as `ReferenceBook` is for the book), and `Engine<Book, M>` for the
//! three other modes, `Naive`, `NaiveTotals` and `NaiveLiquidation`. After every command all
//! five must have emitted identical events and hold identical snapshots (RISK.md 15.5), and
//! `assert_invariants` must pass on each: I1 to I11 and I16, with I4 only where running
//! totals are kept and I10 only where the index is. A rejected command must leave the
//! snapshot exactly as it was (I15).
//!
//! **What agreement proves, and what it doesn't.** The modes share every money formula and
//! differ only in the Mode seam's four functions, so equal events prove that the running
//! totals equal the book's and that the index walk liquidates the same slots, in the same
//! order, as a scan with a binary search. A wrong formula would give the same wrong events
//! in every mode. Two more checks, run after every command, cover that:
//! - the **independent checker** (`engine_equivalence/checker.rs`), with its own formulas,
//!   checks I5, I12 to I14 and I17 to I20, and that each margin reject has the right
//!   reason;
//! - the **shadow ledger** (`engine_equivalence/shadow_ledger.rs`), rebuilt from the events
//!   alone, checks I2 and I3 after every block of events, and must equal the snapshot at the
//!   end of every command.
//!
//! **Generator** (`engine_equivalence/generator.rs`, tuned like D-009): two markets (20x
//! with fees 400/125 ppm; 50x with SP500's real tiers), four traders, two twins and the
//! fund; 256 scenarios of up to 300 commands each, the setup included. Deposits,
//! withdrawals at the reserve edge, `SetLeverage`, marks as a random walk with small steps
//! and 5-15% jumps, places, cancels and modifies with prices mostly at the band edges,
//! invalid commands of every kind, a twin step that makes two slots share a liquidation key,
//! sizes around the tier bounds, tier tables staged and committed on live markets, and
//! markets reconfigured and reopened.
//!
//! **Coverage** (`engine_equivalence/coverage.rs`): the run prints how often each rare rule
//! ran, and fails if any row is below its minimum.
//!
//! **Runtime.** About 25 s in a debug build, where the engine's debug assertions run too
//! (each re-key checks its key against the direct test), and 5 s in a release build.
//!
//! Failure persistence is off, as in the book tests (D-009): when this finds a bug, the
//! shrunk failing case printed in the panic message becomes a named test in
//! `engine_regressions.rs`.
//!
//! The second test runs the same generator on engines with different capacities and hash
//! seeds, which must change nothing but speed.

// A test file is its own crate root, so its modules are named by path.
#[path = "engine_equivalence/checker.rs"]
mod checker;
#[path = "engine_equivalence/coverage.rs"]
mod coverage;
#[path = "engine_equivalence/generator.rs"]
mod generator;
#[path = "engine_equivalence/shadow_ledger.rs"]
mod shadow_ledger;
#[path = "engine_equivalence/state.rs"]
mod state;

use std::cell::RefCell;

use engine::book::{Book, OrderBook};
use engine::command::Command;
use engine::engine::{Engine, EngineOptions, EngineSnapshot};
use engine::event::Event;
use engine::mode::{Fast, Mode, Naive, NaiveLiquidation, NaiveTotals};
use engine::reference::ReferenceBook;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, TestRunner};

use coverage::Coverage;
use generator::{Scenario, Setup, Step, setup, step};
use shadow_ledger::ShadowLedger;

/// Scenarios per run. `PROPTEST_CASES` sets another number for a longer run, for example
/// `./dev env PROPTEST_CASES=10000 cargo test -p engine --release --locked --offline
/// --test engine_equivalence` (about 3 minutes).
const SCENARIOS: u32 = 256;
/// Per scenario, the setup included.
const MAX_COMMANDS: usize = 300;

/// A scenario: its setup, then up to 300 steps (each one or more commands).
fn scenario() -> impl Strategy<Value = (Setup, Vec<Step>)> {
    (setup(), prop::collection::vec(step(), 1..=MAX_COMMANDS))
}

/// Applies one command to one engine and returns its events.
fn run<B: OrderBook, M: Mode>(engine: &mut Engine<B, M>, command: &Command) -> Vec<Event> {
    let mut events = Vec::new();
    engine.apply(command, &mut events);
    events
}

/// The five engines of the module docs.
struct Engines {
    fast: Engine<Book, Fast>,
    reference: Engine<ReferenceBook, Naive>,
    naive: Engine<Book, Naive>,
    naive_totals: Engine<Book, NaiveTotals>,
    naive_liquidation: Engine<Book, NaiveLiquidation>,
}

impl Engines {
    fn new() -> Self {
        let options = EngineOptions::default();
        Engines {
            fast: Engine::new(options),
            reference: Engine::new(options),
            naive: Engine::new(options),
            naive_totals: Engine::new(options),
            naive_liquidation: Engine::new(options),
        }
    }

    /// Applies `command` to all five, checks that they agree and that each keeps its
    /// invariants, and returns the events and the state after the command.
    fn apply(&mut self, command: &Command) -> (Vec<Event>, EngineSnapshot) {
        let events = run(&mut self.fast, command);
        let state = self.fast.snapshot();
        self.fast.assert_invariants();
        agrees("Engine<ReferenceBook, Naive>", &mut self.reference, command, &events, &state);
        agrees("Engine<Book, Naive>", &mut self.naive, command, &events, &state);
        agrees("Engine<Book, NaiveTotals>", &mut self.naive_totals, command, &events, &state);
        agrees("Engine<Book, NaiveLiquidation>", &mut self.naive_liquidation, command, &events, &state);
        (events, state)
    }
}

/// Applies `command` to `engine`, which must emit `events` and end in `state`, with its
/// invariants intact.
fn agrees<B: OrderBook, M: Mode>(
    name: &str,
    engine: &mut Engine<B, M>,
    command: &Command,
    events: &[Event],
    state: &EngineSnapshot,
) {
    assert_eq!(run(engine, command), events, "{name}'s events differ from Fast's on {command:?}");
    assert!(engine.snapshot() == *state, "{name}'s state differs from Fast's after {command:?}");
    engine.assert_invariants();
}

/// One scenario in progress: the five engines, the shadow ledger, what the generator
/// remembers, and the state before the next command.
struct Harness {
    engines: Engines,
    ledger: ShadowLedger,
    scenario: Scenario,
    before: EngineSnapshot,
    commands: usize,
}

impl Harness {
    fn new() -> Self {
        let engines = Engines::new();
        let before = engines.fast.snapshot();
        Harness {
            engines,
            ledger: ShadowLedger::default(),
            scenario: Scenario::default(),
            before,
            commands: 0,
        }
    }

    /// Applies one command with every check of the module docs, and counts what it did.
    fn apply(&mut self, command: Command, coverage: &mut Coverage) {
        let (events, after) = self.engines.apply(&command);
        if let [Event::Reject(_)] = events.as_slice() {
            assert!(after == self.before, "I15: the rejected {command:?} changed the state");
        }
        checker::check(&self.before, &command, &events, &after);
        self.ledger.apply(&command, &events);
        self.ledger.assert_matches(&after);
        coverage.record(&self.before, &command, &events);
        self.scenario.record(&command, &events);
        self.before = after;
        self.commands += 1;
    }
}

/// Runs one scenario, up to `MAX_COMMANDS` commands.
fn check_scenario(setup: &Setup, steps: &[Step], coverage: &mut Coverage) {
    let mut harness = Harness::new();
    coverage.start_scenario();
    for command in Scenario::setup_commands(setup) {
        harness.apply(command, coverage);
    }
    for step in steps {
        for command in harness.scenario.resolve(step, &harness.before) {
            if harness.commands == MAX_COMMANDS {
                return;
            }
            harness.apply(command, coverage);
        }
    }
}

#[test]
fn every_engine_mode_agrees_and_keeps_every_invariant() {
    let cases = std::env::var("PROPTEST_CASES").ok().and_then(|n| n.parse().ok()).unwrap_or(SCENARIOS);
    let config = Config { cases, failure_persistence: None, ..Config::default() };
    let coverage = RefCell::new(Coverage::default());
    let result = TestRunner::new(config).run(&scenario(), |(setup, steps)| {
        check_scenario(&setup, &steps, &mut coverage.borrow_mut());
        Ok(())
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
    let coverage = coverage.into_inner();
    coverage.print();
    coverage.assert_minimums();
}

/// The engine's version of the book's `capacity_and_hash_seed_change_nothing_but_speed`
/// (RISK.md 14.3): with a scratch buffer of capacity 0 or 1 (the touched list shares its
/// capacity, so 0 too), no room reserved for orders, accounts or slots, and other hash
/// seeds, every buffer and map has to grow again and again and every id lands in another
/// bucket. None of it may change a single event.
#[test]
fn capacity_and_hash_seed_change_nothing_but_speed() {
    let grow_everything = |capacity, id_hash_seed| EngineOptions {
        order_capacity: capacity,
        id_hash_seed,
        scratch_capacity: capacity,
        account_capacity: capacity,
        slot_capacity: capacity,
    };
    let options = [EngineOptions::default(), grow_everything(0, 0xDEAD_BEEF), grow_everything(1, u64::MAX)];
    let mut runner = TestRunner::deterministic();
    let mut all_events = Vec::new();
    for _ in 0..32 {
        let (setup, steps) = scenario().new_tree(&mut runner).expect("a scenario").current();
        let mut engines: Vec<Engine<Book, Fast>> = options.iter().map(|&o| Engine::new(o)).collect();
        let mut scenario = Scenario::default();
        // Every engine gets each command; all must emit what the first one does.
        let mut apply = |engines: &mut [Engine<Book, Fast>], scenario: &mut Scenario, command: Command| {
            let expected = run(&mut engines[0], &command);
            for engine in &mut engines[1..] {
                assert_eq!(run(engine, &command), expected, "{command:?}");
            }
            scenario.record(&command, &expected);
            all_events.extend(expected);
        };
        for command in Scenario::setup_commands(&setup) {
            apply(&mut engines, &mut scenario, command);
        }
        for step in &steps {
            for command in scenario.resolve(step, &engines[0].snapshot()) {
                apply(&mut engines, &mut scenario, command);
            }
        }
        for engine in &engines[1..] {
            assert!(engine.snapshot() == engines[0].snapshot(), "the states differ after the scenario");
        }
    }
    let count = |is: fn(&Event) -> bool| all_events.iter().filter(|e| is(e)).count();
    let (fills, liquidations) =
        (count(|e| matches!(e, Event::Fill(_))), count(|e| matches!(e, Event::Liquidation(_))));
    assert!(
        fills > 100 && liquidations > 10,
        "the scenarios should trade and liquidate: {fills}, {liquidations}"
    );
}
