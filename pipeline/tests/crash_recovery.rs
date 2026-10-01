//! Crash, recover, restart, crash again, on a simulated disk (`docs/PIPELINE.md` 18.2,
//! review findings F1 and F2).
//!
//! **What runs.** The real journal writer and the real recovery, doing their file
//! operations on a `SimDisk` (4 KiB segments, and 1 MiB in one test), which keeps the
//! durable bytes and the unsynced 512-byte sectors apart and can:
//! - lose power: keep a subset of the unsynced sectors, drop the rest;
//! - crash the process inside a write (a prefix of it reaches the page cache) or inside a
//!   sync (nothing more reaches the disk), the page cache surviving;
//! - fail a sync the way Linux does: return an error and mark the unsynced sectors clean
//!   without writing them, while reads still return them (11.7).
//!
//! A run is a sequence of lives: recover, start a new life where recovery says, write
//! batches, and crash in one of those ways. The model keeps `history`, the record that the
//! latest life to write each seq wrote, and a `floor`: the highest seq that must survive
//! (the watermark when the process died, or what the last recovery kept, whichever is
//! higher). After every recovery it checks:
//! 1. every record up to the floor is there, unchanged: a released record is never lost,
//!    and after a failed-sync restart and another power loss, what recovery kept is still
//!    there, since recovery made it durable;
//! 2. every record recovery accepted is the one the latest life wrote for that seq: nothing
//!    from an earlier life comes back after the point where that life's journal was cut.
//!
//! **The runs.** Random lives of every kind (300 runs of 6 lives), plus the two findings,
//! each in many random variants of its exact shape, plus a few random runs whose segments
//! are larger than the tail region `W` (622,720 bytes), as production's 1 GiB ones are:
//! with 4 KiB segments the tail region covers whole segments, so recovery's rule about
//! bytes more than `W` past the end never comes into play (review finding F-TEST-W-BOUND).
//! Those lives write batches of up to the full 4,096 records.
//! - F1: a life's last batch starts a new segment and the power fails; the header's sector
//!   is lost but some of the body's reach the disk. The next life reuses that segment, at
//!   the same offsets, with records of the same seqs and lengths, and even the same
//!   timestamps (both lives anchored their clock after the same end, as a clock stepped
//!   back would do), so only the zeroing of 11.8 keeps the old records out. The draft's
//!   rule let them back in.
//! - F2: a sync fails the Linux way; the restart on the same boot keeps what the page cache
//!   still shows, and a power loss right after must not take it away.
//!
//! **Longer soaks.** `CRASH_RECOVERY_SCALE=k` multiplies every count by `k`, with new seeds
//! past the default ones (e.g. `k` = 100: 30,000 runs, 30,000 F1 and 10,000 F2 variants,
//! about 11 s in `--release` here).

use pipeline::clock::RunClock;
use pipeline::codec::encode_command;
use pipeline::counters::Watermark;
use pipeline::journal::files::{Fault, SimDisk};
use pipeline::journal::format::{JournalIdentity, SegmentHeader};
use pipeline::journal::recovery::{Recovered, recover_files, scan_journal};
use pipeline::journal::writer::{JournalWriter, preallocate};
use pipeline::records::{InjectionMode, JournalRecord, Meta, SIGNATURE_WORDS, Source};

use engine::command::{CancelOrder, Command, SetMark};
use engine::engine::EngineOptions;
use engine::types::{AccountId, MarketId, OrderSeq, Price, order_id};

mod common;
use common::XorShift;

const SEGMENT: u64 = 4_096;
/// Signed (152-byte) records that fill a segment exactly: (4,096 − 128) / 152, rounded down.
const PER_SEGMENT: u64 = 26;
/// The larger segments of one test (module docs), and its batch sizes.
const LARGE_SEGMENT: u64 = 1 << 20;
const LARGE_BATCHES: [u64; 5] = [1, 64, 700, 2_000, 4_096];
/// Batch sizes up to 26, so batches often start a new 4 KiB segment (13 and 26 signed
/// records fill one exactly).
const SMALL_BATCHES: [u64; 7] = [1, 2, 3, 5, 8, 13, 26];

/// The run counts' multiplier: `CRASH_RECOVERY_SCALE`, 1 by default (module docs).
fn scale() -> u64 {
    std::env::var("CRASH_RECOVERY_SCALE").ok().and_then(|k| k.parse().ok()).unwrap_or(1)
}

