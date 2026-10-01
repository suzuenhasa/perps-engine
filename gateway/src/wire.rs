//! The signed client message: 136 bytes, a 72-byte signed part and a 64-byte signature
//! (`docs/PIPELINE.md` 5.1 to 5.3; `docs/DECISIONS.md` D-021).
//!
//! **Layout** (5.1). Our own integers are little-endian; `r` and `s` are big-endian, as SEC1
//! and every secp256k1 library write them.
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | magic, ASCII `PERP` |
//! | 4 | 2 | protocol version, `u16` = 1 |
//! | 6 | 2 | reserved, 0 |
//! | 8 | 4 | deployment id, `u32` |
//! | 12 | 4 | account, `u32`: the signer |
//! | 16 | 8 | nonce, `u64` |
//! | 24 | 8 | `expires_at`, `u64`: nanoseconds since the UNIX epoch |
//! | 32 | 40 | the command, CMD40 (4.2); its tag, byte 32, is the message type |
//! | 72 | 32 | `r` |
//! | 104 | 32 | `s` |
//!
//! **Contract.**
//! - [`encode_signed_part`] writes bytes 0..72 for a command; the client signs exactly these
//!   bytes, and [`assemble`] appends the signature.
//! - [`decode`] makes the checks of 7.1 that need nothing but the message: magic and
//!   version (`WrongDomain`), the reserved bytes and the CMD40 (`Malformed`), and the tag
//!   (`OperatorOnly`: clients may only place, cancel and modify). Comparing the deployment
//!   needs the gateway's own id, so [`crate::check::Gateway::check`] does that.
//! - [`verify_signature`] makes checks 11 and 12: low-S by our own byte comparison
//!   (`HighS`), then the verifier's range check and ECDSA verification over the SHA-256 of
//!   the signed part (`BadSignature`), with `k256` or libsecp256k1, whichever the key was
//!   parsed for (`verifier.rs`). The gateway and the signature audit (13.4) both use it.
//!
//! **The domain separator** is bytes 0 to 11 (protocol, version, deployment), and the
//! message type is byte 32. All of them are signed, so a signature made for one deployment,
//! protocol version or message type can't be moved to another (6.5, attacks 5 and 6).
//!
//! **Low-S** (5.3). For any valid `(r, s)`, `(r, n − s)` is valid too, so without a rule
//! anyone could turn one signed message into a second, different-looking one. We accept only
//! `s <= floor(n / 2)`. Two 32-byte big-endian numbers compare as numbers when compared as
//! byte arrays, lexicographically, which is what `<=` on `[u8; 32]` does. Both verifiers
//! refuse a high `s` too, but our rule doesn't depend on a library detail, and it gets its
//! own reject reason. The signature bytes are still not a message id (the key holder can
//! re-sign with another ECDSA nonce); `(deployment, account, nonce)` is.
//!
//! **Version 2: the EIP-712 scheme** (`docs/PIPELINE.md` 5.8; `docs/DECISIONS.md` D-033).
//! The opt-in second scheme, Polymarket Perps' own, uses the same 136 bytes (so the rings,
//! the lane records and the journal stay as they are), with version 2:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | magic, ASCII `PERP` |
//! | 4 | 2 | protocol version, `u16` = 2 |
//! | 6 | 1 | the recovery id `v`: 0 or 1 (Ethereum's 27 or 28, less 27) |
//! | 7 | 1 | reserved, 0 |
//! | 8 | 4 | deployment id, `u32`: the EIP-712 chain id |
//! | 12 | 4 | account, `u32`: whose registered address must have signed |
//! | 16 | 8 | salt, `u64`: any number, chosen by the client |
//! | 24 | 8 | `ts`, `u64`: milliseconds since the UNIX epoch |
//! | 32 | 40 | the command, CMD40 (4.2) |
//! | 72 | 32 | `r` |
//! | 104 | 32 | `s` |
//!
//! What is signed is not these bytes but an EIP-712 digest ([`DecodedEip712::digest`]):
//! the command's compact form, MessagePack-encoded and hashed, then the salt and the
//! timestamp, in the domain of the deployment (`eip712.rs`). The account is not in it: the
//! signer is known by its address, which the gateway recovers from `(digest, r, s, v)` and
//! compares with the account's ([`check_signer`]). The domain is the deployment, and the
//! message type is the command's form, so a signature is still good for one deployment and
//! one command only. The version keeps the two schemes apart: a gateway takes only its own
//! scheme's version, and [`decode_eip712`] refuses any other (`WrongDomain`), as [`decode`]
//! refuses version 2.
//!
//! - [`sign_eip712`] is the client's side: the digest, a recoverable signature, the message.
//! - [`decode_eip712`] makes checks 1 (magic, version 2), 2 (byte 7 zero, `v` 0 or 1, the
//!   CMD40) and 3 (the tag) of the EIP-712 order (`check.rs`).
//! - [`check_signer`] makes checks 11 to 13: low-S by our own compare (`HighS`), then the
//!   recovery (`BadSignature` if no key comes out), then the address (`WrongSigner`).
//! - [`verify_digest`] is the audit's check (13.4): the account's registered key over the
//!   digest, low-S included, with no `v` (it is not journaled).
//!
//! **Complexity.** `decode` is O(1): a few compares and one CMD40 decode. `verify_signature`
//! is one SHA-256 of 72 bytes (two blocks) and one ECDSA verification, about 50 µs.
//! `decode_eip712` is O(1) too; `check_signer` is the MessagePack encoding and three
//! keccak-256 hashes (about 1.3 µs locally, 5.8), one recovery, and one more keccak-256
//! for the address.

