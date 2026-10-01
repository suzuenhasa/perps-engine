//! One market's liquidation index: every slot with a liquidation key, ordered by it
//! (`docs/RISK.md` 9.3, D-003, D-018).
//!
//! **Contract.** Two sides. The first long is the one with the highest key and the first
//! short the one with the lowest key, with ties going to the lower `AccountId` on both
//! sides. So the first long is the next slot a falling mark reaches, and the first short
//! the next one a rising mark reaches: a `SetMark` only ever looks at the first entry of
//! each (9.4), and never at the slots its move doesn't cross. Any entry can be filed, moved
//! to a new key or taken out, whether it is first or not.
//!
//! **Structure: an indexed heap per side** (D-018's fallback). RISK.md 14.4 step 4
//! switches to it if std's `BTreeSet`, the first choice, allocates more than once per 1,000
//! commands, and it did: 47 times per 1,000 in `engine/tests/no_alloc.rs`'s flow with 256
//! accounts, because a B-tree allocates a node when one splits and frees one when two merge,
//! and re-keys keep moving entries between nodes (RISK.md 18, P1). Each side has:
//! - a **heap** in a `Vec`: the entry at index `i` has up to four children, at `4i + 1` to
//!   `4i + 4`, and no child comes before its parent. So the first entry is at index 0. The
//!   rest is only partly sorted, which is all the walk needs: it takes the first entry,
//!   then the new first entry, and so on. Four children rather than the textbook two make
//!   the heap half as deep (10 levels for a million entries, not 20), so an entry that
//!   moves to the top or the bottom passes half as many levels, and the children it
//!   compares sit next to each other in memory. That matters most in the `SetMark` walk,
//!   where each liquidation takes the first entry out and moves another down from the top.
//! - a **position map** from account to where its entry is in the `Vec`. With it, any
//!   entry can be found by account, not only the first one, and then moved to a new key or
//!   removed. (std's `BinaryHeap` has no such map, which is why it can only remove its first
//!   entry.)
//!
//! Both sides are "smallest first" heaps of a pair that compares in walk order:
//! `(Reverse(key), account)` for the longs, so that the highest key is the smallest, and
//! `(key, account)` for the shorts. A tuple compares its second field only when the first
//! ones are equal, so ties go to the lower account on both sides.
//!
//! **Invariant.** On each side, no entry comes before its parent, and an account's position
//! is `Some(i)` exactly when the entry at `i` is its own. [`LiquidationIndex::assert_consistent`]
//! checks both, for tests. The engine keeps one entry per slot whose `indexed_key` is set,
//! equal to it, in `rekey` (RISK.md 12, I10).
//!
//! **Complexity** (n entries on a side). The first entry: O(1). Filing, moving or removing
//! an entry: O(log n), because it moves up or down the heap one level at a time until it is
//! in order again (`log4 n` levels), each step up to four comparisons, a swap and two writes
//! to the position map (hash lookups). A re-key usually moves a key only a little (a
//! top-up, a release, a fee), so the entry mostly stays where it is or moves a level.
//! Against the `BTreeSet` it replaced, that makes a place or a cancel cheaper, and taking
//! the first entry out, which each liquidation in the `SetMark` walk does, dearer: the last
//! entry moves into the top and then down every level, where a B-tree just drops its first
//! element. Ablation B measured about 2.4 times the cost per liquidation at a million
//! positions (RISK.md 18, P1); orders are far more frequent than liquidations.
//!
//! **Allocation.** Each side reserves room for `capacity` entries (the market's
//! `slot_capacity`) in its `Vec` and its position map. An account keeps its place in a
//! side's position map once it has one there: leaving the heap sets its position to `None`
//! rather than removing it. So the map grows only when an account is filed on that side for
//! the first time, at most once per slot, and it never fills up with the markers that
//! removals leave in std's `HashMap` (see `book.rs`, "Allocation"). With at most `capacity`
//! slots in the market, no call allocates.

