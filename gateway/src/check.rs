//! A gateway's checks on one message, in the order of `docs/PIPELINE.md` 7.1, and the
//! state they need: each of its accounts' key and last used nonce (7.2; section 6;
//! `docs/DECISIONS.md` D-021, D-022); or, in the opt-in EIP-712 scheme (D-033), each
//! account's address and the gateway's table of recent requests.
//!
//! **Contract.** [`Gateway::check`] takes a message, the gateway's clock read when it took
//! the message (as nanoseconds since the UNIX epoch) and the free slots in its lane, and
//! either accepts the message or names the first check it fails. A gateway checks the
//! messages of one signing scheme, chosen when it is made ([`Gateway::new`] for the perp
//! scheme, [`Gateway::new_eip712`]); the other scheme's messages have another version, and
//! are `WrongDomain`. The perp scheme's checks:
//!
//! | # | Check | Reject |
//! |---|---|---|
//! | 1 | magic `PERP`, version 1, deployment id | `WrongDomain` |
//! | 2 | header reserved bytes zero; the CMD40 decodes | `Malformed` |
//! | 3 | the command is a place, a cancel or a modify (tag 1, 2 or 3) | `OperatorOnly` |
//! | 4 | `account mod N == g` | `WrongGateway` |
//! | 5 | `account_of(order_id) == account` | `NotOwner` |
//! | 6 | the account is in this gateway's registry | `UnknownAccount` |
//! | 7 | `nonce > last_nonce[account]` | `StaleNonce` |
//! | 8 | `nonce − last_nonce[account] <= 2^32` | `NonceJump` |
//! | 9 | the clock is not past `expires_at` | `Expired` |
//! | 10 | the lane has room: more than 64 free slots for a place or a modify, 1 for a cancel | `Busy` |
//! | 11 | `s <= floor(n / 2)` | `HighS` |
//! | 12 | the signature verifies with the account's key | `BadSignature` |
//!
//! **Invariant: a nonce is used up exactly when the message is accepted** (6.1, rules 3,
//! 4 and 6). The caller then forwards it into the lane at once, so "accepted" and
//! "forwarded" are the same event. A message rejected for any reason leaves `last_nonce`
//! unchanged, so the same bytes may be sent again; and since the nonce moves only after the
//! signature verified, a forger can't burn an account's nonces.
//!
//! **Why this order.** Everything before check 12 costs nanoseconds; check 12 costs about
//! 50 µs. So a malformed, stale, wrong-gateway, expired or lane-full message never costs a
//! verification, and a replay is refused before verifying (6.5, attack 1). The lane check
//! comes before verifying so that the gateway never verifies what it can't forward, and it
//! can't become false afterwards: the gateway is the lane's only producer, so free space
//! only grows until it writes. What the order does *not* buy: a well-formed forgery with a
//! fresh nonce costs one full verification every time (6.5, attack 10; 7.4).
//!
//! **Cancels get headroom** (C42): the last 64 slots of every lane are kept for cancels, so
//! a market maker can still pull its quotes when the pipeline is backed up.
//!
//! **The EIP-712 scheme** (5.8, D-033; Polymarket Perps' own). Its message is version 2 of
//! the same 136 bytes (`wire.rs`): a salt and a timestamp `ts` in milliseconds where the
//! nonce and the expiry are, and the recovery id in byte 6. The signature is over an
//! EIP-712 digest of the command, the salt and `ts` (`eip712.rs`), and the gateway recovers
//! the signer's key from it rather than verifying with the registered key:
//!
//! | # | Check | Reject |
//! |---|---|---|
//! | 1 | magic `PERP`, version 2, deployment id | `WrongDomain` |
//! | 2 | byte 7 zero, recovery id 0 or 1; the CMD40 decodes | `Malformed` |
//! | 3 | the command is a place, a cancel or a modify | `OperatorOnly` |
//! | 4 | `account mod N == g` | `WrongGateway` |
//! | 5 | `account_of(order_id) == account` | `NotOwner` |
//! | 6 | the account is in this gateway's registry | `UnknownAccount` |
//! | 7 | `ts` at most 5 minutes behind the clock, and at most 60 s ahead of it | `StaleTimestamp`, `FutureTimestamp` |
//! | 8 | the request `(account, salt, ts, market)` was not accepted before | `ReusedRequest` |
//! | 9 | the salt table has room for it | `SaltTableFull` |
//! | 10 | the lane has room (as above) | `Busy` |
//! | 11 | `s <= floor(n / 2)` | `HighS` |
//! | 12 | a key is recovered from the signature over the digest | `BadSignature` |
//! | 13 | that key's address is the account's | `WrongSigner` |
//!
//! Checks 1 to 6 and 10 to 12 are the perp scheme's, on the other layout; 7 to 9 take the
//! place of the nonce and the expiry; 13 is new, because a recovery finds *some* key from
//! almost any signature, and only the address says whose. The order has the same reasons:
//! checks 1 to 11 cost nanoseconds (8 and 9 are one lookup, `salts.rs`); 12 and 13 cost a
//! recovery, about what a verification costs, and three keccak-256 hashes before it.
//!
//! **Invariant: a request is used up exactly when the message is accepted,** as a nonce
//! is: the salt table gets `(account, salt, ts, market)` only after check 13 passed. So a
//! forger can neither burn a client's request nor fill the table, and a message rejected
//! for any reason may be sent again (until its `ts` leaves the window).
//!
//! **Why the market is in the request** (the owner's choice, 2026-09-30; `salts.rs`; 5.8,
//! attack 7; D-033, "Trade-offs"). A cancel's or a modify's market is carried but not
//! signed (`eip712.rs`), so a copy of a genuine cancel or modify with another market passes
//! every check, the signer's included. With the market in the request, that copy is another
//! request: the gateway accepts it, the engine rejects it (`UnknownMarket`, or
//! `UnknownOrder`: an order id's sequence is unique per account across all markets, so the
//! order rests in its own market only), and the genuine message is still accepted, before
//! or after it. A copy with the same market is the same request, `ReusedRequest`. So an
//! attacker holding the signed bytes can make the engine reject up to one junk copy per
//! market id, and can never make the genuine message `ReusedRequest` or reach another
//! order; each copy still costs a recovery and takes a table slot, as any accepted message
//! does. A place's market is signed: a copy with another market is `WrongSigner`.
//!
//! **The timestamp window** (D-033): Polymarket refuses a signature more than 5 minutes
//! old, or more than a minute ahead of its clock. The clock is the one read when the
//! message was taken (`t_gw_in`), in whole milliseconds (the nanoseconds divided by 10^6,
//! rounded down). The window also bounds what the salt table must remember: a request
//! whose `ts` has left it can never pass check 7 again, so its slot may be reused.
//!
//! **State.** Accounts are routed to exactly one gateway (`account mod N`), so each
//! gateway's nonces, or requests, need no shared state. The map is std's `HashMap` with its
//! default, randomly keyed hasher: only registered accounts are ever inserted, so an
//! attacker can't choose keys that collide, and a lookup (about 20 ns) is nothing next to a
//! verification. (Keep it that way: every account on gateway `g` is `≡ g (mod N)`, so an
//! identity-like hasher with a power-of-two `N` would put them all in the same buckets.) In
//! the EIP-712 scheme each account's address is derived once, when the gateway is made,
//! from the key `keys.txt` registers (so the file and its digest are the same for both
//! schemes). The reject counts of 7.2 are kept by the thread loop (`thread.rs`), which
//! knows each message's scheduled time and so which rejects fall in the measured window.
//!
//! **The "verify on core" ablation** (section 16, only through `e2e ablate`, and only in
//! the perp scheme): [`Gateway::without_signature_checks`] skips checks 11 and 12, and the
//! core verifies instead (`core_verifier.rs`). **Insecure**: the nonce is then used up
//! before anything was verified, so a forger could burn an account's nonces. It exists only
//! to measure.
//!
//! **Complexity.** Perp checks 1 to 11: O(1). Check 12: one SHA-256 of 72 bytes and one
//! ECDSA verification. EIP-712 checks 1 to 11: O(1) (check 8 walks a few slots on
//! average). Checks 12 and 13: the MessagePack encoding, three keccak-256 hashes, one
//! recovery, and one more keccak-256 for the address.

use std::collections::HashMap;
use std::fmt;

use engine::command::{Command, PlaceOrder};
use engine::types::{AccountId, MarketId, OrderId, Price, Qty, Side, TimeInForce, account_of};

use pipeline::codec::{COMMAND_WORDS, command_tags};
use pipeline::records::{AuthScheme, SIGNATURE_WORDS, signature_words};
use pipeline::replay::NonceTable;

use crate::eip712::{self, Address, Domain};
use crate::gateway_of;
use crate::registry::KeyRegistry;
use crate::salts::{Request, SaltTable};
use crate::verifier::{GENERATOR, PublicKey, VerifierKind};
use crate::wire::{self, MESSAGE_BYTES, SIGNATURE_BYTES};

/// The largest step up a nonce may take (6.1 rule 2; C41): a client bug that sent
/// `2^64 − 1` would otherwise lock its own account out for good.
pub const MAX_NONCE_JUMP: u64 = 1 << 32;

/// Lane slots kept for cancels: a place or a modify needs more than this many free (7.1).
pub const CANCEL_HEADROOM: usize = 64;

/// The EIP-712 scheme's timestamp window (module docs): `ts` may be at most this many
/// milliseconds behind the gateway's clock, 5 minutes...
pub const MAX_AGE_MS: u64 = 300_000;
/// ...and at most this many ahead of it, 60 s.
pub const MAX_AHEAD_MS: u64 = 60_000;

/// Nanoseconds in a millisecond: the clock is read in nanoseconds, `ts` is in milliseconds.
const NANOS_PER_MILLI: u64 = 1_000_000;

