//! Crash recovery (`docs/PIPELINE.md` 11.8; `docs/DECISIONS.md` D-030): run once at start,
//! before anything reads the journal. It finds the journal's end, makes sure nothing after
//! the end can ever be mistaken for data, and makes everything before the end durable. It
//! either succeeds, or stops with an error and changes nothing.
//!
//! **What a crash can leave.** The writer syncs each batch before it writes the next, so at
//! most one flush is unfinished at any moment: at most `B` = 4,096 records of at most 152
//! bytes, plus a segment header if the batch started a new segment. After a power loss,
//! any subset of that flush's 512-byte sectors may be on the disk; after a process crash,
//! the page cache still holds everything the writer wrote. Everything before the unfinished
//! flush is on disk.
//!
//! **Step 1: read the journal and find its end** ([`scan_journal`]). Walk segments 0, 1, 2,
//! ... while they exist. An all-zero or torn header ends the journal; a header with a valid
//! CRC must carry segment 0's identity, its own index and the next seq, or it is an error.
//! Then records from offset 128: a `len` of 0 ends the segment's data (the next segment's
//! header must continue the sequence); a `len` that is not 80 or 152, a record that would
//! cross the segment's end, or a CRC that fails ends the journal. **The CRC decides whether
//! some bytes are a record at all; once they are, everything about them must be right**
//! (the next seq, a kind that matches the length and the journal's mode, a tag that fits
//! the kind, the owner, zero reserved fields, a CMD40 that decodes, a `ts` above the
//! previous one), or recovery stops with an error. A valid record that doesn't fit is a
//! bug, tampering or data from elsewhere, and quietly ending the journal there would delete
//! results clients have already seen.
//!
//! **Step 2: what follows the end can only be a torn tail** ([`check_tail`]). The
//! unfinished flush started at or before the end and wrote at most `W` = 622,720 bytes, all
//! in one segment: the end's, or the next one if that flush started a new segment. So every
//! nonzero byte after the end, in every segment, must lie in the **tail region**: the `W`
//! bytes after the end in its segment, or the first `W` bytes of the next segment. And
//! since the writer syncs a segment's last batch before it writes anything into the next,
//! the nonzero bytes may lie in one of those two parts, never in both. Anything else (bit
//! rot in synced data, a later segment with a valid header, nonzero bytes in both parts) is
//! an error that names the first such byte, and nothing is changed: a person decides.
//!
//! **Step 3: make the disk match the recovered journal** ([`make_durable`]): copy the tail
//! region's nonzero bytes to `torn-<segment>-<offset>.bin` (with `-2`, `-3`, ... added if
//! an earlier recovery already used that name: a copy is never overwritten); write zeros
//! over the tail region;
//! re-write the kept part of the end's segment (its header and records), so that bytes a
//! failed `fdatasync` left readable in the page cache but never wrote become dirty again
//! and really reach the disk (11.7); then sync every segment touched and the directory.
//!
//! **Why this is enough** (the interview version):
//! - every released record was synced before it was released, so it lies before the
//!   unfinished flush, hence before the end; step 3 syncs it once more;
//! - nothing from an earlier life comes back: every life starts with zeros on disk after the
//!   end and writes forward (in a new segment), so after a crash the only nonzero bytes
//!   after the durable data are that life's own unfinished flush, which the next recovery
//!   zeroes. (The draft zeroed only the rest of the end's segment and later headers; after a
//!   crash that lost a new segment's header but kept part of its body, the next life reused
//!   that segment at the same offsets, and a second crash let recovery read on into the old
//!   records: same seqs, valid CRCs. Review finding F1.)
//! - a mismatch never truncates silently.
//!
//! **Complexity.** Step 1 and step 2 read every segment once (about a second per GB); step
//! 3 writes the tail region and at most one segment.

use std::io::ErrorKind;
use std::path::Path;

use engine::command::Command;

use super::files::{JournalFiles, StdFiles};
use super::format::{
    DEFAULT_SEGMENT_BYTES, HEADER_BYTES, HeaderRead, JournalIdentity, SegmentHeader, TAIL_REGION_BYTES,
    build_commit, check_contents, decode_record, record_crc, record_len, stored_crc,
};
use super::{JournalError, JournalPosition, check_identity};
use crate::records::{InjectionMode, JournalRecord};

/// How much is read from a segment at a time.
const READ_CHUNK_BYTES: u64 = 8 << 20;

/// What [`recover`] found, after making it durable (PIPELINE.md 19.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovered {
    /// Segment 0's identity; the configured one for an empty journal.
    pub identity: JournalIdentity,
    /// Just after the last valid record ([`JournalPosition::next_life_segment`] says where
    /// the next life starts).
    pub end: JournalPosition,
    /// Valid records.
    pub records: u64,
    /// The seq the next record gets: `records + 1`.
    pub next_seq: u64,
    /// The last record's `ts`, 0 if there is none: the next life's clock is anchored above
    /// it (9.2).
    pub last_ts: u64,
    /// The key registry digest of each segment's header, in segment order (13.4).
    pub registry_digests: Vec<[u8; 32]>,
    /// Every valid segment header, in segment order: each life's clock anchor, commit and
    /// build profile.
    pub headers: Vec<SegmentHeader>,
    /// The side files the torn tail was copied to (step 3), if there was one.
    pub torn_copies: Vec<String>,
}

