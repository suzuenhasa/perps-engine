//! The gateway thread: takes messages from its ingress ring, checks them, and forwards the
//! accepted ones into its lane (`docs/PIPELINE.md` 7.3; also 2.3, 2.4, 2.8 and 15.4).
//!
//! **Contract.**
//! - Messages are taken in ring order, up to [`GATEWAY_BATCH`] at a time. For each: read
//!   the clock (`t_gw_in`, which is also the clock the expiry, or in the EIP-712 scheme the
//!   timestamp window, is checked against), run
//!   [`Gateway::check`] with the lane's free slots, and if it is accepted, read the clock
//!   again (`t_gw_out`), write its `ClientRecord` (3.3) into the lane and publish it at
//!   once. Publishing each record, not each batch, keeps a verified message from waiting
//!   for the rest of its batch's verifications (up to 31 × 50 µs, about 1.6 ms).
//! - A reject is counted by reason (and `Busy` also by command tag), in total and, if the
//!   message's `t_sched` is inside the measured window, in the window's counts, which the
//!   harness adds to the end-to-end histograms as never served (15.4). A reject is never
//!   written anywhere else: nothing happened in the exchange (7.1).
//! - The batch's ingress slots are freed after the batch.
//! - **Stopping follows the data** (2.8): the thread stops once its ingress ring is closed
//!   and drained. Returning drops the lane's producer, which closes the lane, so the
//!   sequencer drains it and moves on. There is no stop flag.
//! - **A panic stops the whole process.** [`spawn`] installs the abort-on-panic hook (2.8),
//!   as `Pipeline::start` does: a gateway that died quietly would close its lane, and the
//!   run would carry on without its accounts' messages.
//!
//! **Shared with main while the run goes** (3.2): the total rejected, which main's
//! barriers wait on (14.4), and the thread's busy time and id (15.4), all single-writer
//! counters on their own lines ([`GatewayCounters`]). The per-reason counts are the
//! thread's own and are returned when it is joined ([`GatewayStats`]).
//!
//! **Complexity.** Per message: one or two clock reads, the checks (one verification if it
//! gets that far), and for an accepted message one 20-word ring write and one Release
//! store.

use std::sync::Arc;
use std::thread::JoinHandle;

use pipeline::affinity::pin_current_thread;
use pipeline::clock::RunClock;
use pipeline::counters::{BusyMeter, SharedCounter, ThreadCounters};
use pipeline::gate::{Phase, Phases};
use pipeline::idle::IdleStrategy;
use pipeline::panic::install_abort_on_panic;
use pipeline::records::{ClientRecord, IngressSlot, Meta, Source};
use pipeline::ring::{Consumer, Producer};

use crate::check::{CANCEL_HEADROOM, Gateway, GatewayReject};
use crate::wire;

/// Messages taken from the ingress ring per batch, at most (7.3).
pub const GATEWAY_BATCH: usize = 32;
/// Slots in each `ingress[g]` ring, sender → gateway (2.3). The harness creates the rings.
pub const INGRESS_CAPACITY: usize = 1_024;
/// Slots in each `lane[g]` ring, gateway → sequencer (2.3).
pub const LANE_CAPACITY: usize = 1_024;
/// What the loop asks the lane for: one more than the cancel headroom. The ring reloads its
/// view of the sequencer's progress only when its cached count is below this, so the count
/// is exact wherever check 10 could refuse (3.1).
const LANE_WANTED: usize = CANCEL_HEADROOM + 1;
/// Command tags that can be `Busy`: place, cancel and modify.
const CLIENT_TAGS: usize = 3;

/// Rejects by reason, and `Busy` also by command tag (7.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RejectCounts {
    by_reason: [u64; GatewayReject::COUNT],
    /// `Busy` for places, cancels and modifies (tags 1, 2, 3).
    busy_by_tag: [u64; CLIENT_TAGS],
}