/// Why a gateway rejected a message: the first check of 7.1 it failed. Gateway rejects are
/// counted, never journaled: nothing happened in the exchange. The checks are numbered as
/// in the module docs' tables; where the two schemes differ, the reason names its scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GatewayReject {
    /// Check 1: another protocol, version or deployment. A gateway takes only its own
    /// scheme's version.
    WrongDomain,
    /// Check 2: nonzero reserved bytes, a recovery id other than 0 or 1 (EIP-712), or a
    /// CMD40 that doesn't decode.
    Malformed,
    /// Check 3: a deposit, a withdrawal, a mark or market setup: operator commands only.
    OperatorOnly,
    /// Check 4: the account belongs to another gateway.
    WrongGateway,
    /// Check 5: the order id names another account than the signer.
    NotOwner,
    /// Check 6: the account has no registered key.
    UnknownAccount,
    /// Perp check 7: the nonce is not above the account's last used nonce. It means only
    /// that; it never says that *this* message executed (6.1, 12.4).
    StaleNonce,
    /// Perp check 8: the nonce is more than 2^32 above the last.
    NonceJump,
    /// Perp check 9: the message's expiry has passed.
    Expired,
    /// EIP-712 check 7: `ts` is more than 5 minutes behind the gateway's clock.
    StaleTimestamp,
    /// EIP-712 check 7: `ts` is more than 60 s ahead of the gateway's clock.
    FutureTimestamp,
    /// EIP-712 check 8: this request, `(account, salt, ts, market)`, was accepted before
    /// (Polymarket's `signature_already_used`). Like `StaleNonce`, it never says that this
    /// message executed.
    ReusedRequest,
    /// EIP-712 check 9: the salt table has no room for another request (`salts.rs`): it was
    /// sized for fewer requests than the gateway was sent.
    SaltTableFull,
    /// Check 10: the lane is full (for a place or a modify: 64 or fewer slots free).
    Busy,
    /// Check 11: the high form of `s` (5.3).
    HighS,
    /// Check 12: `r` or `s` out of range, or the signature doesn't verify (perp), or no key
    /// comes out of it (EIP-712).
    BadSignature,
    /// EIP-712 check 13: the key the signature recovers to is not the account's.
    WrongSigner,
}

impl GatewayReject {
    /// How many reasons there are.
    pub const COUNT: usize = 17;

    /// Every reason, in the order of the checks: the perp scheme's checks 7 to 9, then the
    /// EIP-712 scheme's, which take their place.
    pub const ALL: [GatewayReject; GatewayReject::COUNT] = [
        GatewayReject::WrongDomain,
        GatewayReject::Malformed,
        GatewayReject::OperatorOnly,
        GatewayReject::WrongGateway,
        GatewayReject::NotOwner,
        GatewayReject::UnknownAccount,
        GatewayReject::StaleNonce,
        GatewayReject::NonceJump,
        GatewayReject::Expired,
        GatewayReject::StaleTimestamp,
        GatewayReject::FutureTimestamp,
        GatewayReject::ReusedRequest,
        GatewayReject::SaltTableFull,
        GatewayReject::Busy,
        GatewayReject::HighS,
        GatewayReject::BadSignature,
        GatewayReject::WrongSigner,
    ];

    /// The reason's place in [`GatewayReject::ALL`], for arrays of counts.
    pub const fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for GatewayReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// A message that passed every check: the fields the gateway forwards in its lane record
/// (3.3), exactly as the client sent them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accepted {
    /// The signer, who owns the order.
    pub account: AccountId,
    /// The nonce; in the EIP-712 scheme, the salt (it is journaled in the nonce's word).
    pub nonce: u64,
    /// The expiry; in the EIP-712 scheme, the timestamp in milliseconds (in the expiry's
    /// word).
    pub expires_at: u64,
    /// The CMD40, as signed.
    pub command: [u64; COMMAND_WORDS],
    /// `r || s`: message bytes 72..136 as eight words. (The EIP-712 scheme's recovery id is
    /// not forwarded: the audit checks the signature with the account's key, 13.4.)
    pub signature: [u64; SIGNATURE_WORDS],
}

/// One account's gateway state (7.2).
struct AccountState {
    /// Parsed once, at load, for the registry's verifier (5.4).
    key: PublicKey,
    /// Perp scheme: 0 until the account's first message, so the first valid nonce is 1 or
    /// more. Not used in the EIP-712 scheme.
    last_nonce: u64,
    /// EIP-712 scheme: the key's Ethereum address, derived once, when the gateway is made.
    /// Zero in the perp scheme, which doesn't use it.
    address: Address,
}

/// What each scheme keeps besides its accounts' state (module docs).
enum Scheme {
    /// The nonces are in each account's state.
    Perp,
    Eip712 {
        /// The deployment's EIP-712 domain, its separator computed once.
        domain: Domain,
        /// The registry's verifier, which recovers the signers.
        verifier: VerifierKind,
        /// Every request accepted recently (`salts.rs`).
        salts: SaltTable,
    },
}

/// Gateway `g` of `N`: its accounts' keys and nonces, or addresses and requests (module
/// docs).
pub struct Gateway {
    index: usize,
    count: usize,
    deployment: u32,
    /// The run clock's anchor: `start_unix_ns + t_gw_in` is the time the expiry, or the
    /// timestamp window, is checked against (7.1, check 9 or 7).
    start_unix_ns: u64,
    accounts: HashMap<AccountId, AccountState>,
    /// Checks 11 and 12 run: false only in the insecure ablation (module docs).
    verifies_signatures: bool,
    scheme: Scheme,
}

impl Gateway {
    /// Gateway `index` of `count` for `deployment`, in the perp scheme, with the keys of its
    /// accounts from `registry` and their last nonces from `nonces`: empty for a new
    /// journal, the table replayed from the journal after a restart (6.3). `start_unix_ns`
    /// is the run clock's anchor (`RunClock::start_unix_ns`).
    ///
    /// Panics unless `index < count` and the registry is for `deployment`: both are wiring
    /// mistakes in the caller.
    pub fn new(
        index: usize,
        count: usize,
        deployment: u32,
        registry: &KeyRegistry,
        nonces: &NonceTable,
        start_unix_ns: u64,
    ) -> Gateway {
        assert!(index < count, "gateway {index} of {count}");
        assert_eq!(registry.deployment(), deployment, "the key registry is for another deployment");
        let accounts = registry
            .partition(index, count)
            .map(|(account, key)| {
                (account, AccountState { key: *key, last_nonce: nonces.get(account), address: [0; 20] })
            })
            .collect();
        Gateway {
            index,
            count,
            deployment,
            start_unix_ns,
            accounts,
            verifies_signatures: true,
            scheme: Scheme::Perp,
        }
    }

    /// Gateway `index` of `count` for `deployment`, in the EIP-712 scheme (module docs), with
    /// the keys of its accounts from `registry`, each account's address derived from its key
    /// here, once, and `salts`, a new, empty table, which remembers the requests it accepts.
    /// Make the table with `SaltTable::new` just before this, on the run's main thread and
    /// before the timed flow, since that allocates it and touches every page of it; size it
    /// for every request the gateway may accept in the run (`salts.rs`, "Full").
    /// `start_unix_ns` is the run clock's anchor.
    ///
    /// Panics unless `index < count`, the registry is for `deployment`, and `salts` is
    /// empty: all wiring mistakes in the caller.
    pub fn new_eip712(
        index: usize,
        count: usize,
        deployment: u32,
        registry: &KeyRegistry,
        salts: SaltTable,
        start_unix_ns: u64,
    ) -> Gateway {
        assert!(index < count, "gateway {index} of {count}");
        assert_eq!(registry.deployment(), deployment, "the key registry is for another deployment");
        assert!(salts.is_empty(), "a gateway starts with a new salt table");
        let accounts = registry
            .partition(index, count)
            .map(|(account, key)| {
                (account, AccountState { key: *key, last_nonce: 0, address: key.address() })
            })
            .collect();
        let scheme = Scheme::Eip712 {
            domain: Domain::new(u64::from(deployment)),
            verifier: registry.verifier(),
            salts,
        };
        Gateway { index, count, deployment, start_unix_ns, accounts, verifies_signatures: true, scheme }
    }

    /// **Insecure**: this gateway skips checks 11 and 12, and uses up each nonce without
    /// verifying anything. Only for the "verify on core" ablation, where the core verifies
    /// instead (module docs; section 16). Panics for a gateway of the EIP-712 scheme: the
    /// core's verifier checks perp signatures only (`Pipeline::start` refuses the ablation
    /// with that scheme too).
    pub fn without_signature_checks(self) -> Gateway {
        assert!(
            matches!(self.scheme, Scheme::Perp),
            "the verify-on-core ablation is for the perp scheme only"
        );
        Gateway { verifies_signatures: false, ..self }
    }

    /// `g`.
    pub fn index(&self) -> usize {
        self.index
    }

    /// The signing scheme this gateway checks.
    pub fn scheme(&self) -> AuthScheme {
        match self.scheme {
            Scheme::Perp => AuthScheme::Perp,
            Scheme::Eip712 { .. } => AuthScheme::Eip712,
        }
    }

    /// The run clock's anchor this gateway checks expiries, or timestamps, against.
    pub fn start_unix_ns(&self) -> u64 {
        self.start_unix_ns
    }

    /// Accounts routed here that have a key.
    pub fn accounts(&self) -> usize {
        self.accounts.len()
    }

    /// The last nonce `account` used, if it is one of this gateway's accounts (always 0 in
    /// the EIP-712 scheme, which has no nonces).
    pub fn last_nonce(&self, account: AccountId) -> Option<u64> {
        self.accounts.get(&account).map(|state| state.last_nonce)
    }

    /// The salt table, in the EIP-712 scheme.
    pub fn salt_table(&self) -> Option<&SaltTable> {
        match &self.scheme {
            Scheme::Perp => None,
            Scheme::Eip712 { salts, .. } => Some(salts),
        }
    }