use std::cmp::Reverse;

use crate::id_hash::{IdBuildHasher, IdMap};
use crate::types::{AccountId, Price, Side};

/// See the module docs. A long's key is filed under `Side::Buy` and a short's under
/// `Side::Sell`, as in [`crate::money::liquidation_key`].
#[derive(Clone, Debug)]
pub struct LiquidationIndex {
    /// `Reverse`, so that the highest key comes first.
    longs: IndexedHeap<Reverse<Price>>,
    shorts: IndexedHeap<Price>,
}

impl LiquidationIndex {
    /// An empty index, with room for `capacity` entries on each side before anything
    /// allocates. `hasher` is the engine's seeded hash (D-011).
    pub fn with_capacity(capacity: usize, hasher: IdBuildHasher) -> Self {
        LiquidationIndex {
            longs: IndexedHeap::with_capacity(capacity, hasher),
            shorts: IndexedHeap::with_capacity(capacity, hasher),
        }
    }

    /// Makes the room both sides reserved resident, changing nothing they hold
    /// (`crate::prefault`).
    pub fn prefault(&mut self) {
        self.longs.prefault(Reverse(0));
        self.shorts.prefault(0);
    }

    /// Files `account` under `key`. It must not be filed on that side already.
    pub fn insert(&mut self, side: Side, key: Price, account: AccountId) {
        match side {
            Side::Buy => self.longs.insert(Reverse(key), account),
            Side::Sell => self.shorts.insert(key, account),
        }
    }

    /// Removes the entry `insert` made for `account` under `key`.
    pub fn remove(&mut self, side: Side, key: Price, account: AccountId) {
        match side {
            Side::Buy => self.longs.remove(Reverse(key), account),
            Side::Sell => self.shorts.remove(key, account),
        }
    }

    /// Moves `account`'s entry from `old` to `new`, where `None` means not filed: what a
    /// re-key does (RISK.md 9.3). A key that stays on its side is changed in place, which
    /// is cheaper than a removal and an insertion; a slot that closes or flips its position
    /// leaves one side and joins the other.
    pub fn refile(&mut self, account: AccountId, old: Option<(Side, Price)>, new: Option<(Side, Price)>) {
        match (old, new) {
            (Some((Side::Buy, old)), Some((Side::Buy, new))) => {
                self.longs.change_key(Reverse(old), Reverse(new), account);
            }
            (Some((Side::Sell, old)), Some((Side::Sell, new))) => self.shorts.change_key(old, new, account),
            _ => {
                if let Some((side, key)) = old {
                    self.remove(side, key, account);
                }
                if let Some((side, key)) = new {
                    self.insert(side, key, account);
                }
            }
        }
    }

    /// The long with the highest key (the lowest account on a tie), if any.
    pub fn first_long(&self) -> Option<(Price, AccountId)> {
        self.longs.first().map(|(Reverse(key), account)| (key, account))
    }

    /// The short with the lowest key (the lowest account on a tie), if any.
    pub fn first_short(&self) -> Option<(Price, AccountId)> {
        self.shorts.first()
    }

    /// Every entry as `(side, key, account)`, longs then shorts, each in walk order.
    /// Allocates and sorts; for the invariant checks in tests.
    pub fn entries(&self) -> Vec<(Side, Price, AccountId)> {
        let longs = self.longs.sorted().into_iter().map(|(Reverse(key), account)| (Side::Buy, key, account));
        let shorts = self.shorts.sorted().into_iter().map(|(key, account)| (Side::Sell, key, account));
        longs.chain(shorts).collect()
    }

    /// Panics unless both sides keep the module's invariant: each is a heap, and its
    /// position map agrees with it. Walks everything; tests only.
    pub fn assert_consistent(&self) {
        self.longs.assert_consistent("longs");
        self.shorts.assert_consistent("shorts");
    }
}

/// How many children each entry of a heap has, at most (module docs, "Structure").
const CHILDREN: usize = 4;

