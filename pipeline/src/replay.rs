//! Replay: the journal alone rebuilds the engine and the gateways' nonces; and the replay
//! test, which checks that it rebuilds exactly what the live run built
//! (`docs/PIPELINE.md` 13.1, 13.3 and 6.3; `docs/DECISIONS.md` D-029).
//!
//! **Contract** ([`replay`], 13.1). After recovery (11.8) has found the journal's valid
//! records and made them durable, replay builds a fresh engine with the options and hash
//! seed of segment 0's header (D-011), and applies every record's command in order. It
//! returns the engine, the highest nonce per account over the client records (kinds 1 and
//! 2: the nonce table the gateways restart with, 6.3), the next seq, the last `ts`, and,
//! if asked, every event as an 8-word slot (`seq`, then EVT56: the capture format of 13.2).
//! It needs no key registry, no clock and no threads, and it doesn't verify signatures
//! (that is the separate audit of 13.4).
//!
//! **Poison pills** (2.8). Replay installs the abort-on-panic hook and keeps the seq and
//! the decoded command it is applying in thread-locals, so a command that makes the engine
//! panic stops replay with a message naming both. The cure is a code fix, never an edit to
//! the journal.
//!
//! **The replay test** ([`replay_test`], 13.3). After a run with capture on: replay the
//! journal from disk, and require the same number of records as the live run sequenced
//! and applied, the replay's events identical to the live capture slot for slot (the first
//! difference is reported with both events decoded), and equal `EngineSnapshot`s; then
//! the same once more with another hash seed (D-011's claim that the seed changes nothing
//! but speed). If the capture was incomplete or the journal discarded, the answer is "not
//! checked", never "identical" on a partial comparison. What it proves: the pipeline adds
//! nothing to the engine's input that isn't in the journal, and the codec round-trips
//! every command. Not that the engine is right, nor that the journal survives a crash (the
//! crash tests do that).
//!
//! **Complexity.** One pass over the journal (through `scan_journal`, which re-checks every
//! rule of 11.8 on the way), one `apply` per record; the capture takes 64 bytes per event.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use engine::book::{Book, OrderBook};
use engine::engine::{Engine, EngineOptions, EngineSnapshot};
use engine::event::{Event, EventSink};
use engine::mode::{Fast, Mode};
use engine::types::AccountId;

use crate::codec::decode_event;
use crate::journal::JournalError;
use crate::journal::files::{JournalFiles, StdFiles};
use crate::journal::format::DEFAULT_SEGMENT_BYTES;
use crate::journal::recovery::{Recovered, scan_journal};
use crate::panic::{install_abort_on_panic, set_current_command, set_current_seq};
use crate::records::{EVENT_SLOT_WORDS, Source, event_slot, is_trailer};

/// The highest nonce each account has used, from the journal's client records (6.3):
/// `Gateway::new` takes it after a restart. An account that isn't listed has used none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NonceTable(BTreeMap<AccountId, u64>);

impl NonceTable {
    pub fn new() -> NonceTable {
        NonceTable::default()
    }

    /// The account's last used nonce; 0 if it has used none (the first valid nonce is 1).
    pub fn get(&self, account: AccountId) -> u64 {
        self.0.get(&account).copied().unwrap_or(0)
    }

    /// Notes that `account` used `nonce`, keeping the highest.
    pub fn note(&mut self, account: AccountId, nonce: u64) {
        let last = self.0.entry(account).or_insert(0);
        *last = (*last).max(nonce);
    }

    /// Accounts with a nonce.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every account and its last nonce, in account order.
    pub fn iter(&self) -> impl Iterator<Item = (AccountId, u64)> + '_ {
        self.0.iter().map(|(&account, &nonce)| (account, nonce))
    }
}

