//! The types of a calibrated flow profile (`docs/DECISIONS.md` D-034): a table of real
//! markets, and the distributions a calibrated flow draws its content from. The one profile,
//! Polymarket Perps' as recorded on 2026-09-30, is [`POLYMARKET`], generated into
//! `polymarket_profile.rs` by `tools/calibrate` (its README gives the sample and the method).
//!
//! **Contract.** A profile is constant data: nothing to initialise, nothing allocated, so it
//! can be read from any thread. Every number a flow can draw its *content* from is an integer,
//! since loadgen's flows contain no floats (`lib.rs`): shares and probabilities in parts per
//! million (`_ppm`, summing to exactly 1,000,000 where they split a whole), fitted parameters
//! in thousandths (`_milli`), spreads and moves in hundredths or thousandths of a basis point
//! (`_centibps`, `_millibps`), money in micro-dollars (D-004) or where named in whole dollars
//! or cents, prices in each market's own ticks (`10^-pd` dollars). A continuous distribution
//! comes as a [`Table`] of equally likely values, computed by the tool from the fitted
//! parameters that sit beside it (quoted for the reader), or from the recorded values
//! themselves where no fit is close enough. The only floats are those of
//! [`BurstModel`], which shape the send schedule, never the plan (PIPELINE.md 14.9).
//!
//! **Classes.** A market's book shape follows its [`BookClass`] (majors; alt crypto of 10x or
//! more; long-tail crypto of 5x or less; tradfi equities; tradfi macro), its price moves its
//! [`LeverageClass`] (maximum leverage 50x, 20x, 10x, or 3x and 5x).
//!
//! **Invariants** (tested on [`POLYMARKET`], `profile/tests.rs`): 88 markets with distinct ids;
//! `pd + qd = 6`; tier tables of 1 to 8 rows that start at 0 with the market's maximum leverage,
//! with bounds strictly rising and leverage strictly falling; start prices on their real grid
//! (`10^max(0, digits − 5)` ticks) and inside the engine's price limits (D-020); weights and
//! every split summing to 1,000,000; every table in ascending order.
//!
//! **Complexity.** A [`Table`] draw is O(1); a [`pick`] over `n` shares is O(n).

use engine::types::{MarketId, Micros, Price};

use crate::SplitMix64;

pub use super::polymarket_profile::POLYMARKET;

// ---------------------------------------------------------------------------------------
// Drawing.

/// A distribution as equally likely values, in ascending order: its quantiles at the middle
/// of equal slices of probability, `F⁻¹((i + 0.5) / n)` for `i` in `0..n`. A draw takes one
/// uniformly, so it needs no floats; the extremes are the quantiles at `0.5 / n` and
/// `1 − 0.5 / n`, so the tails are cut there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Table(pub &'static [u32]);

impl Table {
    /// One value, from `rng`.
    pub fn draw(&self, rng: &mut SplitMix64) -> u32 {
        self.0[rng.below(self.0.len() as u64) as usize]
    }

    /// The middle value (the upper one of the two middle values if their count is even).
    pub fn median(&self) -> u32 {
        self.0[self.0.len() / 2]
    }
}

/// An index into `shares_ppm` (shares summing to 1,000,000), drawn with those probabilities.
pub fn pick(shares_ppm: &[u32], rng: &mut SplitMix64) -> usize {
    let mut left = rng.below(1_000_000) as u32;
    for (i, &share) in shares_ppm.iter().enumerate() {
        if left < share {
            return i;
        }
        left -= share;
    }
    panic!("shares sum to {}, not 1,000,000", shares_ppm.iter().sum::<u32>());
}

// ---------------------------------------------------------------------------------------
// The profile.

/// Everything a calibrated flow draws from.
#[derive(Debug)]
pub struct Profile {
    /// Which venue and day.
    pub name: &'static str,
    /// The recorded sample, briefly (`tools/calibrate/README.md` in full).
    pub sample: &'static str,
    /// SHA-256 of the JSON this table was generated from (`tools/calibrate/profile-*.json`),
    /// in lower-case hex. A flow's digest hashes it, so a regenerated table gives a new
    /// digest (`polymarket.rs`, `PolymarketConfig::digest`).
    pub sha256: &'static str,
    /// Every market, by id.
    pub markets: &'static [Market],
    /// One per [`BookClass`], in its order.
    pub book_shapes: [&'static BookShape; 5],
    /// One per [`LeverageClass`], in its order.
    pub moves: [&'static MoveModel; 4],
    /// `|z|` of a standard normal draw, in thousandths: 1,024 values. A move draws a
    /// component of its class's mixture, then one of these, then a sign.
    pub half_normal_milli: Table,
    pub jumps: Jumps,
    pub maker_activity: MakerActivity,
    pub takers: Takers,
    pub taker_notional: TakerNotional,
    /// `--bursts median` and `--bursts busiest`.
    pub bursts_median: BurstModel,
    pub bursts_busiest: BurstModel,
    pub shocks: Shocks,
    /// Levels quoted per side: 20, as far as the recorded books show.
    pub levels_per_side: u32,
    /// The operator's mark, per market: every 200 ms.
    pub mark_every_ms: u32,
    /// The smallest order: $10 of notional. A flow raises any smaller notional it draws to
    /// this, and rounds lots up to reach it (every table already starts at $10 or more).
    pub min_notional: Micros,
    /// A dust order's notional is drawn from `[low, high)` cents: $10.50 to $11.60, for makers
    /// and takers alike.
    pub dust_draw_cents: [u32; 2],
}

impl Profile {
    /// The book shape of `class`.
    pub fn book_shape(&self, class: BookClass) -> &'static BookShape {
        self.book_shapes[class as usize]
    }

    /// The price moves of `class`.
    pub fn moves_of(&self, class: LeverageClass) -> &'static MoveModel {
        self.moves[class as usize]
    }

