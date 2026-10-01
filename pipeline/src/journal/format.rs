//! The journal's bytes: the segment header, the journal's identity, and the records
//! (`docs/PIPELINE.md` 11.2 and 11.3).
//!
//! **Contract.**
//! - A segment is a fixed-size file: a 128-byte [`SegmentHeader`] at offset 0, then records
//!   back to back from offset 128. A segment is created all zeros, and an all-zero header
//!   means "not used yet".
//! - A record is exactly the sequencer's [`JournalRecord`] words as little-endian bytes (80
//!   bytes for kinds 2 and 3, 152 for kind 1), with bytes 4..8 holding the CRC32C of every
//!   other byte of the record ([`append_record`]).
//! - The header's *identity* fields ([`JournalIdentity`]: deployment, mode, signing scheme,
//!   `engine_semantics` and the five engine options) are the same in every segment of one
//!   journal; the others describe the life that wrote the segment (its clock anchor, its key
//!   registry, the binary's commit and build profile).
//!
//! **Header layout** (11.2), little-endian:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 8 | magic `PERPJNL1` |
//! | 8 | 2 | format version, 1 |
//! | 10 | 1 | mode: 1 signed, 2 pre-verified |
//! | 11 | 1 | build profile: 1 release, 0 other |
//! | 12 | 4 | deployment id |
//! | 16 | 4 | segment index |
//! | 20 | 4 | `engine_semantics` |
//! | 24 | 8 | `first_seq` |
//! | 32 | 8 | `id_hash_seed` |
//! | 40 | 8 | `order_capacity` |
//! | 48 | 8 | `account_capacity` |
//! | 56 | 8 | `slot_capacity` |
//! | 64 | 8 | `scratch_capacity` |
//! | 72 | 8 | `run_start_unix_ns` |
//! | 80 | 32 | SHA-256 of `keys.txt` (zero in a pre-verified journal) |
//! | 112 | 8 | the writer's commit (zero if unknown) |
//! | 120 | 1 | signing scheme: 0 perp, 1 eip712 ([`AuthScheme`]) |
//! | 121 | 3 | reserved, 0 |
//! | 124 | 4 | CRC32C of bytes 0..124 |
//!
//! **The signing scheme** (byte 120; `docs/DECISIONS.md` D-033) was reserved, and so zero,
//! before the second scheme existed: a journal written then reads as `perp`, which is what
//! it is, and a binary from then refuses a journal of the EIP-712 scheme (its reserved
//! bytes are not zero) rather than audit its records the wrong way.
//!
//! **What each record kind may hold** (11.3; recovery checks it all, [`check_contents`]):
//! kinds 1 and 2 hold client commands (tags 1 to 3) whose order id belongs to the record's
//! account; kind 3 holds operator commands (tags 4 to 9) with lane, account and nonce 0;
//! kind 1 appears only in a signed journal and kind 2 only in a pre-verified one.
//!
//! **`ENGINE_SEMANTICS`** ties a journal to the engine's behaviour, which the byte format
//! alone doesn't: bumped by any engine change that can alter an event or the state for a
//! command the old engine applied without panicking, and pinned by
//! `pipeline/tests/engine_semantics.rs` (11.2).
//!
//! **Complexity.** Everything here is O(size of one header or record).

use engine::command::Command;
use engine::engine::EngineOptions;
use engine::types::{OrderId, account_of};

use crate::codec::decode_command;
use crate::crc32c::{Crc32c, crc32c};
use crate::records::{AuthScheme, InjectionMode, JournalRecord, Source};

/// Bytes in a segment header.
pub const HEADER_BYTES: usize = 128;
/// The header's first 8 bytes.
pub const MAGIC: [u8; 8] = *b"PERPJNL1";
/// The format version in every header this code writes.
pub const FORMAT_VERSION: u16 = 1;
/// The engine's behaviour, as far as the journal is concerned (module docs). Bump it, and
/// re-pin `pipeline/tests/engine_semantics.rs`, whenever an engine change alters an event or
/// the state for a command the old engine applied without panicking.
pub const ENGINE_SEMANTICS: u32 = 1;
/// The production segment size, 1 GiB (11.1). Tests use 4 KiB.
pub const DEFAULT_SEGMENT_BYTES: u64 = 1 << 30;
/// The most records one flush may write (`B`, 11.5). Recovery's tail region relies on it.
pub const MAX_BATCH_RECORDS: usize = 4_096;
/// The longest record: kind 1, 152 bytes.
pub const MAX_RECORD_BYTES: usize = JournalRecord::SIGNED_BYTES as usize;
/// `W`: the most bytes one unfinished flush can have written, a segment header and a full
/// batch of the longest records, 622,720 bytes (11.8, step 2).
pub const TAIL_REGION_BYTES: u64 = (HEADER_BYTES + MAX_BATCH_RECORDS * MAX_RECORD_BYTES) as u64;