/// What a replay rebuilt (PIPELINE.md 19.2). `B` and `M` are the engine's book and mode:
/// production's `Book` and `Fast` by default; the smoke test also replays through the
/// reference engine.
#[derive(Debug)]
pub struct ReplayOutput<B = Book, M = Fast> {
    pub engine: Engine<B, M>,
    pub nonces: NonceTable,
    /// The seq a restarted pipeline gives its first command.
    pub next_seq: u64,
    /// The last record's `ts`: a restarted clock is anchored above it (9.2).
    pub last_ts: u64,
    /// Records applied.
    pub records: u64,
    /// Every event as an 8-word slot, if asked for.
    pub capture: Option<Vec<u64>>,
}

/// Replays the recovered journal in `dir` into a fresh `Engine<Book, Fast>` (module docs).
/// `seed_override` replaces the header's hash seed (the replay test's second run).
pub fn replay(
    dir: &Path,
    recovered: &Recovered,
    seed_override: Option<u64>,
    capture: bool,
) -> Result<ReplayOutput, JournalError> {
    replay_as(dir, recovered, seed_override, capture)
}

/// [`replay`] into any engine: `replay_as::<ReferenceBook, Naive>` replays through the
/// reference engine.
pub fn replay_as<B: OrderBook, M: Mode>(
    dir: &Path,
    recovered: &Recovered,
    seed_override: Option<u64>,
    capture: bool,
) -> Result<ReplayOutput<B, M>, JournalError> {
    let mut files = StdFiles::open_existing(dir, DEFAULT_SEGMENT_BYTES)
        .map_err(|e| JournalError::io(format!("opening the journal in {}", dir.display()), e))?;
    replay_files(&mut files, recovered, seed_override, capture)
}

/// [`replay_as`] on any [`JournalFiles`].
pub fn replay_files<B: OrderBook, M: Mode>(
    files: &mut impl JournalFiles,
    recovered: &Recovered,
    seed_override: Option<u64>,
    capture: bool,
) -> Result<ReplayOutput<B, M>, JournalError> {
    install_abort_on_panic();
    let from_header = recovered.identity.engine;
    let options =
        EngineOptions { id_hash_seed: seed_override.unwrap_or(from_header.id_hash_seed), ..from_header };
    let mut engine = Engine::<B, M>::new(options);
    let mut sink = CaptureSink { seq: 0, slots: capture.then(Vec::new) };
    let mut nonces = NonceTable::new();
    let scan = scan_journal(files, |_, record, command| {
        set_current_seq(record.seq);
        set_current_command(Some(*command));
        sink.seq = record.seq;
        engine.apply(command, &mut sink);
        if record.meta.source != Source::Operator {
            nonces.note(record.meta.account, record.nonce);
        }
    })?;
    set_current_seq(0);
    set_current_command(None);
    if scan.end != recovered.end || scan.records != recovered.records {
        return Err(JournalError::Refused(format!(
            "the journal changed since recovery: it now ends at {} with {} records, not at {} with {}",
            scan.end, scan.records, recovered.end, recovered.records
        )));
    }
    Ok(ReplayOutput {
        engine,
        nonces,
        next_seq: scan.next_seq,
        last_ts: scan.last_ts,
        records: scan.records,
        capture: sink.slots,
    })
}

/// Replay's event sink: every event as a capture slot, or nowhere.
struct CaptureSink {
    seq: u64,
    slots: Option<Vec<u64>>,
}

impl EventSink for CaptureSink {
    fn emit(&mut self, event: Event) {
        if let Some(slots) = &mut self.slots {
            slots.extend_from_slice(&event_slot(self.seq, &event));
        }
    }
}

// ---------------------------------------------------------------------------------------
// The replay test (13.3).

/// What the live run hands the replay test.
#[derive(Clone, Copy, Debug)]
pub struct LiveRun<'a> {
    /// The live engine's snapshot, taken after the run.
    pub snapshot: &'a EngineSnapshot,
    /// The gate's capture: `None` if capture was off.
    pub capture: Option<&'a [u64]>,
    pub capture_incomplete: bool,
    pub journal_discarded: bool,
    /// The seq of this life's first command (1 for a run on a new journal): the capture
    /// holds this life's events only.
    pub first_seq: u64,
    /// Commands the sequencer issued and the core applied in this life.
    pub sequenced: u64,
    pub applied: u64,
}

