//! Gateway threads of the EIP-712 scheme in front of the real pipeline, end to end
//! (`docs/DECISIONS.md` D-033; `docs/PIPELINE.md` 7.3, 11.2 and 13.4).
//!
//! **What is checked.**
//! - **The gateways' loop, between real rings, in the EIP-712 scheme**: valid messages
//!   (signed over their EIP-712 digest, with a salt and a millisecond timestamp) have their
//!   signer recovered and are forwarded, sequenced, journaled, applied and released; bad
//!   ones are rejected by the right reason and never reach the journal: a replayed request,
//!   a timestamp too old and one too far ahead, another account's key, the high-S twin, and
//!   a message of the perp scheme. The barrier's counters add up (14.4).
//! - **The journal says its scheme** (header byte 120): recovery takes it for a
//!   configuration of the EIP-712 scheme and refuses it for the perp scheme.
//! - **The records hold what the audit needs**: each kind-1 record's nonce word is the salt
//!   and its expiry word the timestamp, and the signature audit (13.4) passes on them, with
//!   the registered keys and no recovery id.
//!
//! The gateways recover with libsecp256k1 in a build with the `c-secp256k1` feature, and
//! with `k256` otherwise (`VERIFIER`), so the same test covers both.
//!
//! This binary starts a `Pipeline` (and `gateway::spawn`), which installs the
//! abort-on-panic hook (2.8): a failed assertion aborts the binary after printing its
//! message.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::command::{CancelOrder, Command, Deposit, PlaceOrder, SetMark, SetMarketParams, SetRiskTier};
use engine::engine::EngineOptions;
use engine::types::{AccountId, MarketId, Micros, OrderSeq, Price, Qty, Side, TimeInForce, order_id};
use k256::ecdsa::signature::Signer;
use k256::ecdsa::{Signature, SigningKey, VerifyingKey};
use k256::sha2::{Digest, Sha256};

use gateway::audit::verify_journal;
use gateway::eip712::Domain;
use gateway::salts::{SaltTable, random_seed};
use gateway::thread::{INGRESS_CAPACITY, LANE_CAPACITY};
use gateway::wire::{MESSAGE_BYTES, ORDER, assemble, encode_signed_part, sign_eip712};
use gateway::{
    Gateway, GatewayCounters, GatewayReject, GatewayStats, KeyRegistry, ThreadConfig, VerifierKind,
    gateway_of,
};
use pipeline::affinity::CpuLayout;
use pipeline::clock::RunClock;
use pipeline::codec::encode_command;
use pipeline::gate::Phases;
use pipeline::idle::IdleStrategy;
use pipeline::journal::JournalError;
use pipeline::journal::files::StdFiles;
use pipeline::journal::format::JournalIdentity;
use pipeline::journal::recovery::{recover, scan_journal};
use pipeline::journal::writer::JournalConfig;
use pipeline::records::{AuthScheme, IngressSlot, InjectionMode, OperatorRecord, Source, Stamps};
use pipeline::ring::channel;
use pipeline::run::{Pipeline, PipelineConfig, RingCapacities};
use pipeline::sequencer::Inputs;

const DEPLOYMENT: u32 = 137;
const GATEWAYS: usize = 2;
const ACCOUNTS: [AccountId; 4] = [acct(1), acct(2), acct(3), acct(4)];
const MARKET: MarketId = MarketId::new(1);
const IDLE: IdleStrategy = IdleStrategy::SpinThenYield { spins: 64 };
/// The gateways' verifier (module docs).
const VERIFIER: VerifierKind =
    if cfg!(feature = "c-secp256k1") { VerifierKind::LibSecp256k1 } else { VerifierKind::K256 };

/// Account number `n`.
const fn acct(n: u32) -> AccountId {
    AccountId::new(n)
}

/// Account `account`'s private key, derived as the load generator does (14.8), seed 1.
fn signing_key(account: AccountId) -> SigningKey {
    (0..=u8::MAX)
        .find_map(|c| {
            let mut hasher = Sha256::new();
            hasher.update(b"perps-loadgen key v1");
            hasher.update(1u64.to_le_bytes());
            hasher.update(account.get().to_le_bytes());
            hasher.update([c]);
            SigningKey::from_slice(&hasher.finalize()).ok()
        })
        .expect("a key")
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
        id_hash_seed: 0x712,
        scratch_capacity: 4_096,
        account_capacity: 1_024,
        slot_capacity: 1_024,
    }
}

