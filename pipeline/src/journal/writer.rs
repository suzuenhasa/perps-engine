//! The journal writer: group commit into preallocated segments, the durable watermark, and
//! discard mode (`docs/PIPELINE.md` 11.5 to 11.7; `docs/DECISIONS.md` D-025).
//!
//! **Contract.** The writer takes the sequencer's records in seq order and appends each to
//! the current batch as its on-disk bytes with its CRC. It makes a batch durable with one
//! `pwrite` and one `fdatasync`, then stores the batch's last seq into the durable
//! watermark (Release): every record with `seq <= durable` is on disk (11.7). A batch never
//! spans two segments, and never holds more than `B` (at most 4,096) records, which bounds
//! what one unfinished flush can leave behind for recovery (11.8).
//!
//! **The flush rule** ([`should_flush`]): flush when the batch holds `B` records, or when its
//! oldest record was *sequenced* at least `T` ago (its own `t_seq`, not the time the writer
//! took it, so time spent waiting in the ring during the previous flush counts), or when
//! the journal ring is closed and drained. `T` = 0 flushes whenever there is anything.
//!
//! **Segments.** A life writes forward from its first segment, which exists (and is all
//! zeros) before the writer starts. The header goes in front of the first batch written
//! into a segment, so it becomes durable with that batch. When a record doesn't fit in
//! what is left of the segment, the batch so far is flushed, and the record starts the
//! next segment (created now, and counted, if preallocation didn't make it; 11.6). The rest
//! of the old segment stays zero.
//!
//! **Errors** (11.7). A failed `write` or `fdatasync` is returned by [`JournalWriter::flush`];
//! the thread ([`run_writer`]) panics on it, which aborts the process: the watermark stops,
//! nothing more is released, and recovery takes over at the next start. Retrying is unsafe
//! on Linux, which reports a writeback error once and marks the pages clean.
//!
//! **Discard mode** (11.6): the same writer over [`DiscardFiles`], which writes and syncs
//! nothing. It still encodes, checksums, applies the rule and publishes the watermark.
//!
//! **Busy time** (2.2, 15.4). The thread's busy time is its CPU work: taking records,
//! encoding and checksumming them, and the `write` into the page cache. The time it spends
//! blocked in `fdatasync` is the disk's, not the thread's: it is left out of the busy time
//! and published on its own, with the count of flushes, so the report can say "the writer
//! was 1% busy and 85% in fdatasync" rather than blame the thread for the disk (review
//! finding F1-limit-label-fdatasync).
//!
//! **Complexity.** Per record: its words to bytes and one CRC over them (slicing by 8).
//! Per batch: one write and one sync. No allocation after start: the batch buffer is
//! reserved (and its pages touched) for `B` of the longest records and a header.
//!
//! [`DiscardFiles`]: super::files::DiscardFiles

use std::io;
use std::path::PathBuf;

use crate::clock::RunClock;
use crate::counters::{BusyMeter, PipelineCounters, Watermark};
use crate::histogram::LatencyHistogram;
use crate::idle::IdleStrategy;
use crate::records::JournalRecord;
use crate::ring::Consumer;

use super::files::JournalFiles;
use super::format::{
    DEFAULT_SEGMENT_BYTES, HEADER_BYTES, MAX_BATCH_RECORDS, MAX_RECORD_BYTES, SegmentHeader, append_record,
};
use super::{JournalError, JournalPosition};

/// The journal's settings (PIPELINE.md 19.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalConfig {
    /// `<run_dir>/journal`.
    pub dir: PathBuf,
    /// `SEG_BYTES`: 1 GiB; tests use 4 KiB.
    pub segment_bytes: u64,
    /// `T`, in nanoseconds: 1 ms by default; 0 flushes whenever there is anything.
    pub commit_interval_ns: u64,
    /// `B`: 4,096 by default, never more.
    pub max_batch: usize,
    pub mode: JournalMode,
    /// Segments to create before the run, from the life's first one ([`segments_needed`]).
    /// At least the first is always created.
    pub preallocate: u32,
}