/// The replay test's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayVerdict {
    /// The capture was off or incomplete, or the journal discarded: nothing was compared.
    NotChecked(String),
    Identical(ReplayReport),
    /// The first difference found.
    Different(String),
}

/// The report line of 13.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayReport {
    pub records: u64,
    pub events: u64,
    /// The first replay's rate.
    pub records_per_second: u64,
}

/// The replay test of 13.3 (module docs). `other_seed` is the second replay's hash seed.
pub fn replay_test(
    dir: &Path,
    recovered: &Recovered,
    live: &LiveRun<'_>,
    other_seed: u64,
) -> Result<ReplayVerdict, JournalError> {
    if live.journal_discarded {
        return Ok(ReplayVerdict::NotChecked("the journal was discarded (11.6)".into()));
    }
    let Some(capture) = live.capture else {
        return Ok(ReplayVerdict::NotChecked("capture was off".into()));
    };
    if live.capture_incomplete {
        return Ok(ReplayVerdict::NotChecked("the capture is incomplete".into()));
    }
    let started = Instant::now();
    let first = replay(dir, recovered, None, true)?;
    let elapsed_ns = started.elapsed().as_nanos().max(1);
    if let Some(difference) = compare(&first, live, capture) {
        return Ok(ReplayVerdict::Different(difference));
    }
    let second = replay(dir, recovered, Some(other_seed), true)?;
    if let Some(difference) = compare(&second, live, capture) {
        return Ok(ReplayVerdict::Different(format!("with another hash seed: {difference}")));
    }
    Ok(ReplayVerdict::Identical(ReplayReport {
        records: first.records,
        events: (capture.len() / EVENT_SLOT_WORDS) as u64,
        records_per_second: u64::try_from(u128::from(first.records) * 1_000_000_000 / elapsed_ns)
            .unwrap_or(u64::MAX),
    }))
}

/// The first way a replay differs from the live run, if any.
fn compare(replayed: &ReplayOutput, live: &LiveRun<'_>, capture: &[u64]) -> Option<String> {
    let life_records = replayed.records - (live.first_seq - 1);
    if life_records != live.sequenced || life_records != live.applied {
        return Some(format!(
            "the journal holds {life_records} records of this run, but the sequencer issued {} and the core applied {}",
            live.sequenced, live.applied
        ));
    }
    let replayed_events = replayed.capture.as_deref().expect("replayed with capture");
    let this_life = life_events(replayed_events, live.first_seq);
    if let Some(difference) = first_difference(capture, this_life) {
        return Some(difference);
    }
    let snapshot = replayed.engine.snapshot();
    if snapshot != *live.snapshot {
        return Some(format!(
            "the engine snapshots differ: {}",
            snapshot_difference(live.snapshot, &snapshot)
        ));
    }
    None
}

/// The slots of commands with seq `first_seq` or later.
fn life_events(slots: &[u64], first_seq: u64) -> &[u64] {
    let start = slots.as_chunks::<EVENT_SLOT_WORDS>().0.iter().position(|slot| slot[0] >= first_seq);
    start.map_or(&[], |index| &slots[index * EVENT_SLOT_WORDS..])
}

/// Compares two event streams slot for slot; the first difference, with both slots
/// decoded, or `None` if they are identical.
pub fn first_difference(live: &[u64], replayed: &[u64]) -> Option<String> {
    let pairs = live.as_chunks::<EVENT_SLOT_WORDS>().0.iter().zip(replayed.as_chunks::<EVENT_SLOT_WORDS>().0);
    for (index, (a, b)) in pairs.enumerate() {
        if a != b {
            return Some(format!(
                "event slot {index} differs: live seq {} {}, replayed seq {} {}",
                a[0],
                describe_slot(a),
                b[0],
                describe_slot(b)
            ));
        }
    }
    let (live_count, replayed_count) = (live.len() / EVENT_SLOT_WORDS, replayed.len() / EVENT_SLOT_WORDS);
    (live_count != replayed_count)
        .then(|| format!("the live run released {live_count} events, the replay emitted {replayed_count}"))
}