    /// The market with id `id`, if the profile has it.
    pub fn market(&self, id: MarketId) -> Option<&'static Market> {
        self.markets.iter().find(|market| market.id == id)
    }
}

// ---------------------------------------------------------------------------------------
// Markets.

/// Book-shape class (D-034).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookClass {
    /// BTC, ETH and SOL: a spread of one real tick, and deep, dense books.
    Majors,
    /// Other crypto of 10x or more.
    AltCrypto,
    /// Crypto of 5x or less: wider spreads, thinner books, more dust.
    LongTailCrypto,
    /// Single stocks and four small indexes (SOXL, NCLD, DRAM, EWY).
    TradfiEquities,
    /// SP500, NAS100, GOLD, SILVER, WTIOIL and BRENTOIL: deep books.
    TradfiMacro,
}

impl BookClass {
    pub const ALL: [BookClass; 5] = [
        BookClass::Majors,
        BookClass::AltCrypto,
        BookClass::LongTailCrypto,
        BookClass::TradfiEquities,
        BookClass::TradfiMacro,
    ];
}

/// Price-dynamics class: the market's maximum leverage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeverageClass {
    Max50,
    Max20,
    Max10,
    /// 3x and 5x: small-cap crypto.
    Max3To5,
}

impl LeverageClass {
    pub const ALL: [LeverageClass; 4] =
        [LeverageClass::Max50, LeverageClass::Max20, LeverageClass::Max10, LeverageClass::Max3To5];
}

/// One row of a market's tier table: at a position notional of `lower_bound` or more, at most
/// `max_leverage` (RISK.md 6.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tier {
    pub lower_bound: Micros,
    pub max_leverage: u16,
}

/// One real market, with Polymarket's instrument id as our [`MarketId`].
#[derive(Debug)]
pub struct Market {
    pub id: MarketId,
    pub symbol: &'static str,
    /// Its class's book shape: spread, gaps, and each level's sizes.
    pub book: &'static BookShape,
    /// Its class's price moves.
    pub moves: &'static MoveModel,
    /// `pd` and `qd`: a tick is `10^-pd` dollars, a lot `10^-qd` units; `pd + qd = 6`, so ticks ×
    /// lots are micro-dollars (D-004).
    pub price_decimals: u8,
    pub qty_decimals: u8,
    pub max_leverage: u16,
    /// Polymarket's tier table, 1 to 8 rows.
    pub tiers: &'static [Tier],
    /// The recorded mark at 2026-09-30 10:00:00 UTC.
    pub start_price: Price,
    /// Polymarket's price grid at the start price: 5 significant figures, so
    /// `10^max(0, digits(start_price) − 5)` ticks. A price crossing a power of ten changes it.
    pub price_grid: Price,
    /// This market's share of maker messages (level changes of its book).
    pub maker_weight_ppm: u32,
    /// This market's share of taker orders.
    pub taker_weight_ppm: u32,
    /// Its spread: one real tick if it is tick-bound (a median spread of one real tick: BTC, ETH,
    /// SOL, SP500, NAS100, GOLD); its own for SILVER, WTIOIL and BRENTOIL; else its class's.
    pub spread: &'static Spread,
    /// Share of its book levels that are dust ($10 to $12.50).
    pub dust_ppm: u32,
    /// The RMS of its nonzero 1-s mark moves, in thousandths of a bps: the unit of its class's
    /// move mixture, so that its 1-s standard deviation is the recorded one.
    pub move_rms_millibps: u32,
    /// The largest taker order: Polymarket's `max_market_notional`.
    pub max_notional: Micros,
}

// ---------------------------------------------------------------------------------------
// Book shape.

