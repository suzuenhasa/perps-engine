//! The signature verifier: RustCrypto's `k256` (pure Rust, in every build), or Bitcoin
//! Core's libsecp256k1 (C, only in a build with the `c-secp256k1` feature)
//! (`docs/PIPELINE.md` 5.7; `docs/DECISIONS.md` D-032).
//!
//! **Why two.** `k256` is the default (D-021). On 2026-09-30 the owner approved Bitcoin
//! Core's libsecp256k1, through rust-bitcoin's `secp256k1` crate, as an opt-in second
//! verifier, to measure the two side by side on the rented box: a cheaper verification
//! means fewer gateway cores for the same signed rate (section 17). Its C source is
//! compiled only when the gateway's `c-secp256k1` feature is on; the default build never
//! compiles it. (`libsecp256k1` here always means Bitcoin Core's C library; the crates.io
//! crate of that name is the abandoned lookalike that `deny.toml` bans.)
//!
//! **Contract.**
//! - [`VerifierKind`] names a verifier. Both names exist in every build, so a configuration
//!   can name either; [`VerifierKind::check_built`] refuses libsecp256k1 in a build without
//!   the feature, with an error that says how to get it. The registry calls it before it
//!   reads a line, so no gateway can start with a verifier the build doesn't have.
//! - [`PublicKey`] is a registered key, parsed for one verifier. The registry parses each
//!   key once, when it is loaded (a square root in the field, 5.4), never per message.
//! - [`PublicKey::verifies`] is the library's half of checks 11 and 12: it is false if `r`
//!   or `s` is 0 or not below `n`, if `s` is high (both libraries refuse the high form on
//!   their own), or if the ECDSA equation doesn't hold over the SHA-256 of the 72 signed
//!   bytes. Everything around it stays the same whichever verifier runs: the message, the
//!   cheap checks before it, and our own low-S check first (`wire::verify_signature`), so
//!   the reject reasons don't depend on the verifier.
//!
//! **The same digest.** `k256` hashes the signed bytes with SHA-256 itself (5.2);
//! libsecp256k1 takes the 32-byte digest. So for libsecp256k1 we compute SHA-256 of bytes
//! 0..72 with `k256`'s `sha2` re-export and pass that: both check the same equation.
//!
//! **No context object.** libsecp256k1's C functions take a "context". In `secp256k1`
//! 0.33 the ECDSA verification call no longer does: `secp256k1::ecdsa::verify` supplies
//! its own (and the deprecated `Secp256k1::verify_ecdsa` ignores the context it is called
//! on), so a gateway has nothing of its own to hold for it. The workspace turns on the
//! crate's `std` feature, so that context is one per thread, allocated on the thread's first
//! verification. (Without `std` it is one global copy behind a spinlock, rebuilt on the
//! stack for every call: about 2.5 µs of each verification locally, and one memory location
//! every gateway writes. `pipeline_parts` still measures that cost alone,
//! `libsecp256k1/per_call_context`.) Every thread that verifies during a run calls
//! [`prepare_this_thread`] first, so the allocation happens before the timed flow.
//!
//! **Recovery** (the EIP-712 scheme, `eip712.rs`; D-033). There, as at Polymarket, a
//! signature comes with a recovery id, 0 or 1, and the gateway doesn't verify it against a
//! key: it finds the key that made it, and compares that key's address with the account's.
//! ECDSA's `r` is the x of a point `R` the signer made; the id says which of the two points
//! with that x (its y even or odd), and then the key is `r⁻¹ (s R − z G)`, with `z` the
//! digest. [`VerifierKind::recover`] does this with either library and returns the key's
//! address. Neither library refuses a high `s` when recovering (they do when verifying),
//! and the high twin `(r, n − s)` with the other id gives the same key, so `recover`
//! refuses a high `s` itself. [`sign_recoverable`] is the signing side, with `k256`, for
//! the load generator and the tests. libsecp256k1's recovery (its `recovery` module, which
//! the `c-secp256k1` feature turns on) uses the same per-thread context as verification,
//! so [`prepare_this_thread`] allocates it for both.
//! The signature audit, which knows each account's key but not the recovery id (it is not
//! journaled), checks such a signature with [`PublicKey::verifies_digest`] instead.
//!
//! **Complexity.** Parsing a key: one point decompression. [`PublicKey::verifies`]: one
//! SHA-256 of 72 bytes (two blocks) and one ECDSA verification. [`VerifierKind::recover`]:
//! one point decompression (`R` from `r`) and the same double multiplication as a
//! verification, then one keccak-256 for the address.