impl RejectCounts {
    /// Counts one reject of a message whose command tag is `tag`.
    pub fn count(&mut self, reason: GatewayReject, tag: u8) {
        self.by_reason[reason.index()] += 1;
        if reason == GatewayReject::Busy {
            // `Busy` is check 10, after check 3: the tag is 1, 2 or 3.
            self.busy_by_tag[usize::from(tag) - 1] += 1;
        }
    }

    /// Rejects for `reason`.
    pub fn get(&self, reason: GatewayReject) -> u64 {
        self.by_reason[reason.index()]
    }

    /// `Busy` rejects of command tag `tag` (1 place, 2 cancel, 3 modify).
    pub fn busy(&self, tag: u8) -> u64 {
        self.busy_by_tag[usize::from(tag) - 1]
    }

    /// Every reject.
    pub fn total(&self) -> u64 {
        self.by_reason.iter().sum()
    }

    /// Adds `other`'s counts (to sum over the gateways).
    pub fn add(&mut self, other: &RejectCounts) {
        for (mine, theirs) in self.by_reason.iter_mut().zip(other.by_reason) {
            *mine += theirs;
        }
        for (mine, theirs) in self.busy_by_tag.iter_mut().zip(other.busy_by_tag) {
            *mine += theirs;
        }
    }
}

/// What a gateway thread counted, returned when it is joined.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GatewayStats {
    /// Messages forwarded into the lane: each one's nonce (or request, in the EIP-712
    /// scheme) was used up.
    pub forwarded: u64,
    /// Every reject.
    pub rejects: RejectCounts,
    /// The rejects of messages scheduled inside the measured window (15.4).
    pub window_rejects: RejectCounts,
}

/// What a gateway thread shares with main while the run goes (module docs).
#[derive(Debug, Default)]
pub struct GatewayCounters {
    /// Busy time and the OS thread id (15.4).
    pub thread: ThreadCounters,
    /// Messages rejected so far. Main's barriers wait until
    /// `released + gateway rejects + ingress drops == messages sent` (14.4).
    pub rejected: SharedCounter,
    /// Messages forwarded so far, stored after every batch; only `e2e run --watch` reads
    /// it.
    pub forwarded: SharedCounter,
}

/// Everything a gateway thread needs besides its gateway and its two rings.
#[derive(Debug)]
pub struct ThreadConfig {
    /// The run clock: stamps `t_gw_in` and `t_gw_out`, and the time the expiry (or the
    /// EIP-712 timestamp window) is checked against.
    pub clock: RunClock,
    pub idle: IdleStrategy,
    /// The CPU to pin to (2.6); `None`: not pinned.
    pub cpu: Option<usize>,
    /// The run's phases, shared with the gate (`Pipeline::shared_phases`), for the
    /// window's reject counts.
    pub phases: Arc<Phases>,
    pub counters: Arc<GatewayCounters>,
}

/// Starts gateway `gateway.index()`'s thread (PIPELINE.md 19.2): it pins itself, publishes
/// its thread id, and runs [`run_gateway`] until `ingress` is closed and drained, then
/// closes `lane` (2.8). Installs the abort-on-panic hook (module docs).
pub fn spawn(
    gateway: Gateway,
    ingress: Consumer<3>,
    lane: Producer<3>,
    config: ThreadConfig,
) -> JoinHandle<GatewayStats> {
    install_abort_on_panic();
    let name = format!("gateway-{}", gateway.index());
    let thread_name = name.clone();
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            if let Some(cpu) = config.cpu {
                pin_current_thread(cpu).unwrap_or_else(|e| panic!("pinning {thread_name} to CPU {cpu}: {e}"));
            }
            let _ = config.counters.thread.publish_tid(); // if it fails, only the fault count is lost
            crate::verifier::prepare_this_thread(); // libsecp256k1's per-thread context, now (verifier.rs)
            gateway.prepare_this_thread(); // the EIP-712 scheme's recovery, once, now (check.rs)
            run_gateway(gateway, ingress, lane, &config)
        })
        .unwrap_or_else(|e| panic!("spawning a gateway thread: {e}"))
}