/// Where the records go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalMode {
    /// To the segment files, with `fdatasync`.
    Disk,
    /// Nowhere (11.6): only for the core-path search. Nothing it releases was on disk.
    Discard,
}

impl JournalConfig {
    /// The defaults of 11.5: 1 GiB segments, `T` = 1 ms, `B` = 4,096, on disk, one segment
    /// preallocated.
    pub fn new(dir: PathBuf) -> JournalConfig {
        JournalConfig {
            dir,
            segment_bytes: DEFAULT_SEGMENT_BYTES,
            commit_interval_ns: 1_000_000,
            max_batch: MAX_BATCH_RECORDS,
            mode: JournalMode::Disk,
            preallocate: 1,
        }
    }

    /// Refuses settings the rest of the journal relies on not happening: a batch limit
    /// above 4,096 (recovery's tail region, 11.8) or of 0, and a segment too small for a
    /// header and one record, or not a whole number of words.
    pub fn check(&self) -> Result<(), JournalError> {
        if self.max_batch == 0 || self.max_batch > MAX_BATCH_RECORDS {
            return Err(JournalError::Refused(format!(
                "the journal's batch limit B must be 1 to {MAX_BATCH_RECORDS}, not {}",
                self.max_batch
            )));
        }
        let smallest = (HEADER_BYTES + MAX_RECORD_BYTES) as u64;
        if self.segment_bytes < smallest || !self.segment_bytes.is_multiple_of(8) {
            return Err(JournalError::Refused(format!(
                "a segment must be a multiple of 8 bytes and at least {smallest}, not {}",
                self.segment_bytes
            )));
        }
        Ok(())
    }
}

/// How many segments a run needs (11.6):
/// `ceil(expected records × record size × 1.25 / (SEG_BYTES − 128)) + 1`.
pub fn segments_needed(expected_records: u64, record_bytes: u64, segment_bytes: u64) -> u32 {
    let bytes = u128::from(expected_records) * u128::from(record_bytes) * 5;
    let room = 4 * u128::from(segment_bytes - HEADER_BYTES as u64);
    u32::try_from(bytes.div_ceil(room) + 1).unwrap_or(u32::MAX)
}

/// Creates segments `first` to `first + count - 1` that don't exist yet, before a run
/// (11.6), and gets every one of them ready for writing (`JournalFiles::prepare`), so that
/// moving into one during the run allocates nothing. Returns how many it created.
pub fn preallocate(files: &mut impl JournalFiles, first: u32, count: u32) -> io::Result<u32> {
    let mut created = 0;
    for segment in first..first.saturating_add(count) {
        if !files.exists(segment)? {
            files.create_segment(segment)?;
            created += 1;
        }
        files.prepare(segment)?;
    }
    Ok(created)
}

/// The flush rule of 11.5, as a pure function: flush a non-empty batch when it holds
/// `max_batch` records, when its oldest record was sequenced at least `commit_interval_ns`
/// before `now`, or when no more records will come (`finished`).
pub fn should_flush(
    batch_len: usize,
    oldest_t_seq: u64,
    now: u64,
    commit_interval_ns: u64,
    max_batch: usize,
    finished: bool,
) -> bool {
    // `saturating_sub`: stamps taken on two CPUs could, in principle, be out of order.
    batch_len > 0
        && (batch_len >= max_batch || now.saturating_sub(oldest_t_seq) >= commit_interval_ns || finished)
}

/// What the writer measured (15.4, 15.8). Returned when the thread is joined.
#[derive(Clone, Debug)]
pub struct JournalStats {
    /// `write` plus `fdatasync`, per flush.
    pub flush_ns: LatencyHistogram,
    /// `fdatasync` alone, per flush.
    pub fdatasync_ns: LatencyHistogram,
    /// Records per flush (a histogram of counts, not of nanoseconds).
    pub batch_records: LatencyHistogram,
    /// Bytes per flush, headers included.
    pub batch_bytes: LatencyHistogram,
    pub flushes: u64,
    pub records: u64,
    pub bytes: u64,
    /// Nanoseconds spent in `fdatasync`, in total.
    pub fdatasync_total_ns: u64,
    /// Segments the writer had to create itself because preallocation ran out: each makes
    /// a flush slow, and flags the run (11.6).
    pub segments_created: u32,
}

