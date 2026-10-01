//! Named regression tests for the key registry (`docs/PIPELINE.md` 5.4 and 13.4). Each test
//! pins a review finding of Milestone 3 that was fixed (PIPELINE.md 22), so that it can't
//! come back. The loader's unit tests are in `gateway/src/registry.rs`.

use engine::types::AccountId;
use gateway::registry::registry_text;
use gateway::{KeyRegistry, VerifierKind};
use k256::ecdsa::{SigningKey, VerifyingKey};

/// libsecp256k1 in a build with the `c-secp256k1` feature, `k256` otherwise (as the unit
/// tests' `test_support::VERIFIER`).
const VERIFIER: VerifierKind =
    if cfg!(feature = "c-secp256k1") { VerifierKind::LibSecp256k1 } else { VerifierKind::K256 };

// F1: the loader took CRLF line endings, a missing final newline and upper-case hex, so the
// same keys loaded from several files with different digests, and an auditor who rebuilt
// the file from the clients' keys (13.4) could get a digest no segment header carries.
#[test]
fn only_the_written_spelling_of_a_registry_loads() {
    let keys: Vec<(AccountId, VerifyingKey)> = [1u32, 2, 9]
        .into_iter()
        .map(|account| {
            let key = SigningKey::from_slice(&[account as u8; 32]).expect("a valid scalar");
            (account, *key.verifying_key())
        })
        .collect();
    let canonical = registry_text(1, &keys);
    let registry = KeyRegistry::parse(canonical.as_bytes(), 1, VERIFIER).expect("the written file loads");
    assert_eq!(registry.len(), keys.len());

    let header_and_deployment = canonical.lines().take(2).map(|line| format!("{line}\n")).collect::<String>();
    let key_lines = &canonical[header_and_deployment.len()..];
    let spellings = [
        ("CRLF line endings", canonical.replace('\n', "\r\n")),
        ("no final newline", canonical.trim_end_matches('\n').to_string()),
        ("upper-case hex", format!("{header_and_deployment}{}", key_lines.to_uppercase())),
    ];
    for (what, text) in &spellings {
        let loaded = KeyRegistry::parse(text.as_bytes(), 1, VERIFIER);
        assert!(loaded.is_err(), "{what}: the same keys loaded under another digest");
    }
}
