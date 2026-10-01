//! Polymarket Perps' signing scheme: EIP-712 over the keccak-256 of the MessagePack-encoded
//! compact operation, with a salt and a millisecond timestamp (`docs/PIPELINE.md` 5.8;
//! `docs/DECISIONS.md` D-033). The opt-in second scheme, next to the 72 signed bytes of
//! `wire.rs`.
//!
//! **What a client signs**, for a command sent with `salt` and `ts` (Unix milliseconds):
//!
//! 1. The command's compact form, a positional array ([`encode_op`]):
//!    - `PlaceOrder` → `["createOrders", [[iid, buy, p, qty, tif, po, c]]]`;
//!    - `CancelOrder` → `["cancelOrdersCOID", [c]]`;
//!    - `ModifyOrder` → `["modifyOrdersCOID", [[c, p, qty]]]`, where `qty` is the new total
//!      size, the same convention as ours (D-008).
//!
//!    `iid` is the market, `buy` is true for a buy, `p` and `qty` are the price and the size
//!    as decimal strings (below), `tif` is `"gtc"` or `"ioc"`, `po` the post-only flag
//!    (always present), and `c` the order id as 32 lowercase hex digits. Polymarket's order
//!    also has `ro` (reduce-only) and `tr` (a trigger), which v1 doesn't have. Their SDKs
//!    and server build the form with every field in its place and then drop the absent
//!    ones, wherever they are, rather than write them as nil; so an order without `ro` and
//!    `tr` has 7 fields. As of 2026-09-30 the SDKs (py-sdk `to_raw_order`, ts-sdk
//!    `toRawPerpsOrder`) also add two slots after `tr` when a builder session is set: one
//!    reserved, always absent, and a builder-attribution pair `[address, feeRate]`. v1 has
//!    no builders, so both are absent and dropped too. Cancel and modify name the order
//!    only: the market is carried, but not signed, so a copy with another market passes
//!    the signer check. The market is part of the request the gateway accepts once, so
//!    such a copy is another request, which the engine rejects, and the genuine message is
//!    still accepted (`salts.rs`; D-033, "Trade-offs"). Operator commands have no form,
//!    since clients never send them.
//! 2. `data` = keccak-256 of the form's MessagePack bytes (`msgpack.rs`, `keccak.rs`;
//!    [`op_data`]).
//! 3. The EIP-712 digest ([`digest`]):
//!    `keccak256(0x19 0x01 || domainSeparator || hashStruct(op))`, where
//!    `hashStruct(op) = keccak256(OP_TYPEHASH || data || salt || ts)`, each value a 32-byte
//!    big-endian word. The domain ([`Domain`]) is
//!    `EIP712Domain(string name,string version,uint256 chainId)` = `{"Polymarket", "1",
//!    chain id}`, and `domainSeparator = keccak256(DOMAIN_TYPEHASH || keccak256(name) ||
//!    keccak256(version) || chainId)`. Our deployment id is the chain id, so a signature
//!    is good for one deployment only (Polymarket's production chain id is 137).
//!
//! The client signs the digest with secp256k1 (RFC 6979, low-S), and the gateway recovers
//! the signer from the signature and the recovery id (`verifier.rs`). A signer is known by
//! its Ethereum address, the last 20 bytes of keccak-256 of its public key's `x || y`
//! ([`address_of`]).
//!
//! **Prices and sizes** are ticks and lots (`engine::types`), written as decimals with
//! [`PRICE_DECIMALS`] and [`QTY_DECIMALS`] places for every market of the synthetic flow
//! (they add up to 6, so one tick times one lot is still one micro-dollar, D-004), in
//! minimal form: no trailing zeros, and no point for a whole number, so 50 ticks are
//! `"0.5"` and 100,000 lots `"10"`. Polymarket hashes a client's strings exactly as sent
//! (their SDK test signs `"100.50"`, zero included). We rebuild the form from the 40-byte
//! command, which has one spelling, so a signature verifies here only over that spelling
//! (D-033, "Options considered").
//!
//! **Checked against** Polymarket's 12 golden vectors, their TypeScript SDK's `data`
//! vectors and the EIP-712 specification's example (tests below; keccak-256's own known
//! answers are in `keccak.rs`).
//!
//! **Allocation.** None: the MessagePack bytes (at most [`MAX_OP_BYTES`]) and every hash
//! input are stack arrays.
//!
//! **Complexity.** [`op_data`]: one keccak-f permutation (the form is under 136 bytes).
//! [`digest`]: two more (128 and 66 bytes). [`Domain::new`], once per run: three.
//! [`address_of`]: one.

use engine::command::Command;
use engine::types::{OrderId, Side, TimeInForce};

use crate::keccak::keccak256;
use crate::msgpack::Writer;

/// Decimal places of every price (D-033): one tick is 0.01.
pub const PRICE_DECIMALS: u32 = 2;
/// Decimal places of every size (D-033): one lot is 0.0001.
pub const QTY_DECIMALS: u32 = 4;

/// The longest compact form in MessagePack bytes: a place with the largest market id
/// (3 bytes), the longest price and size (`i64::MIN` in 21 characters each, 22 bytes with
/// their header) and `c` (34 bytes), plus 13 bytes of `"createOrders"`, one boolean each
/// for `buy` and `po`, 4 bytes of `tif`, and three array headers.
pub const MAX_OP_BYTES: usize = 103;