    /// Runs the EIP-712 scheme's expensive checks once, on the calling thread, on a
    /// signature nobody sent, before the timed flow: the gateway thread calls it at start,
    /// after `verifier::prepare_this_thread`. Their code and the stack they use are then
    /// mapped, so that the first real message doesn't take page faults inside the measured
    /// window (15.4). Does nothing in the perp scheme.
    pub fn prepare_this_thread(&self) {
        if let Scheme::Eip712 { domain, verifier, .. } = &self.scheme {
            let command = Command::PlaceOrder(PlaceOrder {
                order_id: OrderId::new(1),
                price: Price::new(1),
                qty: Qty::new(1),
                market: MarketId::new(0),
                side: Side::Buy,
                tif: TimeInForce::Gtc,
                post_only: false,
            });
            let data = eip712::op_data(&command).expect("a place has a form");
            let digest = eip712::digest(domain, &data, 0, 0);
            // r = the x of the generator point G (below n, and a point's x, so the whole
            // recovery runs), s = 1. Whatever key comes out is not used.
            let mut signature = [0u8; SIGNATURE_BYTES];
            signature[..32].copy_from_slice(&GENERATOR[1..]); // after the tag
            signature[63] = 1;
            std::hint::black_box(verifier.recover(&digest, &signature, 0));
        }
    }

    /// The checks of the gateway's scheme (module docs), in order, on `message`, taken when
    /// the gateway's clock read `now_unix_ns`, with `lane_free` free slots in its lane. On
    /// success the account's nonce, or the request, is used up, and the caller must forward
    /// the message.
    pub fn check(
        &mut self,
        message: &[u8; MESSAGE_BYTES],
        now_unix_ns: u64,
        lane_free: usize,
    ) -> Result<Accepted, GatewayReject> {
        match self.scheme {
            Scheme::Perp => self.check_perp(message, now_unix_ns, lane_free),
            Scheme::Eip712 { .. } => self.check_eip712(message, now_unix_ns, lane_free),
        }
    }

    /// The perp scheme's checks 1 to 12 (module docs).
    fn check_perp(
        &mut self,
        message: &[u8; MESSAGE_BYTES],
        now_unix_ns: u64,
        lane_free: usize,
    ) -> Result<Accepted, GatewayReject> {
        // 1. The domain. Magic and version are in `decode`; all three are `WrongDomain`, so
        //    comparing the deployment first keeps 7.1's order.
        if wire::deployment_of(message) != self.deployment {
            return Err(GatewayReject::WrongDomain);
        }
        let decoded = wire::decode(message)?; // 1 (magic, version), 2 and 3
        // 4. Routing: one account, one gateway (6.2).
        if gateway_of(decoded.account, self.count) != self.index {
            return Err(GatewayReject::WrongGateway);
        }
        // 5. Ownership (5.5): only the holder of A's key may act on an order whose id says A.
        if account_of(decoded.order_id()) != decoded.account {
            return Err(GatewayReject::NotOwner);
        }
        // 6. The account's key and nonce.
        let state = self.accounts.get_mut(&decoded.account).ok_or(GatewayReject::UnknownAccount)?;
        // 7 and 8.
        check_nonce(decoded.nonce, state.last_nonce)?;
        // 9. The expiry, against the clock read when the message was taken.
        if now_unix_ns > decoded.expires_at {
            return Err(GatewayReject::Expired);
        }
        // 10. Room in the lane, before spending a verification.
        if !lane_has_room(wire::command_tag(message), lane_free) {
            return Err(GatewayReject::Busy);
        }
        // 11 and 12, unless the core verifies instead (the insecure ablation).
        if self.verifies_signatures {
            wire::verify_signature(&state.key, wire::signed_part(message), wire::signature(message))?;
        }
        // Accepted: the nonce is used up now, after the signature verified (6.1).
        state.last_nonce = decoded.nonce;
        Ok(Accepted {
            account: decoded.account,
            nonce: decoded.nonce,
            expires_at: decoded.expires_at,
            command: wire::command_words(message),
            signature: signature_words(message),
        })
    }

    /// The EIP-712 scheme's checks 1 to 13 (module docs).
    fn check_eip712(
        &mut self,
        message: &[u8; MESSAGE_BYTES],
        now_unix_ns: u64,
        lane_free: usize,
    ) -> Result<Accepted, GatewayReject> {
        let Scheme::Eip712 { domain, verifier, salts } = &mut self.scheme else {
            unreachable!("`check` calls this for the EIP-712 scheme only")
        };
        // 1. The domain: the deployment first, as in the perp scheme, then magic and
        //    version 2 in `decode_eip712`.
        if wire::deployment_of(message) != self.deployment {
            return Err(GatewayReject::WrongDomain);
        }
        let decoded = wire::decode_eip712(message)?; // 1 (magic, version), 2 and 3
        // 4. Routing: one account, one gateway (6.2).
        if gateway_of(decoded.account, self.count) != self.index {
            return Err(GatewayReject::WrongGateway);
        }
        // 5. Ownership (5.5): the order id's account is the account whose address must sign.
        if account_of(decoded.order_id()) != decoded.account {
            return Err(GatewayReject::NotOwner);
        }
        // 6. The account's address.
        let state = self.accounts.get(&decoded.account).ok_or(GatewayReject::UnknownAccount)?;
        // 7. The timestamp window, against the clock read when the message was taken.
        let now_ms = now_unix_ns / NANOS_PER_MILLI;
        check_timestamp(decoded.ts_ms, now_ms)?;
        // 8 and 9. A request never accepted before, and room to remember it: a lookup only.
        //    The request includes the command's market (module docs).
        let request = Request::of(decoded.account, decoded.salt, decoded.ts_ms, &decoded.command)
            .expect("decode_eip712 admits only places, cancels and modifies");
        let vacancy = salts.find(request, now_ms)?;
        // 10. Room in the lane, before spending a recovery.
        if !lane_has_room(wire::command_tag(message), lane_free) {
            return Err(GatewayReject::Busy);
        }
        // 11 to 13: low-S, then the recovery, then the address.
        wire::check_signer(*verifier, domain, &decoded, wire::signature(message), &state.address)?;
        // Accepted: the request is used up now, after the signer was checked (module docs).
        salts.insert(vacancy);
        Ok(Accepted {
            account: decoded.account,
            nonce: decoded.salt,
            expires_at: decoded.ts_ms,
            command: wire::command_words(message),
            signature: signature_words(message),
        })
    }
}

impl fmt::Debug for Gateway {
    /// Leaves out the keys, nonces and requests: thousands of each.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gateway")
            .field("index", &self.index)
            .field("count", &self.count)
            .field("deployment", &self.deployment)
            .field("scheme", &self.scheme())
            .field("start_unix_ns", &self.start_unix_ns)
            .field("accounts", &self.accounts.len())
            .field("verifies_signatures", &self.verifies_signatures)
            .field("salt_table", &self.salt_table())
            .finish()
    }
}

/// Checks 7 and 8: strictly above the last nonce (gaps allowed), by at most 2^32.
fn check_nonce(nonce: u64, last_nonce: u64) -> Result<(), GatewayReject> {
    if nonce <= last_nonce {
        Err(GatewayReject::StaleNonce)
    } else if nonce - last_nonce > MAX_NONCE_JUMP {
        Err(GatewayReject::NonceJump)
    } else {
        Ok(())
    }
}

/// EIP-712 check 7: `ts_ms` at most 5 minutes behind `now_ms`, and at most 60 s ahead.
fn check_timestamp(ts_ms: u64, now_ms: u64) -> Result<(), GatewayReject> {
    if is_too_old(ts_ms, now_ms) {
        Err(GatewayReject::StaleTimestamp)
    } else if ts_ms.saturating_sub(now_ms) > MAX_AHEAD_MS {
        Err(GatewayReject::FutureTimestamp)
    } else {
        Ok(())
    }
}

/// True if `ts_ms` is more than 5 minutes behind `now_ms`: check 7 refuses the message, and
/// the salt table may forget a request with that timestamp (`salts.rs`).
pub(crate) fn is_too_old(ts_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(ts_ms) > MAX_AGE_MS
}

/// Check 10: a cancel needs one free slot; a place or a modify needs more than the
/// headroom kept for cancels.
fn lane_has_room(tag: u8, lane_free: usize) -> bool {
    if tag == command_tags::CANCEL_ORDER { lane_free >= 1 } else { lane_free > CANCEL_HEADROOM }
}

#[cfg(test)]
mod tests {
    //! The unit tests of PIPELINE.md 18.1 for `Gateway::check`, the attacks of 6.5, and the
    //! property test of 18.2 against a small model.
    use super::*;
    use crate::test_support::{
        XorShift, acct, cancel, high_s_twin, message, modify, place, registry, signing_key,
    };
    use crate::wire::{
        EXPIRY_OFFSET, ORDER, R_OFFSET, RESERVED_OFFSET, S_OFFSET, SIGNED_BYTES, VERSION_OFFSET, assemble,
        encode_signed_part,
    };
    use engine::command::{Command, Deposit, SetMark};
    use engine::engine::FUND;
    use engine::types::{Micros, OrderSeq, order_id};
    use k256::ecdsa::SigningKey;

    const DEPLOYMENT: u32 = 1;
    const SEED: u64 = 1;
    /// The spec's attacks use 8 gateways: account 9 is on gateway 1, account 7 on gateway 7.
    const N: usize = 8;
    /// A clock far from every expiry the tests use, in nanoseconds since the UNIX epoch.
    const NOW: u64 = 1_790_000_000_000_000_000;
    /// A lane with plenty of room.
    const ROOM: usize = 1_024;

    /// Gateway `g` of 8 for deployment 1, with the keys of accounts 1 to 40 (seed 1) and
    /// `nonces`.
    fn new_gateway(g: usize, nonces: &NonceTable) -> Gateway {
        Gateway::new(g, N, DEPLOYMENT, &registry(SEED, DEPLOYMENT, (1..=40).map(acct)), nonces, 0)
    }

    /// Gateway 1, with account 9's last nonce at 41 (the setup of 6.5).
    fn gateway_1() -> Gateway {
        let mut nonces = NonceTable::new();
        nonces.note(acct(9), 41);
        new_gateway(1, &nonces)
    }

    fn key(account: AccountId) -> SigningKey {
        signing_key(SEED, account)
    }

    /// Account `account`'s own message: `command`, `nonce`, never expiring.
    fn signed(account: AccountId, nonce: u64, command: &Command) -> [u8; MESSAGE_BYTES] {
        message(&key(account), DEPLOYMENT, account, nonce, u64::MAX, command)
    }