impl JournalStats {
    fn new() -> JournalStats {
        JournalStats {
            flush_ns: LatencyHistogram::new(),
            fdatasync_ns: LatencyHistogram::new(),
            batch_records: LatencyHistogram::new(),
            batch_bytes: LatencyHistogram::new(),
            flushes: 0,
            records: 0,
            bytes: 0,
            fdatasync_total_ns: 0,
            segments_created: 0,
        }
    }
}

/// The writer's state: the current segment, and the batch being collected. A plain value
/// with no thread or ring, so the tests (and the crash tests of 18.2) drive it directly;
/// [`run_writer`] is the thread loop around it.
#[derive(Debug)]
pub struct JournalWriter<F> {
    files: F,
    /// This life's header; `segment` and `first_seq` are set for each segment it starts.
    header: SegmentHeader,
    commit_interval_ns: u64,
    max_batch: usize,
    segment: u32,
    /// Where `batch` goes in the segment: everything before it is written and synced.
    offset: u64,
    /// The segment has no header yet: the next record puts one in front of itself.
    header_due: bool,
    /// The batch's encoded records (and a header, if it starts a segment).
    batch: Vec<u8>,
    /// Records in the batch.
    count: usize,
    /// The first record's `t_seq`, which the flush rule measures `T` from.
    oldest_t_seq: u64,
    /// The last record's seq: the watermark after the flush.
    last_seq: u64,
    stats: JournalStats,
}

impl<F: JournalFiles> JournalWriter<F> {
    /// A writer for a life that starts in `first_segment`, which must already exist, all
    /// zeros (11.8, step 4). `header` is the life's header ([`SegmentHeader::for_life`]).
    pub fn new(
        files: F,
        header: SegmentHeader,
        first_segment: u32,
        commit_interval_ns: u64,
        max_batch: usize,
    ) -> JournalWriter<F> {
        assert!((1..=MAX_BATCH_RECORDS).contains(&max_batch), "B must be 1 to 4,096 (JournalConfig::check)");
        let mut batch = Vec::with_capacity(HEADER_BYTES + max_batch * MAX_RECORD_BYTES);
        pre_touch(&mut batch);
        JournalWriter {
            files,
            header,
            commit_interval_ns,
            max_batch,
            segment: first_segment,
            offset: 0,
            header_due: true,
            batch,
            count: 0,
            oldest_t_seq: 0,
            last_seq: 0,
            stats: JournalStats::new(),
        }
    }

    /// [`JournalWriter::new`] with `T` and `B` from the journal's settings.
    pub fn with_config(files: F, header: SegmentHeader, first_segment: u32, config: &JournalConfig) -> Self {
        JournalWriter::new(files, header, first_segment, config.commit_interval_ns, config.max_batch)
    }

    /// Records in the batch.
    pub fn batch_len(&self) -> usize {
        self.count
    }

    /// The batch holds `B` records: take no more before flushing.
    pub fn is_batch_full(&self) -> bool {
        self.count >= self.max_batch
    }

    /// Where the next record would go (before any segment switch).
    pub fn position(&self) -> JournalPosition {
        let header = if self.header_due { HEADER_BYTES as u64 } else { 0 };
        JournalPosition { segment: self.segment, offset: self.offset + self.batch.len() as u64 + header }
    }

    /// The flush rule ([`should_flush`]) for the current batch at run time `now`.
    pub fn flush_due(&self, now: u64, finished: bool) -> bool {
        should_flush(self.count, self.oldest_t_seq, now, self.commit_interval_ns, self.max_batch, finished)
    }

