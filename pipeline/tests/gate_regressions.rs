//! Named regression tests for output gating (`docs/PIPELINE.md` 12.1, 2.5 and 2.8). Each
//! test pins a review finding of Milestone 3 that was fixed (PIPELINE.md 22), so that it
//! can't come back. The gate's unit tests are in `pipeline/src/gate.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use engine::command::{Command, SetMark};
use engine::event::{Event, EventSink, MarkPrice};

use pipeline::clock::RunClock;
use pipeline::codec::encode_command;
use pipeline::core_thread::EventRingSink;
use pipeline::counters::{PipelineCounters, SharedCounter, Watermark};
use pipeline::gate::{Gate, GateConfig, Phases};
use pipeline::idle::IdleStrategy;
use pipeline::records::{CoreRecord, Meta, Stamps};
use pipeline::ring::channel;

const IDLE: IdleStrategy = IdleStrategy::SpinThenYield { spins: 64 };

/// The core record of an operator mark with seq `seq`, for the sink's trailer.
fn mark_record(seq: u64) -> CoreRecord {
    let command = encode_command(&Command::SetMark(SetMark { price: 100, market: 1 }));
    CoreRecord {
        seq,
        meta: Meta::OPERATOR,
        command,
        t_sched: 1,
        t_sent: 2,
        t_gw_in: 0,
        t_gw_out: 0,
        t_seq: 3,
    }
}

fn mark(price: i64) -> Event {
    Event::MarkPrice(MarkPrice { price, market: 1 })
}

// F-PARTIAL-RELEASE: when the event ring filled in the middle of a command, the core
// published the command's first events and the gate released them as soon as the command
// was durable, before its trailer: an engine panic later in the same command (which aborts
// the process) could no longer stop them, although 2.8 promises that nothing partial is
// released.
#[test]
fn a_command_whose_events_filled_the_ring_is_not_released_before_its_trailer() {
    // An event ring of 8 slots. Seq 1 (3 events and a trailer) is published, then seq 2
    // emits 6 events: its 5th finds the ring full, so the sink publishes seq 2's first 4 and
    // waits until the gate frees seq 1's slots. Both commands are durable.
    let clock = RunClock::start();
    let (event_in, mut event_out) = channel::<1>(8);
    let stalls = SharedCounter::new();
    let mut sink = EventRingSink::new(event_in, clock, IDLE, &stalls);
    sink.begin(1);
    for price in 0..3 {
        sink.emit(mark(price));
    }
    sink.trailer(&mark_record(1), clock.now());
    sink.begin(2);
    for price in 10..14 {
        sink.emit(mark(price)); // the ring is full now: 4 slots of seq 1, 4 of seq 2
    }

    let core_done = AtomicBool::new(false);
    let (stats, capture) = std::thread::scope(|scope| {
        // The core goes on with seq 2 on a thread of its own, since the sink will wait.
        scope.spawn(|| {
            for price in 14..16 {
                sink.emit(mark(price)); // the first publishes seq 2's 4 events and waits for space
            }
            // `apply(seq 2)` panics here, before its trailer: the process would abort now.
            core_done.store(true, Ordering::Release);
        });

        // The handshake: the gate starts only once all 8 slots are published. Only the
        // sink's wait for space publishes seq 2's events (nothing reads the ring yet), so by
        // then the ring has filled in the middle of seq 2 and the sink is waiting, however
        // the two threads are scheduled. (Starting the gate first let it free seq 1's
        // slots before the core found the ring full, on a loaded machine: no wait at all.)
        let deadline = Instant::now() + Duration::from_secs(10);
        while event_out.available(8) < 8 {
            assert!(Instant::now() < deadline, "the sink never published seq 2's first events");
            std::thread::yield_now();
        }
        let config = GateConfig {
            lanes: 1,
            next_seq: 1,
            stamps: Stamps::On,
            capture: Some(64),
            release_log: None,
            clock,
        };
        let mut gate = Gate::new(event_out, config);
        let (durable, phases, counters) = (Watermark::new(2), Phases::everything(), PipelineCounters::new());
        // Pass until the core has stopped, and then for a while longer.
        let mut stopped_at: Option<Instant> = None;
        while stopped_at.is_none_or(|at| at.elapsed() < Duration::from_millis(200)) {
            if stopped_at.is_none() && core_done.load(Ordering::Acquire) {
                stopped_at = Some(Instant::now());
            }
            gate.pass(&durable, &phases, &counters);
            std::thread::yield_now();
        }
        gate.finish()
    });

    assert_eq!(stats.commands, 1, "only seq 1 finished");
    let captured: Vec<u64> = capture.expect("capture was on").chunks(8).map(|slot| slot[0]).collect();
    assert_eq!(captured, [1, 1, 1], "seq 2's published events were released without its trailer");
    assert!(stalls.load() > 0, "the sink did wait for space: the ring filled mid-command");
}
