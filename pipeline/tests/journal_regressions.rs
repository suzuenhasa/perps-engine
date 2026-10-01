//! Named regression tests for the journal: recovery (`docs/PIPELINE.md` 11.8), the torn-tail
//! copies it keeps, and a fresh start over a directory a crash left behind (13.5). Each
//! test pins a review finding of Milestone 3 that was fixed (PIPELINE.md 22), so that it
//! can't come back. The random crash sequences are in `crash_recovery.rs`.
//!
//! The segments here are 1 MiB, larger than the tail region `W` = 622,720 bytes, as the
//! production 1 GiB ones are: with the 4 KiB segments of the unit tests the tail region
//! covers whole segments, so the rules about bytes past `W` never come into play (finding
//! F-TEST-W-BOUND).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use engine::command::{CancelOrder, Command, SetMark};
use engine::engine::EngineOptions;
use engine::types::order_id;

use pipeline::affinity::CpuLayout;
use pipeline::clock::RunClock;
use pipeline::codec::encode_command;
use pipeline::counters::Watermark;
use pipeline::gate::Phases;
use pipeline::idle::IdleStrategy;
use pipeline::journal::files::{JournalFiles, SimDisk, StdFiles};
use pipeline::journal::format::{JournalIdentity, SegmentHeader, TAIL_REGION_BYTES, append_record};
use pipeline::journal::recovery::{Recovered, recover, recover_files};
use pipeline::journal::writer::{JournalConfig, JournalMode, JournalWriter};
use pipeline::journal::{JournalError, JournalPosition};
use pipeline::records::{
    AuthScheme, InjectionMode, JournalRecord, Meta, OperatorRecord, SIGNATURE_WORDS, Source, Stamps,
};
use pipeline::ring::channel;
use pipeline::run::{Pipeline, PipelineConfig, RingCapacities};
use pipeline::sequencer::Inputs;

const IDLE: IdleStrategy = IdleStrategy::SpinThenYield { spins: 64 };
const ANCHOR: u64 = 1_790_000_000_000_000_000;
/// Larger than the tail region, like production's segments (module docs).
const SEGMENT: u64 = 1 << 20;
/// Signed (152-byte) records that fit in one segment: 6,897.
const PER_SEGMENT: u64 = (SEGMENT - 128) / 152;
const W: u64 = TAIL_REGION_BYTES;

fn signed_identity() -> JournalIdentity {
    JournalIdentity::new(1, InjectionMode::Signed, EngineOptions::default())
}

/// Record `seq` of a signed journal: a signed cancel of account 9 (152 bytes).
fn signed_record(seq: u64, anchor: u64) -> JournalRecord {
    let command = Command::CancelOrder(CancelOrder { order_id: order_id(9, seq as u32), market: 1 });
    JournalRecord {
        seq,
        ts: anchor + seq * 1_000,
        meta: Meta { source: Source::SignedClient, lane: 1, account: 9 },
        nonce: seq,
        command: encode_command(&command),
        expires_at: u64::MAX,
        signature: [u64::MAX - seq; SIGNATURE_WORDS],
    }
}

/// Record `seq` of an operator mark (80 bytes), as in any journal.
fn operator_record(seq: u64, ts: u64) -> JournalRecord {
    JournalRecord {
        seq,
        ts,
        meta: Meta::OPERATOR,
        nonce: 0,
        command: encode_command(&Command::SetMark(SetMark { price: seq as i64, market: 1 })),
        expires_at: 0,
        signature: [0; SIGNATURE_WORDS],
    }
}

fn words(record: &JournalRecord) -> Vec<u64> {
    record.to_words()[..record.len_words()].to_vec()
}

/// A simulated disk of `segment_bytes` segments holding signed records 1..=records from
/// segment 0 on, written by the real writer in synced batches of at most `batch`: every one
/// of them could have been released.
fn synced_journal(segment_bytes: u64, records: u64, batch: u64) -> SimDisk {
    let mut disk = SimDisk::new(segment_bytes);
    disk.create_segment(0).expect("created");
    let header = SegmentHeader::for_life(signed_identity(), ANCHOR, [1; 32]);
    let (clock, durable) = (RunClock::start(), Watermark::new(0));
    let mut writer = JournalWriter::new(disk, header, 0, u64::MAX, 4_096);
    for seq in 1..=records {
        if (seq - 1) % batch == 0 {
            writer.flush(&clock, &durable).expect("flushed");
        }
        writer.append(&words(&signed_record(seq, ANCHOR)), &clock, &durable).expect("appended");
    }
    writer.flush(&clock, &durable).expect("flushed");
    writer.into_parts().0
}

