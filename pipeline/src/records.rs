//! The records that travel through the rings, word by word (`docs/PIPELINE.md` 3.3 and
//! 10.3): what the sender gives a gateway, what the gateways and the operator queue give
//! the sequencer, what the sequencer gives the journal writer and the core, and the
//! trailer the core adds after each command's events.
//!
//! **Contract.** Each record type has a fixed size in words and converts to and from those
//! words (`to_words`, `from_words`). The word numbers are exactly the tables of 3.3 and
//! 10.3, so a ring slot, a journal record and a capture all mean the same thing. A command
//! travels as its CMD40 words (`codec.rs`), not as an engine `Command`: the gateways decode
//! it once to check it, and the core decodes it once to apply it, but everything in between
//! only copies words.
//!
//! **Stamps.** Every `t_*` field is nanoseconds since the run started (`clock.rs`), and 0
//! means "not applicable" (for example `t_gw_in` in pre-verified mode). The clock never
//! returns 0, so a real stamp is never mistaken for "not applicable".
//!
//! **The meta word** packs where a command came from: bits 0-7 the [`Source`], bits 8-15
//! zero, bits 16-31 the lane, bits 32-63 the account. It is also, bit for bit, word 3 of a
//! journal record (kind, reserved byte, lane, account; 11.3), since a record's kind is its
//! source. So the sequencer copies it unchanged.
//!
//! **Trust.** Records in rings are written by our own code, so `from_words` of a ring
//! record panics on a meta word that can't be ours: that is a bug, and a panic stops the
//! process (2.8). [`JournalRecord::from_words`] returns `None` instead, since recovery
//! reads journal words from disk.

use std::fmt;

use engine::event::{Event, RejectReason};
use engine::types::AccountId;

use crate::codec::{COMMAND_WORDS, EVENT_WORDS, encode_event, reject_reason_code, reject_reason_from_code};

/// Bytes in a client message (5.1): a 72-byte signed part and a 64-byte signature.
pub const MESSAGE_BYTES: usize = 136;
/// Words in a client message: word `k` is bytes `8k..8k+8`, little-endian.
pub const MESSAGE_WORDS: usize = 17;
/// Words in a signature `r || s`: message bytes 72..136, which are message words 9 to 16.
pub const SIGNATURE_WORDS: usize = 8;
/// The first message word of the signature.
const SIGNATURE_FIRST_WORD: usize = 9;

/// Where a command came from. The codes are also the journal's record kinds (11.3).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// A signed client message, checked and verified by a gateway.
    SignedClient = 1,
    /// A client command from the pre-verified sender, which skips the gateways (14.11).
    PreVerifiedClient = 2,
    /// An operator command, from the privileged operator queue (section 8).
    Operator = 3,
}

impl Source {
    pub const fn code(self) -> u8 {
        self as u8
    }

    pub const fn from_code(code: u8) -> Option<Source> {
        match code {
            1 => Some(Source::SignedClient),
            2 => Some(Source::PreVerifiedClient),
            3 => Some(Source::Operator),
            _ => None,
        }
    }
}

/// The two ways client commands enter the pipeline (14.11). Also a journal's mode, byte 10
/// of its segment header (11.2): its code is the code of the only client [`Source`] it
/// may hold.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InjectionMode {
    /// Signed messages through the gateways.
    Signed = 1,
    /// Commands straight into the sequencer's lanes, with no signatures.
    PreVerified = 2,
}

impl InjectionMode {
    pub const fn code(self) -> u8 {
        self as u8
    }

    pub const fn from_code(code: u8) -> Option<InjectionMode> {
        match code {
            1 => Some(InjectionMode::Signed),
            2 => Some(InjectionMode::PreVerified),
            _ => None,
        }
    }

    /// The source of every client command in this mode.
    pub const fn client_source(self) -> Source {
        match self {
            InjectionMode::Signed => Source::SignedClient,
            InjectionMode::PreVerified => Source::PreVerifiedClient,
        }
    }
}

/// How clients sign their messages (`docs/DECISIONS.md` D-033). Also byte 120 of a
/// journal's segment header (11.2), since the signature audit (13.4) must know how to
/// check a kind-1 record again. Byte 120 was reserved, and zero, before the second scheme
/// existed, so an older journal reads as [`AuthScheme::Perp`].
///
/// Both schemes journal a kind-1 record in the same 152 bytes (11.3); only what its nonce
/// and expiry words mean differs.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuthScheme {
    /// Our own scheme (D-021, D-022): the 72 signed bytes, SHA-256, verified with the
    /// account's registered key; a nonce per account and an expiry. The record's words hold
    /// the nonce and `expires_at`.
    Perp = 0,
    /// Polymarket Perps' scheme (D-033): EIP-712 over keccak-256 of the MessagePack-encoded
    /// command, the signer recovered and compared with the account's address; a salt and a
    /// timestamp in milliseconds. The record's nonce word holds the salt, and its expiry word
    /// the timestamp.
    Eip712 = 1,
}

