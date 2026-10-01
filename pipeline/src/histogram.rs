//! A latency histogram of our own, in the style of HdrHistogram (`docs/PIPELINE.md` 15.3;
//! `docs/DECISIONS.md` D-028).
//!
//! **Contract.** Values are nanoseconds (`u64`). Recording is O(1) and never allocates.
//! A percentile is reported as the upper bound of the bucket that holds it (or the exact
//! maximum, if that is smaller), so it overstates the true value by less than 1/128
//! (0.79%) and never understates it. Histograms merge exactly, bucket by bucket.
//!
//! **Buckets** (log-linear, `m` = 7 sub-bucket bits). Values 0 to 127 have one bucket each:
//! `index = v`. Otherwise, with `e` the position of `v`'s top bit (`63 - leading_zeros`),
//! `index = (e - 7) * 128 + (v >> (e - 7))`. So each power of two `[2^e, 2^(e+1))` is split
//! into 128 equal buckets of width `2^(e-7)`, which is 1/128 of the range's lower bound.
//! (For `e` = 7 the width is 1, so the values 0 to 255 are all exact.) The buckets are
//! contiguous and cover the whole `u64` range: 7,424 buckets of 8 bytes, 58 KiB.
//!
//! Examples: 100 ns is bucket 100 (exact); 1,000 ns is bucket 506, `[1,000, 1,003]`;
//! 50,000 ns is bucket 1,219, `[49,920, 50,175]`; 1 ms is bucket 1,780,
//! `[999,424, 1,003,519]`.
//!
//! **The error bound.** A value `v` in a bucket `[lower, upper]` of width `w` has
//! `v >= lower >= 128 w`, and the reported `upper = lower + w - 1`, so the report is high
//! by `upper - v < w <= v / 128`. A consequence: a test "p99 < 50 µs" fails any true p99
//! from 49,920 ns up, because that bucket reports 50,175.
//!
//! **Not served.** A command that was never served is recorded as `u64::MAX` (15.4). It
//! lands in the top bucket, whose upper bound is `u64::MAX`, so any percentile that reaches
//! it reads `u64::MAX`, which fails every limit and prints as "∞" ([`Ns`]).
//!
//! **Percentiles without floats.** Percentile `p` is a fraction `num / den` (99/100,
//! 999/1,000): `rank = ceil(count * num / den)`, then walk the buckets from 0, adding their
//! counts, until the total reaches `rank`. An empty histogram has no percentiles
//! (`None`, printed "no data"), never a number.
//!
//! **Memory.** The 58 KiB of counts are allocated once and every page is written at
//! creation, so recording never takes a first-touch page fault inside a measured window
//! (15.4).
//!
//! **Complexity.** `record`: one `leading_zeros`, a shift, an add and an increment, plus
//! min, max, count and sum. `percentile`: O(buckets). `merge`: O(buckets).

use std::fmt;

/// Sub-bucket bits: each power of two is split into `2^7` = 128 buckets.
const SUB_BUCKET_BITS: u32 = 7;
/// Buckets per power of two.
const SUB_BUCKETS: usize = 1 << SUB_BUCKET_BITS;
/// Buckets in a histogram: 128 exact ones for 0 to 127, then 128 for each top-bit position
/// from 7 to 63.
pub const BUCKETS: usize = SUB_BUCKETS + (64 - SUB_BUCKET_BITS as usize) * SUB_BUCKETS;

const _: () = assert!(BUCKETS == 7_424);

/// The bucket that holds `value`. See the module docs for the formula.
pub const fn bucket_index(value: u64) -> usize {
    if value < SUB_BUCKETS as u64 {
        return value as usize;
    }
    let top_bit = 63 - value.leading_zeros();
    let shift = top_bit - SUB_BUCKET_BITS;
    shift as usize * SUB_BUCKETS + (value >> shift) as usize
}

/// The smallest value in bucket `index`.
pub const fn bucket_lower(index: usize) -> u64 {
    if index < SUB_BUCKETS {
        return index as u64;
    }
    // Invert the formula: index = shift * 128 + m, with m = value >> shift in 128..256.
    let shift = index / SUB_BUCKETS - 1;
    let m = (SUB_BUCKETS + index % SUB_BUCKETS) as u64;
    m << shift
}

