//! Gateway threads in front of the real pipeline, end to end, over a crash-free restart
//! (`docs/PIPELINE.md` 7.3, 2.8, 6.3, 13.4 and 13.5).
//!
//! **What is checked.**
//! - **The gateways' loop, between real rings** (7.3): valid messages are verified,
//!   forwarded, sequenced, journaled, applied and released; bad ones are rejected by the
//!   right reason, counted in total and in the measured window, and never reach the
//!   journal. The barrier's counters add up: `released + gateway rejects == sent` (14.4).
//! - **Stopping follows the data** (2.8): closing the ingress rings and the operator ring
//!   stops the gateways, which close their lanes, which stops the pipeline: nothing is lost.
//! - **Nonces come back from the journal** (6.3): after a restart, gateways started with
//!   the replayed nonce table refuse a message accepted before the restart (`StaleNonce`).
//! - **A key replaced at a restart** (5.4): the new life's segment names the new registry;
//!   the old key no longer verifies for that account, the new one does.
//! - **The signature audit** (13.4) passes on the journal of both lives with both registry
//!   files, and fails the records of a life whose registry is missing.
//!
//! The gateways verify with libsecp256k1 in a build with the `c-secp256k1` feature, and
//! with `k256` otherwise (`VERIFIER`), so the same test covers both (5.7).
//!
//! This binary starts a `Pipeline` (and `gateway::spawn`), which installs the
//! abort-on-panic hook (2.8): a failed assertion aborts the binary after printing its
//! message. Everything idles with "spin then yield", so the outcome doesn't depend on
//! scheduling.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use engine::command::{CancelOrder, Command, Deposit, PlaceOrder, SetMark, SetMarketParams, SetRiskTier};
use engine::engine::EngineOptions;
use engine::types::{AccountId, MarketId, Micros, OrderSeq, Price, Qty, Side, TimeInForce, order_id};
use k256::ecdsa::signature::Signer;
use k256::ecdsa::{Signature, SigningKey, VerifyingKey};
use k256::sha2::{Digest, Sha256};

use gateway::audit::verify_journal;
use gateway::thread::{INGRESS_CAPACITY, LANE_CAPACITY};
use gateway::wire::{MESSAGE_BYTES, ORDER, assemble, encode_signed_part};
use gateway::{
    Gateway, GatewayCounters, GatewayReject, GatewayStats, KeyRegistry, ThreadConfig, VerifierKind,
    gateway_of,
};
use pipeline::affinity::CpuLayout;
use pipeline::clock::RunClock;
use pipeline::codec::encode_command;
use pipeline::gate::{Phases, Stage};
use pipeline::idle::IdleStrategy;
use pipeline::journal::format::JournalIdentity;
use pipeline::journal::recovery::recover;
use pipeline::journal::writer::JournalConfig;
use pipeline::records::{AuthScheme, IngressSlot, InjectionMode, OperatorRecord, Stamps};
use pipeline::replay::{NonceTable, replay};
use pipeline::ring::{Producer, channel};
use pipeline::run::{Pipeline, PipelineConfig, PipelineOutput, Resume, RingCapacities};
use pipeline::sequencer::Inputs;

const DEPLOYMENT: u32 = 21;
const GATEWAYS: usize = 2;
const ACCOUNTS: [AccountId; 6] = [acct(1), acct(2), acct(3), acct(4), acct(5), acct(6)];
const MARKET: MarketId = MarketId::new(1);
const IDLE: IdleStrategy = IdleStrategy::SpinThenYield { spins: 64 };
/// The gateways' verifier (module docs).
const VERIFIER: VerifierKind =
    if cfg!(feature = "c-secp256k1") { VerifierKind::LibSecp256k1 } else { VerifierKind::K256 };

/// Account number `n`.
const fn acct(n: u32) -> AccountId {
    AccountId::new(n)
}

/// Account `account`'s private key for `seed`, derived as the load generator does (14.8).
fn signing_key(seed: u64, account: AccountId) -> SigningKey {
    (0..=u8::MAX)
        .find_map(|c| {
            let mut hasher = Sha256::new();
            hasher.update(b"perps-loadgen key v1");
            hasher.update(seed.to_le_bytes());
            hasher.update(account.get().to_le_bytes());
            hasher.update([c]);
            SigningKey::from_slice(&hasher.finalize()).ok()
        })
        .expect("a key")
}

/// The registry of every account's key, with `seed_of(account)` choosing each key's seed,
/// parsed for `VERIFIER`.
fn registry(seed_of: impl Fn(AccountId) -> u64) -> KeyRegistry {
    let keys: Vec<(AccountId, VerifyingKey)> =
        ACCOUNTS.iter().map(|&a| (a, *signing_key(seed_of(a), a).verifying_key())).collect();
    let registry = KeyRegistry::from_keys(DEPLOYMENT, &keys, VERIFIER).expect("a valid registry");
    assert_eq!(registry.verifier(), VERIFIER);
    registry
}