impl AuthScheme {
    /// Both schemes, `Perp` first.
    pub const ALL: [AuthScheme; 2] = [AuthScheme::Perp, AuthScheme::Eip712];

    pub const fn code(self) -> u8 {
        self as u8
    }

    pub const fn from_code(code: u8) -> Option<AuthScheme> {
        match code {
            0 => Some(AuthScheme::Perp),
            1 => Some(AuthScheme::Eip712),
            _ => None,
        }
    }

    /// The name the command line, the summaries and the reports use.
    pub const fn name(self) -> &'static str {
        match self {
            AuthScheme::Perp => "perp",
            AuthScheme::Eip712 => "eip712",
        }
    }

    /// The scheme called `name` ([`AuthScheme::name`]).
    pub fn from_name(name: &str) -> Result<AuthScheme, String> {
        AuthScheme::ALL
            .into_iter()
            .find(|scheme| scheme.name() == name)
            .ok_or_else(|| format!("not a signing scheme: {name:?} (perp, eip712)"))
    }
}

impl fmt::Display for AuthScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Where a command came from: the meta word of 3.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Meta {
    pub source: Source,
    /// The lane `g` (0 for operator commands).
    pub lane: u16,
    /// The signer (0 for operator commands).
    pub account: AccountId,
}

impl Meta {
    /// Every operator command's meta: lane 0, account 0.
    pub const OPERATOR: Meta = Meta { source: Source::Operator, lane: 0, account: AccountId::new(0) };

    pub const fn pack(self) -> u64 {
        self.source.code() as u64 | (self.lane as u64) << 16 | (self.account.get() as u64) << 32
    }

    /// `None` if the source code is unknown or bits 8-15 are not zero.
    pub const fn unpack(word: u64) -> Option<Meta> {
        let Some(source) = Source::from_code(word as u8) else { return None };
        if (word >> 8) as u8 != 0 {
            return None;
        }
        Some(Meta { source, lane: (word >> 16) as u16, account: AccountId::new((word >> 32) as u32) })
    }

    /// A meta word read from a ring, which only our own code writes (module docs, "Trust").
    fn from_ring(word: u64) -> Meta {
        Meta::unpack(word)
            .unwrap_or_else(|| panic!("bad meta word {word:#018x} in a ring: ring records are ours"))
    }
}

/// The little-endian words of a client message.
pub fn message_words(message: &[u8; MESSAGE_BYTES]) -> [u64; MESSAGE_WORDS] {
    crate::codec::from_le_bytes(message)
}

/// The signature `r || s` of a client message (bytes 72..136), as eight words.
pub fn signature_words(message: &[u8; MESSAGE_BYTES]) -> [u64; SIGNATURE_WORDS] {
    let words = message_words(message);
    std::array::from_fn(|i| words[SIGNATURE_FIRST_WORD + i])
}

/// Sender → gateway, 19 words (3.3): the message as it was sent, and two stamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IngressSlot {
    pub message: [u8; MESSAGE_BYTES],
    pub t_sched: u64,
    pub t_sent: u64,
}

impl IngressSlot {
    pub const WORDS: usize = 19;

    pub fn to_words(&self) -> [u64; Self::WORDS] {
        let message = message_words(&self.message);
        let mut words = [0; Self::WORDS];
        words[..MESSAGE_WORDS].copy_from_slice(&message);
        words[17] = self.t_sched;
        words[18] = self.t_sent;
        words
    }

    pub fn from_words(words: &[u64; Self::WORDS]) -> IngressSlot {
        let message: [u64; MESSAGE_WORDS] = std::array::from_fn(|k| words[k]);
        IngressSlot { message: crate::codec::to_le_bytes(&message), t_sched: words[17], t_sent: words[18] }
    }
}

/// A client command for the sequencer, 20 words (3.3): from gateway `g` in signed mode, or
/// from the pre-verified sender, where `expires_at`, the signature, `t_gw_in` and
/// `t_gw_out` are all zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientRecord {
    /// Source 1 or 2, lane `g`, the account.
    pub meta: Meta,
    pub nonce: u64,
    /// CMD40.
    pub command: [u64; COMMAND_WORDS],
    pub expires_at: u64,
    /// `r || s`: message bytes 72..136 ([`signature_words`]).
    pub signature: [u64; SIGNATURE_WORDS],
    pub t_sched: u64,
    pub t_sent: u64,
    pub t_gw_in: u64,
    pub t_gw_out: u64,
}

impl ClientRecord {
    pub const WORDS: usize = 20;

    pub fn to_words(&self) -> [u64; Self::WORDS] {
        let mut words = [0; Self::WORDS];
        words[0] = self.meta.pack();
        words[1] = self.nonce;
        words[2..7].copy_from_slice(&self.command);
        words[7] = self.expires_at;
        words[8..16].copy_from_slice(&self.signature);
        words[16] = self.t_sched;
        words[17] = self.t_sent;
        words[18] = self.t_gw_in;
        words[19] = self.t_gw_out;
        words
    }

