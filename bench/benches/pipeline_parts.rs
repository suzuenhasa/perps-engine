//! The pipeline's parts, one at a time (`docs/PIPELINE.md` 15.10, 11.4 and section 17):
//! what each costs per call on one thread, so the end-to-end numbers can be read against
//! them.
//!
//! - `k256`: verify and sign one message of the benchmark's kind (the 72 signed bytes of
//!   5.1), and SHA-256 of those 72 bytes. Verification is the gateway's cost per message
//!   (`t_v`, section 17), signing the load generator's (`t_sign`, 14.8). For the EIP-712
//!   scheme (5.8): `recover`, the gateway's signer check of the same place signed that way
//!   (`wire::check_signer`: low-S, the digest, the recovery and the address; its cost per
//!   message there), and `sign_recoverable`, the load generator's signature of a digest.
//! - `libsecp256k1`, only in a build with `--features c-secp256k1` (5.7): verify the same
//!   message with Bitcoin Core's C library, through the same gateway check as `k256/verify`
//!   (low-S, then SHA-256 and the library's verification); `recover`, the EIP-712 signer
//!   check as `k256/recover`, with the C library's recovery; and `per_call_context`, the
//!   part of each of those verifications that is the `secp256k1` crate's own per-call
//!   context (`gateway/src/verifier.rs`, "No context object").
//! - `eip712`: the parts of the signer check before the recovery: the place's compact form
//!   in MessagePack (`encode_op`), and its whole digest (`digest`: that, and three
//!   keccak-256 hashes).
//! - `keccak`: keccak-256 of 64 bytes (one block: the address of a recovered key) and of
//!   136 bytes (two blocks: a full block, then the padding's).
//! - `salts`: the EIP-712 replay table (`gateway::salts`): a new request found and inserted
//!   (every accepted message), and a request found again (a replay refused), in a table
//!   half full.
//! - `codec`: encode and decode a CMD40 and an EVT56 (section 4): every command is decoded
//!   twice (gateway, core), every event encoded once (core) and decoded once (gate).
//! - `crc32c`: slicing by 8 against byte at a time (11.4), on an 80-byte and a 152-byte
//!   record: the journal writer's cost per record.
//! - `ring`: one 3-line record written, published, read and released on one thread (the
//!   work of a hop, without the other core), and a round trip between two threads through
//!   two 1-line rings (a hop's latency is about half of it; the threads spin).
//! - `histogram`: one `LatencyHistogram::record` (15.3), the gate's cost per stage.
//!
//! Run: `./dev cargo bench -p bench --locked --offline --bench pipeline_parts`, with
//! `--features c-secp256k1` for the `libsecp256k1` group.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use engine::command::{Command, PlaceOrder};
use engine::event::{Event, Fill};
use engine::types::{AccountId, MarketId, Micros, OrderSeq, Price, Qty, Side, TimeInForce, order_id};
use gateway::eip712::{self, Address, Domain, MAX_OP_BYTES};
use gateway::keccak::keccak256;
use gateway::salts::{Request, SaltTable};
use gateway::verifier::sign_recoverable;
use gateway::wire::{
    DecodedEip712, SIGNATURE_BYTES, SIGNED_BYTES, check_signer, decode_eip712, encode_signed_part,
    sign_eip712, signature, verify_signature,
};
use gateway::{GatewayReject, PublicKey, VerifierKind};
use k256::ecdsa::signature::Signer;
use k256::ecdsa::{Signature, SigningKey};
use k256::sha2::{Digest, Sha256};
use pipeline::codec::{decode_command, decode_event, encode_command, encode_event};
use pipeline::crc32c::{crc32c, crc32c_bytewise};
use pipeline::histogram::LatencyHistogram;
use pipeline::records::ClientRecord;
use pipeline::ring::channel;

/// The worked example's account (5.6).
const ACCOUNT_9: AccountId = AccountId::new(9);

/// The worked example's place (5.6).
fn place() -> Command {
    Command::PlaceOrder(PlaceOrder {
        order_id: order_id(ACCOUNT_9, OrderSeq::new(1)),
        price: Price::new(102_998),
        qty: Qty::new(500_000),
        market: MarketId::new(3),
        side: Side::Buy,
        tif: TimeInForce::Gtc,
        post_only: true,
    })
}

/// The worked example's message, signed: the key, the 72 signed bytes, and `r || s`.
fn signed_place() -> (SigningKey, [u8; SIGNED_BYTES], [u8; SIGNATURE_BYTES]) {
    let key = loadgen::keys::signing_key(1, ACCOUNT_9);
    let signed = encode_signed_part(1, ACCOUNT_9, 1, u64::MAX, &place());
    let signature: Signature = key.sign(&signed);
    (key, signed, signature.to_bytes().into())
}

