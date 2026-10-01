//! # gateway
//!
//! The parallel front door (`docs/PIPELINE.md` sections 5 to 7 and 13.4; `docs/DECISIONS.md`
//! D-021, D-022 and D-031). Each gateway thread takes signed client messages from its
//! ingress ring, checks them, verifies their secp256k1 signatures, and forwards the good
//! ones into its lane, from which the sequencer takes them. Signature verification costs
//! about 50 µs, far more than the core's whole budget per command, so it never runs on the
//! core thread: `N` gateways verify in parallel, and the core only ever sees commands that
//! passed.
//!
//! ```text
//!  sender ──ingress[g]──> gateway g ──lane[g]──> sequencer ──> journal, core, gate
//!                         (g = 0..N-1)
//! ```
//!
//! **The message** ([`wire`], 5.1): 136 bytes, a 72-byte signed part (magic `PERP`,
//! version, deployment id, account, nonce, expiry, and the command in its 40-byte encoding)
//! and a 64-byte ECDSA signature `r || s` over its SHA-256. Every field that separates one
//! use of a signature from another (protocol, version, deployment, message type) is inside
//! the signed bytes.
//!
//! **The checks** ([`check`], 7.1), cheapest first, so that anything malformed, replayed,
//! misrouted, expired or unforwardable is refused in nanoseconds, before the one expensive
//! check, the signature: the domain; the header and the command's encoding; client
//! commands only; the account's routing (`account mod N == g`); ownership of the order
//! (the order id names its account); a registered key; the nonce (strictly above the last,
//! by at most 2^32); the expiry; room in the lane (the last 64 slots are kept for
//! cancels); low-S; and the signature.
//!
//! **Nonces** (section 6). Each account has one gateway, so its last nonce lives in exactly
//! one place, with no shared state. A nonce is used up exactly when the message is
//! forwarded, after the signature verified; a rejected message uses none. After a restart
//! the nonces come back from the journal (`pipeline::replay::NonceTable`).
//!
//! **Keys** ([`registry`], 5.4): compressed public keys from `keys.txt`, parsed once at
//! start and partitioned by gateway. The SHA-256 of the file goes into every journal
//! segment's header, and [`audit`] uses it to check every signed command in a journal
//! again, later, against the registry that applied (13.4).
//!
//! **The verifier** ([`verifier`], 5.7): RustCrypto's `k256` by default; Bitcoin Core's
//! libsecp256k1 in a build with the `c-secp256k1` feature, chosen when the registry is
//! loaded. Everything else is the same with either.
//!
//! **A second signing scheme, opt-in** (5.8, D-033): Polymarket Perps' own. The message is
//! version 2 of the same 136 bytes, with a salt and a millisecond timestamp in place of the
//! nonce and the expiry; the client signs an EIP-712 digest of the command ([`eip712`]),
//! and the gateway recovers the signer and compares its address with the account's. A
//! request, `(account, salt, ts, market)`, is accepted once: the gateway keeps every
//! request of the last 5 minutes in a preallocated table ([`salts`]). A gateway checks one
//! scheme, chosen when it is made ([`Gateway::new`], [`Gateway::new_eip712`]), and the
//! journal's header records which (`pipeline::records::AuthScheme`), so the audit checks
//! each journal its own way.
//!
//! **v1 trusts its one client** (7.4, D-031): the in-process load generator. It defends
//! against replays, malleability, cross-deployment and cross-type reuse, spoofed
//! ownership, nonce burning and delayed execution, but not against floods of forged
//! messages; a network gateway must add sessions, per-connection accounting and rate
//! limits first. In the EIP-712 scheme a cancel's or a modify's market is not signed, and
//! the market is part of the request the gateway accepts once ([`salts`]): so whoever holds
//! a signed cancel or modify can have up to one copy per other market id accepted, each of
//! which the engine rejects (`UnknownOrder` or `UnknownMarket`), but can't make the genuine
//! message `ReusedRequest` or reach another order (5.8, attack 7).
//!
//! **Modules.**
//! - [`wire`]: the 136-byte message: layout, encoding, decoding, the signature check (5.1
//!   to 5.3).
//! - [`verifier`]: the two verifiers, `k256` and (optional) libsecp256k1, and the parsed
//!   public key (5.7); and, for the EIP-712 scheme, recovery of a signer's address (D-033).
//! - [`registry`]: `keys.txt`, loaded and written, and partitioned by gateway (5.4).
//! - [`check`]: [`Gateway`], its per-account state, and the check order of each scheme
//!   (7.1, 7.2).
//! - [`salts`]: the EIP-712 scheme's table of recent requests (D-033).
//! - [`thread`]: the gateway loop between its rings, and what it counts (7.3).
//! - [`audit`]: the signature audit of a journal (13.4).
//! - [`core_verifier`]: the core's own signature check, for the insecure "verify on core"
//!   ablation only (section 16).
//! - [`eip712`]: the opt-in second scheme, Polymarket Perps' own: a command's compact form,
//!   its hash, and the EIP-712 digest that is signed (5.8, D-033).
//! - [`keccak`]: keccak-256, Ethereum's hash, our own (D-033).
//! - [`msgpack`]: a MessagePack writer for the forms [`eip712`] signs (D-033).

pub mod audit;
pub mod check;
pub mod core_verifier;
pub mod eip712;
pub mod keccak;
pub mod msgpack;
pub mod registry;
pub mod salts;
pub mod thread;
pub mod verifier;
pub mod wire;

#[cfg(test)]
mod test_support;

pub use check::{Accepted, Gateway, GatewayReject};
pub use registry::KeyRegistry;
pub use thread::{GatewayCounters, GatewayStats, ThreadConfig, spawn};
pub use verifier::{PublicKey, VerifierKind};
pub use wire::{Decoded, decode, encode_signed_part};

use engine::types::AccountId;

/// The gateway, `0..gateways`, that owns `account`: `account mod N` (6.2). The sender
/// routes each message by it, and each gateway holds the keys and nonces of its own
/// accounts only.
pub fn gateway_of(account: AccountId, gateways: usize) -> usize {
    account as usize % gateways
}