fn recover_signed(disk: &mut SimDisk) -> Result<Recovered, JournalError> {
    recover_files(disk, &signed_identity(), false)
}

/// Every segment's cached and durable bytes, and the side files.
fn image(disk: &SimDisk) -> Vec<Vec<u8>> {
    let mut parts = Vec::new();
    for segment in disk.clone().segments().expect("listed") {
        parts.push(disk.cached(segment).expect("exists").to_vec());
        parts.push(disk.durable(segment).expect("exists").to_vec());
    }
    for name in disk.side_file_names() {
        parts.push(disk.side_file(&name).expect("exists").to_vec());
    }
    parts
}

/// Writes byte `value` at `offset` of `segment` and syncs it.
fn poke(disk: &mut SimDisk, segment: u32, offset: u64, value: u8) {
    disk.write_at(segment, offset, &[value]).expect("written");
    disk.sync_data(segment).expect("synced");
}

// F-TAIL-UNION: bit rot in synced data at the end of a segment, while the journal went on
// in the next one, was cut silently as a "torn tail" spanning both segments.
#[test]
fn a_flipped_bit_in_synced_data_before_a_later_segment_is_an_error_not_a_torn_tail() {
    // Segment 0 full, segment 1 holding 1,103 records (167,784 bytes, under W); one bit rots
    // in record 6,001, in synced data within segment 0's last W bytes.
    let mut disk = synced_journal(SEGMENT, PER_SEGMENT + 1_103, 1_000);
    let clean = recover_signed(&mut disk.clone()).expect("a clean journal");
    assert_eq!((clean.records, clean.end.segment), (PER_SEGMENT + 1_103, 1));
    let damaged_at = 128 + 152 * 6_000 + 40;
    disk.corrupt(0, damaged_at, 0x04);
    let before = image(&disk);
    let result = recover_signed(&mut disk);
    assert!(
        matches!(result, Err(JournalError::Corrupt { at: JournalPosition { segment: 0, .. }, .. })),
        "a flipped bit in synced data must stop recovery, naming segment 0; got {result:?}"
    );
    assert!(image(&disk) == before, "recovery changed the disk although it must stop");
}

// F-TEST-W-BOUND: nothing tested that a nonzero byte W or more past the end of the journal,
// in its own segment, is an error while one just inside is a torn tail.
#[test]
fn a_nonzero_byte_w_past_the_end_is_an_error_and_one_just_inside_is_a_torn_tail() {
    let disk = synced_journal(SEGMENT, 1_000, 1_000);
    let end = recover_signed(&mut disk.clone()).expect("clean").end;
    assert_eq!(end, JournalPosition { segment: 0, offset: 128 + 152_000 });
    assert!(end.offset + W < SEGMENT, "the end's segment goes on past the tail region");

    let mut inside = disk.clone();
    poke(&mut inside, 0, end.offset + W - 1, 0x5A);
    let recovered = recover_signed(&mut inside).expect("the last byte of the tail region");
    assert_eq!((recovered.records, recovered.end), (1_000, end));
    assert_eq!(inside.durable(0).expect("exists")[(end.offset + W - 1) as usize], 0, "zeroed");
    assert_eq!(recovered.torn_copies, [format!("torn-000000-{}.bin", end.offset)]);

    let mut outside = disk.clone();
    poke(&mut outside, 0, end.offset + W, 0x5A);
    let before = image(&outside);
    match recover_signed(&mut outside) {
        Err(JournalError::Corrupt { at, .. }) => {
            assert_eq!(at, JournalPosition { segment: 0, offset: end.offset + W });
        }
        other => panic!("a byte W past the end must be an error, got {other:?}"),
    }
    assert!(image(&outside) == before, "nothing changed");
}

