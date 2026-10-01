//! The EIP-712 scheme's checks make no heap allocation (`docs/DECISIONS.md` D-033;
//! `docs/PIPELINE.md` 15.4).
//!
//! **Contract checked.** A gateway thread is a hot thread: from the start of the timed flow
//! it must not allocate (the smoke test's counting allocator fails a run that does). The
//! EIP-712 scheme adds work to every message: the MessagePack encoding and the keccak-256
//! hashes (on the stack, `eip712.rs`), a recovery (`verifier.rs`), and a lookup and an
//! insert in the salt table, which is allocated when the gateway is made (`salts.rs`).
//! This test runs `Gateway::check` on 1,200 messages, every reject reason of the scheme
//! among them, after the gateway thread's own warm-up (`verifier::prepare_this_thread`,
//! then `Gateway::prepare_this_thread`), and counts the heap allocations and frees made
//! meanwhile on this thread: none of either.
//!
//! **How allocations are counted**, as in `engine/tests/no_alloc.rs`: a test binary may
//! replace the global allocator, and this one forwards to the system allocator and counts
//! calls per thread. The messages are signed before the measured phase (signing allocates
//! nothing either, but it is the client's work, not the gateway's).
//!
//! The gateway recovers with libsecp256k1 in a build with the `c-secp256k1` feature (whose
//! per-thread context the warm-up allocates), and with `k256` otherwise.

// Test-only exception to the workspace's `unsafe_code = "deny"`, as in the engine's
// `no_alloc.rs`: `GlobalAlloc` is an unsafe trait. Ours only forwards to `System` and
// bumps a counter. This file is its own test binary, so none of this reaches the gateway.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use engine::command::{CancelOrder, Command, ModifyOrder, PlaceOrder};
use engine::types::{AccountId, Side, TimeInForce, order_id};
use k256::ecdsa::signature::Signer;
use k256::ecdsa::{Signature, SigningKey, VerifyingKey};

use gateway::eip712::Domain;
use gateway::salts::SaltTable;
use gateway::wire::{
    MESSAGE_BYTES, R_OFFSET, RECOVERY_ID_OFFSET, S_OFFSET, assemble, encode_signed_part, sign_eip712,
};
use gateway::{Gateway, GatewayReject, KeyRegistry, VerifierKind};

thread_local! {
    /// Heap allocations made by this thread so far.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    /// Heap frees made by this thread so far.
    static FREES: Cell<u64> = const { Cell::new(0) };
}

/// The system allocator, plus a count of allocations and frees per thread.
struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `try_with`: a thread that is shutting down may have lost its thread-locals, and
        // an allocator must not panic.
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        // SAFETY: our caller upholds `GlobalAlloc::alloc`'s contract; we pass it on as is.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _ = FREES.try_with(|count| count.set(count.get() + 1));
        // SAFETY: `ptr` came from `System.alloc` (through `alloc` above) with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// How many heap allocations and frees `f` makes on this thread.
fn allocations_and_frees_during(f: impl FnOnce()) -> (u64, u64) {
    let before = (ALLOCATIONS.with(Cell::get), FREES.with(Cell::get));
    f();
    (ALLOCATIONS.with(Cell::get) - before.0, FREES.with(Cell::get) - before.1)
}

const DEPLOYMENT: u32 = 5;
/// Two gateways; this is gateway 1, with the odd accounts.
const GATEWAYS: usize = 2;
const VERIFIER: VerifierKind =
    if cfg!(feature = "c-secp256k1") { VerifierKind::LibSecp256k1 } else { VerifierKind::K256 };
/// The gateway's clock, in nanoseconds since the UNIX epoch, and in milliseconds.
const NOW: u64 = 1_790_000_000_000_000_000;
const NOW_MS: u64 = NOW / 1_000_000;
const ROOM: usize = 1_024;

fn signing_key(account: AccountId) -> SigningKey {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&account.to_be_bytes());
    bytes[31] = 1;
    SigningKey::from_slice(&bytes).expect("a valid scalar")
}

fn command(account: AccountId, n: u32) -> Command {
    let id = order_id(account, n);
    match n % 3 {
        0 => Command::PlaceOrder(PlaceOrder {
            order_id: id,
            price: 99_000 + i64::from(n),
            qty: 10_000,
            market: 1,
            side: if n.is_multiple_of(2) { Side::Buy } else { Side::Sell },
            tif: if n.is_multiple_of(4) { TimeInForce::Ioc } else { TimeInForce::Gtc },
            post_only: n.is_multiple_of(5),
        }),
        1 => Command::CancelOrder(CancelOrder { order_id: id, market: 1 }),
        _ => {
            Command::ModifyOrder(ModifyOrder { order_id: id, new_price: 99_500, new_size: 5_000, market: 1 })
        }
    }
}

