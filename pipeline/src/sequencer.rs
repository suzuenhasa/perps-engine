//! The sequencer: merges the operator ring and the client lanes, gives each command its seq
//! and timestamp, and sends it to the journal writer and then the core
//! (`docs/PIPELINE.md` section 9).
//!
//! **Contract.**
//! - Every record taken gets the next seq, 1, 2, 3, ... with no gap (`next_seq` after a
//!   restart), and one clock read, `t_seq`, which gives both its journal timestamp
//!   `ts = run_start_unix_ns + t_seq` and its measurement stamp (9.2). `ts` must be
//!   strictly increasing (recovery refuses a record whose `ts` isn't above the previous
//!   one, 11.8), but `Instant` can return the same value twice (WSL2's clock source counts
//!   in 100 ns steps), so `t_seq` is at least the previous one plus 1 ns.
//! - Each record is written and published to the journal ring, then to the core ring, as
//!   soon as it is taken (9.1, C54): the record can join the writer's current batch as early
//!   as possible, and no record waits for the rest of its batch.
//! - **Space first** (2.4): once a record has a seq it must reach both rings, or the
//!   journal would have a gap. So the sequencer takes only as many records as both rings
//!   have room for, and when they are full it takes nothing: the pressure moves back into
//!   the lanes, where the gateways reject `Busy`. A pass in which a ring had less room than
//!   the records waiting (it was full, or filled) is counted, per ring (15.4): a full core
//!   ring means the core is behind, a full journal ring the journal writer or the disk.
//! - **Order.** The operator ring first on every pass, up to 64 records (strict priority,
//!   section 8); then each lane, up to 32 records, round-robin from a start that rotates by
//!   one lane per pass, so no lane waits for long (9.1). Which lane goes first depends on
//!   thread timing, so two runs are sequenced differently; the journal records the order
//!   chosen, and replay follows the journal. One account's messages keep their order: they
//!   use one lane, and a lane is FIFO.
//! - It stops when the operator ring and every lane are closed and drained; dropping its
//!   two producers then closes the journal and core rings (2.8).
//! - **The core ring's record** is 12 words on 2 lines (7 with stamps off). The "verify on
//!   core" ablation (section 16) uses a 3-line core ring instead, `CORE_LINES = 3`, whose
//!   record adds the nonce, the expiry and the signature (22 words, 3.3), in both of its
//!   arms. `CORE_LINES` is a constant, so each pipeline compiles only its own branch.
//!
//! **Trust.** Only our own code writes the lanes and the operator ring. A lane record whose
//! source isn't the journal's client kind (1 in a signed run, 2 in a pre-verified one) is a
//! bug that recovery would later refuse (11.3), so it panics here instead, which aborts the
//! process before the record is journaled.
//!
//! **Complexity.** Per record: one clock read, two ring writes (10 or 19 words, and 12 or
//! 7, or 22 in the ablation), two Release stores. Per pass: one `available` per input and two `free`s per input
//! that has records.

use crate::clock::RunClock;
use crate::counters::{BusyMeter, PipelineCounters};
use crate::idle::IdleStrategy;
use crate::records::{
    ClientRecord, CoreRecord, InjectionMode, JournalRecord, Meta, OperatorRecord, SIGNATURE_WORDS,
    SignedFields, Source, Stamps,
};
use crate::ring::{Consumer, Producer};

/// Operator records taken per pass, at most (section 8).
pub const OPERATOR_BATCH: usize = 64;
/// Records taken from one lane per pass, at most (9.1).
pub const LANE_BATCH: usize = 32;

/// What the sequencer reads: one lane per gateway (or per pre-verified sender lane) and the
/// operator ring (PIPELINE.md 19.2).
#[derive(Debug)]
pub struct Inputs {
    pub lanes: Vec<Consumer<3>>,
    pub operator: Consumer<1>,
}

/// What the sequencer counted, returned when its thread is joined.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SequencerStats {
    /// Records sequenced (and journaled).
    pub records: u64,
    /// Passes in which the core ring had less room than the records waiting (15.4).
    pub core_full_passes: u64,
    /// The same for the journal ring.
    pub journal_full_passes: u64,
}