    pub fn from_words(words: &[u64; Self::WORDS]) -> ClientRecord {
        ClientRecord {
            meta: Meta::from_ring(words[0]),
            nonce: words[1],
            command: std::array::from_fn(|i| words[2 + i]),
            expires_at: words[7],
            signature: std::array::from_fn(|i| words[8 + i]),
            t_sched: words[16],
            t_sent: words[17],
            t_gw_in: words[18],
            t_gw_out: words[19],
        }
    }
}

/// An operator command for the sequencer, 8 words (3.3). Its meta word is always
/// [`Meta::OPERATOR`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperatorRecord {
    /// CMD40.
    pub command: [u64; COMMAND_WORDS],
    pub t_sched: u64,
    pub t_sent: u64,
}

impl OperatorRecord {
    pub const WORDS: usize = 8;

    pub fn to_words(&self) -> [u64; Self::WORDS] {
        let mut words = [0; Self::WORDS];
        words[0] = Meta::OPERATOR.pack();
        words[1..6].copy_from_slice(&self.command);
        words[6] = self.t_sched;
        words[7] = self.t_sent;
        words
    }

    pub fn from_words(words: &[u64; Self::WORDS]) -> OperatorRecord {
        assert_eq!(Meta::from_ring(words[0]), Meta::OPERATOR, "an operator record with another meta word");
        OperatorRecord { command: std::array::from_fn(|i| words[1 + i]), t_sched: words[6], t_sent: words[7] }
    }
}

/// Whether the pipeline stamps each command (15.1). `Off` is a one-off measurement of what
/// the stamps cost: the sequencer writes only the first line of each [`CoreRecord`], the
/// core reads no clock per command and writes trailers with zero timings, and the gate
/// records no latencies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stamps {
    On,
    Off,
}

/// Sequencer → core, 12 words (3.3): the command's seq, where it came from, the command,
/// and its stamps so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoreRecord {
    pub seq: u64,
    /// As the sequencer received it.
    pub meta: Meta,
    /// CMD40.
    pub command: [u64; COMMAND_WORDS],
    pub t_sched: u64,
    pub t_sent: u64,
    pub t_gw_in: u64,
    pub t_gw_out: u64,
    pub t_seq: u64,
}

impl CoreRecord {
    pub const WORDS: usize = 12;
    /// With `--stamps off` (15.1) only the first line is written and read: `seq`, meta and
    /// the command. Taking these words alone from [`CoreRecord::to_words`] is that record.
    pub const WORDS_WITHOUT_STAMPS: usize = 7;

    pub fn to_words(&self) -> [u64; Self::WORDS] {
        let mut words = [0; Self::WORDS];
        words[0] = self.seq;
        words[1] = self.meta.pack();
        words[2..7].copy_from_slice(&self.command);
        words[7] = self.t_sched;
        words[8] = self.t_sent;
        words[9] = self.t_gw_in;
        words[10] = self.t_gw_out;
        words[11] = self.t_seq;
        words
    }

    /// The words the sequencer writes and the core reads: all 12, or the first 7 with
    /// stamps off.
    pub const fn words_for(stamps: Stamps) -> usize {
        match stamps {
            Stamps::On => Self::WORDS,
            Stamps::Off => Self::WORDS_WITHOUT_STAMPS,
        }
    }

    /// Reads a record back from its words. With stamps off only the first 7 words were
    /// written, and the stamps words hold whatever the slot held before: callers ignore them.
    pub fn from_words(words: &[u64; Self::WORDS]) -> CoreRecord {
        CoreRecord {
            seq: words[0],
            meta: Meta::from_ring(words[1]),
            command: std::array::from_fn(|i| words[2 + i]),
            t_sched: words[7],
            t_sent: words[8],
            t_gw_in: words[9],
            t_gw_out: words[10],
            t_seq: words[11],
        }
    }

    /// Words in the "verify on core" ablation's 3-line core record (3.3, section 16): the
    /// 12 above, then [`SignedFields`].
    pub const ABLATION_WORDS: usize = Self::WORDS + SignedFields::WORDS;
}

/// What the "verify on core" ablation adds after a [`CoreRecord`]'s 12 words (3.3, section
/// 16), so that the core can rebuild the signed bytes and check the signature itself. Both
/// arms of the ablation carry these words (zero for operator and pre-verified commands), so
/// the record's size is not a hidden difference between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignedFields {
    pub nonce: u64,
    pub expires_at: u64,
    /// `r || s`, as the client sent it.
    pub signature: [u64; SIGNATURE_WORDS],
}

impl SignedFields {
    pub const WORDS: usize = 2 + SIGNATURE_WORDS;