/// The EIP-712 type of the domain, and its hash, `keccak256(DOMAIN_TYPE)`.
pub const DOMAIN_TYPE: &str = "EIP712Domain(string name,string version,uint256 chainId)";
pub const DOMAIN_TYPEHASH: [u8; 32] = [
    0xC2, 0xF8, 0x78, 0x71, 0x76, 0xB8, 0xAC, 0x6B, 0xF7, 0x21, 0x5B, 0x4A, 0xDC, 0xC1, 0xE0, 0x69, //
    0xBF, 0x4A, 0xB8, 0x2D, 0x9A, 0xB1, 0xDF, 0x05, 0xA5, 0x7A, 0x91, 0xD4, 0x25, 0x93, 0x5B, 0x6E,
];

/// The EIP-712 type of a signed operation, and its hash, `keccak256(OP_TYPE)`.
pub const OP_TYPE: &str = "Op(bytes32 data,uint64 salt,uint64 ts)";
pub const OP_TYPEHASH: [u8; 32] = [
    0xC1, 0x3E, 0x00, 0x21, 0xA8, 0x63, 0x6E, 0x16, 0xBA, 0x7C, 0x86, 0x63, 0x4D, 0xC8, 0x33, 0x94, //
    0x21, 0xD7, 0xC3, 0xDC, 0x3C, 0x61, 0x16, 0x1C, 0xB5, 0xCA, 0x11, 0x75, 0x41, 0xEA, 0x2F, 0xF5,
];

/// The domain's name and version (module docs).
pub const DOMAIN_NAME: &str = "Polymarket";
pub const DOMAIN_VERSION: &str = "1";

/// An Ethereum address: the last 20 bytes of keccak-256 of a public key's `x || y`.
pub type Address = [u8; 20];

/// The signing domain of one deployment, with its separator computed once (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Domain {
    chain_id: u64,
    separator: [u8; 32],
}

impl Domain {
    /// The domain `{"Polymarket", "1", chain_id}`.
    pub fn new(chain_id: u64) -> Domain {
        let fields = [
            DOMAIN_TYPEHASH,
            keccak256(DOMAIN_NAME.as_bytes()),
            keccak256(DOMAIN_VERSION.as_bytes()),
            word(chain_id),
        ];
        Domain { chain_id, separator: keccak256(fields.as_flattened()) }
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// `keccak256(DOMAIN_TYPEHASH || keccak256(name) || keccak256(version) || chainId)`.
    pub fn separator(&self) -> &[u8; 32] {
        &self.separator
    }
}

/// The MessagePack bytes of `command`'s compact form, written into `buffer` (module docs,
/// step 1); `None` for an operator command, which no client may send.
pub fn encode_op<'a>(command: &Command, buffer: &'a mut [u8; MAX_OP_BYTES]) -> Option<&'a [u8]> {
    let mut op = Writer::new(buffer);
    match command {
        Command::PlaceOrder(place) => {
            op.array(2);
            op.str(b"createOrders");
            op.array(1); // one order
            op.array(7); // [iid, buy, p, qty, tif, po, c]
            op.uint(u64::from(place.market.get()));
            op.bool(place.side == Side::Buy);
            op.str(Decimal::new(place.price.ticks(), PRICE_DECIMALS).as_bytes());
            op.str(Decimal::new(place.qty.lots(), QTY_DECIMALS).as_bytes());
            op.str(match place.tif {
                TimeInForce::Gtc => b"gtc",
                TimeInForce::Ioc => b"ioc",
            });
            op.bool(place.post_only);
            op.str(&order_id_hex(place.order_id));
        }
        Command::CancelOrder(cancel) => {
            op.array(2);
            op.str(b"cancelOrdersCOID");
            op.array(1); // one order id
            op.str(&order_id_hex(cancel.order_id));
        }
        Command::ModifyOrder(modify) => {
            op.array(2);
            op.str(b"modifyOrdersCOID");
            op.array(1); // one modify
            op.array(3); // [c, p, qty]
            op.str(&order_id_hex(modify.order_id));
            op.str(Decimal::new(modify.new_price.ticks(), PRICE_DECIMALS).as_bytes());
            op.str(Decimal::new(modify.new_size.lots(), QTY_DECIMALS).as_bytes());
        }
        Command::Deposit(_)
        | Command::Withdraw(_)
        | Command::SetLeverage(_)
        | Command::SetMark(_)
        | Command::SetMarketParams(_)
        | Command::SetRiskTier(_) => return None,
    }
    Some(op.finish())
}

/// `data`: the keccak-256 of `command`'s compact form (module docs, step 2); `None` for an
/// operator command.
pub fn op_data(command: &Command) -> Option<[u8; 32]> {
    let mut buffer = [0; MAX_OP_BYTES];
    encode_op(command, &mut buffer).map(keccak256)
}

/// The EIP-712 digest that is signed for an operation with `data`, `salt` and `ts_ms` in
/// `domain` (module docs, step 3).
pub fn digest(domain: &Domain, data: &[u8; 32], salt: u64, ts_ms: u64) -> [u8; 32] {
    let op = keccak256([OP_TYPEHASH, *data, word(salt), word(ts_ms)].as_flattened());
    sign_hash(domain.separator(), &op)
}

/// The Ethereum address of a public key given in SEC1's uncompressed form, `4 || x || y`
/// (65 bytes, as both verifiers write it): the last 20 bytes of `keccak256(x || y)`.
pub fn address_of(uncompressed: &[u8; 65]) -> Address {
    debug_assert_eq!(uncompressed[0], 4, "SEC1's tag of an uncompressed point");
    *keccak256(&uncompressed[1..]).last_chunk().expect("an address is the last 20 of 32 bytes")
}