    /// Adds one record, given as the sequencer's words with the CRC field 0, to the batch.
    /// If it doesn't fit in the current segment, first flushes the batch so far (which can
    /// fail) and moves to the next segment.
    pub fn append(&mut self, words: &[u64], clock: &RunClock, durable: &Watermark) -> io::Result<()> {
        let len = (words.len() * 8) as u64;
        if self.position().offset + len > self.files.segment_bytes() {
            self.flush(clock, durable)?;
            self.switch_segment()?;
        }
        let (seq, ts) = (words[1], words[2]);
        if self.header_due {
            let header = SegmentHeader { segment: self.segment, first_seq: seq, ..self.header };
            self.batch.extend_from_slice(&header.encode());
            self.header_due = false;
        }
        append_record(&mut self.batch, words);
        if self.count == 0 {
            // "Sequenced at" is the record's own t_seq: its ts minus this life's anchor.
            self.oldest_t_seq = ts.saturating_sub(self.header.run_start_unix_ns);
        }
        self.count += 1;
        self.last_seq = seq;
        Ok(())
    }

    /// Makes the batch durable, one write and one sync, then publishes its last seq as the
    /// durable watermark (11.5, 11.7). Does nothing if the batch is empty. On an error the
    /// watermark doesn't move; the caller must stop (module docs, "Errors").
    pub fn flush(&mut self, clock: &RunClock, durable: &Watermark) -> io::Result<()> {
        if self.count == 0 {
            return Ok(());
        }
        let t0 = clock.now();
        self.files.write_at(self.segment, self.offset, &self.batch)?;
        let t1 = clock.now();
        self.files.sync_data(self.segment)?;
        let t2 = clock.now();
        self.offset += self.batch.len() as u64;
        durable.publish(self.last_seq);

        let stats = &mut self.stats;
        stats.flush_ns.record(t2.saturating_sub(t0));
        stats.fdatasync_ns.record(t2.saturating_sub(t1));
        stats.fdatasync_total_ns += t2.saturating_sub(t1);
        stats.batch_records.record(self.count as u64);
        stats.batch_bytes.record(self.batch.len() as u64);
        stats.flushes += 1;
        stats.records += self.count as u64;
        stats.bytes += self.batch.len() as u64;
        self.batch.clear();
        self.count = 0;
        Ok(())
    }

    /// Moves to the next segment (11.5): the old one was made durable by the flush just
    /// before; the new one is created if preallocation didn't make it, and its header goes
    /// in front of the next record.
    fn switch_segment(&mut self) -> io::Result<()> {
        self.segment += 1;
        if !self.files.exists(self.segment)? {
            self.files.create_segment(self.segment)?;
            self.stats.segments_created += 1;
        }
        self.offset = 0;
        self.header_due = true;
        Ok(())
    }

    /// Flushes so far, and the nanoseconds they spent in `fdatasync` (module docs, "Busy
    /// time").
    pub fn flushes_and_sync_ns(&self) -> (u64, u64) {
        (self.stats.flushes, self.stats.fdatasync_total_ns)
    }

    /// The files, for a test to inject a fault into them.
    pub fn files_mut(&mut self) -> &mut F {
        &mut self.files
    }

    /// The files and the statistics, once the writer is done (or, in the crash tests, has
    /// "died").
    pub fn into_parts(self) -> (F, JournalStats) {
        (self.files, self.stats)
    }
}

/// Writes one byte of every 4 KiB page of `buffer`'s capacity, with a value the compiler
/// can't see, so the first batches of a run take no first-touch page faults (15.4).
fn pre_touch(buffer: &mut Vec<u8>) {
    buffer.resize(buffer.capacity(), 0);
    for byte in buffer.iter_mut().step_by(4_096) {
        *byte = std::hint::black_box(1);
    }
    buffer.clear();
}