fn identity() -> JournalIdentity {
    JournalIdentity::new(1, InjectionMode::Signed, EngineOptions::default())
}

/// Record `seq` of life `life`: a signed cancel (152 bytes) or an operator mark (80 bytes),
/// with `ts = anchor + seq`. Its contents depend on the life, so a record from an earlier
/// life never equals a later life's record with the same seq.
fn record(seq: u64, life: u64, anchor: u64, signed: bool) -> JournalRecord {
    let (meta, nonce, command) = if signed {
        let meta = Meta { source: Source::SignedClient, lane: 1, account: AccountId::new(9) };
        let cancel = CancelOrder {
            order_id: order_id(AccountId::new(9), OrderSeq::new(seq as u32)),
            market: MarketId::new(life as u16),
        };
        (meta, seq, Command::CancelOrder(cancel))
    } else {
        (
            Meta::OPERATOR,
            0,
            Command::SetMark(SetMark {
                price: Price::new((life * 1_000_000 + seq) as i64),
                market: MarketId::new(1),
            }),
        )
    };
    JournalRecord {
        seq,
        ts: anchor + seq,
        meta,
        nonce,
        command: encode_command(&command),
        // Kind 3 records carry no expiry and no signature (11.3).
        expires_at: if signed { u64::MAX - life } else { 0 },
        signature: if signed { [life << 32 | seq; SIGNATURE_WORDS] } else { [0; SIGNATURE_WORDS] },
    }
}

fn words(record: &JournalRecord) -> Vec<u64> {
    record.to_words()[..record.len_words()].to_vec()
}

/// How a life ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Crash {
    /// The process stops after its last flush; nothing is unfinished.
    AfterFlush,
    /// The process dies inside the last write: the first `n` bytes of it reach the page
    /// cache, which survives.
    InWrite(usize),
    /// The power fails during the last flush: its write reached the page cache, its sync
    /// never finished, and some of the unsynced sectors reach the disk.
    PowerLossInFlush,
    /// The last sync fails the Linux way, and the process stops; the restart is on the
    /// same boot.
    FailedSync,
    /// The last sync fails the Linux way, then the power fails too.
    FailedSyncThenPowerLoss,
}

impl Crash {
    /// The fault that makes the last flush end this way.
    fn fault(self) -> Option<Fault> {
        match self {
            Crash::AfterFlush => None,
            Crash::InWrite(n) => Some(Fault::CrashInWrite(n)),
            Crash::PowerLossInFlush => Some(Fault::CrashInSync),
            Crash::FailedSync | Crash::FailedSyncThenPowerLoss => Some(Fault::FailSync),
        }
    }

    fn loses_power(self) -> bool {
        matches!(self, Crash::PowerLossInFlush | Crash::FailedSyncThenPowerLoss)
    }
}

/// What one life does.
#[derive(Clone, Debug)]
struct LifePlan {
    /// Records per batch; each batch is flushed.
    batches: Vec<u64>,
    /// The share of records that are signed (the rest are operator marks), in percent.
    signed_percent: u64,
    /// The clock anchor: records get `ts = anchor + seq`.
    anchor: u64,
    crash: Crash,
    /// At a power loss: the share of unsynced sectors that reach the disk, in percent...
    keep_percent: u64,
    /// ...and whether the first unsynced sector (the new segment's header, when the last
    /// batch started a segment) is lost whatever the share.
    lose_first_sector: bool,
}

/// The model of 18.2 (module docs).
struct Model {
    segment_bytes: u64,
    disk: SimDisk,
    random: XorShift,
    /// `history[s - 1]`: the record the latest life to write seq `s` wrote.
    history: Vec<JournalRecord>,
    /// Every record up to this seq must survive.
    floor: u64,
    lives: u64,
}

impl Model {
    fn new(seed: u64) -> Model {
        Model::with_segments(seed, SEGMENT)
    }

    fn with_segments(seed: u64, segment_bytes: u64) -> Model {
        Model {
            segment_bytes,
            disk: SimDisk::new(segment_bytes),
            random: XorShift(seed | 1),
            history: Vec::new(),
            floor: 0,
            lives: 0,
        }
    }