const _: () = assert!(TAIL_REGION_BYTES == 622_720);

/// What must be the same in every segment of one journal, and what a restarting process
/// checks its configuration against (11.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalIdentity {
    pub deployment: u32,
    pub mode: InjectionMode,
    /// How the kind-1 records were signed (module docs). Always `Perp` in a pre-verified
    /// journal, which holds no signatures (`Pipeline::start` refuses anything else).
    pub auth: AuthScheme,
    /// [`ENGINE_SEMANTICS`] of the binary that started the journal.
    pub engine_semantics: u32,
    /// The five engine options, including the hash seed (D-011). The `Debug` output leaves
    /// the seed out (`EngineOptions`'s own `Debug`).
    pub engine: EngineOptions,
}

impl JournalIdentity {
    /// The identity of a journal this binary starts (or checks against): its own
    /// [`ENGINE_SEMANTICS`], and the perp signing scheme ([`JournalIdentity::with_auth`]
    /// sets the other).
    pub fn new(deployment: u32, mode: InjectionMode, engine: EngineOptions) -> JournalIdentity {
        JournalIdentity {
            deployment,
            mode,
            auth: AuthScheme::Perp,
            engine_semantics: ENGINE_SEMANTICS,
            engine,
        }
    }

    /// The same identity with the signing scheme `auth`.
    pub fn with_auth(self, auth: AuthScheme) -> JournalIdentity {
        JournalIdentity { auth, ..self }
    }

    /// Every field that differs from `other`, as "field: `names[0]` X, `names[1]` Y" (for
    /// example "deployment: journal 1, configuration 2"; the seed without its values).
    /// `engine_semantics` is left out if `allow_engine_change`.
    pub fn differences(
        &self,
        other: &JournalIdentity,
        allow_engine_change: bool,
        names: [&str; 2],
    ) -> Vec<String> {
        let [mine_name, theirs_name] = names;
        let mut differences = Vec::new();
        let mut compare = |field: &str, mine: u64, theirs: u64| {
            if mine != theirs {
                differences.push(format!("{field}: {mine_name} {mine}, {theirs_name} {theirs}"));
            }
        };
        compare("deployment", self.deployment.into(), other.deployment.into());
        compare("mode", self.mode.code().into(), other.mode.code().into());
        if !allow_engine_change {
            compare("engine_semantics", self.engine_semantics.into(), other.engine_semantics.into());
        }
        let (mine, theirs) = (&self.engine, &other.engine);
        compare("order_capacity", mine.order_capacity as u64, theirs.order_capacity as u64);
        compare("account_capacity", mine.account_capacity as u64, theirs.account_capacity as u64);
        compare("slot_capacity", mine.slot_capacity as u64, theirs.slot_capacity as u64);
        compare("scratch_capacity", mine.scratch_capacity as u64, theirs.scratch_capacity as u64);
        if mine.id_hash_seed != theirs.id_hash_seed {
            differences.push("id_hash_seed: differs (values not printed: the seed is a secret)".into());
        }
        // By name ("perp", "eip712"), after the fields `compare` prints.
        if self.auth != other.auth {
            differences.push(format!("auth: {mine_name} {}, {theirs_name} {}", self.auth, other.auth));
        }
        differences
    }
}

/// One segment's header (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentHeader {
    pub identity: JournalIdentity,
    /// The writer was a release build (informational).
    pub release_build: bool,
    /// The segment's index: the `N` of its file name.
    pub segment: u32,
    /// The seq of the segment's first record.
    pub first_seq: u64,
    /// The clock anchor of the life that wrote this segment (9.2).
    pub run_start_unix_ns: u64,
    /// SHA-256 of the key registry that life loaded (zero in a pre-verified journal).
    pub registry_digest: [u8; 32],
    /// The first 8 bytes of the writer's commit ([`build_commit`]).
    pub commit: [u8; 8],
}

