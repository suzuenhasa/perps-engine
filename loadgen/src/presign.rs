//! Every client message built before the run starts: signed, for signed runs; compact and
//! unsigned, for pre-verified runs (`docs/PIPELINE.md` 14.8, 14.11; `docs/DECISIONS.md`
//! D-027).
//!
//! **Why before the run.** Signing costs tens of microseconds a message; done while
//! sending, it would cap the offered rate far below what the pipeline is measured at. So
//! [`presign`] encodes every client item of the plan (phases B1, B2 and the timed flow)
//! into its 136-byte message (5.1), with `expires_at = u64::MAX`, and signs it, and the
//! sender only copies words.
//!
//! **Contract.**
//! - Either flow's plan: the M3 flow's or the Polymarket-shaped one (D-034), through its
//!   config's [`PlanConfig`] (its seed, digest and client accounts).
//! - [`Arena`]: one signed message per client item, in plan order ([`FlowPlan::client_items`]),
//!   each as the 17 little-endian words the ingress ring carries (3.3). 136 bytes a message;
//!   with its 8-byte send time (`schedule.rs`), 144 bytes: 0.94 GB for the 6.5M messages of
//!   the 100k/s headline run.
//! - Signing is split across threads, each taking a contiguous slice. `k256` signs
//!   deterministically (RFC 6979), so the split doesn't change a byte: the same plan and
//!   deployment give the same arena, whatever the thread count.
//! - Every signature is low-S (5.3): `k256` normalises it, and a debug assertion checks it
//!   against the gateway's own constant.
//! - [`CompactArena`]: for pre-verified runs, where nothing is signed, one [`CompactItem`]
//!   per client item: the account, the nonce and the command's 5 words, 56 bytes (64 with
//!   the send time). The core-path search probes rates in the millions, so this matters: a
//!   3.2M/s probe of 25 s needs 80M items, 5.1 GB instead of 11 GB.
//! - **Reused as a prefix.** Each run starts a fresh engine, journal and nonce state, so
//!   the same messages are valid again, and a run that needs fewer uses the first ones. That
//!   is safe only because these are test keys under a benchmark deployment id (5.1).
//!
//! **The EIP-712 scheme** (5.8; D-033: Polymarket Perps' own). [`presign_eip712`] signs the
//! same items as version-2 messages (`gateway::wire`, "Version 2"): the salt is the item's
//! nonce, which is unique per account (D-033: the plan, its digest and its pinned tests stay
//! as they are), and `ts` is one time for the whole arena, `ts_ms`: the wall clock when
//! signing started ([`unix_now_ms`]). The gateways refuse a `ts` more than 5 minutes behind
//! their clock, so such an arena **goes stale**: it serves only runs that end within 5
//! minutes of `ts_ms`, which the harness checks before it reuses one (`bench`'s
//! `workload.rs`). Signing is the same as above otherwise: split across threads, RFC 6979,
//! low-S, and deterministic, so the same plan, deployment and `ts_ms` give the same arena.
//!
//! **`presigned.bin`** ([`Arena::save`], [`Arena::load`]), so a sweep doesn't sign again
//! after a restart. A 64-byte header, then the messages, 136 bytes each:
//!
//! | Bytes | Field |
//! |---|---|
//! | 0..8 | magic `PERPSGN1` |
//! | 8..16 | seed (`u64` LE) |
//! | 16..20 | deployment (`u32` LE); 20..24 zero |
//! | 24..32 | message count (`u64` LE) |
//! | 32..64 | the flow config's digest ([`PlanConfig::digest`](crate::market_flow::PlanConfig::digest)) |
//!
//! An EIP-712 arena's file has its own magic, `PERPSGN2`, and 8 more header bytes: 64..72,
//! its signing time `ts_ms` (`u64` LE, never 0). A perp arena's file is the same, byte for
//! byte, as before the EIP-712 scheme existed.
//!
//! Loading refuses a file whose magic or reserved bytes are wrong, or whose EIP-712 signing
//! time is 0; and a file whose seed, deployment, scheme (its magic) or digest differs from
//! the run's, or that holds fewer messages than the run needs. How fresh an EIP-712 file
//! is, is the caller's to judge, from the header it returns.
//!
//! **Complexity.** `messages × t_sign / threads`: 30 to 60 µs per signature on PERPSBOX
//! (to be replaced by the probe's figure, 15.10), so 11 to 28 s for the headline run. An
//! EIP-712 signature costs a little more: its digest (MessagePack and three keccak-256
//! hashes) adds about 1.3 µs locally (5.8).

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use engine::types::AccountId;
use gateway::eip712::Domain;
use gateway::wire::{self, SIGNATURE_BYTES, assemble, encode_signed_part, sign_eip712};
use k256::ecdsa::signature::Signer;
use k256::ecdsa::{Signature, SigningKey};
use pipeline::affinity::{CpuQuota, allowed_cpus};
use pipeline::codec::{COMMAND_WORDS, encode_command, from_le_bytes, to_le_bytes};
use pipeline::records::{AuthScheme, MESSAGE_BYTES, MESSAGE_WORDS};