use engine::command::Command;
use engine::types::{AccountId, OrderId};
use k256::ecdsa::SigningKey;

use pipeline::codec::{
    COMMAND_BYTES, COMMAND_WORDS, decode_command, encode_command, from_le_bytes, to_le_bytes,
};
use pipeline::journal::format::client_order_id;
pub use pipeline::records::MESSAGE_BYTES;

use crate::check::GatewayReject;
use crate::eip712::{self, Address, Domain};
use crate::verifier::{PublicKey, VerifierKind, sign_recoverable};

/// Bytes the client signs: everything before the signature.
pub const SIGNED_BYTES: usize = 72;
/// Bytes of the signature `r || s`.
pub const SIGNATURE_BYTES: usize = 64;

/// The protocol name, bytes 0..4.
pub const MAGIC: [u8; 4] = *b"PERP";
/// The protocol version, bytes 4..6.
pub const VERSION: u16 = 1;

/// Where each field starts (the table in the module docs).
pub const MAGIC_OFFSET: usize = 0;
pub const VERSION_OFFSET: usize = 4;
pub const RESERVED_OFFSET: usize = 6;
pub const DEPLOYMENT_OFFSET: usize = 8;
pub const ACCOUNT_OFFSET: usize = 12;
pub const NONCE_OFFSET: usize = 16;
pub const EXPIRY_OFFSET: usize = 24;
pub const COMMAND_OFFSET: usize = 32;
pub const R_OFFSET: usize = 72;
pub const S_OFFSET: usize = 104;

/// The protocol version of the EIP-712 scheme's message (module docs, "Version 2").
pub const EIP712_VERSION: u16 = 2;

/// Where the EIP-712 message's own fields are (module docs, "Version 2"). The others are
/// where they are in version 1.
pub const RECOVERY_ID_OFFSET: usize = 6;
pub const EIP712_RESERVED_OFFSET: usize = 7;
pub const SALT_OFFSET: usize = NONCE_OFFSET;
pub const TS_OFFSET: usize = EXPIRY_OFFSET;

/// `n`, the order of the secp256k1 group, big-endian (5.1).
pub const ORDER: [u8; 32] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE, //
    0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C, 0xD0, 0x36, 0x41, 0x41,
];

/// `floor(n / 2)`, big-endian: the largest `s` accepted (5.3).
pub const HALF_ORDER: [u8; 32] = [
    0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, //
    0x5D, 0x57, 0x6E, 0x73, 0x57, 0xA4, 0x50, 0x1D, 0xDF, 0xE9, 0x2F, 0x46, 0x68, 0x1B, 0x20, 0xA0,
];

/// A client message whose fields passed the checks [`decode`] makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub deployment: u32,
    /// The signer.
    pub account: AccountId,
    pub nonce: u64,
    /// Nanoseconds since the UNIX epoch.
    pub expires_at: u64,
    /// A place, a cancel or a modify.
    pub command: Command,
}

impl Decoded {
    /// The order the command names; its high 32 bits are the owning account (5.5).
    pub fn order_id(&self) -> OrderId {
        client_order_id(&self.command).expect("decode admits only places, cancels and modifies")
    }
}