fn run_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gateway-eip712-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// The high-S twin of an EIP-712 message: `n − s`, with the same recovery id (5.3).
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
fn eip712_messages_go_through_the_gateways_and_pass_the_audit() {
    let dir = run_dir("run");
    let journal = dir.join("journal");
    let keys: Vec<(AccountId, VerifyingKey)> =
        ACCOUNTS.iter().map(|&a| (a, *signing_key(a).verifying_key())).collect();
    let registry = KeyRegistry::from_keys(DEPLOYMENT, &keys, VERIFIER).expect("a valid registry");
    let domain = Domain::new(u64::from(DEPLOYMENT));

    // ---- The pipeline, and one EIP-712 gateway thread per lane (19.2).
    let clock = RunClock::start();
    let (mut ingress, ingress_ends): (Vec<_>, Vec<_>) =
        (0..GATEWAYS).map(|_| channel::<3>(INGRESS_CAPACITY)).unzip();
    let (lanes_in, lanes_out): (Vec<_>, Vec<_>) = (0..GATEWAYS).map(|_| channel::<3>(LANE_CAPACITY)).unzip();
    let (mut operator, operator_end) = channel::<1>(4_096);
    let config = PipelineConfig {
        deployment: DEPLOYMENT,
        mode: InjectionMode::Signed,
        auth: AuthScheme::Eip712,
        engine: engine_options(),
        lanes: GATEWAYS,
        capacities: RingCapacities { journal: 1_024, core: 256, event: 1_024 },
        journal: JournalConfig { segment_bytes: 256 << 10, ..JournalConfig::new(journal.clone()) },
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
    let identity = config.identity();
    assert_eq!(identity.auth, AuthScheme::Eip712);
    let pipeline = Pipeline::start(config, Inputs { lanes: lanes_out, operator: operator_end }, clock, None)
        .expect("the pipeline starts");
    let mut threads = Vec::new();
    let mut counters = Vec::new();
    for (g, (ingress_end, lane)) in ingress_ends.into_iter().zip(lanes_in).enumerate() {
        // Room for every request this test sends; allocated and touched here, before the
        // thread starts.
        let salts = SaltTable::new(64, random_seed());
        let gateway = Gateway::new_eip712(g, GATEWAYS, DEPLOYMENT, &registry, salts, clock.start_unix_ns());
        assert_eq!(gateway.scheme(), AuthScheme::Eip712);
        let shared = Arc::new(GatewayCounters::default());
        let thread_config = ThreadConfig {
            clock,
            idle: IDLE,
            cpu: None,
            phases: pipeline.shared_phases(),
            counters: Arc::clone(&shared),
        };
        threads.push(gateway::spawn(gateway, ingress_end, lane, thread_config));
        counters.push(shared);
    }
    let mut sent = 0u64;
    for command in setup() {
        let now = clock.now();
        while operator.free(1) == 0 {
            IDLE.idle();
        }
        operator.write(
            &OperatorRecord { command: encode_command(&command), t_sched: now, t_sent: now }.to_words(),
        );
        operator.publish();
        sent += 1;
    }
    let mut send = |account: AccountId, message: &[u8; MESSAGE_BYTES]| {
        let ring = &mut ingress[gateway_of(account, GATEWAYS)];
        while ring.free(1) == 0 {
            IDLE.idle();
        }
        let now = clock.now();
        ring.write(&IngressSlot { message: *message, t_sched: now, t_sent: now }.to_words());
        ring.publish();
        sent += 1;
    };

    // ---- Ten bids per account, salts 1 to 10, then a cancel with salt 11; all stamped
    // with the clock now, in milliseconds, as a client would.
    let now_ms = || clock.unix_ns(clock.now()) / 1_000_000;
    let signed = |account: AccountId, salt: u64, ts_ms: u64, command: &Command| {
        sign_eip712(&signing_key(account), &domain, account, salt, ts_ms, command)
    };
    let mut genuine = Vec::new();
    for n in 1..=10u32 {
        for account in ACCOUNTS {
            let message = signed(account, u64::from(n), now_ms(), &bid(account, n));
            send(account, &message);
            genuine.push((account, u64::from(n)));
        }
    }
    let cancel = |account| {
        Command::CancelOrder(CancelOrder { order_id: order_id(account, OrderSeq::new(1)), market: MARKET })
    };
    for account in ACCOUNTS {
        send(account, &signed(account, 11, now_ms(), &cancel(account)));
    }
    // ---- Six bad messages, each for its reason, then one good one.
    let first_of_1 = signed(acct(1), 12, now_ms(), &bid(acct(1), 12));
    send(acct(1), &first_of_1);
    send(acct(1), &first_of_1); // ReusedRequest
    send(acct(2), &signed(acct(2), 12, now_ms() - 300_001 - 5_000, &bid(acct(2), 12))); // StaleTimestamp
    send(acct(3), &signed(acct(3), 12, now_ms() + 60_001 + 5_000, &bid(acct(3), 12))); // FutureTimestamp
    let by_4 = sign_eip712(&signing_key(acct(4)), &domain, acct(3), 13, now_ms(), &bid(acct(3), 13));
    send(acct(3), &by_4); // WrongSigner: account 4's key, claiming account 3
    let good_4 = signed(acct(4), 12, now_ms(), &bid(acct(4), 12));
    send(acct(4), &high_s_twin(&good_4)); // HighS
    let perp_signed = encode_signed_part(DEPLOYMENT, acct(4), 1, u64::MAX, &bid(acct(4), 13));
    let perp_signature: Signature = signing_key(acct(4)).sign(&perp_signed);
    send(acct(4), &assemble(&perp_signed, &perp_signature.to_bytes().into())); // WrongDomain: version 1
    send(acct(4), &good_4); // the low form: accepted
    let forwarded = ACCOUNTS.len() as u64 * 11 + 2;

    // ---- The barrier of 14.4, then stop in data-flow order (2.8).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let rejected: u64 = counters.iter().map(|c| c.rejected.load()).sum();
        if pipeline.released() + rejected == sent {
            break;
        }
        assert!(Instant::now() < deadline, "only {} of {sent} resolved", pipeline.released() + rejected);
        std::thread::sleep(Duration::from_micros(200));
    }
    drop(ingress);
    drop(operator);
    let mut total = GatewayStats::default();
    for handle in threads {
        let stats = handle.join().expect("a gateway thread ends");
        total.forwarded += stats.forwarded;
        total.rejects.add(&stats.rejects);
    }
    let output = pipeline.join();
    assert_eq!(total.forwarded, forwarded);
    for reason in [
        GatewayReject::ReusedRequest,
        GatewayReject::StaleTimestamp,
        GatewayReject::FutureTimestamp,
        GatewayReject::WrongSigner,
        GatewayReject::HighS,
        GatewayReject::WrongDomain,
    ] {
        assert_eq!(total.rejects.get(reason), 1, "{reason}");
    }
    assert_eq!(total.rejects.total(), 6);
    let commands = setup().len() as u64 + forwarded;
    assert_eq!(output.journal.records, commands);
    assert_eq!(output.stats.commands, commands);

    // ---- The journal: its header says eip712, and a perp configuration is refused.
    let recovered = recover(&journal, &identity, false).expect("recovered");
    assert_eq!(recovered.identity.auth, AuthScheme::Eip712);
    assert_eq!(recovered.records, commands);
    let perp_identity = JournalIdentity { auth: AuthScheme::Perp, ..identity };
    match recover(&journal, &perp_identity, false) {
        Err(JournalError::IdentityMismatch { differences }) => {
            assert_eq!(differences, ["auth: journal eip712, configuration perp"]);
        }
        other => panic!("a perp configuration must be refused: {other:?}"),
    }
    // Each kind-1 record holds the salt in the nonce's word and a timestamp of this run.
    let mut files = StdFiles::open_existing(&journal, 256 << 10).expect("opened");
    let start_ms = clock.start_unix_ns() / 1_000_000;
    let mut salts_seen = Vec::new();
    scan_journal(&mut files, |_, record, _| {
        if record.meta.source == Source::SignedClient {
            assert!(
                record.expires_at >= start_ms && record.expires_at <= record.ts / 1_000_000,
                "{record:?}"
            );
            salts_seen.push((record.meta.account, record.nonce));
        }
    })
    .expect("scanned");
    for request in &genuine {
        assert!(salts_seen.contains(request), "{request:?} was journaled");
    }
    // The audit passes, with the registered keys and no recovery id (13.4).
    let audit = verify_journal(&journal, std::slice::from_ref(&registry));
    assert!(audit.passed(), "{audit}");
    assert_eq!((audit.signed, audit.operator), (forwarded, setup().len() as u64));
    std::fs::remove_dir_all(&dir).expect("cleaned up");
}