/// The sequencer's two output rings and everything it needs to give a record its seq.
#[derive(Debug)]
struct Outputs<const CORE_LINES: usize> {
    journal: Producer<3>,
    core: Producer<CORE_LINES>,
    clock: RunClock,
    stamps: Stamps,
    next_seq: u64,
    /// The last record's `t_seq`: the next one is at least 1 more (module docs).
    last_t_seq: u64,
    stats: SequencerStats,
    /// The first `t_seq` of the current pass, for the busy meter.
    pass_started: Option<u64>,
    /// In the current pass, the core ring (the journal ring) had less room than a take
    /// wanted: it was full, or filled.
    core_was_full: bool,
    journal_was_full: bool,
}

impl<const CORE_LINES: usize> Outputs<CORE_LINES> {
    /// How many records both output rings have room for, up to `limit`, noting a ring that
    /// has less (module docs). Reloads a ring's `head` only if its cached view shows fewer
    /// than `limit` free (3.1), so a ring noted as full really had fewer than `limit` free.
    fn room(&mut self, limit: usize) -> usize {
        let (core, journal) = (self.core.free(limit), self.journal.free(limit));
        self.core_was_full |= core < limit;
        self.journal_was_full |= journal < limit;
        limit.min(core).min(journal)
    }

    /// Gives `record` the next seq and a timestamp, and writes it to the journal ring, then
    /// the core ring, publishing each at once (module docs). The caller checked `room`.
    fn sequence(&mut self, record: &ClientRecord) {
        let t_seq = self.clock.now().max(self.last_t_seq + 1);
        self.last_t_seq = t_seq;
        self.pass_started.get_or_insert(t_seq);
        let seq = self.next_seq;
        self.next_seq += 1;
        let journal = JournalRecord {
            seq,
            ts: self.clock.unix_ns(t_seq),
            meta: record.meta,
            nonce: record.nonce,
            command: record.command,
            expires_at: record.expires_at,
            signature: record.signature,
        };
        self.journal.write(&journal.to_words()[..journal.len_words()]);
        self.journal.publish(); // journal first (9.3)
        let core = CoreRecord {
            seq,
            meta: record.meta,
            command: record.command,
            t_sched: record.t_sched,
            t_sent: record.t_sent,
            t_gw_in: record.t_gw_in,
            t_gw_out: record.t_gw_out,
            t_seq,
        };
        if CORE_LINES == 3 {
            // The "verify on core" ablation's record (3.3, section 16).
            let signed = SignedFields {
                nonce: record.nonce,
                expires_at: record.expires_at,
                signature: record.signature,
            };
            let mut words = [0; CoreRecord::ABLATION_WORDS];
            words[..CoreRecord::WORDS].copy_from_slice(&core.to_words());
            words[CoreRecord::WORDS..].copy_from_slice(&signed.to_words());
            self.core.write(&words);
        } else {
            self.core.write(&core.to_words()[..CoreRecord::words_for(self.stamps)]);
        }
        self.core.publish();
        self.stats.records += 1;
    }
}

/// The merge rule of 9.1 as a plain value: [`Sequencer::pass`] is one pass of the loop, so
/// tests drive it one pass at a time; [`run_sequencer`] is the thread. `CORE_LINES` is the
/// core ring's slot size: 2, or 3 in the "verify on core" ablation (module docs).
#[derive(Debug)]
pub struct Sequencer<const CORE_LINES: usize = 2> {
    inputs: Inputs,
    out: Outputs<CORE_LINES>,
    /// The kind every lane record must carry (module docs, "Trust").
    client_source: Source,
    /// The lane the next pass starts with.
    start: usize,
    busy: BusyMeter,
}

