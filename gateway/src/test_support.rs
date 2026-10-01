//! Keys, signed messages and commands for the gateway's unit tests.
//!
//! Keys are derived exactly as the load generator derives them (`docs/PIPELINE.md` 14.8),
//! so the worked example of 5.6 is a known-answer test of the derivation too. `loadgen`
//! has its own copy (it signs the benchmark's messages); both must agree with 5.6.

use engine::command::{CancelOrder, Command, ModifyOrder, PlaceOrder};
use engine::types::{AccountId, MarketId, Side, TimeInForce, order_id};
use k256::ecdsa::signature::Signer;
use k256::ecdsa::{Signature, SigningKey, VerifyingKey};
use k256::sha2::{Digest, Sha256};

use crate::eip712::Domain;
use crate::registry::KeyRegistry;
use crate::verifier::{PublicKey, VerifierKind};
use crate::wire::{MESSAGE_BYTES, ORDER, SIGNATURE_BYTES, assemble, encode_signed_part, sign_eip712};

/// The verifier the gateway's tests run with: libsecp256k1 in a build with the
/// `c-secp256k1` feature, `k256` otherwise. So `cargo test -p gateway` runs every test
/// against `k256`, and `cargo test -p gateway --features c-secp256k1` runs them all again
/// against libsecp256k1 (and the cross-check of the two, `verifier.rs`).
pub const VERIFIER: VerifierKind =
    if cfg!(feature = "c-secp256k1") { VerifierKind::LibSecp256k1 } else { VerifierKind::K256 };

/// Account 9's private key with seed 1 (5.6).
pub const ACCOUNT_9_PRIVATE_KEY: &str = "af3ac022da885faa582f5ec5f871ed9d0868a43a7bf5ddc0268209ca743cea37";
/// Its compressed public key, as `keys.txt` lists it (5.4).
pub const ACCOUNT_9_PUBLIC_KEY: &str = "03872ba80a104a5c56609998c3da5e267e380deffe94d6e3d024969cb2e0efb729";

/// The bytes of hex text: pairs of hex digits, white space ignored.
pub fn bytes(hex: &str) -> Vec<u8> {
    let digits: String = hex.split_whitespace().collect();
    (0..digits.len() / 2)
        .map(|i| u8::from_str_radix(&digits[2 * i..2 * i + 2], 16).expect("a hex byte"))
        .collect()
}

/// Account `account`'s private key for `seed` (14.8): SHA-256 of "perps-loadgen key v1",
/// the seed (`u64`, little-endian), the account (`u32`, little-endian) and a counter byte
/// `c`, read as a big-endian number, with `c` = 0, 1, 2, ... until it is in `1..n`.
pub fn signing_key(seed: u64, account: AccountId) -> SigningKey {
    (0..=u8::MAX)
        .find_map(|c| {
            let mut hasher = Sha256::new();
            hasher.update(b"perps-loadgen key v1");
            hasher.update(seed.to_le_bytes());
            hasher.update(account.to_le_bytes());
            hasher.update([c]);
            SigningKey::from_slice(&hasher.finalize()).ok()
        })
        .expect("a retry is needed with probability about 2^-128")
}

/// The registry of `accounts`' keys for `seed`, parsed for [`VERIFIER`].
pub fn registry(seed: u64, deployment: u32, accounts: impl IntoIterator<Item = AccountId>) -> KeyRegistry {
    let keys: Vec<(AccountId, VerifyingKey)> =
        accounts.into_iter().map(|account| (account, *signing_key(seed, account).verifying_key())).collect();
    KeyRegistry::from_keys(deployment, &keys, VERIFIER).expect("a valid registry")
}

/// `key`'s public key, parsed for [`VERIFIER`].
pub fn public_key(key: &SigningKey) -> PublicKey {
    let compressed = PublicKey::K256(*key.verifying_key()).to_compressed();
    PublicKey::from_compressed(VERIFIER, &compressed).expect("a point on the curve")
}

/// A whole message: `command` from `account` with `nonce`, signed by `key`.
pub fn message(
    key: &SigningKey,
    deployment: u32,
    account: AccountId,
    nonce: u64,
    expires_at: u64,
    command: &Command,
) -> [u8; MESSAGE_BYTES] {
    let signed = encode_signed_part(deployment, account, nonce, expires_at, command);
    let signature: Signature = key.sign(&signed);
    assemble(&signed, &signature.to_bytes().into())
}

/// A whole message of the EIP-712 scheme (version 2, D-033): `command` for `account`, with
/// `salt` and `ts_ms`, signed by `key` in the domain of `deployment`.
pub fn message_eip712(
    key: &SigningKey,
    deployment: u32,
    account: AccountId,
    salt: u64,
    ts_ms: u64,
    command: &Command,
) -> [u8; MESSAGE_BYTES] {
    sign_eip712(key, &Domain::new(u64::from(deployment)), account, salt, ts_ms, command)
}

/// The same `r` with `n − s`: the high-S twin of a signature (5.3).
pub fn high_s_twin(signature: &[u8; SIGNATURE_BYTES]) -> [u8; SIGNATURE_BYTES] {
    let mut twin = *signature;
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let difference = i16::from(ORDER[i]) - i16::from(signature[32 + i]) - borrow;
        twin[32 + i] = difference.rem_euclid(256) as u8;
        borrow = i16::from(difference < 0);
    }
    twin
}

/// A post-only GTC bid of `account`'s order `sequence` on market 3.
pub fn place(account: AccountId, sequence: u32) -> Command {
    Command::PlaceOrder(PlaceOrder {
        order_id: order_id(account, sequence),
        price: 102_998,
        qty: 500_000,
        market: 3,
        side: Side::Buy,
        tif: TimeInForce::Gtc,
        post_only: true,
    })
}

/// A cancel of `account`'s order `sequence` on market 3.
pub fn cancel(account: AccountId, sequence: u32) -> Command {
    Command::CancelOrder(CancelOrder { order_id: order_id(account, sequence), market: 3 })
}

/// A modify of `account`'s order `sequence` on market 3.
pub fn modify(account: AccountId, sequence: u32) -> Command {
    Command::ModifyOrder(ModifyOrder {
        order_id: order_id(account, sequence),
        new_price: 103_001,
        new_size: 250_000,
        market: 3,
    })
}

/// `command`, a place, a cancel or a modify, with its market changed to `market`: what a
/// copy with another market looks like after decoding (in the EIP-712 scheme, a cancel's
/// or a modify's market is not signed, `eip712.rs`).
pub fn with_market(command: &Command, market: MarketId) -> Command {
    match *command {
        Command::PlaceOrder(place) => Command::PlaceOrder(PlaceOrder { market, ..place }),
        Command::CancelOrder(cancel) => Command::CancelOrder(CancelOrder { market, ..cancel }),
        Command::ModifyOrder(modify) => Command::ModifyOrder(ModifyOrder { market, ..modify }),
        _ => panic!("a place, a cancel or a modify, not {command:?}"),
    }
}

/// A tiny deterministic generator (xorshift64), so randomized tests need no dependency.
#[derive(Clone, Debug)]
pub struct XorShift(pub u64);

impl XorShift {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}
