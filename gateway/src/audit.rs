//! The signature audit of a journal (`docs/PIPELINE.md` 13.4, 11.3 "Why the signature is
//! journaled", Q3): anyone with the key registry can check again that every signed command
//! in the journal was signed by its account's key, and that no signed message was used
//! twice.
//!
//! **Contract.** [`verify_journal`] reads the journal in `dir` from the start to its end
//! (through `scan_journal`, the reader recovery and replay use, which re-checks each
//! record's CRC and 11.8's rules on the way; it changes nothing) and reports every failure,
//! which must be zero:
//! - **Signatures.** For every kind-1 record, it rebuilds the 72 signed bytes (magic,
//!   version 1, reserved 0, the header's deployment, then the record's account, nonce,
//!   `expires_at` and CMD40; canonical form, 4.4, makes them exactly the bytes the client
//!   signed) and verifies the signature, low-S included, with the key the record's
//!   *segment's* registry names for its account. Each segment header holds the SHA-256 of
//!   the `keys.txt` its life loaded (11.2), so a key replaced at a restart means two
//!   registry files, and each record is checked against the right one.
//! - **Ownership.** `account == account_of(order_id)`: a signature alone doesn't prove it
//!   (account 7 can sign a cancel of account 9's order, 5.5).
//! - **Nonces strictly increase per account** over the whole journal: a copied or replayed
//!   record fails this (6.1).
//! - **In the EIP-712 scheme** (segment 0's header says which, byte 120; D-033), the
//!   signatures and the replays are checked another way; ownership and the kinds, as
//!   above and below. For every kind-1 record it rebuilds the EIP-712 digest the client
//!   signed from the header's deployment (the domain) and the record's salt (the nonce's
//!   word), timestamp `ts` (the expiry's word) and CMD40 (`eip712.rs`), and verifies the
//!   signature, low-S included, with the account's registered key over that digest. The
//!   recovery id is not journaled, and not needed: a signature by that key over the digest
//!   is exactly one whose recovery gives that key, and so its address. The digest covers
//!   the whole CMD40 but a cancel's or a modify's market, which the scheme doesn't sign
//!   (`eip712.rs`), so the audit can't see that market changed. And **no request
//!   repeats**: the request `(account, salt, ts, market)`, the gateway's own key
//!   (`salts::Request`), appears once in the whole journal, and each `ts` lies in the
//!   window around the record's own time (its `ts` field, the sequencer's): at most 60 s
//!   after it, and at most 5 minutes before it plus [`SEQUENCING_SLACK_MS`]. (The gateway
//!   checked the window against its clock when it took the message, which is not
//!   journaled. That was before the sequencer's time, so the first bound is exact; the
//!   second allows for the time a record waits in its lane between the two.) With the
//!   market in the key, a copy of a signed cancel or modify with another market, which the
//!   gateway accepts as another request (`salts.rs`, "Why the market is in the key"), passes
//!   here too, next to the genuine record: both signatures verify over the same digest, and
//!   their requests differ. The replay (13.1) rejects the copy (`UnknownOrder` or
//!   `UnknownMarket`) and applies the genuine one. An exact repeat, the same market
//!   included, fails.
//! - **Kinds and tags fit** (11.3): kind 1 only in a signed journal, kind 2 only in a
//!   pre-verified one, client kinds hold places, cancels and modifies, kind 3 holds
//!   operator commands. Kind-2 and kind-3 records are counted separately.
//!
//! `scan_journal` already refuses a record that breaks the ownership or kind rules (as an
//! error, which the audit reports as a failure and stops at). The audit checks them again
//! itself, record by record, so that its claim doesn't rest on recovery's code.
//!
//! **What it proves, exactly** (13.4): every client command in the journal was signed by
//! the key the registry names for its account, and no signed message was used twice (and,
//! in the EIP-712 scheme, none was used outside its time window, up to the slack). In the
//! EIP-712 scheme "signed" means every field but a cancel's or a modify's market, and
//! "used twice" means the same request, market included: a signed cancel or modify may
//! appear once per market id, each copy but the genuine one rejected by the engine (above).
//! So a journal whose market was changed there, after the fact, still passes, and its
//! replay turns that cancel or modify into an `UnknownOrder` or `UnknownMarket` reject
//! (D-033, "Trade-offs"). It is relative to the registry: whoever can write both the
//! journal and `keys.txt` can forge both, so the auditor must trust the registry from
//! another source (for example, each client confirming its own public key). The header's
//! digest only shows which file the exchange used.
//!
//! **In parallel.** Verifications dominate (about 50 µs each; 6.5M records are about 5
//! minutes on one thread). The scan collects signed records in batches of
//! [`AUDIT_BATCH`], and each batch is verified on `threads` scoped threads, each taking a
//! contiguous slice, before the scan goes on, so memory stays at one batch.
//!
//! **Complexity.** One pass over the journal; one verification per kind-1 record, spread
//! over the threads; one map entry per account, or in the EIP-712 scheme one set entry per
//! request (about 100 MB for 3.5 million).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;

use engine::command::Command;
use engine::types::{AccountId, account_of};