impl<const CORE_LINES: usize> Sequencer<CORE_LINES> {
    /// A sequencer that gives the first record `next_seq` (1 for a new journal, the
    /// recovered journal's next seq after a restart, 13.5).
    pub fn new(
        inputs: Inputs,
        journal: Producer<3>,
        core: Producer<CORE_LINES>,
        mode: InjectionMode,
        stamps: Stamps,
        clock: RunClock,
        next_seq: u64,
    ) -> Self {
        let out = Outputs {
            journal,
            core,
            clock,
            stamps,
            next_seq,
            last_t_seq: 0,
            stats: SequencerStats::default(),
            pass_started: None,
            core_was_full: false,
            journal_was_full: false,
        };
        Self { inputs, out, client_source: mode.client_source(), start: 0, busy: BusyMeter::new() }
    }

    /// The seq the next record will get.
    pub fn next_seq(&self) -> u64 {
        self.out.next_seq
    }

    /// One pass of the merge rule (9.1): the operator ring, then every lane from the
    /// rotating start. Returns how many records it took, and publishes the busy time and
    /// the full-ring counts to `counters`.
    pub fn pass(&mut self, counters: &PipelineCounters) -> usize {
        let mut took = take(&mut self.inputs.operator, OPERATOR_BATCH, &mut self.out, read_operator);
        let lanes = self.inputs.lanes.len();
        for i in 0..lanes {
            let client_source = self.client_source;
            let lane = &mut self.inputs.lanes[(self.start + i) % lanes];
            took += take(lane, LANE_BATCH, &mut self.out, |lane| read_lane(lane, client_source));
        }
        if lanes > 0 {
            self.start = (self.start + 1) % lanes;
        }
        if let Some(started) = self.out.pass_started.take() {
            self.busy.begin(started);
            self.busy.end(self.out.clock.now(), &counters.sequencer.busy_ns);
        }
        let out = &mut self.out;
        if std::mem::take(&mut out.core_was_full) {
            out.stats.core_full_passes += 1;
            counters.sequencer_core_full_passes.store(out.stats.core_full_passes);
        }
        if std::mem::take(&mut out.journal_was_full) {
            out.stats.journal_full_passes += 1;
            counters.sequencer_journal_full_passes.store(out.stats.journal_full_passes);
        }
        took
    }

    /// True once the operator ring and every lane are closed and drained.
    pub fn inputs_finished(&mut self) -> bool {
        self.inputs.operator.is_finished() && self.inputs.lanes.iter_mut().all(Consumer::is_finished)
    }

    /// The counts so far. Dropping the sequencer drops its producers, which closes the
    /// journal and core rings (2.8).
    pub fn into_stats(self) -> SequencerStats {
        self.out.stats
    }
}

/// `take(input, limit)` of 9.1: as many records as the input has, up to `limit`, but only
/// as many as both output rings have room for (space first; a ring with less room is noted
/// for the pass's count). Frees the input's slots once the records are sequenced.
fn take<const LINES: usize, const CORE_LINES: usize>(
    input: &mut Consumer<LINES>,
    limit: usize,
    out: &mut Outputs<CORE_LINES>,
    mut read: impl FnMut(&mut Consumer<LINES>) -> ClientRecord,
) -> usize {
    let available = input.available(limit).min(limit);
    if available == 0 {
        return 0; // nothing to take: don't look at the output rings' counters
    }
    let k = out.room(available);
    for _ in 0..k {
        let record = read(input);
        out.sequence(&record);
    }
    input.release();
    k
}

/// Reads one operator record, as a [`ClientRecord`] with the operator's meta and no nonce,
/// expiry, signature or gateway stamps.
fn read_operator(input: &mut Consumer<1>) -> ClientRecord {
    let mut words = [0; OperatorRecord::WORDS];
    input.read(&mut words);
    let operator = OperatorRecord::from_words(&words);
    ClientRecord {
        meta: Meta::OPERATOR,
        nonce: 0,
        command: operator.command,
        expires_at: 0,
        signature: [0; SIGNATURE_WORDS],
        t_sched: operator.t_sched,
        t_sent: operator.t_sent,
        t_gw_in: 0,
        t_gw_out: 0,
    }
}