use crate::keys::signing_keys;
use crate::market_flow::{ClientItem, FlowPlan, PlanConfig};

/// The benchmark's messages never expire (5.1): the gateway still compares, so the check is
/// measured.
pub const NEVER_EXPIRES: u64 = u64::MAX;

/// What an arena was signed for, and when. [`Arena::load`] compares all of it but when.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ArenaHeader {
    /// The flow's seed, which is also the keys' seed (14.8).
    pub seed: u64,
    pub deployment: u32,
    /// The digest of the flow config that generated the plan.
    pub flow_digest: [u8; 32],
    /// How the messages are signed (module docs).
    pub auth: AuthScheme,
    /// The EIP-712 scheme: the `ts` of every message, in milliseconds since the UNIX epoch
    /// (module docs). 0 in the perp scheme, whose messages carry no time.
    pub signed_at_ms: u64,
}

impl ArenaHeader {
    /// True if this header's arena was signed for what `expected` asks: the same seed,
    /// deployment, flow and scheme. When it was signed is not compared: that says how fresh
    /// the messages are, not what they are, and the caller judges it (module docs).
    pub fn is_for(&self, expected: &ArenaHeader) -> bool {
        ArenaHeader { signed_at_ms: expected.signed_at_ms, ..*self } == *expected
    }
}

impl fmt::Debug for ArenaHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArenaHeader")
            .field("seed", &self.seed)
            .field("deployment", &self.deployment)
            .field("flow_digest", &gateway::registry::hex(&self.flow_digest))
            .field("auth", &self.auth)
            .field("signed_at_ms", &self.signed_at_ms)
            .finish()
    }
}

/// The signed messages of a plan (module docs).
#[derive(Clone)]
pub struct Arena {
    header: ArenaHeader,
    messages: Vec<[u64; MESSAGE_WORDS]>,
}

impl fmt::Debug for Arena {
    /// The header and the size: not millions of messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Arena").field("header", &self.header).field("messages", &self.messages.len()).finish()
    }
}

impl Arena {
    pub fn header(&self) -> ArenaHeader {
        self.header
    }

    /// Messages in the arena.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Message `index` (the plan's client item `index`), as the ingress ring's words 0..17.
    pub fn message(&self, index: usize) -> &[u64; MESSAGE_WORDS] {
        &self.messages[index]
    }

    /// Message `index` as its 136 bytes.
    pub fn message_bytes(&self, index: usize) -> [u8; MESSAGE_BYTES] {
        to_le_bytes(&self.messages[index])
    }

    /// Writes the arena to `path` as `presigned.bin` (module docs).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut file = BufWriter::new(File::create(path)?);
        file.write_all(&encode_header(&self.header, self.messages.len() as u64))?;
        for message in &self.messages {
            file.write_all(&to_le_bytes::<MESSAGE_WORDS, MESSAGE_BYTES>(message))?;
        }
        file.flush()
    }

    /// Reads the first `needed` messages of the `presigned.bin` at `path`, if it was signed
    /// for `expected` ([`ArenaHeader::is_for`]) and holds at least that many. The arena
    /// keeps the file's header, and so its signing time.
    pub fn load(path: &Path, expected: &ArenaHeader, needed: usize) -> Result<Arena, ArenaFileError> {
        let mut file = BufReader::new(File::open(path)?);
        // The perp layout's 64 bytes, then 8 more if the magic says EIP-712 (module docs).
        let mut header = [0; EIP712_HEADER_BYTES];
        file.read_exact(&mut header[..HEADER_BYTES])?;
        let length = if header[0..8] == EIP712_ARENA_MAGIC { EIP712_HEADER_BYTES } else { HEADER_BYTES };
        file.read_exact(&mut header[HEADER_BYTES..length])?;
        let (found, count) = decode_header(&header[..length]).ok_or(ArenaFileError::NotAnArena)?;
        if !found.is_for(expected) {
            return Err(ArenaFileError::Mismatch { found, expected: *expected });
        }
        if count < needed as u64 {
            return Err(ArenaFileError::TooShort { count, needed });
        }
        let mut messages = Vec::with_capacity(needed);
        let mut bytes = [0; MESSAGE_BYTES];
        for _ in 0..needed {
            file.read_exact(&mut bytes)?;
            messages.push(from_le_bytes(&bytes));
        }
        Ok(Arena { header: found, messages })
    }
}