/// The signed part of a message (bytes 0..72) for `command`, sent by `account` with `nonce`
/// to deployment `deployment`, usable until `expires_at`.
pub fn encode_signed_part(
    deployment: u32,
    account: AccountId,
    nonce: u64,
    expires_at: u64,
    command: &Command,
) -> [u8; SIGNED_BYTES] {
    let mut bytes = [0; SIGNED_BYTES];
    bytes[MAGIC_OFFSET..VERSION_OFFSET].copy_from_slice(&MAGIC);
    bytes[VERSION_OFFSET..RESERVED_OFFSET].copy_from_slice(&VERSION.to_le_bytes());
    // Bytes 6..8 are reserved and stay 0.
    bytes[DEPLOYMENT_OFFSET..ACCOUNT_OFFSET].copy_from_slice(&deployment.to_le_bytes());
    bytes[ACCOUNT_OFFSET..NONCE_OFFSET].copy_from_slice(&account.to_le_bytes());
    bytes[NONCE_OFFSET..EXPIRY_OFFSET].copy_from_slice(&nonce.to_le_bytes());
    bytes[EXPIRY_OFFSET..COMMAND_OFFSET].copy_from_slice(&expires_at.to_le_bytes());
    let command: [u8; COMMAND_BYTES] = to_le_bytes(&encode_command(command));
    bytes[COMMAND_OFFSET..SIGNED_BYTES].copy_from_slice(&command);
    bytes
}

/// The whole message: the signed part, then the signature `r || s` (as `k256`'s
/// `Signature::to_bytes` writes it, and libsecp256k1's compact form).
pub fn assemble(signed: &[u8; SIGNED_BYTES], signature: &[u8; SIGNATURE_BYTES]) -> [u8; MESSAGE_BYTES] {
    let mut message = [0; MESSAGE_BYTES];
    message[..SIGNED_BYTES].copy_from_slice(signed);
    message[SIGNED_BYTES..].copy_from_slice(signature);
    message
}

/// Bytes 0..72: what the client signed.
pub fn signed_part(message: &[u8; MESSAGE_BYTES]) -> &[u8; SIGNED_BYTES] {
    message.first_chunk().expect("a message is longer than its signed part")
}

/// Bytes 72..136: the signature `r || s`.
pub fn signature(message: &[u8; MESSAGE_BYTES]) -> &[u8; SIGNATURE_BYTES] {
    message.last_chunk().expect("a message is longer than its signature")
}

/// The deployment id field, read without any other check.
pub fn deployment_of(message: &[u8; MESSAGE_BYTES]) -> u32 {
    u32::from_le_bytes(field(message, DEPLOYMENT_OFFSET))
}

/// Byte 32, the command's tag (the message type), read without any other check.
pub fn command_tag(message: &[u8; MESSAGE_BYTES]) -> u8 {
    message[COMMAND_OFFSET]
}

/// The CMD40 words of a message (bytes 32..72), as every ring and the journal carry them.
pub fn command_words(message: &[u8; MESSAGE_BYTES]) -> [u64; COMMAND_WORDS] {
    from_le_bytes(&field::<COMMAND_BYTES>(message, COMMAND_OFFSET))
}

/// Checks 1 (magic and version; the deployment is the gateway's), 2 and 3 of 7.1, in that
/// order, and returns the fields.
pub fn decode(message: &[u8; MESSAGE_BYTES]) -> Result<Decoded, GatewayReject> {
    let version = u16::from_le_bytes(field(message, VERSION_OFFSET));
    if field::<4>(message, MAGIC_OFFSET) != MAGIC || version != VERSION {
        return Err(GatewayReject::WrongDomain);
    }
    if field::<2>(message, RESERVED_OFFSET) != [0, 0] {
        return Err(GatewayReject::Malformed);
    }
    let command = decode_command(&command_words(message)).map_err(|_| GatewayReject::Malformed)?;
    if client_order_id(&command).is_none() {
        return Err(GatewayReject::OperatorOnly); // tags 4 to 9: deposits, marks, market setup
    }
    Ok(Decoded {
        deployment: deployment_of(message),
        account: u32::from_le_bytes(field(message, ACCOUNT_OFFSET)),
        nonce: u64::from_le_bytes(field(message, NONCE_OFFSET)),
        expires_at: u64::from_le_bytes(field(message, EXPIRY_OFFSET)),
        command,
    })
}

/// An EIP-712 message (version 2) whose fields passed the checks [`decode_eip712`] makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedEip712 {
    pub deployment: u32,
    /// The account whose registered address must be the signer's.
    pub account: AccountId,
    pub salt: u64,
    /// Milliseconds since the UNIX epoch.
    pub ts_ms: u64,
    /// 0 or 1.
    pub recovery_id: u8,
    /// A place, a cancel or a modify.
    pub command: Command,
}

impl DecodedEip712 {
    /// The order the command names; its high 32 bits are the owning account (5.5).
    pub fn order_id(&self) -> OrderId {
        client_order_id(&self.command).expect("decode_eip712 admits only places, cancels and modifies")
    }

    /// The EIP-712 digest the client signed, in `domain`, the deployment's (`eip712.rs`):
    /// the command's compact form, hashed, with the salt and the timestamp.
    pub fn digest(&self, domain: &Domain) -> [u8; 32] {
        let data =
            eip712::op_data(&self.command).expect("decode_eip712 admits only places, cancels and modifies");
        eip712::digest(domain, &data, self.salt, self.ts_ms)
    }
}