    /// Recovers, checks both properties, and returns what recovery found.
    fn recover_and_check(&mut self, context: &str) -> Recovered {
        let recovered = recover_files(&mut self.disk, &identity(), false)
            .unwrap_or_else(|e| panic!("{context}: recovery failed: {e}"));
        let mut records = Vec::new();
        scan_journal(&mut self.disk, |_, record, _| records.push(*record)).expect("the journal reads back");
        assert_eq!(records.len() as u64, recovered.records, "{context}");
        assert!(
            recovered.records >= self.floor,
            "{context}: a released record was lost: recovered {} records, {} were durable",
            recovered.records,
            self.floor
        );
        for record in &records {
            let latest = &self.history[record.seq as usize - 1];
            assert_eq!(record, latest, "{context}: seq {} is not the latest life's record", record.seq);
        }
        // What recovery kept is durable now, and what it cut is gone for good.
        self.floor = recovered.records;
        self.history.truncate(recovered.records as usize);
        let end = recovered.end;
        if let Some(bytes) = self.disk.durable(end.segment) {
            assert!(bytes[end.offset as usize..].iter().all(|&b| b == 0), "{context}: zeros after the end");
        }
        recovered
    }

    /// A random life (module docs, "The runs"), with batches of at most one of `sizes`.
    fn random_plan(&mut self, recovered: &Recovered, sizes: &[u64]) -> LifePlan {
        let random = &mut self.random;
        // In half the lives every batch full.
        let limit = sizes[random.below(sizes.len() as u64) as usize];
        let full = random.percent(50);
        let batches =
            (0..random.below(8)).map(|_| if full { limit } else { 1 + random.below(limit) }).collect();
        let crash = match random.below(5) {
            0 => Crash::AfterFlush,
            1 => Crash::InWrite(random.below(2_000) as usize),
            2 => Crash::PowerLossInFlush,
            3 => Crash::FailedSync,
            _ => Crash::FailedSyncThenPowerLoss,
        };
        LifePlan {
            batches,
            signed_percent: if random.percent(50) { 100 } else { 70 },
            anchor: recovered.last_ts + 1_000_000,
            crash,
            keep_percent: random.below(101),
            lose_first_sector: random.percent(20),
        }
    }

    /// One life: start where recovery says, write the plan's batches, crash as it says.
    fn live(&mut self, recovered: &Recovered, plan: &LifePlan) {
        self.lives += 1;
        let life = self.lives;
        let first = recovered.end.next_life_segment();
        let preallocated = 1 + self.random.below(3) as u32;
        preallocate(&mut self.disk, first, preallocated).expect("segments created");
        let header = SegmentHeader::for_life(identity(), plan.anchor, [life as u8; 32]);
        let (clock, durable) = (RunClock::start(), Watermark::new(recovered.next_seq - 1));
        let disk = std::mem::replace(&mut self.disk, SimDisk::new(self.segment_bytes));
        let largest = plan.batches.iter().copied().max().unwrap_or(1) as usize;
        let mut writer = JournalWriter::new(disk, header, first, 0, largest);
        let mut seq = recovered.next_seq;
        'batches: for (i, &size) in plan.batches.iter().enumerate() {
            if i + 1 == plan.batches.len()
                && let Some(fault) = plan.crash.fault()
            {
                writer.files_mut().inject(fault);
            }
            for _ in 0..size {
                let record = record(seq, life, plan.anchor, self.random.percent(plan.signed_percent));
                self.history.truncate(seq as usize - 1);
                self.history.push(record);
                seq += 1;
                if writer.append(&words(&record), &clock, &durable).is_err() {
                    break 'batches; // the process died in a flush this append started
                }
            }
            if writer.flush(&clock, &durable).is_err() {
                break; // the process died
            }
        }
        self.floor = self.floor.max(durable.load());
        self.disk = writer.into_parts().0;
        if plan.crash.loses_power() {
            let (random, mut first) = (&mut self.random, plan.lose_first_sector);
            self.disk.lose_power(|| !std::mem::take(&mut first) && random.percent(plan.keep_percent));
        }
    }
}

#[test]
fn random_crash_sequences_never_lose_a_released_record_nor_revive_a_stale_one() {
    for run in 0..300 * scale() {
        let mut model = Model::new(0x9E37_79B9_7F4A_7C15 ^ run);
        let mut recovered = model.recover_and_check(&format!("run {run}, fresh"));
        for life in 1..=6 {
            let plan = model.random_plan(&recovered, &SMALL_BATCHES);
            model.live(&recovered, &plan);
            recovered = model.recover_and_check(&format!("run {run}, after life {life}: {plan:?}"));
        }
    }
}