/// Reads one lane record, which must carry the journal's client kind (module docs).
fn read_lane(input: &mut Consumer<3>, client_source: Source) -> ClientRecord {
    let mut words = [0; ClientRecord::WORDS];
    input.read(&mut words);
    let record = ClientRecord::from_words(&words);
    assert_eq!(
        record.meta.source, client_source,
        "a lane record of the wrong kind; recovery would refuse it (PIPELINE.md 11.3)"
    );
    record
}

/// The sequencer's thread loop (9.1): pass after pass until every input is closed and
/// drained. Returning drops the sequencer, which closes the journal and core rings (2.8).
pub fn run_sequencer<const CORE_LINES: usize>(
    mut sequencer: Sequencer<CORE_LINES>,
    idle: IdleStrategy,
    counters: &PipelineCounters,
) -> SequencerStats {
    loop {
        if sequencer.pass(counters) == 0 {
            if sequencer.inputs_finished() {
                break;
            }
            idle.idle();
        }
    }
    sequencer.into_stats()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::encode_command;
    use crate::ring::channel;
    use engine::command::{CancelOrder, Command, SetMark};
    use engine::types::order_id;

    /// A sequencer over `lanes` lanes, with its input producers and output consumers.
    struct Rig {
        sequencer: Sequencer,
        lanes: Vec<Producer<3>>,
        operator: Producer<1>,
        journal: Consumer<3>,
        core: Consumer<2>,
        counters: PipelineCounters,
    }

    fn rig(lanes: usize, core_capacity: usize, stamps: Stamps, next_seq: u64) -> Rig {
        let (lane_producers, lane_consumers): (Vec<_>, Vec<_>) =
            (0..lanes).map(|_| channel::<3>(256)).unzip();
        let (operator, operator_consumer) = channel::<1>(256);
        let (journal_producer, journal) = channel::<3>(1_024);
        let (core_producer, core) = channel::<2>(core_capacity);
        let inputs = Inputs { lanes: lane_consumers, operator: operator_consumer };
        let clock = RunClock::anchored_at(1_790_000_000_000_000_000);
        let sequencer = Sequencer::new(
            inputs,
            journal_producer,
            core_producer,
            InjectionMode::PreVerified,
            stamps,
            clock,
            next_seq,
        );
        Rig { sequencer, lanes: lane_producers, operator, journal, core, counters: PipelineCounters::new() }
    }

    /// A pre-verified cancel of account `account`'s order `n`, on lane `lane`.
    fn client(lane: u16, account: u32, n: u32) -> ClientRecord {
        ClientRecord {
            meta: Meta { source: Source::PreVerifiedClient, lane, account },
            nonce: u64::from(n),
            command: encode_command(&Command::CancelOrder(CancelOrder {
                order_id: order_id(account, n),
                market: 1,
            })),
            expires_at: 0,
            signature: [0; SIGNATURE_WORDS],
            t_sched: 100 + u64::from(n),
            t_sent: 200 + u64::from(n),
            t_gw_in: 0,
            t_gw_out: 0,
        }
    }

    fn operator(price: i64) -> OperatorRecord {
        let command = encode_command(&Command::SetMark(SetMark { price, market: 1 }));
        OperatorRecord { command, t_sched: 7, t_sent: 8 }
    }

    impl Rig {
        fn send(&mut self, lane: usize, record: &ClientRecord) {
            self.lanes[lane].write(&record.to_words());
            self.lanes[lane].publish();
        }

        fn send_operator(&mut self, record: &OperatorRecord) {
            self.operator.write(&record.to_words());
            self.operator.publish();
        }

        fn pass(&mut self) -> usize {
            self.sequencer.pass(&self.counters)
        }

        /// Every journal record and core record published so far.
        fn drain(&mut self) -> Vec<(JournalRecord, CoreRecord)> {
            let mut out = Vec::new();
            while self.journal.available(1) > 0 {
                let len = JournalRecord::len_of(self.journal.peek(0)) as usize / 8;
                let mut words = [0; JournalRecord::SIGNED_WORDS];
                self.journal.read(&mut words[..len]);
                let journal = JournalRecord::from_words(&words[..len]).expect("a valid record");
                assert!(self.core.available(1) > 0, "the core ring has the same records");
                let mut core_words = [0; CoreRecord::WORDS];
                self.core.read(&mut core_words);
                out.push((journal, CoreRecord::from_words(&core_words)));
            }
            self.journal.release();
            self.core.release();
            out
        }
    }

    #[test]
    fn each_record_gets_the_next_seq_and_goes_to_the_journal_and_the_core_alike() {
        let mut rig = rig(2, 64, Stamps::On, 41);
        rig.send(0, &client(0, 10, 1));
        rig.send(1, &client(1, 11, 1));
        rig.send_operator(&operator(100));
        assert_eq!(rig.pass(), 3);
        let out = rig.drain();
        let seqs: Vec<u64> = out.iter().map(|(j, _)| j.seq).collect();
        assert_eq!(seqs, [41, 42, 43], "numbered from next_seq, with no gap");
        let first_meta: Vec<Meta> = out.iter().map(|(j, _)| j.meta).collect();
        assert_eq!(first_meta[0], Meta::OPERATOR, "the operator ring first");
        assert_eq!((first_meta[1].lane, first_meta[2].lane), (0, 1));
        for (journal, core) in &out {
            assert_eq!(journal.seq, core.seq);
            assert_eq!((journal.meta, journal.command), (core.meta, core.command));
            assert_eq!(journal.ts, 1_790_000_000_000_000_000 + core.t_seq, "one clock read for both");
            assert!(core.t_seq >= 1);
        }
        let (journal, core) = &out[1];
        assert_eq!((journal.nonce, core.t_sched, core.t_sent), (1, 101, 201));
        assert_eq!(out[0].0.nonce, 0);
        assert_eq!(rig.sequencer.next_seq(), 44);
    }

    #[test]
    fn timestamps_strictly_increase_even_when_the_clock_repeats_itself() {
        let mut rig = rig(1, 1_024, Stamps::On, 1);
        for n in 1..=200 {
            rig.send(0, &client(0, 10, n));
        }
        while rig.pass() > 0 {}
        let out = rig.drain();
        assert_eq!(out.len(), 200);
        assert!(out.windows(2).all(|pair| pair[1].0.ts > pair[0].0.ts), "every ts above the one before");
    }

    #[test]
    fn the_operator_ring_takes_up_to_64_and_each_lane_up_to_32_per_pass() {
        let mut rig = rig(2, 1_024, Stamps::On, 1);
        for n in 0..100 {
            rig.send_operator(&operator(n));
        }
        for n in 1..=40 {
            rig.send(0, &client(0, 10, n));
            rig.send(1, &client(1, 11, n));
        }
        assert_eq!(rig.pass(), 64 + 32 + 32);
        assert_eq!(rig.pass(), 36 + 8 + 8);
        let out = rig.drain();
        assert_eq!(out.len(), 180);
        // Pass 1: 64 marks, lane 0's 32, lane 1's 32; pass 2 starts with lane 1.
        assert!(out[..64].iter().all(|(j, _)| j.meta == Meta::OPERATOR));
        assert!(out[64..96].iter().all(|(j, _)| j.meta.lane == 0));
        assert!(out[96..128].iter().all(|(j, _)| j.meta.lane == 1));
        assert!(out[128..164].iter().all(|(j, _)| j.meta == Meta::OPERATOR));
        assert!(out[164..172].iter().all(|(j, _)| j.meta.lane == 1), "the start rotated");
        assert!(out[172..180].iter().all(|(j, _)| j.meta.lane == 0));
        // One lane's records keep their order.
        let lane_0: Vec<u64> = out
            .iter()
            .filter(|(j, _)| j.meta.lane == 0 && j.meta.account == 10)
            .map(|(j, _)| j.nonce)
            .collect();
        assert_eq!(lane_0, (1..=40).collect::<Vec<_>>());
    }

    #[test]
    fn it_takes_only_what_both_rings_have_room_for_and_counts_a_full_pass() {
        let mut rig = rig(1, 4, Stamps::On, 1);
        for n in 1..=10 {
            rig.send(0, &client(0, 10, n));
        }
        assert_eq!(rig.pass(), 4, "the core ring has 4 slots");
        assert_eq!(rig.counters.sequencer_core_full_passes.load(), 1, "it filled");
        assert_eq!(rig.pass(), 0, "no room: takes nothing");
        assert_eq!(rig.counters.sequencer_core_full_passes.load(), 2, "one per pass, not per take");
        assert_eq!(rig.counters.sequencer_journal_full_passes.load(), 0, "the journal ring had room");
        assert_eq!(rig.drain().len(), 4);
        assert_eq!(rig.pass(), 4);
        assert_eq!(rig.drain().iter().map(|(j, _)| j.seq).collect::<Vec<_>>(), [5, 6, 7, 8], "no gap");
    }

    #[test]
    fn with_stamps_off_the_core_record_is_one_line() {
        let mut rig = rig(1, 64, Stamps::Off, 1);
        rig.send(0, &client(0, 10, 1));
        rig.pass();
        let mut words = [u64::MAX; CoreRecord::WORDS];
        assert_eq!(rig.core.available(1), 1);
        rig.core.read(&mut words);
        assert_eq!(words[0], 1);
        assert_eq!(words[7..], [0; 5], "the stamp words were never written (the slot's zeros)");
    }

    #[test]
    fn a_three_line_core_ring_carries_the_signed_fields_for_the_ablation() {
        let (mut lane, lane_consumer) = channel::<3>(16);
        let (_operator, operator_consumer) = channel::<1>(16);
        let (journal_producer, _journal) = channel::<3>(16);
        let (core_producer, mut core) = channel::<3>(16);
        let inputs = Inputs { lanes: vec![lane_consumer], operator: operator_consumer };
        let clock = RunClock::start();
        let mut sequencer = Sequencer::<3>::new(
            inputs,
            journal_producer,
            core_producer,
            InjectionMode::Signed,
            Stamps::On,
            clock,
            1,
        );
        let signature = std::array::from_fn(|i| 0xA0 + i as u64);
        let signed = ClientRecord {
            meta: Meta { source: Source::SignedClient, lane: 0, account: 10 },
            expires_at: 12_345,
            signature,
            ..client(0, 10, 7)
        };
        lane.write(&signed.to_words());
        lane.publish();
        assert_eq!(sequencer.pass(&PipelineCounters::new()), 1);
        let mut words = [0; CoreRecord::ABLATION_WORDS];
        core.read(&mut words);
        let record = CoreRecord::from_words(words[..CoreRecord::WORDS].try_into().expect("12 words"));
        assert_eq!((record.seq, record.meta, record.command), (1, signed.meta, signed.command));
        let fields = SignedFields::from_words(words[CoreRecord::WORDS..].try_into().expect("10 words"));
        assert_eq!(fields, SignedFields { nonce: 7, expires_at: 12_345, signature });
    }

    #[test]
    fn it_finishes_when_every_input_is_closed_and_drained_and_closes_its_outputs() {
        let mut rig = rig(2, 64, Stamps::On, 1);
        rig.send(1, &client(1, 11, 1));
        let Rig { sequencer, lanes, operator, mut journal, mut core, counters } = rig;
        drop(lanes);
        drop(operator);
        let stats = run_sequencer(sequencer, IdleStrategy::Spin, &counters);
        assert_eq!(stats.records, 1);
        assert!(journal.available(1) == 1 && core.available(1) == 1);
        let mut words = [0; CoreRecord::WORDS];
        core.read(&mut words);
        assert!(core.is_finished(), "closed after the last record");
        let mut words = [0; JournalRecord::SIGNED_WORDS];
        journal.read(&mut words[..10]);
        assert!(journal.is_finished());
    }

    #[test]
    #[should_panic(expected = "wrong kind")]
    fn a_lane_record_of_the_wrong_kind_panics() {
        let mut rig = rig(1, 64, Stamps::On, 1);
        let signed = ClientRecord {
            meta: Meta { source: Source::SignedClient, lane: 0, account: 10 },
            ..client(0, 10, 1)
        };
        rig.send(0, &signed);
        rig.pass();
    }
}