/// EIP-712's final hash of a struct `struct_hash` in the domain `separator`:
/// `keccak256(0x19 0x01 || separator || struct_hash)`. (`0x19` is what no RLP-encoded
/// Ethereum transaction starts with, and `0x01` is EIP-712's version byte.)
fn sign_hash(separator: &[u8; 32], struct_hash: &[u8; 32]) -> [u8; 32] {
    let mut encoded = [0u8; 66];
    encoded[..2].copy_from_slice(&[0x19, 0x01]);
    encoded[2..34].copy_from_slice(separator);
    encoded[34..].copy_from_slice(struct_hash);
    keccak256(&encoded)
}

/// `value` as an EIP-712 `uint64` or `uint256`: a 32-byte big-endian word.
fn word(value: u64) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[24..].copy_from_slice(&value.to_be_bytes());
    word
}

/// `c`, the order id as Polymarket's 32 lowercase hex digits. Theirs has 128 bits and ours
/// 64, so the first 16 digits are always 0.
fn order_id_hex(order_id: OrderId) -> [u8; 32] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = [b'0'; 32];
    for (i, digit) in hex[16..].iter_mut().enumerate() {
        let nibble = (order_id.get() >> (60 - 4 * i)) & 0xF;
        *digit = DIGITS[nibble as usize];
    }
    hex
}

/// A number of ticks or lots as a decimal string, on the stack: `value / 10^decimals` in
/// minimal form (module docs, "Prices and sizes"). Every `i64` has one, negative ones
/// with a leading `-`, although a valid order's price and size are positive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Decimal {
    /// The text is `bytes[start..]`, written from the end backwards.
    bytes: [u8; DECIMAL_BYTES],
    start: usize,
}

/// The longest decimal: `i64::MIN` with 19 places, `-0.` and 19 digits.
const DECIMAL_BYTES: usize = 22;