/// One side of the index: an indexed heap of `(order, account)` pairs, smallest
/// first (module docs, "Structure"). `K` is `Reverse<Price>` for the longs and `Price` for
/// the shorts.
#[derive(Clone, Debug)]
struct IndexedHeap<K> {
    /// The heap. `entries[0]` comes first; the children of `entries[i]` are at `4i + 1` to
    /// `4i + 4` (those that exist), and none comes before it.
    entries: Vec<(K, AccountId)>,
    /// Where each account's entry is in `entries`. `None` for an account that was filed on
    /// this side before and isn't now (module docs, "Allocation").
    positions: IdMap<AccountId, Option<usize>>,
}

impl<K: Ord + Copy> IndexedHeap<K> {
    fn with_capacity(capacity: usize, hasher: IdBuildHasher) -> Self {
        IndexedHeap {
            entries: Vec::with_capacity(capacity),
            positions: IdMap::with_capacity_and_hasher(capacity, hasher),
        }
    }

    /// See [`LiquidationIndex::prefault`]. `filler` is any key, written into spare room.
    fn prefault(&mut self, filler: K) {
        crate::prefault::touch_spare(&mut self.entries, (filler, 0));
        crate::prefault::touch_map(&mut self.positions, |i| i as AccountId, || None);
    }

    fn first(&self) -> Option<(K, AccountId)> {
        self.entries.first().copied()
    }

    /// Adds `(key, account)` at the bottom of the heap, then moves it up to where it
    /// belongs. The account must not be filed here already.
    fn insert(&mut self, key: K, account: AccountId) {
        let index = self.entries.len();
        self.entries.push((key, account));
        let previous = self.positions.insert(account, Some(index));
        debug_assert!(previous.flatten().is_none(), "account {account} is filed twice");
        self.sift_up(index);
    }

    /// Takes out `account`'s entry, which must be `(key, account)`. The last entry fills the
    /// hole, then moves up or down to where it belongs.
    fn remove(&mut self, key: K, account: AccountId) {
        let index = self.position_of(account, key);
        self.positions.insert(account, None);
        let last = self.entries.pop().expect("a filed account has an entry");
        // If the entry taken out was the last one, there is no hole to fill.
        if index < self.entries.len() {
            self.entries[index] = last;
            self.set_position(last.1, index);
            self.restore_order(index);
        }
    }

    /// Changes `account`'s entry from `(old, account)` to `(new, account)` where it is,
    /// then moves it up or down to where it belongs.
    fn change_key(&mut self, old: K, new: K, account: AccountId) {
        let index = self.position_of(account, old);
        self.entries[index].0 = new;
        self.restore_order(index);
    }

    /// Where `account`'s entry is. It must be filed here, as `(key, account)`.
    fn position_of(&self, account: AccountId, key: K) -> usize {
        let position = self.positions.get(&account).copied().flatten();
        let index = position.unwrap_or_else(|| panic!("account {account} is not filed"));
        debug_assert!(self.entries[index] == (key, account), "account {account} is filed under another key");
        index
    }

    /// Records that `account`'s entry is now at `index`. The account has a position
    /// already: every entry's account got one when it was inserted.
    fn set_position(&mut self, account: AccountId, index: usize) {
        let position = self.positions.get_mut(&account).expect("an entry's account has a position");
        *position = Some(index);
    }

    /// After the entry at `index` has changed, moves it up or down until no entry comes
    /// before its parent. At most one of the two moves anything: an entry that moved up is
    /// already before the children it ends up with.
    fn restore_order(&mut self, index: usize) {
        let index = self.sift_up(index);
        self.sift_down(index);
    }

    /// Moves the entry at `index` up while it comes before its parent. Returns where it
    /// ends up.
    fn sift_up(&mut self, mut index: usize) -> usize {
        while index > 0 {
            let parent = (index - 1) / CHILDREN;
            if !self.comes_before(index, parent) {
                break;
            }
            self.swap(index, parent);
            index = parent;
        }
        index
    }