/// The largest value in bucket `index`.
pub const fn bucket_upper(index: usize) -> u64 {
    if index < SUB_BUCKETS {
        return index as u64;
    }
    let shift = index / SUB_BUCKETS - 1;
    bucket_lower(index) + ((1u64 << shift) - 1)
}

/// Latencies in nanoseconds. See the module docs.
#[derive(Clone)]
pub struct LatencyHistogram {
    /// `counts[i]`: values recorded in bucket `i`.
    counts: Box<[u64]>,
    count: u64,
    /// Exact; meaningful only when `count > 0`.
    min: u64,
    max: u64,
    /// Exact, for the mean: 2^64 values of `u64::MAX` fit in a `u128`.
    sum: u128,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        LatencyHistogram::new()
    }
}

impl LatencyHistogram {
    /// An empty histogram, with every page of its counts already touched (module docs).
    pub fn new() -> Self {
        let mut counts = vec![0; BUCKETS].into_boxed_slice();
        // `vec![0; n]` may come from the allocator as untouched zero pages, and a plain
        // store of 0 into memory known to be zero may be optimised away. `black_box` hides
        // the value from the compiler, so every store happens and maps its page.
        for count in counts.iter_mut() {
            *count = std::hint::black_box(0);
        }
        LatencyHistogram { counts, count: 0, min: u64::MAX, max: 0, sum: 0 }
    }

    /// Records one value, in nanoseconds; `u64::MAX` for "never served".
    #[inline]
    pub fn record(&mut self, value: u64) {
        self.counts[bucket_index(value)] += 1;
        self.count += 1;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += u128::from(value);
    }

    /// Values recorded.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The smallest value recorded, exactly; `None` if empty.
    pub fn min(&self) -> Option<u64> {
        (self.count > 0).then_some(self.min)
    }

    /// The largest value recorded, exactly; `None` if empty.
    pub fn max(&self) -> Option<u64> {
        (self.count > 0).then_some(self.max)
    }

    /// The mean, rounded down; `None` if empty.
    pub fn mean(&self) -> Option<u64> {
        (self.count > 0).then(|| (self.sum / u128::from(self.count)) as u64)
    }

    /// Values recorded in bucket `index`.
    pub fn bucket_count(&self, index: usize) -> u64 {
        self.counts[index]
    }

    /// The percentile `num / den` (for p99, `99, 100`): the upper bound of the bucket where
    /// the running count reaches `ceil(count * num / den)`, or the exact maximum if that is
    /// smaller. `None` if the histogram is empty. Panics unless `0 < den` and `num <= den`.
    pub fn percentile(&self, num: u64, den: u64) -> Option<u64> {
        assert!(den > 0 && num <= den, "a percentile is a fraction num/den between 0 and 1, got {num}/{den}");
        if self.count == 0 {
            return None;
        }
        // At least rank 1: the 0th percentile is the smallest value's bucket, not bucket 0.
        let rank = (u128::from(self.count) * u128::from(num)).div_ceil(u128::from(den)).max(1);
        let mut seen: u128 = 0;
        for (index, &count) in self.counts.iter().enumerate() {
            seen += u128::from(count);
            if seen >= rank {
                return Some(bucket_upper(index).min(self.max));
            }
        }
        unreachable!("the counts add up to `count`, and rank <= count")
    }

    /// Adds every value recorded in `other`, as if recorded here: exact.
    pub fn merge(&mut self, other: &LatencyHistogram) {
        for (mine, theirs) in self.counts.iter_mut().zip(other.counts.iter()) {
            *mine += theirs;
        }
        self.count += other.count;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.sum += other.sum;
    }

    /// Forgets every value recorded.
    pub fn clear(&mut self) {
        self.counts.fill(0);
        self.count = 0;
        self.min = u64::MAX;
        self.max = 0;
        self.sum = 0;
    }
}

/// Leaves out the 7,424 counts.
impl fmt::Debug for LatencyHistogram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LatencyHistogram")
            .field("count", &self.count)
            .field("min", &self.min())
            .field("max", &self.max())
            .field("mean", &self.mean())
            .finish_non_exhaustive()
    }
}

/// A reported latency, for printing: "no data" for `None`, "∞" for `u64::MAX` (never
/// served), otherwise the number of nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ns(pub Option<u64>);