/// The worked example's place signed in the EIP-712 scheme (5.8), for deployment 1 with
/// salt 1, as the gateway holds it after its cheap checks: the domain, the decoded message,
/// `r || s`, and account 9's address.
fn eip712_place() -> (Domain, DecodedEip712, [u8; SIGNATURE_BYTES], Address) {
    let key = loadgen::keys::signing_key(1, ACCOUNT_9);
    let domain = Domain::new(1);
    let message = sign_eip712(&key, &domain, ACCOUNT_9, 1, 1_790_000_000_000, &place());
    let decoded = decode_eip712(&message).expect("the gateway decodes it");
    (domain, decoded, *signature(&message), PublicKey::K256(*key.verifying_key()).address())
}

fn k256(c: &mut Criterion) {
    let (key, signed, signature) = signed_place();
    let public = PublicKey::K256(*key.verifying_key());
    let (domain, decoded, eip712_signature, address) = eip712_place();
    let digest = decoded.digest(&domain);
    let mut group = c.benchmark_group("k256");
    group.bench_function("verify", |b| {
        b.iter(|| verify_signature(&public, black_box(&signed), black_box(&signature)).expect("verifies"))
    });
    group.bench_function("sign", |b| {
        b.iter(|| {
            let signature: Signature = key.sign(black_box(&signed));
            signature
        })
    });
    group.bench_function("sha256_72_bytes", |b| b.iter(|| Sha256::digest(black_box(&signed))));
    group.bench_function("recover", |b| {
        b.iter(|| {
            check_signer(
                VerifierKind::K256,
                &domain,
                black_box(&decoded),
                black_box(&eip712_signature),
                &address,
            )
            .expect("the signer")
        })
    });
    group.bench_function("sign_recoverable", |b| b.iter(|| sign_recoverable(&key, black_box(&digest))));
    group.finish();
}

#[cfg(feature = "c-secp256k1")]
fn libsecp256k1(c: &mut Criterion) {
    use gateway::VerifierKind;
    use gateway::verifier::secp256k1;

    let (key, signed, signature) = signed_place();
    let compressed = PublicKey::K256(*key.verifying_key()).to_compressed();
    let public = PublicKey::from_compressed(VerifierKind::LibSecp256k1, &compressed).expect("a point");
    let (domain, decoded, eip712_signature, address) = eip712_place();
    let mut group = c.benchmark_group("libsecp256k1");
    group.bench_function("verify", |b| {
        b.iter(|| verify_signature(&public, black_box(&signed), black_box(&signature)).expect("verifies"))
    });
    group.bench_function("recover", |b| {
        b.iter(|| {
            let verifier = VerifierKind::LibSecp256k1;
            check_signer(verifier, &domain, black_box(&decoded), black_box(&eip712_signature), &address)
                .expect("the signer")
        })
    });
    // What `secp256k1::ecdsa::verify` does around the C call, and nothing else: borrow the
    // crate's global context and hand it to `black_box`, which does nothing with it.
    group.bench_function("per_call_context", |b| {
        b.iter(|| secp256k1::with_raw_global_context(black_box, None))
    });
    group.finish();
}

/// A build without the feature has no libsecp256k1 to measure.
#[cfg(not(feature = "c-secp256k1"))]
fn libsecp256k1(_: &mut Criterion) {}

fn eip712_parts(c: &mut Criterion) {
    let (domain, decoded, _, _) = eip712_place();
    let mut buffer = [0; MAX_OP_BYTES];
    let mut group = c.benchmark_group("eip712");
    group.bench_function("encode_op", |b| {
        b.iter(|| eip712::encode_op(black_box(&decoded.command), &mut buffer).map(<[u8]>::len))
    });
    group.bench_function("digest", |b| b.iter(|| black_box(&decoded).digest(&domain)));
    group.finish();
}

fn keccak(c: &mut Criterion) {
    let bytes = [0x5A_u8; 136];
    let mut group = c.benchmark_group("keccak");
    group.bench_function("64_bytes", |b| b.iter(|| keccak256(black_box(&bytes[..64]))));
    group.bench_function("136_bytes", |b| b.iter(|| keccak256(black_box(&bytes))));
    group.finish();
}

/// Requests in the replay table of the `salts` group: 2^17 slots, half of them used once
/// it is full.
const TABLE_REQUESTS: usize = 1 << 16;
/// The clock of every lookup: 2026-09-21, in milliseconds.
const NOW_MS: u64 = 1_790_000_000_000;

/// Request `i`: 1,000 accounts, each with its own run of salts, all at `NOW_MS`, in market 1.
fn request(i: u64) -> Request {
    Request { account: AccountId::new((i % 1_000) as u32), salt: i, ts_ms: NOW_MS, market: MarketId::new(1) }
}

