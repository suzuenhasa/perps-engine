//! The hash function for the book's maps keyed by order id or account id.
//!
//! **Why not std's default.** `HashMap`'s default hasher (SipHash with a random key) is
//! slower than needed for 8-byte keys, and it draws its key from the operating system's
//! randomness, which the engine promises never to touch (crate docs, "Contract").
//!
//! **Why seeded.** Order ids are chosen by clients. With a fixed, public hash function, a
//! client could pick ids that all land in the same bucket, turning each O(1) lookup into a
//! walk along that bucket's probe sequence (hash flooding). Mixing a secret seed into
//! every hash stops them computing such ids in advance. In production the seed is a
//! secret per deployment, recorded in the journal header (Milestone 3): a replay rebuilds
//! exactly the same maps, while clients can't predict where their ids land.
//!
//! **The function.** `splitmix64_finaliser(key ^ seed)`: two multiplies and three
//! shift-xors, after which every input bit affects every output bit. So ids with a lot of
//! structure (account in the high 32 bits, a small counter in the low 32) still spread
//! over all buckets. For a fixed seed it is a bijection on `u64`, so two different keys
//! never share a full 64-bit hash.
//!
//! **Not a MAC.** This is not a keyed cryptographic hash like SipHash. Someone who could
//! learn enough about the seed (by timing a great many requests, say) could still aim
//! collisions. It turns flooding from free into hard, at a much lower cost per lookup.
//!
//! **Determinism.** The engine never iterates these maps, so the hash (and the seed) can
//! only change how fast the book runs, never which events it emits.

use std::collections::HashMap;
use std::fmt;
use std::hash::{BuildHasher, Hasher};

/// A `HashMap` keyed by an integer id, hashed with [`IdHasher`].
pub type IdMap<K, V> = HashMap<K, V, IdBuildHasher>;

/// Makes [`IdHasher`]s that all use the same seed. This is the map's "hasher state".
#[derive(Clone, Copy)]
pub struct IdBuildHasher {
    seed: u64,
}

/// Leaves the seed out, so debug output and logs can't leak it.
impl fmt::Debug for IdBuildHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdBuildHasher").finish_non_exhaustive()
    }
}

impl IdBuildHasher {
    pub fn new(seed: u64) -> Self {
        IdBuildHasher { seed }
    }
}

impl BuildHasher for IdBuildHasher {
    type Hasher = IdHasher;

    fn build_hasher(&self) -> IdHasher {
        IdHasher { state: self.seed }
    }
}

/// Hashes one integer key to `splitmix64_finaliser(key ^ seed)`. See the module docs.
#[derive(Clone, Copy)]
pub struct IdHasher {
    /// Starts as the seed; each write mixes a value into it.
    state: u64,
}

/// Leaves the state out: before the first write it *is* the seed.
impl fmt::Debug for IdHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdHasher").finish_non_exhaustive()
    }
}

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.state
    }

    /// How `u64` keys (order ids) are hashed.
    fn write_u64(&mut self, value: u64) {
        self.state = splitmix64_finaliser(self.state ^ value);
    }

    /// How `u32` keys (account ids) are hashed.
    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    /// Fallback for any other key type: one byte at a time. Slow, but the book's keys are
    /// all integers and never come here.
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }
}

/// The output step of the splitmix64 generator (Steele, Lea and Flood, 2014): a fixed
/// scramble of 64 bits. The same three lines end `loadgen::SplitMix64::next_u64`.
fn splitmix64_finaliser(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AccountId, OrderId, OrderSeq, order_id};
    use std::collections::HashSet;

    /// splitmix64's generator adds this constant to its state before each output.
    const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

    #[test]
    fn the_hash_is_the_splitmix64_finaliser_of_key_xor_seed() {
        // The reference C implementation's first output for seed 1234567 is the finaliser
        // of 1234567 + GOLDEN_GAMMA (loadgen checks the same value).
        let x = 1_234_567u64.wrapping_add(GOLDEN_GAMMA);
        let expected = 6_457_827_717_110_365_317;
        assert_eq!(IdBuildHasher::new(0).hash_one(x), expected);
        // With a seed, the key is xored with it first.
        let seed = 0xDEAD_BEEF_1234_5678;
        assert_eq!(IdBuildHasher::new(seed).hash_one(x ^ seed), expected);
    }

    #[test]
    fn account_ids_hash_like_the_same_value_as_a_u64() {
        let hasher = IdBuildHasher::new(42);
        assert_eq!(hasher.hash_one(AccountId::new(7)), hasher.hash_one(7u64));
        assert_eq!(hasher.hash_one(OrderId::new(7)), hasher.hash_one(7u64));
    }

    #[test]
    fn the_seed_changes_every_hash() {
        let (a, b) = (IdBuildHasher::new(1), IdBuildHasher::new(2));
        for seq in 1..1_000 {
            let id = order_id(AccountId::new(5), OrderSeq::new(seq));
            assert_ne!(a.hash_one(id), b.hash_one(id), "id {:#x}", id.get());
        }
    }

    /// How many of 1,024 buckets these ids land in, for a table that picks the bucket from
    /// the hash's low 10 bits.
    fn buckets_used(ids: impl Iterator<Item = OrderId>) -> usize {
        let hasher = IdBuildHasher::new(0x1234);
        ids.map(|id| hasher.hash_one(id) % 1_024).collect::<HashSet<_>>().len()
    }

    #[test]
    fn structured_ids_spread_over_the_buckets() {
        // 1,024 ids that differ only in the low bits (one account's sequence numbers), and
        // 1,024 that differ only in the high bits (the first order of many accounts).
        // Random hashes would fill about 647 distinct buckets (1,024 * (1 - 1/e)); the
        // identity hash would put all of the second set in bucket 1.
        let one_account = buckets_used((0..1_024).map(|seq| order_id(AccountId::new(9), OrderSeq::new(seq))));
        let many_accounts =
            buckets_used((0..1_024).map(|account| order_id(AccountId::new(account), OrderSeq::new(1))));
        assert!(one_account > 580, "one account's ids use only {one_account} buckets");
        assert!(many_accounts > 580, "many accounts' ids use only {many_accounts} buckets");
    }

    #[test]
    fn debug_output_does_not_show_the_seed() {
        let seed = 0xDEAD_BEEF_1234_5678u64;
        let builder = IdBuildHasher::new(seed);
        for text in [format!("{builder:?}"), format!("{:?}", builder.build_hasher())] {
            assert!(!text.contains(&seed.to_string()) && !text.to_lowercase().contains("deadbeef"), "{text}");
        }
    }
}