/// One slot, decoded, for a report.
fn describe_slot(slot: &[u64; EVENT_SLOT_WORDS]) -> String {
    if is_trailer(slot) {
        return "(a trailer)".into();
    }
    match decode_event(slot[1..].try_into().expect("7 words")) {
        Ok(event) => format!("{event:?}"),
        Err(e) => format!("(not an event: {e})"),
    }
}

/// Where two snapshots first differ, roughly: which account, market or fund figure.
fn snapshot_difference(live: &EngineSnapshot, replayed: &EngineSnapshot) -> String {
    if live.accounts != replayed.accounts {
        let first = live.accounts.iter().zip(&replayed.accounts).find(|(a, b)| a != b);
        return match first {
            Some((a, b)) => format!("account {}: live {a:?}, replayed {b:?}", a.account),
            None => format!("{} accounts live, {} replayed", live.accounts.len(), replayed.accounts.len()),
        };
    }
    if live.markets != replayed.markets {
        let first = live.markets.iter().zip(&replayed.markets).find(|(a, b)| a != b);
        return match first {
            Some((a, _)) => format!("market {}", a.params.market),
            None => format!("{} markets live, {} replayed", live.markets.len(), replayed.markets.len()),
        };
    }
    format!(
        "the fund: live balance {} upnl {} uncovered {} deposits {}, replayed {} {} {} {}",
        live.fund_balance,
        live.fund_upnl_total,
        live.last_reported_uncovered,
        live.net_deposits,
        replayed.fund_balance,
        replayed.fund_upnl_total,
        replayed.last_reported_uncovered,
        replayed.net_deposits
    )
}

#[cfg(test)]
mod tests {
    //! Replay itself installs the abort-on-panic hook, so it is tested in
    //! `pipeline/tests/` (a test binary here also holds `#[should_panic]` tests). These
    //! test the parts that don't.
    use super::*;
    use engine::event::Ack;
    use engine::types::OrderId;

    fn ack(seq: u64, id: u64) -> [u64; 8] {
        event_slot(seq, &Event::Ack(Ack { order_id: OrderId::new(id) }))
    }

    #[test]
    fn the_nonce_table_keeps_the_highest_nonce_per_account() {
        let mut table = NonceTable::new();
        let (four, nine) = (AccountId::new(4), AccountId::new(9));
        assert_eq!(table.get(nine), 0);
        table.note(nine, 5);
        table.note(nine, 3);
        table.note(four, 1);
        assert_eq!((table.get(nine), table.get(four), table.len()), (5, 1, 2));
        assert_eq!(table.iter().collect::<Vec<_>>(), [(four, 1), (nine, 5)]);
    }

    #[test]
    fn the_first_difference_is_found_and_decoded() {
        let a = [ack(1, 10), ack(2, 20)].concat();
        assert_eq!(first_difference(&a, &a), None);
        let b = [ack(1, 10), ack(2, 21)].concat();
        let difference = first_difference(&a, &b).expect("they differ");
        assert!(difference.contains("event slot 1"), "{difference}");
        assert!(difference.contains("order_id: 20") && difference.contains("order_id: 21"), "{difference}");
        let shorter = first_difference(&a, &a[..8]).expect("lengths differ");
        assert!(shorter.contains("released 2 events, the replay emitted 1"), "{shorter}");
    }

    #[test]
    fn a_later_life_compares_only_its_own_events() {
        let all = [ack(1, 1), ack(3, 3), ack(3, 4), ack(5, 5)].concat();
        assert_eq!(life_events(&all, 3), &all[8..]);
        assert_eq!(life_events(&all, 1), &all[..]);
        assert!(life_events(&all, 6).is_empty());
    }
}