/// Bytes in the header of a perp arena's `presigned.bin` (module docs).
pub const HEADER_BYTES: usize = 64;
/// Bytes in the header of an EIP-712 arena's: the perp layout's, then the signing time.
pub const EIP712_HEADER_BYTES: usize = HEADER_BYTES + 8;
/// The first 8 bytes of a perp arena's `presigned.bin`.
pub const ARENA_MAGIC: [u8; 8] = *b"PERPSGN1";
/// The first 8 bytes of an EIP-712 arena's.
pub const EIP712_ARENA_MAGIC: [u8; 8] = *b"PERPSGN2";

/// Bytes in the header of an arena of `auth`'s scheme (module docs).
fn header_bytes(auth: AuthScheme) -> usize {
    match auth {
        AuthScheme::Perp => HEADER_BYTES,
        AuthScheme::Eip712 => EIP712_HEADER_BYTES,
    }
}

/// The header of an arena of `count` messages: [`HEADER_BYTES`] or
/// [`EIP712_HEADER_BYTES`] of them, by its scheme (module docs).
fn encode_header(header: &ArenaHeader, count: u64) -> Vec<u8> {
    let mut bytes = [0; EIP712_HEADER_BYTES];
    let magic = match header.auth {
        AuthScheme::Perp => ARENA_MAGIC,
        AuthScheme::Eip712 => EIP712_ARENA_MAGIC,
    };
    bytes[0..8].copy_from_slice(&magic);
    bytes[8..16].copy_from_slice(&header.seed.to_le_bytes());
    bytes[16..20].copy_from_slice(&header.deployment.to_le_bytes());
    bytes[24..32].copy_from_slice(&count.to_le_bytes());
    bytes[32..64].copy_from_slice(&header.flow_digest);
    bytes[64..72].copy_from_slice(&header.signed_at_ms.to_le_bytes()); // EIP-712 only
    bytes[..header_bytes(header.auth)].to_vec()
}

/// The header and the message count, or `None` if the magic, the length or the reserved
/// bytes are wrong, or an EIP-712 header's signing time is 0 (module docs).
fn decode_header(bytes: &[u8]) -> Option<(ArenaHeader, u64)> {
    let auth = match bytes.first_chunk::<8>()? {
        magic if *magic == ARENA_MAGIC => AuthScheme::Perp,
        magic if *magic == EIP712_ARENA_MAGIC => AuthScheme::Eip712,
        _ => return None,
    };
    if bytes.len() != header_bytes(auth) || bytes[20..24] != [0; 4] {
        return None;
    }
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
    // Only EIP-712 messages carry a time, and every one of them does.
    let signed_at_ms = if auth == AuthScheme::Eip712 { u64_at(64) } else { 0 };
    if auth == AuthScheme::Eip712 && signed_at_ms == 0 {
        return None;
    }
    let header = ArenaHeader {
        seed: u64_at(8),
        deployment: u32::from_le_bytes(bytes[16..20].try_into().expect("4 bytes")),
        flow_digest: bytes[32..64].try_into().expect("32 bytes"),
        auth,
        signed_at_ms,
    };
    Some((header, u64_at(24)))
}

/// Why a `presigned.bin` was refused.
#[derive(Debug)]
pub enum ArenaFileError {
    Io(io::Error),
    /// No magic, reserved bytes that are not zero, or an EIP-712 header without a signing
    /// time.
    NotAnArena,
    /// Signed for another seed, deployment, flow or scheme.
    Mismatch {
        found: ArenaHeader,
        expected: ArenaHeader,
    },
    /// Fewer messages than the run needs.
    TooShort {
        count: u64,
        needed: usize,
    },
}

impl From<io::Error> for ArenaFileError {
    fn from(error: io::Error) -> ArenaFileError {
        ArenaFileError::Io(error)
    }
}

