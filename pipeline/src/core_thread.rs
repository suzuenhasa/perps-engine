//! The core thread: applies each sequenced command to `Engine<Book, Fast>` and writes its
//! events, then a trailer, to the event ring (`docs/PIPELINE.md` section 10).
//!
//! **Contract.**
//! - Commands are applied in seq order, each completely before the next, and the core
//!   checks with one compare per command that the seqs are consecutive from `next_seq`: a
//!   sequencer that skipped or reordered anything stops the process at once rather than
//!   showing up later as a replay mismatch (10.1).
//! - Every event of a command goes to the event ring as one slot (`seq`, then its EVT56),
//!   in the engine's order; then one trailer slot (10.3) carries the command's source, tag,
//!   outcome, event count and stamps to the gate, and the ring is published. So the gate
//!   sees a command's events and trailer in one piece, and the trailer marks its end.
//! - **The core never drops an event** (2.4, 2.5): when the event ring is full, the sink
//!   first publishes what it has written (the gate can only free slots it can see), then
//!   waits for space. The wait always ends: the gate keeps releasing as the journal
//!   flushes, and if either of them panicked, the whole process has already stopped (2.8).
//!   So a command of any size (an IOC sweeping many levels, a `SetMark` that liquidates many
//!   positions) streams through, even one with more events than the ring holds. The gate
//!   releases a command's events only once its trailer is in the ring too, unless the ring
//!   is full of that one command's events (`gate.rs`, "A command is released whole"): so an
//!   engine panic in the middle of a command releases none of it, unless the command had
//!   already emitted more events than the ring holds (131,072).
//! - The seq being applied is kept in a thread-local, so a panic inside the engine names it
//!   (2.8).
//! - **No first-touch page faults in the window** (15.4). The engine reserves its capacity
//!   up front, but the operating system maps each page only when it is first written, so
//!   new slots and new order ids would keep taking minor faults on this thread long into a
//!   run. The core writes it all once instead, before its first command
//!   (`Engine::prefault`), and again for a market after each accepted `SetMarketParams`,
//!   which creates it or gives it a new book (`Engine::prefault_market`); both change
//!   nothing the engine does or emits. `SetMarketParams` comes in setup (14.4), before the
//!   window. A rejected one changed nothing, so it is not followed by a prefault, which on a
//!   live market would rebuild its maps (`engine/src/prefault.rs`) on the command path. An
//!   accepted one on a market that already had accounts rebuilds that market's slot map
//!   once: one allocation, on an operator command that only an empty market accepts.
//!
//! **Measurement.** One clock read per command, after `apply` (`t_done`), plus one per batch
//! for the busy meter; the gate derives every latency from the trailers. With stamps off
//! the core reads only the first line of each record, reads no clock per command, and
//! writes trailers with zero timings (15.1). The time spent waiting for the event ring is
//! the core's own histogram, and a running total main can read (10.2, 15.4).
//!
//! **Trust.** Only our own code writes the core ring, so a command that doesn't decode is a
//! bug: it panics, which aborts the process (10.1).
//!
//! **The "verify on core" ablation** (section 16, only through `e2e ablate`). The core
//! ring then has 3-line records that also carry each command's nonce, expiry and signature
//! ([`SignedFields`]), in both arms. In the arm that verifies on the core, a
//! [`CoreVerifier`] (the gateway crate's, with the core's own copy of the key registry)
//! rebuilds the signed bytes and checks the signature of every signed client command just
//! before `apply`, so each such command's core path includes one verification. A bad
//! signature panics, which aborts the run: the pre-signed flow has none, and the mode
//! exists only to measure the cost. **It is not a secure design**: the gateways have
//! already used up the nonce before anything was verified.
//!
//! **Complexity.** Per command: one decode, `apply`, one ring write per event (8 words) and
//! one for the trailer, one Release store. No allocation of its own. In the ablation's core
//! arm, one signature verification per signed command.

use std::fmt;

use engine::book::Book;
use engine::command::Command;
use engine::engine::Engine;
use engine::event::{Event, EventSink};
use engine::mode::Fast;
use engine::types::AccountId;