use pipeline::codec::{command_tag, to_le_bytes};
use pipeline::journal::files::{JournalFiles, StdFiles};
use pipeline::journal::format::{
    DEFAULT_SEGMENT_BYTES, HEADER_BYTES, HeaderRead, SegmentHeader, client_order_id,
};
use pipeline::journal::recovery::scan_journal;
use pipeline::records::{AuthScheme, InjectionMode, JournalRecord, Source};

use crate::check::{MAX_AGE_MS, MAX_AHEAD_MS};
use crate::eip712::{self, Domain};
use crate::registry::{KeyRegistry, hex};
use crate::salts::Request;
use crate::wire::{SIGNATURE_BYTES, SIGNED_BYTES, encode_signed_part, verify_digest, verify_signature};

/// Signed records verified together, across the threads.
pub const AUDIT_BATCH: usize = 16_384;
/// How much later than the gateway took a message its record may be sequenced, for the
/// EIP-712 scheme's window rule (module docs): 10 s. A record waits in its lane between
/// the two, microseconds when the pipeline keeps up, longer behind a slow disk (2.4). Only
/// a message taken within this long of the end of its window, and sequenced this late,
/// could fail the rule unfairly.
pub const SEQUENCING_SLACK_MS: u64 = 10_000;
/// Failures the report lists one by one; the count covers them all.
pub const LISTED_FAILURES: usize = 100;

/// What the audit found (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditReport {
    /// Kind-1 (signed client) records whose signature was checked.
    pub signed: u64,
    /// Kind-2 (pre-verified client) records.
    pub pre_verified: u64,
    /// Kind-3 (operator) records.
    pub operator: u64,
    /// Every failure found. Must be 0.
    pub failures: u64,
    /// The first [`LISTED_FAILURES`] failures, in journal order.
    pub listed: Vec<AuditFailure>,
}

/// One failure: the record's seq (0 for the journal as a whole) and what is wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditFailure {
    pub seq: u64,
    pub what: String,
}

impl AuditReport {
    /// True if nothing failed.
    pub fn passed(&self) -> bool {
        self.failures == 0
    }

    fn fail(&mut self, seq: u64, what: impl Into<String>) {
        self.failures += 1;
        if self.listed.len() < LISTED_FAILURES {
            self.listed.push(AuditFailure { seq, what: what.into() });
        }
    }
}

impl fmt::Display for AuditReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "signature audit: {} signed records verified, {} pre-verified, {} operator; {} failures",
            self.signed, self.pre_verified, self.operator, self.failures
        )?;
        for failure in &self.listed {
            write!(f, "\n  seq {}: {}", failure.seq, failure.what)?;
        }
        if self.failures > self.listed.len() as u64 {
            write!(f, "\n  ... and {} more", self.failures - self.listed.len() as u64)?;
        }
        Ok(())
    }
}

/// Audits the journal in `dir` against `registries`: every registry file its segments name
/// (module docs). Runs on as many threads as the process may use.
pub fn verify_journal(dir: &Path, registries: &[KeyRegistry]) -> AuditReport {
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    match StdFiles::open_existing(dir, DEFAULT_SEGMENT_BYTES) {
        Ok(mut files) => verify_files(&mut files, registries, threads),
        Err(e) => {
            let mut report = AuditReport::default();
            report.fail(0, format!("opening the journal in {}: {e}", dir.display()));
            report
        }
    }
}

/// [`verify_journal`] on any [`JournalFiles`], on `threads` threads.
pub fn verify_files(
    files: &mut impl JournalFiles,
    registries: &[KeyRegistry],
    threads: usize,
) -> AuditReport {
    let mut report = AuditReport::default();
    let headers = match read_headers(files) {
        Ok(headers) => headers,
        Err(what) => {
            report.fail(0, what);
            return report;
        }
    };
    let Some(first) = headers.first() else {
        // No valid header: an empty journal, unless segment 0's header is not one at all,
        // which the scan reports.
        if let Err(error) = scan_journal(files, |_, _, _| {}) {
            report.fail(0, format!("the journal can't be read: {error}"));
        }
        return report;
    };
    let identity = first.identity;
    let mut auditor = Auditor {
        deployment: identity.deployment,
        mode: identity.mode,
        auth: identity.auth,
        domain: Domain::new(u64::from(identity.deployment)),
        registries: headers
            .iter()
            .map(|header| registry_for(header, registries, identity.deployment))
            .collect(),
        last_nonce: HashMap::new(),
        requests: HashSet::new(),
        batch: Vec::with_capacity(AUDIT_BATCH),
        threads: threads.max(1),
        report,
    };
    let scan = scan_journal(files, |segment, record, command| auditor.visit(segment, record, command));
    auditor.verify_batch();
    if let Err(error) = scan {
        auditor.report.fail(0, format!("the journal can't be read to its end: {error}"));
    }
    auditor.report
}

/// Every valid segment header, from segment 0 until the first segment that is missing or
/// has no valid header (where the journal ends). `scan_journal` checks them again, in full.
fn read_headers(files: &mut impl JournalFiles) -> Result<Vec<SegmentHeader>, String> {
    let mut headers = Vec::new();
    for segment in 0.. {
        if !files.exists(segment).map_err(|e| format!("reading segment {segment}: {e}"))? {
            break;
        }
        let mut bytes = [0; HEADER_BYTES];
        files.read_at(segment, 0, &mut bytes).map_err(|e| format!("reading segment {segment}: {e}"))?;
        match SegmentHeader::decode(&bytes) {
            HeaderRead::Valid(header) => headers.push(header),
            HeaderRead::Unused | HeaderRead::Torn | HeaderRead::Invalid(_) => break,
        }
    }
    Ok(headers)
}