impl Decimal {
    /// `value / 10^decimals`; `decimals` is at most 19, so that `10^decimals` fits a `u64`.
    fn new(value: i64, decimals: u32) -> Decimal {
        assert!(decimals <= 19, "10^{decimals} doesn't fit a u64");
        let unit = 10u64.pow(decimals);
        let magnitude = value.unsigned_abs();
        let (mut whole, mut fraction) = (magnitude / unit, magnitude % unit);
        // Drop the fraction's trailing zeros; if it was all zeros, no fraction is left.
        let mut fraction_digits = decimals;
        while fraction_digits > 0 && fraction % 10 == 0 {
            fraction /= 10;
            fraction_digits -= 1;
        }
        // Right to left: the fraction's digits (with its leading zeros), the point, the
        // whole part's digits (at least one), the sign.
        let mut bytes = [0u8; DECIMAL_BYTES];
        let mut start = DECIMAL_BYTES;
        for _ in 0..fraction_digits {
            start -= 1;
            bytes[start] = b'0' + (fraction % 10) as u8;
            fraction /= 10;
        }
        if fraction_digits > 0 {
            start -= 1;
            bytes[start] = b'.';
        }
        loop {
            start -= 1;
            bytes[start] = b'0' + (whole % 10) as u8;
            whole /= 10;
            if whole == 0 {
                break;
            }
        }
        if value < 0 {
            start -= 1;
            bytes[start] = b'-';
        }
        Decimal { bytes, start }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[self.start..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{acct, bytes};
    use crate::verifier::{PublicKey, VerifierKind, sign_recoverable};
    use engine::command::{CancelOrder, Deposit, ModifyOrder, PlaceOrder, SetMark};
    use engine::types::{MarketId, Micros, OrderSeq, Price, Qty, order_id};
    use k256::ecdsa::SigningKey;

    /// The bytes of hex text, with or without a leading `0x`, white space ignored.
    fn hex(text: &str) -> Vec<u8> {
        bytes(text.trim_start().trim_start_matches("0x"))
    }

    /// What `write` writes.
    fn written(write: fn(&mut Writer)) -> Vec<u8> {
        let mut buffer = [0u8; 256];
        let mut writer = Writer::new(&mut buffer);
        write(&mut writer);
        writer.finish().to_vec()
    }

    /// EIP-712's `hashStruct`: keccak-256 of the type's hash, then the encoded fields, 32
    /// bytes each (a string's field is its keccak-256, an address is left-padded).
    fn hash_struct(type_string: &str, fields: &[[u8; 32]]) -> [u8; 32] {
        let mut words = vec![keccak256(type_string.as_bytes())];
        words.extend_from_slice(fields);
        keccak256(words.as_flattened())
    }

    /// An address, `0x` and 40 hex digits in either case, as an EIP-712 word.
    fn address_word(address: &str) -> [u8; 32] {
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(&hex(address));
        word
    }

    /// The uncompressed public key of `key`.
    fn uncompressed(key: &SigningKey) -> [u8; 65] {
        key.verifying_key().to_sec1_point(false).as_bytes().try_into().expect("65 bytes")
    }

    // ---- Polymarket's golden vectors ----
    //
    // From py-sdk `tests/unit/test_perps_signing_golden.py` ("Golden signing vectors
    // generated from the TypeScript SDK Perps implementation"), fetched 2026-09-30:
    // https://github.com/Polymarket/py-sdk/blob/main/tests/unit/test_perps_signing_golden.py
    // Its ten operations, then its two owner-signed messages, CreateProxy and Withdraw: 12
    // signatures, all by one key, with chain id 137. The file gives each operation's
    // `data` and each signature, `r || s || v` with `v` = 27 + the recovery id.

    const GOLDEN_KEY: &str = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const GOLDEN_CHAIN_ID: u64 = 137;
    /// The key's address. The file doesn't print it; Circle's cctp-go pairs the same key
    /// with it (`testutil.TestAddress`, "Derived from TestPrivateKey"), and
    /// [`the_golden_keys_address`] derives it from the key.
    const GOLDEN_ADDRESS: &str = "0xfcad0b19bb29d4674531d6f115237e16afce377c";

    fn golden_key() -> SigningKey {
        SigningKey::from_slice(&hex(GOLDEN_KEY)).expect("a valid private key")
    }

    fn golden_address() -> Address {
        hex(GOLDEN_ADDRESS).try_into().expect("20 bytes")
    }

    /// One of the file's operations: its compact form (the Python value, in the comment),
    /// written with our MessagePack writer, then `salt`, `timestamp` (ms), `data` and the
    /// signature, as the file gives them.
    struct OpVector {
        name: &'static str,
        op: fn(&mut Writer),
        salt: u64,
        timestamp: u64,
        data: &'static str,
        signature: &'static str,
    }

    fn golden_op_vectors() -> [OpVector; 10] {
        [
            OpVector {
                name: "createOrders single GTC",
                // ["createOrders", [[1, True, "0.5", "10", "gtc", False, None, None, None]]]
                op: |w| {
                    w.array(2);
                    w.str(b"createOrders");
                    w.array(1);
                    w.array(6);
                    w.uint(1);
                    w.bool(true);
                    w.str(b"0.5");
                    w.str(b"10");
                    w.str(b"gtc");
                    w.bool(false);
                },
                salt: 12345,
                timestamp: 1751500000000,
                data: "0x8004f264b573f0d5edd3377ef127f251a2b11e0b9463c5fb5f1be3b42c94336a",
                signature: "0xdd933bdada3c14c01dbe48fc02d470f423f0e0ef0897052602c964e20f82c709
                            7c1ea1033e1e939a0633db606781f3b7160603337ab6201755cb191e99bc2d9b1c",
            },
            OpVector {
                name: "createOrders IOC no price with coid",
                // ["createOrders", [[42, False, None, "2.5", "ioc", False, None,
                //   "aabbccddeeff00112233445566778899", None]]]
                op: |w| {
                    w.array(2);
                    w.str(b"createOrders");
                    w.array(1);
                    w.array(6);
                    w.uint(42);
                    w.bool(false);
                    w.str(b"2.5");
                    w.str(b"ioc");
                    w.bool(false);
                    w.str(b"aabbccddeeff00112233445566778899");
                },
                salt: 4294967295,
                timestamp: 1751500000001,
                data: "0x3b358621af8f4d297ea0c78b9d40aa9d33b84de0f9a619726f8f9e2f305c0157",
                signature: "0x1b728cc550e2691bab29ccf183990ab53786fe86b28a5659560f6c7e92faa8f6
                            0535f2a0731265f0bdb9bf9c3649ada69857dfe33c887ff9b3fc2794d605adde1b",
            },
            OpVector {
                name: "createOrders grouped with tpsl triggers",
                // ["createOrders", [[7, True, "100", "3", "gtc", False, None, None, None],
                //   [7, False, None, "3", None, False, True, None, [True, "200", "tp"]],
                //   [7, False, "50", "3", None, False, True, None, [None, "49", "sl"]]], "order"]
                op: |w| {
                    w.array(3);
                    w.str(b"createOrders");
                    w.array(3);
                    w.array(6);
                    w.uint(7);
                    w.bool(true);
                    w.str(b"100");
                    w.str(b"3");
                    w.str(b"gtc");
                    w.bool(false);
                    w.array(6);
                    w.uint(7);
                    w.bool(false);
                    w.str(b"3");
                    w.bool(false);
                    w.bool(true);
                    w.array(3);
                    w.bool(true);
                    w.str(b"200");
                    w.str(b"tp");
                    w.array(7);
                    w.uint(7);
                    w.bool(false);
                    w.str(b"50");
                    w.str(b"3");
                    w.bool(false);
                    w.bool(true);
                    w.array(2);
                    w.str(b"49");
                    w.str(b"sl");
                    w.str(b"order");
                },
                salt: 1,
                timestamp: 1751500000002,
                data: "0x34b31ff82aa6d39bd6ee54a7781287d6e6dffcc0cc7dcc981f70e381c7d8a641",
                signature: "0xfa7438604c02142ed9a0e3612a38d3b0b49eb21141515ac9a30be03b9f4422ea
                            5b598c70b2e733f6564eb322ac6ec5293c656c86426f389e9c5454e33d086cb21b",
            },
            OpVector {
                name: "cancelOrders",
                // ["cancelOrders", [11, 22, 33]]
                op: |w| {
                    w.array(2);
                    w.str(b"cancelOrders");
                    w.array(3);
                    w.uint(11);
                    w.uint(22);
                    w.uint(33);
                },
                salt: 999,
                timestamp: 1751500000003,
                data: "0x13f4659952efbcd144324c0a1c50cb75069635b2bbf28d3cc1e51b6e176a7617",
                signature: "0xb97be11bcffaca178b29ecc42cabc732d0f436fad42a42e3a63f32131b347be5
                            11d452b14a81bdefc7cfda84bf83c94afedfb16fd8df16883cf6e261e411dfe91c",
            },
            OpVector {
                name: "cancelOrdersCOID",
                // ["cancelOrdersCOID", ["aabbccddeeff00112233445566778899"]]
                op: |w| {
                    w.array(2);
                    w.str(b"cancelOrdersCOID");
                    w.array(1);
                    w.str(b"aabbccddeeff00112233445566778899");
                },
                salt: 1000,
                timestamp: 1751500000004,
                data: "0xdf8f1749d1b75392360c1117dcf2e0f251b0197b4db21c37a4e7d2451bc7cc95",
                signature: "0xf3b23b2357864efb2cd1ac128a9b6601c85a7894d9be000243f253a37f778c39
                            525d0359d95e01350d5bfca6ebf4b84bb5735acfbcb8c8fb90978bb9f371d58a1c",
            },
            OpVector {
                name: "updateLeverage",
                // ["updateLeverage", [3, 20, True]]
                op: |w| {
                    w.array(2);
                    w.str(b"updateLeverage");
                    w.array(3);
                    w.uint(3);
                    w.uint(20);
                    w.bool(true);
                },
                salt: 7,
                timestamp: 1751500000005,
                data: "0x1daa002ce2f4e42ab0ef2d3c489b2386cccac2cbb861bdd7695761d4d93dd410",
                signature: "0x5b81c182f89acc819488f30daa7a6fdb7563b0f65b2b404e9ece8a72d8077c3f
                            6bf328900ed6594d56605e564fa1e4469c8be670ac86015ab160bd78a97ea2bd1b",
            },
            OpVector {
                name: "autoCancel arm",
                // ["autoCancel", [1767000045000]]
                op: |w| {
                    w.array(2);
                    w.str(b"autoCancel");
                    w.array(1);
                    w.uint(1767000045000);
                },
                salt: 12345,
                timestamp: 1751500000000,
                data: "0x8d16f1dbf6be71cea6ad70c5b09f028dd1b451cebb2c7dd20c82dbf1022440ba",
                signature: "0x9019fb6edfc125e19635ac74d50efc1c863bc26e4d395ee76a8e26d7df001144
                            24d5a338540f9b7ecb46fb3825ad662da852856257829c3664ce84d02bfcdda81c",
            },
            OpVector {
                name: "autoCancel clear",
                // ["autoCancel", [0]]
                op: |w| {
                    w.array(2);
                    w.str(b"autoCancel");
                    w.array(1);
                    w.uint(0);
                },
                salt: 12345,
                timestamp: 1751500000000,
                data: "0x569d0f5365d9bb75e7f2218717986828ea553b937a4c901528c70fa173fa539e",
                signature: "0x188bebb866e6f11dd439ea12e2869eb9308fb6a9c6c9da09ece8a74467d7c4ac
                            582c4a9929094af1d994467e40c81143de007b6b1499286b265512fdde82b29a1c",
            },
            OpVector {
                name: "updateMargin",
                // ["updateMargin", [3, "-25.000000000000000001"]]
                op: |w| {
                    w.array(2);
                    w.str(b"updateMargin");
                    w.array(2);
                    w.uint(3);
                    w.str(b"-25.000000000000000001");
                },
                salt: 8,
                timestamp: 1751500000006,
                data: "0x1bceee4cd50ae288f5ae7e993deeea51fe9dd41b5eff27840455ed72e72d4f4c",
                signature: "0xfd6ec2ee47423b3f87adfc64eadd9764180e05102efadb112403016a446d072d
                            497ec201b2df1e7944f465c9c734d3698b80dbe51a42eb8167a7a54e4249dfc81b",
            },
            OpVector {
                name: "deleteProxy",
                // ["deleteProxy", ["0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"]]: 42
                // characters, so a str 8
                op: |w| {
                    w.array(2);
                    w.str(b"deleteProxy");
                    w.array(1);
                    w.str(b"0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc");
                },
                salt: 55,
                timestamp: 1751500000006,
                data: "0x7274223decc60adbbe3ba91ceade2e6cd7901ab7ba506d07fb2d0dbef83e30d4",
                signature: "0x2f6400725b8af890a69b8c47fe9d2b100d15714a73a13a92b15187909d1c1080
                            51dc906b8ec65cfc4d161c579df7b2cb9c6f7efea30dc238cfd7526fd3b797c01c",
            },
        ]
    }

    /// Signs `digest` with the golden key and checks the result is `golden` (`r || s || v`)
    /// byte for byte (RFC 6979 makes signing deterministic), then that every verifier this
    /// build has recovers the golden address from it: `k256`, and libsecp256k1 in a build
    /// with the `c-secp256k1` feature.
    fn check_golden_signature(name: &str, digest: &[u8; 32], golden: &str) {
        let golden = hex(golden);
        assert_eq!(golden.len(), 65, "{name}: r, s and v");
        let (signature, recovery_id) = sign_recoverable(&golden_key(), digest);
        assert_eq!(signature[..], golden[..64], "{name}: r || s");
        assert_eq!(27 + recovery_id, golden[64], "{name}: v");
        for verifier in VerifierKind::built() {
            let recovered = verifier.recover(digest, &signature, recovery_id);
            assert_eq!(recovered, Some(golden_address()), "{name}: recovered by {verifier}");
        }
    }

    #[test]
    fn the_golden_keys_address() {
        assert_eq!(address_of(&uncompressed(&golden_key())), golden_address());
        let compressed = PublicKey::K256(*golden_key().verifying_key()).to_compressed();
        for verifier in VerifierKind::built() {
            let key = PublicKey::from_compressed(verifier, &compressed).expect("a point");
            assert_eq!(key.address(), golden_address(), "{verifier}");
        }
    }

    #[test]
    fn polymarkets_ten_golden_operations() {
        let domain = Domain::new(GOLDEN_CHAIN_ID);
        for vector in golden_op_vectors() {
            let data = keccak256(&written(vector.op));
            assert_eq!(data[..], hex(vector.data), "{}: data", vector.name);
            let digest = digest(&domain, &data, vector.salt, vector.timestamp);
            check_golden_signature(vector.name, &digest, vector.signature);
        }
    }

    #[test]
    fn polymarkets_two_golden_owner_signed_messages() {
        // Not operations: struct types of their own, built as the py-sdk's
        // `build_perps_create_proxy_typed_data` and `build_perps_withdraw_typed_data` list
        // their fields (src/polymarket/_internal/actions/perps/signing.py), in EIP-712's
        // type-string form. CreateProxy uses the operations' domain; Withdraw's adds the
        // deposit contract as `verifyingContract`, and its `ts` is in seconds.
        let create_proxy = hash_struct(
            "CreateProxy(address addr,uint64 exp,uint64 salt,uint64 ts)",
            &[
                address_word("0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"),
                word(1752000000000),
                word(4242),
                word(1751500000007),
            ],
        );
        check_golden_signature(
            "CreateProxy",
            &sign_hash(Domain::new(GOLDEN_CHAIN_ID).separator(), &create_proxy),
            "0x64e2d5aef2aeb5e58072ebb63f8419ba0f89d1de34635a007ef648db9e6ac6f3
             01997797def6e6a5b118f09fb1da0e3f58a55c34a871968e13227ae40eabf9341c",
        );

        let withdraw_domain = hash_struct(
            "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
            &[
                keccak256(b"Polymarket"),
                keccak256(b"1"),
                word(GOLDEN_CHAIN_ID),
                address_word("0xDCa4af75705dbB50f62437045afF9921947917d2"),
            ],
        );
        let withdraw = hash_struct(
            "Withdraw(address account,address token,uint256 amount,uint256 fee,address to,uint64 salt,uint64 ts)",
            &[
                address_word("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
                address_word("0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB"),
                word(10_000_000),
                word(0),
                address_word("0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"),
                word(77),
                word(1751500000),
            ],
        );
        check_golden_signature(
            "Withdraw",
            &sign_hash(&withdraw_domain, &withdraw),
            "0x75b6d0d9572921ed71d2a13fd3585dbd34326d9b503edaca08e3bf72dcd7bb43
             7fe67378df001fc9e4352063a87d9e0a690599bf0ed6849ec00660e6f28cfa1c1b",
        );
    }

    #[test]
    fn the_typescript_sdks_data_vectors() {
        // ts-sdk packages/client/src/websockets/perps/actions/trading.test.ts,
        // CREATE_ORDER_DATA_HASH and UPDATE_MARGIN_DATA_HASH ("backend-compatible" bytes).
        // The price keeps its trailing zero: a client's strings are hashed as sent.
        let create_order: fn(&mut Writer) = |w| {
            w.array(2);
            w.str(b"createOrders");
            w.array(1);
            w.array(6);
            w.uint(1);
            w.bool(true);
            w.str(b"100.50");
            w.str(b"10");
            w.str(b"gtc");
            w.bool(false);
        };
        assert_eq!(
            keccak256(&written(create_order))[..],
            hex("0x817207b7b8b31044a8f27e43c16e24d9fd5e11d3f106feb962f104f3ef28d52a")
        );
        let update_margin: fn(&mut Writer) = |w| {
            w.array(2);
            w.str(b"updateMargin");
            w.array(2);
            w.uint(7);
            w.str(b"-1234567890.123456789012345678");
        };
        assert_eq!(
            keccak256(&written(update_margin))[..],
            hex("0xf61d7d83b4367ce136bf66cfee6a5d41303a8b2d8724f5458f9812c46f5e55b3")
        );
    }

    #[test]
    fn the_eip712_specifications_mail_example() {
        // ethereum/EIPs, assets/eip-712/Example.js: every value below is one its test
        // asserts. Nested structs and a verifying contract, which Polymarket's operations
        // don't use, so this checks the EIP-712 encoding rules themselves.
        let mail_type = "Mail(Person from,Person to,string contents)Person(string name,address wallet)";
        assert_eq!(
            keccak256(mail_type.as_bytes())[..],
            hex("0xa0cedeb2dc280ba39b857546d74f5549c3a1d7bdc2dd96bf881f76108e23dac2")
        );
        let person = |name: &str, wallet: &str| {
            hash_struct(
                "Person(string name,address wallet)",
                &[keccak256(name.as_bytes()), address_word(wallet)],
            )
        };
        let mail = [
            keccak256(mail_type.as_bytes()),
            person("Cow", "0xCD2a3d9F938E13CD947Ec05AbC7FE734Df8DD826"),
            person("Bob", "0xbBbBBBBbbBBBbbbBbbBbbbbBBbBbbbbBbBbbBBbB"),
            keccak256(b"Hello, Bob!"),
        ];
        assert_eq!(
            mail.as_flattened(),
            hex("0xa0cedeb2dc280ba39b857546d74f5549c3a1d7bdc2dd96bf881f76108e23dac2
                 fc71e5fa27ff56c350aa531bc129ebdf613b772b6604664f5d8dbe21b85eb0c8
                 cd54f074a4af31b4411ff6a60c9719dbd559c221c8ac3492d9d872b041d703d1
                 b5aadf3154a261abdd9086fc627b61efca26ae5702701d05cd2305f7c52a2fc8"),
            "encodeData"
        );
        let struct_hash = keccak256(mail.as_flattened());
        assert_eq!(
            struct_hash[..],
            hex("0xc52c0ee5d84264471806290a3f2c4cecfc5490626bf912d01f240d7a274b371e")
        );
        let separator = hash_struct(
            "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
            &[
                keccak256(b"Ether Mail"),
                keccak256(b"1"),
                word(1),
                address_word("0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC"),
            ],
        );
        assert_eq!(separator[..], hex("0xf2cee375fa42b42143804025fc449deafd50cc031ca257e0b194a650a912090f"));
        let signed = sign_hash(&separator, &struct_hash);
        assert_eq!(signed[..], hex("0xbe609aee343fb3c4b28e1df9e632fca64fcfaede20f02e86244efddf30957bd2"));

        // The signer: the private key keccak256("cow"), its address, and its signature of
        // the hash (v 28, so recovery id 1).
        let key = SigningKey::from_slice(&keccak256(b"cow")).expect("a valid private key");
        let address: Address =
            hex("0xcd2a3d9f938e13cd947ec05abc7fe734df8dd826").try_into().expect("20 bytes");
        assert_eq!(address_of(&uncompressed(&key)), address);
        let (signature, recovery_id) = sign_recoverable(&key, &signed);
        assert_eq!(
            signature[..32],
            hex("0x4355c47d63924e8a72e509b65029052eb6c299d53a04e167c5775fd466751c9d")
        );
        assert_eq!(
            signature[32..],
            hex("0x07299936d304c153f6443dfa05f40ff007d72911b6f72307f996231605b91562")
        );
        assert_eq!(27 + recovery_id, 28);
        for verifier in VerifierKind::built() {
            assert_eq!(verifier.recover(&signed, &signature, recovery_id), Some(address), "{verifier}");
        }
    }

    // ---- Our pieces ----

    #[test]
    fn the_typehashes_are_the_keccak_of_their_type_strings() {
        assert_eq!(keccak256(DOMAIN_TYPE.as_bytes()), DOMAIN_TYPEHASH);
        assert_eq!(keccak256(OP_TYPE.as_bytes()), OP_TYPEHASH);
    }

    #[test]
    fn the_domain_separator_and_the_digest_follow_eip712() {
        // The same hashes, spelled out with the test's own `hash_struct`.
        for chain_id in [1, 137, 31_337, u64::from(u32::MAX)] {
            let domain = Domain::new(chain_id);
            assert_eq!(domain.chain_id(), chain_id);
            let separator =
                hash_struct(DOMAIN_TYPE, &[keccak256(b"Polymarket"), keccak256(b"1"), word(chain_id)]);
            assert_eq!(domain.separator(), &separator, "chain {chain_id}");
            let data = keccak256(b"an operation");
            let op = hash_struct(OP_TYPE, &[data, word(12_345), word(1_751_500_000_000)]);
            assert_eq!(digest(&domain, &data, 12_345, 1_751_500_000_000), sign_hash(&separator, &op));
        }
        // Every input changes the digest: the chain (so the deployment), the data, the
        // salt and the timestamp.
        let (domain, data) = (Domain::new(1), keccak256(b"an operation"));
        let reference = digest(&domain, &data, 5, 6);
        assert_ne!(digest(&Domain::new(2), &data, 5, 6), reference);
        assert_ne!(digest(&domain, &keccak256(b"another"), 5, 6), reference);
        assert_ne!(digest(&domain, &data, 6, 6), reference);
        assert_ne!(digest(&domain, &data, 5, 7), reference);
    }

    #[test]
    fn prices_and_sizes_are_written_in_minimal_form() {
        for (value, decimals, text) in [
            (50, PRICE_DECIMALS, "0.5"),
            (100_000, QTY_DECIMALS, "10"),
            (0, PRICE_DECIMALS, "0"),
            (1, QTY_DECIMALS, "0.0001"),
            (10, PRICE_DECIMALS, "0.1"),
            (100, PRICE_DECIMALS, "1"),
            (102_998, PRICE_DECIMALS, "1029.98"),
            (250_000, QTY_DECIMALS, "25"),
            (123_450, QTY_DECIMALS, "12.345"),
            (10_203, QTY_DECIMALS, "1.0203"),
            (7, 0, "7"),
            (-25, PRICE_DECIMALS, "-0.25"),
            (-100, PRICE_DECIMALS, "-1"),
            (i64::MAX, PRICE_DECIMALS, "92233720368547758.07"),
            (i64::MIN, QTY_DECIMALS, "-922337203685477.5808"),
            (i64::MIN, 19, "-0.9223372036854775808"),
            (i64::MIN, 0, "-9223372036854775808"),
        ] {
            let decimal = Decimal::new(value, decimals);
            assert_eq!(decimal.as_bytes(), text.as_bytes(), "{value} with {decimals} decimals");
        }
    }

    #[test]
    fn an_order_id_is_32_lowercase_hex_digits() {
        assert_eq!(&order_id_hex(order_id(acct(9), OrderSeq::new(1))), b"00000000000000000000000900000001");
        assert_eq!(&order_id_hex(OrderId::new(0x0123_4567_89AB_CDEF)), b"00000000000000000123456789abcdef");
        assert_eq!(&order_id_hex(OrderId::new(u64::MAX)), b"0000000000000000ffffffffffffffff");
    }

    #[test]
    fn a_place_is_golden_vector_1_with_its_order_id() {
        // Golden vector 1 is ["createOrders", [[1, true, "0.5", "10", "gtc", false]]]: on
        // market 1, a GTC buy of 10 at 0.5, not post-only, which is 50 ticks and 100,000
        // lots with our decimals. These are its bytes (msgpack.rs checks them too):
        let golden_1 = hex("92 ac 6372656174654f7264657273 91 96 01 c3 a3 302e35 a2 3130 a3 677463 c2");
        assert_eq!(keccak256(&golden_1)[..], hex(golden_op_vectors()[0].data));
        let place = Command::PlaceOrder(PlaceOrder {
            order_id: order_id(acct(9), OrderSeq::new(1)),
            price: Price::new(50),
            qty: Qty::new(100_000),
            market: MarketId::new(1),
            side: Side::Buy,
            tif: TimeInForce::Gtc,
            post_only: false,
        });
        // Ours always has `c` too: 7 fields (the order's header 0x96 becomes 0x97), the last
        // a str 8 of 32 characters.
        let mut expected = golden_1.clone();
        expected[15] = 0x97;
        expected.extend_from_slice(&[0xD9, 0x20]);
        expected.extend_from_slice(b"00000000000000000000000900000001");
        let mut buffer = [0; MAX_OP_BYTES];
        assert_eq!(encode_op(&place, &mut buffer), Some(&expected[..]));
        assert_eq!(op_data(&place), Some(keccak256(&expected)));
    }

    #[test]
    fn a_place_writes_each_field_in_its_place() {
        // A post-only IOC sell on market 300 (whether an order makes sense is the engine's
        // business; the form only spells it).
        let place = Command::PlaceOrder(PlaceOrder {
            order_id: order_id(acct(0x0102_0304), OrderSeq::new(0xA0B0_C0D0)),
            price: Price::new(102_998),
            qty: Qty::new(500_000),
            market: MarketId::new(300),
            side: Side::Sell,
            tif: TimeInForce::Ioc,
            post_only: true,
        });
        let expected = written(|w| {
            w.array(2);
            w.str(b"createOrders");
            w.array(1);
            w.array(7);
            w.uint(300);
            w.bool(false);
            w.str(b"1029.98");
            w.str(b"50");
            w.str(b"ioc");
            w.bool(true);
            w.str(b"000000000000000001020304a0b0c0d0");
        });
        let mut buffer = [0; MAX_OP_BYTES];
        assert_eq!(encode_op(&place, &mut buffer), Some(&expected[..]));
    }

    #[test]
    fn a_cancel_is_golden_vector_5_with_our_order_id() {
        let cancel = Command::CancelOrder(CancelOrder {
            order_id: order_id(acct(9), OrderSeq::new(1)),
            market: MarketId::new(3),
        });
        let mut expected = hex("92 b0");
        expected.extend_from_slice(b"cancelOrdersCOID");
        expected.extend_from_slice(&[0x91, 0xD9, 0x20]);
        expected.extend_from_slice(b"00000000000000000000000900000001");
        let mut buffer = [0; MAX_OP_BYTES];
        assert_eq!(encode_op(&cancel, &mut buffer), Some(&expected[..]));
        // Golden vector 5 cancels "aabbccddeeff00112233445566778899", a 128-bit id ours
        // can't be. With it in place of ours, the bytes are golden vector 5's.
        let mut golden_5 = expected.clone();
        golden_5[expected.len() - 32..].copy_from_slice(b"aabbccddeeff00112233445566778899");
        assert_eq!(keccak256(&golden_5)[..], hex(golden_op_vectors()[4].data));
        // The market is not signed (module docs).
        let elsewhere = Command::CancelOrder(CancelOrder {
            order_id: order_id(acct(9), OrderSeq::new(1)),
            market: MarketId::new(7),
        });
        assert_eq!(op_data(&elsewhere), op_data(&cancel));
    }

    #[test]
    fn a_modify_names_the_order_and_its_new_price_and_total_size() {
        let modify = Command::ModifyOrder(ModifyOrder {
            order_id: order_id(acct(9), OrderSeq::new(1)),
            new_price: Price::new(103_001),
            new_size: Qty::new(250_000),
            market: MarketId::new(3),
        });
        let mut expected = hex("92 b0");
        expected.extend_from_slice(b"modifyOrdersCOID");
        expected.extend_from_slice(&[0x91, 0x93, 0xD9, 0x20]);
        expected.extend_from_slice(b"00000000000000000000000900000001");
        expected.push(0xA7); // a fixstr of 7 bytes: 103,001 ticks
        expected.extend_from_slice(b"1030.01");
        expected.push(0xA2); // a fixstr of 2 bytes: 250,000 lots
        expected.extend_from_slice(b"25");
        let mut buffer = [0; MAX_OP_BYTES];
        assert_eq!(encode_op(&modify, &mut buffer), Some(&expected[..]));
        let elsewhere = Command::ModifyOrder(ModifyOrder {
            order_id: order_id(acct(9), OrderSeq::new(1)),
            new_price: Price::new(103_001),
            new_size: Qty::new(250_000),
            market: MarketId::new(4),
        });
        assert_eq!(op_data(&elsewhere), op_data(&modify), "the market is not signed");
    }

    #[test]
    fn the_longest_form_fills_max_op_bytes_exactly() {
        let longest = Command::PlaceOrder(PlaceOrder {
            order_id: OrderId::new(u64::MAX),
            price: Price::new(i64::MIN),
            qty: Qty::new(i64::MIN),
            market: MarketId::new(u16::MAX),
            side: Side::Sell,
            tif: TimeInForce::Gtc,
            post_only: true,
        });
        let mut buffer = [0; MAX_OP_BYTES];
        assert_eq!(encode_op(&longest, &mut buffer).map(<[u8]>::len), Some(MAX_OP_BYTES));
        let modify = Command::ModifyOrder(ModifyOrder {
            order_id: OrderId::new(u64::MAX),
            new_price: Price::new(i64::MIN),
            new_size: Qty::new(i64::MIN),
            market: MarketId::new(u16::MAX),
        });
        assert!(encode_op(&modify, &mut buffer).is_some_and(|op| op.len() < MAX_OP_BYTES));
    }

    #[test]
    fn operator_commands_have_no_form() {
        for operator in [
            Command::Deposit(Deposit { amount: Micros::new(1), account: acct(9) }),
            Command::SetMark(SetMark { price: Price::new(1), market: MarketId::new(3) }),
        ] {
            let mut buffer = [0; MAX_OP_BYTES];
            assert_eq!(encode_op(&operator, &mut buffer), None);
            assert_eq!(op_data(&operator), None);
        }
    }
}