use std::fmt;

use k256::ecdsa::signature::Verifier as _;
use k256::ecdsa::signature::hazmat::PrehashVerifier as _;
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
#[cfg(feature = "c-secp256k1")]
use k256::sha2::{Digest, Sha256};

use crate::eip712::{Address, address_of};
use crate::wire::{SIGNATURE_BYTES, SIGNED_BYTES, is_low_s};

/// The wrapper crate itself, for the benchmark that measures its per-call context
/// (`bench/benches/pipeline_parts.rs`; module docs, "No context object").
#[cfg(feature = "c-secp256k1")]
pub use secp256k1;

/// Bytes in a compressed public key (SEC1: a tag byte, 2 or 3, and `x`).
pub const KEY_BYTES: usize = 33;

/// The secp256k1 generator point G, compressed: the tag 2, then G's x. Both warm-ups use
/// it: [`prepare_this_thread`] as a key, and `Gateway::prepare_this_thread` (the EIP-712
/// scheme's) its x as an `r`.
pub(crate) const GENERATOR: [u8; KEY_BYTES] = [
    0x02, 0x79, 0xBE, 0x66, 0x7E, 0xF9, 0xDC, 0xBB, 0xAC, 0x55, 0xA0, 0x62, 0x95, 0xCE, 0x87, 0x0B, 0x07,
    0x02, 0x9B, 0xFC, 0xDB, 0x2D, 0xCE, 0x28, 0xD9, 0x59, 0xF2, 0x81, 0x5B, 0x16, 0xF8, 0x17, 0x98,
];

/// Gets this thread ready to verify without allocating (module docs, "No context object").
/// With the `c-secp256k1` feature, it runs one libsecp256k1 verification that fails (the
/// generator point as the key, `r = s = 1`), which allocates this thread's context. Without
/// the feature it does nothing: `k256` keeps no per-thread state. (libsecp256k1's recovery
/// uses the same context. The EIP-712 gateway's own warm-up runs one recovery, on the
/// threads that recover; module docs, "Recovery".)
pub fn prepare_this_thread() {
    #[cfg(feature = "c-secp256k1")]
    {
        let key = secp256k1::PublicKey::from_byte_array_compressed(GENERATOR).expect("G is a valid key");
        let mut compact = [0u8; SIGNATURE_BYTES];
        compact[31] = 1; // r = 1
        compact[63] = 1; // s = 1
        let signature =
            secp256k1::ecdsa::Signature::from_compact(&compact).expect("r = s = 1 is well formed");
        let message = secp256k1::Message::from_digest([0; 32]);
        let verified = secp256k1::ecdsa::verify(&signature, message, &key).is_ok();
        debug_assert!(!verified, "the warm-up signature must not verify");
    }
}

/// `digest` signed with `key` so that the signer can be recovered (D-033; module docs,
/// "Recovery"), as Polymarket's SDKs sign: deterministically (RFC 6979) and low-S. Returns
/// `r || s` and the recovery id, 0 or 1 (Ethereum's `v` is 27 plus it). For the load
/// generator and the tests; the gateways only recover.
pub fn sign_recoverable(key: &SigningKey, digest: &[u8; 32]) -> ([u8; SIGNATURE_BYTES], u8) {
    // `k256` returns the low-S form, with the id's y-parity bit flipped to match it.
    let (signature, id) = key.sign_prehash_recoverable(digest);
    // The id's other bit says R's x was at least n before it was reduced to r: probability
    // about 2^-128, and Ethereum's `v` has no value for it.
    assert!(!id.is_x_reduced(), "a recovery id of 2 or 3 (probability about 2^-128)");
    (signature.to_bytes().into(), id.to_byte())
}

/// A signature verifier (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VerifierKind {
    /// RustCrypto's `k256`, pure Rust: in every build, and the default.
    K256,
    /// Bitcoin Core's libsecp256k1 (C), through rust-bitcoin's `secp256k1` crate: only in a
    /// build with the `c-secp256k1` feature.
    LibSecp256k1,
}

impl VerifierKind {
    /// Both verifiers, `k256` first.
    pub const ALL: [VerifierKind; 2] = [VerifierKind::K256, VerifierKind::LibSecp256k1];