impl Recovered {
    /// Warnings for segments written by a binary with another commit or build profile than
    /// the running one (11.2): recorded, printed, not enforced.
    pub fn build_warnings(&self) -> Vec<String> {
        let (commit, release) = (build_commit(), !cfg!(debug_assertions));
        let profile = |release: bool| if release { "release" } else { "debug" };
        self.headers
            .iter()
            .filter(|header| header.commit != commit || header.release_build != release)
            .map(|header| {
                format!(
                    "segment {} was written by commit {:?} ({}); this binary is commit {:?} ({})",
                    header.segment,
                    String::from_utf8_lossy(&header.commit).trim_end_matches('\0'),
                    profile(header.release_build),
                    String::from_utf8_lossy(&commit).trim_end_matches('\0'),
                    profile(release),
                )
            })
            .collect()
    }
}

/// Recovers the journal in `dir` (11.8): checks it against `expect`, the configured
/// identity (every field, and `engine_semantics` too unless `allow_engine_change`), zeroes
/// the torn tail, re-writes and syncs what it keeps. Changes nothing if it returns an error
/// from steps 1 and 2 or the identity check; if step 3 itself fails, the process must not
/// start (11.7's operating rule).
pub fn recover(
    dir: &Path,
    expect: &JournalIdentity,
    allow_engine_change: bool,
) -> Result<Recovered, JournalError> {
    let mut files = StdFiles::open_existing(dir, DEFAULT_SEGMENT_BYTES)
        .map_err(|e| JournalError::io(format!("opening the journal in {}", dir.display()), e))?;
    recover_files(&mut files, expect, allow_engine_change)
}

/// [`recover`] on any [`JournalFiles`]: the crash tests run it on a simulated disk.
pub fn recover_files(
    files: &mut impl JournalFiles,
    expect: &JournalIdentity,
    allow_engine_change: bool,
) -> Result<Recovered, JournalError> {
    let scan = scan_journal(files, |_, _, _| {})?;
    let identity = scan.identity.unwrap_or(*expect);
    check_identity(&identity, expect, allow_engine_change)?;
    let tail = check_tail(files, scan.end)?;
    let torn_copies = make_durable(files, scan.end, &tail)?;
    Ok(Recovered {
        identity,
        end: scan.end,
        records: scan.records,
        next_seq: scan.next_seq,
        last_ts: scan.last_ts,
        registry_digests: scan.headers.iter().map(|header| header.registry_digest).collect(),
        headers: scan.headers,
        torn_copies,
    })
}

// ---------------------------------------------------------------------------------------
// Step 1.

/// What reading the journal found (step 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalScan {
    /// Segment 0's identity; `None` if the journal has no valid header.
    pub identity: Option<JournalIdentity>,
    pub headers: Vec<SegmentHeader>,
    pub end: JournalPosition,
    pub records: u64,
    pub next_seq: u64,
    pub last_ts: u64,
}

/// Step 1 (module docs): reads the journal from the start to its end, checking every rule,
/// and calls `visit(segment, record, command)` for each valid record in order. Replay and
/// the signature audit read the journal through this too, after recovery. Changes nothing.
pub fn scan_journal(
    files: &mut impl JournalFiles,
    mut visit: impl FnMut(u32, &JournalRecord, &Command),
) -> Result<JournalScan, JournalError> {
    let segment_bytes = files.segment_bytes();
    let mut scan = JournalScan {
        identity: None,
        headers: Vec::new(),
        end: JournalPosition { segment: 0, offset: 0 },
        records: 0,
        next_seq: 1,
        last_ts: 0,
    };
    'segments: for segment in 0.. {
        if !files.exists(segment).map_err(|e| read_error(segment, e))? {
            break;
        }
        let mut reader = SegmentReader::new(segment);
        let header_bytes = reader.get(files, 0, HEADER_BYTES as u64)?;
        let header = match SegmentHeader::decode(header_bytes.try_into().expect("128 bytes")) {
            HeaderRead::Unused | HeaderRead::Torn => break, // the journal ends at `end`
            HeaderRead::Invalid(why) => {
                return Err(JournalError::corrupt(segment, 0, format!("header: {why}")));
            }
            HeaderRead::Valid(header) => header,
        };
        check_header(&scan, &header, segment)?;
        let mode = scan.identity.get_or_insert(header.identity).mode;
        scan.headers.push(header);
        let mut offset = HEADER_BYTES as u64;
        scan.end = JournalPosition { segment, offset };
        loop {
            if offset + 8 > segment_bytes {
                break; // the end of this segment: go on to the next
            }
            let len = u64::from(record_len(reader.get(files, offset, 8)?));
            if len == 0 {
                break; // the end of this segment's data
            }
            let is_record_length = len == u64::from(JournalRecord::UNSIGNED_BYTES)
                || len == u64::from(JournalRecord::SIGNED_BYTES);
            if !is_record_length || offset + len > segment_bytes {
                break 'segments; // these bytes are not a record: the journal ends at `end`
            }
            let bytes = reader.get(files, offset, len)?;
            if stored_crc(bytes) != record_crc(bytes) {
                break 'segments; // a torn write
            }
            // The CRC matched: this is a record, and everything about it must be right.
            let (record, command) = check_record(bytes, &scan, mode).map_err(|why| {
                JournalError::corrupt(segment, offset, format!("seq {}: {why}", scan.next_seq))
            })?;
            visit(segment, &record, &command);
            scan.records += 1;
            scan.next_seq += 1;
            scan.last_ts = record.ts;
            offset += len;
            scan.end = JournalPosition { segment, offset };
        }
    }
    Ok(scan)
}