// F-TEST-W-BOUND: the same bound in the next segment, where a new segment's first flush
// could have left at most its first W bytes.
#[test]
fn in_the_next_segment_a_nonzero_byte_at_w_is_an_error_and_one_before_it_is_a_torn_tail() {
    let mut disk = synced_journal(SEGMENT, 1_000, 1_000);
    disk.create_segment(1).expect("created");
    let end = recover_signed(&mut disk.clone()).expect("clean").end;

    let mut inside = disk.clone();
    poke(&mut inside, 1, W - 1, 0x5A);
    let recovered = recover_signed(&mut inside).expect("inside the next segment's first W bytes");
    assert_eq!((recovered.records, recovered.end), (1_000, end));
    assert!(inside.durable(1).expect("exists").iter().all(|&b| b == 0), "zeroed");
    assert_eq!(recovered.torn_copies, ["torn-000001-0.bin"]);

    let mut outside = disk.clone();
    poke(&mut outside, 1, W, 0x5A);
    let before = image(&outside);
    match recover_signed(&mut outside) {
        Err(JournalError::Corrupt { at, .. }) => assert_eq!(at, JournalPosition { segment: 1, offset: W }),
        other => panic!("a byte at W of the next segment must be an error, got {other:?}"),
    }
    assert!(image(&outside) == before, "nothing changed");
}

// F-TORN-COPY-OVERWRITE: a second life torn at the same place overwrote the first life's
// torn-<segment>-<offset>.bin, the only trace of it.
#[test]
fn a_second_life_torn_at_the_same_place_keeps_the_first_torn_copy() {
    let mut disk = synced_journal(4_096, 10, 10);
    let first = recover_signed(&mut disk).expect("a clean journal");
    assert_eq!((first.records, first.end.next_life_segment()), (10, 1));

    // A life whose first flush (header and records 11 to 15, in segment 1) was torn: the
    // header's 512-byte sector never reached the disk, the rest did.
    let torn_first_flush = |disk: &mut SimDisk, anchor: u64| {
        if !disk.exists(1).expect("checked") {
            disk.create_segment(1).expect("created");
        }
        let header = SegmentHeader::for_life(signed_identity(), anchor, [2; 32]);
        let mut bytes = SegmentHeader { segment: 1, first_seq: 11, ..header }.encode().to_vec();
        for seq in 11..=15 {
            append_record(&mut bytes, &words(&signed_record(seq, anchor)));
        }
        bytes[..512].fill(0);
        disk.write_at(1, 0, &bytes).expect("written");
        disk.sync_data(1).expect("synced");
    };

    torn_first_flush(&mut disk, ANCHOR + 1_000_000_000);
    let life_1 = recover_signed(&mut disk).expect("a torn tail");
    assert_eq!(life_1.torn_copies, ["torn-000001-0.bin"]);
    let copy_1 = disk.side_file("torn-000001-0.bin").expect("copied").to_vec();
    assert_eq!(life_1.end.next_life_segment(), 1, "the next life reuses segment 1");

    torn_first_flush(&mut disk, ANCHOR + 2_000_000_000);
    let life_2 = recover_signed(&mut disk).expect("a torn tail");
    assert_eq!(
        (life_2.records, life_2.torn_copies.as_slice()),
        (10, ["torn-000001-0-2.bin".to_string()].as_slice())
    );
    assert_eq!(disk.side_file("torn-000001-0.bin"), Some(&copy_1[..]), "life 1's copy is intact");
    assert_ne!(disk.side_file("torn-000001-0-2.bin"), Some(&copy_1[..]), "life 2's is its own");
}