    /// The name the command line, the summaries and the reports use.
    pub fn name(self) -> &'static str {
        match self {
            VerifierKind::K256 => "k256",
            VerifierKind::LibSecp256k1 => "libsecp256k1",
        }
    }

    /// The verifier called `name` ([`VerifierKind::name`]), built or not.
    pub fn from_name(name: &str) -> Result<VerifierKind, String> {
        VerifierKind::ALL
            .into_iter()
            .find(|kind| kind.name() == name)
            .ok_or_else(|| format!("not a verifier: {name:?} (k256, libsecp256k1)"))
    }

    /// True if this build has the verifier.
    pub const fn is_built(self) -> bool {
        match self {
            VerifierKind::K256 => true,
            VerifierKind::LibSecp256k1 => cfg!(feature = "c-secp256k1"),
        }
    }

    /// Every verifier this build has, `k256` first.
    pub fn built() -> impl Iterator<Item = VerifierKind> {
        VerifierKind::ALL.into_iter().filter(|kind| kind.is_built())
    }

    /// `Ok` if this build has the verifier; else the error to stop at, at start.
    pub fn check_built(self) -> Result<(), NotBuilt> {
        if self.is_built() { Ok(()) } else { Err(NotBuilt(self)) }
    }

    /// The address of the key that made `signature` (`r || s`, big-endian) over the 32-byte
    /// `digest`, found from the signature and `recovery_id` (module docs, "Recovery").
    /// `None` if `recovery_id` is not 0 or 1, if `s` is high, if `r` or `s` is 0 or not below
    /// `n`, or if no key comes out (`r` is not the x of a point); also `None` for a verifier
    /// this build doesn't have. Any other signature gives *some* address: whether it is the
    /// signer's is the caller's comparison.
    pub fn recover(
        self,
        digest: &[u8; 32],
        signature: &[u8; SIGNATURE_BYTES],
        recovery_id: u8,
    ) -> Option<Address> {
        let s = signature.last_chunk::<32>().expect("s is the last 32 bytes");
        if recovery_id > 1 || !is_low_s(s) {
            return None;
        }
        match self {
            VerifierKind::K256 => {
                // `from_slice` refuses r or s equal to 0 or not below n. The id is R's y
                // parity; "x reduced" (ids 2 and 3) is refused above.
                let signature = Signature::from_slice(signature).ok()?;
                let id = RecoveryId::new(recovery_id == 1, false);
                let key = VerifyingKey::recover_from_prehash(digest, &signature, id).ok()?;
                Some(address_of(&k256_uncompressed(&key)))
            }
            // `from_compact` refuses r or s not below n; `recover_ecdsa` refuses r or s
            // equal to 0.
            #[cfg(feature = "c-secp256k1")]
            VerifierKind::LibSecp256k1 => {
                let id = secp256k1::ecdsa::RecoveryId::from_u8_masked(recovery_id);
                let signature = secp256k1::ecdsa::RecoverableSignature::from_compact(signature, id).ok()?;
                let key = signature.recover_ecdsa(secp256k1::Message::from_digest(*digest)).ok()?;
                Some(address_of(&key.serialize_uncompressed()))
            }
            #[cfg(not(feature = "c-secp256k1"))]
            VerifierKind::LibSecp256k1 => None,
        }
    }
}

impl fmt::Display for VerifierKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A verifier was asked for that this build doesn't have (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotBuilt(pub VerifierKind);

impl fmt::Display for NotBuilt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the {} verifier is not in this build: build with `--features c-secp256k1`", self.0)
    }
}

impl std::error::Error for NotBuilt {}

/// A registered public key, parsed for one verifier (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicKey {
    K256(VerifyingKey),
    #[cfg(feature = "c-secp256k1")]
    LibSecp256k1(secp256k1::PublicKey),
}

impl PublicKey {
    /// Parses a 33-byte compressed SEC1 key for `verifier`. `None` if the bytes are not a
    /// point on secp256k1 (a tag other than 2 or 3, `x` not below `p`, or no point with
    /// that `x`); also `None` for a verifier this build doesn't have, which callers refuse
    /// first, with [`VerifierKind::check_built`], to say so.
    pub fn from_compressed(verifier: VerifierKind, bytes: &[u8; KEY_BYTES]) -> Option<PublicKey> {
        // The tag first, for both: `k256` would also take SEC1's "compact" form, tag 5 and
        // `x` alone (33 bytes too), which libsecp256k1 refuses.
        if !matches!(bytes[0], 2 | 3) {
            return None;
        }
        match verifier {
            VerifierKind::K256 => VerifyingKey::from_sec1_bytes(bytes).ok().map(PublicKey::K256),
            #[cfg(feature = "c-secp256k1")]
            VerifierKind::LibSecp256k1 => {
                secp256k1::PublicKey::from_byte_array_compressed(*bytes).ok().map(PublicKey::LibSecp256k1)
            }
            #[cfg(not(feature = "c-secp256k1"))]
            VerifierKind::LibSecp256k1 => None,
        }
    }