/// What 128 header bytes turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderRead {
    /// All zeros: the segment is not used yet.
    Unused,
    /// The CRC fails: a header write that didn't finish (a torn header).
    Torn,
    /// The CRC is valid, but the bytes are not a header this code understands. Recovery
    /// treats that as an error, never as the end (11.8).
    Invalid(String),
    Valid(SegmentHeader),
}

impl SegmentHeader {
    /// A header for a segment written by this binary, in a life whose clock anchor is
    /// `run_start_unix_ns` and whose key registry has the digest `registry_digest`; its
    /// segment index and first seq are filled in when the segment gets its first record.
    pub fn for_life(identity: JournalIdentity, run_start_unix_ns: u64, registry_digest: [u8; 32]) -> Self {
        SegmentHeader {
            identity,
            release_build: !cfg!(debug_assertions),
            segment: 0,
            first_seq: 0,
            run_start_unix_ns,
            registry_digest,
            commit: build_commit(),
        }
    }

    /// The 128 bytes of this header, CRC included.
    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0; HEADER_BYTES];
        let engine = &self.identity.engine;
        bytes[0..8].copy_from_slice(&MAGIC);
        bytes[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes[10] = self.identity.mode.code();
        bytes[11] = u8::from(self.release_build);
        bytes[12..16].copy_from_slice(&self.identity.deployment.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.segment.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.identity.engine_semantics.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.first_seq.to_le_bytes());
        bytes[32..40].copy_from_slice(&engine.id_hash_seed.to_le_bytes());
        bytes[40..48].copy_from_slice(&(engine.order_capacity as u64).to_le_bytes());
        bytes[48..56].copy_from_slice(&(engine.account_capacity as u64).to_le_bytes());
        bytes[56..64].copy_from_slice(&(engine.slot_capacity as u64).to_le_bytes());
        bytes[64..72].copy_from_slice(&(engine.scratch_capacity as u64).to_le_bytes());
        bytes[72..80].copy_from_slice(&self.run_start_unix_ns.to_le_bytes());
        bytes[80..112].copy_from_slice(&self.registry_digest);
        bytes[112..120].copy_from_slice(&self.commit);
        bytes[120] = self.identity.auth.code();
        // Bytes 121..124 are reserved and stay 0.
        let crc = crc32c(&bytes[..124]);
        bytes[124..128].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    /// Reads 128 header bytes (see [`HeaderRead`]).
    pub fn decode(bytes: &[u8; HEADER_BYTES]) -> HeaderRead {
        if bytes.iter().all(|&b| b == 0) {
            return HeaderRead::Unused;
        }
        if crc32c(&bytes[..124]) != u32_at(bytes, 124) {
            return HeaderRead::Torn;
        }
        match Self::parse(bytes) {
            Ok(header) => HeaderRead::Valid(header),
            Err(why) => HeaderRead::Invalid(why),
        }
    }

    /// The fields of a header whose CRC is valid, or what is wrong with them.
    fn parse(bytes: &[u8; HEADER_BYTES]) -> Result<SegmentHeader, String> {
        if bytes[0..8] != MAGIC {
            return Err(format!("the magic is {:?}, not PERPJNL1", String::from_utf8_lossy(&bytes[0..8])));
        }
        let version = u16::from_le_bytes([bytes[8], bytes[9]]);
        if version != FORMAT_VERSION {
            return Err(format!("format version {version}, not {FORMAT_VERSION}"));
        }
        let mode =
            InjectionMode::from_code(bytes[10]).ok_or(format!("unknown journal mode {}", bytes[10]))?;
        let release_build = match bytes[11] {
            0 => false,
            1 => true,
            other => return Err(format!("unknown build profile {other}")),
        };
        let auth =
            AuthScheme::from_code(bytes[120]).ok_or(format!("unknown signing scheme {}", bytes[120]))?;
        if bytes[121..124] != [0; 3] {
            return Err("the reserved bytes 121..124 are not zero".into());
        }
        let engine = EngineOptions {
            id_hash_seed: u64_at(bytes, 32),
            order_capacity: usize_at(bytes, 40)?,
            account_capacity: usize_at(bytes, 48)?,
            slot_capacity: usize_at(bytes, 56)?,
            scratch_capacity: usize_at(bytes, 64)?,
        };
        Ok(SegmentHeader {
            identity: JournalIdentity {
                deployment: u32_at(bytes, 12),
                mode,
                auth,
                engine_semantics: u32_at(bytes, 20),
                engine,
            },
            release_build,
            segment: u32_at(bytes, 16),
            first_seq: u64_at(bytes, 24),
            run_start_unix_ns: u64_at(bytes, 72),
            registry_digest: bytes[80..112].try_into().expect("32 bytes"),
            commit: bytes[112..120].try_into().expect("8 bytes"),
        })
    }
}

