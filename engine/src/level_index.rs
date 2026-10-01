//! Which price levels hold orders, as a hierarchical bitmap. The book keeps one per side, to
//! find the next non-empty level without scanning every level (INFO.md section 4, "Order
//! book").
//!
//! **Structure.** Layer 0 has one bit per level, set while that level holds orders. Each
//! layer above summarises the one below it: bit `j` of layer `k + 1` is set iff word `j` of
//! layer `k` is non-zero. Words are `u64`, so each layer is 64 times shorter than the one
//! below, and the top layer is a single word. For the widest range the book accepts, 2^24
//! levels, the layers have 262,144, 4,096, 64 and 1 words: about 2 MiB.
//!
//! **Searching.** To find the first set bit at or above `i`, look in the rest of `i`'s own
//! word. If there is none, go up a layer and look, the same way, for the next non-empty
//! word after it. Once a set bit is found, come back down: at each layer below, take the
//! lowest set bit (`trailing_zeros`) of the word the bit above points to. Searching
//! downwards is the mirror image, with `leading_zeros`.
//!
//! **Invariant.** Bit `j` of layer `k + 1` is set iff word `j` of layer `k` is non-zero.
//! `set` and `clear` keep it by walking up only while a word changes between zero and
//! non-zero.
//!
//! **Complexity.** Every operation reads or writes at most one word per layer on the way
//! up, and a search at most one more per layer on the way down. There are at most 4 layers
//! for up to 2^24 levels (3 for up to 262,144).

/// A set of level numbers in `0..len`, with fast "next level in the set" searches in both
/// directions. See the module docs.
#[derive(Clone, Debug)]
pub struct LevelIndex {
    /// `layers[0]` has one bit per level; each later layer has one bit per word of the
    /// layer below it. The last layer is exactly one word.
    layers: Vec<Vec<u64>>,
    /// Number of levels.
    len: usize,
}

impl LevelIndex {
    /// An empty index over levels `0..len`.
    pub fn new(len: usize) -> Self {
        assert!(len > 0, "a level index needs at least one level");
        let mut layers = Vec::new();
        let mut bits = len;
        loop {
            let words = bits.div_ceil(64);
            layers.push(vec![0; words]);
            if words == 1 {
                break;
            }
            // The next layer has one bit per word of this one.
            bits = words;
        }
        LevelIndex { layers, len }
    }

    /// Writes every word once, keeping its value (`crate::prefault`): the layers are
    /// allocated as zeros, which the operating system may hand out as untouched pages.
    pub fn prefault(&mut self) {
        for layer in &mut self.layers {
            crate::prefault::touch_all(layer);
        }
    }

    /// True if level `i` is in the set.
    pub fn contains(&self, i: usize) -> bool {
        i < self.len && self.layers[0][i / 64] & bit(i) != 0
    }

    /// Adds level `i`.
    pub fn set(&mut self, i: usize) {
        debug_assert!(i < self.len, "level {i} is outside 0..{}", self.len);
        let mut index = i;
        for layer in &mut self.layers {
            let word = &mut layer[index / 64];
            let was_empty = *word == 0;
            *word |= bit(index);
            // A word that already had a bit set is already marked in the layer above.
            if !was_empty {
                break;
            }
            index /= 64;
        }
    }

    /// Removes level `i`.
    pub fn clear(&mut self, i: usize) {
        debug_assert!(i < self.len, "level {i} is outside 0..{}", self.len);
        let mut index = i;
        for layer in &mut self.layers {
            let word = &mut layer[index / 64];
            *word &= !bit(index);
            // A word with other bits still set stays marked in the layer above.
            if *word != 0 {
                break;
            }
            index /= 64;
        }
    }

    /// The lowest level in the set that is `>= i`, if any.
    pub fn first_at_or_above(&self, i: usize) -> Option<usize> {
        if i >= self.len {
            return None;
        }
        let mut index = i;
        for (depth, layer) in self.layers.iter().enumerate() {
            let word_number = index / 64;
            // Past the last word of this layer: there is nothing further on.
            let word = *layer.get(word_number)?;
            let from_index = word & (u64::MAX << (index % 64));
            if from_index != 0 {
                let found = word_number * 64 + from_index.trailing_zeros() as usize;
                return Some(self.lowest_under(depth, found));
            }
            // Nothing left in this word, so look from the following word, one layer up.
            index = word_number + 1;
        }
        None
    }