#[test]
fn the_eip712_checks_allocate_nothing_on_any_path() {
    let accounts: Vec<AccountId> = (1..=20).collect();
    let keys: Vec<(AccountId, VerifyingKey)> =
        accounts.iter().map(|&a| (a, *signing_key(a).verifying_key())).collect();
    let registry = KeyRegistry::from_keys(DEPLOYMENT, &keys, VERIFIER).expect("a valid registry");
    let domain = Domain::new(u64::from(DEPLOYMENT));
    let mine: Vec<AccountId> = accounts.iter().copied().filter(|a| a % 2 == 1).collect();

    // ---- The messages, before anything is counted: per round, one of each outcome.
    let mut messages = Vec::new();
    let mut lane_free = Vec::new();
    for round in 0..100u32 {
        let account = mine[round as usize % mine.len()];
        let salt = u64::from(round);
        let good =
            sign_eip712(&signing_key(account), &domain, account, salt, NOW_MS, &command(account, round));
        let mut push = |message: [u8; MESSAGE_BYTES], free: usize| {
            messages.push(message);
            lane_free.push(free);
        };
        push(good, ROOM); // accepted
        push(good, ROOM); // ReusedRequest
        let stale = sign_eip712(
            &signing_key(account),
            &domain,
            account,
            salt,
            NOW_MS - 300_001,
            &command(account, round),
        );
        push(stale, ROOM);
        let future = sign_eip712(
            &signing_key(account),
            &domain,
            account,
            salt,
            NOW_MS + 60_001,
            &command(account, round),
        );
        push(future, ROOM);
        let busy = sign_eip712(
            &signing_key(account),
            &domain,
            account,
            salt + 1_000,
            NOW_MS,
            &command(account, 3 * round),
        );
        push(busy, 0); // Busy (a place with no room)
        let other = mine[(round as usize + 1) % mine.len()];
        let wrong = sign_eip712(
            &signing_key(other),
            &domain,
            account,
            salt + 2_000,
            NOW_MS,
            &command(account, round),
        );
        push(wrong, ROOM); // WrongSigner
        let mut high = wrong;
        high[S_OFFSET..].fill(0xFF);
        push(high, ROOM); // HighS
        let mut no_key = wrong;
        no_key[R_OFFSET..S_OFFSET].fill(0);
        push(no_key, ROOM); // BadSignature
        let mut malformed = wrong;
        malformed[RECOVERY_ID_OFFSET] = 27;
        push(malformed, ROOM); // Malformed
        let perp_signed = encode_signed_part(DEPLOYMENT, account, 1, u64::MAX, &command(account, round));
        let perp_signature: Signature = signing_key(account).sign(&perp_signed);
        push(assemble(&perp_signed, &perp_signature.to_bytes().into()), ROOM); // WrongDomain
        let even = account + 1;
        let routed = sign_eip712(&signing_key(even), &domain, even, salt, NOW_MS, &command(even, round));
        push(routed, ROOM); // WrongGateway
        let not_owner = sign_eip712(
            &signing_key(account),
            &domain,
            account,
            salt + 3_000,
            NOW_MS,
            &command(other, round),
        );
        push(not_owner, ROOM); // NotOwner
    }

    // The counter counts: a `Vec` with room allocates once and frees once.
    let counted = allocations_and_frees_during(|| drop(std::hint::black_box(Vec::<u8>::with_capacity(16))));
    assert_eq!(counted, (1, 1));

    // ---- The gateway, made as the harness makes it, and warmed up as its thread is.
    let salts = SaltTable::new(128, 0x5A17);
    let mut gateway = Gateway::new_eip712(1, GATEWAYS, DEPLOYMENT, &registry, salts, 0);
    gateway::verifier::prepare_this_thread();
    gateway.prepare_this_thread();
    let mut outcomes = [0u64; GatewayReject::COUNT + 1]; // the last one: accepted
    let (allocations, frees) = allocations_and_frees_during(|| {
        for (message, &free) in messages.iter().zip(&lane_free) {
            match gateway.check(message, NOW, free) {
                Ok(_) => outcomes[GatewayReject::COUNT] += 1,
                Err(reason) => outcomes[reason.index()] += 1,
            }
        }
    });
    assert_eq!((allocations, frees), (0, 0), "the EIP-712 checks allocate nothing");

    // Every path above ran, 100 times each.
    assert_eq!(outcomes[GatewayReject::COUNT], 100, "accepted");
    for reason in [
        GatewayReject::ReusedRequest,
        GatewayReject::StaleTimestamp,
        GatewayReject::FutureTimestamp,
        GatewayReject::Busy,
        GatewayReject::WrongSigner,
        GatewayReject::HighS,
        GatewayReject::BadSignature,
        GatewayReject::Malformed,
        GatewayReject::WrongDomain,
        GatewayReject::WrongGateway,
        GatewayReject::NotOwner,
    ] {
        assert_eq!(outcomes[reason.index()], 100, "{reason}");
    }

    // A full salt table refuses without allocating, too.
    let mut small = Gateway::new_eip712(1, GATEWAYS, DEPLOYMENT, &registry, SaltTable::new(1, 7), 0);
    small.prepare_this_thread();
    let first = sign_eip712(&signing_key(1), &domain, 1, 1, NOW_MS, &command(1, 1));
    let second = sign_eip712(&signing_key(1), &domain, 1, 2, NOW_MS, &command(1, 2));
    let (allocations, frees) = allocations_and_frees_during(|| {
        assert!(small.check(&first, NOW, ROOM).is_ok());
        assert_eq!(small.check(&second, NOW, ROOM), Err(GatewayReject::SaltTableFull));
    });
    assert_eq!((allocations, frees), (0, 0));
}