    /// Moves the entry at `index` down while one of its children comes before it, each
    /// time swapping it with whichever child comes first.
    fn sift_down(&mut self, mut index: usize) {
        loop {
            let mut first = index;
            for child in CHILDREN * index + 1..=CHILDREN * index + CHILDREN {
                if child < self.entries.len() && self.comes_before(child, first) {
                    first = child;
                }
            }
            if first == index {
                return;
            }
            self.swap(index, first);
            index = first;
        }
    }

    /// True if the entry at `a` comes before the entry at `b`. That is the pairs' own order
    /// (module docs, "Structure"), so the same order as the walk's.
    fn comes_before(&self, a: usize, b: usize) -> bool {
        self.entries[a] < self.entries[b]
    }

    /// Swaps two entries, and updates both accounts' positions.
    fn swap(&mut self, a: usize, b: usize) {
        self.entries.swap(a, b);
        self.set_position(self.entries[a].1, a);
        self.set_position(self.entries[b].1, b);
    }

    /// Every entry, in walk order. Allocates; for tests.
    fn sorted(&self) -> Vec<(K, AccountId)> {
        let mut entries = self.entries.clone();
        entries.sort_unstable();
        entries
    }

    /// See [`LiquidationIndex::assert_consistent`]. `side` names the side in the message.
    fn assert_consistent(&self, side: &str) {
        for (index, &(_, account)) in self.entries.iter().enumerate() {
            if index > 0 {
                let parent = (index - 1) / CHILDREN;
                assert!(!self.comes_before(index, parent), "{side}: entry {index} comes before its parent");
            }
            let position = self.positions.get(&account).copied().flatten();
            assert_eq!(position, Some(index), "{side}: account {account}'s position");
        }
        let filed = self.positions.values().filter(|position| position.is_some()).count();
        assert_eq!(filed, self.entries.len(), "{side}: accounts with a position, against entries");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn empty_index() -> LiquidationIndex {
        LiquidationIndex::with_capacity(16, IdBuildHasher::new(0))
    }

    #[test]
    fn longs_come_highest_key_first_and_shorts_lowest_first_ties_to_the_lower_account() {
        let mut index = empty_index();
        assert_eq!((index.first_long(), index.first_short()), (None, None));
        for (key, account) in [(73_100, 5), (73_200, 9), (73_200, 2), (70_000, 1)] {
            index.insert(Side::Buy, key, account);
        }
        for (key, account) in [(76_854, 3), (76_000, 8), (76_000, 4)] {
            index.insert(Side::Sell, key, account);
        }
        assert_eq!(index.first_long(), Some((73_200, 2)));
        assert_eq!(index.first_short(), Some((76_000, 4)));
        assert_eq!(
            index.entries(),
            vec![
                (Side::Buy, 73_200, 2),
                (Side::Buy, 73_200, 9),
                (Side::Buy, 73_100, 5),
                (Side::Buy, 70_000, 1),
                (Side::Sell, 76_000, 4),
                (Side::Sell, 76_000, 8),
                (Side::Sell, 76_854, 3),
            ]
        );

        index.remove(Side::Buy, 73_200, 2);
        index.remove(Side::Sell, 76_000, 4);
        assert_eq!(index.first_long(), Some((73_200, 9)));
        assert_eq!(index.first_short(), Some((76_000, 8)));
        index.assert_consistent();
    }

    #[test]
    fn the_same_account_and_key_on_different_sides_are_different_entries() {
        let mut index = empty_index();
        index.insert(Side::Buy, 100, 1);
        index.insert(Side::Sell, 100, 1);
        index.remove(Side::Buy, 100, 1);
        assert_eq!(index.entries(), vec![(Side::Sell, 100, 1)]);
        index.assert_consistent();
    }

    #[test]
    fn refiling_moves_an_entry_within_its_side_or_across_sides() {
        let mut index = empty_index();
        index.insert(Side::Buy, 90, 1);
        index.insert(Side::Buy, 80, 2);
        // A realized loss raises account 2's key past account 1's: it becomes the first long.
        index.refile(2, Some((Side::Buy, 80)), Some((Side::Buy, 95)));
        assert_eq!(index.first_long(), Some((95, 2)));
        // The long flips to a short, then closes.
        index.refile(2, Some((Side::Buy, 95)), Some((Side::Sell, 120)));
        assert_eq!((index.first_long(), index.first_short()), (Some((90, 1)), Some((120, 2))));
        index.refile(2, Some((Side::Sell, 120)), None);
        assert_eq!(index.entries(), vec![(Side::Buy, 90, 1)]);
        // Filed again later, on the side it was on before.
        index.refile(2, None, Some((Side::Buy, 90)));
        assert_eq!(index.first_long(), Some((90, 1)), "a tie goes to the lower account");
        index.assert_consistent();
    }

    /// A small deterministic pseudo-random generator (xorshift64), so the next test is the
    /// same on every run.
    fn random_below(state: &mut u64, n: u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state % n
    }

    #[test]
    fn matches_a_btreeset_on_random_operations() {
        // The model is what the index was before it became a heap: one ordered set per side,
        // whose first element is the first entry. Keys come from a narrow range, so ties are
        // common, and small moves (like a re-key's) are mixed with large ones.
        let mut state = 0x9E37_79B9_7F4A_7C15;
        for accounts in [1u64, 2, 3, 17, 200] {
            let mut index = empty_index();
            let mut longs: BTreeSet<(Reverse<Price>, AccountId)> = BTreeSet::new();
            let mut shorts: BTreeSet<(Price, AccountId)> = BTreeSet::new();
            // What each account has filed now, as `LiquidationIndex::refile` takes it.
            let mut filed: Vec<Option<(Side, Price)>> = vec![None; accounts as usize];
            for step in 0..20_000 {
                let account = random_below(&mut state, accounts) as usize;
                let key = random_below(&mut state, 40) as Price;
                let new = match random_below(&mut state, 5) {
                    0 => None,
                    1 | 2 => Some((Side::Buy, key)),
                    3 => Some((Side::Sell, key)),
                    // A small move of the current key, as after a top-up or a release.
                    _ => filed[account]
                        .map(|(side, old)| (side, old + random_below(&mut state, 3) as Price - 1)),
                };
                // As the engine's re-key does: nothing if the key didn't change.
                let (id, old) = (account as AccountId, filed[account]);
                if new == old {
                    continue;
                }
                index.refile(id, old, new);
                filed[account] = new;
                match old {
                    Some((Side::Buy, key)) => assert!(longs.remove(&(Reverse(key), id))),
                    Some((Side::Sell, key)) => assert!(shorts.remove(&(key, id))),
                    None => {}
                }
                match new {
                    Some((Side::Buy, key)) => assert!(longs.insert((Reverse(key), id))),
                    Some((Side::Sell, key)) => assert!(shorts.insert((key, id))),
                    None => {}
                }

                let expected_long = longs.first().map(|&(Reverse(key), account)| (key, account));
                assert_eq!(index.first_long(), expected_long, "{accounts} accounts, step {step}");
                assert_eq!(index.first_short(), shorts.first().copied(), "{accounts} accounts, step {step}");
                if step % 97 == 0 {
                    index.assert_consistent();
                    let expected: Vec<(Side, Price, AccountId)> = longs
                        .iter()
                        .map(|&(Reverse(key), account)| (Side::Buy, key, account))
                        .chain(shorts.iter().map(|&(key, account)| (Side::Sell, key, account)))
                        .collect();
                    assert_eq!(index.entries(), expected, "{accounts} accounts, step {step}");
                }
            }
            index.assert_consistent();
        }
    }
}