    /// The highest level in the set that is `<= i`, if any. `i` may be past the last
    /// level, in which case every level counts as below it.
    pub fn last_at_or_below(&self, i: usize) -> Option<usize> {
        let mut index = i.min(self.len - 1);
        for (depth, layer) in self.layers.iter().enumerate() {
            let word_number = index / 64;
            let up_to_index = layer[word_number] & (u64::MAX >> (63 - index % 64));
            if up_to_index != 0 {
                let found = word_number * 64 + 63 - up_to_index.leading_zeros() as usize;
                return Some(self.highest_under(depth, found));
            }
            // Nothing at or below in this word, so look from the previous word, one layer
            // up. If this was the first word, there is nothing below.
            index = word_number.checked_sub(1)?;
        }
        None
    }

    /// Follows set bit `index` of layer `depth` down to layer 0, taking the lowest set bit
    /// at each step. Bit `index` of a layer marks word `index` of the layer below, whose
    /// lowest set bit is at position `index * 64 + trailing_zeros`.
    fn lowest_under(&self, depth: usize, mut index: usize) -> usize {
        for layer in self.layers[..depth].iter().rev() {
            index = index * 64 + layer[index].trailing_zeros() as usize;
        }
        index
    }

    /// Like [`Self::lowest_under`], taking the highest set bit at each step.
    fn highest_under(&self, depth: usize, mut index: usize) -> usize {
        for layer in self.layers[..depth].iter().rev() {
            index = index * 64 + 63 - layer[index].leading_zeros() as usize;
        }
        index
    }
}