/// The registry whose digest segment `header` names, or why there is none.
fn registry_for<'r>(
    header: &SegmentHeader,
    registries: &'r [KeyRegistry],
    deployment: u32,
) -> Result<&'r KeyRegistry, String> {
    let digest = header.registry_digest;
    let registry = registries.iter().find(|registry| registry.digest() == digest).ok_or_else(|| {
        format!("no registry given has the digest of segment {}'s ({})", header.segment, hex(&digest))
    })?;
    if registry.deployment() != deployment {
        return Err(format!(
            "the registry of segment {} is for deployment {}, the journal's is {deployment}",
            header.segment,
            registry.deployment()
        ));
    }
    Ok(registry)
}

/// A kind-1 record, ready to verify: what its client signed, and the signature.
#[derive(Clone, Debug)]
struct SignedRecord {
    seq: u64,
    segment: u32,
    account: AccountId,
    signed: Signed,
    signature: [u8; SIGNATURE_BYTES],
}

/// What a client signed, in each scheme.
#[derive(Clone, Debug)]
enum Signed {
    /// The perp scheme's 72 bytes, which the verifier hashes with SHA-256 (5.2).
    Bytes([u8; SIGNED_BYTES]),
    /// The EIP-712 scheme's digest, signed as it is (module docs).
    Digest([u8; 32]),
}

/// The audit's state while the scan runs.
struct Auditor<'r> {
    deployment: u32,
    mode: InjectionMode,
    /// The journal's signing scheme, from segment 0's header.
    auth: AuthScheme,
    /// The deployment's EIP-712 domain (used in that scheme only).
    domain: Domain,
    /// Each segment's registry, by segment index, or why it has none.
    registries: Vec<Result<&'r KeyRegistry, String>>,
    /// Each account's last nonce so far (the perp scheme).
    last_nonce: HashMap<AccountId, u64>,
    /// Every request `(account, salt, ts, market)` so far (the EIP-712 scheme).
    requests: HashSet<Request>,
    /// Signed records waiting to be verified.
    batch: Vec<SignedRecord>,
    threads: usize,
    report: AuditReport,
}

impl Auditor<'_> {
    /// One record, in journal order: the rules, the nonce order (or the requests), and
    /// (kind 1) its signature, in the next batch.
    fn visit(&mut self, segment: u32, record: &JournalRecord, command: &Command) {
        let seq = record.seq;
        if let Err(what) = kind_fits(record, command, self.mode) {
            self.report.fail(seq, what);
        }
        match record.meta.source {
            Source::Operator => self.report.operator += 1,
            Source::PreVerifiedClient => {
                self.report.pre_verified += 1;
                self.check_nonce(record);
            }
            Source::SignedClient => {
                let signed = match self.auth {
                    AuthScheme::Perp => {
                        self.check_nonce(record);
                        Signed::Bytes(encode_signed_part(
                            self.deployment,
                            record.meta.account,
                            record.nonce,
                            record.expires_at,
                            command,
                        ))
                    }
                    AuthScheme::Eip712 => {
                        // An operator command has no signed form and no request; `kind_fits`
                        // failed it above.
                        let Some(data) = eip712::op_data(command) else { return };
                        self.check_request(record, command);
                        Signed::Digest(eip712::digest(&self.domain, &data, record.nonce, record.expires_at))
                    }
                };
                self.batch.push(SignedRecord {
                    seq,
                    segment,
                    account: record.meta.account,
                    signed,
                    signature: to_le_bytes(&record.signature),
                });
                if self.batch.len() == AUDIT_BATCH {
                    self.verify_batch();
                }
            }
        }
    }

    /// The EIP-712 scheme's rules for a kind-1 record of a place, a cancel or a modify,
    /// `command` (module docs): its request `(account, salt, ts, market)` was not journaled
    /// before, and its `ts` (milliseconds) lies in the window around the record's own time.
    fn check_request(&mut self, record: &JournalRecord, command: &Command) {
        let (account, salt, ts_ms) = (record.meta.account, record.nonce, record.expires_at);
        // The salt is in the nonce's word and `ts` in the expiry's; the market is the
        // command's, as in the gateway's key.
        let request = Request::of(account, salt, ts_ms, command).expect("a place, a cancel or a modify");
        if !self.requests.insert(request) {
            let what = format!(
                "account {account}'s request (salt {salt}, ts {ts_ms}, market {}) is journaled twice (a copied or replayed message)",
                request.market
            );
            self.report.fail(record.seq, what);
        }
        let record_ms = record.ts / 1_000_000;
        if ts_ms > record_ms.saturating_add(MAX_AHEAD_MS) {
            let what = format!(
                "account {account}'s ts {ts_ms} is more than 60 s after the record's time, {record_ms} ms"
            );
            self.report.fail(record.seq, what);
        } else if ts_ms.saturating_add(MAX_AGE_MS + SEQUENCING_SLACK_MS) < record_ms {
            let what = format!(
                "account {account}'s ts {ts_ms} is more than 5 minutes and {SEQUENCING_SLACK_MS} ms before the record's time, {record_ms} ms"
            );
            self.report.fail(record.seq, what);
        }
    }

    /// Nonces strictly increase per account over the whole journal (6.1): the first is 1
    /// or more, and each is above the one before.
    fn check_nonce(&mut self, record: &JournalRecord) {
        let account = record.meta.account;
        let last = self.last_nonce.entry(account).or_insert(0);
        if record.nonce <= *last {
            let what = format!(
                "account {account}'s nonce {} is not above its previous one, {} (a copied or replayed message)",
                record.nonce, *last
            );
            self.report.fail(record.seq, what);
        } else {
            *last = record.nonce;
        }
    }

    /// Verifies the batch's signatures on the threads, and empties it.
    fn verify_batch(&mut self) {
        let chunk = self.batch.len().div_ceil(self.threads).max(1);
        let registries = &self.registries;
        let failures: Vec<AuditFailure> = std::thread::scope(|scope| {
            let check_part = |part: &[SignedRecord]| -> Vec<AuditFailure> {
                part.iter().filter_map(|record| check_signature(record, registries)).collect()
            };
            let workers: Vec<_> =
                self.batch.chunks(chunk).map(|part| scope.spawn(move || check_part(part))).collect();
            // Joined in order, so the failures stay in journal order.
            workers.into_iter().flat_map(|w| w.join().expect("an audit thread ends")).collect::<Vec<_>>()
        });
        for failure in failures {
            self.report.fail(failure.seq, failure.what);
        }
        self.report.signed += self.batch.len() as u64;
        self.batch.clear();
    }
}

