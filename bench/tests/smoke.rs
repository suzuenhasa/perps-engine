//! The smoke test of `docs/PIPELINE.md` 18.4: the whole pipeline, end to end, on the smoke
//! flow, in seconds, in the container.
//!
//! **The runs** (`RunConfig::smoke`): the smoke flow (6 markets, a fund of $1); signed, 2
//! gateways and 3,000 client messages at 5k/s; pre-verified, 2 lanes and 30,000 commands at
//! 20k/s. Threads spin, then yield, and nothing is pinned, so the test shares the machine.
//! In a build with `--features c-secp256k1`, the signed run runs a second time with
//! libsecp256k1 verifying (5.7), and must show everything below just the same. The signed
//! run also runs in Polymarket Perps' EIP-712 scheme (5.8: the gateways recover each
//! signer and keep a replay table, the audit checks the rebuilt digests), with `k256` and,
//! in that build, with libsecp256k1, and must show everything below too, and that no
//! gateway refused one of its messages as stale, early, reused or for want of room.
//!
//! **The Polymarket-shaped flow** (D-034; `RunConfig::polymarket_smoke`): the same runs on
//! its smoke flow, Polymarket Perps' 88 real markets with their tier tables, price grids and
//! recorded book shapes, 5,000 messages per second of flow time and a stress shock every
//! 250 ms of it (so its liquidations and shortfalls come from shocks, not jumps): signed;
//! pre-verified with two of its stress switches, three makers quoting every market and
//! bursty arrivals; signed in the EIP-712 scheme; and, in the feature build, signed with
//! libsecp256k1. Each must show everything below, and its report the flow's shape.
//!
//! **What each run must show.**
//! - Every stage handled commands: every histogram the mode has is non-empty.
//! - The counts reconcile: offered = sent + ingress drops; sent = forwarded + gateway
//!   rejects (signed); forwarded + operator = sequenced = journaled = applied by the core =
//!   trailers released. The shutdown of 2.8 loses nothing.
//! - At least one `Liquidation` and one `InsuranceShortfall` were released, so the replay
//!   covers both.
//! - **No allocation on the hot threads during the timed flow:** this binary's allocator
//!   counts allocations made on the threads the pipeline flags as hot (through
//!   `pipeline::counters::set_hot_thread_hook`), from when the timed flow starts.
//! - **No minor page faults on the hot threads in the window**, the core's included: the
//!   core thread writes the engine's reserved memory once before the run
//!   (`Engine::prefault`), so none is first touched in the window (PIPELINE.md 15.4, 22).
//!   Unless the kernel migrated pages during the window: memory compaction moves pages
//!   that are already mapped, and a thread that touches one mid-move takes a minor fault
//!   that no code of ours caused. On a machine that has just freed a lot of memory (after a
//!   build, or after the kill test's children) compaction can move tens of thousands of
//!   pages in a 350 ms window, and then any hot thread may take a few such faults. So in a
//!   window where the machine's count of page migrations went up, faults are printed with
//!   that count, not failed. A first touch shows in every window, and most windows see no
//!   migration, so a real regression still fails.
//! - The replay test of 13.3 says "identical" (events and snapshot, also with another hash
//!   seed), **and** a replay through `Engine<ReferenceBook, Naive>` gives the same events:
//!   the M2 reference checked on the M3 flow's markets, jumps and liquidations.
//! - The signature audit (13.4) finds no failure, and checked every forwarded message
//!   (signed).
//! - `report.md` renders, and names the verifier and the signing scheme.
//!
//! Whether a run is *valid* (15.7) is not asserted: a debug build that yields its CPU makes
//! the sender late, which is the generator-limited flag's job to report, not this test's.
//!
//! This binary starts the pipeline, which installs the abort-on-panic hook (2.8): a failed
//! assertion aborts it after printing its message. The two runs take turns (a lock), so
//! the allocation counter sees one run at a time.