#[test]
fn random_crash_sequences_with_segments_larger_than_the_tail_region() {
    for run in 0..6 * scale() {
        let mut model = Model::with_segments(0x5EED_1A26_E000 ^ run, LARGE_SEGMENT);
        let mut recovered = model.recover_and_check(&format!("run {run}, fresh"));
        for life in 1..=4 {
            let plan = model.random_plan(&recovered, &LARGE_BATCHES);
            model.live(&recovered, &plan);
            let batches = &plan.batches;
            recovered = model.recover_and_check(&format!("large run {run}, after life {life}: {batches:?}"));
        }
    }
}

#[test]
fn f1_a_lost_header_over_a_kept_body_never_brings_the_old_records_back() {
    let mut random = XorShift(0xF1);
    for variant in 0..300 * scale() {
        let mut model = Model::new(variant);
        let fresh = model.recover_and_check("fresh");
        // Life 1: segment 0 filled and synced, then a batch that starts segment 1, whose
        // sync never finishes; the power fails, losing segment 1's header sector and some
        // of the rest.
        let life_1 = LifePlan {
            batches: vec![PER_SEGMENT, 1 + random.below(PER_SEGMENT)],
            signed_percent: 100,
            anchor: 1_000_000,
            crash: Crash::PowerLossInFlush,
            keep_percent: random.below(101),
            lose_first_sector: true,
        };
        model.live(&fresh, &life_1);
        let context = format!("variant {variant}, after life 1: {life_1:?}");
        let recovered = model.recover_and_check(&context);
        assert_eq!((recovered.records, recovered.end.next_life_segment()), (PER_SEGMENT, 1), "{context}");
        // Life 2: the same segment, offsets, seqs, lengths and timestamps, fewer or more
        // records, and any ending.
        let n = 1 + random.below(PER_SEGMENT);
        let split = random.below(n + 1);
        let crash =
            [Crash::AfterFlush, Crash::PowerLossInFlush, Crash::InWrite(random.below(2_000) as usize)]
                [random.below(3) as usize];
        let life_2 = LifePlan {
            batches: [split, n - split].into_iter().filter(|&size| size > 0).collect(),
            crash,
            keep_percent: random.below(101),
            lose_first_sector: false,
            ..life_1.clone()
        };
        model.live(&recovered, &life_2);
        model.recover_and_check(&format!("variant {variant}, after life 2: {life_1:?} then {life_2:?}"));
    }
}

#[test]
fn f2_what_recovery_keeps_after_a_failed_sync_survives_the_next_power_loss() {
    let mut random = XorShift(0xF2);
    for variant in 0..100 * scale() {
        // At most 24 records: they stay in segment 0, so the failing sync is the last one.
        let batches = vec![1 + random.below(12), 1 + random.below(12)];
        let plan = LifePlan {
            batches: batches.clone(),
            signed_percent: 70,
            anchor: 1_000_000,
            crash: Crash::FailedSync,
            keep_percent: 0,
            lose_first_sector: false,
        };
        let written = batches.iter().sum::<u64>();
        // Without a restart in between, a power loss loses the batch whose sync failed.
        let mut unrecovered = Model::new(variant);
        let fresh = unrecovered.recover_and_check("fresh");
        unrecovered.live(&fresh, &plan);
        unrecovered.disk.lose_power(|| true); // nothing is unsynced: the failed sync "cleaned" it
        let lost = unrecovered.recover_and_check("power loss before any restart").records;
        assert!(lost < written, "variant {variant}: the failed batch is lost: {lost} of {written}");
        // With the restart on the same boot, recovery keeps what the page cache still shows,
        // and re-writes and syncs it, so the same power loss keeps it.
        let mut model = Model::new(variant);
        let fresh = model.recover_and_check("fresh");
        model.live(&fresh, &plan);
        let kept = model.recover_and_check("restart on the same boot").records;
        assert_eq!(kept, written, "variant {variant}: the page cache still shows every record");
        model.disk.lose_power(|| true);
        let after = model.recover_and_check("then a power loss").records;
        assert_eq!(after, written, "variant {variant}: recovery made them durable");
    }
}