/// The journal writer's thread loop (11.5): take records until the batch is full or the
/// ring is empty, free their ring slots, flush if the rule says so, and stop once the ring
/// is closed and drained and the last batch is flushed. Publishes its busy time (without
/// the time blocked in `fdatasync`), that time, and its flushes (module docs, "Busy
/// time"). Panics on an I/O error, which aborts the process (module docs, "Errors").
pub fn run_writer<F: JournalFiles>(
    mut writer: JournalWriter<F>,
    mut ring: Consumer<3>,
    durable: &Watermark,
    clock: RunClock,
    idle: IdleStrategy,
    counters: &PipelineCounters,
) -> JournalStats {
    let mut words = [0u64; JournalRecord::SIGNED_WORDS];
    let mut busy = BusyMeter::new();
    loop {
        // A flush can come from `append` too (a segment switch), so the pass's time in
        // fdatasync is what the writer's total moved by.
        let (flushes_before, synced_before) = writer.flushes_and_sync_ns();
        let took_any = ring.available(1) > 0;
        if took_any {
            busy.begin(clock.now());
        }
        while !writer.is_batch_full() && ring.available(1) > 0 {
            let len = JournalRecord::len_of(ring.peek(0)) as usize / 8;
            ring.read(&mut words[..len]);
            writer.append(&words[..len], &clock, durable).unwrap_or_else(|error| io_failure(error));
        }
        ring.release(); // the records are in the batch now: give the sequencer the space
        let finished = ring.is_finished();
        let now = clock.now();
        let flushed = writer.flush_due(now, finished);
        if flushed {
            if !took_any {
                // A pass that only flushes (`T` after records taken earlier) is a busy
                // stretch of its own; the wait since the last record was idle (2.2).
                busy.begin(now);
            }
            writer.flush(&clock, durable).unwrap_or_else(|error| io_failure(error));
        }
        if took_any || flushed {
            let (flushes, synced) = writer.flushes_and_sync_ns();
            busy.end_except(clock.now(), synced - synced_before, &counters.journal.busy_ns);
            if flushes != flushes_before {
                counters.journal_sync_ns.store(synced);
                counters.journal_flushes.store(flushes);
            }
        } else if writer.batch_len() == 0 {
            if finished {
                break;
            }
            idle.idle();
        }
    }
    writer.into_parts().1
}