/// One signed record's signature, with its segment's registry; `None` if it is good.
fn check_signature(
    record: &SignedRecord,
    registries: &[Result<&KeyRegistry, String>],
) -> Option<AuditFailure> {
    let fail = |what: String| Some(AuditFailure { seq: record.seq, what });
    // Every segment the scan visits had a valid header, so it has an entry.
    let registry = match registries.get(record.segment as usize) {
        Some(Ok(registry)) => registry,
        Some(Err(why)) => return fail(why.clone()),
        None => return fail(format!("segment {} has no header", record.segment)),
    };
    let Some(key) = registry.key(record.account) else {
        return fail(format!(
            "account {} has no key in segment {}'s registry",
            record.account, record.segment
        ));
    };
    let checked = match &record.signed {
        Signed::Bytes(bytes) => verify_signature(key, bytes, &record.signature),
        Signed::Digest(digest) => verify_digest(key, digest, &record.signature),
    };
    match checked {
        Ok(()) => None,
        Err(reason) => fail(format!("account {}'s signature fails: {reason}", record.account)),
    }
}

/// 11.3's rules for what each kind may hold, in a journal of `mode` (module docs).
fn kind_fits(record: &JournalRecord, command: &Command, mode: InjectionMode) -> Result<(), String> {
    let kind = record.meta.source;
    let tag = command_tag(command);
    match (kind, client_order_id(command)) {
        (Source::SignedClient, _) if mode != InjectionMode::Signed => {
            Err("a kind-1 (signed) record in a pre-verified journal".into())
        }
        (Source::PreVerifiedClient, _) if mode != InjectionMode::PreVerified => {
            Err("a kind-2 (pre-verified) record in a signed journal".into())
        }
        (Source::SignedClient | Source::PreVerifiedClient, None) => {
            Err(format!("a client record holding operator command tag {tag}"))
        }
        (Source::SignedClient | Source::PreVerifiedClient, Some(order_id))
            if account_of(order_id) != record.meta.account =>
        {
            Err(format!(
                "account {} doesn't own order {order_id:#x}, which is account {}'s",
                record.meta.account,
                account_of(order_id)
            ))
        }
        (Source::Operator, Some(_)) => Err(format!("an operator record holding client command tag {tag}")),
        (Source::SignedClient | Source::PreVerifiedClient, Some(_)) | (Source::Operator, None) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        VERIFIER, cancel, message_eip712, modify, place, registry, signing_key, with_market,
    };
    use engine::command::{Deposit, SetMark};
    use engine::engine::EngineOptions;
    use engine::types::MarketId;
    use k256::ecdsa::Signature;
    use k256::ecdsa::signature::Signer;
    use pipeline::clock::RunClock;
    use pipeline::codec::{encode_command, from_le_bytes};
    use pipeline::counters::Watermark;
    use pipeline::journal::files::SimDisk;
    use pipeline::journal::format::JournalIdentity;
    use pipeline::journal::writer::JournalWriter;
    use pipeline::records::{Meta, SIGNATURE_WORDS};

    const DEPLOYMENT: u32 = 4;
    const SEGMENT_BYTES: u64 = 4_096;

    /// One command as the test journal will hold it.
    enum Entry {
        /// A signed client command: account, nonce, command, and the key that signs it.
        Signed(AccountId, u64, Command, u64),
        Operator(Command),
        /// A record as given, but for its seq and `ts`, which the journal sets.
        Record(JournalRecord),
    }

    /// The kind-1 record the sequencer would write for a message from `account`, signed by
    /// the key derived from `key_seed`.
    fn signed_record(
        seq: u64,
        ts: u64,
        account: AccountId,
        nonce: u64,
        command: &Command,
        key_seed: u64,
    ) -> JournalRecord {
        let expires_at = u64::MAX - seq;
        let signed = encode_signed_part(DEPLOYMENT, account, nonce, expires_at, command);
        let signature: Signature = signing_key(key_seed, account).sign(&signed);
        let signature: [u8; 64] = signature.to_bytes().into();
        JournalRecord {
            seq,
            ts,
            meta: Meta { source: Source::SignedClient, lane: (account % 2) as u16, account },
            nonce,
            command: encode_command(command),
            expires_at,
            signature: from_le_bytes(&signature),
        }
    }

    fn operator_record(seq: u64, ts: u64, command: &Command) -> JournalRecord {
        JournalRecord {
            seq,
            ts,
            meta: Meta::OPERATOR,
            nonce: 0,
            command: encode_command(command),
            expires_at: 0,
            signature: [0; SIGNATURE_WORDS],
        }
    }

    /// Writes `lives`, each a registry digest and its entries, as one journal on a
    /// simulated disk: each life starts a new segment, as a restart does (11.1).
    fn journal(lives: &[([u8; 32], Vec<Entry>)]) -> SimDisk {
        let identity = JournalIdentity::new(DEPLOYMENT, InjectionMode::Signed, EngineOptions::default());
        journal_of(identity, 1_000, lives)
    }

    /// [`journal`] for a journal of `identity`, whose clock starts at `start_ns`: record
    /// `seq`'s time is `start_ns + seq`.
    fn journal_of(identity: JournalIdentity, start_ns: u64, lives: &[([u8; 32], Vec<Entry>)]) -> SimDisk {
        let mut disk = SimDisk::new(SEGMENT_BYTES);
        let (clock, durable) = (RunClock::anchored_at(start_ns), Watermark::new(0));
        let (mut seq, mut segment) = (1, 0);
        for (digest, entries) in lives {
            disk.create_segment(segment).expect("created");
            let header = SegmentHeader::for_life(identity, start_ns, *digest);
            let mut writer = JournalWriter::new(disk, header, segment, 0, 4_096);
            for entry in entries {
                let ts = start_ns + seq;
                let record = match entry {
                    Entry::Signed(account, nonce, command, key_seed) => {
                        signed_record(seq, ts, *account, *nonce, command, *key_seed)
                    }
                    Entry::Operator(command) => operator_record(seq, ts, command),
                    Entry::Record(record) => JournalRecord { seq, ts, ..*record },
                };
                writer.append(&record.to_words()[..record.len_words()], &clock, &durable).expect("appended");
                seq += 1;
            }
            writer.flush(&clock, &durable).expect("flushed");
            let position = writer.position();
            (disk, _) = writer.into_parts();
            segment = position.segment + 1;
        }
        disk
    }

    fn deposit(account: AccountId) -> Entry {
        Entry::Operator(Command::Deposit(Deposit { amount: 1_000_000, account }))
    }

    #[test]
    fn a_journal_of_genuine_messages_passes_across_segments_and_a_key_change() {
        // Life 1 with keys from seed 1; after a "restart", account 9's key is replaced
        // (seed 2), and its segment names the new registry.
        let first = registry(1, DEPLOYMENT, [3, 9]);
        let keys_2 = [(3, *signing_key(1, 3).verifying_key()), (9, *signing_key(2, 9).verifying_key())];
        let second = KeyRegistry::from_keys(DEPLOYMENT, &keys_2, VERIFIER).expect("valid");
        assert_ne!(first.digest(), second.digest());
        let mut life_1 = vec![deposit(3), deposit(9)];
        // Enough records to cross 4 KiB segments several times (152 bytes each).
        for n in 1..=60 {
            life_1.push(Entry::Signed(9, n, place(9, n as u32), 1));
            life_1.push(Entry::Signed(3, n, cancel(3, n as u32), 1));
        }
        let life_2 = vec![
            Entry::Signed(9, 61, place(9, 61), 2),
            Entry::Operator(Command::SetMark(SetMark { price: 5, market: 3 })),
        ];
        let mut disk = journal(&[(first.digest(), life_1), (second.digest(), life_2)]);
        let report = verify_files(&mut disk, &[second.clone(), first.clone()], 3);
        assert!(report.passed(), "{report}");
        assert_eq!((report.signed, report.pre_verified, report.operator), (121, 0, 3));

        // Without the second registry, the second life's signed record can't be checked.
        let report = verify_files(&mut disk, &[first], 3);
        assert_eq!(report.failures, 1, "{report}");
        assert!(report.listed[0].what.contains("no registry given has the digest"), "{report}");
    }

    #[test]
    fn a_signature_by_another_key_fails_and_names_the_record() {
        let keys = registry(1, DEPLOYMENT, [3, 9]);
        let entries = vec![
            Entry::Signed(9, 1, place(9, 1), 1),
            Entry::Signed(9, 2, place(9, 2), 7), // signed with another key
            Entry::Signed(3, 1, place(3, 1), 1),
        ];
        let mut disk = journal(&[(keys.digest(), entries)]);
        let report = verify_files(&mut disk, &[keys], 2);
        assert_eq!(report.failures, 1, "{report}");
        assert_eq!(report.listed[0].seq, 2);
        assert!(report.listed[0].what.contains("BadSignature"), "{report}");
    }

    #[test]
    fn a_copied_record_fails_the_nonce_order() {
        let keys = registry(1, DEPLOYMENT, [9]);
        // The same genuine message twice (a replay the gateway would have refused).
        let entries = vec![Entry::Signed(9, 5, place(9, 1), 1), Entry::Signed(9, 5, place(9, 1), 1)];
        let mut disk = journal(&[(keys.digest(), entries)]);
        let report = verify_files(&mut disk, &[keys], 2);
        assert_eq!(report.failures, 1, "{report}");
        assert!(report.listed[0].what.contains("nonce 5 is not above its previous one, 5"), "{report}");
    }

    #[test]
    fn an_account_without_a_key_fails() {
        let keys = registry(1, DEPLOYMENT, [9]);
        let entries = vec![Entry::Signed(11, 1, place(11, 1), 1)];
        let mut disk = journal(&[(keys.digest(), entries)]);
        let report = verify_files(&mut disk, &[keys], 1);
        assert_eq!(report.failures, 1);
        assert!(report.listed[0].what.contains("account 11 has no key"), "{report}");
    }

    #[test]
    fn a_registry_for_another_deployment_fails() {
        let other = registry(1, DEPLOYMENT + 1, [9]);
        let mut disk = journal(&[(other.digest(), vec![Entry::Signed(9, 1, place(9, 1), 1)])]);
        let report = verify_files(&mut disk, &[other], 1);
        assert_eq!(report.failures, 1);
        assert!(report.listed[0].what.contains("is for deployment 5, the journal's is 4"), "{report}");
    }

    #[test]
    fn records_that_break_the_kind_rules_fail_even_if_the_scan_let_them_through() {
        // `scan_journal` refuses these as errors; the audit's own rule is checked here
        // directly.
        let place_9 = place(9, 1);
        let not_owner = signed_record(1, 1, 7, 1, &place_9, 1);
        assert!(kind_fits(&not_owner, &place_9, InjectionMode::Signed).unwrap_err().contains("account 9's"));
        let mark = Command::SetMark(SetMark { price: 1, market: 1 });
        let client_mark =
            JournalRecord { command: encode_command(&mark), ..signed_record(1, 1, 9, 1, &mark, 1) };
        assert!(
            kind_fits(&client_mark, &mark, InjectionMode::Signed)
                .unwrap_err()
                .contains("operator command tag 7")
        );
        let operator_place = operator_record(1, 1, &place_9);
        assert!(kind_fits(&operator_place, &place_9, InjectionMode::Signed).is_err());
        let signed = signed_record(1, 1, 9, 1, &place_9, 1);
        assert!(kind_fits(&signed, &place_9, InjectionMode::PreVerified).unwrap_err().contains("kind-1"));
        let pre_verified =
            JournalRecord { meta: Meta { source: Source::PreVerifiedClient, ..signed.meta }, ..signed };
        assert!(kind_fits(&pre_verified, &place_9, InjectionMode::Signed).unwrap_err().contains("kind-2"));
        assert_eq!(kind_fits(&signed, &place_9, InjectionMode::Signed), Ok(()));
        assert_eq!(kind_fits(&pre_verified, &place_9, InjectionMode::PreVerified), Ok(()));
    }

    #[test]
    fn a_corrupt_record_stops_the_audit_with_a_failure() {
        let keys = registry(1, DEPLOYMENT, [9]);
        let entries: Vec<_> = (1..=3).map(|n| Entry::Signed(9, n, place(9, n as u32), 1)).collect();
        let mut disk = journal(&[(keys.digest(), entries)]);
        // The second record's tag becomes a deposit's, and its CRC is fixed up: the scan
        // sees a valid record that breaks 11.3's rules, and stops with an error.
        let offset = HEADER_BYTES as u64 + 152 + 40;
        disk.corrupt(0, offset, 1 ^ 4);
        let mut record = [0u8; 152];
        disk.read_at(0, HEADER_BYTES as u64 + 152, &mut record).expect("read");
        let crc = pipeline::journal::format::record_crc(&record);
        disk.write_at(0, HEADER_BYTES as u64 + 156, &crc.to_le_bytes()).expect("written");
        let report = verify_files(&mut disk, &[keys], 1);
        assert!(!report.passed());
        assert!(report.listed.iter().any(|f| f.what.contains("can't be read to its end")), "{report}");
        assert_eq!(report.signed, 1, "the record before it was still verified");
    }

    #[test]
    fn an_empty_or_missing_journal() {
        let mut disk = SimDisk::new(SEGMENT_BYTES);
        assert_eq!(verify_files(&mut disk, &[], 1), AuditReport::default());
        disk.create_segment(0).expect("created");
        assert!(verify_files(&mut disk, &[], 1).passed(), "a segment not used yet");
        // A header with a valid CRC but another magic is an error, never an empty journal.
        let identity = JournalIdentity::new(DEPLOYMENT, InjectionMode::Signed, EngineOptions::default());
        let mut header =
            SegmentHeader { first_seq: 1, ..SegmentHeader::for_life(identity, 1, [0; 32]) }.encode();
        header[0] = b'X';
        let crc = pipeline::crc32c::crc32c(&header[..124]);
        header[124..].copy_from_slice(&crc.to_le_bytes());
        disk.write_at(0, 0, &header).expect("written");
        let report = verify_files(&mut disk, &[], 1);
        assert!(report.to_string().contains("the journal can't be read"), "{report}");
        let missing = std::env::temp_dir().join(format!("gateway-audit-missing-{}", std::process::id()));
        let report = verify_journal(&missing, &[]);
        assert!(!report.passed());
        assert!(report.to_string().contains("opening the journal"), "{report}");
    }

    #[test]
    fn the_report_lists_the_first_failures_and_counts_the_rest() {
        let mut report = AuditReport::default();
        for seq in 1..=(LISTED_FAILURES as u64 + 5) {
            report.fail(seq, "bad");
        }
        assert_eq!((report.failures, report.listed.len()), (105, 100));
        assert!(report.to_string().ends_with("... and 5 more"));
    }
    // ---- The EIP-712 scheme (module docs; D-033) ----

    /// The EIP-712 journals' clock: every record's time is this many nanoseconds, plus its
    /// seq, so within one millisecond of `START_MS`.
    const START: u64 = 1_790_000_000_000_000_000;
    const START_MS: u64 = START / 1_000_000;

    fn eip712_identity() -> JournalIdentity {
        JournalIdentity::new(DEPLOYMENT, InjectionMode::Signed, EngineOptions::default())
            .with_auth(AuthScheme::Eip712)
    }

    /// The kind-1 record of an EIP-712 message from `account` with `salt` and `ts_ms`, signed
    /// by the key derived from `key_seed`, as the gateway forwards it (the salt in the
    /// nonce's word, `ts` in the expiry's).
    fn eip712_record(
        account: AccountId,
        salt: u64,
        ts_ms: u64,
        command: &Command,
        key_seed: u64,
    ) -> JournalRecord {
        let message =
            message_eip712(&signing_key(key_seed, account), DEPLOYMENT, account, salt, ts_ms, command);
        JournalRecord {
            seq: 0,
            ts: 0,
            meta: Meta { source: Source::SignedClient, lane: (account % 2) as u16, account },
            nonce: salt,
            command: encode_command(command),
            expires_at: ts_ms,
            signature: pipeline::records::signature_words(&message),
        }
    }

    /// An EIP-712 journal of `entries`, one life, with `keys`.
    fn eip712_journal(keys: &KeyRegistry, entries: Vec<Entry>) -> SimDisk {
        journal_of(eip712_identity(), START, &[(keys.digest(), entries)])
    }

    #[test]
    fn an_eip712_journal_of_genuine_messages_passes() {
        let keys = registry(1, DEPLOYMENT, [3, 9]);
        let mut entries = vec![deposit(3), deposit(9)];
        // Enough records to cross 4 KiB segments several times; salts repeat across
        // accounts and timestamps, which is allowed: the request is all three.
        for n in 1..=40u32 {
            let ts_ms = START_MS - u64::from(n) * 1_000;
            entries.push(Entry::Record(eip712_record(9, u64::from(n % 4), ts_ms, &place(9, n), 1)));
            entries.push(Entry::Record(eip712_record(3, u64::from(n % 4), ts_ms, &cancel(3, n), 1)));
            entries.push(Entry::Record(eip712_record(3, 7, START_MS + u64::from(n), &modify(3, n), 1)));
        }
        let mut disk = eip712_journal(&keys, entries);
        let report = verify_files(&mut disk, std::slice::from_ref(&keys), 3);
        assert!(report.passed(), "{report}");
        assert_eq!((report.signed, report.operator), (120, 2));
        // The same records in a journal whose header says perp: every signature fails, since
        // the audit then checks the 72 bytes of the perp scheme.
        let identity = JournalIdentity::new(DEPLOYMENT, InjectionMode::Signed, EngineOptions::default());
        let entries = vec![Entry::Record(eip712_record(9, 1, START_MS, &place(9, 1), 1))];
        let mut perp = journal_of(identity, START, &[(keys.digest(), entries)]);
        let report = verify_files(&mut perp, &[keys], 1);
        assert_eq!(report.failures, 1, "{report}");
        assert!(report.listed[0].what.contains("BadSignature"), "{report}");
    }

    #[test]
    fn an_eip712_record_with_a_field_changed_after_signing_fails() {
        let keys = registry(1, DEPLOYMENT, [9]);
        let genuine = eip712_record(9, 5, START_MS, &place(9, 1), 1);
        let mut disk = eip712_journal(&keys, vec![Entry::Record(genuine)]);
        assert!(
            verify_files(&mut disk, std::slice::from_ref(&keys), 1).passed(),
            "so each failure is the edit's"
        );
        let signature: [u8; SIGNATURE_BYTES] = to_le_bytes(&genuine.signature);
        let high_s_twin = from_le_bytes(&crate::test_support::high_s_twin(&signature));
        let edits = [
            ("the salt", JournalRecord { nonce: 6, ..genuine }, "BadSignature"),
            ("the timestamp", JournalRecord { expires_at: START_MS - 1, ..genuine }, "BadSignature"),
            (
                "the command",
                JournalRecord { command: encode_command(&place(9, 2)), ..genuine },
                "BadSignature",
            ),
            ("another key", eip712_record(9, 5, START_MS, &place(9, 1), 7), "BadSignature"),
            ("no signature", JournalRecord { signature: [0; SIGNATURE_WORDS], ..genuine }, "BadSignature"),
            ("the high-S twin", JournalRecord { signature: high_s_twin, ..genuine }, "HighS"),
        ];
        for (what, record, reason) in edits {
            let mut disk = eip712_journal(&keys, vec![Entry::Record(record)]);
            let report = verify_files(&mut disk, std::slice::from_ref(&keys), 1);
            assert_eq!(report.failures, 1, "{what}: {report}");
            assert!(report.listed[0].what.contains(reason), "{what}: {report}");
        }
    }

    #[test]
    fn a_changed_market_on_an_eip712_cancel_or_modify_is_not_seen() {
        // The limitation of module docs, "What it proves": the scheme signs a cancel's and a
        // modify's order id, not their market, so the audit passes a journal whose market
        // was changed there. (A place's market is signed, and its edit fails above.)
        let keys = registry(1, DEPLOYMENT, [9]);
        for (salt, command) in [(5, cancel(9, 1)), (6, modify(9, 1))] {
            let genuine = eip712_record(9, salt, START_MS, &command, 1);
            let changed = with_market(&command, 2);
            assert_ne!(changed, command);
            let edited = JournalRecord { command: encode_command(&changed), ..genuine };
            let mut disk = eip712_journal(&keys, vec![Entry::Record(edited)]);
            let report = verify_files(&mut disk, std::slice::from_ref(&keys), 1);
            assert!(report.passed(), "{report}");
            assert_eq!(report.signed, 1);
        }
    }

    #[test]
    fn an_eip712_cancel_or_modify_copied_to_other_markets_passes_next_to_the_genuine_one() {
        // Module docs, "no request repeats": the request includes the market, as the
        // gateway's key does, so copies the gateway accepted as other requests (and the
        // engine rejected) pass here, next to the genuine record, in any order. An exact
        // repeat, of the genuine record or of a copy, still fails.
        let keys = registry(1, DEPLOYMENT, [9]);
        for (salt, command) in [(5, cancel(9, 1)), (6, modify(9, 1))] {
            let genuine = eip712_record(9, salt, START_MS, &command, 1);
            let on =
                |market| JournalRecord { command: encode_command(&with_market(&command, market)), ..genuine };
            let journal_with = |records: [JournalRecord; 3]| {
                eip712_journal(&keys, records.into_iter().map(Entry::Record).collect())
            };
            let mut disk = journal_with([on(2), genuine, on(MarketId::MAX)]);
            let report = verify_files(&mut disk, std::slice::from_ref(&keys), 1);
            assert!(report.passed(), "{report}");
            assert_eq!(report.signed, 3, "and each signature verified");
            for (records, market) in [([on(2), genuine, genuine], 3), ([genuine, on(2), on(2)], 2)] {
                let mut disk = journal_with(records);
                let report = verify_files(&mut disk, std::slice::from_ref(&keys), 1);
                assert_eq!(report.failures, 1, "{report}");
                assert_eq!(report.listed[0].seq, 3);
                let what = format!("(salt {salt}, ts {START_MS}, market {market}) is journaled twice");
                assert!(report.listed[0].what.contains(&what), "{report}");
            }
        }
    }

    #[test]
    fn a_repeated_eip712_request_fails() {
        let keys = registry(1, DEPLOYMENT, [3, 9]);
        let request = eip712_record(9, 5, START_MS, &place(9, 1), 1);
        // The same request twice (a replay the gateway would have refused), and the same
        // salt and timestamp for another account, which is another request.
        let entries = vec![
            Entry::Record(request),
            Entry::Record(eip712_record(3, 5, START_MS, &place(3, 1), 1)),
            Entry::Record(request),
        ];
        let mut disk = eip712_journal(&keys, entries);
        let report = verify_files(&mut disk, &[keys], 2);
        assert_eq!(report.failures, 1, "{report}");
        assert_eq!(report.listed[0].seq, 3);
        assert!(
            report.listed[0].what.contains("(salt 5, ts 1790000000000, market 3) is journaled twice"),
            "{report}"
        );
    }

    #[test]
    fn an_eip712_timestamp_outside_the_window_of_its_record_fails() {
        let keys = registry(1, DEPLOYMENT, [9]);
        let oldest = START_MS - MAX_AGE_MS - SEQUENCING_SLACK_MS;
        let latest = START_MS + MAX_AHEAD_MS;
        let at =
            |salt: u64, ts_ms: u64| Entry::Record(eip712_record(9, salt, ts_ms, &place(9, salt as u32), 1));
        // The edges pass.
        let mut disk = eip712_journal(&keys, vec![at(1, oldest), at(2, latest)]);
        let report = verify_files(&mut disk, std::slice::from_ref(&keys), 1);
        assert!(report.passed(), "{report}");
        // One millisecond beyond either fails, though the signatures are genuine.
        let mut disk = eip712_journal(&keys, vec![at(1, oldest - 1), at(2, latest + 1), at(3, START_MS)]);
        let report = verify_files(&mut disk, &[keys], 1);
        assert_eq!(report.failures, 2, "{report}");
        assert!(report.listed[0].what.contains("more than 5 minutes and 10000 ms before"), "{report}");
        assert!(report.listed[1].what.contains("more than 60 s after the record's time"), "{report}");
        assert_eq!(report.signed, 3, "and every signature was still checked");
    }
}