impl fmt::Display for Ns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.write_str("no data"),
            Some(u64::MAX) => f.write_str("∞"),
            Some(ns) => write!(f, "{ns}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic generator (xorshift64).
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn the_specs_examples() {
        assert_eq!(bucket_index(100), 100);
        assert_eq!((bucket_lower(100), bucket_upper(100)), (100, 100));
        assert_eq!(bucket_index(1_000), 506);
        assert_eq!((bucket_lower(506), bucket_upper(506)), (1_000, 1_003));
        assert_eq!(bucket_index(50_000), 1_219);
        assert_eq!((bucket_lower(1_219), bucket_upper(1_219)), (49_920, 50_175));
        assert_eq!(bucket_index(1_000_000), 1_780);
        assert_eq!((bucket_lower(1_780), bucket_upper(1_780)), (999_424, 1_003_519));
        assert_eq!(size_of::<u64>() * BUCKETS, 59_392, "58 KiB");
    }

    #[test]
    fn index_and_bounds_at_every_power_of_two_and_either_side() {
        for e in 0..64 {
            let power = 1u64 << e;
            for v in [power - 1, power, power + 1] {
                let index = bucket_index(v);
                assert!(bucket_lower(index) <= v && v <= bucket_upper(index), "{v} in bucket {index}");
            }
            // A power of two always starts a bucket.
            assert_eq!(bucket_lower(bucket_index(power)), power, "2^{e}");
        }
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(127), 127);
        assert_eq!(bucket_index(128), 128);
        assert_eq!(bucket_index(255), 255);
        assert_eq!(bucket_index(256), 256);
        assert_eq!(bucket_index(u64::MAX), BUCKETS - 1);
        assert_eq!(bucket_upper(BUCKETS - 1), u64::MAX);
    }

    #[test]
    fn every_bucket_is_contiguous_with_the_next_and_at_most_a_128th_of_its_lower_bound_wide() {
        // All 7,424 buckets, so beyond the spec's check up to 2^40.
        assert_eq!(bucket_lower(0), 0);
        for index in 0..BUCKETS {
            let (lower, upper) = (bucket_lower(index), bucket_upper(index));
            assert!(lower <= upper, "bucket {index}");
            assert_eq!(bucket_index(lower), index, "lower bound of {index}");
            assert_eq!(bucket_index(upper), index, "upper bound of {index}");
            if index + 1 < BUCKETS {
                assert_eq!(upper + 1, bucket_lower(index + 1), "gap or overlap after bucket {index}");
            }
            let width = upper - lower + 1;
            if lower < 256 {
                assert_eq!(width, 1, "values below 256 are exact");
            } else {
                assert!(u128::from(width) * 128 <= u128::from(lower), "bucket {index} is too wide");
                // The widest a bucket gets: exactly 1/128 of its lower bound, at a power of two.
                if lower.is_power_of_two() {
                    assert_eq!(u128::from(width) * 128, u128::from(lower), "bucket {index}");
                }
            }
        }
    }

    #[test]
    fn an_empty_histogram_reports_no_data() {
        let histogram = LatencyHistogram::new();
        assert_eq!(histogram.count(), 0);
        assert_eq!(histogram.percentile(99, 100), None);
        assert_eq!((histogram.min(), histogram.max(), histogram.mean()), (None, None, None));
        assert_eq!(Ns(histogram.percentile(50, 100)).to_string(), "no data");
    }

    #[test]
    fn percentiles_on_a_small_known_set() {
        let mut histogram = LatencyHistogram::new();
        for v in 1..=100 {
            histogram.record(v);
        }
        // Values below 256 are exact, so the percentiles are exact here.
        assert_eq!(histogram.percentile(50, 100), Some(50));
        assert_eq!(histogram.percentile(99, 100), Some(99));
        assert_eq!(histogram.percentile(999, 1_000), Some(100)); // rank ceil(99.9) = 100
        assert_eq!(histogram.percentile(1, 1), Some(100));
        assert_eq!(histogram.percentile(0, 1), Some(1), "rank 0 reads as the smallest value");
        assert_eq!((histogram.min(), histogram.max(), histogram.mean()), (Some(1), Some(100), Some(50)));

        // One large value: its bucket's upper bound is above it, so the exact max is reported.
        histogram.record(50_000);
        assert_eq!(histogram.percentile(1, 1), Some(50_000));
        histogram.record(50_001);
        assert_eq!(histogram.percentile(1, 1), Some(50_001));
        assert_eq!(histogram.percentile(101, 102), Some(50_001), "the 101st of 102 values");
        assert_eq!(histogram.percentile(100, 102), Some(100));
    }

    #[test]
    fn a_percentile_of_one_bucket_reports_its_upper_bound_unless_the_max_is_smaller() {
        let mut histogram = LatencyHistogram::new();
        histogram.record(49_920);
        histogram.record(50_100);
        // Both are in [49,920, 50,175]: p50 reports the bucket's upper bound, capped at max.
        assert_eq!(histogram.percentile(50, 100), Some(50_100));
        histogram.record(50_175);
        assert_eq!(histogram.percentile(50, 100), Some(50_175));
    }

    #[test]
    fn never_served_reads_as_infinity() {
        let mut histogram = LatencyHistogram::new();
        for _ in 0..98 {
            histogram.record(1_000);
        }
        histogram.record(u64::MAX);
        histogram.record(u64::MAX);
        assert_eq!(histogram.percentile(98, 100), Some(1_003));
        assert_eq!(histogram.percentile(99, 100), Some(u64::MAX), "2% never served: p99 is infinite");
        assert_eq!(Ns(histogram.percentile(99, 100)).to_string(), "∞");
        assert_eq!(Ns(histogram.percentile(50, 100)).to_string(), "1003");
        assert_eq!(histogram.mean(), Some(((98 * 1_000 + 2 * u128::from(u64::MAX)) / 100) as u64));
    }

    #[test]
    fn merging_equals_recording_both() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let (mut a, mut b, mut both) =
            (LatencyHistogram::new(), LatencyHistogram::new(), LatencyHistogram::new());
        for i in 0..10_000 {
            let v = next(&mut state) >> (next(&mut state) % 64);
            let part = if i % 3 == 0 { &mut a } else { &mut b };
            part.record(v);
            both.record(v);
        }
        a.merge(&b);
        assert_eq!(a.counts, both.counts);
        assert_eq!(
            (a.count(), a.min(), a.max(), a.mean()),
            (both.count(), both.min(), both.max(), both.mean())
        );
        // Merging an empty histogram changes nothing.
        let before = a.clone();
        a.merge(&LatencyHistogram::new());
        assert_eq!((a.counts, a.min, a.max, a.sum), (before.counts, before.min, before.max, before.sum));
    }

    #[test]
    fn percentiles_of_random_values_are_within_the_error_bound_of_the_exact_ones() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut histogram = LatencyHistogram::new();
        let mut values: Vec<u64> = (0..100_000)
            .map(|_| {
                // Every magnitude: random bits shifted right by a random amount.
                let v = next(&mut state) >> (next(&mut state) % 64);
                histogram.record(v);
                v
            })
            .collect();
        values.sort_unstable();
        for (num, den) in [(1, 100), (50, 100), (90, 100), (99, 100), (999, 1_000), (9_999, 10_000), (1, 1)] {
            let rank = (values.len() as u64 * num).div_ceil(den);
            let exact = values[rank as usize - 1];
            let reported = histogram.percentile(num, den).expect("not empty");
            // Never below the exact value, and above it by less than 1/128 of it
            // (0.79%): the bound of the module docs.
            assert!(reported >= exact, "p{num}/{den}: {reported} below {exact}");
            assert!(
                u128::from(reported - exact) * 128 <= u128::from(exact),
                "p{num}/{den}: {reported} vs {exact}"
            );
        }
    }

    #[test]
    fn clearing_forgets_everything() {
        let mut histogram = LatencyHistogram::new();
        histogram.record(7);
        histogram.record(u64::MAX);
        histogram.clear();
        assert_eq!(histogram.count(), 0);
        assert_eq!(histogram.percentile(50, 100), None);
        assert_eq!(histogram.counts.len(), BUCKETS);
        assert!(histogram.counts.iter().all(|&c| c == 0));
        histogram.record(3);
        assert_eq!((histogram.min(), histogram.max()), (Some(3), Some(3)));
    }
}