/// A spread model. At least one real tick, whatever the draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spread {
    /// Always one real tick: the tick-bound markets (median spread of one real tick).
    OneTick,
    /// A split lognormal in bps: the median, and the log-sd below and above it, fitted to the
    /// 10th and 90th percentiles; `centibps` holds 64 equally likely spreads.
    SplitLognormal { median_centibps: u32, sigma_below_milli: u32, sigma_above_milli: u32, centibps: Table },
}

/// Gaps between consecutive levels of one side, in real ticks: one of 1 to 10 ticks with
/// these shares, else `10 + exp(N(mu, sd))` ticks (`tail_ticks`: 64 equally likely values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gaps {
    pub one_to_ten_ppm: [u32; 10],
    pub beyond_ten_ppm: u32,
    pub tail_mu_milli: i32,
    pub tail_sd_milli: u32,
    pub tail_ticks: Table,
}

/// The sizes of one level (1 to 20) of a class's books: dust with `dust_ppm`, else clip `c` of
/// the class's menu with `clips_ppm[c]`, else a draw of `background_usd`: the other sizes
/// recorded at that level, 64 equally likely whole dollars (their own quantiles, raised to the
/// $10 minimum; bimodal, small orders and large ones, which no lognormal fits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelSizes {
    pub dust_ppm: u32,
    pub clips_ppm: &'static [u32],
    pub background_usd: Table,
}

/// The shape of one class's books. A level's size comes from that level's [`LevelSizes`]:
/// depth is not flat, level 1 is thin and the clips sit at levels 3 to 10 (D-034).
#[derive(Debug)]
pub struct BookShape {
    pub class: BookClass,
    /// The class's spread, pooled over its markets that are not tick-bound; one real tick for
    /// the majors and tradfi macro (whose other three markets have their own).
    pub spread: &'static Spread,
    /// Gaps after levels 1–4, 5–9 and 10–19, as recorded on the sides that show all 20
    /// levels, as a flow's ladders always do.
    pub gaps: [Gaps; 3],
    /// Share of all the class's levels that are dust: a market's own dust share
    /// ([`Market::dust_ppm`]) scales each level's against it.
    pub dust_ppm: u32,
    /// The clip menu: fixed notionals, in dollars, that makers stack at several levels and on
    /// both sides; a clip's notional is jittered by up to `clip_jitter_ppm` either way.
    pub clips_usd: &'static [u32],
    pub clip_jitter_ppm: u32,
    /// Level by level, best first: 20 of them (`Profile::levels_per_side`).
    pub levels: &'static [LevelSizes],
}

// ---------------------------------------------------------------------------------------
// Prices.

/// The 1-s moves of a class's marks: none with `no_move_ppm`, else a normal scale mixture in
/// units of the market's [`Market::move_rms_millibps`]: component `k` with `weights_ppm[k]`,
/// a normal of standard deviation `sd_milli[k]` thousandths. Snapped to the real grid.
#[derive(Debug)]
pub struct MoveModel {
    pub class: LeverageClass,
    pub no_move_ppm: u32,
    pub weights_ppm: [u32; 3],
    pub sd_milli: [u32; 3],
}

/// Persistent jumps of a market's mark: 1-s moves over `over_bps` whose move from the second
/// before to 5 s after is still over half `over_bps` (25 bps).
#[derive(Debug)]
pub struct Jumps {
    pub over_bps: u32,
    /// Their rate: one per this many market-seconds (the median hour's).
    pub one_in_market_seconds: u64,
    /// The same rate, per market-hour, in thousandths.
    pub per_market_hour_milli: u32,
    /// Jumps observed, from which `centibps` comes.
    pub observed: u32,
    /// Their sizes, 16 equally likely values.
    pub centibps: Table,
}

// ---------------------------------------------------------------------------------------
// Makers and takers.

/// What one maker level change is (1-s snapshots): a new level, a removed level, or a size
/// change up or down (places, cancels, total-size modifies); and which levels it touches.
#[derive(Debug)]
pub struct MakerActivity {
    pub add_ppm: u32,
    pub remove_ppm: u32,
    pub size_up_ppm: u32,
    pub size_down_ppm: u32,
    pub levels_1_5_ppm: u32,
    pub levels_6_10_ppm: u32,
    pub levels_11_20_ppm: u32,
}

/// Taker orders: how many, which side, and how they cluster within milliseconds.
#[derive(Debug)]
pub struct Takers {
    /// Share of all messages (a lower bound on the makers' side, so an upper bound here).
    pub share_of_messages_ppm: u32,
    /// Taker orders a second at Polymarket's own volume, in thousandths.
    pub events_per_s_milli: u32,
    pub buy_ppm: u32,
    /// Clusters (orders at most 50 ms apart, exchange-wide) a second at real volume.
    pub cluster_starts_per_s_milli: u32,
    /// Orders per cluster: index `n − 1` is the share of clusters of `n` orders.
    pub cluster_size_ppm: &'static [u32],
    /// Within a cluster: the next order in the same market as the one before; on the first
    /// order's side.
    pub cluster_same_market_ppm: u32,
    pub cluster_same_side_ppm: u32,
    /// Gaps inside a cluster, in ms: 16 equally likely values.
    pub cluster_gap_ms: Table,
}