    #[test]
    fn a_valid_message_is_accepted_and_uses_up_its_nonce() {
        let mut gateway = gateway_1();
        let msg = signed(acct(9), 42, &place(acct(9), 17));
        let accepted = gateway.check(&msg, NOW, ROOM).expect("accepted");
        assert_eq!((accepted.account, accepted.nonce, accepted.expires_at), (acct(9), 42, u64::MAX));
        assert_eq!(accepted.command, pipeline::codec::encode_command(&place(acct(9), 17)));
        assert_eq!(accepted.signature, signature_words(&msg));
        assert_eq!(gateway.last_nonce(acct(9)), Some(42));
        assert_eq!(gateway.accounts(), 5, "accounts 1, 9, 17, 25 and 33");
    }

    #[test]
    fn without_signature_checks_a_forgery_is_forwarded_but_every_cheap_check_still_runs() {
        // The insecure "verify on core" ablation (section 16): checks 11 and 12 are the
        // core's job there.
        let mut gateway = gateway_1().without_signature_checks();
        let forged = message(&key(acct(17)), DEPLOYMENT, acct(9), 42, u64::MAX, &place(acct(9), 18));
        assert!(gateway.check(&forged, NOW, ROOM).is_ok(), "signed by account 17's key, yet forwarded");
        assert_eq!(gateway.last_nonce(acct(9)), Some(42), "the nonce is used up without any verification");
        assert_eq!(gateway.check(&forged, NOW, ROOM), Err(GatewayReject::StaleNonce));
        let busy = signed(acct(9), 43, &place(acct(9), 19));
        assert_eq!(gateway.check(&busy, NOW, CANCEL_HEADROOM), Err(GatewayReject::Busy));
        assert_eq!(gateway_1().check(&forged, NOW, ROOM), Err(GatewayReject::BadSignature));
    }