impl fmt::Display for ArenaFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArenaFileError::Io(error) => write!(f, "reading presigned.bin: {error}"),
            ArenaFileError::NotAnArena => write!(f, "not a presigned.bin file"),
            ArenaFileError::Mismatch { found, expected } => {
                write!(f, "presigned.bin was signed for {found:?}, but this run is {expected:?}")
            }
            ArenaFileError::TooShort { count, needed } => {
                write!(f, "presigned.bin holds {count} messages, but this run needs {needed}")
            }
        }
    }
}

impl std::error::Error for ArenaFileError {}

/// The 136-byte message of `item`, signed with `key` for `deployment` (5.1, 5.2).
pub fn sign_message(key: &SigningKey, deployment: u32, item: &ClientItem) -> [u8; MESSAGE_BYTES] {
    let signed = encode_signed_part(deployment, item.account, item.nonce, NEVER_EXPIRES, &item.command);
    let signature: Signature = key.sign(&signed);
    let signature: [u8; SIGNATURE_BYTES] = signature.to_bytes().into();
    debug_assert!(
        wire::is_low_s(signature.last_chunk().expect("s is the last 32 bytes")),
        "k256 signs low-S (5.3)"
    );
    assemble(&signed, &signature)
}

/// The EIP-712 message of `item` (module docs, "The EIP-712 scheme"): its command, with the
/// item's nonce as the salt and `ts_ms` as the timestamp, signed with `key` over its digest
/// in `domain`, the deployment's (5.8).
pub fn sign_eip712_message(
    key: &SigningKey,
    domain: &Domain,
    item: &ClientItem,
    ts_ms: u64,
) -> [u8; MESSAGE_BYTES] {
    let message = sign_eip712(key, domain, item.account, item.nonce, ts_ms, &item.command);
    debug_assert!(
        wire::is_low_s(wire::signature(&message).last_chunk().expect("s is the last 32 bytes")),
        "the signature is low-S (5.3)"
    );
    message
}

/// Signs every client item of `plan` for `deployment`, on `threads` threads (module docs).
/// The keys are the plan's seed's (14.8).
pub fn presign<C: PlanConfig>(plan: &FlowPlan<C>, deployment: u32, threads: usize) -> Arena {
    let config = &plan.config;
    let messages = sign_items(plan, threads, |key, item| sign_message(key, deployment, item));
    let header = ArenaHeader {
        seed: config.seed(),
        deployment,
        flow_digest: config.digest(),
        auth: AuthScheme::Perp,
        signed_at_ms: 0,
    };
    Arena { header, messages }
}

/// Signs every client item of `plan` for `deployment` in the EIP-712 scheme, with `ts_ms` as
/// every message's timestamp, on `threads` threads (module docs, "The EIP-712 scheme"). The
/// keys are the plan's seed's (14.8). Pass the wall clock when signing starts
/// ([`unix_now_ms`]): the messages are then fresh for 5 minutes from that time.
pub fn presign_eip712<C: PlanConfig>(
    plan: &FlowPlan<C>,
    deployment: u32,
    ts_ms: u64,
    threads: usize,
) -> Arena {
    let config = &plan.config;
    // The deployment is the EIP-712 chain id; its domain separator is computed once here.
    let domain = Domain::new(u64::from(deployment));
    let messages = sign_items(plan, threads, |key, item| sign_eip712_message(key, &domain, item, ts_ms));
    let header = ArenaHeader {
        seed: config.seed(),
        deployment,
        flow_digest: config.digest(),
        auth: AuthScheme::Eip712,
        signed_at_ms: ts_ms,
    };
    Arena { header, messages }
}

/// Every client item of `plan`, in plan order, as the words of the message `sign` makes of
/// it with the item's account's key, on `threads` threads (module docs).
fn sign_items<C: PlanConfig>(
    plan: &FlowPlan<C>,
    threads: usize,
    sign: impl Fn(&SigningKey, &ClientItem) -> [u8; MESSAGE_BYTES] + Sync,
) -> Vec<[u64; MESSAGE_WORDS]> {
    let config = &plan.config;
    let keys: BTreeMap<AccountId, SigningKey> = signing_keys(config.seed(), &config.client_accounts());
    let items: Vec<&ClientItem> = plan.client_items().collect();
    let mut messages = vec![[0; MESSAGE_WORDS]; items.len()];
    // Each thread signs one contiguous slice into its own part of `messages`.
    let per_thread = items.len().div_ceil(threads.max(1)).max(1);
    std::thread::scope(|scope| {
        for (items, messages) in items.chunks(per_thread).zip(messages.chunks_mut(per_thread)) {
            let (keys, sign) = (&keys, &sign);
            scope.spawn(move || {
                for (item, message) in items.iter().zip(messages) {
                    let key = &keys[&item.account];
                    *message = from_le_bytes(&sign(key, item));
                }
            });
        }
    });
    messages
}