/// A notional that taker orders use exactly (bots with round sizes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointMass {
    pub cents: u64,
    pub share_ppm: u32,
}

/// One component of a lognormal mixture over ln(USD).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogComponent {
    pub weight_ppm: u32,
    pub mu_milli: i32,
    pub sd_milli: u32,
}

/// USD per taker order: dust with `dust_ppm` (a draw of [`Profile::dust_draw_cents`]), a point
/// mass with its share, else the continuous part: the two-lognormal `mixture` truncated below
/// `truncate_below_cents`, as 1,024 equally likely values. Capped at the market's
/// [`Market::max_notional`]; lots rounded up so that the order reaches $10.
#[derive(Debug)]
pub struct TakerNotional {
    pub dust_ppm: u32,
    pub point_masses: &'static [PointMass],
    pub continuous_ppm: u32,
    pub truncate_below_cents: u32,
    pub mixture: [LogComponent; 2],
    pub continuous_cents: Table,
}

// ---------------------------------------------------------------------------------------
// Bursts and shocks.

/// An AR(1) process on the log of the rate, stepped once a second:
/// `s(t) = phi × s(t − 1) + N(0, innovation_sd²)`; stationary variance
/// `innovation_sd² / (1 − phi²)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ar1 {
    pub phi: f64,
    pub innovation_sd: f64,
}

/// The rate multiplier of `--bursts`: `exp(fast + slow)`, normalised by the send schedule over
/// each phase, so that the phase offers the offered rate on average (`schedule.rs`, "Bursts").
/// `exp(V / 2)` (`V` the two stationary variances' sum) would be its long-run mean; it is not
/// used (D-034). Floats: it only moves send times (PIPELINE.md 14.9).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BurstModel {
    pub fast: Ar1,
    pub slow: Ar1,
}

/// Correlated shocks: many markets moving the same way within tens of milliseconds.
#[derive(Debug)]
pub struct Shocks {
    /// Mean seconds between shocks over the whole sample, and in the median hour.
    pub pooled_every_s: u32,
    pub calm_every_s: u32,
    /// Markets per shock: the recorded discrete Pareto of this alpha (above 8 markets),
    /// clamped to `min_markets` to `max_markets`: a draw below the first counts as the first,
    /// one beyond the last as the last (`markets_ppm[n − min_markets]`). Median 14, 90th
    /// percentile 38; recorded: 14 and 31, at most 71.
    pub markets_pareto_alpha_milli: u32,
    pub min_markets: u32,
    pub max_markets: u32,
    pub markets_ppm: &'static [u32],
    /// Each mover's move, in units of its RMS of nonzero 1-s moves
    /// ([`Market::move_rms_millibps`], the detector's unit; median mover, 50th and 90th
    /// percentiles of shocks), and in bps (10th, 50th, 90th).
    pub move_sigmas_p50_milli: u32,
    pub move_sigmas_p90_milli: u32,
    pub move_centibps_p10_p50_p90: [u32; 3],
    /// The stress size of `--shock`: 2% to 6% (D-034), in ppm of the price.
    pub stress_move_ppm: [u32; 2],
    /// Time over which the movers' first trades come, for shocks within one second.
    pub spread_ms_p50: u32,
    pub spread_ms_p90: u32,
}

#[cfg(test)]
impl BookShape {
    /// The median of level `level`'s sizes (1-based), in dollars: of its dust (about $11), its
    /// clips and its background's 64 values, each with its share. For the tests.
    pub(crate) fn median_usd(&self, level: usize) -> u32 {
        let sizes = &self.levels[level - 1];
        let background = 1_000_000 - sizes.dust_ppm - sizes.clips_ppm.iter().sum::<u32>();
        // Weights in 64ths of a ppm: a background value has 1/64 of the background's share.
        let mut weighted: Vec<(u32, u64)> = vec![(11, u64::from(sizes.dust_ppm) * 64)];
        let clips = self.clips_usd.iter().zip(sizes.clips_ppm);
        weighted.extend(clips.map(|(&usd, &share)| (usd, u64::from(share) * 64)));
        weighted.extend(sizes.background_usd.0.iter().map(|&usd| (usd, u64::from(background))));
        weighted.sort_unstable();
        let half = weighted.iter().map(|(_, weight)| weight).sum::<u64>() / 2;
        let mut below = 0;
        for (usd, weight) in weighted {
            below += weight;
            if below > half {
                return usd;
            }
        }
        unreachable!("the weights sum to more than their half")
    }
}

#[cfg(test)]
mod tests;