    #[test]
    fn a_foreign_domain_is_wrong_domain() {
        let mut gateway = gateway_1();
        let good = signed(acct(9), 42, &place(acct(9), 17));
        let mut magic = good;
        magic[0] = b'X';
        let mut version = good;
        version[VERSION_OFFSET] = 2;
        let deployment_2 = message(&key(acct(9)), 2, acct(9), 42, u64::MAX, &place(acct(9), 17));
        for msg in [magic, version, deployment_2] {
            assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::WrongDomain));
        }
        // The deployment is compared first: it wins over a malformed header too (7.1).
        let mut both = deployment_2;
        both[RESERVED_OFFSET] = 1;
        assert_eq!(gateway.check(&both, NOW, ROOM), Err(GatewayReject::WrongDomain));
        assert_eq!(gateway.last_nonce(acct(9)), Some(41));
    }

    #[test]
    fn nonzero_reserved_bytes_are_malformed() {
        let mut gateway = gateway_1();
        for offset in [RESERVED_OFFSET, RESERVED_OFFSET + 1] {
            let mut msg = signed(acct(9), 42, &place(acct(9), 17));
            msg[offset] = 1;
            assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::Malformed));
        }
    }

    #[test]
    fn operator_commands_are_operator_only_whatever_the_signature() {
        let mut gateway = gateway_1();
        let operator_commands = [
            Command::Deposit(Deposit { amount: Micros::new(1_000_000), account: acct(9) }),
            Command::Withdraw(engine::command::Withdraw { amount: Micros::new(1), account: acct(9) }),
            Command::SetLeverage(engine::command::SetLeverage {
                account: acct(9),
                market: MarketId::new(3),
                leverage: 5,
            }),
            Command::SetMark(SetMark { price: Price::new(103_000), market: MarketId::new(3) }),
            Command::SetMarketParams(engine::command::SetMarketParams {
                min_price: Price::new(1),
                max_price: Price::new(2),
                maker_fee_ppm: 0,
                taker_fee_ppm: 0,
                price_band_ppm: 1,
                market: MarketId::new(3),
                max_leverage: 1,
            }),
            Command::SetRiskTier(engine::command::SetRiskTier {
                lower_bound: Micros::ZERO,
                market: MarketId::new(3),
                max_leverage: 1,
                index: 0,
                count: 1,
            }),
        ];
        for (i, command) in operator_commands.iter().enumerate() {
            assert_eq!(pipeline::codec::command_tag(command), 4 + i as u8, "tags 4 to 9");
            let msg = signed(acct(9), 42, command);
            assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::OperatorOnly));
        }
    }

    #[test]
    fn another_gateways_account_is_wrong_gateway() {
        let mut gateway = gateway_1();
        let msg = signed(acct(10), 1, &place(acct(10), 1));
        assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::WrongGateway));
    }

    #[test]
    fn an_order_of_another_account_is_not_owner() {
        let mut gateway = gateway_1();
        // Account 9 signs a cancel of account 17's order (both on gateway 1).
        let msg = signed(acct(9), 42, &cancel(acct(17), 3));
        assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::NotOwner));
    }

    #[test]
    fn an_unregistered_account_and_the_fund_are_unknown() {
        let mut gateway = gateway_1();
        let stranger = SigningKey::from_slice(&[3; 32]).expect("a valid scalar");
        let msg = message(&stranger, DEPLOYMENT, acct(41), 1, u64::MAX, &place(acct(41), 1));
        assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::UnknownAccount));
        // FUND = 2^32 − 1 routes to gateway 7 of 8; the registry never holds it.
        let mut gateway_7 = new_gateway(7, &NonceTable::new());
        let msg = message(&stranger, DEPLOYMENT, FUND, 1, u64::MAX, &place(FUND, 1));
        assert_eq!(gateway_7.check(&msg, NOW, ROOM), Err(GatewayReject::UnknownAccount));
    }

    #[test]
    fn a_nonce_equal_to_or_below_the_last_is_stale() {
        let mut gateway = gateway_1();
        for nonce in [41, 40, 1, 0] {
            let msg = signed(acct(9), nonce, &place(acct(9), 17));
            assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::StaleNonce), "nonce {nonce}");
        }
    }

    #[test]
    fn a_nonce_more_than_2_to_the_32_above_the_last_is_a_jump() {
        let mut gateway = gateway_1();
        let msg = signed(acct(9), 41 + MAX_NONCE_JUMP + 1, &place(acct(9), 17));
        assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::NonceJump));
        let msg = signed(acct(9), 41 + MAX_NONCE_JUMP, &place(acct(9), 17));
        assert!(gateway.check(&msg, NOW, ROOM).is_ok(), "exactly 2^32 above is accepted");
        assert_eq!(gateway.last_nonce(acct(9)), Some(41 + MAX_NONCE_JUMP));
    }

    #[test]
    fn a_message_is_expired_one_nanosecond_after_its_expiry() {
        let mut gateway = gateway_1();
        let msg = message(&key(acct(9)), DEPLOYMENT, acct(9), 42, NOW - 1, &place(acct(9), 17));
        assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::Expired));
        let msg = message(&key(acct(9)), DEPLOYMENT, acct(9), 42, NOW, &place(acct(9), 17));
        assert!(gateway.check(&msg, NOW, ROOM).is_ok(), "equal is accepted");
    }

    #[test]
    fn places_and_modifies_leave_the_last_64_lane_slots_to_cancels() {
        let mut gateway = gateway_1();
        let place_msg = signed(acct(9), 42, &place(acct(9), 17));
        let modify_msg = signed(acct(9), 42, &modify(acct(9), 17));
        let cancel_msg = signed(acct(9), 42, &cancel(acct(9), 17));
        assert_eq!(gateway.check(&place_msg, NOW, CANCEL_HEADROOM), Err(GatewayReject::Busy));
        assert_eq!(gateway.check(&modify_msg, NOW, CANCEL_HEADROOM), Err(GatewayReject::Busy));
        assert_eq!(gateway.check(&cancel_msg, NOW, 0), Err(GatewayReject::Busy));
        assert_eq!(gateway.last_nonce(acct(9)), Some(41), "Busy uses no nonce");
        assert!(gateway.check(&cancel_msg, NOW, 1).is_ok(), "one free slot is enough for a cancel");
        let place_msg = signed(acct(9), 43, &place(acct(9), 18));
        assert!(gateway.check(&place_msg, NOW, CANCEL_HEADROOM + 1).is_ok());
    }

    #[test]
    fn the_high_s_twin_is_refused_before_the_verifier_is_called() {
        let mut gateway = gateway_1();
        let msg = signed(acct(9), 42, &place(acct(9), 17));
        let twin = assemble(wire::signed_part(&msg), &high_s_twin(wire::signature(&msg)));
        assert_eq!(gateway.check(&twin, NOW, ROOM), Err(GatewayReject::HighS));
        // An `s` above n is also high: our comparison refuses it before the verifier's range
        // check.
        let mut huge_s = msg;
        huge_s[S_OFFSET..].fill(0xFF);
        assert_eq!(gateway.check(&huge_s, NOW, ROOM), Err(GatewayReject::HighS));
        assert!(gateway.check(&msg, NOW, ROOM).is_ok(), "the low form is accepted");
    }

    #[test]
    fn r_or_s_out_of_range_is_a_bad_signature() {
        let mut gateway = gateway_1();
        let msg = signed(acct(9), 42, &place(acct(9), 17));
        let mut r_zero = msg;
        r_zero[R_OFFSET..S_OFFSET].fill(0);
        let mut s_zero = msg;
        s_zero[S_OFFSET..].fill(0);
        let mut r_is_n = msg;
        r_is_n[R_OFFSET..S_OFFSET].copy_from_slice(&ORDER);
        for bad in [r_zero, s_zero, r_is_n] {
            assert_eq!(gateway.check(&bad, NOW, ROOM), Err(GatewayReject::BadSignature));
        }
        assert_eq!(gateway.last_nonce(acct(9)), Some(41));
    }

    #[test]
    fn a_signature_by_another_key_or_over_edited_bytes_is_bad() {
        let mut gateway = gateway_1();
        // Signed with account 17's key, claiming account 9 (a forgery, 6.5 attack 4).
        let by_17 = message(&key(acct(17)), DEPLOYMENT, acct(9), 42, u64::MAX, &cancel(acct(9), 17));
        assert_eq!(gateway.check(&by_17, NOW, ROOM), Err(GatewayReject::BadSignature));
        // Signed for deployment 2, then its field edited to 1 (6.5 attack 5).
        let mut edited = message(&key(acct(9)), 2, acct(9), 42, u64::MAX, &place(acct(9), 17));
        edited[wire::DEPLOYMENT_OFFSET] = 1;
        assert_eq!(gateway.check(&edited, NOW, ROOM), Err(GatewayReject::BadSignature));
        // An expiry byte flipped after signing.
        let mut later = signed(acct(9), 42, &place(acct(9), 17));
        later[EXPIRY_OFFSET] ^= 1;
        assert_eq!(gateway.check(&later, NOW, ROOM), Err(GatewayReject::BadSignature));
        assert_eq!(gateway.last_nonce(acct(9)), Some(41), "a forgery uses no nonce (6.1 rule 6)");
        assert!(gateway.check(&signed(acct(9), 42, &place(acct(9), 17)), NOW, ROOM).is_ok());
    }

    #[test]
    fn busy_and_expired_messages_can_be_sent_again() {
        let mut gateway = gateway_1();
        let msg = message(&key(acct(9)), DEPLOYMENT, acct(9), 42, NOW + 10, &place(acct(9), 17));
        assert_eq!(gateway.check(&msg, NOW, 0), Err(GatewayReject::Busy));
        assert_eq!(gateway.check(&msg, NOW, 10), Err(GatewayReject::Busy));
        assert!(gateway.check(&msg, NOW + 10, ROOM).is_ok(), "the same bytes, once there is room");
        let late = message(&key(acct(9)), DEPLOYMENT, acct(9), 43, NOW, &place(acct(9), 18));
        assert_eq!(gateway.check(&late, NOW + 1, ROOM), Err(GatewayReject::Expired));
        assert_eq!(gateway.last_nonce(acct(9)), Some(42));
        // With a clock that has not passed its expiry (a gateway whose clock lags, say),
        // the same bytes are still good: the expiry, not the rejection, ends a message.
        assert!(gateway.check(&late, NOW, ROOM).is_ok());
    }

    #[test]
    fn gaps_are_allowed_and_everything_at_or_below_the_last_is_stale() {
        let mut gateway = new_gateway(1, &NonceTable::new());
        assert!(gateway.check(&signed(acct(9), 5, &place(acct(9), 1)), NOW, ROOM).is_ok());
        assert_eq!(
            gateway.check(&signed(acct(9), 5, &place(acct(9), 2)), NOW, ROOM),
            Err(GatewayReject::StaleNonce)
        );
        assert_eq!(
            gateway.check(&signed(acct(9), 3, &place(acct(9), 2)), NOW, ROOM),
            Err(GatewayReject::StaleNonce)
        );
        assert!(gateway.check(&signed(acct(9), 7, &place(acct(9), 2)), NOW, ROOM).is_ok());
    }

    #[test]
    fn the_nonce_table_carries_over_a_restart_and_other_gateways_accounts_are_left_out() {
        let mut nonces = NonceTable::new();
        nonces.note(acct(9), 41);
        nonces.note(acct(10), 3); // gateway 2's
        let gateway = new_gateway(1, &nonces);
        assert_eq!(gateway.last_nonce(acct(9)), Some(41));
        assert_eq!(gateway.last_nonce(acct(17)), Some(0), "never used a nonce");
        assert_eq!(gateway.last_nonce(acct(10)), None, "not this gateway's account");
    }

    // -----------------------------------------------------------------------------------
    // The attacks of 6.5 (setup: N = 8, account 9 on gateway 1 with last nonce 41).

    #[test]
    fn attack_1_replaying_an_accepted_message_is_stale_before_verifying() {
        let mut gateway = new_gateway(1, &NonceTable::new());
        let msg = signed(acct(9), 41, &place(acct(9), 16));
        assert!(gateway.check(&msg, NOW, ROOM).is_ok());
        assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::StaleNonce));
        // Before verifying: the same replay with its signature destroyed gets the same answer.
        let mut broken = msg;
        broken[R_OFFSET] ^= 1;
        assert_eq!(gateway.check(&broken, NOW, ROOM), Err(GatewayReject::StaleNonce));
    }

    #[test]
    fn attack_2_replaying_a_place_the_engine_rejected_is_stale() {
        // Nonce 42 was forwarded (the engine then rejected the place, InsufficientMargin):
        // forwarding alone used the nonce up.
        let mut gateway = gateway_1();
        let msg = signed(acct(9), 42, &place(acct(9), 17));
        assert!(gateway.check(&msg, NOW, ROOM).is_ok());
        assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::StaleNonce));
    }

    #[test]
    fn attack_3_a_place_delivered_after_its_own_cancel_is_stale() {
        let mut nonces = NonceTable::new();
        nonces.note(acct(9), 49);
        let mut gateway = new_gateway(1, &nonces);
        let place_50 = signed(acct(9), 50, &place(acct(9), 20));
        let cancel_51 = signed(acct(9), 51, &cancel(acct(9), 20));
        assert!(gateway.check(&cancel_51, NOW, ROOM).is_ok(), "51 > 49");
        assert_eq!(gateway.check(&place_50, NOW, ROOM), Err(GatewayReject::StaleNonce));
    }

    #[test]
    fn attack_4_cancelling_someone_elses_order() {
        // Account 7 (gateway 7) cancels order_id(9, 17) as itself: NotOwner.
        let mut gateway_7 = new_gateway(7, &NonceTable::new());
        let as_7 = signed(acct(7), 1, &cancel(acct(9), 17));
        assert_eq!(gateway_7.check(&as_7, NOW, ROOM), Err(GatewayReject::NotOwner));
        // Writing account = 9 routes it to account 9's gateway, which uses 9's key.
        let mut gateway_1 = gateway_1();
        let as_9 = message(&key(acct(7)), DEPLOYMENT, acct(9), 42, u64::MAX, &cancel(acct(9), 17));
        assert_eq!(gateway_1.check(&as_9, NOW, ROOM), Err(GatewayReject::BadSignature));
    }

    #[test]
    fn attack_5_a_message_signed_for_another_deployment() {
        let mut gateway = gateway_1();
        let staging = message(&key(acct(9)), 2, acct(9), 42, u64::MAX, &place(acct(9), 17));
        assert_eq!(gateway.check(&staging, NOW, ROOM), Err(GatewayReject::WrongDomain));
        let mut edited = staging;
        edited[wire::DEPLOYMENT_OFFSET..wire::ACCOUNT_OFFSET].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(gateway.check(&edited, NOW, ROOM), Err(GatewayReject::BadSignature));
    }

    #[test]
    fn attack_6_changing_the_message_type() {
        let mut gateway = gateway_1();
        // A signed cancel whose tag byte (byte 32) is turned into a place's: the place's
        // price and quantity are zero, which decodes, but the tag is signed.
        let cancel_msg = signed(acct(9), 42, &cancel(acct(9), 17));
        let mut as_place = cancel_msg;
        as_place[wire::COMMAND_OFFSET] = command_tags::PLACE_ORDER;
        assert_eq!(gateway.check(&as_place, NOW, ROOM), Err(GatewayReject::BadSignature));
        // A validly signed deposit is refused whatever its signature.
        let deposit = signed(
            acct(9),
            42,
            &Command::Deposit(Deposit { amount: Micros::new(1 << 40), account: acct(9) }),
        );
        assert_eq!(gateway.check(&deposit, NOW, ROOM), Err(GatewayReject::OperatorOnly));
    }

    #[test]
    fn attack_7_the_high_s_twin_of_a_message() {
        let mut gateway = gateway_1();
        let msg = signed(acct(9), 42, &place(acct(9), 17));
        let twin = assemble(wire::signed_part(&msg), &high_s_twin(wire::signature(&msg)));
        assert_eq!(gateway.check(&twin, NOW, ROOM), Err(GatewayReject::HighS), "original not yet sent");
        assert!(gateway.check(&msg, NOW, ROOM).is_ok());
        assert_eq!(gateway.check(&twin, NOW, ROOM), Err(GatewayReject::StaleNonce), "original accepted");
    }

    #[test]
    fn attack_8_nonce_burning_by_a_forger() {
        let mut gateway = gateway_1();
        let forger = SigningKey::from_slice(&[5; 32]).expect("a valid scalar");
        let max = message(&forger, DEPLOYMENT, acct(9), u64::MAX, u64::MAX, &place(acct(9), 17));
        assert_eq!(gateway.check(&max, NOW, ROOM), Err(GatewayReject::NonceJump));
        let next = message(&forger, DEPLOYMENT, acct(9), 42, u64::MAX, &place(acct(9), 17));
        assert_eq!(gateway.check(&next, NOW, ROOM), Err(GatewayReject::BadSignature));
        assert_eq!(gateway.last_nonce(acct(9)), Some(41));
        assert!(
            gateway.check(&signed(acct(9), 42, &place(acct(9), 17)), NOW, ROOM).is_ok(),
            "the genuine one"
        );
    }

    #[test]
    fn attack_9_an_abandoned_message_sent_the_next_day_is_expired() {
        let taker = acct(1_500); // routes to gateway 1_500 mod 8 = 4
        let keys = registry(SEED, DEPLOYMENT, [taker]);
        let mut nonces = NonceTable::new();
        nonces.note(taker, 6);
        let mut gateway = Gateway::new(4, N, DEPLOYMENT, &keys, &nonces, 0);
        let ioc = Command::PlaceOrder(engine::command::PlaceOrder {
            order_id: order_id(taker, OrderSeq::new(30)),
            price: Price::new(103_010),
            qty: Qty::new(20_000),
            market: MarketId::new(3),
            side: engine::types::Side::Buy,
            tif: engine::types::TimeInForce::Ioc,
            post_only: false,
        });
        let five_seconds = 5_000_000_000;
        let msg = message(&signing_key(SEED, taker), DEPLOYMENT, taker, 7, NOW + five_seconds, &ioc);
        assert_eq!(gateway.check(&msg, NOW, 0), Err(GatewayReject::Busy), "the user gives up");
        let next_day = NOW + 86_400_000_000_000;
        assert_eq!(gateway.check(&msg, next_day, ROOM), Err(GatewayReject::Expired));
        assert_eq!(gateway.last_nonce(taker), Some(6));
    }

    #[test]
    fn attack_10_a_forgery_with_a_fresh_nonce_can_be_sent_again() {
        // v1 does not defend against this (7.4): each copy costs a verification.
        let mut gateway = gateway_1();
        let mut forged = signed(acct(9), 42, &place(acct(9), 17));
        forged[R_OFFSET + 5] ^= 0x40;
        for _ in 0..3 {
            assert_eq!(gateway.check(&forged, NOW, ROOM), Err(GatewayReject::BadSignature));
        }
        assert_eq!(gateway.last_nonce(acct(9)), Some(41));
    }

    // -----------------------------------------------------------------------------------
    // The property test of 18.2.

    /// One random message of the property test, as the model sees it.
    #[derive(Clone, Copy, Debug)]
    struct Spec {
        nonce: u64,
        expires_at: u64,
        tag: u8,
        forged: bool,
    }

    /// The 10-line model of 6.1 and 7.1 for well-formed messages of this gateway's
    /// accounts: forwarded or not, and the account's last nonce.
    fn model(spec: &Spec, last: &mut u64, now: u64, lane_free: usize) -> bool {
        let fresh = spec.nonce > *last && spec.nonce - *last <= MAX_NONCE_JUMP;
        let alive = now <= spec.expires_at;
        let room = if spec.tag == command_tags::CANCEL_ORDER { lane_free >= 1 } else { lane_free > 64 };
        let forwarded = fresh && alive && room && !spec.forged;
        if forwarded {
            *last = spec.nonce;
        }
        forwarded
    }

    #[test]
    fn random_messages_are_forwarded_exactly_as_the_model_says() {
        let accounts = [1, 9, 17].map(acct); // all on gateway 1 of 8
        let keys = accounts.map(key);
        let mut gateway = new_gateway(1, &NonceTable::new());
        let mut model_nonces = [0u64; 3];
        let forger = SigningKey::from_slice(&[9; 32]).expect("a valid scalar");
        let mut random = XorShift(0x5EED_6A7E);
        let mut forwarded = (0, 0);
        for step in 0..1_500 {
            let which = random.below(3) as usize;
            let account = accounts[which];
            let last = model_nonces[which];
            // Mostly near the last nonce (stale, next, small gaps), sometimes a huge jump.
            let nonce = match random.below(10) {
                0 => last + MAX_NONCE_JUMP + random.below(3),
                1..=3 => last.saturating_sub(random.below(3)),
                _ => last + 1 + random.below(4),
            };
            let now = NOW + random.below(100);
            let expires_at = NOW + random.below(120);
            let lane_free = [0, 1, 64, 65, 1_024][random.below(5) as usize];
            let (tag, command) = match random.below(3) {
                0 => (1, place(account, step)),
                1 => (2, cancel(account, step)),
                _ => (3, modify(account, step)),
            };
            let forged = random.below(4) == 0;
            let signer = if forged { &forger } else { &keys[which] };
            let msg = message(signer, DEPLOYMENT, account, nonce, expires_at, &command);
            let spec = Spec { nonce, expires_at, tag, forged };

            let expected = model(&spec, &mut model_nonces[which], now, lane_free);
            let got = gateway.check(&msg, now, lane_free);
            assert_eq!(
                got.is_ok(),
                expected,
                "step {step}: account {account}, {spec:?} at {now}, {lane_free} free: {got:?}"
            );
            assert_eq!(gateway.last_nonce(account), Some(model_nonces[which]), "step {step}");
            if expected {
                forwarded.0 += 1;
            } else {
                forwarded.1 += 1;
            }
        }
        // Both outcomes are common, so the comparison means something.
        assert!(forwarded.0 > 150 && forwarded.1 > 150, "{forwarded:?}");
    }

    #[test]
    fn a_rebuilt_signed_part_matches_what_the_client_signed() {
        // The ClientRecord carries the message's own words, so the journal can rebuild the
        // signed bytes exactly (13.4).
        let mut gateway = gateway_1();
        let msg = signed(acct(9), 42, &modify(acct(9), 17));
        let accepted = gateway.check(&msg, NOW, ROOM).expect("accepted");
        let command = pipeline::codec::decode_command(&accepted.command).expect("decodes");
        let rebuilt: [u8; SIGNED_BYTES] =
            encode_signed_part(1, acct(9), accepted.nonce, accepted.expires_at, &command);
        assert_eq!(&rebuilt, wire::signed_part(&msg));
    }

    #[test]
    fn every_reason_has_its_own_index_in_check_order() {
        for (i, reason) in GatewayReject::ALL.iter().enumerate() {
            assert_eq!(reason.index(), i);
        }
        assert_eq!(GatewayReject::Busy.to_string(), "Busy");
    }
}