use crate::clock::RunClock;
use crate::codec::decode_command;
use crate::counters::{BusyMeter, SharedCounter, ThreadCounters};
use crate::histogram::LatencyHistogram;
use crate::idle::IdleStrategy;
use crate::panic::set_current_seq;
use crate::records::{CoreRecord, SignedFields, Source, Stamps, Trailer, event_slot, outcome};
use crate::ring::{Consumer, Producer};

/// Commands the core takes from its ring per batch, at most (10.1).
pub const CORE_BATCH: usize = 256;

/// Checks a signed client command's signature on the core thread: the "verify on core"
/// ablation only (module docs, section 16). Implemented by the gateway crate, which holds
/// the keys; the pipeline itself never verifies anything.
pub trait CoreVerifier: Send + fmt::Debug {
    /// True if `signed.signature` is a valid low-S signature, by `account`'s registered
    /// key, over the 72 signed bytes rebuilt from `account`, the nonce, the expiry and
    /// `command` (5.1, 5.2).
    fn verify(&self, account: AccountId, command: &Command, signed: &SignedFields) -> bool;

    /// Called once on the core thread before its first command, so that anything the
    /// verifier keeps per thread (libsecp256k1's context) is allocated outside the timed
    /// window. Does nothing by default.
    fn prepare_this_thread(&self) {}
}

/// The core's `EventSink`: writes each event into the event ring, waiting for space when
/// the ring is full (module docs; PIPELINE.md 10.2).
#[derive(Debug)]
pub struct EventRingSink<'a> {
    ring: Producer<1>,
    clock: RunClock,
    idle: IdleStrategy,
    /// The seq of the command being applied.
    seq: u64,
    /// Its events so far.
    events: u64,
    /// Its outcome: 0 accepted, else its reject reason's code + 1 (from its first event).
    outcome: u8,
    /// Every wait for space, in nanoseconds.
    stall_ns: LatencyHistogram,
    stall_total_ns: u64,
    /// The running total, for main (15.4).
    stall_counter: &'a SharedCounter,
}

impl<'a> EventRingSink<'a> {
    pub fn new(
        ring: Producer<1>,
        clock: RunClock,
        idle: IdleStrategy,
        stall_counter: &'a SharedCounter,
    ) -> Self {
        EventRingSink {
            ring,
            clock,
            idle,
            seq: 0,
            events: 0,
            outcome: 0,
            stall_ns: LatencyHistogram::new(),
            stall_total_ns: 0,
            stall_counter,
        }
    }

    /// A new command starts: its events will carry `seq`.
    pub fn begin(&mut self, seq: u64) {
        self.seq = seq;
        self.events = 0;
        self.outcome = 0;
    }

    /// Events the current command has emitted so far.
    pub fn events(&self) -> u64 {
        self.events
    }

    /// True if the current command was accepted: its first event, if any, is no reject.
    pub fn accepted(&self) -> bool {
        self.outcome == 0
    }

    /// Ends the current command: writes its trailer (10.3) and publishes the ring.
    pub fn trailer(&mut self, record: &CoreRecord, t_done: u64) {
        let trailer = Trailer {
            seq: self.seq,
            source: record.meta.source,
            command_tag: record.command[0] as u8,
            outcome: self.outcome,
            lane: record.meta.lane,
            events: Trailer::saturating_events(self.events),
            t_sched: record.t_sched,
            t_sent: record.t_sent,
            t_gw_in: record.t_gw_in,
            t_gw_out: record.t_gw_out,
            t_seq: record.t_seq,
            t_done,
        };
        self.wait_for_space();
        self.ring.write(&trailer.to_words());
        self.ring.publish();
    }

    /// Returns once the ring has a free slot. If it has none: publish first, so the gate
    /// can see (and eventually free) every slot written so far, then wait (10.2).
    fn wait_for_space(&mut self) {
        if self.ring.free(1) > 0 {
            return;
        }
        self.ring.publish();
        let t0 = self.clock.now();
        while self.ring.free(1) == 0 {
            self.idle.idle();
        }
        let stalled = self.clock.now().saturating_sub(t0);
        self.stall_ns.record(stalled);
        self.stall_total_ns += stalled;
        self.stall_counter.store(self.stall_total_ns);
    }