    pub fn to_words(&self) -> [u64; Self::WORDS] {
        let mut words = [0; Self::WORDS];
        words[0] = self.nonce;
        words[1] = self.expires_at;
        words[2..].copy_from_slice(&self.signature);
        words
    }

    pub fn from_words(words: &[u64; Self::WORDS]) -> SignedFields {
        SignedFields {
            nonce: words[0],
            expires_at: words[1],
            signature: std::array::from_fn(|i| words[2 + i]),
        }
    }
}

/// Sequencer → journal writer: exactly the on-disk record of 11.3, as 10 words (80 bytes,
/// kinds 2 and 3) or 19 words (152 bytes, kind 1), with the CRC left 0 for the writer to
/// fill in.
///
/// | Word | Content |
/// |---|---|
/// | 0 | `len` (bytes 0..4), `crc` (bytes 4..8) |
/// | 1 | `seq` |
/// | 2 | `ts`: nanoseconds since the UNIX epoch |
/// | 3 | the meta word: kind, reserved byte, lane, account |
/// | 4 | `nonce` (the salt, in a journal of the EIP-712 scheme: [`AuthScheme`]) |
/// | 5-9 | the command, CMD40 |
/// | 10 | kind 1 only: `expires_at` (the timestamp in milliseconds, in the EIP-712 scheme) |
/// | 11-18 | kind 1 only: the signature `r \|\| s` |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalRecord {
    pub seq: u64,
    pub ts: u64,
    /// The kind is `meta.source`.
    pub meta: Meta,
    /// 0 for operator commands.
    pub nonce: u64,
    /// CMD40.
    pub command: [u64; COMMAND_WORDS],
    /// Kind 1 only; ignored (and not written) for kinds 2 and 3.
    pub expires_at: u64,
    /// Kind 1 only; ignored (and not written) for kinds 2 and 3.
    pub signature: [u64; SIGNATURE_WORDS],
}

impl JournalRecord {
    /// Words in a kind-1 (signed client) record.
    pub const SIGNED_WORDS: usize = 19;
    /// Words in a kind-2 or kind-3 record.
    pub const UNSIGNED_WORDS: usize = 10;
    pub const SIGNED_BYTES: u32 = 152;
    pub const UNSIGNED_BYTES: u32 = 80;

    /// The record's length in bytes, the `len` field: 152 for kind 1, 80 otherwise.
    pub const fn len_bytes(&self) -> u32 {
        match self.meta.source {
            Source::SignedClient => Self::SIGNED_BYTES,
            Source::PreVerifiedClient | Source::Operator => Self::UNSIGNED_BYTES,
        }
    }

    /// The record's length in words.
    pub const fn len_words(&self) -> usize {
        self.len_bytes() as usize / 8
    }

    /// The `len` field of a record whose first word is `word0`, so a reader can tell how
    /// many words to read before it reads them.
    pub const fn len_of(word0: u64) -> u32 {
        word0 as u32
    }

    /// The record's words, with the CRC field 0. Only the first [`JournalRecord::len_words`]
    /// are the record; the rest are zero.
    pub fn to_words(&self) -> [u64; Self::SIGNED_WORDS] {
        let mut words = [0; Self::SIGNED_WORDS];
        words[0] = u64::from(self.len_bytes());
        words[1] = self.seq;
        words[2] = self.ts;
        words[3] = self.meta.pack();
        words[4] = self.nonce;
        words[5..10].copy_from_slice(&self.command);
        if self.meta.source == Source::SignedClient {
            words[10] = self.expires_at;
            words[11..19].copy_from_slice(&self.signature);
        }
        words
    }

    /// Reads a record back from its words, ignoring the CRC field. `None` if the meta word
    /// is not valid, or if `len`, the kind and `words.len()` don't agree. Checks nothing
    /// else: the rules of 11.8 (seq, tags against kinds, timestamps) are recovery's.
    pub fn from_words(words: &[u64]) -> Option<JournalRecord> {
        if words.len() < Self::UNSIGNED_WORDS {
            return None;
        }
        let meta = Meta::unpack(words[3])?;
        let record = JournalRecord {
            seq: words[1],
            ts: words[2],
            meta,
            nonce: words[4],
            command: std::array::from_fn(|i| words[5 + i]),
            expires_at: 0,
            signature: [0; SIGNATURE_WORDS],
        };
        if Self::len_of(words[0]) != record.len_bytes() || words.len() != record.len_words() {
            return None;
        }
        if meta.source == Source::SignedClient {
            return Some(JournalRecord {
                expires_at: words[10],
                signature: std::array::from_fn(|i| words[11 + i]),
                ..record
            });
        }
        Some(record)
    }
}

/// Words in an event-ring slot (3.3): the command's `seq`, then an EVT56 event or a trailer.
pub const EVENT_SLOT_WORDS: usize = 8;

