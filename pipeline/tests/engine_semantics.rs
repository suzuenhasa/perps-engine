//! `ENGINE_SEMANTICS`, pinned (`docs/PIPELINE.md` 11.2 and 18.1).
//!
//! **Why.** The journal's byte format alone doesn't tie a journal to the engine's
//! behaviour: a later build with different fee rounding would replay the same journal into
//! a different state, silently. So every segment header records `ENGINE_SEMANTICS`, and
//! recovery and replay refuse a journal written under another value (11.2). This test keeps
//! the constant honest: it applies the first 100,000 commands of a fixed flow to a fresh
//! engine and compares the CRC32C of the event stream's EVT56 bytes with a pinned value.
//! An engine change that alters any result fails it, until someone bumps `ENGINE_SEMANTICS`
//! and re-pins the CRC, on purpose. (A fix for a poison pill doesn't bump it: it changes
//! only what happens where the old engine panicked.)
//!
//! **The flow** is `common::TestFlow` (the M3 smoke flow lives in `loadgen`, which
//! `pipeline` can't depend on; PIPELINE.md 22). The test also checks that the flow reaches
//! liquidations, a shortfall, fills and the other paths, so that the pinned stream covers
//! them.

mod common;

use engine::book::Book;
use engine::engine::Engine;
use engine::event::{CancelReason, Event, EventSink};
use engine::mode::Fast;
use pipeline::codec::{EVENT_BYTES, encode_event, to_le_bytes};
use pipeline::crc32c::Crc32c;
use pipeline::journal::format::ENGINE_SEMANTICS;

use common::{TestFlow, engine_options};

/// Commands applied, setup included.
const COMMANDS: usize = 100_000;
/// The flow's seed.
const SEED: u64 = 0x5EED_0001;
/// The `ENGINE_SEMANTICS` the CRC below was pinned for.
const PINNED_FOR: u32 = 1;
/// CRC32C of every event's 56 bytes, in order, for the flow above.
const PINNED_CRC: u32 = 0xE45B_2803;

/// Folds every event into the CRC, and counts what the flow reached.
#[derive(Default)]
struct Sink {
    crc: Crc32c,
    events: u64,
    fills: u64,
    liquidations: u64,
    shortfalls: u64,
    band_sweeps: u64,
    rejects: u64,
}

impl EventSink for Sink {
    fn emit(&mut self, event: Event) {
        let bytes: [u8; EVENT_BYTES] = to_le_bytes(&encode_event(&event));
        self.crc.update(&bytes);
        self.events += 1;
        match event {
            Event::Fill(_) => self.fills += 1,
            Event::Liquidation(_) => self.liquidations += 1,
            Event::InsuranceShortfall(_) => self.shortfalls += 1,
            Event::Cancelled(c) if c.reason == CancelReason::PriceBand => self.band_sweeps += 1,
            Event::Reject(_) => self.rejects += 1,
            _ => {}
        }
    }
}

#[test]
fn the_pinned_flow_gives_the_pinned_event_stream() {
    let setup = TestFlow::new(SEED).setup().len();
    let commands = TestFlow::commands(SEED, COMMANDS - setup);
    assert_eq!(commands.len(), COMMANDS);
    let mut engine: Engine<Book, Fast> = Engine::new(engine_options(0x5EED));
    let mut sink = Sink::default();
    for command in &commands {
        engine.apply(command, &mut sink);
    }
    engine.assert_invariants();
    assert_eq!(
        ENGINE_SEMANTICS, PINNED_FOR,
        "ENGINE_SEMANTICS changed: re-pin PINNED_CRC (and PINNED_FOR) for the new engine"
    );
    assert_eq!(
        sink.crc.finish(),
        PINNED_CRC,
        "the engine's results changed: bump ENGINE_SEMANTICS (pipeline/src/journal/format.rs) and re-pin this \
         CRC, on purpose (PIPELINE.md 11.2)"
    );
    // The pinned stream covers what matters (when pinned: 334,362 events, 34,627 fills,
    // 1,003 liquidations, 1,139 shortfall reports, 4,510 band sweeps, 26,970 rejects).
    assert!(sink.fills > 10_000 && sink.rejects > 1_000, "fills and rejects");
    assert!(sink.liquidations > 100 && sink.shortfalls > 100, "liquidations and shortfalls");
    assert!(sink.band_sweeps > 100, "orders swept out of the band");
}