/// The running binary's commit: the first 8 bytes of the `PERPS_GIT_COMMIT` it was built
/// with (the short commit id, as the recorder's build sets it), zero-padded; all zero if
/// unknown (11.2).
pub fn build_commit() -> [u8; 8] {
    let mut commit = [0; 8];
    let text = option_env!("PERPS_GIT_COMMIT").unwrap_or("").as_bytes();
    let n = text.len().min(8);
    commit[..n].copy_from_slice(&text[..n]);
    commit
}

// ---------------------------------------------------------------------------------------
// Records.

/// Appends one record, given as the sequencer's words (CRC field 0), to `buf` as its
/// on-disk bytes, with the CRC filled in (11.3, 11.4).
pub fn append_record(buf: &mut Vec<u8>, words: &[u64]) {
    let start = buf.len();
    for word in words {
        buf.extend_from_slice(&word.to_le_bytes());
    }
    let record = &mut buf[start..];
    let crc = record_crc(record);
    record[4..8].copy_from_slice(&crc.to_le_bytes());
}

/// The CRC a record's bytes should carry: CRC32C of bytes 0..4 followed by bytes 8..len,
/// every byte but the CRC field itself (11.3).
pub fn record_crc(record: &[u8]) -> u32 {
    let mut crc = Crc32c::new();
    crc.update(&record[..4]);
    crc.update(&record[8..]);
    crc.finish()
}

/// The CRC a record's bytes do carry (bytes 4..8).
pub fn stored_crc(record: &[u8]) -> u32 {
    u32_at(record, 4)
}

/// The `len` field of a record starting at `bytes[0]`.
pub fn record_len(bytes: &[u8]) -> u32 {
    u32_at(bytes, 0)
}

/// Reads a record's bytes back into its fields, ignoring the CRC. `None` if the meta word
/// (kind, reserved byte) is not valid or the length doesn't match the kind
/// ([`JournalRecord::from_words`]).
pub fn decode_record(bytes: &[u8]) -> Option<JournalRecord> {
    let (words, rest) = bytes.as_chunks::<8>();
    if !rest.is_empty() {
        return None;
    }
    let words: Vec<u64> = words.iter().map(|chunk| u64::from_le_bytes(*chunk)).collect();
    JournalRecord::from_words(&words)
}

/// 11.3's rules for what a record of each kind may hold, in a journal of `mode`. Returns the
/// decoded command, or what is wrong.
pub fn check_contents(record: &JournalRecord, mode: InjectionMode) -> Result<Command, String> {
    let kind = record.meta.source;
    match kind {
        Source::SignedClient if mode != InjectionMode::Signed => {
            return Err("a kind-1 (signed) record in a pre-verified journal".into());
        }
        Source::PreVerifiedClient if mode != InjectionMode::PreVerified => {
            return Err("a kind-2 (pre-verified) record in a signed journal".into());
        }
        _ => {}
    }
    let command = decode_command(&record.command).map_err(|e| format!("the CMD40 doesn't decode: {e}"))?;
    let tag = command_tag_of(&record.command);
    match (kind, client_order_id(&command)) {
        (Source::SignedClient | Source::PreVerifiedClient, Some(order_id)) => {
            if account_of(order_id) != record.meta.account {
                return Err(format!(
                    "account {} doesn't own order {order_id:#x} (its account is {})",
                    record.meta.account,
                    account_of(order_id)
                ));
            }
        }
        (Source::SignedClient | Source::PreVerifiedClient, None) => {
            return Err(format!("a client record (kind {}) holding operator command tag {tag}", kind.code()));
        }
        (Source::Operator, Some(_)) => {
            return Err(format!("an operator record (kind 3) holding client command tag {tag}"));
        }
        (Source::Operator, None) => {
            let meta = record.meta;
            if meta.lane != 0 || meta.account != 0 || record.nonce != 0 {
                return Err("an operator record whose lane, account or nonce is not 0".into());
            }
        }
    }
    Ok(command)
}