/// A valid header must carry segment 0's identity, its own index, and continue the
/// sequence.
fn check_header(scan: &JournalScan, header: &SegmentHeader, segment: u32) -> Result<(), JournalError> {
    if let Some(identity) = &scan.identity {
        let differences = identity.differences(&header.identity, false, ["segment 0", "this segment"]);
        if !differences.is_empty() {
            let what = format!("the header's identity differs from segment 0's: {}", differences.join("; "));
            return Err(JournalError::corrupt(segment, 0, what));
        }
    }
    if header.segment != segment {
        return Err(JournalError::corrupt(segment, 0, format!("header says segment {}", header.segment)));
    }
    if header.first_seq != scan.next_seq {
        let what = format!(
            "header's first_seq is {}, but the journal continues at {}",
            header.first_seq, scan.next_seq
        );
        return Err(JournalError::corrupt(segment, 0, what));
    }
    Ok(())
}

/// The rules for a record whose CRC is valid (module docs, step 1).
fn check_record(
    bytes: &[u8],
    scan: &JournalScan,
    mode: InjectionMode,
) -> Result<(JournalRecord, Command), String> {
    let record = decode_record(bytes)
        .ok_or("the kind doesn't match the length, or the kind or its reserved byte is invalid")?;
    if record.seq != scan.next_seq {
        return Err(format!("the record's seq is {}", record.seq));
    }
    let command = check_contents(&record, mode)?;
    if record.ts <= scan.last_ts {
        return Err(format!("ts {} is not above the previous record's {}", record.ts, scan.last_ts));
    }
    Ok((record, command))
}

/// Reads a segment through a buffer of up to 8 MiB, since records are read one at a time.
struct SegmentReader {
    segment: u32,
    /// The segment offset of `bytes[0]`.
    start: u64,
    bytes: Vec<u8>,
}

impl SegmentReader {
    fn new(segment: u32) -> SegmentReader {
        SegmentReader { segment, start: 0, bytes: Vec::new() }
    }

    /// `len` bytes at `offset`, which must lie inside the segment. Reads a new chunk from
    /// `offset` when they aren't all in the one held.
    fn get(&mut self, files: &mut impl JournalFiles, offset: u64, len: u64) -> Result<&[u8], JournalError> {
        if offset < self.start || offset + len > self.start + self.bytes.len() as u64 {
            let chunk = READ_CHUNK_BYTES.max(len).min(files.segment_bytes() - offset);
            self.bytes.resize(chunk as usize, 0);
            files.read_at(self.segment, offset, &mut self.bytes).map_err(|e| read_error(self.segment, e))?;
            self.start = offset;
        }
        let from = (offset - self.start) as usize;
        Ok(&self.bytes[from..from + len as usize])
    }
}

fn read_error(segment: u32, error: std::io::Error) -> JournalError {
    JournalError::io(format!("reading segment {segment}"), error)
}

// ---------------------------------------------------------------------------------------
// Step 2.

/// One segment's part of the tail region, and the nonzero bytes step 2 found in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TailPart {
    pub segment: u32,
    pub start: u64,
    pub end: u64,
    /// The first and the last nonzero byte in `start..end`, if any.
    pub nonzero: Option<(u64, u64)>,
}

/// The tail region after `end` (module docs, step 2): the `W` bytes after the end in its
/// segment, and the first `W` bytes of the next one (each cut at the segment's end).
pub fn tail_region(end: JournalPosition, segment_bytes: u64) -> [(u32, u64, u64); 2] {
    [
        (end.segment, end.offset, (end.offset + TAIL_REGION_BYTES).min(segment_bytes)),
        (end.segment + 1, 0, TAIL_REGION_BYTES.min(segment_bytes)),
    ]
}