#[cfg(test)]
mod eip712_tests {
    //! The EIP-712 scheme's checks (module docs, its table; D-033): each reject reason, the
    //! order (the cheap checks before the recovery, `Busy` before it, the salt table's
    //! lookup before it and its insert only after it), and a property test against a small
    //! model. Every test runs with the tests' verifier (`test_support::VERIFIER`).
    use super::*;
    use crate::test_support::{
        XorShift, acct, cancel, high_s_twin, message, message_eip712, modify, place, registry, signing_key,
        with_market,
    };
    use crate::wire::{
        COMMAND_OFFSET, EIP712_RESERVED_OFFSET, R_OFFSET, RECOVERY_ID_OFFSET, S_OFFSET, SALT_OFFSET,
        TS_OFFSET, encode_eip712, verify_digest,
    };
    use engine::command::{Deposit, SetMark};
    use engine::types::{MarketId, Micros};
    use k256::ecdsa::SigningKey;
    use std::collections::HashSet;

    const DEPLOYMENT: u32 = 1;
    const SEED: u64 = 1;
    /// 8 gateways, as in the perp tests: accounts 1, 9, 17, 25 and 33 are on gateway 1.
    const N: usize = 8;
    /// The gateway's clock, in nanoseconds since the UNIX epoch...
    const NOW: u64 = 1_790_000_000_000_000_000;
    /// ...and in milliseconds, as timestamps are.
    const NOW_MS: u64 = NOW / 1_000_000;
    /// A lane with plenty of room.
    const ROOM: usize = 1_024;

    /// Gateway `g` of 8 for deployment 1 in the EIP-712 scheme, with the keys of accounts 1
    /// to 40 (seed 1) and a salt table with room for `requests`.
    fn eip712_gateway(g: usize, requests: usize) -> Gateway {
        let salts = SaltTable::new(requests, 0x5A17);
        Gateway::new_eip712(g, N, DEPLOYMENT, &registry(SEED, DEPLOYMENT, (1..=40).map(acct)), salts, 0)
    }

    fn gateway_1() -> Gateway {
        eip712_gateway(1, 64)
    }

    fn key(account: AccountId) -> SigningKey {
        signing_key(SEED, account)
    }

    /// Account `account`'s own message: `command` with `salt`, timestamped `ts_ms`.
    fn signed(account: AccountId, salt: u64, ts_ms: u64, command: &Command) -> [u8; MESSAGE_BYTES] {
        message_eip712(&key(account), DEPLOYMENT, account, salt, ts_ms, command)
    }

    /// Requests in the gateway's salt table.
    fn used(gateway: &Gateway) -> usize {
        gateway.salt_table().expect("the EIP-712 scheme").used()
    }

