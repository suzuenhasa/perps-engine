//! Making the memory the engine reserved resident before a run (`docs/PIPELINE.md` 15.4;
//! `Engine::prefault`).
//!
//! **Why.** The engine reserves its capacity up front (`EngineOptions`), so that commands
//! don't allocate. But reserving is not touching: a fresh allocation is only address space,
//! and the operating system maps each 4 KiB page the first time something writes to it (a
//! minor page fault, about a microsecond on the thread that writes). hashbrown, std's
//! `HashMap`, writes only its control bytes when it allocates, so every new id that lands
//! on an untouched bucket page takes a fault, and new (account, market) pairs and new order
//! ids keep landing on fresh pages long into a run. Writing every page once, before the run,
//! moves those faults out of the measured window.
//!
//! **Contract.** These helpers write memory the engine owns but doesn't use yet, and leave
//! every value it uses as it was. Nothing they touch is ever read before it is written
//! again: a `Vec`'s spare capacity lies past its length, and a map's buckets are read only
//! where a control byte says they are full. They allocate only to rebuild a map that
//! already holds entries ([`touch_map`]), which the core thread does before its first
//! command, never on a command's path.
//!
//! **Complexity.** Linear in the memory reserved: a few hundred MB for the M3 flow's 67
//! markets, well under a second, once per run.

use std::hash::Hash;

use crate::id_hash::IdMap;

/// Writes `vec`'s spare capacity once, with `filler`, and leaves its length and contents as
/// they were. No allocation: the length never passes the capacity.
pub(crate) fn touch_spare<T: Clone>(vec: &mut Vec<T>, filler: T) {
    let len = vec.len();
    vec.resize(vec.capacity(), filler);
    // The writes above are to memory that is dropped again at once: `black_box` makes the
    // compiler keep them.
    std::hint::black_box(vec.as_slice());
    vec.truncate(len);
}

/// Writes every element of `slice` with its own value: for memory allocated as zeros
/// (`vec![0; n]`), which the operating system may hand out as untouched pages. The value
/// passes through `black_box`, so the compiler can't drop the write as storing what is
/// already there.
pub(crate) fn touch_all<T: Copy>(slice: &mut [T]) {
    for element in slice {
        *element = std::hint::black_box(*element);
    }
}

/// Writes every bucket page of `map`'s table, and leaves the map holding exactly what it
/// held, with the same capacity and hasher.
///
/// - An empty map is filled to its capacity with distinct dummy keys (`dummy_key(0)`,
///   `dummy_key(1)`, ...) and then cleared. `clear` keeps the table and resets every
///   control byte, so the table is then exactly a fresh one: the same state as before, as
///   far as any later insert or lookup can tell. Filling to `capacity()` never grows it.
/// - A map that already holds entries (a replayed engine's) is rebuilt: a new table of the
///   same capacity is touched as above, and the entries move into it. Where an entry sits in
///   the table may change, but the engine never iterates its maps (`id_hash.rs`), so
///   nothing it emits or holds can. This allocates the new table and frees the old one.
pub(crate) fn touch_map<K: Eq + Hash, V>(
    map: &mut IdMap<K, V>,
    dummy_key: impl Fn(usize) -> K,
    dummy_value: impl Fn() -> V,
) {
    if !map.is_empty() {
        let mut fresh = IdMap::with_capacity_and_hasher(map.capacity(), *map.hasher());
        fill_and_clear(&mut fresh, &dummy_key, &dummy_value);
        fresh.extend(map.drain());
        *map = fresh;
    } else {
        fill_and_clear(map, &dummy_key, &dummy_value);
    }
}

/// The empty-map case of [`touch_map`].
fn fill_and_clear<K: Eq + Hash, V>(
    map: &mut IdMap<K, V>,
    dummy_key: &impl Fn(usize) -> K,
    dummy_value: &impl Fn() -> V,
) {
    debug_assert!(map.is_empty());
    for i in 0..map.capacity() {
        map.insert(dummy_key(i), dummy_value());
    }
    map.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id_hash::IdBuildHasher;

    #[test]
    fn a_touched_vec_keeps_its_length_contents_and_capacity() {
        let mut vec = Vec::with_capacity(1_000);
        vec.extend([3u64, 1, 4]);
        touch_spare(&mut vec, 9);
        assert_eq!((vec.as_slice(), vec.capacity()), (&[3, 1, 4][..], 1_000));
        let mut zeros = vec![0u64; 10];
        touch_all(&mut zeros);
        assert_eq!(zeros, [0; 10]);
    }

    #[test]
    fn a_touched_map_keeps_its_entries_capacity_and_hasher() {
        let hasher = IdBuildHasher::new(7);
        let mut empty: IdMap<u64, u32> = IdMap::with_capacity_and_hasher(100, hasher);
        let capacity = empty.capacity();
        touch_map(&mut empty, |i| i as u64, || 0);
        assert!(empty.is_empty());
        assert_eq!(empty.capacity(), capacity, "not grown");

        let mut held: IdMap<u64, u32> = IdMap::with_capacity_and_hasher(100, hasher);
        held.extend([(5, 50), (1_000_000, 1)]);
        touch_map(&mut held, |i| i as u64, || 0);
        assert_eq!((held.len(), held.get(&5), held.get(&1_000_000)), (2, Some(&50), Some(&1)));
        assert_eq!(held.capacity(), capacity, "the same capacity");
        assert_eq!(held.get(&0), None, "no dummy key is left behind");
    }
}
