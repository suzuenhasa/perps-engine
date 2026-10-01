//! Each load-generator account's secp256k1 key, derived from the run's seed, and the key
//! registry file the gateways load (`docs/PIPELINE.md` 14.8 and 5.4; `docs/DECISIONS.md`
//! D-021).
//!
//! **Contract.** Account `a`'s private key for seed `seed` is
//! `d = SHA-256("perps-loadgen key v1" || seed as u64 LE || a as u32 LE || c as u8)`, read
//! as a big-endian number, with the counter `c` = 0, 1, 2, … until `1 <= d < n` (`n`, the
//! order of the group). A retry has probability about 2^-128, so in practice `c` is 0. The
//! same seed and account always give the same key, on every machine, with no random-number
//! crate: account 9 with seed 1 gives the key of the worked example (5.6).
//!
//! **Test keys only.** Anyone who knows the seed knows every key. That is the point for a
//! benchmark: runs are reproducible, and the pre-signed messages can be reused across runs.
//! It also means these keys must never sign anything outside a benchmark deployment (5.1).
//!
//! **The registry.** [`write_registry`] writes `keys.txt` (5.4) with every account's
//! compressed public key, through the gateway's own writer, so what is written loads back
//! and has the digest the journal's segment headers will carry.
//!
//! **Complexity.** One key costs a SHA-256 and a scalar multiplication (its public key),
//! tens of microseconds: about 0.1 s for the M3 flow's 2,870 accounts.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use engine::types::AccountId;
use gateway::registry;
use gateway::{KeyRegistry, VerifierKind};
use k256::FieldBytes;
use k256::ecdsa::{SigningKey, VerifyingKey};
use k256::sha2::{Digest, Sha256};

/// The prefix of every key derivation, so these keys can't collide with keys derived the
/// same way for another purpose.
pub const KEY_DOMAIN: &[u8] = b"perps-loadgen key v1";

/// The 32 bytes hashed for attempt `counter` at account `account`'s key (module docs).
pub fn key_candidate(seed: u64, account: AccountId, counter: u8) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(KEY_DOMAIN);
    hasher.update(seed.to_le_bytes());
    hasher.update(account.get().to_le_bytes());
    hasher.update([counter]);
    hasher.finalize().into()
}

/// Account `account`'s private key for `seed`: the first candidate in `1..n`.
/// `SigningKey::from_bytes` reads the bytes as a big-endian number and refuses 0 and
/// anything at or above `n`, which is exactly the rule.
pub fn signing_key(seed: u64, account: AccountId) -> SigningKey {
    (0..=u8::MAX)
        .find_map(|counter| {
            let candidate: FieldBytes = key_candidate(seed, account, counter).into();
            SigningKey::from_bytes(&candidate).ok()
        })
        .expect("256 candidates in a row outside 1..n: probability about 2^-32,768")
}

/// Every account's private key, by account.
pub fn signing_keys(seed: u64, accounts: &[AccountId]) -> BTreeMap<AccountId, SigningKey> {
    accounts.iter().map(|&account| (account, signing_key(seed, account))).collect()
}

/// Every account's public key, in the order given.
pub fn public_keys(seed: u64, accounts: &[AccountId]) -> Vec<(AccountId, VerifyingKey)> {
    accounts.iter().map(|&account| (account, *signing_key(seed, account).verifying_key())).collect()
}

/// The registry the gateways would load from the file [`write_registry`] writes: the same
/// keys, checks and digest, without the file (its keys parsed for `k256`).
pub fn registry(seed: u64, deployment: u32, accounts: &[AccountId]) -> KeyRegistry {
    KeyRegistry::from_keys(deployment, &public_keys(seed, accounts), VerifierKind::K256)
        .unwrap_or_else(|e| panic!("the load generator's registry is refused: {e}"))
}