// Test-only exception to the workspace's `unsafe_code = "deny"`, as in
// `engine/tests/no_alloc.rs`: counting allocations needs a global allocator, and
// `GlobalAlloc` is an unsafe trait. Ours only forwards to `std::alloc::System` and bumps a
// counter. This file is its own test binary, so none of this reaches the harness.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bench::e2e::config::RunConfig;
use bench::e2e::report::run_report;
use bench::e2e::results::RunResult;
use bench::e2e::runner::{RunObserver, run};
use bench::e2e::workload::Workloads;
use engine::event::Event;
use engine::mode::Naive;
use engine::reference::ReferenceBook;
use loadgen::schedule::{Arrivals, Bursts};
use pipeline::codec::decode_event;
use pipeline::counters::set_hot_thread_hook;
use pipeline::gate::Stage;
use pipeline::journal::recovery::recover;
use pipeline::records::{AuthScheme, EVENT_SLOT_WORDS, InjectionMode};
use pipeline::replay::{ReplayVerdict, first_difference, replay_as};

thread_local! {
    /// Set on the pipeline's hot threads when they start.
    static HOT: Cell<bool> = const { Cell::new(false) };
}

/// Counting is on during the timed flow.
static COUNTING: AtomicBool = AtomicBool::new(false);
/// Allocations made on hot threads while counting.
static HOT_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// The system allocator, plus a count of allocations on hot threads. `realloc` and
/// `alloc_zeroed` keep the trait's default implementations, which call `alloc`.
struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `try_with`: a thread that is shutting down may have lost its thread-locals, and
        // an allocator must not panic.
        if COUNTING.load(Ordering::Relaxed) && HOT.try_with(Cell::get).unwrap_or(false) {
            HOT_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: our caller upholds `GlobalAlloc::alloc`'s contract; we pass it on as is.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` (through `alloc` above) with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// The hot-thread hook: runs on each hot thread when it starts.
fn flag_hot_thread() {
    HOT.with(|hot| hot.set(true));
}

/// Starts counting when main sees the timed flow start.
struct CountDuringTimedFlow;

impl RunObserver for CountDuringTimedFlow {
    fn timed_flow_started(&mut self) {
        COUNTING.store(true, Ordering::Relaxed);
    }
}

/// One run at a time (module docs).
static SERIAL: Mutex<()> = Mutex::new(());

/// Runs `config` (a smoke run) in a fresh directory, and returns it with the hot threads'
/// allocations during the timed flow.
fn smoke(config: RunConfig) -> (RunResult, u64) {
    let _one_at_a_time = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    set_hot_thread_hook(flag_hot_thread);
    HOT_ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(false, Ordering::Relaxed);
    // The name says the scheme (`-eip712`) and the verifier, so no two runs share a directory.
    let name = format!("{}-{}", config.name(), config.verifier_name());
    let dir = std::env::temp_dir().join(format!("bench-smoke-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let result =
        run(&config, &mut Workloads::default(), &dir, &[], &mut CountDuringTimedFlow).expect("the run");
    COUNTING.store(false, Ordering::Relaxed);
    (result, HOT_ALLOCATIONS.load(Ordering::Relaxed))
}

/// Every check of the module docs, on one run.
fn check(result: &RunResult, hot_allocations: u64) {
    let name = result.config.name();
    let signed = result.config.mode == InjectionMode::Signed;
    eprintln!("{name}: {}", result.verdict().flags.join("; "));

    // Every stage handled commands.
    let stats = &result.output.stats;
    for stage in Stage::ALL {
        let gateway_stage = matches!(stage, Stage::IngressWait | Stage::Verification);
        if gateway_stage && !signed {
            continue;
        }
        assert!(stats.client.get(stage).count() > 0, "{name}: no client command in {}", stage.name());
        if !gateway_stage {
            assert!(stats.operator.get(stage).count() > 0, "{name}: no operator command in {}", stage.name());
        }
    }
    assert!(stats.set_mark_core_path.count() > 0);
    if signed {
        for (g, gateway) in stats.per_gateway.iter().enumerate() {
            assert!(gateway.ingress_wait.count() > 0 && gateway.verification.count() > 0, "gateway {g}");
        }
    }
    assert!(result.output.journal.fdatasync_ns.count() > 0 && result.sender.lag.count() > 0);
    assert!(result.output.core.commands > 0);

    // The counts reconcile, and nothing was lost in the shutdown.
    assert_eq!(result.count_mismatches(), Vec::<String>::new(), "{name}: the counts don't add up");
    let sequenced = result.output.sequencer.records;
    assert_eq!(result.output.journal.records, sequenced);
    assert_eq!((result.output.core.commands, result.output.stats.commands), (sequenced, sequenced));
    assert!(result.fund_consistent(), "{name}: the gate's fund equity differs from the snapshot's");

    // A liquidation and a shortfall, somewhere in the run.
    let capture = result.output.capture.as_deref().expect("capture is on in a smoke run");
    let events: Vec<Event> = capture
        .as_chunks::<EVENT_SLOT_WORDS>()
        .0
        .iter()
        .map(|slot| decode_event(slot[1..].try_into().expect("7 words")).expect("a captured event"))
        .collect();
    assert!(events.iter().any(|e| matches!(e, Event::Liquidation(_))), "{name}: no liquidation");
    assert!(events.iter().any(|e| matches!(e, Event::InsuranceShortfall(_))), "{name}: no shortfall");

    // No allocation on the hot threads during the timed flow, and no faults in the window
    // unless the kernel was migrating pages then (module docs).
    assert_eq!(hot_allocations, 0, "{name}: the hot threads allocated during the timed flow");
    let health = result.monitor.health().expect("the window opened and closed");
    for thread in &health.threads {
        let faults =
            thread.minor_faults.unwrap_or_else(|| panic!("{name}: {}'s faults are unknown", thread.name));
        if faults > 0 && health.kernel_migrated_pages() {
            let pages = health.page_migrations.unwrap_or(0);
            eprintln!(
                "{name}: {} took {faults} minor faults in the window, while the kernel migrated {pages} pages: not failed (module docs)",
                thread.name
            );
        } else {
            assert_eq!(
                faults, 0,
                "{name}: {} faulted in the window, and the kernel migrated no page",
                thread.name
            );
        }
    }

    // The replay test, and a replay through the reference engine.
    match &result.replay {
        Some(ReplayVerdict::Identical(report)) => assert_eq!(report.events, events.len() as u64),
        other => panic!("{name}: the replay test says {other:?}"),
    }
    let journal = result.dir.join("journal");
    let recovered = recover(&journal, &result.config.identity(), false).expect("recovered");
    let reference = replay_as::<ReferenceBook, Naive>(&journal, &recovered, None, true).expect("replayed");
    let reference_events = reference.capture.as_deref().expect("captured");
    assert_eq!(first_difference(capture, reference_events), None, "{name}: the reference engine differs");
    assert_eq!(reference.engine.snapshot(), result.snapshot, "{name}: the reference engine's state differs");

    // The audit, and the report.
    if signed {
        let audit = result.audit.as_ref().expect("the audit ran");
        assert!(audit.passed(), "{name}: {audit}");
        assert_eq!(audit.signed, result.gateways.iter().map(|g| g.forwarded).sum::<u64>());
    }
    // The EIP-712 scheme: every message was fresh and new, and every table had room.
    assert_eq!(result.harness_rejects(), None, "{name}");
    let summary = result.summary();
    assert_eq!(summary.get("run.verifier"), Some(result.config.verifier_name()));
    assert_eq!(summary.get("run.auth"), Some(result.config.auth_name()));
    assert_eq!(summary.get("run.flow"), Some(result.config.flow.name()));
    // The flow's shape: every client message in the window was counted once.
    let lanes: u64 = (0..result.config.lanes)
        .map(|g| summary.u64(&format!("flow.lane.{g}.messages")).expect("each lane's count"))
        .sum();
    assert_eq!(Some(lanes), summary.u64("flow.window_clients"), "{name}");
    assert_eq!(summary.u64("flow.window_clients"), Some(result.sender.window_offered), "{name}");
    let report = run_report(&summary);
    assert!(report.contains(&format!("### Run `{name}`")) && report.contains("**core path**"));
    assert!(report.contains(&format!("verifier: {}.", result.config.verifier_name())), "{report}");
    let scheme = match result.config.auth_name() {
        "eip712" => "auth: eip712 (Polymarket Perps format);".to_string(),
        other => format!("auth: {other};"),
    };
    assert!(report.contains(&scheme), "{report}");
    assert!(report.contains("identical state and an identical event stream"), "{report}");
    assert!(report.contains("**The flow's shape in the window**"), "{report}");
    std::fs::write(result.dir.join("report.md"), report).expect("written");
    std::fs::remove_dir_all(&result.dir).expect("cleaned up");
}

#[test]
fn signed_smoke_run() {
    let (result, allocations) = smoke(RunConfig::smoke(InjectionMode::Signed));
    check(&result, allocations);
}

#[test]
fn pre_verified_smoke_run() {
    let (result, allocations) = smoke(RunConfig::smoke(InjectionMode::PreVerified));
    check(&result, allocations);
}

/// The signed smoke run with Bitcoin Core's libsecp256k1 verifying, in the gateways and in
/// the audit (module docs).
#[cfg(feature = "c-secp256k1")]
#[test]
fn signed_smoke_run_with_libsecp256k1() {
    let config = RunConfig {
        verifier: gateway::VerifierKind::LibSecp256k1,
        ..RunConfig::smoke(InjectionMode::Signed)
    };
    let (result, allocations) = smoke(config);
    assert_eq!(
        result.registry.as_ref().map(|registry| registry.verifier()),
        Some(gateway::VerifierKind::LibSecp256k1)
    );
    check(&result, allocations);
}

/// The signed smoke run in Polymarket Perps' EIP-712 scheme (module docs), with `k256`
/// recovering the signers in the gateways.
#[test]
fn signed_eip712_smoke_run() {
    let config = RunConfig { auth: AuthScheme::Eip712, ..RunConfig::smoke(InjectionMode::Signed) };
    let (result, allocations) = smoke(config);
    assert_eq!(result.config.name(), "signed-5k-eip712");
    check(&result, allocations);
}

/// The EIP-712 smoke run with Bitcoin Core's libsecp256k1 recovering the signers in the
/// gateways, and verifying them in the audit (module docs).
#[cfg(feature = "c-secp256k1")]
#[test]
fn signed_eip712_smoke_run_with_libsecp256k1() {
    let config = RunConfig {
        auth: AuthScheme::Eip712,
        verifier: gateway::VerifierKind::LibSecp256k1,
        ..RunConfig::smoke(InjectionMode::Signed)
    };
    let (result, allocations) = smoke(config);
    assert_eq!(
        result.registry.as_ref().map(|registry| registry.verifier()),
        Some(gateway::VerifierKind::LibSecp256k1)
    );
    check(&result, allocations);
}

/// The Polymarket-shaped flow's signed smoke run (module docs).
#[test]
fn signed_polymarket_smoke_run() {
    let (result, allocations) = smoke(RunConfig::polymarket_smoke(InjectionMode::Signed));
    assert_eq!(result.config.name(), "signed-5k-polymarket-shock-stress");
    check(&result, allocations);
}

/// The Polymarket-shaped flow's pre-verified smoke run, with three makers quoting every
/// market and bursty arrivals (module docs).
#[test]
fn pre_verified_polymarket_smoke_run_with_three_makers_and_bursts() {
    let smoke_run = RunConfig::polymarket_smoke(InjectionMode::PreVerified);
    let config = RunConfig {
        flow: smoke_run.flow.with_makers(3).expect("a switch of the Polymarket flow"),
        arrivals: Arrivals::Cox(Bursts::Median),
        ..smoke_run
    };
    let (result, allocations) = smoke(config);
    assert_eq!(result.config.name(), "preverified-20k-polymarket-makers3-bursts-median-shock-stress");
    check(&result, allocations);
    // Three accounts make every market: one of them sends the most.
    let (busiest, _) = result.content.busiest_account.expect("someone sent");
    assert!((1..=3).contains(&busiest.get()), "the busiest account is {busiest}");
}

/// The Polymarket-shaped flow's signed smoke run in Polymarket Perps' EIP-712 scheme
/// (module docs).
#[test]
fn signed_eip712_polymarket_smoke_run() {
    let config = RunConfig { auth: AuthScheme::Eip712, ..RunConfig::polymarket_smoke(InjectionMode::Signed) };
    let (result, allocations) = smoke(config);
    check(&result, allocations);
}

/// The Polymarket-shaped flow's signed smoke run with libsecp256k1 verifying (module docs).
#[cfg(feature = "c-secp256k1")]
#[test]
fn signed_polymarket_smoke_run_with_libsecp256k1() {
    let config = RunConfig {
        verifier: gateway::VerifierKind::LibSecp256k1,
        ..RunConfig::polymarket_smoke(InjectionMode::Signed)
    };
    let (result, allocations) = smoke(config);
    check(&result, allocations);
}