    /// The stall histogram and total. Dropping the sink drops the event ring's producer,
    /// which closes the ring (2.8).
    fn into_stalls(self) -> (LatencyHistogram, u64) {
        (self.stall_ns, self.stall_total_ns)
    }
}

impl EventSink for EventRingSink<'_> {
    fn emit(&mut self, event: Event) {
        self.wait_for_space();
        self.ring.write(&event_slot(self.seq, &event));
        if self.events == 0 {
            self.outcome = outcome(&event);
        }
        self.events += 1;
    }
}

/// What the core measured, returned when its thread is joined.
#[derive(Clone, Debug)]
pub struct CoreStats {
    /// Commands applied.
    pub commands: u64,
    /// Engine events emitted (trailers not counted).
    pub events: u64,
    /// The most events one command emitted.
    pub max_events_per_command: u64,
    /// Each wait for space in the event ring (10.2).
    pub stall_ns: LatencyHistogram,
    pub stall_total_ns: u64,
}

/// The core's settings.
#[derive(Clone, Copy, Debug)]
pub struct CoreConfig {
    /// The seq of the first command: 1, or the recovered journal's next seq (13.5).
    pub next_seq: u64,
    pub stamps: Stamps,
    pub clock: RunClock,
    pub idle: IdleStrategy,
}

/// The core loop (10.1): applies every command from `input` until the ring is closed and
/// drained, then returns the engine (for the snapshot, 13.3) and its statistics. Returning
/// drops the sink, which closes the event ring (2.8). `LINES` is the core ring's slot size:
/// 2, or 3 in the "verify on core" ablation, where `verifier` (the core arm's) checks every
/// signed command before it is applied (module docs).
pub fn run_core<const LINES: usize>(
    mut engine: Engine<Book, Fast>,
    mut input: Consumer<LINES>,
    mut sink: EventRingSink<'_>,
    config: CoreConfig,
    verifier: Option<&dyn CoreVerifier>,
    counters: &ThreadCounters,
) -> (Engine<Book, Fast>, CoreStats) {
    let CoreConfig { next_seq, stamps, clock, idle } = config;
    let stamped = stamps == Stamps::On;
    let words = if LINES == 3 { CoreRecord::ABLATION_WORDS } else { CoreRecord::words_for(stamps) };
    let mut expected = next_seq;
    let (mut commands, mut events, mut max_events) = (0, 0, 0);
    let mut busy = BusyMeter::new();
    engine.prefault(); // module docs: nothing reserved is first touched inside the window
    if let Some(verifier) = verifier {
        verifier.prepare_this_thread();
    }
    loop {
        let n = input.available(CORE_BATCH).min(CORE_BATCH);
        if n == 0 {
            if input.is_finished() {
                break;
            }
            idle.idle();
            continue;
        }
        busy.begin(clock.now()); // busy time (2.2): one extra read per batch
        let mut t_done = 0;
        for _ in 0..n {
            let (record, signed) = read_record(&mut input, words);
            assert_eq!(record.seq, expected, "the core got seq {} but expected {expected}", record.seq);
            expected += 1;
            set_current_seq(record.seq);
            let command = decode_command(&record.command).unwrap_or_else(|e| {
                panic!("seq {}: a command that doesn't decode ({e}): our own code encoded it", record.seq)
            });
            if let (Some(verifier), Some(signed)) = (verifier, signed)
                && record.meta.source == Source::SignedClient
            {
                assert!(
                    verifier.verify(record.meta.account, &command, &signed),
                    "seq {}: a bad signature in the verify-on-core ablation, whose pre-signed flow has none",
                    record.seq
                );
            }
            sink.begin(record.seq);
            engine.apply(&command, &mut sink);
            t_done = if stamped { clock.now() } else { 0 };
            sink.trailer(&record, t_done);
            if let Command::SetMarketParams(params) = command
                && sink.accepted()
            {
                engine.prefault_market(params.market); // a new market or a new book (module docs)
            }
            commands += 1;
            events += sink.events();
            max_events = max_events.max(sink.events());
        }
        input.release();
        let busy_until = if stamped { t_done } else { clock.now() };
        busy.end(busy_until, &counters.busy_ns);
    }
    set_current_seq(0);
    let (stall_ns, stall_total_ns) = sink.into_stalls();
    (engine, CoreStats { commands, events, max_events_per_command: max_events, stall_ns, stall_total_ns })
}