/// The whole EIP-712 message (module docs, "Version 2"): `command` for `account`, with
/// `salt` and `ts_ms`, to deployment `deployment`, carrying `signature` (`r || s`) and
/// `recovery_id`. Any `recovery_id` is written as given; [`decode_eip712`] refuses all but 0
/// and 1.
pub fn encode_eip712(
    deployment: u32,
    account: AccountId,
    salt: u64,
    ts_ms: u64,
    command: &Command,
    signature: &[u8; SIGNATURE_BYTES],
    recovery_id: u8,
) -> [u8; MESSAGE_BYTES] {
    let mut message = [0; MESSAGE_BYTES];
    message[MAGIC_OFFSET..VERSION_OFFSET].copy_from_slice(&MAGIC);
    message[VERSION_OFFSET..RECOVERY_ID_OFFSET].copy_from_slice(&EIP712_VERSION.to_le_bytes());
    message[RECOVERY_ID_OFFSET] = recovery_id;
    // Byte 7 is reserved and stays 0.
    message[DEPLOYMENT_OFFSET..ACCOUNT_OFFSET].copy_from_slice(&deployment.to_le_bytes());
    message[ACCOUNT_OFFSET..SALT_OFFSET].copy_from_slice(&account.to_le_bytes());
    message[SALT_OFFSET..TS_OFFSET].copy_from_slice(&salt.to_le_bytes());
    message[TS_OFFSET..COMMAND_OFFSET].copy_from_slice(&ts_ms.to_le_bytes());
    let command: [u8; COMMAND_BYTES] = to_le_bytes(&encode_command(command));
    message[COMMAND_OFFSET..SIGNED_BYTES].copy_from_slice(&command);
    message[SIGNED_BYTES..].copy_from_slice(signature);
    message
}

/// The client's side of the EIP-712 scheme: `command` for `account`, with `salt` and
/// `ts_ms`, signed by `key` over its digest in `domain` (RFC 6979, low-S), as a whole
/// message. The deployment is the domain's chain id. For the load generator and the tests.
///
/// Panics for an operator command, which has no signed form (clients never send one), and
/// for a domain whose chain id is not a deployment id (a `u32`).
pub fn sign_eip712(
    key: &SigningKey,
    domain: &Domain,
    account: AccountId,
    salt: u64,
    ts_ms: u64,
    command: &Command,
) -> [u8; MESSAGE_BYTES] {
    let deployment = u32::try_from(domain.chain_id()).expect("the chain id is a deployment id, a u32");
    let data = eip712::op_data(command).expect("only places, cancels and modifies are signed");
    let (signature, recovery_id) = sign_recoverable(key, &eip712::digest(domain, &data, salt, ts_ms));
    encode_eip712(deployment, account, salt, ts_ms, command, &signature, recovery_id)
}

/// Checks 1 (magic and version 2; the deployment is the gateway's), 2 and 3 of the EIP-712
/// order (module docs, "Version 2"), in that order, and returns the fields.
pub fn decode_eip712(message: &[u8; MESSAGE_BYTES]) -> Result<DecodedEip712, GatewayReject> {
    let version = u16::from_le_bytes(field(message, VERSION_OFFSET));
    if field::<4>(message, MAGIC_OFFSET) != MAGIC || version != EIP712_VERSION {
        return Err(GatewayReject::WrongDomain);
    }
    let recovery_id = message[RECOVERY_ID_OFFSET];
    if message[EIP712_RESERVED_OFFSET] != 0 || recovery_id > 1 {
        return Err(GatewayReject::Malformed);
    }
    let command = decode_command(&command_words(message)).map_err(|_| GatewayReject::Malformed)?;
    if client_order_id(&command).is_none() {
        return Err(GatewayReject::OperatorOnly); // tags 4 to 9: deposits, marks, market setup
    }
    Ok(DecodedEip712 {
        deployment: deployment_of(message),
        account: u32::from_le_bytes(field(message, ACCOUNT_OFFSET)),
        salt: u64::from_le_bytes(field(message, SALT_OFFSET)),
        ts_ms: u64::from_le_bytes(field(message, TS_OFFSET)),
        recovery_id,
        command,
    })
}