fn salts(c: &mut Criterion) {
    let mut group = c.benchmark_group("salts");
    // A fresh table for each run of `TABLE_REQUESTS` inserts, made outside the timing.
    group.bench_function("find_and_insert", |b| {
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            let mut done = 0;
            while done < iterations {
                let n = (iterations - done).min(TABLE_REQUESTS as u64);
                let mut table = SaltTable::new(TABLE_REQUESTS, 1);
                let started = Instant::now();
                for i in 0..n {
                    let vacancy = table.find(request(black_box(i)), NOW_MS).expect("room");
                    table.insert(vacancy);
                }
                elapsed += started.elapsed();
                done += n;
            }
            elapsed
        })
    });
    // A full table, and requests it holds: each lookup is a replay.
    let mut table = SaltTable::new(TABLE_REQUESTS, 1);
    for i in 0..TABLE_REQUESTS as u64 {
        let vacancy = table.find(request(i), NOW_MS).expect("room");
        table.insert(vacancy);
    }
    let mut i = 0;
    group.bench_function("find_a_replay", |b| {
        b.iter(|| {
            i = (i + 1) % TABLE_REQUESTS as u64;
            let found = table.find(request(black_box(i)), NOW_MS);
            assert_eq!(found, Err(GatewayReject::ReusedRequest));
        })
    });
    group.finish();
}

fn codec(c: &mut Criterion) {
    let command = place();
    let command_words = encode_command(&command);
    let event = Event::Fill(Fill {
        maker_order: order_id(AccountId::new(7), OrderSeq::new(3)),
        taker_order: order_id(ACCOUNT_9, OrderSeq::new(1)),
        price: Price::new(102_998),
        qty: Qty::new(1_000),
        maker_fee: Micros::new(10),
        taker_fee: Micros::new(41),
        market: MarketId::new(3),
        taker_side: Side::Buy,
    });
    let event_words = encode_event(&event);
    let mut group = c.benchmark_group("codec");
    group.bench_function("encode_command", |b| b.iter(|| encode_command(black_box(&command))));
    group.bench_function("decode_command", |b| b.iter(|| decode_command(black_box(&command_words))));
    group.bench_function("encode_event", |b| b.iter(|| encode_event(black_box(&event))));
    group.bench_function("decode_event", |b| b.iter(|| decode_event(black_box(&event_words))));
    group.finish();
}

fn crc(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32c");
    for bytes in [80, 152] {
        let record: Vec<u8> = (0..bytes).map(|i| (i * 7 + 3) as u8).collect();
        assert_eq!(crc32c(&record), crc32c_bytewise(&record), "the two agree");
        group
            .bench_function(format!("slicing_by_8/{bytes}_bytes"), |b| b.iter(|| crc32c(black_box(&record))));
        group.bench_function(format!("bytewise/{bytes}_bytes"), |b| {
            b.iter(|| crc32c_bytewise(black_box(&record)))
        });
    }
    group.finish();
}

fn ring(c: &mut Criterion) {
    let mut group = c.benchmark_group("ring");
    // One hop's work on one thread: a lane record (20 words, 3 lines) in and out.
    let (mut producer, mut consumer) = channel::<3>(1_024);
    let record = [7u64; ClientRecord::WORDS];
    let mut out = [0u64; ClientRecord::WORDS];
    group.bench_function("write_publish_read_release_3_lines", |b| {
        b.iter(|| {
            producer.write(black_box(&record));
            producer.publish();
            consumer.available(1);
            consumer.read(&mut out);
            consumer.release();
            black_box(out[0])
        })
    });
    // A round trip between two spinning threads through two 1-line rings.
    group.bench_function("round_trip_two_threads", |b| {
        b.iter_custom(|iterations| {
            let (mut ping, mut ping_end) = channel::<1>(64);
            let (mut pong, mut pong_end) = channel::<1>(64);
            let stop = Arc::new(AtomicBool::new(false));
            let echo_stop = Arc::clone(&stop);
            let echo = std::thread::spawn(move || {
                let mut slot = [0u64; 8];
                while !echo_stop.load(Ordering::Relaxed) {
                    if ping_end.available(1) > 0 {
                        ping_end.read(&mut slot);
                        ping_end.release();
                        pong.write(&slot);
                        pong.publish();
                    }
                }
            });
            let mut slot = [0u64; 8];
            let started = Instant::now();
            for i in 0..iterations {
                slot[0] = i;
                ping.write(&slot);
                ping.publish();
                while pong_end.available(1) == 0 {
                    std::hint::spin_loop();
                }
                pong_end.read(&mut slot);
                pong_end.release();
            }
            let elapsed = started.elapsed();
            stop.store(true, Ordering::Relaxed);
            echo.join().expect("the echo thread ends");
            elapsed
        })
    });
    group.finish();
}

fn histogram(c: &mut Criterion) {
    let mut histogram = LatencyHistogram::new();
    let mut value = 1u64;
    c.bench_function("histogram/record", |b| {
        b.iter(|| {
            // A spread of values, so the bucket index isn't always the same.
            value = value.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            histogram.record(black_box(value >> 44));
        })
    });
}

criterion_group! {
    name = pipeline_parts;
    config = Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(2));
    targets = k256, libsecp256k1, eip712_parts, keccak, salts, codec, crc, ring, histogram
}
criterion_main!(pipeline_parts);