/// A message from `account`, signed with the key of `key_seed`.
fn message(
    key_seed: u64,
    account: AccountId,
    nonce: u64,
    expires_at: u64,
    command: &Command,
) -> [u8; MESSAGE_BYTES] {
    let signed = encode_signed_part(DEPLOYMENT, account, nonce, expires_at, command);
    let signature: Signature = signing_key(key_seed, account).sign(&signed);
    assemble(&signed, &signature.to_bytes().into())
}

fn bid(account: AccountId, sequence: u32) -> Command {
    Command::PlaceOrder(PlaceOrder {
        order_id: order_id(account, OrderSeq::new(sequence)),
        price: Price::new(99_000 + i64::from(sequence)),
        qty: Qty::new(10),
        market: MARKET,
        side: Side::Buy,
        tif: TimeInForce::Gtc,
        post_only: false,
    })
}

fn setup() -> Vec<Command> {
    let mut commands = vec![
        Command::SetMarketParams(SetMarketParams {
            min_price: Price::new(1_000),
            max_price: Price::new(1_000_000),
            maker_fee_ppm: 0,
            taker_fee_ppm: 0,
            price_band_ppm: 20_000,
            market: MARKET,
            max_leverage: 10,
        }),
        Command::SetRiskTier(SetRiskTier {
            lower_bound: Micros::ZERO,
            market: MARKET,
            max_leverage: 10,
            index: 0,
            count: 1,
        }),
        Command::SetMark(SetMark { price: Price::new(100_000), market: MARKET }),
    ];
    let amount = Micros::new(1 << 40);
    commands.extend(ACCOUNTS.iter().map(|&account| Command::Deposit(Deposit { amount, account })));
    commands
}

fn engine_options() -> EngineOptions {
    EngineOptions {
        order_capacity: 4_096,
        id_hash_seed: 0x5157,
        scratch_capacity: 4_096,
        account_capacity: 1_024,
        slot_capacity: 1_024,
    }
}

fn identity() -> JournalIdentity {
    JournalIdentity::new(DEPLOYMENT, InjectionMode::Signed, engine_options())
}

fn run_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gateway-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// The pipeline, its gateways, and the test's sender ends of their rings.
struct Harness {
    pipeline: Pipeline,
    ingress: Vec<Producer<3>>,
    operator: Producer<1>,
    gateways: Vec<JoinHandle<GatewayStats>>,
    counters: Vec<Arc<GatewayCounters>>,
    clock: RunClock,
    /// Messages and operator commands sent.
    sent: u64,
}

impl Harness {
    /// Starts the pipeline (fresh, or resumed) and one gateway thread per lane, wired as
    /// the harness of 19.2 wires them.
    fn start(
        journal: &Path,
        registry: &KeyRegistry,
        nonces: &NonceTable,
        clock: RunClock,
        resume: Option<Resume>,
    ) -> Harness {
        let (ingress, ingress_ends): (Vec<_>, Vec<_>) =
            (0..GATEWAYS).map(|_| channel::<3>(INGRESS_CAPACITY)).unzip();
        let (lanes_in, lanes_out): (Vec<_>, Vec<_>) =
            (0..GATEWAYS).map(|_| channel::<3>(LANE_CAPACITY)).unzip();
        let (operator, operator_end) = channel::<1>(4_096);
        let config = PipelineConfig {
            deployment: DEPLOYMENT,
            mode: InjectionMode::Signed,
            auth: AuthScheme::Perp,
            engine: engine_options(),
            lanes: GATEWAYS,
            capacities: RingCapacities { journal: 1_024, core: 256, event: 1_024 },
            journal: JournalConfig { segment_bytes: 256 << 10, ..JournalConfig::new(journal.to_path_buf()) },
            registry_digest: registry.digest(),
            layout: CpuLayout::unpinned(),
            idle: IDLE,
            capture: None,
            release_log: None,
            live: false,
            stamps: Stamps::On,
            phases: Phases::not_yet(),
            ablation_verify_on_core: None,
        };
        let inputs = Inputs { lanes: lanes_out, operator: operator_end };
        let pipeline = Pipeline::start(config, inputs, clock, resume).expect("the pipeline starts");
        let (mut gateways, mut counters) = (Vec::new(), Vec::new());
        for (g, (ingress_end, lane)) in ingress_ends.into_iter().zip(lanes_in).enumerate() {
            let gateway = Gateway::new(g, GATEWAYS, DEPLOYMENT, registry, nonces, clock.start_unix_ns());
            let shared = Arc::new(GatewayCounters::default());
            let config = ThreadConfig {
                clock,
                idle: IDLE,
                cpu: None,
                phases: pipeline.shared_phases(),
                counters: Arc::clone(&shared),
            };
            gateways.push(gateway::spawn(gateway, ingress_end, lane, config));
            counters.push(shared);
        }
        Harness { pipeline, ingress, operator, gateways, counters, clock, sent: 0 }
    }