    /// The key's 33-byte compressed form, as `keys.txt` lists it (5.4).
    pub fn to_compressed(&self) -> [u8; KEY_BYTES] {
        match self {
            PublicKey::K256(key) => {
                let point = key.to_sec1_point(true);
                point.as_bytes().try_into().expect("a compressed point is 33 bytes")
            }
            #[cfg(feature = "c-secp256k1")]
            PublicKey::LibSecp256k1(key) => key.serialize(),
        }
    }

    /// The key's Ethereum address (D-033): the last 20 bytes of keccak-256 of its `x || y`
    /// (`eip712::address_of`), to compare with the address [`VerifierKind::recover`] finds.
    pub fn address(&self) -> Address {
        match self {
            PublicKey::K256(key) => address_of(&k256_uncompressed(key)),
            #[cfg(feature = "c-secp256k1")]
            PublicKey::LibSecp256k1(key) => address_of(&key.serialize_uncompressed()),
        }
    }

    /// The verifier the key was parsed for.
    pub fn verifier(&self) -> VerifierKind {
        match self {
            PublicKey::K256(_) => VerifierKind::K256,
            #[cfg(feature = "c-secp256k1")]
            PublicKey::LibSecp256k1(_) => VerifierKind::LibSecp256k1,
        }
    }

    /// True if `signature` (`r || s`, big-endian) is this key's ECDSA signature over the
    /// SHA-256 of `signed` (module docs, "Contract").
    pub fn verifies(&self, signed: &[u8; SIGNED_BYTES], signature: &[u8; SIGNATURE_BYTES]) -> bool {
        match self {
            // `from_slice` refuses r or s equal to 0 or not below n; `verify` hashes
            // `signed` with SHA-256 and refuses a high s (5.2).
            PublicKey::K256(key) => {
                Signature::from_slice(signature).is_ok_and(|signature| key.verify(signed, &signature).is_ok())
            }
            // `from_compact` refuses r or s not below n; `verify` refuses r or s equal to
            // 0, and a high s.
            #[cfg(feature = "c-secp256k1")]
            PublicKey::LibSecp256k1(key) => {
                let digest: [u8; 32] = Sha256::digest(signed).into();
                let message = secp256k1::Message::from_digest(digest);
                secp256k1::ecdsa::Signature::from_compact(signature)
                    .is_ok_and(|signature| secp256k1::ecdsa::verify(&signature, message, key).is_ok())
            }
        }
    }

    /// True if `signature` (`r || s`, big-endian) is this key's ECDSA signature over the
    /// 32-byte `digest` itself, which is not hashed again: the EIP-712 scheme's check in
    /// the signature audit, which knows the account's key but not the recovery id (D-033).
    /// The same refusals as [`PublicKey::verifies`].
    pub fn verifies_digest(&self, digest: &[u8; 32], signature: &[u8; SIGNATURE_BYTES]) -> bool {
        match self {
            // `from_slice` refuses r or s equal to 0 or not below n; `verify_prehash`
            // refuses a high s, and reads the digest as the number to sign.
            PublicKey::K256(key) => Signature::from_slice(signature)
                .is_ok_and(|signature| key.verify_prehash(digest, &signature).is_ok()),
            // `from_compact` refuses r or s not below n; `verify` refuses r or s equal to
            // 0, and a high s.
            #[cfg(feature = "c-secp256k1")]
            PublicKey::LibSecp256k1(key) => {
                let message = secp256k1::Message::from_digest(*digest);
                secp256k1::ecdsa::Signature::from_compact(signature)
                    .is_ok_and(|signature| secp256k1::ecdsa::verify(&signature, message, key).is_ok())
            }
        }
    }
}