/// The tag in byte 0 of a trailer's second word. Engine events use 1 to 14.
pub const TRAILER_TAG: u8 = 255;

/// An event-ring slot holding an engine event: `seq`, then the event's EVT56 words.
pub fn event_slot(seq: u64, event: &Event) -> [u64; EVENT_SLOT_WORDS] {
    let event_words: [u64; EVENT_WORDS] = encode_event(event);
    let mut slot = [0; EVENT_SLOT_WORDS];
    slot[0] = seq;
    slot[1..].copy_from_slice(&event_words);
    slot
}

/// True if an event-ring slot holds a trailer rather than an engine event.
pub fn is_trailer(slot: &[u64; EVENT_SLOT_WORDS]) -> bool {
    slot[1] as u8 == TRAILER_TAG
}

/// A trailer's outcome byte for a command whose first event is `first_event`: 0 if the
/// command was accepted, else its `RejectReason` code + 1 (10.2, 10.3).
pub fn outcome(first_event: &Event) -> u8 {
    match first_event {
        Event::Reject(reject) => reject_reason_code(reject.reason) + 1,
        _ => 0,
    }
}

/// The reject reason an outcome byte names; `None` for 0 (accepted) or a byte that names
/// no reason.
pub fn outcome_reject_reason(outcome: u8) -> Option<RejectReason> {
    reject_reason_from_code(outcome.checked_sub(1)?)
}

/// The slot the core writes after each command's events (10.3): it marks the end of the
/// command and carries its timings to the gate. Never an engine event, and never captured.
///
/// | Word | Content |
/// |---|---|
/// | 0 | `seq` |
/// | 1 | bytes: `[0]` 255, `[1]` source, `[2]` command tag, `[3]` outcome, `[4..6]` lane, `[6..8]` events |
/// | 2-7 | `t_sched`, `t_sent`, `t_gw_in`, `t_gw_out`, `t_seq`, `t_done` |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trailer {
    pub seq: u64,
    pub source: Source,
    /// The command's CMD40 tag.
    pub command_tag: u8,
    /// 0 accepted, else `RejectReason` code + 1 ([`outcome`]).
    pub outcome: u8,
    pub lane: u16,
    /// The command's events, not counting the trailer; saturates at 65,535
    /// ([`Trailer::saturating_events`]).
    pub events: u16,
    pub t_sched: u64,
    pub t_sent: u64,
    pub t_gw_in: u64,
    pub t_gw_out: u64,
    pub t_seq: u64,
    pub t_done: u64,
}

impl Trailer {
    /// The events field for a command that emitted `count` events.
    pub fn saturating_events(count: u64) -> u16 {
        u16::try_from(count).unwrap_or(u16::MAX)
    }

    pub fn to_words(&self) -> [u64; EVENT_SLOT_WORDS] {
        let info = u64::from(TRAILER_TAG)
            | u64::from(self.source.code()) << 8
            | u64::from(self.command_tag) << 16
            | u64::from(self.outcome) << 24
            | u64::from(self.lane) << 32
            | u64::from(self.events) << 48;
        [self.seq, info, self.t_sched, self.t_sent, self.t_gw_in, self.t_gw_out, self.t_seq, self.t_done]
    }

    /// Panics if the slot is not a trailer (check [`is_trailer`] first) or names an unknown
    /// source: the event ring is written only by the core.
    pub fn from_words(words: &[u64; EVENT_SLOT_WORDS]) -> Trailer {
        assert!(is_trailer(words), "slot {words:?} is not a trailer");
        let info = words[1];
        let source = (info >> 8) as u8;
        Trailer {
            seq: words[0],
            source: Source::from_code(source).unwrap_or_else(|| panic!("a trailer with source {source}")),
            command_tag: (info >> 16) as u8,
            outcome: (info >> 24) as u8,
            lane: (info >> 32) as u16,
            events: (info >> 48) as u16,
            t_sched: words[2],
            t_sent: words[3],
            t_gw_in: words[4],
            t_gw_out: words[5],
            t_seq: words[6],
            t_done: words[7],
        }
    }
}

/// The worked examples of PIPELINE.md 11.3, for the tests here and in `crc32c.rs`.
#[cfg(test)]
pub(crate) mod spec_examples {
    /// Worked example 1: the message of 5.6 as a kind-1 record, seq 5,000, lane 1.
    pub const RECORD_1: &str = "
        98 00 00 00 1d cc c5 e5 88 13 00 00 00 00 00 00
        15 cd 4e 2b 84 5b d7 18 01 00 01 00 09 00 00 00
        01 00 00 00 00 00 00 00 01 00 00 01 03 00 00 00
        01 00 00 00 09 00 00 00 56 92 01 00 00 00 00 00
        20 a1 07 00 00 00 00 00 00 00 00 00 00 00 00 00
        00 ac 16 20 8b 5b d7 18 96 29 36 e1 f0 2f 1c 30
        22 21 2d f5 bb 4c 09 03 4b d8 1f b4 9f 5a ca 9e
        04 d8 26 e8 f0 6c 1d d5 75 35 21 c3 a0 eb 5f 3a
        8a e8 5e 6e cf ad 67 2b f6 54 3d 71 8e 16 9e 07
        6d 2c 1f d8 cb 60 70 10";