    fn operator(&mut self, command: &Command) {
        while self.operator.free(1) == 0 {
            IDLE.idle();
        }
        let now = self.clock.now();
        self.operator.write(
            &OperatorRecord { command: encode_command(command), t_sched: now, t_sent: now }.to_words(),
        );
        self.operator.publish();
        self.sent += 1;
    }

    /// Sends a message into gateway `g`'s ingress ring (the test waits for room; the real
    /// sender would drop it instead).
    fn send_to(&mut self, g: usize, message: &[u8; MESSAGE_BYTES]) {
        let ring = &mut self.ingress[g];
        while ring.free(1) == 0 {
            IDLE.idle();
        }
        let now = self.clock.now();
        ring.write(&IngressSlot { message: *message, t_sched: now, t_sent: now }.to_words());
        ring.publish();
        self.sent += 1;
    }

    /// Sends a message to its account's gateway.
    fn send(&mut self, account: AccountId, message: &[u8; MESSAGE_BYTES]) {
        self.send_to(gateway_of(account, GATEWAYS), message);
    }

    /// The barrier of 14.4: waits until everything sent is released or rejected.
    fn barrier(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let rejected: u64 = self.counters.iter().map(|c| c.rejected.load()).sum();
            let resolved = self.pipeline.released() + rejected;
            if resolved == self.sent {
                return;
            }
            assert!(Instant::now() < deadline, "only {resolved} of {} resolved", self.sent);
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    /// Measures every command sent from now on.
    fn open_window(&self) {
        let now = self.clock.now();
        self.pipeline.phases().set(now, now..u64::MAX);
    }

    /// Closes the sender's rings and waits for the cascade of 2.8.
    fn stop(self) -> (PipelineOutput, GatewayStats) {
        drop(self.ingress);
        drop(self.operator);
        let mut total = GatewayStats::default();
        for handle in self.gateways {
            let stats = handle.join().expect("a gateway thread ends");
            total.forwarded += stats.forwarded;
            total.rejects.add(&stats.rejects);
            total.window_rejects.add(&stats.window_rejects);
        }
        (self.pipeline.join(), total)
    }
}

/// `n − s`: the high-S twin of a message's signature (5.3).
fn high_s_twin(message: &[u8; MESSAGE_BYTES]) -> [u8; MESSAGE_BYTES] {
    let mut twin = *message;
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let difference = i16::from(ORDER[i]) - i16::from(message[104 + i]) - borrow;
        twin[104 + i] = difference.rem_euclid(256) as u8;
        borrow = i16::from(difference < 0);
    }
    twin
}