    #[test]
    fn a_valid_message_is_accepted_and_uses_up_its_request() {
        let mut gateway = gateway_1();
        assert_eq!(gateway.scheme(), AuthScheme::Eip712);
        let msg = signed(acct(9), 42, NOW_MS, &place(acct(9), 17));
        let accepted = gateway.check(&msg, NOW, ROOM).expect("accepted");
        // The salt and the timestamp travel in the nonce's and the expiry's fields.
        assert_eq!((accepted.account, accepted.nonce, accepted.expires_at), (acct(9), 42, NOW_MS));
        assert_eq!(accepted.command, pipeline::codec::encode_command(&place(acct(9), 17)));
        assert_eq!(accepted.signature, signature_words(&msg));
        assert_eq!(used(&gateway), 1);
        assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::ReusedRequest));
        // The same salt with another timestamp, or by another account, is another request.
        assert!(gateway.check(&signed(acct(9), 42, NOW_MS - 1, &place(acct(9), 18)), NOW, ROOM).is_ok());
        assert!(gateway.check(&signed(acct(17), 42, NOW_MS, &place(acct(17), 1)), NOW, ROOM).is_ok());
        assert_eq!(used(&gateway), 3);
        assert_eq!(gateway.last_nonce(acct(9)), Some(0), "no nonces in this scheme");
        assert_eq!(gateway.accounts(), 5, "accounts 1, 9, 17, 25 and 33");
    }

    #[test]
    fn each_scheme_refuses_the_others_messages_as_wrong_domain() {
        let mut eip712 = gateway_1();
        let perp_message = message(&key(acct(9)), DEPLOYMENT, acct(9), 42, u64::MAX, &place(acct(9), 17));
        assert_eq!(eip712.check(&perp_message, NOW, ROOM), Err(GatewayReject::WrongDomain));
        let mut perp = Gateway::new(
            1,
            N,
            DEPLOYMENT,
            &registry(SEED, DEPLOYMENT, (1..=40).map(acct)),
            &NonceTable::new(),
            0,
        );
        let eip712_message = signed(acct(9), 42, NOW_MS, &place(acct(9), 17));
        assert_eq!(perp.check(&eip712_message, NOW, ROOM), Err(GatewayReject::WrongDomain));
        // Another deployment or magic; the deployment is compared first, before byte 7.
        let deployment_2 = message_eip712(&key(acct(9)), 2, acct(9), 42, NOW_MS, &place(acct(9), 17));
        let mut magic = eip712_message;
        magic[0] = b'X';
        let mut both = deployment_2;
        both[EIP712_RESERVED_OFFSET] = 1;
        for msg in [deployment_2, magic, both] {
            assert_eq!(eip712.check(&msg, NOW, ROOM), Err(GatewayReject::WrongDomain));
        }
        assert_eq!(used(&eip712), 0);
    }

    #[test]
    fn a_bad_recovery_id_or_reserved_byte_is_malformed_and_operator_commands_are_operator_only() {
        let mut gateway = gateway_1();
        let good = signed(acct(9), 42, NOW_MS, &place(acct(9), 17));
        let with = |offset: usize, value: u8| {
            let mut msg = good;
            msg[offset] = value;
            msg
        };
        assert_eq!(gateway.check(&with(EIP712_RESERVED_OFFSET, 1), NOW, ROOM), Err(GatewayReject::Malformed));
        for v in [2, 3, 27, 28] {
            assert_eq!(
                gateway.check(&with(RECOVERY_ID_OFFSET, v), NOW, ROOM),
                Err(GatewayReject::Malformed),
                "{v}"
            );
        }
        assert_eq!(
            gateway.check(&with(COMMAND_OFFSET + 1, 2), NOW, ROOM),
            Err(GatewayReject::Malformed),
            "side 2"
        );
        // Operator commands have no signed form; any signature is refused before it matters.
        for command in [
            Command::Deposit(Deposit { amount: Micros::new(1_000_000), account: acct(9) }),
            Command::SetMark(SetMark { price: Price::new(103_000), market: MarketId::new(3) }),
        ] {
            let msg = encode_eip712(DEPLOYMENT, acct(9), 42, NOW_MS, &command, &[1; SIGNATURE_BYTES], 0);
            assert_eq!(gateway.check(&msg, NOW, ROOM), Err(GatewayReject::OperatorOnly));
        }
        assert!(gateway.check(&good, NOW, ROOM).is_ok(), "none of these used the request");
    }

    #[test]
    fn routing_ownership_and_registration_come_before_the_timestamp() {
        let mut gateway = gateway_1();
        // Each with a timestamp far outside the window: the earlier check still wins.
        let stale = 0;
        let another_gateways = signed(acct(10), 1, stale, &place(acct(10), 1));
        assert_eq!(gateway.check(&another_gateways, NOW, ROOM), Err(GatewayReject::WrongGateway));
        let not_owner = signed(acct(9), 1, stale, &cancel(acct(17), 3)); // 9 cancels 17's order
        assert_eq!(gateway.check(&not_owner, NOW, ROOM), Err(GatewayReject::NotOwner));
        let stranger = SigningKey::from_slice(&[3; 32]).expect("a valid scalar");
        let unknown = message_eip712(&stranger, DEPLOYMENT, acct(41), 1, stale, &place(acct(41), 1)); // 41 mod 8 = 1
        assert_eq!(gateway.check(&unknown, NOW, ROOM), Err(GatewayReject::UnknownAccount));
    }

    #[test]
    fn the_timestamp_window_is_5_minutes_back_and_60_seconds_ahead_of_the_clock() {
        let mut gateway = gateway_1();
        let at = |salt: u64, ts_ms: u64| signed(acct(9), salt, ts_ms, &place(acct(9), salt as u32));
        assert_eq!(
            gateway.check(&at(1, NOW_MS - MAX_AGE_MS - 1), NOW, ROOM),
            Err(GatewayReject::StaleTimestamp)
        );
        assert!(gateway.check(&at(2, NOW_MS - MAX_AGE_MS), NOW, ROOM).is_ok(), "5 minutes exactly");
        assert_eq!(
            gateway.check(&at(3, NOW_MS + MAX_AHEAD_MS + 1), NOW, ROOM),
            Err(GatewayReject::FutureTimestamp)
        );
        assert!(gateway.check(&at(4, NOW_MS + MAX_AHEAD_MS), NOW, ROOM).is_ok(), "60 s exactly");
        assert_eq!(gateway.check(&at(5, 0), NOW, ROOM), Err(GatewayReject::StaleTimestamp));
        assert_eq!(gateway.check(&at(6, u64::MAX), NOW, ROOM), Err(GatewayReject::FutureTimestamp));
        // The clock counts whole milliseconds, rounded down: 999,999 ns later is the same
        // millisecond, and one more nanosecond is the next.
        let edge = at(7, NOW_MS - MAX_AGE_MS);
        assert_eq!(gateway.check(&edge, NOW + 1_000_000, ROOM), Err(GatewayReject::StaleTimestamp));
        assert!(gateway.check(&edge, NOW + 999_999, ROOM).is_ok());
        assert_eq!(used(&gateway), 3, "only the accepted ones");
    }

    #[test]
    fn a_replay_is_refused_before_anything_is_recovered() {
        let mut gateway = gateway_1();
        let msg = signed(acct(9), 42, NOW_MS, &place(acct(9), 17));
        assert!(gateway.check(&msg, NOW, ROOM).is_ok());
        // The same request with its signature destroyed, or with the high-S twin (6.5,
        // attack 7), gets the same answer: the lookup comes first.
        let mut broken = msg;
        broken[R_OFFSET + 5] ^= 0x40;
        let twin = encode_eip712(
            DEPLOYMENT,
            acct(9),
            42,
            NOW_MS,
            &place(acct(9), 17),
            &high_s_twin(wire::signature(&msg)),
            msg[6],
        );
        for replay in [msg, broken, twin] {
            assert_eq!(gateway.check(&replay, NOW, ROOM), Err(GatewayReject::ReusedRequest));
        }
        // Even with the lane full, since check 8 comes before check 10.
        assert_eq!(gateway.check(&msg, NOW, 0), Err(GatewayReject::ReusedRequest));
    }

    #[test]
    fn a_forgery_never_uses_up_a_request_or_a_slot() {
        let mut gateway = gateway_1();
        // Signed with account 17's key, claiming account 9 and its order (6.5, attack 4).
        let by_17 = message_eip712(&key(acct(17)), DEPLOYMENT, acct(9), 42, NOW_MS, &place(acct(9), 17));
        assert_eq!(gateway.check(&by_17, NOW, ROOM), Err(GatewayReject::WrongSigner));
        // r = 0 (or any r that is not a point's x): no key comes out at all.
        let mut no_key = signed(acct(9), 42, NOW_MS, &place(acct(9), 17));
        no_key[R_OFFSET..S_OFFSET].fill(0);
        assert_eq!(gateway.check(&no_key, NOW, ROOM), Err(GatewayReject::BadSignature));
        assert_eq!(used(&gateway), 0, "neither took a slot");
        // So the genuine message with the same salt and timestamp is still accepted.
        assert!(gateway.check(&signed(acct(9), 42, NOW_MS, &place(acct(9), 17)), NOW, ROOM).is_ok());
        assert_eq!(used(&gateway), 1);
    }

    #[test]
    fn a_signature_by_another_key_or_over_edited_fields_is_the_wrong_signer() {
        let mut gateway = gateway_1();
        let genuine = signed(acct(9), 42, NOW_MS, &place(acct(9), 17));
        // Every signed field edited after signing: the digest changes, and the key that
        // comes out of the signature is somebody else's. (The account and the order id are
        // caught earlier, by NotOwner.)
        let mut salt = genuine;
        salt[SALT_OFFSET] ^= 1;
        let mut ts = genuine;
        ts[TS_OFFSET] ^= 1; // 1 ms earlier: still inside the window
        let mut price = genuine;
        price[COMMAND_OFFSET + 16] ^= 1; // the price's lowest byte
        let mut id = genuine;
        id[RECOVERY_ID_OFFSET] ^= 1; // the other point with x = r
        // Signed for deployment 2, then its field edited to 1 (6.5, attack 5): the domain
        // is the deployment, so the digest differs.
        let mut deployment = message_eip712(&key(acct(9)), 2, acct(9), 42, NOW_MS, &place(acct(9), 17));
        deployment[wire::DEPLOYMENT_OFFSET] = 1;
        for (what, forged) in
            [("salt", salt), ("ts", ts), ("price", price), ("v", id), ("deployment", deployment)]
        {
            assert_eq!(gateway.check(&forged, NOW, ROOM), Err(GatewayReject::WrongSigner), "{what}");
        }
        assert_eq!(used(&gateway), 0);
        assert!(gateway.check(&genuine, NOW, ROOM).is_ok());
    }

    /// `message` with its market, bytes 4 and 5 of the CMD40, set to `market`: a copy made
    /// after signing.
    fn on_market(message: &[u8; MESSAGE_BYTES], market: MarketId) -> [u8; MESSAGE_BYTES] {
        let mut copy = *message;
        copy[COMMAND_OFFSET + 4..COMMAND_OFFSET + 6].copy_from_slice(&market.get().to_le_bytes());
        copy
    }

    #[test]
    fn a_cancel_or_a_modify_copied_to_another_market_is_another_request_and_the_genuine_one_is_accepted() {
        // Module docs, "Why the market is in the request" (D-033, "Trade-offs"; 5.8, attack
        // 7). Polymarket's cancel and modify name only the order, so a copy with another
        // market passes every check, the signer's included. It is another request: accepted
        // here (the engine will answer UnknownOrder or UnknownMarket), and it doesn't use up
        // the genuine one, whether it comes before the genuine message or after it.
        let mut gateway = gateway_1();
        let cases = [
            (42, cancel(acct(9), 17), true),
            (43, modify(acct(9), 17), true),
            (44, cancel(acct(9), 18), false),
        ];
        for (salt, command, copy_first) in cases {
            let genuine = signed(acct(9), salt, NOW_MS, &command);
            let (on_2, on_3) = (MarketId::new(2), MarketId::new(3));
            let copy = on_market(&genuine, on_2);
            // Each message with the market it is forwarded with: the genuine one is on 3.
            let order =
                if copy_first { [(copy, on_2), (genuine, on_3)] } else { [(genuine, on_3), (copy, on_2)] };
            for (msg, market) in order {
                let accepted = gateway.check(&msg, NOW, ROOM).expect("accepted");
                let forwarded = pipeline::codec::decode_command(&accepted.command).expect("decodes");
                assert_eq!(forwarded, with_market(&command, market), "salt {salt}");
            }
            // Each is used up now: an exact copy of either, the same market, is reused.
            assert_eq!(gateway.check(&genuine, NOW, ROOM), Err(GatewayReject::ReusedRequest));
            assert_eq!(gateway.check(&copy, NOW, ROOM), Err(GatewayReject::ReusedRequest));
        }
        assert_eq!(used(&gateway), 6, "two requests for each signed message");
        // One copy per market id, up to the last one.
        let genuine = signed(acct(9), 45, NOW_MS, &cancel(acct(9), 19));
        for market in [0, 1, 4, u16::MAX].map(MarketId::new) {
            assert!(gateway.check(&on_market(&genuine, market), NOW, ROOM).is_ok(), "market {market}");
        }
        assert!(gateway.check(&genuine, NOW, ROOM).is_ok(), "the genuine one, after four copies");
        assert_eq!(used(&gateway), 11);
    }

    #[test]
    fn a_places_market_is_signed_so_a_copy_on_another_market_is_the_wrong_signer_and_takes_no_slot() {
        // The request includes a place's market too, but a place signs it: a copy with
        // another market fails the signer check, whether the genuine message came first or
        // not, and never gets a request of its own.
        let mut gateway = gateway_1();
        let genuine = signed(acct(9), 42, NOW_MS, &place(acct(9), 17));
        let copy = on_market(&genuine, MarketId::new(2));
        assert_eq!(gateway.check(&copy, NOW, ROOM), Err(GatewayReject::WrongSigner));
        assert!(gateway.check(&genuine, NOW, ROOM).is_ok());
        assert_eq!(gateway.check(&genuine, NOW, ROOM), Err(GatewayReject::ReusedRequest));
        // After the genuine message, the copy is another request (its market differs), so
        // the lookup doesn't refuse it: the signer check does.
        assert_eq!(gateway.check(&copy, NOW, ROOM), Err(GatewayReject::WrongSigner));
        assert_eq!(used(&gateway), 1, "only the genuine place");
    }

    #[test]
    fn busy_comes_before_the_recovery_and_keeps_the_last_64_slots_for_cancels() {
        let mut gateway = gateway_1();
        // A forgery with a full lane is Busy: the gateway spends no recovery on what it
        // can't forward.
        let forged = message_eip712(&key(acct(17)), DEPLOYMENT, acct(9), 1, NOW_MS, &place(acct(9), 1));
        assert_eq!(gateway.check(&forged, NOW, CANCEL_HEADROOM), Err(GatewayReject::Busy));
        let place_msg = signed(acct(9), 2, NOW_MS, &place(acct(9), 2));
        let modify_msg = signed(acct(9), 3, NOW_MS, &modify(acct(9), 2));
        let cancel_msg = signed(acct(9), 4, NOW_MS, &cancel(acct(9), 2));
        assert_eq!(gateway.check(&place_msg, NOW, CANCEL_HEADROOM), Err(GatewayReject::Busy));
        assert_eq!(gateway.check(&modify_msg, NOW, CANCEL_HEADROOM), Err(GatewayReject::Busy));
        assert_eq!(gateway.check(&cancel_msg, NOW, 0), Err(GatewayReject::Busy));
        assert_eq!(used(&gateway), 0, "Busy uses no request");
        assert!(gateway.check(&cancel_msg, NOW, 1).is_ok(), "one free slot is enough for a cancel");
        assert!(
            gateway.check(&place_msg, NOW, CANCEL_HEADROOM + 1).is_ok(),
            "the same bytes, once there is room"
        );
    }

    #[test]
    fn the_high_s_twin_is_refused_before_anything_is_recovered() {
        let mut gateway = gateway_1();
        let msg = signed(acct(9), 42, NOW_MS, &place(acct(9), 17));
        let twin = encode_eip712(
            DEPLOYMENT,
            acct(9),
            42,
            NOW_MS,
            &place(acct(9), 17),
            &high_s_twin(wire::signature(&msg)),
            msg[6],
        );
        assert_eq!(gateway.check(&twin, NOW, ROOM), Err(GatewayReject::HighS), "original not yet sent");
        let mut huge_s = msg;
        huge_s[S_OFFSET..].fill(0xFF);
        assert_eq!(gateway.check(&huge_s, NOW, ROOM), Err(GatewayReject::HighS));
        assert!(gateway.check(&msg, NOW, ROOM).is_ok(), "the low form is accepted");
    }

    #[test]
    fn a_full_salt_table_refuses_before_the_recovery_and_only_new_requests() {
        let mut gateway = eip712_gateway(1, 1); // 2 slots, room for 1
        let forged = message_eip712(&key(acct(17)), DEPLOYMENT, acct(9), 1, NOW_MS, &place(acct(9), 1));
        assert_eq!(gateway.check(&forged, NOW, ROOM), Err(GatewayReject::WrongSigner));
        let first = signed(acct(9), 1, NOW_MS, &place(acct(9), 1));
        assert!(gateway.check(&first, NOW, ROOM).is_ok(), "the forgery took no room");
        let second = signed(acct(9), 2, NOW_MS, &place(acct(9), 2));
        assert_eq!(gateway.check(&second, NOW, ROOM), Err(GatewayReject::SaltTableFull));
        // Check 9 comes before Busy and the recovery; check 8 before it.
        let forged = message_eip712(&key(acct(17)), DEPLOYMENT, acct(9), 3, NOW_MS, &place(acct(9), 3));
        assert_eq!(gateway.check(&forged, NOW, 0), Err(GatewayReject::SaltTableFull));
        assert_eq!(gateway.check(&first, NOW, ROOM), Err(GatewayReject::ReusedRequest));
    }

    #[test]
    #[should_panic(expected = "perp scheme only")]
    fn the_verify_on_core_ablation_is_refused_for_the_eip712_scheme() {
        let _ = gateway_1().without_signature_checks();
    }

    #[test]
    fn a_rebuilt_digest_is_what_the_client_signed() {
        // The lane record carries the message's own words, and the journal keeps them, so
        // the audit can rebuild the digest and check it with the account's key (13.4).
        let mut gateway = gateway_1();
        let msg = signed(acct(9), 42, NOW_MS, &modify(acct(9), 17));
        let accepted = gateway.check(&msg, NOW, ROOM).expect("accepted");
        let command = pipeline::codec::decode_command(&accepted.command).expect("decodes");
        let data = eip712::op_data(&command).expect("a modify has a form");
        let digest = eip712::digest(&Domain::new(1), &data, accepted.nonce, accepted.expires_at);
        let signature: [u8; SIGNATURE_BYTES] = pipeline::codec::to_le_bytes(&accepted.signature);
        let key_9 = registry(SEED, DEPLOYMENT, [9].map(acct)).key(acct(9)).copied().expect("a key");
        assert_eq!(verify_digest(&key_9, &digest, &signature), Ok(()));
    }

    #[test]
    fn the_warm_up_runs_for_both_schemes() {
        gateway_1().prepare_this_thread();
        Gateway::new(1, N, DEPLOYMENT, &registry(SEED, DEPLOYMENT, [9].map(acct)), &NonceTable::new(), 0)
            .prepare_this_thread();
    }

    // -----------------------------------------------------------------------------------
    // A property test, as 18.2's for the perp scheme.

    /// The model of the EIP-712 order for well-formed messages of this gateway's accounts:
    /// the first failing check among the window, the request `(account, salt, ts, market)`,
    /// the lane and the signer; and the requests accepted so far.
    fn model(
        request: (AccountId, u64, u64, MarketId),
        tag: u8,
        forged: bool,
        lane_free: usize,
        accepted: &mut HashSet<(AccountId, u64, u64, MarketId)>,
    ) -> Result<(), GatewayReject> {
        let (_, _, ts_ms, _) = request;
        let room = if tag == command_tags::CANCEL_ORDER { lane_free >= 1 } else { lane_free > 64 };
        if ts_ms + MAX_AGE_MS < NOW_MS {
            Err(GatewayReject::StaleTimestamp)
        } else if ts_ms > NOW_MS + MAX_AHEAD_MS {
            Err(GatewayReject::FutureTimestamp)
        } else if accepted.contains(&request) {
            Err(GatewayReject::ReusedRequest)
        } else if !room {
            Err(GatewayReject::Busy)
        } else if forged {
            Err(GatewayReject::WrongSigner)
        } else {
            accepted.insert(request);
            Ok(())
        }
    }

    #[test]
    fn random_messages_are_answered_exactly_as_the_model_says() {
        let accounts = [1, 9, 17].map(acct); // all on gateway 1 of 8
        let keys = accounts.map(key);
        let forger = SigningKey::from_slice(&[9; 32]).expect("a valid scalar");
        let mut gateway = eip712_gateway(1, 4_096);
        let mut accepted = HashSet::new();
        let mut random = XorShift(0xE1_7120_5EED);
        // Few salts and timestamps, so requests repeat; timestamps on both edges.
        let timestamps = [
            NOW_MS - MAX_AGE_MS - 1,
            NOW_MS - MAX_AGE_MS,
            NOW_MS - 1,
            NOW_MS,
            NOW_MS + MAX_AHEAD_MS,
            NOW_MS + MAX_AHEAD_MS + 1,
        ];
        let mut outcomes = (0, 0);
        for step in 0..1_500 {
            let which = random.below(3) as usize;
            let account = accounts[which];
            let salt = random.below(4);
            let ts_ms = timestamps[random.below(timestamps.len() as u64) as usize];
            let lane_free = [0, 1, 64, 65, 1_024][random.below(5) as usize];
            // Two markets, so some requests differ only in their market.
            let market = MarketId::new(3 + random.below(2) as u16);
            let (tag, command) = match random.below(3) {
                0 => (1, place(account, step)),
                1 => (2, cancel(account, step)),
                _ => (3, modify(account, step)),
            };
            let command = with_market(&command, market);
            let forged = random.below(4) == 0;
            let signer = if forged { &forger } else { &keys[which] };
            let msg = message_eip712(signer, DEPLOYMENT, account, salt, ts_ms, &command);

            let request = (account, salt, ts_ms, market);
            let expected = model(request, tag, forged, lane_free, &mut accepted);
            let got = gateway.check(&msg, NOW, lane_free).map(|_| ());
            assert_eq!(
                got, expected,
                "step {step}: account {account}, salt {salt}, ts {ts_ms}, market {market}, {lane_free} free"
            );
            if expected.is_ok() { outcomes.0 += 1 } else { outcomes.1 += 1 }
        }
        assert_eq!(used(&gateway), accepted.len());
        // Both outcomes are common, so the comparison means something.
        assert!(outcomes.0 > 20 && outcomes.1 > 150, "{outcomes:?}");
    }
}