/// Checks 11 to 13 of the EIP-712 order (module docs, "Version 2"): `HighS` if `s` is
/// above `floor(n / 2)`, before anything is hashed or recovered; then `BadSignature` if no
/// key comes out of the signature over the message's digest in `domain` (`r` or `s` 0 or
/// not below `n`, or no point with `x = r`; `VerifierKind::recover`); then `WrongSigner`
/// if a key came out but its address is not `address`, the account's.
pub fn check_signer(
    verifier: VerifierKind,
    domain: &Domain,
    decoded: &DecodedEip712,
    signature: &[u8; SIGNATURE_BYTES],
    address: &Address,
) -> Result<(), GatewayReject> {
    let s = signature.last_chunk::<32>().expect("s is the last 32 bytes");
    if !is_low_s(s) {
        return Err(GatewayReject::HighS);
    }
    let digest = decoded.digest(domain);
    match verifier.recover(&digest, signature, decoded.recovery_id) {
        None => Err(GatewayReject::BadSignature),
        Some(signer) if signer != *address => Err(GatewayReject::WrongSigner),
        Some(_) => Ok(()),
    }
}

/// The signature audit's check of an EIP-712 record (13.4; module docs, "Version 2"):
/// `HighS` if `s` is above `floor(n / 2)`; `BadSignature` if `signature` is not `key`'s
/// over `digest` (`PublicKey::verifies_digest`). It needs no recovery id: a signature by
/// `key` over `digest` is exactly one whose recovery, with the right id, gives `key`.
pub fn verify_digest(
    key: &PublicKey,
    digest: &[u8; 32],
    signature: &[u8; SIGNATURE_BYTES],
) -> Result<(), GatewayReject> {
    let s = signature.last_chunk::<32>().expect("s is the last 32 bytes");
    if !is_low_s(s) {
        return Err(GatewayReject::HighS);
    }
    if key.verifies_digest(digest, signature) { Ok(()) } else { Err(GatewayReject::BadSignature) }
}

/// True if `s` (big-endian) is at most `floor(n / 2)` (module docs, "Low-S").
pub fn is_low_s(s: &[u8; 32]) -> bool {
    *s <= HALF_ORDER
}

/// Checks 11 and 12 of 7.1: `HighS` if `s` is above `floor(n / 2)`, before the verifier is
/// called; `BadSignature` if `r` or `s` is 0 or not below `n`, or if the signature doesn't
/// verify over the SHA-256 of `signed` with `key` (5.2; `PublicKey::verifies`).
pub fn verify_signature(
    key: &PublicKey,
    signed: &[u8; SIGNED_BYTES],
    signature: &[u8; SIGNATURE_BYTES],
) -> Result<(), GatewayReject> {
    let s = signature.last_chunk::<32>().expect("s is the last 32 bytes");
    if !is_low_s(s) {
        return Err(GatewayReject::HighS);
    }
    if key.verifies(signed, signature) { Ok(()) } else { Err(GatewayReject::BadSignature) }
}