    /// Worked example 2: `SetMark { market: 3, price: 103,001 }` as seq 5,001, 2 µs later.
    pub const RECORD_2: &str = "
        50 00 00 00 c8 eb 8e ff 89 13 00 00 00 00 00 00
        e5 d4 4e 2b 84 5b d7 18 03 00 00 00 00 00 00 00
        00 00 00 00 00 00 00 00 07 00 00 00 03 00 00 00
        59 92 01 00 00 00 00 00 00 00 00 00 00 00 00 00
        00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00";

    /// The bytes of a hex dump: pairs of hex digits separated by white space.
    pub fn bytes(hex: &str) -> Vec<u8> {
        hex.split_whitespace().map(|pair| u8::from_str_radix(pair, 16).expect("a hex byte")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{command_tag, encode_command, to_le_bytes};
    use engine::command::{CancelOrder, Command, PlaceOrder, SetMark};
    use engine::event::{Ack, Reject};
    use engine::types::{AccountId, MarketId, OrderId, OrderSeq, Price, Qty, Side, TimeInForce, order_id};

    fn place_9_1() -> [u64; COMMAND_WORDS] {
        encode_command(&Command::PlaceOrder(PlaceOrder {
            order_id: order_id(AccountId::new(9), OrderSeq::new(1)),
            price: Price::new(102_998),
            qty: Qty::new(500_000),
            market: MarketId::new(3),
            side: Side::Buy,
            tif: TimeInForce::Gtc,
            post_only: true,
        }))
    }

    /// `r || s` of the worked example (5.6), as the message carries it.
    fn signature_5_6() -> [u64; SIGNATURE_WORDS] {
        let hex = "962936e1f02f1c3022212df5bb4c09034bd81fb49f5aca9e04d826e8f06c1dd5\
                   753521c3a0eb5f3a8ae85e6ecfad672bf6543d718e169e076d2c1fd8cb607010";
        let bytes: Vec<u8> =
            (0..64).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("a hex byte")).collect();
        crate::codec::from_le_bytes::<64, 8>(&bytes.try_into().expect("64 bytes"))
    }

    #[test]
    fn the_meta_word_has_the_specs_bit_positions() {
        let meta = Meta { source: Source::SignedClient, lane: 1, account: AccountId::new(9) };
        assert_eq!(meta.pack(), 0x0000_0009_0001_0001);
        assert_eq!(Meta::unpack(meta.pack()), Some(meta));
        let extreme =
            Meta { source: Source::PreVerifiedClient, lane: u16::MAX, account: AccountId::new(u32::MAX) };
        assert_eq!(extreme.pack(), 0xFFFF_FFFF_FFFF_0002);
        assert_eq!(Meta::unpack(extreme.pack()), Some(extreme));
        assert_eq!(Meta::OPERATOR.pack(), 3);
    }

    #[test]
    fn a_meta_word_with_an_unknown_source_or_a_nonzero_reserved_byte_is_refused() {
        assert_eq!(Meta::unpack(0), None);
        assert_eq!(Meta::unpack(4), None);
        assert_eq!(Meta::unpack(0xFF), None);
        assert_eq!(Meta::unpack(0x0100 | 1), None, "bits 8-15 must be zero");
        for code in 1..=3 {
            assert_eq!(Source::from_code(code).map(Source::code), Some(code));
        }
        for mode in [InjectionMode::Signed, InjectionMode::PreVerified] {
            assert_eq!(InjectionMode::from_code(mode.code()), Some(mode));
            assert_eq!(mode.client_source().code(), mode.code(), "a journal's mode is its client kind");
        }
        assert_eq!(InjectionMode::from_code(3), None);
    }

    #[test]
    fn auth_schemes_round_trip_by_code_and_by_name() {
        for scheme in AuthScheme::ALL {
            assert_eq!(AuthScheme::from_code(scheme.code()), Some(scheme));
            assert_eq!(AuthScheme::from_name(scheme.name()), Ok(scheme));
            assert_eq!(scheme.to_string(), scheme.name());
        }
        assert_eq!(AuthScheme::Perp.code(), 0, "what a header written before the byte existed holds");
        assert_eq!(AuthScheme::from_code(2), None);
        assert!(AuthScheme::from_name("EIP712").unwrap_err().contains("perp, eip712"));
    }

    #[test]
    #[should_panic(expected = "ring records are ours")]
    fn a_ring_record_with_a_bad_meta_word_panics() {
        let mut words = [0; CoreRecord::WORDS];
        words[1] = 7;
        CoreRecord::from_words(&words);
    }

    #[test]
    fn an_ingress_slot_carries_the_message_as_little_endian_words() {
        let message: [u8; MESSAGE_BYTES] = std::array::from_fn(|i| i as u8);
        let slot = IngressSlot { message, t_sched: 1_000_000, t_sent: 1_000_150 };
        let words = slot.to_words();
        // Word k is bytes 8k..8k+8, little-endian: word 1 is bytes 8..16.
        assert_eq!(words[1], u64::from_le_bytes([8, 9, 10, 11, 12, 13, 14, 15]));
        assert_eq!(words[16], u64::from_le_bytes([128, 129, 130, 131, 132, 133, 134, 135]));
        assert_eq!((words[17], words[18]), (1_000_000, 1_000_150));
        assert_eq!(IngressSlot::from_words(&words), slot);
        // The signature is message bytes 72..136: words 9 to 16.
        assert_eq!(signature_words(&message), std::array::from_fn(|i| words[9 + i]));
        assert_eq!(signature_words(&message)[0], u64::from_le_bytes([72, 73, 74, 75, 76, 77, 78, 79]));
    }

    #[test]
    fn a_client_record_round_trips_and_has_the_specs_word_numbers() {
        let record = ClientRecord {
            meta: Meta { source: Source::SignedClient, lane: 1, account: AccountId::new(9) },
            nonce: 1,
            command: place_9_1(),
            expires_at: u64::MAX,
            signature: signature_5_6(),
            t_sched: 1_000_000,
            t_sent: 1_000_150,
            t_gw_in: 1_000_400,
            t_gw_out: 1_051_300,
        };
        let words = record.to_words();
        assert_eq!(words.len(), 20);
        assert_eq!(words[0], record.meta.pack());
        assert_eq!(words[1], 1);
        assert_eq!(words[2..7], place_9_1());
        assert_eq!(words[7], u64::MAX);
        assert_eq!(words[8..16], signature_5_6());
        assert_eq!(words[16..20], [1_000_000, 1_000_150, 1_000_400, 1_051_300]);
        assert_eq!(ClientRecord::from_words(&words), record);
    }

    #[test]
    fn the_ablation_record_is_the_core_record_then_the_signed_fields() {
        let fields = SignedFields { nonce: 1, expires_at: u64::MAX, signature: signature_5_6() };
        let words = fields.to_words();
        assert_eq!(words[..2], [1, u64::MAX]);
        assert_eq!(words[2..], signature_5_6());
        assert_eq!(SignedFields::from_words(&words), fields);
        assert_eq!(CoreRecord::ABLATION_WORDS, 22, "3 lines of 8 words hold it (3.3)");
    }

    #[test]
    fn an_operator_record_round_trips_with_the_operator_meta_word() {
        let command = encode_command(&Command::SetMark(SetMark {
            price: Price::new(103_001),
            market: MarketId::new(3),
        }));
        let record = OperatorRecord { command, t_sched: 5, t_sent: 6 };
        let words = record.to_words();
        assert_eq!(words[0], 3, "source 3, lane 0, account 0");
        assert_eq!(words[1..6], command);
        assert_eq!(words[6..8], [5, 6]);
        assert_eq!(OperatorRecord::from_words(&words), record);
    }

    #[test]
    fn a_core_record_round_trips_and_its_first_line_is_the_stampless_record() {
        let record = CoreRecord {
            seq: 5_000,
            meta: Meta { source: Source::SignedClient, lane: 1, account: AccountId::new(9) },
            command: place_9_1(),
            t_sched: 1,
            t_sent: 2,
            t_gw_in: 3,
            t_gw_out: 4,
            t_seq: u64::MAX,
        };
        let words = record.to_words();
        assert_eq!(words.len(), 12);
        // The first line alone: seq, meta and the command.
        assert_eq!(CoreRecord::WORDS_WITHOUT_STAMPS, 7);
        assert_eq!(words[..2], [5_000, record.meta.pack()]);
        assert_eq!(words[2..7], place_9_1());
        assert_eq!(words[7..12], [1, 2, 3, 4, u64::MAX]);
        assert_eq!(CoreRecord::from_words(&words), record);
    }

    #[test]
    fn the_worked_journal_records_encode_to_the_specs_bytes() {
        let signed = JournalRecord {
            seq: 5_000,
            ts: 1_790_000_000_123_456_789,
            meta: Meta { source: Source::SignedClient, lane: 1, account: AccountId::new(9) },
            nonce: 1,
            command: place_9_1(),
            expires_at: 1_790_000_030_000_000_000,
            signature: signature_5_6(),
        };
        let operator = JournalRecord {
            seq: 5_001,
            ts: 1_790_000_000_123_458_789,
            meta: Meta::OPERATOR,
            nonce: 0,
            command: encode_command(&Command::SetMark(SetMark {
                price: Price::new(103_001),
                market: MarketId::new(3),
            })),
            expires_at: 0,
            signature: [0; SIGNATURE_WORDS],
        };
        for (record, hex) in [(signed, spec_examples::RECORD_1), (operator, spec_examples::RECORD_2)] {
            let mut expected = spec_examples::bytes(hex);
            assert_eq!(expected.len(), record.len_bytes() as usize);
            // The ring record leaves the CRC (bytes 4..8) for the writer.
            expected[4..8].fill(0);
            let words = record.to_words();
            let bytes: [u8; 8 * JournalRecord::SIGNED_WORDS] = to_le_bytes(&words);
            assert_eq!(bytes[..expected.len()], expected[..], "seq {}", record.seq);
            assert_eq!(JournalRecord::len_of(words[0]), record.len_bytes());
            assert_eq!(JournalRecord::from_words(&words[..record.len_words()]), Some(record));
        }
        assert_eq!((signed.len_words(), operator.len_words()), (19, 10));
    }

    #[test]
    fn a_journal_record_whose_length_disagrees_with_its_kind_is_refused() {
        let record = JournalRecord {
            seq: 1,
            ts: 2,
            meta: Meta { source: Source::PreVerifiedClient, lane: 0, account: AccountId::new(4) },
            nonce: 3,
            command: encode_command(&Command::CancelOrder(CancelOrder {
                order_id: order_id(AccountId::new(4), OrderSeq::new(1)),
                market: MarketId::new(0),
            })),
            expires_at: 99,
            signature: [7; SIGNATURE_WORDS],
        };
        let words = record.to_words();
        assert_eq!(words[10..], [0; 9], "kind 2 writes no expiry and no signature");
        let back = JournalRecord::from_words(&words[..10]).expect("a valid kind-2 record");
        assert_eq!((back.expires_at, back.signature), (0, [0; SIGNATURE_WORDS]));
        assert_eq!(JournalRecord::from_words(&words), None, "19 words for kind 2");
        assert_eq!(JournalRecord::from_words(&words[..3]), None, "too short");
        let mut wrong_len = words;
        wrong_len[0] = 152;
        assert_eq!(JournalRecord::from_words(&wrong_len[..10]), None, "len 152 for kind 2");
        let mut bad_meta = words;
        bad_meta[3] = 0;
        assert_eq!(JournalRecord::from_words(&bad_meta[..10]), None, "kind 0");
    }

    #[test]
    fn a_trailer_round_trips_and_is_never_mistaken_for_an_event() {
        let trailer = Trailer {
            seq: 5_000,
            source: Source::SignedClient,
            command_tag: command_tag(&Command::PlaceOrder(PlaceOrder {
                order_id: OrderId::new(1),
                price: Price::new(1),
                qty: Qty::new(1),
                market: MarketId::new(0),
                side: Side::Sell,
                tif: TimeInForce::Ioc,
                post_only: false,
            })),
            outcome: 10,
            lane: u16::MAX,
            events: 3,
            t_sched: 1,
            t_sent: 2,
            t_gw_in: 3,
            t_gw_out: 4,
            t_seq: 5,
            t_done: u64::MAX,
        };
        let words = trailer.to_words();
        assert_eq!(words[1], 0x0003_FFFF_0A01_01FF, "tag 255, source 1, tag 1, outcome 10, lane, 3 events");
        assert!(is_trailer(&words));
        assert_eq!(Trailer::from_words(&words), trailer);

        let slot = event_slot(5_000, &Event::Ack(Ack { order_id: OrderId::new(7) }));
        assert_eq!(slot[..2], [5_000, 1]);
        assert!(!is_trailer(&slot));
    }

    #[test]
    fn the_outcome_is_the_reject_reason_plus_one() {
        assert_eq!(outcome(&Event::Ack(Ack { order_id: OrderId::new(1) })), 0);
        let reject =
            |reason| Event::Reject(Reject { order_id: OrderId::new(1), account: AccountId::new(1), reason });
        assert_eq!(outcome(&reject(RejectReason::InvalidPrice)), 1);
        assert_eq!(outcome(&reject(RejectReason::InsufficientMargin)), 10);
        assert_eq!(outcome(&reject(RejectReason::NoRiskTiers)), 19);
        assert_eq!(outcome_reject_reason(0), None);
        assert_eq!(outcome_reject_reason(10), Some(RejectReason::InsufficientMargin));
        assert_eq!(outcome_reject_reason(19), Some(RejectReason::NoRiskTiers));
        assert_eq!(outcome_reject_reason(20), None);
    }

    #[test]
    fn the_event_count_saturates() {
        assert_eq!(Trailer::saturating_events(0), 0);
        assert_eq!(Trailer::saturating_events(65_535), 65_535);
        assert_eq!(Trailer::saturating_events(65_536), 65_535);
        assert_eq!(Trailer::saturating_events(u64::MAX), 65_535);
    }
}