/// Writes `keys.txt` (5.4) for `deployment` at `path`, with the public key of every account
/// in `accounts`, and returns its digest: the `registry_digest` of the pipeline's
/// configuration (11.2).
pub fn write_registry(
    path: &Path,
    seed: u64,
    deployment: u32,
    accounts: &[AccountId],
) -> io::Result<[u8; 32]> {
    registry::write_registry(path, deployment, &public_keys(seed, accounts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway::PublicKey;
    use gateway::registry::hex;

    #[test]
    fn account_9_with_seed_1_has_the_key_of_the_worked_example() {
        // PIPELINE.md 5.6, computed by two independent Python models.
        let key = signing_key(1, AccountId::new(9));
        assert_eq!(hex(&key.to_bytes()), "af3ac022da885faa582f5ec5f871ed9d0868a43a7bf5ddc0268209ca743cea37");
        let public = key.verifying_key().to_sec1_point(true);
        assert_eq!(
            hex(public.as_bytes()),
            "03872ba80a104a5c56609998c3da5e267e380deffe94d6e3d024969cb2e0efb729"
        );
    }

    #[test]
    fn other_accounts_and_seeds_match_the_formula() {
        // Computed with Python's hashlib from the formula in the module docs.
        let vectors: [(u64, u32, &str); 3] = [
            (1, 1_001, "b9d309fb79b8b03db5a02a3cec55b75b61efda5a8f13ba95f84b841ad949e4f0"),
            (7, 5_001, "04c50220a8ad1031bca635ce253a2b98f994dc9aed29852396a38a9181a9dd68"),
            (u64::MAX, 4_294_967_294, "446122ac9ddfdc2f7ce054247d7cc622f0e0c8b96c835fa1a72af9449b40cdca"),
        ];
        for (seed, account, expected) in vectors {
            let account = AccountId::new(account);
            assert_eq!(hex(&key_candidate(seed, account, 0)), expected);
            assert_eq!(hex(&signing_key(seed, account).to_bytes()), expected);
        }
        assert_ne!(
            signing_key(1, AccountId::new(9)).to_bytes(),
            signing_key(2, AccountId::new(9)).to_bytes()
        );
    }

    #[test]
    fn a_candidate_outside_1_to_n_is_refused_so_the_counter_moves_on() {
        // The retry rule rests on `SigningKey::from_bytes` refusing exactly 0 and n or more.
        let mut n: [u8; 32] = gateway::wire::ORDER;
        assert!(SigningKey::from_bytes(&FieldBytes::from([0; 32])).is_err());
        assert!(SigningKey::from_bytes(&FieldBytes::from(n)).is_err());
        assert!(SigningKey::from_bytes(&FieldBytes::from([0xFF; 32])).is_err());
        n[31] -= 1; // n − 1, the largest valid key
        assert!(SigningKey::from_bytes(&FieldBytes::from(n)).is_ok());
        let mut one = [0; 32];
        one[31] = 1;
        assert!(SigningKey::from_bytes(&FieldBytes::from(one)).is_ok());
    }

    #[test]
    fn the_registry_file_loads_back_with_the_same_keys_and_digest() {
        let dir = std::env::temp_dir().join(format!("loadgen-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("created");
        let path = dir.join("keys.txt");
        let accounts = [1, 9, 1_001, 5_001, 7_200].map(AccountId::new);
        let digest = write_registry(&path, 1, 42, &accounts).expect("written");
        let loaded = KeyRegistry::load(&path, 42, VerifierKind::K256).expect("the gateway loads it");
        assert_eq!(loaded.digest(), digest);
        assert_eq!(registry(1, 42, &accounts).digest(), digest);
        assert_eq!(loaded.len(), accounts.len());
        for account in accounts {
            assert_eq!(loaded.key(account), Some(&PublicKey::K256(*signing_key(1, account).verifying_key())));
        }
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("9 03872ba80a104a5c56609998c3da5e267e380deffe94d6e3d024969cb2e0efb729\n"),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }
}