/// Step 2 (module docs): every nonzero byte after `end` must lie in the tail region, and
/// all of them in one of its two parts. Returns the tail region's parts in segments that
/// exist; an error names the first nonzero byte that can't be a torn tail. Changes nothing.
pub fn check_tail(
    files: &mut impl JournalFiles,
    end: JournalPosition,
) -> Result<Vec<TailPart>, JournalError> {
    let segment_bytes = files.segment_bytes();
    let region = tail_region(end, segment_bytes);
    let mut parts = Vec::new();
    let segments = files.segments().map_err(|e| JournalError::io("listing the segments", e))?;
    for segment in segments.into_iter().filter(|&segment| segment >= end.segment) {
        let from = if segment == end.segment { end.offset } else { 0 };
        // The part of this segment in the tail region (possibly empty), and what is outside.
        let (start, stop) = match region.iter().find(|(s, _, _)| *s == segment) {
            Some(&(_, start, stop)) => (start, stop),
            None => (from, from),
        };
        let outside = [(from, start), (stop, segment_bytes)];
        for (lo, hi) in outside {
            if let Some((first, _)) = nonzero_bounds(files, segment, lo, hi)? {
                let what = "a nonzero byte after the journal's end, outside what one unfinished flush can \
                            have written (damage to synced data, or data from elsewhere)";
                return Err(JournalError::corrupt(segment, first, what));
            }
        }
        if start < stop {
            let nonzero = nonzero_bounds(files, segment, start, stop)?;
            parts.push(TailPart { segment, start, end: stop, nonzero });
        }
    }
    // One unfinished flush writes into one segment: the writer syncs a segment's last batch
    // before it writes anything into the next. So nonzero bytes in both parts can't be a
    // torn tail: the journal's end was found early, by damage to synced data (review
    // finding F-TAIL-UNION), and zeroing both parts would silently cut released records.
    if let [ours, next] = parts.as_slice()
        && let (Some((first, _)), Some((next_first, _))) = (ours.nonzero, next.nonzero)
    {
        let what = format!(
            "nonzero bytes after the journal's end both here and in segment {} (from offset {next_first}): \
             one unfinished flush writes into one segment, so this is damage to synced data, or data from \
             elsewhere",
            next.segment
        );
        return Err(JournalError::corrupt(ours.segment, first, what));
    }
    Ok(parts)
}

/// The first and last nonzero byte offsets in `lo..hi` of a segment, if any.
fn nonzero_bounds(
    files: &mut impl JournalFiles,
    segment: u32,
    lo: u64,
    hi: u64,
) -> Result<Option<(u64, u64)>, JournalError> {
    let mut bounds: Option<(u64, u64)> = None;
    let mut chunk = vec![0; READ_CHUNK_BYTES.min(hi.saturating_sub(lo)) as usize];
    let mut at = lo;
    while at < hi {
        let n = chunk.len().min((hi - at) as usize);
        files.read_at(segment, at, &mut chunk[..n]).map_err(|e| read_error(segment, e))?;
        if let Some(first) = chunk[..n].iter().position(|&b| b != 0) {
            let last = chunk[..n].iter().rposition(|&b| b != 0).expect("there is one");
            let first = bounds.map_or(at + first as u64, |(f, _)| f);
            bounds = Some((first, at + last as u64));
        }
        at += n as u64;
    }
    Ok(bounds)
}

// ---------------------------------------------------------------------------------------
// Step 3.

/// Step 3 (module docs): copies the torn tail aside, zeroes the tail region, re-writes the
/// kept part of the end's segment, and syncs everything touched and the directory. Returns
/// the names of the side files.
pub fn make_durable(
    files: &mut impl JournalFiles,
    end: JournalPosition,
    tail: &[TailPart],
) -> Result<Vec<String>, JournalError> {
    let write_error = |what: String| move |e| JournalError::io(what, e);
    // 1. The torn tail, for inspection.
    let mut copies = Vec::new();
    for part in tail {
        if let Some((_, last)) = part.nonzero {
            let mut bytes = vec![0; (last + 1 - part.start) as usize];
            files.read_at(part.segment, part.start, &mut bytes).map_err(|e| read_error(part.segment, e))?;
            copies.push(write_torn_copy(files, part.segment, part.start, &bytes)?);
        }
    }
    if !copies.is_empty() {
        files.sync_dir().map_err(write_error("syncing the journal directory".into()))?;
    }
    // 2. Zeros over the tail region.
    let mut touched = Vec::new();
    for part in tail {
        let zeros = vec![0; (part.end - part.start) as usize];
        files
            .write_at(part.segment, part.start, &zeros)
            .map_err(write_error(format!("zeroing segment {}", part.segment)))?;
        touched.push(part.segment);
    }
    // 3. The kept part of the end's segment, re-written so that the next sync really writes
    // it (11.7).
    if end.offset > 0 {
        rewrite(files, end.segment, end.offset)?;
        touched.push(end.segment);
    }
    // 4. Sync every segment touched, then the directory.
    touched.sort_unstable();
    touched.dedup();
    for segment in touched {
        files.sync_data(segment).map_err(write_error(format!("syncing segment {segment}")))?;
    }
    files.sync_dir().map_err(write_error("syncing the journal directory".into()))?;
    Ok(copies)
}

/// Writes a torn tail found at `offset` of `segment` to `torn-<segment>-<offset>.bin`, and
/// returns the name. Two lives can be torn at the same place (a life whose first flush
/// lost its new segment's header, and the next life, which starts in that segment again),
/// so if an earlier recovery already wrote that name, the copy goes to
/// `torn-<segment>-<offset>-2.bin`, `-3`, and so on: a copy is never overwritten (review
/// finding F-TORN-COPY-OVERWRITE).
fn write_torn_copy(
    files: &mut impl JournalFiles,
    segment: u32,
    offset: u64,
    bytes: &[u8],
) -> Result<String, JournalError> {
    let mut n = 1;
    loop {
        let name = match n {
            1 => format!("torn-{segment:06}-{offset}.bin"),
            n => format!("torn-{segment:06}-{offset}-{n}.bin"),
        };
        match files.write_side_file(&name, bytes) {
            Ok(()) => return Ok(name),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(JournalError::io(format!("writing {name}"), e)),
        }
    }
}