/// The wall clock, in milliseconds since the UNIX epoch: an EIP-712 arena's `ts_ms`. The
/// gateways compare `ts` with the run clock, which is anchored to the same wall clock
/// (`pipeline::clock`).
pub fn unix_now_ms() -> u64 {
    let since_epoch =
        SystemTime::now().duration_since(UNIX_EPOCH).expect("the system clock is set before 1970");
    u64::try_from(since_epoch.as_millis()).expect("the system clock is set in a sane year")
}

/// The threads to sign on: `min(allowed CPUs, floor(CPU quota))` (14.8). Signing finishes
/// before the pipeline's threads start, so it may use every CPU the quota allows.
pub fn signing_threads() -> usize {
    let allowed = allowed_cpus().map(|cpus| cpus.len()).unwrap_or(1).max(1);
    match CpuQuota::read() {
        Ok(CpuQuota::Limited { quota_us, period_us }) => {
            let whole_cpus = usize::try_from(quota_us / period_us.max(1)).unwrap_or(usize::MAX);
            allowed.min(whole_cpus.max(1))
        }
        Ok(CpuQuota::Unlimited) | Err(_) => allowed,
    }
}

/// The signer of a message given as its words: bytes 12..16, the high half of word 1
/// (bytes 8..12 are the deployment).
pub fn message_account(message: &[u64; MESSAGE_WORDS]) -> AccountId {
    AccountId::new((message[1] >> 32) as u32)
}

/// A client item without a signature, for pre-verified runs (14.11): 7 words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactItem {
    pub account: AccountId,
    pub nonce: u64,
    /// The command's CMD40 words (4.2).
    pub command: [u64; COMMAND_WORDS],
}

/// The compact items of a plan (module docs).
#[derive(Clone)]
pub struct CompactArena {
    /// The digest of the flow config that generated the plan.
    flow_digest: [u8; 32],
    items: Vec<CompactItem>,
}

impl fmt::Debug for CompactArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompactArena")
            .field("flow_digest", &gateway::registry::hex(&self.flow_digest))
            .field("items", &self.items.len())
            .finish()
    }
}