/// An I/O error on the journal: stop everything (11.7). The panic hook aborts the process.
fn io_failure(error: io::Error) -> ! {
    panic!("journal write or fdatasync failed: {error}; the process stops and recovery takes over (11.7)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::encode_command;
    use crate::journal::files::{DiscardFiles, SimDisk};
    use crate::journal::format::{HeaderRead, JournalIdentity, decode_record, record_crc, stored_crc};
    use crate::records::{InjectionMode, Meta, SIGNATURE_WORDS, Source};
    use engine::command::{Command, SetMark};
    use engine::engine::EngineOptions;
    use engine::types::{AccountId, MarketId, Price};

    const SEGMENT: u64 = 4_096;
    const ANCHOR: u64 = 1_790_000_000_000_000_000;

    fn header() -> SegmentHeader {
        let identity = JournalIdentity::new(1, InjectionMode::Signed, EngineOptions::default());
        SegmentHeader::for_life(identity, ANCHOR, [0; 32])
    }

    /// Record `seq`: kind 1 (152 bytes) when `signed`, else an operator mark (80 bytes),
    /// sequenced at run time `seq * 1_000`.
    fn record(seq: u64, signed: bool) -> Vec<u64> {
        let meta = if signed {
            Meta { source: Source::SignedClient, lane: 0, account: AccountId::new(9) }
        } else {
            Meta::OPERATOR
        };
        let command = if signed {
            let id = engine::types::order_id(AccountId::new(9), engine::types::OrderSeq::new(seq as u32));
            Command::CancelOrder(engine::command::CancelOrder { order_id: id, market: MarketId::new(1) })
        } else {
            Command::SetMark(SetMark { price: Price::new(seq as i64), market: MarketId::new(1) })
        };
        let record = JournalRecord {
            seq,
            ts: ANCHOR + seq * 1_000,
            meta,
            nonce: if signed { seq } else { 0 },
            command: encode_command(&command),
            expires_at: u64::MAX,
            signature: [seq; SIGNATURE_WORDS],
        };
        record.to_words()[..record.len_words()].to_vec()
    }

    fn new_disk() -> SimDisk {
        let mut disk = SimDisk::new(SEGMENT);
        disk.create_segment(0).expect("created");
        disk
    }

    #[test]
    fn the_flush_rule_at_each_boundary() {
        // Empty: never.
        assert!(!should_flush(0, 0, u64::MAX, 0, 4_096, true));
        // B records: at once.
        assert!(should_flush(4_096, 1_000, 1_000, 1_000_000, 4_096, false));
        assert!(!should_flush(4_095, 1_000, 1_000, 1_000_000, 4_096, false));
        assert!(should_flush(1, 1_000, 1_000, 1_000_000, 1, false), "B = 1: every record alone");
        // T after the oldest record's t_seq: not a nanosecond before.
        assert!(!should_flush(5, 1_000, 1_000_999, 1_000_000, 4_096, false));
        assert!(should_flush(5, 1_000, 1_001_000, 1_000_000, 4_096, false));
        // T = 0: whenever there is anything.
        assert!(should_flush(1, 1_000, 1_000, 0, 4_096, false));
        // A clock read below the oldest stamp (another CPU): not a flush, and no underflow.
        assert!(!should_flush(1, 2_000, 1_000, 1, 4_096, false));
        // The ring is closed and drained: flush what there is.
        assert!(should_flush(1, 1_000, 1_000, 1_000_000, 4_096, true));
    }

    #[test]
    fn segments_needed_follows_the_formula_of_11_6() {
        assert_eq!(segments_needed(6_500_000, 152, DEFAULT_SEGMENT_BYTES), 3, "100k signed/s for 65 s");
        assert_eq!(segments_needed(70_000_000, 80, DEFAULT_SEGMENT_BYTES), 8, "2M pre-verified/s for 35 s");
        assert_eq!(segments_needed(0, 152, 4_096), 1);
    }

    #[test]
    fn the_config_refuses_a_batch_above_4096_and_a_segment_too_small() {
        let mut config = JournalConfig::new("/tmp/j".into());
        assert!(config.check().is_ok());
        config.max_batch = 4_097;
        assert!(config.check().is_err());
        config.max_batch = 0;
        assert!(config.check().is_err());
        config.max_batch = 1;
        config.segment_bytes = 128 + 144;
        assert!(config.check().is_err());
        config.segment_bytes = 4_096;
        assert!(config.check().is_ok());
    }

    #[test]
    fn a_flush_makes_the_batch_durable_and_then_moves_the_watermark() {
        let clock = RunClock::start();
        let durable = Watermark::new(0);
        let mut writer = JournalWriter::new(new_disk(), header(), 0, 1_000_000, 4_096);
        writer.append(&record(1, true), &clock, &durable).expect("appended");
        writer.append(&record(2, false), &clock, &durable).expect("appended");
        assert_eq!((writer.batch_len(), durable.load()), (2, 0), "nothing durable before the flush");
        assert!(writer.flush_due(1_000 + 1_000_000, false), "T after record 1's t_seq");
        assert!(!writer.flush_due(1_000 + 999_999, false));
        writer.flush(&clock, &durable).expect("flushed");
        assert_eq!((writer.batch_len(), durable.load()), (0, 2));
        assert_eq!(writer.position(), JournalPosition { segment: 0, offset: 128 + 152 + 80 });

        let (disk, stats) = writer.into_parts();
        let bytes = disk.durable(0).expect("exists");
        match SegmentHeader::decode(bytes[..128].try_into().expect("128 bytes")) {
            HeaderRead::Valid(h) => assert_eq!((h.segment, h.first_seq, h.run_start_unix_ns), (0, 1, ANCHOR)),
            other => panic!("{other:?}"),
        }
        let first = &bytes[128..280];
        assert_eq!(stored_crc(first), record_crc(first));
        assert_eq!(decode_record(first).expect("a record").seq, 1);
        assert_eq!(decode_record(&bytes[280..360]).expect("a record").seq, 2);
        assert!(bytes[360..].iter().all(|&b| b == 0));
        assert_eq!((stats.flushes, stats.records, stats.bytes), (1, 2, 360));
        assert_eq!(stats.batch_records.max(), Some(2));
    }

    #[test]
    fn a_record_that_does_not_fit_flushes_the_batch_and_starts_the_next_segment() {
        let clock = RunClock::start();
        let durable = Watermark::new(0);
        let mut disk = new_disk();
        disk.create_segment(1).expect("preallocated");
        let mut writer = JournalWriter::new(disk, header(), 0, 1_000_000, 4_096);
        // (4,096 - 128) / 152 = 26 signed records fit in a segment, with 16 bytes left.
        for seq in 1..=26 {
            writer.append(&record(seq, true), &clock, &durable).expect("appended");
        }
        assert_eq!(durable.load(), 0);
        writer.append(&record(27, true), &clock, &durable).expect("appended");
        assert_eq!(durable.load(), 26, "the first segment's batch was flushed before switching");
        assert_eq!((writer.batch_len(), writer.position().segment), (1, 1));
        writer.flush(&clock, &durable).expect("flushed");
        let (disk, stats) = writer.into_parts();
        assert_eq!(stats.segments_created, 0, "segment 1 was preallocated");
        let first = disk.durable(0).expect("exists");
        assert!(first[128 + 26 * 152..].iter().all(|&b| b == 0), "the rest of segment 0 stays zero");
        let second = disk.durable(1).expect("exists");
        match SegmentHeader::decode(second[..128].try_into().expect("128 bytes")) {
            HeaderRead::Valid(h) => assert_eq!((h.segment, h.first_seq), (1, 27)),
            other => panic!("{other:?}"),
        }
        assert_eq!(decode_record(&second[128..280]).expect("a record").seq, 27);
    }

    #[test]
    fn the_writer_creates_a_segment_that_preallocation_did_not() {
        let clock = RunClock::start();
        let durable = Watermark::new(0);
        let mut writer = JournalWriter::new(new_disk(), header(), 0, 0, 4_096);
        for seq in 1..=60 {
            writer.append(&record(seq, seq % 2 == 0), &clock, &durable).expect("appended");
            writer.flush(&clock, &durable).expect("flushed");
        }
        assert_eq!(durable.load(), 60);
        let (mut disk, stats) = writer.into_parts();
        assert_eq!(stats.segments_created, 1);
        assert_eq!(disk.segments().expect("listed"), [0, 1]);
        assert_eq!(stats.flushes, 60);
    }

    #[test]
    fn preallocate_creates_only_what_is_missing() {
        let mut disk = new_disk();
        assert_eq!(preallocate(&mut disk, 0, 3).expect("created"), 2);
        assert_eq!(preallocate(&mut disk, 0, 3).expect("nothing to do"), 0);
        assert_eq!(disk.segments().expect("listed"), [0, 1, 2]);
    }

    #[test]
    fn discard_mode_runs_the_same_logic_and_publishes_the_watermark() {
        let clock = RunClock::start();
        let durable = Watermark::new(0);
        let mut writer = JournalWriter::new(DiscardFiles::new(SEGMENT), header(), 0, 1_000_000, 2);
        for seq in 1..=100 {
            writer.append(&record(seq, true), &clock, &durable).expect("appended");
            if writer.is_batch_full() {
                writer.flush(&clock, &durable).expect("flushed");
            }
        }
        assert_eq!(durable.load(), 100);
        assert_eq!(writer.position().segment, 3, "segment switches happen as on disk");
        assert_eq!(writer.into_parts().1.flushes, 50);
    }

    #[test]
    fn a_pass_that_only_flushes_counts_only_itself_as_busy() {
        // One record, flushed T = 30 ms after it was sequenced by a pass that takes
        // nothing: the wait for T is idle time, not busy time (2.2).
        let clock = RunClock::start();
        let header = SegmentHeader { run_start_unix_ns: clock.start_unix_ns(), ..header() };
        let writer = JournalWriter::new(DiscardFiles::new(SEGMENT), header, 0, 30_000_000, 4_096);
        let (mut producer, consumer) = crate::ring::channel::<3>(4);
        let (durable, counters) = (Watermark::new(0), PipelineCounters::new());
        let mut words = record(1, false);
        words[2] = header.run_start_unix_ns + clock.now(); // `ts`: sequenced now
        producer.write(&words);
        producer.publish();
        let idle = IdleStrategy::SpinThenYield { spins: 64 };
        let stats = std::thread::scope(|scope| {
            let writer = scope.spawn(|| run_writer(writer, consumer, &durable, clock, idle, &counters));
            while durable.load() < 1 {
                std::thread::yield_now();
            }
            drop(producer); // closes the ring: the writer stops
            writer.join().expect("the writer ends")
        });
        assert_eq!(stats.flushes, 1);
        let busy_ns = counters.journal.busy_ns.load();
        assert!(busy_ns < 10_000_000, "{busy_ns} ns busy: the wait for T was counted as busy");
    }

    /// Discard mode's files, but every sync takes 30 ms: a slow disk.
    struct SlowSync(DiscardFiles);

    impl JournalFiles for SlowSync {
        fn segment_bytes(&self) -> u64 {
            self.0.segment_bytes()
        }
        fn segments(&mut self) -> io::Result<Vec<u32>> {
            self.0.segments()
        }
        fn exists(&mut self, segment: u32) -> io::Result<bool> {
            self.0.exists(segment)
        }
        fn read_at(&mut self, segment: u32, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            self.0.read_at(segment, offset, buf)
        }
        fn write_at(&mut self, segment: u32, offset: u64, bytes: &[u8]) -> io::Result<()> {
            self.0.write_at(segment, offset, bytes)
        }
        fn sync_data(&mut self, _segment: u32) -> io::Result<()> {
            std::thread::sleep(std::time::Duration::from_millis(30));
            Ok(())
        }
        fn create_segment(&mut self, segment: u32) -> io::Result<()> {
            self.0.create_segment(segment)
        }
        fn sync_dir(&mut self) -> io::Result<()> {
            self.0.sync_dir()
        }
        fn write_side_file(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
            self.0.write_side_file(name, bytes)
        }
    }

    #[test]
    fn time_blocked_in_fdatasync_is_published_apart_and_not_counted_as_busy() {
        let clock = RunClock::start();
        let header = SegmentHeader { run_start_unix_ns: clock.start_unix_ns(), ..header() };
        let writer = JournalWriter::new(SlowSync(DiscardFiles::new(SEGMENT)), header, 0, 0, 4_096);
        let (mut producer, consumer) = crate::ring::channel::<3>(4);
        let (durable, counters) = (Watermark::new(0), PipelineCounters::new());
        producer.write(&record(1, false));
        producer.publish();
        let idle = IdleStrategy::SpinThenYield { spins: 64 };
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| run_writer(writer, consumer, &durable, clock, idle, &counters));
            while durable.load() < 1 {
                std::thread::yield_now();
            }
            drop(producer);
            writer.join().expect("the writer ends")
        });
        let (busy_ns, sync_ns) = (counters.journal.busy_ns.load(), counters.journal_sync_ns.load());
        assert_eq!(counters.journal_flushes.load(), 1);
        assert!(sync_ns >= 30_000_000, "{sync_ns} ns in fdatasync");
        assert!(busy_ns < 10_000_000, "{busy_ns} ns busy: the 30 ms in fdatasync were counted as busy");
    }

    #[test]
    fn a_failed_sync_leaves_the_watermark_where_it_was() {
        let clock = RunClock::start();
        let durable = Watermark::new(0);
        let mut disk = new_disk();
        disk.inject(crate::journal::files::Fault::FailSync);
        let mut writer = JournalWriter::new(disk, header(), 0, 0, 4_096);
        writer.append(&record(1, false), &clock, &durable).expect("appended");
        assert!(writer.flush(&clock, &durable).is_err());
        assert_eq!(durable.load(), 0);
    }
}
