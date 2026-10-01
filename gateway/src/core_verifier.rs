//! The core's signature check in the "verify on core" ablation (`docs/PIPELINE.md` section
//! 16): the one place a signature is verified outside a gateway thread.
//!
//! **Contract.** [`RegistryVerifier`] implements the pipeline's `CoreVerifier` with its own
//! copy of the key registry (every account's parsed key, not one gateway's share). For each
//! signed client command the core hands it, it rebuilds the 72 signed bytes from the
//! journal-like fields (the deployment, the account, the nonce, the expiry and the
//! command's CMD40; 5.1) exactly as the audit does (13.4), and runs checks 11 and 12 of 7.1
//! (low-S, then the signature) through the same function as the gateways,
//! [`crate::wire::verify_signature`].
//!
//! **Insecure by design.** In this mode the gateways skip checks 11 and 12 and have already
//! used up the nonce before anything is verified. The ablation exists only to measure what
//! verifying on the core costs, and only `e2e ablate` turns it on.
//!
//! **The perp scheme only.** It rebuilds that scheme's signed bytes. The EIP-712 scheme
//! (D-033) has no core verifier: `Pipeline::start` refuses the ablation with it, and
//! `Gateway::without_signature_checks` panics for an EIP-712 gateway.
//!
//! **Complexity.** Per command: one hash-map lookup, one SHA-256 of 72 bytes and one ECDSA
//! verification (about 50 µs), on the core thread.

use std::collections::HashMap;
use std::fmt;

use engine::command::Command;
use engine::types::AccountId;
use pipeline::codec::to_le_bytes;
use pipeline::core_thread::CoreVerifier;
use pipeline::records::SignedFields;

use crate::registry::KeyRegistry;
use crate::verifier::PublicKey;
use crate::wire::{SIGNATURE_BYTES, encode_signed_part, verify_signature};

/// Every account's key, for the core (module docs).
pub struct RegistryVerifier {
    deployment: u32,
    /// Parsed for the registry's verifier, as the gateways' are.
    keys: HashMap<AccountId, PublicKey>,
}

impl RegistryVerifier {
    /// A verifier with every key of `registry` (all gateways' accounts), parsed for the
    /// registry's verifier.
    pub fn new(registry: &KeyRegistry) -> RegistryVerifier {
        let keys = registry.partition(0, 1).map(|(account, key)| (account, *key)).collect();
        RegistryVerifier { deployment: registry.deployment(), keys }
    }
}

impl CoreVerifier for RegistryVerifier {
    fn verify(&self, account: AccountId, command: &Command, signed: &SignedFields) -> bool {
        let Some(key) = self.keys.get(&account) else { return false };
        let bytes = encode_signed_part(self.deployment, account, signed.nonce, signed.expires_at, command);
        let signature: [u8; SIGNATURE_BYTES] = to_le_bytes(&signed.signature);
        verify_signature(key, &bytes, &signature).is_ok()
    }

    fn prepare_this_thread(&self) {
        crate::verifier::prepare_this_thread();
    }
}

impl fmt::Debug for RegistryVerifier {
    /// Leaves out the keys: thousands of accounts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryVerifier")
            .field("deployment", &self.deployment)
            .field("accounts", &self.keys.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{acct, high_s_twin, message, place, registry, signing_key};
    use crate::wire::{MESSAGE_BYTES, R_OFFSET};
    use pipeline::records::signature_words;

    const DEPLOYMENT: u32 = 5;

    /// The fields the core would hand over for `message` from `account`.
    fn fields(message: &[u8; MESSAGE_BYTES], nonce: u64) -> SignedFields {
        SignedFields { nonce, expires_at: u64::MAX, signature: signature_words(message) }
    }

    #[test]
    fn it_accepts_exactly_what_a_gateway_would() {
        let verifier = RegistryVerifier::new(&registry(1, DEPLOYMENT, [3, 4, 9].map(acct)));
        let good = message(&signing_key(1, acct(9)), DEPLOYMENT, acct(9), 7, u64::MAX, &place(acct(9), 1));
        assert!(verifier.verify(acct(9), &place(acct(9), 1), &fields(&good, 7)));
        assert!(!verifier.verify(acct(9), &place(acct(9), 2), &fields(&good, 7)), "another command");
        assert!(!verifier.verify(acct(9), &place(acct(9), 1), &fields(&good, 8)), "another nonce");
        assert!(!verifier.verify(acct(4), &place(acct(9), 1), &fields(&good, 7)), "another account's key");
        assert!(!verifier.verify(acct(10), &place(acct(9), 1), &fields(&good, 7)), "no key at all");
        let mut high = good;
        let twin = high_s_twin(high[R_OFFSET..].try_into().expect("64 bytes"));
        high[R_OFFSET..].copy_from_slice(&twin);
        assert!(!verifier.verify(acct(9), &place(acct(9), 1), &fields(&high, 7)), "high-S");
        let other_deployment =
            message(&signing_key(1, acct(9)), DEPLOYMENT + 1, acct(9), 7, u64::MAX, &place(acct(9), 1));
        assert!(!verifier.verify(acct(9), &place(acct(9), 1), &fields(&other_deployment, 7)));
    }
}