/// Reads the first `len` bytes of a segment and writes them back, chunk by chunk.
fn rewrite(files: &mut impl JournalFiles, segment: u32, len: u64) -> Result<(), JournalError> {
    let mut chunk = vec![0; READ_CHUNK_BYTES.min(len) as usize];
    let mut at = 0;
    while at < len {
        let n = chunk.len().min((len - at) as usize);
        files.read_at(segment, at, &mut chunk[..n]).map_err(|e| read_error(segment, e))?;
        files
            .write_at(segment, at, &chunk[..n])
            .map_err(|e| JournalError::io(format!("re-writing segment {segment}"), e))?;
        at += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::RunClock;
    use crate::codec::encode_command;
    use crate::counters::Watermark;
    use crate::journal::files::SimDisk;
    use crate::journal::format::append_record;
    use crate::journal::writer::JournalWriter;
    use crate::records::{Meta, SIGNATURE_WORDS, Source};
    use engine::command::{CancelOrder, PlaceOrder, SetMark};
    use engine::engine::EngineOptions;
    use engine::types::{AccountId, MarketId, OrderSeq, Price, Qty, Side, TimeInForce, order_id};

    const SEGMENT: u64 = 4_096;
    const ANCHOR: u64 = 1_790_000_000_000_000_000;

    fn identity() -> JournalIdentity {
        JournalIdentity::new(1, InjectionMode::Signed, EngineOptions::default())
    }

    /// Record `seq` of a signed journal: every third an operator mark (80 bytes, whose last
    /// 24 bytes are zero), the rest signed cancels (152 bytes) of account 9, whose last bytes
    /// are not.
    fn record(seq: u64) -> JournalRecord {
        let (meta, nonce, command) = if seq.is_multiple_of(3) {
            (
                Meta::OPERATOR,
                0,
                Command::SetMark(SetMark { price: Price::new(seq as i64), market: MarketId::new(1) }),
            )
        } else {
            let meta = Meta { source: Source::SignedClient, lane: 1, account: AccountId::new(9) };
            let command = Command::CancelOrder(CancelOrder {
                order_id: order_id(AccountId::new(9), OrderSeq::new(seq as u32)),
                market: MarketId::new(1),
            });
            (meta, seq, command)
        };
        JournalRecord {
            seq,
            ts: ANCHOR + seq * 1_000,
            meta,
            nonce,
            command: encode_command(&command),
            expires_at: u64::MAX,
            signature: [u64::MAX - seq; SIGNATURE_WORDS],
        }
    }

    fn words(record: &JournalRecord) -> Vec<u64> {
        record.to_words()[..record.len_words()].to_vec()
    }

    /// A disk holding records 1..=records, written by the writer in batches of `batch`, in
    /// segment 0 onwards, every batch synced. Returns the disk and the start of the last
    /// batch.
    fn journal(records: u64, batch: u64) -> (SimDisk, JournalPosition) {
        let mut disk = SimDisk::new(SEGMENT);
        disk.create_segment(0).expect("created");
        let header = SegmentHeader::for_life(identity(), ANCHOR, [1; 32]);
        let (clock, durable) = (RunClock::start(), Watermark::new(0));
        let mut writer = JournalWriter::new(disk, header, 0, u64::MAX, 4_096);
        let mut last_batch = writer.position();
        for seq in 1..=records {
            if (seq - 1) % batch == 0 {
                writer.flush(&clock, &durable).expect("flushed");
                last_batch = writer.position();
            }
            writer.append(&words(&record(seq)), &clock, &durable).expect("appended");
        }
        writer.flush(&clock, &durable).expect("flushed");
        (writer.into_parts().0, last_batch)
    }

    fn recover(disk: &mut SimDisk) -> Result<Recovered, JournalError> {
        recover_files(disk, &identity(), false)
    }

    /// Every segment's cached and durable bytes.
    fn image(disk: &SimDisk) -> Vec<(Vec<u8>, Vec<u8>)> {
        let segments = disk.clone().segments().expect("listed");
        segments
            .into_iter()
            .map(|s| (disk.cached(s).expect("exists").to_vec(), disk.durable(s).expect("exists").to_vec()))
            .collect()
    }

    /// Writes one record's bytes (CRC filled in) at a place, synced: for crafting journals
    /// the writer would never produce.
    fn put(disk: &mut SimDisk, segment: u32, offset: u64, record: &JournalRecord) {
        let mut bytes = Vec::new();
        append_record(&mut bytes, &words(record));
        disk.write_at(segment, offset, &bytes).expect("written");
        disk.sync_data(segment).expect("synced");
    }

    fn corrupt_at(error: JournalError) -> (JournalPosition, String) {
        match error {
            JournalError::Corrupt { at, what } => (at, what),
            other => panic!("expected a corruption error, got {other}"),
        }
    }

    #[test]
    fn records_across_several_segments_are_read_back_in_order() {
        let (mut disk, _) = journal(100, 7);
        let mut seen = Vec::new();
        let scan =
            scan_journal(&mut disk, |segment, record, _| seen.push((segment, record.seq))).expect("valid");
        assert_eq!(scan.records, 100);
        assert_eq!(seen.iter().map(|&(_, seq)| seq).collect::<Vec<_>>(), (1..=100).collect::<Vec<_>>());
        assert!(scan.headers.len() >= 3, "several segments: {}", scan.headers.len());
        for (segment, header) in scan.headers.iter().enumerate() {
            let first = seen.iter().find(|&&(s, _)| s == segment as u32).expect("records in every segment").1;
            assert_eq!((header.segment, header.first_seq), (segment as u32, first));
        }
        let recovered = recover(&mut disk).expect("recovered");
        assert_eq!((recovered.records, recovered.next_seq, recovered.last_ts), (100, 101, ANCHOR + 100_000));
        assert_eq!(recovered.registry_digests, vec![[1; 32]; scan.headers.len()]);
        assert!(recovered.torn_copies.is_empty());
        assert_eq!(recovered.end, scan.end);
    }

    #[test]
    fn an_empty_journal_recovers_to_nothing_and_the_next_life_starts_in_segment_0() {
        let mut disk = SimDisk::new(SEGMENT);
        let recovered = recover(&mut disk).expect("an empty journal");
        assert_eq!((recovered.records, recovered.next_seq, recovered.end.offset), (0, 1, 0));
        assert_eq!(recovered.identity, identity());
        assert_eq!(recovered.end.next_life_segment(), 0);
        disk.create_segment(0).expect("created");
        assert_eq!(recover(&mut disk).expect("still empty").records, 0);
    }

    #[test]
    fn a_tail_zeroed_from_any_byte_of_the_last_batch_keeps_exactly_the_records_before_it() {
        let (disk, last_batch) = journal(30, 6);
        let segment = last_batch.segment;
        let original = disk.cached(segment).expect("exists").to_vec();
        // Where each record of the journal starts, in the last batch's segment.
        let mut starts = Vec::new();
        scan_journal(&mut disk.clone(), |s, record, _| {
            if s == segment {
                starts.push((record.seq, record.len_bytes() as usize));
            }
        })
        .expect("valid");
        let mut offset = HEADER_BYTES;
        let layout: Vec<(u64, usize, usize)> = starts
            .iter()
            .map(|&(seq, len)| {
                let at = offset;
                offset += len;
                (seq, at, len)
            })
            .collect();
        let batch_end = offset;
        for x in last_batch.offset as usize..batch_end {
            let mut damaged = disk.clone();
            let zeros = vec![0; batch_end - x];
            damaged.write_at(segment, x as u64, &zeros).expect("written");
            damaged.sync_data(segment).expect("synced");
            let after = damaged.cached(segment).expect("exists").to_vec();
            // A record survives iff zeroing left its bytes unchanged (zeroing a mark's zero
            // tail changes nothing), and every record after the first changed one is lost.
            let first_changed = layout
                .iter()
                .find(|&&(_, at, len)| after[at..at + len] != original[at..at + len])
                .map_or(31, |&(seq, _, _)| seq);
            let recovered = recover(&mut damaged).unwrap_or_else(|e| panic!("offset {x}: {e}"));
            assert_eq!(recovered.next_seq, first_changed, "zeroed from offset {x}");
            let bytes = damaged.durable(segment).expect("exists");
            assert!(bytes[recovered.end.offset as usize..].iter().all(|&b| b == 0), "offset {x}");
        }
    }

    #[test]
    fn a_flipped_byte_in_the_last_record_ends_the_journal_and_the_tail_is_copied_aside() {
        let (mut disk, _) = journal(10, 10);
        let end = recover(&mut disk.clone()).expect("valid").end;
        disk.corrupt(end.segment, end.offset - 3, 0x01);
        let recovered = recover(&mut disk).expect("a torn tail");
        assert_eq!(recovered.records, 9);
        assert_eq!(recovered.torn_copies, [format!("torn-{:06}-{}.bin", end.segment, recovered.end.offset)]);
        let copy = disk.side_file(&recovered.torn_copies[0]).expect("copied");
        assert_eq!(copy.len() as u64, end.offset - recovered.end.offset, "the torn record");
        let bytes = disk.durable(end.segment).expect("exists");
        assert!(bytes[recovered.end.offset as usize..].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_torn_record_at_a_segment_boundary_ends_the_journal_after_the_new_header() {
        // 40 records take 5,144 bytes: segment 0 and part of segment 1.
        let (disk, _) = journal(40, 1);
        let recovered = recover(&mut disk.clone()).expect("valid");
        assert_eq!(recovered.end.segment, 1);
        let first_of_1 = recovered.headers[1].first_seq;
        let mut damaged = disk.clone();
        damaged.corrupt(1, HEADER_BYTES as u64 + 20, 0x80);
        let recovered = recover(&mut damaged).expect("a torn tail");
        assert_eq!(recovered.next_seq, first_of_1, "segment 1 keeps its header and no record");
        assert_eq!(recovered.end, JournalPosition { segment: 1, offset: HEADER_BYTES as u64 });
        assert_eq!(recovered.end.next_life_segment(), 2);
    }

    #[test]
    fn a_lost_header_of_a_new_segment_whose_body_reached_the_disk_is_a_torn_tail() {
        let (disk, _) = journal(40, 1);
        let first_of_1 = recover(&mut disk.clone()).expect("valid").headers[1].first_seq;
        let mut damaged = disk.clone();
        damaged.write_at(1, 0, &[0; HEADER_BYTES]).expect("written");
        damaged.sync_data(1).expect("synced");
        let recovered = recover(&mut damaged).expect("a torn tail");
        assert_eq!((recovered.next_seq, recovered.end.segment), (first_of_1, 0));
        assert!(damaged.durable(1).expect("exists").iter().all(|&b| b == 0), "the old body is zeroed");
        assert_eq!(recovered.torn_copies.len(), 1);
        assert_eq!(recovered.end.next_life_segment(), 1, "the next life reuses segment 1, now all zeros");
    }

    #[test]
    fn a_valid_record_that_does_not_fit_is_an_error_and_changes_nothing() {
        let signed = Meta { source: Source::SignedClient, lane: 1, account: AccountId::new(9) };
        let place = |account| {
            encode_command(&Command::PlaceOrder(PlaceOrder {
                order_id: order_id(AccountId::new(account), OrderSeq::new(1)),
                price: Price::new(1),
                qty: Qty::new(1),
                market: MarketId::new(1),
                side: Side::Buy,
                tif: TimeInForce::Gtc,
                post_only: false,
            }))
        };
        let cases: Vec<(&str, JournalRecord)> = vec![
            ("the record's seq is 12", JournalRecord { seq: 12, ..record(11) }),
            (
                "client command tag 1",
                JournalRecord { meta: Meta::OPERATOR, nonce: 0, command: place(0), ..record(11) },
            ),
            (
                "kind-2",
                JournalRecord { meta: Meta { source: Source::PreVerifiedClient, ..signed }, ..record(11) },
            ),
            ("doesn't own", JournalRecord { meta: signed, command: place(8), ..record(11) }),
            ("is not above", JournalRecord { ts: ANCHOR + 10_000, ..record(11) }),
        ];
        for (expected, bad) in cases {
            let (mut disk, _) = journal(10, 10);
            let end = recover(&mut disk.clone()).expect("valid").end;
            put(&mut disk, end.segment, end.offset, &bad);
            let before = image(&disk);
            let (at, what) = corrupt_at(recover(&mut disk).expect_err(expected));
            assert_eq!(at, end, "{expected}: the place is named");
            assert!(what.contains(expected), "{expected}: {what}");
            assert!(image(&disk) == before, "{expected}: nothing changed");
        }
    }

    #[test]
    fn a_later_header_with_a_valid_crc_that_does_not_fit_is_an_error() {
        let (disk, _) = journal(40, 1);
        let good = recover(&mut disk.clone()).expect("valid").headers[1];
        let other_options = EngineOptions { order_capacity: 99, ..good.identity.engine };
        let cases: [(&str, SegmentHeader); 4] = [
            (
                "deployment",
                SegmentHeader { identity: JournalIdentity { deployment: 2, ..good.identity }, ..good },
            ),
            (
                "order_capacity",
                SegmentHeader {
                    identity: JournalIdentity { engine: other_options, ..good.identity },
                    ..good
                },
            ),
            ("first_seq", SegmentHeader { first_seq: good.first_seq + 1, ..good }),
            ("says segment 7", SegmentHeader { segment: 7, ..good }),
        ];
        for (expected, header) in cases {
            let mut disk = disk.clone();
            disk.write_at(1, 0, &header.encode()).expect("written");
            disk.sync_data(1).expect("synced");
            let before = image(&disk);
            let (at, what) = corrupt_at(recover(&mut disk).expect_err(expected));
            assert_eq!(at, JournalPosition { segment: 1, offset: 0 }, "{expected}");
            assert!(what.contains(expected), "{expected}: {what}");
            assert!(image(&disk) == before, "{expected}: nothing changed");
        }
    }

    #[test]
    fn a_flipped_byte_in_synced_data_far_from_the_end_is_an_error_and_changes_nothing() {
        let (mut disk, _) = journal(100, 5);
        let end = recover(&mut disk.clone()).expect("valid").end;
        assert!(end.segment >= 3);
        // A record in segment 0: the journal now ends there. With 4 KiB segments the tail
        // region covers the rest of segment 0 and all of segment 1, so the first nonzero
        // byte outside it is segment 2's header.
        disk.corrupt(0, 500, 0x04);
        let before = image(&disk);
        let (at, what) = corrupt_at(recover(&mut disk).expect_err("damage in the middle"));
        assert_eq!(at.segment, 2, "the first nonzero byte outside the tail region");
        assert!(what.contains("outside"), "{what}");
        assert!(image(&disk) == before, "nothing changed");
    }

    #[test]
    fn a_flipped_byte_in_synced_data_while_the_journal_goes_on_in_the_next_segment_is_an_error() {
        // 40 records: segment 0 and part of segment 1, so both parts of the tail region
        // behind a record of segment 0 hold synced data.
        let (mut disk, _) = journal(40, 1);
        assert_eq!(recover(&mut disk.clone()).expect("valid").end.segment, 1);
        disk.corrupt(0, 500, 0x04);
        let before = image(&disk);
        let (at, what) = corrupt_at(recover(&mut disk).expect_err("damage to synced data"));
        assert_eq!(at.segment, 0, "named where the journal was cut short");
        assert!(what.contains("both here and in segment 1"), "{what}");
        assert!(image(&disk) == before, "nothing changed");
    }

    #[test]
    fn a_second_torn_tail_at_the_same_place_gets_its_own_copy() {
        let (mut disk, _) = journal(10, 10);
        let end = recover(&mut disk.clone()).expect("valid").end;
        let mut names = Vec::new();
        for flipped in [0x01, 0x02] {
            // A record written after the end, torn: one bit wrong.
            put(&mut disk, end.segment, end.offset, &record(11));
            disk.corrupt(end.segment, end.offset + 20, flipped);
            let recovered = recover(&mut disk).expect("a torn tail");
            assert_eq!((recovered.records, recovered.end), (10, end));
            names.extend(recovered.torn_copies);
        }
        let first = format!("torn-{:06}-{}.bin", end.segment, end.offset);
        let second = format!("torn-{:06}-{}-2.bin", end.segment, end.offset);
        assert_eq!(names, [first.clone(), second.clone()]);
        assert_ne!(disk.side_file(&first), disk.side_file(&second), "each torn tail's own bytes");
    }

    #[test]
    fn after_recovery_the_next_life_starts_in_the_next_segment_with_the_next_seq() {
        let (mut disk, _) = journal(10, 10);
        let recovered = recover(&mut disk).expect("valid");
        let first = recovered.end.next_life_segment();
        assert_eq!(first, 1);
        disk.create_segment(first).expect("created");
        let header = SegmentHeader::for_life(identity(), ANCHOR + 1_000_000, [2; 32]);
        let (clock, durable) = (RunClock::start(), Watermark::new(recovered.next_seq - 1));
        let mut writer = JournalWriter::new(disk, header, first, 0, 4_096);
        for seq in recovered.next_seq..recovered.next_seq + 5 {
            let record = JournalRecord { ts: ANCHOR + 1_000_000 + seq, ..record(seq) };
            writer.append(&words(&record), &clock, &durable).expect("appended");
        }
        writer.flush(&clock, &durable).expect("flushed");
        let mut disk = writer.into_parts().0;
        let again = recover(&mut disk).expect("valid");
        assert_eq!(again.records, 15);
        assert_eq!(again.headers[1].first_seq, 11);
        assert_eq!(again.registry_digests, [[1; 32], [2; 32]], "a new registry's digest in the new segment");
    }

    #[test]
    fn a_configuration_that_differs_from_the_journals_identity_is_refused() {
        let (mut disk, _) = journal(5, 5);
        let other = JournalIdentity { deployment: 2, ..identity() };
        let before = image(&disk);
        match recover_files(&mut disk, &other, false) {
            Err(JournalError::IdentityMismatch { differences }) => {
                assert_eq!(differences, ["deployment: journal 1, configuration 2"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(image(&disk) == before);
        // A journal signed one way is never resumed signing the other (D-033).
        let eip712 = identity().with_auth(crate::records::AuthScheme::Eip712);
        match recover_files(&mut disk, &eip712, false) {
            Err(JournalError::IdentityMismatch { differences }) => {
                assert_eq!(differences, ["auth: journal perp, configuration eip712"]);
            }
            other => panic!("{other:?}"),
        }
        let newer = JournalIdentity { engine_semantics: identity().engine_semantics + 1, ..identity() };
        assert!(recover_files(&mut disk, &newer, false).is_err());
        assert!(recover_files(&mut disk, &newer, true).is_ok(), "--allow-engine-change");
    }

    #[test]
    fn recovery_rewrites_what_a_failed_sync_left_only_in_the_page_cache() {
        let (mut disk, _) = journal(10, 10);
        let end = recover(&mut disk.clone()).expect("valid").end;
        // One more record, written, then a sync that fails the Linux way.
        let mut bytes = Vec::new();
        append_record(&mut bytes, &words(&record(11)));
        disk.write_at(end.segment, end.offset, &bytes).expect("written");
        disk.inject(crate::journal::files::Fault::FailSync);
        assert!(disk.sync_data(end.segment).is_err());
        // Restart on the same boot: the page cache still shows it, so recovery keeps it...
        let recovered = recover(&mut disk).expect("valid");
        assert_eq!(recovered.records, 11);
        // ...and made it durable: a power loss now keeps it.
        disk.lose_power(|| false);
        assert_eq!(recover(&mut disk).expect("valid").records, 11);
    }

    #[test]
    fn the_tail_region_is_w_bytes_after_the_end_and_the_first_w_of_the_next_segment() {
        let end = JournalPosition { segment: 3, offset: 1_000 };
        assert_eq!(tail_region(end, 1 << 30), [(3, 1_000, 1_000 + 622_720), (4, 0, 622_720)]);
        assert_eq!(tail_region(end, 4_096), [(3, 1_000, 4_096), (4, 0, 4_096)]);
    }
}