/// A `k256` key in SEC1's uncompressed form, `4 || x || y`.
fn k256_uncompressed(key: &VerifyingKey) -> [u8; 65] {
    key.to_sec1_point(false).as_bytes().try_into().expect("an uncompressed point is 65 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keccak::keccak256;
    use crate::test_support::{
        ACCOUNT_9_PUBLIC_KEY, VERIFIER, acct, bytes, high_s_twin, public_key, signing_key,
    };

    #[test]
    fn names_round_trip_and_an_unknown_name_is_refused() {
        for kind in VerifierKind::ALL {
            assert_eq!(VerifierKind::from_name(kind.name()), Ok(kind));
            assert_eq!(kind.to_string(), kind.name());
        }
        assert!(VerifierKind::from_name("libsecp").unwrap_err().contains("k256, libsecp256k1"));
    }

    #[test]
    fn k256_is_always_built_and_libsecp256k1_only_with_the_feature() {
        assert_eq!(VerifierKind::K256.check_built(), Ok(()));
        let libsecp = VerifierKind::LibSecp256k1.check_built();
        if cfg!(feature = "c-secp256k1") {
            assert_eq!(libsecp, Ok(()));
            assert_eq!(VerifierKind::built().collect::<Vec<_>>(), VerifierKind::ALL);
        } else {
            let error = libsecp.expect_err("not built").to_string();
            assert!(error.contains("`--features c-secp256k1`"), "{error}");
            assert_eq!(VerifierKind::built().collect::<Vec<_>>(), [VerifierKind::K256]);
        }
    }

    #[test]
    fn a_key_round_trips_through_its_compressed_form_for_the_tests_verifier() {
        let compressed: [u8; KEY_BYTES] = bytes(ACCOUNT_9_PUBLIC_KEY).try_into().expect("33 bytes");
        let key = PublicKey::from_compressed(VERIFIER, &compressed).expect("a point");
        assert_eq!(key.to_compressed(), compressed);
        assert_eq!(key.verifier(), VERIFIER);
        assert_eq!(key, public_key(&signing_key(1, acct(9))));
        let mut off_curve = [0; KEY_BYTES];
        off_curve[0] = 2;
        off_curve[KEY_BYTES - 1] = 5; // x = 5: 5^3 + 7 = 132 is not a square modulo p
        assert_eq!(PublicKey::from_compressed(VERIFIER, &off_curve), None);
    }

    #[test]
    fn recovery_finds_the_signer_and_refuses_bad_ids_high_s_and_zeros() {
        let key = signing_key(1, acct(9));
        let address = public_key(&key).address();
        assert_eq!(address, PublicKey::K256(*key.verifying_key()).address());
        let digest = keccak256(b"any 32 bytes");
        let (signature, recovery_id) = sign_recoverable(&key, &digest);
        assert!(recovery_id <= 1);
        assert!(is_low_s(signature.last_chunk().expect("s")));
        assert_eq!(VERIFIER.recover(&digest, &signature, recovery_id), Some(address));

        // The other id names the other point with x = r: another key. So does another digest.
        let other_id = VERIFIER.recover(&digest, &signature, 1 - recovery_id);
        assert!(other_id.is_some_and(|other| other != address));
        let other_digest = VERIFIER.recover(&keccak256(b"other bytes"), &signature, recovery_id);
        assert!(other_digest.is_some_and(|other| other != address));

        // Ids 2 and 3 ("x reduced"), and Ethereum's v values 27 and 28, are not ids here.
        for id in [2, 3, 27, 28] {
            assert_eq!(VERIFIER.recover(&digest, &signature, id), None, "id {id}");
        }

        // The high-S twin with the other id: `k256` itself recovers the signer's key from
        // it, which is why `recover` checks s first.
        let twin = high_s_twin(&signature);
        let twin_signature = Signature::from_slice(&twin).expect("in range");
        let flipped = RecoveryId::new(recovery_id == 0, false);
        let from_twin = VerifyingKey::recover_from_prehash(&digest, &twin_signature, flipped).ok();
        assert_eq!(from_twin, Some(*key.verifying_key()));
        assert_eq!(VERIFIER.recover(&digest, &twin, 1 - recovery_id), None);

        // r or s of 0.
        let mut zero_r = signature;
        zero_r[..32].fill(0);
        let mut zero_s = signature;
        zero_s[32..].fill(0);
        for (what, bad) in [("r = 0", zero_r), ("s = 0", zero_s)] {
            for id in [0, 1] {
                assert_eq!(VERIFIER.recover(&digest, &bad, id), None, "{what}, id {id}");
            }
        }

        // A verifier this build doesn't have recovers nothing.
        if !VerifierKind::LibSecp256k1.is_built() {
            assert_eq!(VerifierKind::LibSecp256k1.recover(&digest, &signature, recovery_id), None);
        }
    }

    #[test]
    fn a_signature_over_a_digest_verifies_with_the_signers_key_only() {
        // The audit's check in the EIP-712 scheme: the registered key, and no recovery id.
        let key = signing_key(1, acct(9));
        let digest = keccak256(b"any 32 bytes");
        let (signature, _) = sign_recoverable(&key, &digest);
        assert!(public_key(&key).verifies_digest(&digest, &signature));
        assert!(!public_key(&key).verifies_digest(&keccak256(b"other bytes"), &signature), "another digest");
        assert!(!public_key(&signing_key(1, acct(10))).verifies_digest(&digest, &signature), "another key");
        assert!(!public_key(&key).verifies_digest(&digest, &high_s_twin(&signature)), "the high-S twin");
        let mut zero_r = signature;
        zero_r[..32].fill(0);
        assert!(!public_key(&key).verifies_digest(&digest, &zero_r), "r = 0");
    }
}

/// The two verifiers against each other (PIPELINE.md 5.7, 18.1): only in a build that has
/// both. Every case is decided by both, and both must answer the same.
#[cfg(all(test, feature = "c-secp256k1"))]
mod cross_check {
    use super::*;
    use crate::keccak::keccak256;
    use crate::test_support::{
        ACCOUNT_9_PUBLIC_KEY, XorShift, acct, bytes, cancel, high_s_twin, modify, place, signing_key,
    };
    use crate::wire::{ORDER, encode_signed_part};
    use engine::command::Command;
    use k256::ecdsa::SigningKey;
    use k256::ecdsa::signature::Signer;

    /// The key of `signing` parsed for each verifier: `[k256, libsecp256k1]`.
    fn both(signing: &SigningKey) -> [PublicKey; 2] {
        let compressed = PublicKey::K256(*signing.verifying_key()).to_compressed();
        VerifierKind::ALL.map(|kind| PublicKey::from_compressed(kind, &compressed).expect("a point"))
    }

    /// What each verifier says, `[k256, libsecp256k1]`.
    fn answers(
        keys: &[PublicKey; 2],
        signed: &[u8; SIGNED_BYTES],
        signature: &[u8; SIGNATURE_BYTES],
    ) -> [bool; 2] {
        keys.map(|key| key.verifies(signed, signature))
    }

    /// What each verifier recovers, `[k256, libsecp256k1]`.
    fn recoveries(digest: &[u8; 32], signature: &[u8; SIGNATURE_BYTES], id: u8) -> [Option<Address>; 2] {
        VerifierKind::ALL.map(|kind| kind.recover(digest, signature, id))
    }

    /// `signed`, signed by `key` (k256, RFC 6979, low-S), as `r || s`.
    fn sign(key: &SigningKey, signed: &[u8; SIGNED_BYTES]) -> [u8; SIGNATURE_BYTES] {
        let signature: Signature = key.sign(signed);
        signature.to_bytes().into()
    }

    /// `signature` with byte `index` flipped by `mask`.
    fn flipped(signature: &[u8; SIGNATURE_BYTES], index: usize, mask: u8) -> [u8; SIGNATURE_BYTES] {
        let mut edited = *signature;
        edited[index] ^= mask;
        edited
    }

    #[test]
    fn both_accept_what_k256_signs_and_refuse_the_same_edits() {
        let mut random = XorShift(0xC055_C4EC);
        let mut cases = 0;
        for n in 1..=40u32 {
            let account = acct(n);
            let key = signing_key(1 + u64::from(n % 3), account);
            let keys = both(&key);
            let other = both(&signing_key(99, account));
            for sequence in 0..6u32 {
                let command: Command = match sequence % 3 {
                    0 => place(account, sequence),
                    1 => cancel(account, sequence),
                    _ => modify(account, sequence),
                };
                let nonce = random.next();
                let expires_at = random.next();
                let signed = encode_signed_part(random.below(4) as u32, account, nonce, expires_at, &command);
                let signature = sign(&key, &signed);
                let here = format!("account {account}, sequence {sequence}");
                assert_eq!(answers(&keys, &signed, &signature), [true, true], "{here}: genuine");

                // One flipped byte in the signed bytes, in r, in s: anywhere, any bit.
                let mut edited = signed;
                let bit = 1 << random.below(8);
                edited[random.below(SIGNED_BYTES as u64) as usize] ^= bit;
                assert_eq!(answers(&keys, &edited, &signature), [false, false], "{here}: signed bytes");
                let in_r = flipped(&signature, random.below(32) as usize, bit);
                assert_eq!(answers(&keys, &signed, &in_r), [false, false], "{here}: r");
                let in_s = flipped(&signature, 32 + random.below(32) as usize, bit);
                assert_eq!(answers(&keys, &signed, &in_s), [false, false], "{here}: s");

                // The right signature, checked against another account's key.
                assert_eq!(answers(&other, &signed, &signature), [false, false], "{here}: wrong key");

                // The high-S twin satisfies the ECDSA equation, and both refuse it anyway.
                let twin = high_s_twin(&signature);
                assert_eq!(answers(&keys, &signed, &twin), [false, false], "{here}: high-S twin");
                cases += 1;
            }
        }
        assert_eq!(cases, 240);
    }

    #[test]
    fn both_refuse_r_or_s_of_zero_or_at_least_n() {
        let key = signing_key(1, acct(9));
        let keys = both(&key);
        let signed = encode_signed_part(1, acct(9), 1, u64::MAX, &place(acct(9), 1));
        let genuine = sign(&key, &signed);
        assert_eq!(answers(&keys, &signed, &genuine), [true, true], "so each refusal below is the edit's");
        let mut n_plus_1 = ORDER;
        n_plus_1[31] += 1; // n ends in 0x41: no carry
        for (what, value) in [("0", [0; 32]), ("n", ORDER), ("n + 1", n_plus_1), ("2^256 − 1", [0xFF; 32])]
        {
            let mut r = genuine;
            r[..32].copy_from_slice(&value);
            assert_eq!(answers(&keys, &signed, &r), [false, false], "r = {what}");
            let mut s = genuine;
            s[32..].copy_from_slice(&value);
            assert_eq!(answers(&keys, &signed, &s), [false, false], "s = {what}");
        }
    }

    #[test]
    fn both_verify_the_worked_example_of_5_6_and_refuse_its_twin() {
        // The fixed vector of PIPELINE.md 5.6: bytes, public key, r and s, as the spec
        // prints them (computed there with two independent Python models).
        let signed: [u8; SIGNED_BYTES] = bytes(
            "50 45 52 50 01 00 00 00 01 00 00 00 09 00 00 00
             01 00 00 00 00 00 00 00 00 ac 16 20 8b 5b d7 18
             01 00 00 01 03 00 00 00 01 00 00 00 09 00 00 00
             56 92 01 00 00 00 00 00 20 a1 07 00 00 00 00 00
             00 00 00 00 00 00 00 00",
        )
        .try_into()
        .expect("72 bytes");
        let digest: [u8; 32] = Sha256::digest(signed).into();
        assert_eq!(digest[..], bytes("1b3977db461c6b73508894a9d1075fbb2ff90a9c6013d18a1718138390a8f19d")[..]);
        let compressed: [u8; KEY_BYTES] = bytes(ACCOUNT_9_PUBLIC_KEY).try_into().expect("33 bytes");
        let keys =
            VerifierKind::ALL.map(|kind| PublicKey::from_compressed(kind, &compressed).expect("a point"));
        let signature: [u8; SIGNATURE_BYTES] = bytes(
            "962936e1f02f1c3022212df5bb4c09034bd81fb49f5aca9e04d826e8f06c1dd5
             753521c3a0eb5f3a8ae85e6ecfad672bf6543d718e169e076d2c1fd8cb607010",
        )
        .try_into()
        .expect("64 bytes");
        assert_eq!(answers(&keys, &signed, &signature), [true, true]);
        let twin = high_s_twin(&signature);
        assert_eq!(twin[32..], bytes("8acade3c5f14a0c57517a191305298d2c45a9f752132023452a63eb404d5d131")[..]);
        assert_eq!(answers(&keys, &signed, &twin), [false, false], "the high-S twin");
        // Flipping the deployment byte (offset 8) or an expiry byte (offset 24), 5.6.
        for offset in [8, 24] {
            let mut edited = signed;
            edited[offset] ^= 1;
            assert_eq!(answers(&keys, &edited, &signature), [false, false], "offset {offset}");
        }
    }

    #[test]
    fn both_recover_the_same_signer_and_refuse_the_same_edits() {
        let mut random = XorShift(0x5EC0_4E4C);
        let mut cases = 0;
        for n in 1..=40u32 {
            let account = acct(n);
            let key = signing_key(1 + u64::from(n % 3), account);
            let [k256, libsecp] = both(&key);
            let address = k256.address();
            assert_eq!(libsecp.address(), address, "account {account}");
            for sequence in 0..6u32 {
                let digest = keccak256(&random.next().to_le_bytes());
                let (signature, id) = sign_recoverable(&key, &digest);
                let here = format!("account {account}, sequence {sequence}");
                assert_eq!(recoveries(&digest, &signature, id), [Some(address); 2], "{here}: genuine");

                // One flipped bit in the digest, in r, in s, or the other id: both find the
                // same other key, or both none; never the signer's.
                let bit = 1 << random.below(8);
                let mut edited = digest;
                edited[random.below(32) as usize] ^= bit;
                let in_r = flipped(&signature, random.below(32) as usize, bit);
                let in_s = flipped(&signature, 32 + random.below(32) as usize, bit);
                for (what, [by_k256, by_libsecp]) in [
                    ("digest", recoveries(&edited, &signature, id)),
                    ("r", recoveries(&digest, &in_r, id)),
                    ("s", recoveries(&digest, &in_s, id)),
                    ("the other id", recoveries(&digest, &signature, 1 - id)),
                ] {
                    assert_eq!(by_k256, by_libsecp, "{here}: {what}");
                    assert_ne!(by_k256, Some(address), "{here}: {what}");
                }

                // Ids that are not 0 or 1, and the high-S twin with either id: none.
                for bad_id in [2, 3, 27, 28] {
                    assert_eq!(recoveries(&digest, &signature, bad_id), [None; 2], "{here}: id {bad_id}");
                }
                let twin = high_s_twin(&signature);
                for either in [0, 1] {
                    assert_eq!(recoveries(&digest, &twin, either), [None; 2], "{here}: high-S twin");
                }

                // The audit's check (the registered key over the digest): the same answers.
                let with_key = |digest: &[u8; 32], signature| {
                    [k256, libsecp].map(|key| key.verifies_digest(digest, signature))
                };
                assert_eq!(with_key(&digest, &signature), [true; 2], "{here}: with the key");
                for (what, answers) in [
                    ("digest", with_key(&edited, &signature)),
                    ("r", with_key(&digest, &in_r)),
                    ("s", with_key(&digest, &in_s)),
                    ("high-S twin", with_key(&digest, &twin)),
                ] {
                    assert_eq!(answers, [false; 2], "{here}: with the key, {what}");
                }
                cases += 1;
            }
        }
        assert_eq!(cases, 240);
    }

    #[test]
    fn both_refuse_to_recover_from_r_or_s_of_zero_or_at_least_n() {
        let key = signing_key(1, acct(9));
        let digest = keccak256(b"a digest");
        let (genuine, id) = sign_recoverable(&key, &digest);
        let address = both(&key)[0].address();
        assert_eq!(
            recoveries(&digest, &genuine, id),
            [Some(address); 2],
            "so each refusal below is the edit's"
        );
        let mut n_plus_1 = ORDER;
        n_plus_1[31] += 1; // n ends in 0x41: no carry
        for (what, value) in [("0", [0; 32]), ("n", ORDER), ("n + 1", n_plus_1), ("2^256 − 1", [0xFF; 32])]
        {
            let mut r = genuine;
            r[..32].copy_from_slice(&value);
            let mut s = genuine;
            s[32..].copy_from_slice(&value);
            for either in [0, 1] {
                assert_eq!(recoveries(&digest, &r, either), [None; 2], "r = {what}, id {either}");
                assert_eq!(recoveries(&digest, &s, either), [None; 2], "s = {what}, id {either}");
            }
        }
    }

    #[test]
    fn both_parse_the_same_keys_and_refuse_the_same_bytes() {
        for account in (1..=20).map(acct) {
            let [k256, libsecp] = both(&signing_key(1, account));
            assert_eq!(k256.to_compressed(), libsecp.to_compressed(), "account {account}");
            assert_eq!(
                (k256.verifier(), libsecp.verifier()),
                (VerifierKind::K256, VerifierKind::LibSecp256k1)
            );
        }
        let good: [u8; KEY_BYTES] = bytes(ACCOUNT_9_PUBLIC_KEY).try_into().expect("33 bytes");
        let mut off_curve = [0; KEY_BYTES];
        off_curve[0] = 2;
        off_curve[KEY_BYTES - 1] = 5; // x = 5: no point
        let mut too_big = [0xFF; KEY_BYTES];
        too_big[0] = 3; // x = 2^256 − 1, not below p
        let mut wrong_tag = good;
        wrong_tag[0] = 4; // uncompressed, which needs 65 bytes
        let mut compact = good;
        compact[0] = 5; // SEC1's "compact" form (`x` alone, 33 bytes too)
        let bad_keys =
            [("off the curve", off_curve), ("x above p", too_big), ("tag 4", wrong_tag), ("tag 5", compact)];
        for (what, bad) in bad_keys {
            for kind in VerifierKind::ALL {
                assert_eq!(PublicKey::from_compressed(kind, &bad), None, "{kind}: {what}");
            }
        }
    }
}