#[test]
fn signed_messages_go_through_the_gateways_survive_a_restart_and_pass_the_audit() {
    let dir = run_dir("restart");
    let journal = dir.join("journal");
    let registry_1 = registry(|_| 1);

    // ---- Life 1.
    let clock = RunClock::start();
    let mut run = Harness::start(&journal, &registry_1, &NonceTable::new(), clock, None);
    for command in setup() {
        run.operator(&command);
    }
    run.barrier();
    run.open_window();
    // Ten bids per account, nonces 1 to 10, and a cancel with nonce 11.
    for n in 1..=10 {
        for account in ACCOUNTS {
            run.send(account, &message(1, account, n, u64::MAX, &bid(account, n as u32)));
        }
    }
    let cancel = |account| {
        Command::CancelOrder(CancelOrder { order_id: order_id(account, OrderSeq::new(1)), market: MARKET })
    };
    for account in ACCOUNTS {
        run.send(account, &message(1, account, 11, u64::MAX, &cancel(account)));
    }
    // Five bad messages, each for its reason.
    let first_of_1 = message(1, acct(1), 1, u64::MAX, &bid(acct(1), 1));
    run.send(acct(1), &first_of_1); // a replay: StaleNonce
    run.send_to(1, &message(1, acct(2), 12, u64::MAX, &bid(acct(2), 12))); // account 2 is gateway 0's: WrongGateway
    run.send(acct(3), &message(1, acct(3), 12, clock.start_unix_ns(), &bid(acct(3), 12))); // Expired
    run.send(acct(4), &message(5, acct(4), 12, u64::MAX, &bid(acct(4), 12))); // signed with account 5's key: BadSignature
    let genuine = message(1, acct(5), 12, u64::MAX, &bid(acct(5), 12));
    run.send(acct(5), &high_s_twin(&genuine)); // HighS
    run.send(acct(5), &genuine); // then the genuine low-S form: accepted
    run.barrier();
    let setup_count = setup().len() as u64;
    let forwarded_1 = 6 * 11 + 1;
    let (output, gateways) = run.stop();

    assert_eq!(gateways.forwarded, forwarded_1);
    for reason in [
        GatewayReject::StaleNonce,
        GatewayReject::WrongGateway,
        GatewayReject::Expired,
        GatewayReject::BadSignature,
        GatewayReject::HighS,
    ] {
        assert_eq!(gateways.rejects.get(reason), 1, "{reason}");
        assert_eq!(gateways.window_rejects.get(reason), 1, "{reason} in the window");
    }
    assert_eq!(gateways.rejects.total(), 5);
    // Nothing lost: every forwarded message and operator command was sequenced, journaled,
    // applied and released.
    let commands_1 = setup_count + forwarded_1;
    assert_eq!(output.sequencer.records, commands_1);
    assert_eq!(output.journal.records, commands_1);
    assert_eq!(output.stats.commands, commands_1);
    // The gate measured each gateway's verification of every client command in the window.
    let verified: u64 = output.stats.per_gateway.iter().map(|g| g.verification.count()).sum();
    assert_eq!(verified, forwarded_1);
    assert!(output.stats.per_gateway.iter().all(|g| g.verification.count() > 0), "both gateways worked");
    assert_eq!(output.stats.client.get(Stage::Verification).count(), forwarded_1);

    // The journal rebuilds every account's last nonce (6.3), and the audit passes (13.4).
    let recovered = recover(&journal, &identity(), false).expect("recovered");
    assert_eq!(recovered.registry_digests, [registry_1.digest()]);
    let replayed = replay(&journal, &recovered, None, false).expect("replayed");
    for account in ACCOUNTS {
        let expected = if account == acct(5) { 12 } else { 11 };
        assert_eq!(replayed.nonces.get(account), expected, "account {account}");
    }
    let audit = verify_journal(&journal, std::slice::from_ref(&registry_1));
    assert!(audit.passed(), "{audit}");
    assert_eq!((audit.signed, audit.operator), (forwarded_1, setup_count));

    // ---- Life 2: a restart (13.5), with account 6's key replaced (5.4).
    let registry_2 = registry(|account| if account == acct(6) { 2 } else { 1 });
    let resume = Resume {
        engine: replayed.engine,
        next_seq: replayed.next_seq,
        last_ts: replayed.last_ts,
        end: recovered.end,
    };
    let clock = RunClock::resume(replayed.last_ts);
    let mut run = Harness::start(&journal, &registry_2, &replayed.nonces, clock, Some(resume));
    run.open_window();
    run.send(acct(1), &first_of_1); // accepted in life 1: StaleNonce, from the journal's nonces
    run.send(acct(6), &message(1, acct(6), 12, u64::MAX, &bid(acct(6), 12))); // the replaced key: BadSignature
    run.send(acct(6), &message(2, acct(6), 12, u64::MAX, &bid(acct(6), 12))); // the new key: accepted
    run.send(acct(1), &message(1, acct(1), 12, u64::MAX, &bid(acct(1), 12))); // accepted
    run.barrier();
    let (output, gateways) = run.stop();
    assert_eq!(gateways.forwarded, 2);
    assert_eq!(gateways.rejects.get(GatewayReject::StaleNonce), 1);
    assert_eq!(gateways.rejects.get(GatewayReject::BadSignature), 1);
    assert_eq!(output.stats.commands, 2);

    // Both lives: the second life's segment names the new registry; the audit needs both.
    let recovered = recover(&journal, &identity(), false).expect("recovered");
    assert_eq!(recovered.registry_digests, [registry_1.digest(), registry_2.digest()]);
    assert_eq!(recovered.records, commands_1 + 2);
    let both = verify_journal(&journal, &[registry_2.clone(), registry_1.clone()]);
    assert!(both.passed(), "{both}");
    assert_eq!(both.signed, forwarded_1 + 2);
    let only_first = verify_journal(&journal, std::slice::from_ref(&registry_1));
    assert_eq!(only_first.failures, 2, "the second life's two records can't be checked: {only_first}");
    let replayed = replay(&journal, &recovered, None, false).expect("replayed");
    assert_eq!((replayed.nonces.get(acct(1)), replayed.nonces.get(acct(6))), (12, 12));
    std::fs::remove_dir_all(&dir).expect("cleaned up");
}