/// The bit for position `i` within its 64-bit word.
fn bit(i: usize) -> u64 {
    1 << (i % 64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Positions where a word (64), a layer-1 word (4,096) or a layer-2 word (262,144)
    /// begins or ends.
    const BOUNDARIES: [usize; 9] = [0, 63, 64, 65, 4_095, 4_096, 4_097, 262_143, 262_144];

    /// The largest index the book uses: 2^24 levels, four layers.
    const LEN: usize = 1 << 24;

    /// Checks the invariant from the module docs on every word.
    fn assert_layers_agree(index: &LevelIndex) {
        for (k, pair) in index.layers.windows(2).enumerate() {
            let (below, above) = (&pair[0], &pair[1]);
            for (j, &word) in below.iter().enumerate() {
                let marked = above[j / 64] & bit(j) != 0;
                assert_eq!(marked, word != 0, "layer {} bit {j} disagrees with layer {k} word {j}", k + 1);
            }
        }
    }

    /// A number in `0..n`, from a tiny deterministic generator (xorshift64), so the
    /// randomized test needs no dependency and always runs the same operations.
    fn random_below(state: &mut u64, n: usize) -> usize {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state % n as u64) as usize
    }

    #[test]
    fn layer_count_grows_by_one_every_64_times_more_levels() {
        let layers = |len| LevelIndex::new(len).layers.len();
        assert_eq!(layers(1), 1);
        assert_eq!(layers(64), 1);
        assert_eq!(layers(65), 2);
        assert_eq!(layers(4_096), 2);
        assert_eq!(layers(4_097), 3);
        assert_eq!(layers(262_144), 3);
        assert_eq!(layers(262_145), 4);
        assert_eq!(layers(LEN), 4);
    }

    #[test]
    fn an_empty_index_finds_nothing() {
        let index = LevelIndex::new(LEN);
        for i in BOUNDARIES.into_iter().chain([LEN - 1]) {
            assert!(!index.contains(i));
            assert_eq!(index.first_at_or_above(i), None);
            assert_eq!(index.last_at_or_below(i), None);
        }
        assert_eq!(index.last_at_or_below(usize::MAX), None);
    }

    #[test]
    fn a_single_level_is_found_from_both_directions() {
        let mut index = LevelIndex::new(LEN);
        for i in BOUNDARIES.into_iter().chain([LEN - 1]) {
            index.set(i);
            assert!(index.contains(i));
            assert_eq!(index.first_at_or_above(0), Some(i), "level {i}");
            assert_eq!(index.first_at_or_above(i), Some(i), "level {i}");
            assert_eq!(index.first_at_or_above(i + 1), None, "level {i}");
            assert_eq!(index.last_at_or_below(LEN - 1), Some(i), "level {i}");
            assert_eq!(index.last_at_or_below(i), Some(i), "level {i}");
            if i > 0 {
                assert_eq!(index.last_at_or_below(i - 1), None, "level {i}");
            }
            assert_layers_agree(&index);

            index.clear(i);
            assert!(!index.contains(i));
            assert_eq!(index.first_at_or_above(0), None, "level {i} cleared");
            assert_eq!(index.last_at_or_below(LEN - 1), None, "level {i} cleared");
            assert_layers_agree(&index);
        }
    }

    #[test]
    fn searches_step_across_word_and_layer_boundaries() {
        for (low, high) in [(63, 64), (4_095, 4_096), (262_143, 262_144), (0, LEN - 1), (64, 4_097)] {
            let mut index = LevelIndex::new(LEN);
            index.set(low);
            index.set(high);
            assert_eq!(index.first_at_or_above(low + 1), Some(high), "{low} -> {high}");
            assert_eq!(index.last_at_or_below(high - 1), Some(low), "{high} -> {low}");

            // Emptying one side of the boundary leaves the other findable from anywhere.
            index.clear(low);
            assert_eq!(index.first_at_or_above(0), Some(high));
            assert_eq!(index.last_at_or_below(high - 1), None);
            index.set(low);
            index.clear(high);
            assert_eq!(index.last_at_or_below(LEN - 1), Some(low));
            assert_eq!(index.first_at_or_above(low + 1), None);
            assert_layers_agree(&index);
        }
    }

    #[test]
    fn clearing_a_level_keeps_its_neighbours_in_the_same_word() {
        let mut index = LevelIndex::new(4_097);
        index.set(3);
        index.set(5);
        index.clear(3);
        assert_eq!(index.first_at_or_above(0), Some(5));
        assert_eq!(index.last_at_or_below(4), None);
        // Setting a level twice and clearing it once removes it: this is a set, not a count.
        index.set(5);
        index.clear(5);
        assert_eq!(index.first_at_or_above(0), None);
        assert_layers_agree(&index);
    }

    #[test]
    fn queries_from_past_the_end_are_answered() {
        let mut index = LevelIndex::new(100);
        index.set(99);
        assert_eq!(index.first_at_or_above(100), None);
        assert_eq!(index.first_at_or_above(usize::MAX), None);
        assert_eq!(index.last_at_or_below(100), Some(99));
        assert_eq!(index.last_at_or_below(usize::MAX), Some(99));
        assert!(!index.contains(100));
    }

    #[test]
    fn matches_a_btreeset_on_random_operations() {
        let mut state = 0x2545_F491_4F6C_DD1D;
        for len in [1, 2, 63, 64, 65, 4_096, 4_097, 300_000] {
            let mut index = LevelIndex::new(len);
            let mut expected = BTreeSet::new();
            let mut cursor = 0;
            for step in 0..20_000 {
                // Half the time any level; otherwise one near the last level used, so that
                // words fill up, empty out and get searched across, rather than holding one
                // scattered bit each.
                let i = if random_below(&mut state, 2) == 0 {
                    random_below(&mut state, len)
                } else {
                    (cursor + random_below(&mut state, 260)).saturating_sub(130).min(len - 1)
                };
                cursor = i;
                match random_below(&mut state, 5) {
                    0 | 1 => {
                        index.set(i);
                        expected.insert(i);
                    }
                    2 | 3 => {
                        index.clear(i);
                        expected.remove(&i);
                    }
                    _ => {
                        // Clear the lowest member, like a best ask whose level empties.
                        if let Some(&lowest) = expected.first() {
                            index.clear(lowest);
                            expected.remove(&lowest);
                        }
                    }
                }

                // Queries include the two positions just past the end.
                let q = random_below(&mut state, len + 2);
                assert_eq!(index.contains(q), expected.contains(&q), "len {len}, step {step}, contains({q})");
                assert_eq!(
                    index.first_at_or_above(q),
                    expected.range(q..).next().copied(),
                    "len {len}, step {step}, first_at_or_above({q})"
                );
                assert_eq!(
                    index.last_at_or_below(q),
                    expected.range(..=q).next_back().copied(),
                    "len {len}, step {step}, last_at_or_below({q})"
                );
            }
            assert_layers_agree(&index);
        }
    }
}