/// Reads the next record's first `words` words (7, 12, or 22 in the ablation) and returns
/// the record, and its signed fields if the ring carries them. The buffer is zeroed each
/// time: with stamps off only the first line is read, and the stamp words must read as 0.
fn read_record<const LINES: usize>(
    input: &mut Consumer<LINES>,
    words: usize,
) -> (CoreRecord, Option<SignedFields>) {
    let mut buffer = [0; CoreRecord::ABLATION_WORDS];
    input.read(&mut buffer[..words]);
    let (core, rest) = buffer.split_at(CoreRecord::WORDS);
    let record = CoreRecord::from_words(core.try_into().expect("12 words"));
    let signed = (words == CoreRecord::ABLATION_WORDS)
        .then(|| SignedFields::from_words(rest.try_into().expect("the signed fields' 10 words")));
    (record, signed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_event, encode_command};
    use crate::records::{EVENT_SLOT_WORDS, Meta, Source, is_trailer, outcome_reject_reason};
    use crate::ring::channel;
    use engine::command::{Command, Deposit, PlaceOrder, SetMark, SetMarketParams, SetRiskTier};
    use engine::engine::EngineOptions;
    use engine::event::{CancelReason, RejectReason};
    use engine::types::{MarketId, Micros, OrderSeq, Price, Qty, Side, TimeInForce, order_id};

    const IDLE: IdleStrategy = IdleStrategy::SpinThenYield { spins: 16 };

    /// A market with a 2% band around 100,000, a funded account 1 with ten bids resting
    /// just under the mark, and a `SetMark` of 95,000, whose band's upper edge (96,900) is
    /// below every bid, so it sweeps all ten (a command with many events).
    fn commands() -> Vec<Command> {
        let mut commands = vec![
            Command::SetMarketParams(SetMarketParams {
                min_price: Price::new(1_000),
                max_price: Price::new(1_000_000),
                maker_fee_ppm: 0,
                taker_fee_ppm: 0,
                price_band_ppm: 20_000,
                market: MarketId::new(1),
                max_leverage: 10,
            }),
            Command::SetRiskTier(SetRiskTier {
                lower_bound: Micros::ZERO,
                market: MarketId::new(1),
                max_leverage: 10,
                index: 0,
                count: 1,
            }),
            Command::SetMark(SetMark { price: Price::new(100_000), market: MarketId::new(1) }),
            Command::Deposit(Deposit { amount: Micros::new(1_000_000_000_000), account: AccountId::new(1) }),
        ];
        for n in 1..=10 {
            commands.push(Command::PlaceOrder(PlaceOrder {
                order_id: order_id(AccountId::new(1), OrderSeq::new(n)),
                price: Price::new(99_000 + i64::from(n)),
                qty: Qty::new(10),
                market: MarketId::new(1),
                side: Side::Buy,
                tif: TimeInForce::Gtc,
                post_only: false,
            }));
        }
        commands.push(Command::PlaceOrder(PlaceOrder {
            order_id: order_id(AccountId::new(1), OrderSeq::new(11)),
            price: Price::new(99_000),
            qty: Qty::ZERO, // rejected: InvalidQty
            market: MarketId::new(1),
            side: Side::Buy,
            tif: TimeInForce::Gtc,
            post_only: false,
        }));
        commands.push(Command::SetMark(SetMark { price: Price::new(95_000), market: MarketId::new(1) }));
        commands
    }

    fn record(seq: u64, command: &Command) -> CoreRecord {
        let meta = match command {
            Command::PlaceOrder(_) => {
                Meta { source: Source::PreVerifiedClient, lane: 3, account: AccountId::new(1) }
            }
            _ => Meta::OPERATOR,
        };
        CoreRecord {
            seq,
            meta,
            command: encode_command(command),
            t_sched: seq * 10,
            t_sent: seq * 10 + 1,
            t_gw_in: 0,
            t_gw_out: 0,
            t_seq: seq * 10 + 2,
        }
    }

    /// Runs the core over `commands` numbered from `first_seq`, with an event ring of
    /// `event_capacity` read by another thread; returns every slot it read and the stats.
    fn run(
        commands: &[Command],
        first_seq: u64,
        event_capacity: usize,
        stamps: Stamps,
    ) -> (Vec<[u64; 8]>, CoreStats) {
        let (mut core_in, core_out) = channel::<2>(64);
        for (i, command) in commands.iter().enumerate() {
            core_in.write(&record(first_seq + i as u64, command).to_words());
        }
        drop(core_in);
        let (events_in, mut events_out) = channel::<1>(event_capacity);
        let reader = std::thread::spawn(move || {
            let mut slots = Vec::new();
            loop {
                if events_out.available(1) == 0 {
                    if events_out.is_finished() {
                        return slots;
                    }
                    IDLE.idle();
                    continue;
                }
                let mut slot = [0; EVENT_SLOT_WORDS];
                events_out.read(&mut slot);
                events_out.release();
                slots.push(slot);
            }
        });
        let (stall, counters) = (SharedCounter::new(), ThreadCounters::new());
        let clock = RunClock::start();
        let sink = EventRingSink::new(events_in, clock, IDLE, &stall);
        let engine = Engine::new(EngineOptions::default());
        let config = CoreConfig { next_seq: first_seq, stamps, clock, idle: IDLE };
        let (engine, stats) = run_core(engine, core_out, sink, config, None, &counters);
        engine.assert_invariants();
        assert_eq!(stall.load(), stats.stall_total_ns);
        (reader.join().expect("the reader ends"), stats)
    }

    #[test]
    fn each_command_emits_its_events_then_a_trailer() {
        let commands = commands();
        let (slots, stats) = run(&commands, 5, 1_024, Stamps::On);
        let trailers: Vec<Trailer> =
            slots.iter().filter(|s| is_trailer(s)).map(Trailer::from_words).collect();
        assert_eq!(trailers.len(), commands.len());
        assert_eq!(stats.commands, commands.len() as u64);
        assert_eq!(stats.events + stats.commands, slots.len() as u64);
        let mut events_seen = 0;
        for (i, trailer) in trailers.iter().enumerate() {
            let seq = 5 + i as u64;
            let record = record(seq, &commands[i]);
            assert_eq!(trailer.seq, seq);
            assert_eq!((trailer.source, trailer.lane), (record.meta.source, record.meta.lane));
            assert_eq!(trailer.command_tag, record.command[0] as u8);
            assert_eq!(
                (trailer.t_sched, trailer.t_sent, trailer.t_seq),
                (seq * 10, seq * 10 + 1, seq * 10 + 2)
            );
            assert!(trailer.t_done >= 1, "stamped");
            events_seen += u64::from(trailer.events);
        }
        assert_eq!(events_seen, stats.events);
        // The rejected place: one Reject, outcome InvalidQty + 1.
        let reject = &trailers[14];
        assert_eq!(
            (reject.events, outcome_reject_reason(reject.outcome)),
            (1, Some(RejectReason::InvalidQty))
        );
        assert_eq!(trailers[4].outcome, 0, "an accepted place");
        // Every event slot carries its command's seq, in order, before that command's trailer.
        let mut current = 5;
        for slot in &slots {
            assert_eq!(slot[0], current);
            if is_trailer(slot) {
                current += 1;
            } else {
                decode_event(&slot[1..].try_into().expect("7 words")).expect("a valid event");
            }
        }
    }

    #[test]
    fn a_command_with_more_events_than_the_ring_holds_streams_through() {
        let commands = commands();
        let (slots, stats) = run(&commands, 1, 4, Stamps::On);
        let sweep = slots.iter().filter(|s| s[0] == 16 && !is_trailer(s)).count();
        assert!(sweep > 10, "the last mark swept all ten bids: {sweep} events");
        let swept = slots
            .iter()
            .filter(|s| !is_trailer(s))
            .filter_map(|s| decode_event(&s[1..].try_into().expect("7 words")).ok())
            .filter(|e| matches!(e, Event::Cancelled(c) if c.reason == CancelReason::PriceBand))
            .count();
        assert_eq!(swept, 10);
        assert_eq!(stats.max_events_per_command, sweep as u64);
        let (all, _) = run(&commands, 1, 1_024, Stamps::On);
        assert_eq!(slots.len(), all.len(), "nothing dropped with a tiny ring");
    }

    #[test]
    fn with_stamps_off_trailers_carry_no_timings() {
        let (slots, _) = run(&commands()[..4], 1, 64, Stamps::Off);
        for trailer in slots.iter().filter(|s| is_trailer(s)).map(Trailer::from_words) {
            let times = [
                trailer.t_sched,
                trailer.t_sent,
                trailer.t_gw_in,
                trailer.t_gw_out,
                trailer.t_seq,
                trailer.t_done,
            ];
            assert_eq!(times, [0; 6]);
        }
    }

    #[test]
    #[should_panic(expected = "the core got seq 7 but expected 6")]
    fn a_gap_in_the_seqs_stops_the_core() {
        let (mut core_in, core_out) = channel::<2>(4);
        core_in.write(
            &record(7, &Command::SetMark(SetMark { price: Price::new(1), market: MarketId::new(1) }))
                .to_words(),
        );
        drop(core_in);
        let (events_in, _events_out) = channel::<1>(64);
        let (stall, counters) = (SharedCounter::new(), ThreadCounters::new());
        let clock = RunClock::start();
        let sink = EventRingSink::new(events_in, clock, IDLE, &stall);
        let config = CoreConfig { next_seq: 6, stamps: Stamps::On, clock, idle: IDLE };
        run_core(Engine::new(EngineOptions::default()), core_out, sink, config, None, &counters);
    }

    /// A verifier that accepts a signature whose first word is not zero, and counts calls.
    #[derive(Debug, Default)]
    struct FakeVerifier {
        calls: std::sync::atomic::AtomicU64,
    }

    impl CoreVerifier for FakeVerifier {
        fn verify(&self, account: AccountId, _: &Command, signed: &SignedFields) -> bool {
            assert_eq!(account, AccountId::new(1));
            self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            signed.signature[0] != 0
        }
    }

    /// Runs the core on a 3-line ring over the commands of [`commands`], with the places
    /// signed (source 1) and carrying `signature_word` as their signature's first word.
    fn run_verifying(verifier: &FakeVerifier, signature_word: u64) -> CoreStats {
        let commands = commands();
        let (mut core_in, core_out) = channel::<3>(64);
        for (i, command) in commands.iter().enumerate() {
            let mut record = record(1 + i as u64, command);
            if record.meta.source == Source::PreVerifiedClient {
                record.meta.source = Source::SignedClient;
            }
            let signed = SignedFields { nonce: 1, expires_at: u64::MAX, signature: [signature_word; 8] };
            let mut words = [0; CoreRecord::ABLATION_WORDS];
            words[..CoreRecord::WORDS].copy_from_slice(&record.to_words());
            words[CoreRecord::WORDS..].copy_from_slice(&signed.to_words());
            core_in.write(&words);
        }
        drop(core_in);
        let (events_in, events_out) = channel::<1>(1_024);
        let (stall, counters) = (SharedCounter::new(), ThreadCounters::new());
        let clock = RunClock::start();
        let sink = EventRingSink::new(events_in, clock, IDLE, &stall);
        let config = CoreConfig { next_seq: 1, stamps: Stamps::On, clock, idle: IDLE };
        let engine = Engine::new(EngineOptions::default());
        let (_, stats) = run_core(engine, core_out, sink, config, Some(verifier), &counters);
        drop(events_out);
        stats
    }

    #[test]
    fn in_the_ablation_every_signed_command_is_verified_before_it_is_applied() {
        let verifier = FakeVerifier::default();
        let stats = run_verifying(&verifier, 7);
        assert_eq!(stats.commands, 16);
        let places = commands().iter().filter(|c| matches!(c, Command::PlaceOrder(_))).count();
        assert_eq!(verifier.calls.into_inner(), places as u64, "signed commands only, not the operator's");
    }

    #[test]
    #[should_panic(expected = "a bad signature in the verify-on-core ablation")]
    fn in_the_ablation_a_bad_signature_stops_the_core() {
        run_verifying(&FakeVerifier::default(), 0);
    }
}