/// The loop of 7.3 on the calling thread (module docs). Returning drops `lane`, which
/// closes it.
pub fn run_gateway(
    mut gateway: Gateway,
    mut ingress: Consumer<3>,
    mut lane: Producer<3>,
    config: &ThreadConfig,
) -> GatewayStats {
    assert_eq!(
        config.clock.start_unix_ns(),
        gateway.start_unix_ns(),
        "the gateway checks expiries against the run clock's anchor"
    );
    let mut stats = GatewayStats::default();
    let mut busy = BusyMeter::new();
    loop {
        let n = ingress.available(1).min(GATEWAY_BATCH);
        if n == 0 {
            if ingress.is_finished() {
                break;
            }
            config.idle.idle();
            continue;
        }
        busy.begin(config.clock.now()); // busy time (2.2): one extra clock read per batch
        for _ in 0..n {
            let mut words = [0; IngressSlot::WORDS];
            ingress.read(&mut words);
            handle(&mut gateway, &IngressSlot::from_words(&words), &mut lane, config, &mut stats);
        }
        ingress.release();
        config.counters.forwarded.store(stats.forwarded);
        busy.end(config.clock.now(), &config.counters.thread.busy_ns);
    }
    stats
}

/// One message: check it, then forward it or count its reject.
fn handle(
    gateway: &mut Gateway,
    slot: &IngressSlot,
    lane: &mut Producer<3>,
    config: &ThreadConfig,
    stats: &mut GatewayStats,
) {
    let clock = &config.clock;
    let t_gw_in = clock.now();
    match gateway.check(&slot.message, clock.unix_ns(t_gw_in), lane.free(LANE_WANTED)) {
        Ok(accepted) => {
            let t_gw_out = clock.now();
            let lane_index = u16::try_from(gateway.index()).expect("at most 65,536 lanes");
            let record = ClientRecord {
                meta: Meta { source: Source::SignedClient, lane: lane_index, account: accepted.account },
                nonce: accepted.nonce,
                command: accepted.command,
                expires_at: accepted.expires_at,
                signature: accepted.signature,
                t_sched: slot.t_sched,
                t_sent: slot.t_sent,
                t_gw_in,
                t_gw_out,
            };
            // Check 10 saw room, and only this thread writes the lane, so the room is still
            // there.
            lane.write(&record.to_words());
            lane.publish();
            stats.forwarded += 1;
        }
        Err(reason) => {
            let tag = wire::command_tag(&slot.message);
            stats.rejects.count(reason, tag);
            if config.phases.phase_of(slot.t_sched) == Phase::Window {
                stats.window_rejects.count(reason, tag);
            }
            config.counters.rejected.add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    //! The loop on rings, on the test's threads. `spawn` installs the abort-on-panic hook,
    //! so it is tested in `gateway/tests/signed_pipeline.rs`, not in this binary.
    use super::*;
    use crate::salts::SaltTable;
    use crate::test_support::{cancel, message, message_eip712, place, registry, signing_key};
    use crate::wire::MESSAGE_BYTES;
    use engine::types::AccountId;
    use pipeline::codec::encode_command;
    use pipeline::records::signature_words;
    use pipeline::replay::NonceTable;
    use pipeline::ring::channel;

    const DEPLOYMENT: u32 = 3;
    const IDLE: IdleStrategy = IdleStrategy::SpinThenYield { spins: 16 };
    /// The run clock's anchor, in nanoseconds since the UNIX epoch.
    const ANCHOR: u64 = 1_790_000_000_000_000_000;

    /// Gateway 1 of 2 (accounts 1, 3, 5, 7 have keys), on a clock anchored at `ANCHOR`,
    /// with every command measured in `window`.
    fn setup(window: std::ops::Range<u64>) -> (Gateway, ThreadConfig) {
        let gateway = Gateway::new(
            1,
            2,
            DEPLOYMENT,
            &registry(1, DEPLOYMENT, [1, 3, 5, 7]),
            &NonceTable::new(),
            ANCHOR,
        );
        (gateway, config(window))
    }

    /// The thread's configuration: a clock anchored at `ANCHOR`, and every command
    /// measured in `window`.
    fn config(window: std::ops::Range<u64>) -> ThreadConfig {
        ThreadConfig {
            clock: RunClock::anchored_at(ANCHOR),
            idle: IDLE,
            cpu: None,
            phases: Arc::new(Phases::new(0, window)),
            counters: Arc::new(GatewayCounters::default()),
        }
    }

    fn signed(account: AccountId, nonce: u64, command: &engine::command::Command) -> [u8; MESSAGE_BYTES] {
        message(&signing_key(1, account), DEPLOYMENT, account, nonce, u64::MAX, command)
    }

    /// Writes `messages` into a new ingress ring as the sender would, with `t_sched` 1,000
    /// apart, and closes it.
    fn ingress_with(messages: &[[u8; MESSAGE_BYTES]]) -> Consumer<3> {
        let (mut sender, ingress) = channel::<3>(256);
        for (i, message) in messages.iter().enumerate() {
            let t_sched = 1_000 * (i as u64 + 1);
            sender.write(&IngressSlot { message: *message, t_sched, t_sent: t_sched + 5 }.to_words());
        }
        drop(sender); // publishes, then closes
        ingress
    }

    /// Every record in the lane, until it is closed.
    fn drain(lane: &mut Consumer<3>) -> Vec<ClientRecord> {
        let mut records = Vec::new();
        loop {
            if lane.available(1) == 0 {
                if lane.is_finished() {
                    return records;
                }
                IDLE.idle();
                continue;
            }
            let mut words = [0; ClientRecord::WORDS];
            lane.read(&mut words);
            lane.release();
            records.push(ClientRecord::from_words(&words));
        }
    }

    #[test]
    fn accepted_messages_become_lane_records_and_rejects_are_counted() {
        let (gateway, config) = setup(0..u64::MAX);
        let expired = message(&signing_key(1, 3), DEPLOYMENT, 3, 1, ANCHOR - 1, &place(3, 1));
        let messages = [
            signed(1, 1, &place(1, 1)),
            signed(1, 1, &place(1, 2)), // StaleNonce: a replayed nonce
            signed(2, 1, &place(2, 1)), // WrongGateway: account 2 is gateway 0's
            expired,                    // Expired: the clock is past it from the start
            signed(3, 7, &cancel(3, 9)),
            signed(1, 2, &cancel(1, 1)),
        ];
        let (lane_in, mut lane) = channel::<3>(LANE_CAPACITY);
        let stats = run_gateway(gateway, ingress_with(&messages), lane_in, &config);

        let records = drain(&mut lane);
        assert_eq!(records.len(), 3);
        let expected = [(1, 1, place(1, 1), 0), (3, 7, cancel(3, 9), 4), (1, 2, cancel(1, 1), 5)];
        for (record, (account, nonce, command, i)) in records.iter().zip(expected) {
            assert_eq!(record.meta, Meta { source: Source::SignedClient, lane: 1, account });
            assert_eq!(record.nonce, nonce);
            assert_eq!(record.command, encode_command(&command));
            assert_eq!(record.expires_at, u64::MAX);
            assert_eq!(record.signature, signature_words(&messages[i]));
            let t_sched = 1_000 * (i as u64 + 1);
            assert_eq!((record.t_sched, record.t_sent), (t_sched, t_sched + 5));
            assert!(0 < record.t_gw_in && record.t_gw_in <= record.t_gw_out, "{record:?}");
        }
        assert!(records.windows(2).all(|pair| pair[0].t_gw_out <= pair[1].t_gw_in), "in ring order");

        assert_eq!(stats.forwarded, 3);
        for reason in [GatewayReject::StaleNonce, GatewayReject::WrongGateway, GatewayReject::Expired] {
            assert_eq!(stats.rejects.get(reason), 1, "{reason}");
        }
        assert_eq!(stats.rejects.total(), 3);
        assert_eq!(config.counters.rejected.load(), 3, "the barrier's counter");
        assert!(config.counters.thread.busy_ns.load() > 0, "busy time is published");
    }

    #[test]
    fn only_rejects_scheduled_in_the_window_count_as_the_windows() {
        // Messages are scheduled at 1,000, 2,000, 3,000 and 4,000; the window is
        // [2,000, 4,000).
        let (gateway, config) = setup(2_000..4_000);
        let stale = signed(1, 1, &place(1, 1));
        let messages = [stale, stale, stale, stale]; // the first is accepted, the rest stale
        let (lane_in, mut lane) = channel::<3>(LANE_CAPACITY);
        let stats = run_gateway(gateway, ingress_with(&messages), lane_in, &config);
        assert_eq!(drain(&mut lane).len(), 1);
        assert_eq!(stats.rejects.get(GatewayReject::StaleNonce), 3);
        assert_eq!(stats.window_rejects.get(GatewayReject::StaleNonce), 2, "t_sched 2,000 and 3,000");
        assert_eq!(stats.window_rejects.total(), 2);
    }

    #[test]
    fn a_full_lane_rejects_busy_per_tag_and_keeps_its_last_64_slots_for_cancels() {
        // Nobody reads the 128-slot lane while the gateway runs, so its free slots only
        // shrink: places are taken while more than 64 are free, cancels until none is.
        let (gateway, config) = setup(0..u64::MAX);
        let places = (1..=70).map(|n| signed(5, n, &place(5, n as u32)));
        let cancels = (71..=140).map(|n| signed(5, n, &cancel(5, n as u32 - 70)));
        let messages: Vec<_> = places.chain(cancels).collect();
        let (lane_in, mut lane) = channel::<3>(128);
        let stats = run_gateway(gateway, ingress_with(&messages), lane_in, &config);

        assert_eq!(stats.forwarded, 128, "the lane is full");
        assert_eq!(stats.rejects.get(GatewayReject::Busy), 12);
        assert_eq!((stats.rejects.busy(1), stats.rejects.busy(2), stats.rejects.busy(3)), (6, 6, 0));
        let records = drain(&mut lane);
        let tags: Vec<u8> = records.iter().map(|record| record.command[0] as u8).collect();
        assert_eq!(tags[..64], [1; 64], "64 places: the 65th would leave 64 or fewer free");
        assert_eq!(tags[64..], [2; 64], "then cancels, into the headroom");
        // The six places refused used no nonce, so cancels 71.. were fresh.
        assert_eq!(records[64].nonce, 71);
    }

    #[test]
    fn the_gateway_streams_between_a_sender_and_a_reader_and_closes_its_lane() {
        // A small ingress ring, so it fills and drains many times while three threads run;
        // every message is valid, so all must arrive, in order. The lane has room for all 300
        // plus the 64 free slots a place needs (check 10), so a reader the OS pauses can't
        // turn a place into `Busy`: with 128 it could, and did with libsecp256k1 (5.7).
        let (gateway, config) = setup(0..u64::MAX);
        let messages: Vec<_> = (1..=300).map(|n| signed(7, n, &place(7, n as u32))).collect();
        let (mut sender, ingress) = channel::<3>(8);
        let (lane_in, mut lane) = channel::<3>(512);
        let records = std::thread::scope(|scope| {
            scope.spawn(|| {
                for (i, message) in messages.iter().enumerate() {
                    while sender.free(1) == 0 {
                        IDLE.idle();
                    }
                    let t_sched = i as u64 + 1;
                    sender.write(&IngressSlot { message: *message, t_sched, t_sent: t_sched }.to_words());
                    sender.publish();
                }
                drop(sender);
            });
            let gateway_thread = scope.spawn(|| run_gateway(gateway, ingress, lane_in, &config));
            let records = drain(&mut lane);
            let stats = gateway_thread.join().expect("the gateway ends");
            assert_eq!(stats.forwarded, 300, "{stats:?}");
            records
        });
        let nonces: Vec<u64> = records.iter().map(|record| record.nonce).collect();
        assert_eq!(nonces, (1..=300).collect::<Vec<_>>());
    }

    #[test]
    fn eip712_messages_become_lane_records_with_their_salt_and_timestamp() {
        // The EIP-712 scheme (D-033) through the same loop: the salt and the timestamp travel
        // in the record's nonce and expiry words, and the rejects are counted by reason.
        let keys = registry(1, DEPLOYMENT, [1, 3]);
        let gateway = Gateway::new_eip712(1, 2, DEPLOYMENT, &keys, SaltTable::new(16, 1), ANCHOR);
        let config = config(0..u64::MAX);
        let now_ms = ANCHOR / 1_000_000; // the clock starts there, and a test takes far less than 5 minutes
        let signed = |account, key_of, salt, ts_ms, command: &engine::command::Command| {
            message_eip712(&signing_key(1, key_of), DEPLOYMENT, account, salt, ts_ms, command)
        };
        let messages = [
            signed(1, 1, 7, now_ms, &place(1, 1)),
            signed(1, 1, 7, now_ms, &place(1, 1)), // ReusedRequest
            signed(1, 3, 8, now_ms, &place(1, 2)), // WrongSigner: account 3's key
            signed(3, 3, 9, now_ms - 300_001, &cancel(3, 1)), // StaleTimestamp
            signed(3, 3, 9, now_ms + 1, &cancel(3, 1)),
        ];
        let (lane_in, mut lane) = channel::<3>(LANE_CAPACITY);
        let stats = run_gateway(gateway, ingress_with(&messages), lane_in, &config);

        let records = drain(&mut lane);
        let expected = [(1, 7, now_ms, place(1, 1), 0), (3, 9, now_ms + 1, cancel(3, 1), 4)];
        assert_eq!(records.len(), expected.len());
        for (record, (account, salt, ts_ms, command, i)) in records.iter().zip(expected) {
            assert_eq!(record.meta, Meta { source: Source::SignedClient, lane: 1, account });
            assert_eq!((record.nonce, record.expires_at), (salt, ts_ms));
            assert_eq!(record.command, encode_command(&command));
            assert_eq!(record.signature, signature_words(&messages[i]));
        }
        assert_eq!(stats.forwarded, 2);
        for reason in
            [GatewayReject::ReusedRequest, GatewayReject::WrongSigner, GatewayReject::StaleTimestamp]
        {
            assert_eq!(stats.rejects.get(reason), 1, "{reason}");
        }
        assert_eq!(stats.rejects.total(), 3);
    }

    #[test]
    fn reject_counts_add_up_across_gateways() {
        let mut a = RejectCounts::default();
        a.count(GatewayReject::Busy, 2);
        a.count(GatewayReject::HighS, 1);
        let mut b = RejectCounts::default();
        b.count(GatewayReject::Busy, 2);
        b.count(GatewayReject::Busy, 3);
        a.add(&b);
        assert_eq!((a.get(GatewayReject::Busy), a.busy(2), a.busy(3), a.total()), (3, 2, 1, 4));
        // Every reason has its own count, the EIP-712 scheme's too.
        let mut each = RejectCounts::default();
        for reason in GatewayReject::ALL {
            each.count(reason, 1);
        }
        assert!(GatewayReject::ALL.iter().all(|&reason| each.get(reason) == 1));
        assert_eq!(each.total(), GatewayReject::COUNT as u64);
    }
}