/// The order id of a client command (tags 1 to 3); `None` for an operator command.
pub fn client_order_id(command: &Command) -> Option<OrderId> {
    match command {
        Command::PlaceOrder(place) => Some(place.order_id),
        Command::CancelOrder(cancel) => Some(cancel.order_id),
        Command::ModifyOrder(modify) => Some(modify.order_id),
        _ => None,
    }
}

/// Byte 0 of a CMD40: the command's tag.
fn command_tag_of(command: &[u64; 5]) -> u8 {
    command[0] as u8
}

// ---------------------------------------------------------------------------------------
// Little-endian fields.

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

fn usize_at(bytes: &[u8], at: usize) -> Result<usize, String> {
    usize::try_from(u64_at(bytes, at)).map_err(|_| format!("the value at byte {at} doesn't fit a usize"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::encode_command;
    use crate::records::{Meta, SIGNATURE_WORDS, spec_examples};
    use engine::command::{CancelOrder, Deposit, PlaceOrder, SetMark};
    use engine::types::{Side, TimeInForce, order_id};

    fn identity() -> JournalIdentity {
        JournalIdentity::new(
            7,
            InjectionMode::Signed,
            EngineOptions {
                order_capacity: 11,
                id_hash_seed: 0xDEAD_BEEF,
                scratch_capacity: 13,
                account_capacity: 17,
                slot_capacity: 19,
            },
        )
    }

    fn header() -> SegmentHeader {
        SegmentHeader {
            segment: 3,
            first_seq: 5_000,
            commit: *b"a1b2c3d\0",
            ..SegmentHeader::for_life(identity(), 1_790_000_000_000_000_000, [0xAB; 32])
        }
    }

    fn place(account: u32, seq: u32) -> Command {
        Command::PlaceOrder(PlaceOrder {
            order_id: order_id(account, seq),
            price: 102_998,
            qty: 500_000,
            market: 3,
            side: Side::Buy,
            tif: TimeInForce::Gtc,
            post_only: true,
        })
    }

    fn record(meta: Meta, nonce: u64, command: &Command) -> JournalRecord {
        JournalRecord {
            seq: 1,
            ts: 2,
            meta,
            nonce,
            command: encode_command(command),
            expires_at: 3,
            signature: [4; SIGNATURE_WORDS],
        }
    }

    #[test]
    fn a_header_round_trips_and_has_the_specs_offsets() {
        let header = header();
        let bytes = header.encode();
        assert_eq!(&bytes[0..8], b"PERPJNL1");
        assert_eq!(bytes[8..10], [1, 0]);
        assert_eq!(bytes[10], 1, "signed");
        assert_eq!(bytes[11], u8::from(!cfg!(debug_assertions)));
        assert_eq!(bytes[12..16], 7u32.to_le_bytes());
        assert_eq!(bytes[16..20], 3u32.to_le_bytes());
        assert_eq!(bytes[20..24], ENGINE_SEMANTICS.to_le_bytes());
        assert_eq!(bytes[24..32], 5_000u64.to_le_bytes());
        assert_eq!(bytes[32..40], 0xDEAD_BEEFu64.to_le_bytes());
        assert_eq!(bytes[40..48], 11u64.to_le_bytes(), "order_capacity");
        assert_eq!(bytes[48..56], 17u64.to_le_bytes(), "account_capacity");
        assert_eq!(bytes[56..64], 19u64.to_le_bytes(), "slot_capacity");
        assert_eq!(bytes[64..72], 13u64.to_le_bytes(), "scratch_capacity");
        assert_eq!(bytes[72..80], 1_790_000_000_000_000_000u64.to_le_bytes());
        assert_eq!(bytes[80..112], [0xAB; 32]);
        assert_eq!(&bytes[112..120], b"a1b2c3d\0");
        assert_eq!(bytes[120..124], [0; 4]);
        assert_eq!(u32_at(&bytes, 124), crc32c(&bytes[..124]));
        assert_eq!(SegmentHeader::decode(&bytes), HeaderRead::Valid(header));
    }

    #[test]
    fn zeros_are_unused_a_bad_crc_is_torn_and_a_valid_crc_on_nonsense_is_invalid() {
        assert_eq!(SegmentHeader::decode(&[0; HEADER_BYTES]), HeaderRead::Unused);
        let good = header().encode();
        for at in [0, 10, 64, 123, 124, 127] {
            let mut torn = good;
            torn[at] ^= 0x10;
            assert_eq!(SegmentHeader::decode(&torn), HeaderRead::Torn, "byte {at} flipped");
        }
        // Change a byte and fix the CRC: the header is well-formed as far as the CRC goes.
        let resealed = |at: usize, value: u8| {
            let mut bytes = good;
            bytes[at] = value;
            let crc = crc32c(&bytes[..124]);
            bytes[124..].copy_from_slice(&crc.to_le_bytes());
            SegmentHeader::decode(&bytes)
        };
        assert!(matches!(resealed(0, b'X'), HeaderRead::Invalid(why) if why.contains("magic")));
        assert!(matches!(resealed(8, 2), HeaderRead::Invalid(why) if why.contains("version")));
        assert!(matches!(resealed(10, 3), HeaderRead::Invalid(why) if why.contains("mode")));
        assert!(matches!(resealed(11, 2), HeaderRead::Invalid(why) if why.contains("profile")));
        assert!(matches!(resealed(121, 1), HeaderRead::Invalid(why) if why.contains("reserved")));
        assert!(matches!(resealed(12, 8), HeaderRead::Valid(h) if h.identity.deployment == 8));
    }

    #[test]
    fn identity_differences_name_each_field_and_hide_the_seed() {
        let journal = identity();
        let names = ["journal", "configuration"];
        assert!(journal.differences(&journal, false, names).is_empty());
        let other = JournalIdentity {
            deployment: 8,
            mode: InjectionMode::PreVerified,
            auth: AuthScheme::Eip712,
            engine_semantics: ENGINE_SEMANTICS + 1,
            engine: EngineOptions { order_capacity: 12, id_hash_seed: 1, ..journal.engine },
        };
        let differences = journal.differences(&other, false, names);
        assert_eq!(differences.len(), 6, "{differences:?}");
        assert!(differences[0].contains("deployment: journal 7, configuration 8"));
        assert!(
            differences.contains(&"auth: journal perp, configuration eip712".to_string()),
            "{differences:?}"
        );
        assert!(differences.iter().any(|d| d.starts_with("engine_semantics")));
        let seed = differences.iter().find(|d| d.starts_with("id_hash_seed")).expect("the seed differs");
        assert!(!seed.contains("3735928559") && !seed.contains(" 1"), "no seed values: {seed}");
        let allowed = journal.differences(&other, true, names);
        assert!(!allowed.iter().any(|d| d.starts_with("engine_semantics")), "{allowed:?}");
    }

    #[test]
    fn the_signing_scheme_is_byte_120_and_an_older_header_reads_as_perp() {
        // The EIP-712 scheme (D-033): byte 120 is 1, and the header round-trips.
        let eip712 = SegmentHeader { identity: identity().with_auth(AuthScheme::Eip712), ..header() };
        let bytes = eip712.encode();
        assert_eq!(bytes[120], 1);
        assert_eq!(bytes[121..124], [0; 3]);
        assert_eq!(SegmentHeader::decode(&bytes), HeaderRead::Valid(eip712));
        // The perp scheme writes 0 there, as every header did before the byte had a meaning;
        // so a header written then (all four bytes 120..124 zero) reads as perp.
        let perp = header().encode();
        assert_eq!(perp[120..124], [0; 4]);
        match SegmentHeader::decode(&perp) {
            HeaderRead::Valid(read) => assert_eq!(read.identity.auth, AuthScheme::Perp),
            other => panic!("{other:?}"),
        }
        // Only these two codes; and the three bytes after it are still reserved.
        let resealed = |at: usize, value: u8| {
            let mut bytes = perp;
            bytes[at] = value;
            let crc = crc32c(&bytes[..124]);
            bytes[124..].copy_from_slice(&crc.to_le_bytes());
            SegmentHeader::decode(&bytes)
        };
        assert!(matches!(resealed(120, 2), HeaderRead::Invalid(why) if why.contains("signing scheme 2")));
        assert!(matches!(resealed(123, 1), HeaderRead::Invalid(why) if why.contains("reserved")));
    }

    #[test]
    fn the_worked_records_of_11_3_come_out_byte_for_byte_with_their_crcs() {
        let words_of = |hex: &str| -> Vec<u64> {
            let mut bytes = spec_examples::bytes(hex);
            bytes[4..8].fill(0); // the sequencer's record has no CRC yet
            bytes.chunks(8).map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes"))).collect()
        };
        for (hex, crc) in [(spec_examples::RECORD_1, 0xE5C5_CC1D), (spec_examples::RECORD_2, 0xFF8E_EBC8)] {
            let mut buf = vec![0xEE]; // something before, to show append leaves it alone
            append_record(&mut buf, &words_of(hex));
            assert_eq!(buf[0], 0xEE);
            let record = &buf[1..];
            assert_eq!(record, &spec_examples::bytes(hex)[..]);
            assert_eq!((stored_crc(record), record_crc(record)), (crc, crc));
            assert_eq!(record_len(record) as usize, record.len());
            let decoded = decode_record(record).expect("a valid record");
            assert!(check_contents(&decoded, InjectionMode::Signed).is_ok());
        }
    }

    #[test]
    fn records_must_hold_what_their_kind_allows() {
        let signed = Meta { source: Source::SignedClient, lane: 1, account: 9 };
        let preverified = Meta { source: Source::PreVerifiedClient, lane: 1, account: 9 };
        let mark = Command::SetMark(SetMark { price: 1, market: 3 });
        let check = |record: JournalRecord, mode| check_contents(&record, mode);
        use InjectionMode::{PreVerified, Signed};

        assert_eq!(check(record(signed, 1, &place(9, 1)), Signed), Ok(place(9, 1)));
        assert_eq!(check(record(preverified, 1, &place(9, 1)), PreVerified), Ok(place(9, 1)));
        assert_eq!(check(record(Meta::OPERATOR, 0, &mark), Signed), Ok(mark));
        assert_eq!(check(record(Meta::OPERATOR, 0, &mark), PreVerified), Ok(mark));

        let error = |record, mode| check(record, mode).expect_err("must be refused");
        assert!(error(record(signed, 1, &place(9, 1)), PreVerified).contains("kind-1"));
        assert!(error(record(preverified, 1, &place(9, 1)), Signed).contains("kind-2"));
        assert!(error(record(signed, 1, &place(7, 1)), Signed).contains("doesn't own"));
        let cancel = Command::CancelOrder(CancelOrder { order_id: order_id(8, 2), market: 0 });
        assert!(error(record(signed, 1, &cancel), Signed).contains("doesn't own"));
        assert!(error(record(signed, 1, &mark), Signed).contains("operator command tag 7"));
        assert!(error(record(Meta::OPERATOR, 0, &place(9, 1)), Signed).contains("client command tag 1"));
        let deposit = Command::Deposit(Deposit { amount: 1, account: 9 });
        let busy_operator = Meta { lane: 1, ..Meta::OPERATOR };
        assert!(error(record(busy_operator, 0, &deposit), Signed).contains("lane, account or nonce"));
        assert!(error(record(Meta::OPERATOR, 5, &deposit), Signed).contains("lane, account or nonce"));
        let mut bad_cmd = record(Meta::OPERATOR, 0, &deposit);
        bad_cmd.command[0] |= 1 << 8; // a reserved byte of the head
        assert!(error(bad_cmd, Signed).contains("doesn't decode"));
    }

    #[test]
    fn a_record_with_a_bad_meta_or_length_does_not_decode() {
        let mut buf = Vec::new();
        let meta = Meta { source: Source::PreVerifiedClient, lane: 0, account: 9 };
        append_record(&mut buf, &record(meta, 1, &place(9, 1)).to_words()[..10]);
        assert!(decode_record(&buf).is_some());
        assert!(decode_record(&buf[..72]).is_none(), "too short for its kind");
        assert!(decode_record(&buf[..77]).is_none(), "not whole words");
        let mut reserved = buf.clone();
        reserved[25] = 1;
        assert!(decode_record(&reserved).is_none(), "the reserved byte after the kind");
    }

    #[test]
    fn the_tail_region_is_one_header_and_a_full_batch_of_signed_records() {
        assert_eq!(TAIL_REGION_BYTES, 128 + 4_096 * 152);
    }
}