/// The `N` bytes of `message` at `offset`.
fn field<const N: usize>(message: &[u8; MESSAGE_BYTES], offset: usize) -> [u8; N] {
    message[offset..offset + N].try_into().expect("the field lies inside the message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, VERIFIER, bytes, high_s_twin, public_key, signing_key};
    use engine::command::{CancelOrder, Deposit, ModifyOrder, PlaceOrder, SetMark};
    use engine::types::{Side, TimeInForce, order_id};
    use k256::ecdsa::signature::Signer;
    use k256::ecdsa::{Signature, SigningKey};
    use k256::sha2::{Digest, Sha256};

    /// The signed bytes of the worked example (5.6).
    const EXAMPLE: &str = "
        50 45 52 50 01 00 00 00 01 00 00 00 09 00 00 00
        01 00 00 00 00 00 00 00 00 ac 16 20 8b 5b d7 18
        01 00 00 01 03 00 00 00 01 00 00 00 09 00 00 00
        56 92 01 00 00 00 00 00 20 a1 07 00 00 00 00 00
        00 00 00 00 00 00 00 00";

    fn example_place() -> Command {
        Command::PlaceOrder(PlaceOrder {
            order_id: order_id(9, 1),
            price: 102_998,
            qty: 500_000,
            market: 3,
            side: Side::Buy,
            tif: TimeInForce::Gtc,
            post_only: true,
        })
    }

    const EXAMPLE_EXPIRY: u64 = 1_790_000_030_000_000_000;

    #[test]
    fn the_worked_example_encodes_to_the_specs_bytes() {
        let signed = encode_signed_part(1, 9, 1, EXAMPLE_EXPIRY, &example_place());
        assert_eq!(signed[..], bytes(EXAMPLE)[..]);
    }

    /// The known-answer test of 5.6 (PIPELINE.md 18.1): the key derived from seed 1 for
    /// account 9 (14.8), its public key, the digest, and the RFC 6979 signature `k256`
    /// makes, which is already low-S; and its high-S twin. The tests' verifier checks it
    /// (`test_support::VERIFIER`).
    #[test]
    fn the_worked_example_signs_to_the_specs_signature() {
        let key = test_support::signing_key(1, 9);
        assert_eq!(key.to_bytes()[..], bytes(test_support::ACCOUNT_9_PRIVATE_KEY)[..]);
        let public = key.verifying_key().to_sec1_point(true);
        assert_eq!(public.as_bytes(), &bytes(test_support::ACCOUNT_9_PUBLIC_KEY)[..]);

        let signed = encode_signed_part(1, 9, 1, EXAMPLE_EXPIRY, &example_place());
        let digest = Sha256::digest(signed);
        assert_eq!(digest[..], bytes("1b3977db461c6b73508894a9d1075fbb2ff90a9c6013d18a1718138390a8f19d")[..]);

        let signature: Signature = key.sign(&signed);
        let (r, s) = (signature.to_bytes()[..32].to_vec(), signature.to_bytes()[32..].to_vec());
        assert_eq!(r, bytes("962936e1f02f1c3022212df5bb4c09034bd81fb49f5aca9e04d826e8f06c1dd5"));
        assert_eq!(s, bytes("753521c3a0eb5f3a8ae85e6ecfad672bf6543d718e169e076d2c1fd8cb607010"));
        assert!(is_low_s(&s.clone().try_into().expect("32 bytes")));
        let signature: [u8; SIGNATURE_BYTES] = signature.to_bytes().into();
        assert_eq!(verify_signature(&public_key(&key), &signed, &signature), Ok(()));

        // n − s, the high-S twin: also a valid ECDSA signature, refused as HighS.
        let twin = test_support::high_s_twin(&signature);
        assert_eq!(twin[32..], bytes("8acade3c5f14a0c57517a191305298d2c45a9f752132023452a63eb404d5d131")[..]);
        assert_eq!(verify_signature(&public_key(&key), &signed, &twin), Err(GatewayReject::HighS));
    }

    #[test]
    fn the_half_order_is_n_over_two_rounded_down() {
        // n is odd, so floor(n / 2) = (n − 1) / 2: shift n right by one bit.
        let mut half = [0u8; 32];
        let mut carry = 0;
        for (i, byte) in ORDER.iter().enumerate() {
            half[i] = (byte >> 1) | (carry << 7);
            carry = byte & 1;
        }
        assert_eq!(half, HALF_ORDER);
        let mut above = HALF_ORDER;
        above[31] += 1;
        assert!(is_low_s(&HALF_ORDER));
        assert!(!is_low_s(&above));
        assert!(is_low_s(&[0; 32]), "0 is low; the verifier refuses it (BadSignature)");
    }

    #[test]
    fn place_cancel_and_modify_encode_then_decode() {
        let commands = [
            example_place(),
            Command::CancelOrder(CancelOrder { order_id: order_id(9, 17), market: 3 }),
            Command::ModifyOrder(ModifyOrder {
                order_id: order_id(9, 17),
                new_price: 103_001,
                new_size: 250_000,
                market: 3,
            }),
        ];
        for (nonce, command) in commands.iter().enumerate() {
            let nonce = nonce as u64 + 41;
            let signed = encode_signed_part(7, 9, nonce, u64::MAX, command);
            let message = assemble(&signed, &[0x11; SIGNATURE_BYTES]);
            let decoded = decode(&message).expect("decodes");
            assert_eq!(
                decoded,
                Decoded { deployment: 7, account: 9, nonce, expires_at: u64::MAX, command: *command }
            );
            assert_eq!(decoded.order_id() >> 32, 9);
            assert_eq!(signed_part(&message), &signed);
            assert_eq!(signature(&message), &[0x11; SIGNATURE_BYTES]);
            assert_eq!(deployment_of(&message), 7);
            assert_eq!(command_tag(&message), pipeline::codec::command_tag(command));
            assert_eq!(command_words(&message), encode_command(command));
        }
    }

    #[test]
    fn decode_refuses_a_foreign_domain_bad_bytes_and_operator_commands() {
        let good = assemble(&encode_signed_part(1, 9, 1, u64::MAX, &example_place()), &[1; 64]);
        let with = |offset: usize, value: u8| {
            let mut message = good;
            message[offset] = value;
            message
        };
        assert_eq!(decode(&with(0, b'Q')), Err(GatewayReject::WrongDomain), "wrong magic");
        assert_eq!(decode(&with(VERSION_OFFSET, 2)), Err(GatewayReject::WrongDomain), "version 2");
        assert_eq!(decode(&with(RESERVED_OFFSET, 1)), Err(GatewayReject::Malformed));
        assert_eq!(decode(&with(RESERVED_OFFSET + 1, 1)), Err(GatewayReject::Malformed));
        assert_eq!(decode(&with(COMMAND_OFFSET, 0)), Err(GatewayReject::Malformed), "tag 0");
        assert_eq!(decode(&with(COMMAND_OFFSET + 1, 2)), Err(GatewayReject::Malformed), "side 2");
        assert_eq!(decode(&with(COMMAND_OFFSET + 32, 1)), Err(GatewayReject::Malformed), "f4 != 0");
        for operator in [
            Command::Deposit(Deposit { amount: 1, account: 9 }),
            Command::SetMark(SetMark { price: 1, market: 3 }),
        ] {
            let message = assemble(&encode_signed_part(1, 9, 1, u64::MAX, &operator), &[1; 64]);
            assert_eq!(decode(&message), Err(GatewayReject::OperatorOnly));
        }
        // The deployment is not decode's to judge: the gateway compares it with its own.
        assert_eq!(decode(&with(DEPLOYMENT_OFFSET, 2)).map(|d| d.deployment), Ok(2));
    }

    #[test]
    fn a_signature_by_another_key_or_over_other_bytes_is_bad() {
        let key = test_support::signing_key(1, 9);
        let other = SigningKey::from_slice(&[7; 32]).expect("a valid scalar");
        let signed = encode_signed_part(1, 9, 1, u64::MAX, &example_place());
        let by_other: Signature = other.sign(&signed);
        let by_other: [u8; 64] = by_other.normalize_s().to_bytes().into();
        assert_eq!(verify_signature(&public_key(&key), &signed, &by_other), Err(GatewayReject::BadSignature));

        let genuine: Signature = key.sign(&signed);
        let genuine: [u8; 64] = genuine.to_bytes().into();
        let mut edited = signed;
        edited[EXPIRY_OFFSET] ^= 1;
        assert_eq!(verify_signature(&public_key(&key), &edited, &genuine), Err(GatewayReject::BadSignature));
    }
    // ---- Version 2: the EIP-712 scheme (module docs, "Version 2") ----

    /// A timestamp in milliseconds, near the tests' clocks.
    const TS: u64 = 1_790_000_000_123;

    fn example_cancel() -> Command {
        Command::CancelOrder(CancelOrder { order_id: order_id(9, 17), market: 3 })
    }

    fn example_modify() -> Command {
        Command::ModifyOrder(ModifyOrder {
            order_id: order_id(9, 17),
            new_price: 103_001,
            new_size: 250_000,
            market: 3,
        })
    }

    #[test]
    fn an_eip712_message_has_the_layout_of_the_module_docs() {
        let salt = 0x0102_0304_0506_0708;
        let message = encode_eip712(7, 9, salt, TS, &example_place(), &[0x11; SIGNATURE_BYTES], 1);
        assert_eq!(&message[0..4], b"PERP");
        assert_eq!(message[4..6], [2, 0], "version 2, little-endian");
        assert_eq!(message[6], 1, "the recovery id");
        assert_eq!(message[7], 0, "reserved");
        assert_eq!(message[8..12], 7u32.to_le_bytes(), "the deployment");
        assert_eq!(message[12..16], 9u32.to_le_bytes(), "the account");
        assert_eq!(message[16..24], [8, 7, 6, 5, 4, 3, 2, 1], "the salt, little-endian");
        assert_eq!(message[24..32], TS.to_le_bytes(), "the timestamp");
        assert_eq!(command_words(&message), encode_command(&example_place()));
        assert_eq!(signature(&message), &[0x11; SIGNATURE_BYTES]);
    }

    #[test]
    fn place_cancel_and_modify_round_trip_through_version_2() {
        for (i, command) in [example_place(), example_cancel(), example_modify()].iter().enumerate() {
            for recovery_id in [0, 1] {
                let salt = u64::MAX - i as u64;
                let message = encode_eip712(7, 9, salt, TS + i as u64, command, &[0x22; 64], recovery_id);
                let decoded = decode_eip712(&message).expect("decodes");
                let expected = DecodedEip712 {
                    deployment: 7,
                    account: 9,
                    salt,
                    ts_ms: TS + i as u64,
                    recovery_id,
                    command: *command,
                };
                assert_eq!(decoded, expected);
                assert_eq!(decoded.order_id() >> 32, 9);
            }
        }
    }

    #[test]
    fn decode_eip712_refuses_another_version_bad_bytes_bad_ids_and_operator_commands() {
        let good = encode_eip712(1, 9, 1, TS, &example_place(), &[1; 64], 0);
        let with = |offset: usize, value: u8| {
            let mut message = good;
            message[offset] = value;
            message
        };
        assert_eq!(decode_eip712(&with(0, b'Q')), Err(GatewayReject::WrongDomain), "wrong magic");
        for version in [1, 3] {
            assert_eq!(
                decode_eip712(&with(VERSION_OFFSET, version)),
                Err(GatewayReject::WrongDomain),
                "{version}"
            );
        }
        assert_eq!(decode_eip712(&with(EIP712_RESERVED_OFFSET, 1)), Err(GatewayReject::Malformed), "byte 7");
        // Recovery ids 2 and 3 ("x reduced") are not used, and Ethereum's 27 and 28 are
        // written as 0 and 1 here.
        for id in [2, 3, 27, 28, 255] {
            assert_eq!(
                decode_eip712(&with(RECOVERY_ID_OFFSET, id)),
                Err(GatewayReject::Malformed),
                "v = {id}"
            );
        }
        assert_eq!(decode_eip712(&with(COMMAND_OFFSET, 0)), Err(GatewayReject::Malformed), "tag 0");
        assert_eq!(decode_eip712(&with(COMMAND_OFFSET + 1, 2)), Err(GatewayReject::Malformed), "side 2");
        for operator in [
            Command::Deposit(Deposit { amount: 1, account: 9 }),
            Command::SetMark(SetMark { price: 1, market: 3 }),
        ] {
            let message = encode_eip712(1, 9, 1, TS, &operator, &[1; 64], 0);
            assert_eq!(decode_eip712(&message), Err(GatewayReject::OperatorOnly));
        }
        // Each scheme refuses the other's version.
        let version_1 = assemble(&encode_signed_part(1, 9, 1, u64::MAX, &example_place()), &[1; 64]);
        assert_eq!(decode_eip712(&version_1), Err(GatewayReject::WrongDomain));
        assert_eq!(decode(&good), Err(GatewayReject::WrongDomain));
        // The deployment is not decode's to judge: the gateway compares it with its own.
        assert_eq!(decode_eip712(&with(DEPLOYMENT_OFFSET, 2)).map(|d| d.deployment), Ok(2));
    }

    #[test]
    fn a_signed_eip712_message_recovers_to_its_signer_and_to_nobody_else() {
        let key = signing_key(1, 9);
        let address = public_key(&key).address();
        let domain = Domain::new(1);
        let message = sign_eip712(&key, &domain, 9, 42, TS, &example_place());
        let decoded = decode_eip712(&message).expect("decodes");
        assert_eq!((decoded.deployment, decoded.account, decoded.salt, decoded.ts_ms), (1, 9, 42, TS));
        let data = eip712::op_data(&example_place()).expect("a place has a form");
        assert_eq!(decoded.digest(&domain), eip712::digest(&domain, &data, 42, TS), "eip712.rs's digest");
        let genuine = signature(&message);
        assert_eq!(check_signer(VERIFIER, &domain, &decoded, genuine, &address), Ok(()));

        // Any other digest or id recovers *some* key, not the signer's: another account's
        // address, another deployment (the domain), another salt, timestamp or command, the
        // other recovery id.
        let other = public_key(&signing_key(1, 10)).address();
        assert_eq!(
            check_signer(VERIFIER, &domain, &decoded, genuine, &other),
            Err(GatewayReject::WrongSigner)
        );
        let deployment_2 = Domain::new(2);
        assert_eq!(
            check_signer(VERIFIER, &deployment_2, &decoded, genuine, &address),
            Err(GatewayReject::WrongSigner)
        );
        let edits = [
            DecodedEip712 { salt: 43, ..decoded },
            DecodedEip712 { ts_ms: TS + 1, ..decoded },
            DecodedEip712 { command: example_cancel(), ..decoded },
            DecodedEip712 { recovery_id: 1 - decoded.recovery_id, ..decoded },
        ];
        for edited in edits {
            assert_eq!(
                check_signer(VERIFIER, &domain, &edited, genuine, &address),
                Err(GatewayReject::WrongSigner),
                "{edited:?}"
            );
        }

        // The high-S twin is refused before anything is recovered; `r = 0` recovers nothing.
        let twin = high_s_twin(genuine);
        assert_eq!(check_signer(VERIFIER, &domain, &decoded, &twin, &address), Err(GatewayReject::HighS));
        let mut r_zero = *genuine;
        r_zero[..32].fill(0);
        assert_eq!(
            check_signer(VERIFIER, &domain, &decoded, &r_zero, &address),
            Err(GatewayReject::BadSignature)
        );

        // The audit's check: the registered key over the digest, without the recovery id.
        let digest = decoded.digest(&domain);
        assert_eq!(verify_digest(&public_key(&key), &digest, genuine), Ok(()));
        let key_10 = public_key(&signing_key(1, 10));
        assert_eq!(verify_digest(&key_10, &digest, genuine), Err(GatewayReject::BadSignature));
        assert_eq!(verify_digest(&public_key(&key), &digest, &twin), Err(GatewayReject::HighS));
    }
}