// F-FRESH-START: a fresh start looked only at segment 0's header, so over a first flush that
// lost its header's sector it wrote new records in front of the old ones, and a later
// recovery read on into them.
//
// `Pipeline::start` installs the process-wide abort-on-panic hook (PIPELINE.md 2.8), which
// would abort the other tests of this binary if they failed, so this one runs in a child
// process.
#[test]
fn a_fresh_start_over_a_lost_header_sector_zeroes_the_stale_body() {
    const CHILD: &str = "JOURNAL_REGRESSIONS_FRESH_START_CHILD";
    if std::env::var_os(CHILD).is_some() {
        fresh_start_over_a_lost_header_sector();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["a_fresh_start_over_a_lost_header_sector_zeroes_the_stale_body", "--exact", "--nocapture"])
        .args(["--test-threads=1"])
        .env(CHILD, "1")
        .env("RUST_BACKTRACE", "0")
        .output()
        .expect("the test binary runs");
    assert!(
        output.status.success(),
        "the child failed ({}):\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fresh_start_over_a_lost_header_sector() {
    const SMALL_SEGMENT: u64 = 64 << 10;
    let dir = run_dir("fresh-start");
    let journal_dir = dir.join("journal");
    let config = PipelineConfig {
        deployment: 7,
        mode: InjectionMode::PreVerified,
        auth: AuthScheme::Perp,
        engine: EngineOptions::default(),
        lanes: 1,
        capacities: RingCapacities { journal: 1_024, core: 256, event: 512 },
        journal: JournalConfig {
            segment_bytes: SMALL_SEGMENT,
            commit_interval_ns: 0,
            mode: JournalMode::Disk,
            preallocate: 2,
            ..JournalConfig::new(journal_dir.clone())
        },
        registry_digest: [0; 32],
        layout: CpuLayout::unpinned(),
        idle: IDLE,
        capture: None,
        release_log: None,
        stamps: Stamps::On,
        phases: Phases::everything(),
        ablation_verify_on_core: None,
    };
    let identity = config.identity();

    // Life A, a minute ago: one flush of the header and records 1 to 50, then the power
    // fails and only the header's sector is lost. Nothing of it was released.
    let mut files = StdFiles::open(&journal_dir, SMALL_SEGMENT).expect("opened");
    files.create_segment(0).expect("created");
    let anchor_a = RunClock::start().start_unix_ns() - 60_000_000_000;
    let header = SegmentHeader::for_life(identity, anchor_a, [0; 32]);
    let (clock_a, durable_a) = (RunClock::start(), Watermark::new(0));
    let mut writer = JournalWriter::new(files, header, 0, u64::MAX, 4_096);
    for seq in 1..=50 {
        let record = operator_record(seq, anchor_a + seq * 1_000);
        writer.append(&words(&record), &clock_a, &durable_a).expect("appended");
    }
    writer.flush(&clock_a, &durable_a).expect("flushed");
    let mut files = writer.into_parts().0;
    files.write_at(0, 0, &[0; 512]).expect("written");
    files.sync_data(0).expect("synced");
    drop(files);

    // Life B: the same command, rerun. A fresh start in the same directory.
    let (mut operator, operator_end) = channel::<1>(256);
    let (lane, lane_end) = channel::<3>(256);
    let inputs = Inputs { lanes: vec![lane_end], operator: operator_end };
    let clock = RunClock::start();
    let pipeline = Pipeline::start(config, inputs, clock, None).expect("a torn first flush is not a journal");
    const LIFE_B: u64 = 20;
    for i in 0..LIFE_B {
        let command = encode_command(&Command::SetMark(SetMark { price: 1_000 + i as i64, market: 1 }));
        let record = OperatorRecord { command, t_sched: clock.now(), t_sent: clock.now() };
        while operator.free(1) == 0 {
            IDLE.idle();
        }
        operator.write(&record.to_words());
        operator.publish();
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while pipeline.released() < LIFE_B {
        assert!(Instant::now() < deadline, "life B released only {} commands", pipeline.released());
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(operator);
    drop(lane);
    pipeline.join();

    // Life B's journal is life B's 20 records and nothing else, and life A's body was
    // copied aside before it was zeroed.
    let recovered = recover(&journal_dir, &identity, false);
    assert!(
        matches!(&recovered, Ok(r) if r.records == LIFE_B),
        "after a fresh life of {LIFE_B} released records over life A's stale body, recovery returned \
         {recovered:?}"
    );
    assert!(journal_dir.join("torn-000000-0.bin").is_file(), "life A's body, kept for inspection");
    std::fs::remove_dir_all(&dir).expect("cleaned up");
}

/// A fresh directory for one test.
fn run_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pipeline-journal-regressions-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}