impl CompactArena {
    /// The digest of the flow config that generated the plan.
    pub fn flow_digest(&self) -> [u8; 32] {
        self.flow_digest
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Item `index` (the plan's client item `index`).
    pub fn item(&self, index: usize) -> &CompactItem {
        &self.items[index]
    }
}

/// The compact items of every client item of `plan`, in plan order: the same commands as
/// the signed arena, with no signatures.
pub fn preverified<C: PlanConfig>(plan: &FlowPlan<C>) -> CompactArena {
    let items = plan
        .client_items()
        .map(|item| CompactItem {
            account: item.account,
            nonce: item.nonce,
            command: encode_command(&item.command),
        })
        .collect();
    CompactArena { flow_digest: plan.config.digest(), items }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::signing_key;
    use crate::market_flow::{MarketFlowConfig, generate};
    use gateway::wire::{check_signer, decode, decode_eip712, signature, signed_part, verify_signature};
    use gateway::{PublicKey, VerifierKind};
    use pipeline::codec::decode_command;

    const DEPLOYMENT: u32 = 77;
    /// An EIP-712 arena's signing time: 2026-09-21, in milliseconds.
    const TS_MS: u64 = 1_790_000_000_000;

    fn plan() -> FlowPlan {
        generate(&MarketFlowConfig::smoke(), 300)
    }

    #[test]
    fn every_message_decodes_to_its_item_and_verifies_with_a_low_s() {
        let plan = plan();
        let arena = presign(&plan, DEPLOYMENT, 3);
        assert_eq!(arena.len(), plan.setup_client_items() + 300);
        assert_eq!(arena.header().flow_digest, plan.config.digest());
        for (index, item) in plan.client_items().enumerate() {
            let message = arena.message_bytes(index);
            let decoded = decode(&message).expect("the gateway decodes it");
            assert_eq!(decoded.deployment, DEPLOYMENT);
            assert_eq!(
                (decoded.account, decoded.nonce, decoded.expires_at),
                (item.account, item.nonce, u64::MAX)
            );
            assert_eq!(decoded.command, item.command);
            assert_eq!(message_account(arena.message(index)), item.account);
            let key = PublicKey::K256(*signing_key(plan.config.seed, item.account).verifying_key());
            assert_eq!(verify_signature(&key, signed_part(&message), signature(&message)), Ok(()));
            assert!(wire::is_low_s(signature(&message).last_chunk().expect("s")));
        }
    }

    #[test]
    fn every_eip712_message_decodes_to_its_item_and_recovers_its_signer() {
        let plan = plan();
        let arena = presign_eip712(&plan, DEPLOYMENT, TS_MS, 3);
        assert_eq!(arena.len(), plan.setup_client_items() + 300);
        let header = arena.header();
        assert_eq!((header.auth, header.signed_at_ms), (AuthScheme::Eip712, TS_MS));
        assert_eq!(header.flow_digest, plan.config.digest());
        let domain = Domain::new(u64::from(DEPLOYMENT));
        for (index, item) in plan.client_items().enumerate() {
            let message = arena.message_bytes(index);
            let decoded = decode_eip712(&message).expect("the gateway decodes it");
            assert_eq!(decoded.deployment, DEPLOYMENT);
            // The salt is the item's nonce (module docs); every message has the arena's time.
            assert_eq!((decoded.account, decoded.salt, decoded.ts_ms), (item.account, item.nonce, TS_MS));
            assert_eq!(decoded.command, item.command);
            assert_eq!(
                message_account(arena.message(index)),
                item.account,
                "the sender routes it the same way"
            );
            let address =
                PublicKey::K256(*signing_key(plan.config.seed, item.account).verifying_key()).address();
            let signature = signature(&message);
            assert_eq!(check_signer(VerifierKind::K256, &domain, &decoded, signature, &address), Ok(()));
            assert!(wire::is_low_s(signature.last_chunk().expect("s")));
        }
    }

    #[test]
    fn an_eip712_arena_is_the_same_on_any_thread_count_and_bound_to_its_time() {
        let plan = plan();
        let one = presign_eip712(&plan, DEPLOYMENT, TS_MS, 1);
        let many = presign_eip712(&plan, DEPLOYMENT, TS_MS, 7);
        assert_eq!(one.messages, many.messages);
        // Another time, another deployment or the other scheme signs other bytes.
        assert_ne!(presign_eip712(&plan, DEPLOYMENT, TS_MS + 1, 2).messages, one.messages);
        assert_ne!(presign_eip712(&plan, DEPLOYMENT + 1, TS_MS, 2).messages, one.messages);
        assert_ne!(presign(&plan, DEPLOYMENT, 2).messages, one.messages);
    }

    #[test]
    fn the_wall_clock_is_in_milliseconds() {
        // After 2026-01-01 and before 2100-01-01, in milliseconds.
        assert!((1_767_225_600_000..4_102_444_800_000).contains(&unix_now_ms()));
    }

    #[test]
    fn the_thread_count_changes_no_byte() {
        let plan = plan();
        let one = presign(&plan, DEPLOYMENT, 1);
        let many = presign(&plan, DEPLOYMENT, 7);
        assert_eq!(one.messages, many.messages);
        // Another deployment signs other bytes.
        assert_ne!(presign(&plan, DEPLOYMENT + 1, 2).messages, one.messages);
    }

    #[test]
    fn the_compact_arena_holds_the_same_commands_unsigned() {
        let plan = plan();
        let compact = preverified(&plan);
        assert_eq!(compact.len(), plan.client_items().count());
        for (index, item) in plan.client_items().enumerate() {
            let compact = compact.item(index);
            assert_eq!((compact.account, compact.nonce), (item.account, item.nonce));
            assert_eq!(decode_command(&compact.command), Ok(item.command));
        }
        assert_eq!(size_of::<CompactItem>(), 56);
    }

    #[test]
    fn presigned_bin_loads_back_and_refuses_another_run() {
        let plan = plan();
        let arena = presign(&plan, DEPLOYMENT, 2);
        let dir = std::env::temp_dir().join(format!("loadgen-presign-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("created");
        let path = dir.join("presigned.bin");
        arena.save(&path).expect("saved");
        let bytes = std::fs::metadata(&path).expect("exists").len();
        assert_eq!(bytes, (HEADER_BYTES + MESSAGE_BYTES * arena.len()) as u64);
        // The header, byte for byte as before the EIP-712 scheme existed (module docs).
        let header = arena.header();
        let mut expected = [0u8; HEADER_BYTES];
        expected[0..8].copy_from_slice(b"PERPSGN1");
        expected[8..16].copy_from_slice(&plan.config.seed.to_le_bytes());
        expected[16..20].copy_from_slice(&DEPLOYMENT.to_le_bytes());
        expected[24..32].copy_from_slice(&(arena.len() as u64).to_le_bytes());
        expected[32..64].copy_from_slice(&plan.config.digest());
        assert_eq!(std::fs::read(&path).expect("read")[..HEADER_BYTES], expected);

        let all = Arena::load(&path, &header, arena.len()).expect("loads");
        assert_eq!(all.messages, arena.messages);
        let prefix = Arena::load(&path, &header, 10).expect("a prefix loads");
        assert_eq!(prefix.messages[..], arena.messages[..10]);

        let refused = |expected: ArenaHeader, needed| Arena::load(&path, &expected, needed).map(|_| ());
        assert!(matches!(
            refused(ArenaHeader { seed: 2, ..header }, 1),
            Err(ArenaFileError::Mismatch { .. })
        ));
        let other_deployment = ArenaHeader { deployment: 1, ..header };
        assert!(matches!(refused(other_deployment, 1), Err(ArenaFileError::Mismatch { .. })));
        let other_flow = ArenaHeader { flow_digest: MarketFlowConfig::m3().digest(), ..header };
        assert!(matches!(refused(other_flow, 1), Err(ArenaFileError::Mismatch { .. })));
        assert!(matches!(refused(header, arena.len() + 1), Err(ArenaFileError::TooShort { .. })));
        let other_scheme = ArenaHeader { auth: AuthScheme::Eip712, signed_at_ms: TS_MS, ..header };
        assert!(matches!(refused(other_scheme, 1), Err(ArenaFileError::Mismatch { .. })));
        std::fs::write(&path, [0u8; 100]).expect("overwritten");
        assert!(matches!(refused(header, 1), Err(ArenaFileError::NotAnArena)));
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn presigned_bin_keeps_the_scheme_and_the_signing_time_and_refuses_what_doesnt_fit() {
        let plan = plan();
        let arena = presign_eip712(&plan, DEPLOYMENT, TS_MS, 2);
        let dir = std::env::temp_dir().join(format!("loadgen-presign-eip712-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("created");
        let path = dir.join("presigned.bin");
        arena.save(&path).expect("saved");
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(bytes.len(), EIP712_HEADER_BYTES + MESSAGE_BYTES * arena.len());
        assert_eq!((&bytes[0..8], &bytes[64..72]), (&b"PERPSGN2"[..], &TS_MS.to_le_bytes()[..]));

        // The signing time is kept, and not compared: the caller judges freshness.
        let header = arena.header();
        let asked = ArenaHeader { signed_at_ms: 1, ..header };
        let loaded = Arena::load(&path, &asked, arena.len()).expect("loads");
        assert_eq!(loaded.header(), header);
        assert_eq!(loaded.messages, arena.messages);
        let perp = ArenaHeader { auth: AuthScheme::Perp, signed_at_ms: 0, ..header };
        assert!(matches!(Arena::load(&path, &perp, 1), Err(ArenaFileError::Mismatch { .. })));

        // Headers that can't be ours (module docs): the perp magic on the longer header,
        // reserved bytes, and no signing time.
        let good = encode_header(&header, 1);
        let damaged = |at: usize, value: u8| {
            let mut bytes = good.clone();
            bytes[at] = value;
            bytes
        };
        assert!(decode_header(&good).is_some());
        let perp_magic = {
            let mut bytes = good.clone();
            bytes[0..8].copy_from_slice(&ARENA_MAGIC);
            bytes
        };
        let without_a_time = encode_header(&ArenaHeader { signed_at_ms: 0, ..header }, 1);
        for bytes in
            [perp_magic, damaged(20, 1), damaged(23, 1), without_a_time, good[..HEADER_BYTES].to_vec()]
        {
            assert_eq!(decode_header(&bytes), None);
        }
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn signing_uses_at_least_one_thread() {
        assert!(signing_threads() >= 1);
    }
}
