//! The Polymarket-shaped flow (`docs/DECISIONS.md` D-034): Polymarket Perps' 88 real
//! markets, traded with the shape of their recorded traffic (the calibrated profile,
//! `profile.rs`) at the volume the pipeline is measured at, plus two stress switches in the
//! flow (concentrated market making, correlated shocks with a liquidation cascade) and one in
//! the send schedule (bursts, `schedule.rs`). The M3 flow (`market_flow.rs`) stays the
//! conservative stress flow; this one answers "what does Polymarket's own traffic shape cost
//! the engine, at about 100 times its volume".
//!
//! **Contract.** [`generate`] turns a [`PolymarketConfig`] into a [`FlowPlan`] with the M3
//! flow's contract (`market_flow.rs`): the setup phases A, B1 and B2, then the timed flow, in
//! one ordered list of client and operator items; the same config gives the same list on
//! every machine and at every offered rate, which only changes when each item is sent
//! (14.1); a longer plan starts with every item of a shorter one. The signer and the sender
//! take this plan as they take the M3 flow's (`PlanConfig`).
//!
//! **Scale.** A second of flow time holds about `messages_per_flow_second` client messages
//! (100,000), in Polymarket's proportions, while prices move as much as they did in a
//! recorded second. Polymarket itself sent 750 to 1,700 messages a second, so at an offered
//! 100,000 a second, where flow time runs at about real time, this is its shape at about 100
//! times its volume with its real price speed (D-034, "Scale").
//!
//! **Markets.** The profile's 88, under Polymarket's instrument ids (1 to 90), each with its
//! real decimals, maximum leverage, tier table, start price and price grid ([`market_params`]):
//! - Band: `400,000 / max_leverage` ppm (8,000 at 50x, 20,000 at 20x, 40,000 at 10x, 80,000
//!   at 5x, 133,333 at 3x). Polymarket's own bands (`1 / max_leverage`) break our band rule 1
//!   (D-015); these pass both rules, with `20 × Lmax × (band + fee)` from 8,029,980 to
//!   8,400,000 against 9,000,000, and `min_price` at least 2,173 ticks against rule 2's floor
//!   of about `20 × Lmax`.
//! - Fees: the M3 classes' (the profile has none): 400 / 100 ppm (taker / maker) at 50x,
//!   400 / 125 at 20x, 500 / 100 at 10x and below.
//! - Price range: half to twice the start price, as in the M3 flow. A book costs 8 bytes per
//!   tick of its range, `1.5 × F0` ticks: the 88 start prices sum to 7,134,135 ticks, so 85.6
//!   MB of levels in all (XRP-USD alone 18.1 MB, BTC-USD 10.0 MB), against the M3 flow's
//!   110 MB. The rest of the engine's reservation is per market and per account, with the M3
//!   flow's capacities ([`PolymarketConfig::engine_options`]): after setup A and the core's
//!   prefault the engine holds 249 MB, against the M3 flow's 231 MB (both measured locally).
//!   A narrower range would save little: the other 163 MB is 1.9 MB per market.
//!
//! **Phases** (14.4; the harness puts a barrier between them):
//! - **A** (operator): per market in id order, `SetMarketParams`, its tier table (one
//!   `SetRiskTier` per row) and its first mark at the start price; the insurance fund's
//!   capital; every client's deposit, in id order; then the leverage of every maker in every
//!   market it quotes (5, or the market's maximum if lower), and of every high-leverage and
//!   cascade account in its market (the market's maximum).
//! - **B1** (client): every market's two ladders, 20 quotes a side, market by market.
//! - **B2** (client): the high-leverage accounts' first positions, 3 IOCs each, then (with
//!   the stress shock) the cascade accounts', one IOC each, at the start marks: as in the M3
//!   flow, entries at a frozen mark, so a cohort's entries in a market are close together.
//! - **Timed**: the events below.
//!
//! **The timed flow** is an event simulation in flow time (integer nanoseconds), which only
//! orders the items. Events, processed in order of time, then of this list, then of their
//! market:
//!
//! | Event | When | Emits |
//! |---|---|---|
//! | `MarkTick(m)` | every 200 ms per market, markets spread evenly over the 200 ms | every 5th tick first a 1-s fair-value step; the mark; the ladders following a move |
//! | `ShockMove(m)` | within 52 ms of a shock | the market's move, its mark, its ladders quoted again |
//! | `Shock` | every `every_ns` (`--shock`) | nothing itself: draws the movers and schedules their moves |
//! | `Rebuild` | half-way between shocks | the movers' cascade accounts re-enter, one IOC each |
//! | `HighLeverage` | at 100 ppm of the messages: every 100 ms at 100,000 a flow-second | one high-leverage account adds to its position |
//! | `TakerFollow` | a drawn gap after the order before | the next taker order of a cluster |
//! | `TakerTick` | every 100 µs | a taker cluster's first order, with the calibrated chance |
//! | `Message` | the message clock | one maker event: one to a few maker messages |
//!
//! **Makers** (D-034, "Maker activity"). The message clock sends `messages_per_flow_second ×
//! (1 − 695 ppm)` maker messages a second: every maker message advances it, whichever event
//! emitted it (a maker event, or ladders following a move, a jump or a shock), so a
//! flow-second holds `messages_per_flow_second` client messages. A maker event is a re-price
//! or a resize (below); it picks a market by the profile's maker weights, then a side, a
//! level band and a rank in it:
//! - A **re-price** cancels the quote and its maker places a new one between the quotes
//!   around it, the two gaps next to it drawn together (`market.rs`, "Gaps"), so the ladder
//!   keeps the profile's gaps: one remove and one add. At rank 0 it is a **spread change**:
//!   both best quotes move to a new spread around the fair value, drawn together with the
//!   gaps behind them, so the book keeps the market's spread model and the profile's gaps
//!   (where no spread fits ahead of the second quotes, a fresh one, and any quote then at or
//!   ahead of a new best moves behind it too).
//! - A **resize** sends a new total size at the same price: a size up or down, half and half,
//!   since the new size is a fresh draw.
//! - Which of the two: whichever keeps the running split of maker messages at the recorded
//!   one, a re-price while cancels and places are below 73.2% of the maker messages so far,
//!   else a resize ([`MakerMix::reprice_next`]; about 54% of events are re-prices). A fixed
//!   chance would drift from it, since ladders following a move (below) and a fresh spread
//!   move more quotes than a maker event's one or two. So the messages split as recorded:
//!   36.6% adds, 36.6% removes, 13.4% size ups, 13.4% size downs.
//! - Levels: 52% of changes at levels 1–5, 27.7% at 6–10, 20.3% at 11–20, a rank uniform
//!   within its band. A spread change is two level-1 changes, so for re-prices band 1–5 is
//!   picked 5/6 as often, and the bands renormalised ([`MakerMix`]).
//! - Sizes (`market.rs`), level by level as recorded (level 1 thin, the clips at levels 3 to
//!   10): dust ($10.50 to $11.60, mostly deep), the class's clips ($6,250 in crypto; $25k,
//!   $50k and $100k in tradfi; ±3%), or that level's recorded background sizes.
//! - Accounts: 60 makers in 20 groups of 3, each group quoting the markets dealt to it
//!   heaviest first so that every group carries about 1/20 of the maker messages
//!   ([`PolymarketConfig::market_makers`]); so each maker sends about 1/60 of all traffic.
//!   In setup the bid of rank `r` of market `i` (in id order) goes to its group's maker
//!   `(r + i) mod 3` and the ask to maker `(r + 1 + i) mod 3`, and a quote keeps its maker
//!   when it moves. An account verifies on gateway `account mod N`, and 60 is a multiple of
//!   every gateway count from 1 to 6 and of 10, 12, 15, 20 and 30, so the makers spread
//!   evenly over the gateways (12 makers would put 2 on gateways 1 and 2 of 10, each then
//!   carrying 1.7 times the mean). `--makers K` (`makers_k`, 1 to 20): `K` makers quote every
//!   market, each about 1/K of all traffic. At most 20, the quotes a side: with more, the
//!   ranks would run out before the makers, and the others would never quote.
//! - Deposits of $1,000,000,000 each, so no quote ever lacks margin, whatever it and the
//!   positions takers leave behind are worth.
//!
//! **Takers** (D-034, "Takers"). IOCs are 695 ppm of all client messages: the high-leverage
//! cohort's 100 (below), and the takers' 595. A taker tick every 100 µs starts a cluster with
//! the chance that gives the takers' share, from the profile's cluster sizes (84% single
//! orders, mean 1.28). A cluster is one account's burst: its account, its first market (by
//! taker weight) and side (51% buys); each next order a drawn gap later (p50 7 ms), in the
//! same market as the one before 62% of the time and on the first order's side 90%.
//! Each order's notional: 14.6% dust ($10.50 to $11.60), 10.7% round sizes ($1,000 is 6.6%),
//! else the two-lognormal mixture's table; capped at the market's maximum order notional. It
//! is an IOC at the band's edge (`market.rs`, `MarketState::ioc`): it sweeps the book
//! until filled. 1,000 taker accounts of $10,000,000 each, at leverage 1.
//!
//! **Prices** (D-034, "Price dynamics"). Each market's fair value takes one step a second
//! (every 5th mark tick): with the market's class's no-move share (62% to 74%) none; else
//! `|move| = rms × sd_k × |z|`, with `rms` the market's own RMS of nonzero 1-s moves,
//! component `k` of its class's 3-part normal mixture, `|z|` from the profile's half-normal
//! table, and a fair coin for the direction; in whole real ticks, at least one. With a
//! chance of 1 in 70,400 (0.051 per market-hour, the recorded rate over 50 bps) the step is a
//! persistent jump instead, of 50 to 123 bps (the recorded sizes). The mark is the fair value,
//! every 200 ms and at once after every change, so every order is priced against the latest
//! mark. After a move the ladders follow it whole, every quote moving by the move with its
//! maker and size (`market.rs`, "Gaps"), their messages taken from the message clock; after
//! a jump they are pulled and quoted again.
//!
//! **High-leverage cohort.** 2 accounts per market, one long, one short, at the market's
//! maximum leverage, $100,000 each: 3 IOCs of $2,000 each in B2, then adds of $2,000 at 100
//! ppm of the client messages, out of the IOCs' 695 (every 100 ms at 100,000 a second, each
//! account about every 18 s): a market by taker weight, then one of its two accounts. So the
//! IOCs stay the recorded 695 ppm, spread over the markets as the takers' are, 14% of them a
//! flat $2,000. At the maximum leverage a fresh position is liquidated by an adverse move of
//! about `0.5 / max_leverage`: 1% at 50x, 2.5% at 20x. The recorded jumps are 0.5% to 1.2%,
//! so a jump liquidates the wrong side only on a 50x market and only if it is large (about 1
//! jump in 8). The recording has no such jump: the 50x markets (SP500, NAS100, BTC, ETH) had
//! none over 50 bps, and the 20x ones 0.011 per market-hour; this flow applies one pooled rate
//! to every market, a simplification, so the default flow's rare liquidations are its
//! artefact, not Polymarket's. The stress shock is the liquidation switch.
//!
//! **Stress switches** (D-034):
//! - `makers_k` (`--makers K`): above.
//! - `shock` (`--shock`, [`ShockConfig`]): every `every_ns` of flow time, `N` markets (the
//!   profile's Pareto, α 1.41, clamped to 14 to 88: median 14, 90th percentile 38) move the
//!   same way, each at a drawn time within 52 ms (the recorded median spread), each with its
//!   mark at once and its ladders quoted again. [`ShockConfig::real`]: the recorded sizes, 4
//!   to 8 of each market's own `rms` (median 5.1, 90th percentile 6.5: about 11 bps), each
//!   mover keeping the shared direction with 95% chance. [`ShockConfig::stress`]: 2% to 6%,
//!   all one way, and a **cascade cohort**: 8 accounts per market (4 long, 4 short) at the
//!   maximum leverage, entering together at the start mark (B2) and again at each `Rebuild`
//!   for the last shock's movers, so their entries are close and one mark liquidates the
//!   wrong side of a market together (on 50x and 20x markets, and 10x ones for moves over
//!   about 5%).
//! - Bursts (`--bursts`) live in the send schedule (`schedule.rs`), never in the plan.
//!
//! **Open loop.** The generator never sees the engine: it doesn't know about fills,
//! liquidations or band sweeps, only what it sent (14.5). So a cancel or a resize can name a
//! quote a taker has filled, and after a shock or jump a cancel can name a quote the mark's
//! band sweep has removed; the engine rejects those (`UnknownOrder`), as it would real flow.
//! And across accounts the sequencer promises no order (PIPELINE.md 9.1): a spread change, a
//! ladder following a move or a requote cancels one maker's quote and places another
//! maker's at or through its price, so through the pipeline the place can come first and be
//! rejected `PostOnlyWouldCross` (about 0.01% of places at 100k/s with shocks; never in plan
//! order, `market.rs`).
//!
//! **Randomness** (14.6). Each concern draws from its own [`SplitMix64`] stream
//! ([`stream_ids`]): per market `FAIR(m)` (the fair value) and `MM(m)` (its makers' changes
//! and the sizes, gaps and spreads they draw); and `CLOCK` (the market of each maker event),
//! `TAKER` (clusters and taker orders), `HL` (which high-leverage account adds: its market by
//! taker weight, then which of the market's accounts), `SHOCK` and `SCHEDULE` (the send
//! schedule). Within a stream, draws happen in the order the code's docs list them.
//!
//! **Arithmetic.** Integers only, as in every loadgen flow: every distribution comes from the
//! profile as a table of equally likely values or as shares in ppm (`profile.rs`), drawn
//! with [`SplitMix64`]; products that could pass `i64` are `i128`. No float decides what the
//! plan contains, so it is the same on every machine. (Floats appear only in the send
//! schedule, which decides when items go, never what they are.)
//!
//! **Invariants** (checked by the tests): per account, nonces are 1, 2, 3, … over every
//! client item and order sequences 1, 2, 3, … over its places; every cancel and modify names
//! a quote of the same account that the generator still holds; every price and mark is on
//! its real grid and inside its market's range; every order is inside the band of the
//! generator's latest mark; the generator's own book is never crossed.
//!
//! **Complexity.** A maker event costs O(log E) for the event queue (`E`, about 100 events)
//! plus O(log 88) to pick its market, O(20) per quote it touches and a few hundred steps per
//! gap or spread it draws (`market.rs`). The whole plan is generated before the run, which
//! may allocate; the send path never runs this code.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use engine::command::{Command, Deposit, PlaceOrder, SetLeverage, SetMark, SetMarketParams, SetRiskTier};
use engine::engine::{EngineOptions, FUND};
use engine::types::{AccountId, MarketId, Micros, Price, Side};
use k256::sha2::{Digest, Sha256};

use super::profile::{Market, POLYMARKET, Profile, pick};
use super::{
    Clients, ENGINE_CAPACITY, FlowPlan, HIGH_LEVERAGE_BASE, Item, Jump, MAKER_BASE, PlanConfig, SetupPhases,
    TAKER_BASE, dollars, stream,
};
use crate::SplitMix64;

mod market;

use market::MarketState;
pub use market::{on_grid, real_tick, snap_down, snap_up};

/// Parts per million.
const MILLION: u64 = 1_000_000;
/// Nanoseconds in a second.
const SECOND_NS: u64 = 1_000_000_000;
/// Flow time between two taker ticks: 100 µs.
const TAKER_TICK_NS: u64 = 100_000;

/// The random streams' ids (14.6): the M3 flow's for the same concerns (a per-market stream
/// adds the market's id), and two of this flow's own.
pub mod stream_ids {
    pub use super::super::stream_ids::{FAIR, HASH_SEED, HL, MM, SCHEDULE, TAKER};
    /// `CLOCK`: the market of each maker event.
    pub const CLOCK: u64 = 0xA_0000;
    /// `SHOCK`: each shock's markets, direction, times and sizes.
    pub const SHOCK: u64 = 0xB_0000;
}

// ---------------------------------------------------------------------------------------
// Markets.

/// The price band of a market of `max_leverage` (module docs): `400,000 / max_leverage` ppm.
pub fn band_ppm(max_leverage: u16) -> u32 {
    400_000 / u32::from(max_leverage)
}

/// The taker and maker fees of a market of `max_leverage`, in ppm (module docs): the M3
/// flow's classes (`MARKET_CLASSES`).
pub fn fees_ppm(max_leverage: u16) -> (i32, i32) {
    match max_leverage {
        50.. => (400, 100),
        20..50 => (400, 125),
        _ => (500, 100),
    }
}

/// The parameters of `market` (module docs): half to twice its start price, its maximum
/// leverage, and this flow's band and fees for that leverage.
pub fn market_params(market: &Market) -> SetMarketParams {
    let (taker_fee_ppm, maker_fee_ppm) = fees_ppm(market.max_leverage);
    SetMarketParams {
        min_price: Price::new(market.start_price / 2),
        max_price: Price::new(2 * market.start_price),
        maker_fee_ppm,
        taker_fee_ppm,
        price_band_ppm: band_ppm(market.max_leverage),
        market: market.market_id(),
        max_leverage: market.max_leverage,
    }
}

// ---------------------------------------------------------------------------------------
// Accounts.

/// The cascade cohort's accounts are 7,001 onwards (the M3 flow's thin layer's ids): a base
/// account number, as the M3 flow's are.
pub const CASCADE_BASE: u32 = 7_001;

/// Which cohort an account belongs to, with what the flow needs to know about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cohort {
    /// Maker `index` (0-based).
    Maker {
        index: u32,
    },
    Taker,
    /// Trades only `market`, only on `side` (buy: long).
    HighLeverage {
        market: MarketId,
        side: Side,
    },
    /// The same, for the stress shock's cascade cohort.
    Cascade {
        market: MarketId,
        side: Side,
    },
}

// ---------------------------------------------------------------------------------------
// The configuration.

/// How far a shock's movers move (module docs, "Stress switches").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShockSize {
    /// The recorded sizes, in each market's own `rms`, each mover keeping the shared
    /// direction with 95% chance.
    Calibrated,
    /// 2% to 6% of the price, all one way.
    Stress,
}

/// Correlated shocks (`--shock`; module docs, "Stress switches").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShockConfig {
    /// Flow time between shocks, the first one this long after the timed flow starts: 10 s.
    /// The recorded rate is one every 347 s over the whole sample (900 s in the median hour),
    /// so this is a stress switch: a 30-s window at 100,000 a second holds three.
    pub every_ns: u64,
    pub size: ShockSize,
    /// The movers' marks come within this much flow time of the shock: 52 ms, the recorded
    /// median.
    pub spread_ns: u64,
    /// Cascade accounts per market, the first half long, the second short: 0 or 8.
    pub cascade_per_market: u32,
    /// Each cascade IOC's notional: $5,000.
    pub cascade_notional: Micros,
    /// $1,000,000 each, at the market's maximum leverage.
    pub cascade_deposit: Micros,
}

impl ShockConfig {
    /// The recorded shock sizes (about 11 bps), every 10 s; no cascade cohort.
    pub fn real() -> ShockConfig {
        ShockConfig {
            every_ns: 10 * SECOND_NS,
            size: ShockSize::Calibrated,
            spread_ns: u64::from(POLYMARKET.shocks.spread_ms_p50) * 1_000_000,
            cascade_per_market: 0,
            cascade_notional: dollars(5_000),
            cascade_deposit: dollars(1_000_000),
        }
    }

    /// Moves of 2% to 6%, every 10 s, with a cascade cohort of 8 accounts per market.
    pub fn stress() -> ShockConfig {
        ShockConfig { size: ShockSize::Stress, cascade_per_market: 8, ..ShockConfig::real() }
    }
}

/// Everything that shapes the flow that isn't the profile's (module docs).
/// [`PolymarketConfig::default`] is D-034's flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolymarketConfig {
    /// Seeds every random stream, the account keys (14.8) and the engine's hash seed.
    pub seed: u64,
    /// Client messages per second of flow time: 100,000.
    pub messages_per_flow_second: u64,

    // ---- Makers.
    /// Maker accounts without `makers_k`: 60, a multiple of the usual gateway counts (module
    /// docs, "Makers").
    pub makers: u32,
    /// Makers quoting each market without `makers_k`: 3.
    pub makers_per_market: u32,
    /// `--makers K`: `K` makers, 1 to 20, quote every market. `None`: the 60 above.
    pub makers_k: Option<u32>,
    /// $1,000,000,000 each.
    pub maker_deposit: Micros,
    /// 5 in every market it quotes, or the market's maximum if lower.
    pub maker_leverage: u16,

    // ---- Takers.
    /// 1,000 accounts.
    pub takers: u32,
    /// $10,000,000 each, at the default leverage of 1.
    pub taker_deposit: Micros,

    // ---- Prices.
    /// A fair-value step is a jump with probability `1 / jump_one_in`: 1 in 70,400, the
    /// profile's rate (tests raise it).
    pub jump_one_in: u64,

    // ---- The high-leverage cohort.
    /// Per market: 2, the first half long, the second half short.
    pub high_leverage_per_market: u32,
    /// Each IOC's notional: $2,000.
    pub high_leverage_notional: Micros,
    /// IOCs per account in B2: 3.
    pub high_leverage_setup_iocs: u32,
    /// The adds' share of client messages, taken out of the IOCs' 695 ppm: 100 (module
    /// docs). They come evenly spaced in flow time, the first half a period in.
    pub high_leverage_ppm: u64,
    /// $100,000 each, at the market's maximum leverage.
    pub high_leverage_deposit: Micros,

    /// The insurance fund's capital: $1,000, small on purpose as in the M3 flow, so that a
    /// cascade past bankruptcy prices shows as a shortfall.
    pub fund_deposit: Micros,

    /// `--shock`: `None` by default.
    pub shock: Option<ShockConfig>,
}

/// Bumped whenever this generator's algorithm changes, so a `presigned.bin` made by an older
/// one is refused (its digest no longer matches). A new profile table changes the digest by
/// itself: it hashes the table's `sha256`.
pub const FLOW_VERSION: u64 = 2;

impl Default for PolymarketConfig {
    /// D-034's flow: the values quoted in each field's docs.
    fn default() -> PolymarketConfig {
        PolymarketConfig {
            seed: 1,
            messages_per_flow_second: 100_000,
            makers: 60,
            makers_per_market: 3,
            makers_k: None,
            maker_deposit: dollars(1_000_000_000),
            maker_leverage: 5,
            takers: 1_000,
            taker_deposit: dollars(10_000_000),
            jump_one_in: POLYMARKET.jumps.one_in_market_seconds,
            high_leverage_per_market: 2,
            high_leverage_notional: dollars(2_000),
            high_leverage_setup_iocs: 3,
            high_leverage_ppm: 100,
            high_leverage_deposit: dollars(100_000),
            fund_deposit: dollars(1_000),
            shock: None,
        }
    }
}

impl PolymarketConfig {
    /// Every market of the flow, in id order: the profile's.
    pub fn markets(&self) -> &'static [Market] {
        POLYMARKET.markets
    }

    /// Maker accounts: `K`, or `makers`.
    pub fn maker_accounts(&self) -> u32 {
        self.makers_k.unwrap_or(self.makers)
    }

    /// The makers of each market, by index (in id order): its first account's number and how
    /// many (module docs, "Makers"). With `makers_k`, all `K` quote every market. Otherwise the
    /// markets are dealt to the `makers / makers_per_market` groups heaviest first, by maker
    /// weight, each to the group with the least weight so far (the lowest group on a tie), so
    /// that the groups carry about the same share of maker messages (within 4% for the
    /// default 20); group `g` is makers `g × makers_per_market` onwards (0-based).
    /// O(M log M + M × G) for `M` markets and `G` groups.
    pub fn market_makers(&self) -> Vec<(u32, u32)> {
        let markets = self.markets();
        if let Some(k) = self.makers_k {
            return vec![(MAKER_BASE, k); markets.len()];
        }
        let groups = (self.makers / self.makers_per_market) as usize;
        let mut weight = vec![0u64; groups];
        let mut heaviest_first: Vec<usize> = (0..markets.len()).collect();
        heaviest_first.sort_by_key(|&index| (Reverse(markets[index].maker_weight_ppm), index));
        let mut makers = vec![(0, 0); markets.len()];
        for index in heaviest_first {
            // The first of the lightest groups.
            let group = (0..groups).min_by_key(|&group| weight[group]).expect("at least one group");
            weight[group] += u64::from(markets[index].maker_weight_ppm);
            makers[index] = (MAKER_BASE + group as u32 * self.makers_per_market, self.makers_per_market);
        }
        makers
    }

    /// High-leverage accounts in all.
    fn high_leverage_accounts(&self) -> u32 {
        self.markets().len() as u32 * self.high_leverage_per_market
    }

    /// The high-leverage adds' share of client messages, in ppm: `high_leverage_ppm`, or 0
    /// without the cohort.
    fn high_leverage_share(&self) -> u64 {
        if self.high_leverage_per_market > 0 { self.high_leverage_ppm } else { 0 }
    }

    /// Flow time between two high-leverage adds: `high_leverage_ppm` of
    /// `messages_per_flow_second` a second, so `10^9 × 10^6 / (R × ppm)` ns (100 ms at the
    /// defaults).
    fn high_leverage_every_ns(&self) -> u64 {
        SECOND_NS * MILLION / (self.messages_per_flow_second * self.high_leverage_ppm)
    }

    /// Cascade accounts in all: none without the shock.
    fn cascade_accounts(&self) -> u32 {
        self.markets().len() as u32 * self.shock.map_or(0, |shock| shock.cascade_per_market)
    }

    /// Account `base + index` of a cohort of `per_market` accounts per market: its market's
    /// index, and its side (long for the first half of each market's accounts).
    fn cohort_member(index: u32, per_market: u32) -> (usize, Side) {
        let side = if index % per_market < per_market / 2 { Side::Buy } else { Side::Sell };
        ((index / per_market) as usize, side)
    }

    /// High-leverage account `index` (0-based): its account, its market's index and its side.
    pub fn high_leverage(&self, index: u32) -> (AccountId, usize, Side) {
        let (market, side) = Self::cohort_member(index, self.high_leverage_per_market);
        (AccountId::new(HIGH_LEVERAGE_BASE + index), market, side)
    }

    /// Cascade account `index` (0-based): its account, its market's index and its side.
    pub fn cascade(&self, index: u32) -> (AccountId, usize, Side) {
        let per_market = self.shock.map_or(1, |shock| shock.cascade_per_market);
        let (market, side) = Self::cohort_member(index, per_market);
        (AccountId::new(CASCADE_BASE + index), market, side)
    }

    /// The cohort of `account`, or `None` if the flow doesn't use it.
    pub fn cohort_of(&self, account: AccountId) -> Option<Cohort> {
        let market_id = |index: usize| self.markets()[index].market_id();
        let number = account.get();
        let in_cohort = |base: u32, count: u32| (base..base + count).contains(&number);
        if in_cohort(MAKER_BASE, self.maker_accounts()) {
            Some(Cohort::Maker { index: number - MAKER_BASE })
        } else if in_cohort(TAKER_BASE, self.takers) {
            Some(Cohort::Taker)
        } else if in_cohort(HIGH_LEVERAGE_BASE, self.high_leverage_accounts()) {
            let (_, market, side) = self.high_leverage(number - HIGH_LEVERAGE_BASE);
            Some(Cohort::HighLeverage { market: market_id(market), side })
        } else if in_cohort(CASCADE_BASE, self.cascade_accounts()) {
            let (_, market, side) = self.cascade(number - CASCADE_BASE);
            Some(Cohort::Cascade { market: market_id(market), side })
        } else {
            None
        }
    }

    /// The deposit of a client account, by its cohort.
    fn deposit_of(&self, cohort: Cohort) -> Micros {
        match cohort {
            Cohort::Maker { .. } => self.maker_deposit,
            Cohort::Taker => self.taker_deposit,
            Cohort::HighLeverage { .. } => self.high_leverage_deposit,
            Cohort::Cascade { .. } => self.shock.map_or(Micros::ZERO, |shock| shock.cascade_deposit),
        }
    }

    /// Chance, in ppm, that a taker tick starts a cluster (module docs, "Takers"): taker
    /// orders are `share` of `R` messages a second (the IOCs' share less the high-leverage
    /// adds'), `R × share / 10^6` of them in clusters of mean size `S / 10^6`
    /// (`S` = `Σ n × share(n)`), so `R × share / S` clusters a second, `R × share × 100 / S` ppm
    /// of the 10,000 ticks a second.
    fn taker_start_ppm(&self) -> u64 {
        let takers = &POLYMARKET.takers;
        let mean_size_ppm: u64 = takers
            .cluster_size_ppm
            .iter()
            .enumerate()
            .map(|(n, &share)| (n as u64 + 1) * u64::from(share))
            .sum();
        let share = u64::from(takers.share_of_messages_ppm) - self.high_leverage_share();
        self.messages_per_flow_second * share * 100 / mean_size_ppm
    }

    /// Maker messages a second of flow time on the message clock: all messages but the IOCs'
    /// share (the takers' and the high-leverage adds').
    fn maker_rate(&self) -> u64 {
        let takers =
            self.messages_per_flow_second * u64::from(POLYMARKET.takers.share_of_messages_ppm) / MILLION;
        self.messages_per_flow_second - takers
    }

    /// The engine options for runs of this flow: the M3 flow's capacities (4,096 orders and
    /// slots per market, 4,096 accounts), with the hash seed drawn from the flow's seed.
    pub fn engine_options(&self) -> EngineOptions {
        EngineOptions {
            order_capacity: ENGINE_CAPACITY,
            id_hash_seed: stream(self.seed, stream_ids::HASH_SEED).next_u64(),
            scratch_capacity: ENGINE_CAPACITY,
            account_capacity: ENGINE_CAPACITY,
            slot_capacity: ENGINE_CAPACITY,
        }
    }

    /// Refuses a config the generator can't run: a cohort too large for its id range or for
    /// the engine's capacity, an empty cohort a stream would draw from, makers who would never
    /// quote, a zero period, or a rate at which a taker tick would need a chance above 1.
    pub fn check(&self) -> Result<(), String> {
        let c = self;
        let levels = POLYMARKET.levels_per_side;
        // Cohort sizes in u64, so that an absurd config is refused rather than overflowing.
        let markets = c.markets().len() as u64;
        let high_leverage = markets * u64::from(c.high_leverage_per_market);
        let cascade_per_market = c.shock.map_or(0, |shock| u64::from(shock.cascade_per_market));
        let clients = u64::from(c.maker_accounts())
            + u64::from(c.takers)
            + high_leverage
            + markets * cascade_per_market;
        let shock_ok = c.shock.is_none_or(|shock| {
            shock.spread_ns < shock.every_ns / 2
                && shock.cascade_per_market <= 100
                && (shock.cascade_per_market == 0 || shock.cascade_notional >= dollars(10))
                && shock.cascade_deposit > Micros::ZERO
        });
        let rules = [
            // Up to 10M, a taker tick's chance stays below 1 (54% at 10M).
            (
                (1_000..=10_000_000).contains(&c.messages_per_flow_second),
                "1,000 to 10,000,000 messages a second of flow time",
            ),
            // A market's makers hold its 20 quotes a side by rank, so a 21st maker of a market
            // would never quote there; and a group of makers beyond the 88th market's would
            // never quote at all.
            (
                (1..=1_000).contains(&c.makers)
                    && (1..=levels).contains(&c.makers_per_market)
                    && c.makers.is_multiple_of(c.makers_per_market)
                    && u64::from(c.makers / c.makers_per_market) <= markets,
                "1 to 1,000 makers in whole groups of 1 to 20, at most one group a market",
            ),
            (c.makers_k.is_none_or(|k| (1..=levels).contains(&k)), "makers_k of 1 to 20, the quotes a side"),
            (c.maker_leverage >= 1, "a maker leverage of at least 1"),
            ((1..=HIGH_LEVERAGE_BASE - TAKER_BASE).contains(&c.takers), "1 to 4,000 takers"),
            (c.jump_one_in >= 1, "a jump chance of 1 in at least 1"),
            (
                high_leverage <= u64::from(CASCADE_BASE - HIGH_LEVERAGE_BASE),
                "at most 2,000 high-leverage accounts",
            ),
            // The adds come out of the IOCs' share, and leave the takers some of it.
            (
                c.high_leverage_per_market == 0
                    || ((1..u64::from(POLYMARKET.takers.share_of_messages_ppm))
                        .contains(&c.high_leverage_ppm)
                        && c.high_leverage_notional >= dollars(10)),
                "high-leverage adds of 1 to 694 ppm of the messages, of at least $10",
            ),
            (
                shock_ok,
                "shocks more than two spreads apart, at most 100 cascade accounts per market of $10 or more",
            ),
            (clients < ENGINE_CAPACITY as u64, "fewer client accounts than the engine's 4,096"),
            (
                [c.maker_deposit, c.taker_deposit, c.high_leverage_deposit, c.fund_deposit]
                    .iter()
                    .all(|&d| d > Micros::ZERO),
                "deposits above zero",
            ),
        ];
        match rules.iter().find(|(holds, _)| !holds) {
            Some((_, rule)) => Err(format!("invalid Polymarket flow config: needs {rule}: {c:?}")),
            None => Ok(()),
        }
    }

    /// SHA-256 of the generator's version, the profile's name and the SHA-256 of the JSON its
    /// table was generated from, and every field, in order: two configs with the same digest
    /// generate the same plan, and a regenerated table gives a new digest by itself. It never
    /// equals an M3 flow's digest, whose hash starts with another tag.
    pub fn digest(&self) -> [u8; 32] {
        // Destructured, so that a new field that isn't hashed here is a compile error.
        let PolymarketConfig {
            seed,
            messages_per_flow_second,
            makers,
            makers_per_market,
            makers_k,
            maker_deposit,
            maker_leverage,
            takers,
            taker_deposit,
            jump_one_in,
            high_leverage_per_market,
            high_leverage_notional,
            high_leverage_setup_iocs,
            high_leverage_ppm,
            high_leverage_deposit,
            fund_deposit,
            shock,
        } = *self;
        // An absent option hashes as a 0 flag, a present one as a 1 flag and its values.
        let makers_k = makers_k.map_or([0, 0], |k| [1, u64::from(k)]);
        let shock = match shock {
            None => [0; 7],
            Some(ShockConfig {
                every_ns,
                size,
                spread_ns,
                cascade_per_market,
                cascade_notional,
                cascade_deposit,
            }) => {
                let size = match size {
                    ShockSize::Calibrated => 0,
                    ShockSize::Stress => 1,
                };
                [
                    1,
                    every_ns,
                    size,
                    spread_ns,
                    u64::from(cascade_per_market),
                    cascade_notional.micros() as u64,
                    cascade_deposit.micros() as u64,
                ]
            }
        };
        let fields = [
            FLOW_VERSION,
            seed,
            messages_per_flow_second,
            u64::from(makers),
            u64::from(makers_per_market),
            makers_k[0],
            makers_k[1],
            maker_deposit.micros() as u64,
            u64::from(maker_leverage),
            u64::from(takers),
            taker_deposit.micros() as u64,
            jump_one_in,
            u64::from(high_leverage_per_market),
            high_leverage_notional.micros() as u64,
            u64::from(high_leverage_setup_iocs),
            high_leverage_ppm,
            high_leverage_deposit.micros() as u64,
            fund_deposit.micros() as u64,
        ];
        let mut hasher = Sha256::new();
        hasher.update(b"perps-loadgen polymarket flow");
        hasher.update(POLYMARKET.name.as_bytes());
        hasher.update(POLYMARKET.sha256.as_bytes());
        for field in fields.into_iter().chain(shock) {
            hasher.update(field.to_le_bytes());
        }
        hasher.finalize().into()
    }

    /// Every client account, in id order: the makers, the takers, the high-leverage accounts
    /// and the cascade cohort. Each needs a key (14.8). Not the fund, which can't trade.
    pub fn client_accounts(&self) -> Vec<AccountId> {
        let mut numbers: Vec<u32> = (MAKER_BASE..MAKER_BASE + self.maker_accounts()).collect();
        numbers.extend(TAKER_BASE..TAKER_BASE + self.takers);
        numbers.extend(HIGH_LEVERAGE_BASE..HIGH_LEVERAGE_BASE + self.high_leverage_accounts());
        numbers.extend(CASCADE_BASE..CASCADE_BASE + self.cascade_accounts());
        numbers.into_iter().map(AccountId::new).collect()
    }
}

impl PlanConfig for PolymarketConfig {
    fn seed(&self) -> u64 {
        self.seed
    }

    fn digest(&self) -> [u8; 32] {
        PolymarketConfig::digest(self)
    }

    fn client_accounts(&self) -> Vec<AccountId> {
        PolymarketConfig::client_accounts(self)
    }

    fn engine_options(&self) -> EngineOptions {
        PolymarketConfig::engine_options(self)
    }
}

// ---------------------------------------------------------------------------------------
// The maker mix.

/// How maker events are chosen (module docs, "Makers"), from the profile's `maker_activity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MakerMix {
    /// The recorded share of adds and removes among maker messages, in ppm: 732,000.
    pub moves_ppm: u64,
    /// The shares of the level bands (1–5, 6–10, 11–20) of re-prices and of resizes.
    pub reprice_bands: [u32; 3],
    pub resize_bands: [u32; 3],
}

impl MakerMix {
    /// The mix of `profile` (module docs, "Makers").
    pub fn of(profile: &Profile) -> MakerMix {
        let activity = &profile.maker_activity;
        // A re-price at rank 0 moves both best quotes, so a pick of band 1–5 (ranks 0 to 4)
        // is `(2 + 4) / 5 = 6/5` re-prices on average: band 1–5 is picked 5/6 as often.
        let bands = [activity.levels_1_5_ppm, activity.levels_6_10_ppm, activity.levels_11_20_ppm];
        let weights = [u64::from(bands[0]) * 5 / 6, u64::from(bands[1]), u64::from(bands[2])];
        let total: u64 = weights.iter().sum();
        let first = (weights[0] * MILLION / total) as u32;
        let second = (weights[1] * MILLION / total) as u32;
        MakerMix {
            moves_ppm: u64::from(activity.add_ppm + activity.remove_ppm),
            reprice_bands: [first, second, MILLION as u32 - first - second],
            resize_bands: bands,
        }
    }

    /// True if the next maker event should be a re-price: while cancels and places (`moves`)
    /// are below their recorded share of the maker messages so far (`moves + resizes`); else
    /// a resize (module docs, "Makers").
    pub fn reprice_next(&self, moves: u64, resizes: u64) -> bool {
        u128::from(moves) * u128::from(MILLION) < u128::from(self.moves_ppm) * u128::from(moves + resizes)
    }
}

// ---------------------------------------------------------------------------------------
// The generator.

/// The next orders of a taker cluster (module docs, "Takers").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Follow {
    account: AccountId,
    /// The market of the order before, by index.
    market: u32,
    /// The side of the cluster's first order.
    first_buy: bool,
    /// Orders still to come, this one included.
    remaining: u32,
}

/// One shock's move of one market: its direction, and its size (in thousandths of the
/// market's `rms` for [`ShockSize::Calibrated`], in ppm of the price for [`ShockSize::Stress`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ShockMove {
    market: u32,
    up: bool,
    size: u32,
}

/// The events of the timed flow, declared in tie-rank order (module docs, "The timed flow"):
/// at equal flow times a `MarkTick` goes first, and so on; the derived order then compares
/// their fields (the market first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    /// The market's index.
    MarkTick(u32),
    ShockMove(ShockMove),
    Shock,
    Rebuild,
    HighLeverage,
    TakerFollow(Follow),
    TakerTick,
    Message,
}

/// An event due at flow time `time`: ordered by time, then by the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Due {
    time: u64,
    event: Event,
}

/// The timed flow, as an endless iterator of items (module docs). It exists only after
/// setup, which [`PolymarketFlow::start`] generates first: the timed flow continues the
/// setup's streams, ladders, nonces and order sequences.
#[derive(Clone, Debug)]
pub struct PolymarketFlow {
    config: PolymarketConfig,
    /// In id order.
    markets: Vec<MarketState>,
    clients: Clients,
    mix: MakerMix,
    /// Running sums of the markets' maker and taker weights (ppm), for picking a market.
    maker_weights: Vec<u32>,
    taker_weights: Vec<u32>,
    clock_stream: SplitMix64,
    taker_stream: SplitMix64,
    high_leverage_stream: SplitMix64,
    shock_stream: SplitMix64,
    /// See [`PolymarketConfig::taker_start_ppm`] and [`PolymarketConfig::maker_rate`].
    taker_start_ppm: u64,
    maker_rate: u64,
    /// Maker messages so far, from any event: the message clock's count (module docs,
    /// "Makers").
    clock_messages: u64,
    /// Maker messages of the timed flow so far: cancels and places, and modifies. They
    /// choose between re-price and resize ([`MakerMix::reprice_next`]).
    maker_moves: u64,
    maker_resizes: u64,
    /// The last shock's movers, by index, in draw order: the next `Rebuild`'s markets.
    movers: Vec<u32>,
    /// Every pending event; a min-heap.
    due: BinaryHeap<Reverse<Due>>,
    /// Flow time of the event whose items are being returned.
    flow_ns: u64,
    /// That event's items, and how many of them have been returned.
    pending: Vec<Item>,
    returned: usize,
    /// Items returned so far.
    emitted: usize,
    jumps: Vec<Jump>,
}

/// The plan for `config`: the setup phases, then the timed flow up to and including its
/// `timed_client_items`-th client item. Panics if the config fails [`PolymarketConfig::check`].
pub fn generate(config: &PolymarketConfig, timed_client_items: usize) -> FlowPlan<PolymarketConfig> {
    let (setup, mut flow) = PolymarketFlow::start(*config);
    // Operator items are under 1% of the timed flow: 440 marks a second in 100,000 messages.
    let mut timed = Vec::with_capacity(timed_client_items + timed_client_items / 64);
    let mut clients = 0;
    while clients < timed_client_items {
        let item = flow.next().expect("the timed flow never ends");
        clients += usize::from(item.is_client());
        timed.push(item);
    }
    FlowPlan {
        config: *config,
        setup_a: setup.a,
        setup_b1: setup.b1,
        setup_b2: setup.b2,
        timed,
        jumps: flow.jumps,
        flow_ns: flow.flow_ns,
    }
}

impl PolymarketFlow {
    /// Generates the setup phases, and returns them with the timed flow that follows them.
    /// Panics if the config fails [`PolymarketConfig::check`].
    pub fn start(config: PolymarketConfig) -> (SetupPhases, PolymarketFlow) {
        if let Err(problem) = config.check() {
            panic!("{problem}");
        }
        let highest_account = config.client_accounts().last().copied().unwrap_or(AccountId::new(0));
        let running_sums = |weight: fn(&Market) -> u32| -> Vec<u32> {
            let mut sum = 0;
            config
                .markets()
                .iter()
                .map(|market| {
                    sum += weight(market);
                    sum
                })
                .collect()
        };
        let mut flow = PolymarketFlow {
            config,
            markets: config.markets().iter().map(|spec| MarketState::new(config.seed, spec)).collect(),
            clients: Clients::new(highest_account),
            mix: MakerMix::of(&POLYMARKET),
            maker_weights: running_sums(|market| market.maker_weight_ppm),
            taker_weights: running_sums(|market| market.taker_weight_ppm),
            clock_stream: stream(config.seed, stream_ids::CLOCK),
            taker_stream: stream(config.seed, stream_ids::TAKER),
            high_leverage_stream: stream(config.seed, stream_ids::HL),
            shock_stream: stream(config.seed, stream_ids::SHOCK),
            taker_start_ppm: config.taker_start_ppm(),
            maker_rate: config.maker_rate(),
            clock_messages: 0,
            maker_moves: 0,
            maker_resizes: 0,
            movers: Vec::new(),
            due: BinaryHeap::new(),
            flow_ns: 0,
            pending: Vec::new(),
            returned: 0,
            emitted: 0,
            jumps: Vec::new(),
        };
        let a = setup_a(&config);
        let b1 = flow.setup_b1();
        let b2 = flow.setup_b2();
        flow.schedule_first_events();
        (SetupPhases { a, b1, b2 }, flow)
    }

    /// Flow time of the event that produced the last item returned.
    pub fn flow_ns(&self) -> u64 {
        self.flow_ns
    }

    /// Every jump and shock move so far, in order.
    pub fn jumps(&self) -> &[Jump] {
        &self.jumps
    }

    /// Setup B1: every market's ladders, market by market (module docs, "Makers"). Of market
    /// `i`'s makers `0..n`, the bid of rank `r` belongs to maker `(r + i) mod n` and the ask
    /// of rank `r` to maker `(r + 1 + i) mod n`, so that each maker holds about as many
    /// quotes, the two best quotes, the busiest, have different makers, and which makers hold
    /// them turns from market to market.
    fn setup_b1(&mut self) -> Vec<Item> {
        let mut out = Vec::new();
        let levels = POLYMARKET.levels_per_side;
        let market_makers = self.config.market_makers();
        for (index, market) in self.markets.iter_mut().enumerate() {
            let (first, count) = market_makers[index];
            let turn = index as u32;
            let bid_makers: Vec<AccountId> =
                (0..levels).map(|rank| AccountId::new(first + (rank + turn) % count)).collect();
            let ask_makers: Vec<AccountId> =
                (0..levels).map(|rank| AccountId::new(first + (rank + 1 + turn) % count)).collect();
            market.build(&bid_makers, &ask_makers, &mut self.clients, &mut out);
        }
        out
    }

    /// Setup B2: each high-leverage account, in id order, sends its setup IOCs; then each
    /// cascade account, in id order, one. All at the start marks.
    fn setup_b2(&mut self) -> Vec<Item> {
        let config = self.config;
        let mut out = Vec::new();
        for index in 0..config.high_leverage_accounts() {
            for _ in 0..config.high_leverage_setup_iocs {
                out.push(self.high_leverage_ioc(index));
            }
        }
        for index in 0..config.cascade_accounts() {
            out.push(self.cascade_ioc(index));
        }
        out
    }

    /// Account `HIGH_LEVERAGE_BASE + index` adds to its position: an IOC on its side.
    fn high_leverage_ioc(&mut self, index: u32) -> Item {
        let (account, market, side) = self.config.high_leverage(index);
        let order = self.markets[market].ioc(side, self.config.high_leverage_notional);
        self.clients.place(account, order).1
    }

    /// Account `CASCADE_BASE + index` enters: an IOC on its side.
    fn cascade_ioc(&mut self, index: u32) -> Item {
        let (account, market, side) = self.config.cascade(index);
        let notional = self.config.shock.map_or(Micros::ZERO, |shock| shock.cascade_notional);
        let order = self.markets[market].ioc(side, notional);
        self.clients.place(account, order).1
    }

    fn push(&mut self, time: u64, event: Event) {
        self.due.push(Reverse(Due { time, event }));
    }

    /// Flow time of the clock's `n`-th maker message: `n × 10^9 / maker_rate`, from the
    /// start, so rounding never accumulates.
    fn clock_time(&self, n: u64) -> u64 {
        (u128::from(n) * u128::from(SECOND_NS) / u128::from(self.maker_rate)) as u64
    }

    /// Every recurring event at its first time: market `i`'s first mark at `200 ms + i ×
    /// 200 ms / 88`; the first maker message, taker tick, high-leverage add (at half its
    /// period, [`PolymarketConfig::high_leverage_every_ns`]) and shock.
    fn schedule_first_events(&mut self) {
        let mark_every = u64::from(POLYMARKET.mark_every_ms) * 1_000_000;
        let markets = self.markets.len() as u64;
        for index in 0..self.markets.len() as u32 {
            let phase = u64::from(index) * mark_every / markets;
            self.push(mark_every + phase, Event::MarkTick(index));
        }
        self.push(self.clock_time(1), Event::Message);
        self.push(TAKER_TICK_NS, Event::TakerTick);
        if self.config.high_leverage_per_market > 0 {
            self.push(self.config.high_leverage_every_ns() / 2, Event::HighLeverage);
        }
        if let Some(shock) = self.config.shock {
            self.push(shock.every_ns, Event::Shock);
        }
    }

    /// Runs the next event: its items go to `pending`.
    fn run_next_event(&mut self) {
        let Reverse(Due { time, event }) = self.due.pop().expect("recurring events never run out");
        self.flow_ns = time;
        match event {
            Event::MarkTick(index) => {
                self.push(time + u64::from(POLYMARKET.mark_every_ms) * 1_000_000, event);
                let market = &mut self.markets[index as usize];
                if let Some(from) =
                    market.mark_tick(self.config.jump_one_in, &mut self.clients, &mut self.pending)
                {
                    self.record_jump(index, from);
                }
            }
            Event::ShockMove(shock_move) => self.shock_move(shock_move),
            Event::Shock => {
                let every =
                    self.config.shock.expect("shocks are scheduled only with a shock config").every_ns;
                self.push(time + every, event);
                self.shock();
            }
            Event::Rebuild => self.rebuild(),
            Event::HighLeverage => {
                self.push(time + self.config.high_leverage_every_ns(), event);
                // A market by taker weight, then one of its accounts.
                let per_market = self.config.high_leverage_per_market;
                let market = pick_by_running_sums(&self.taker_weights, &mut self.high_leverage_stream) as u32;
                let member = self.high_leverage_stream.below(u64::from(per_market)) as u32;
                let item = self.high_leverage_ioc(market * per_market + member);
                self.pending.push(item);
            }
            Event::TakerFollow(follow) => self.taker_follow(follow),
            Event::TakerTick => {
                self.push(time + TAKER_TICK_NS, event);
                if self.taker_stream.below(MILLION) < self.taker_start_ppm {
                    self.taker_start();
                }
            }
            Event::Message => {
                let index = pick_by_running_sums(&self.maker_weights, &mut self.clock_stream);
                let reprice = self.mix.reprice_next(self.maker_moves, self.maker_resizes);
                let market = &mut self.markets[index];
                market.maker_event(reprice, &self.mix, &mut self.clients, &mut self.pending);
            }
        }
        // Every maker message of the event, whichever event it is, counts towards the running
        // split (cancels and post-only places are moves, modifies resizes; only makers send
        // either) and on the message clock: ladders following a move or a shock take their
        // messages from the clock, so a flow-second holds `messages_per_flow_second`.
        for item in &self.pending {
            match item.command() {
                Command::CancelOrder(_) | Command::PlaceOrder(PlaceOrder { post_only: true, .. }) => {
                    self.maker_moves += 1;
                    self.clock_messages += 1;
                }
                Command::ModifyOrder(_) => {
                    self.maker_resizes += 1;
                    self.clock_messages += 1;
                }
                _ => {}
            }
        }
        if event == Event::Message {
            self.push(self.clock_time(self.clock_messages + 1), event);
        }
    }

    /// Records a jump or shock move of the market at `index`, from `from`: its `SetMark` is
    /// the event's first item, so the next one returned.
    fn record_jump(&mut self, index: u32, from: Price) {
        let market = &self.markets[index as usize];
        let jump =
            Jump { item: self.emitted, flow_ns: self.flow_ns, market: market.id(), from, to: market.fair };
        self.jumps.push(jump);
    }

    /// A taker order from `account` in the market at `index`: its notional (from `TAKER`),
    /// capped at the market's maximum, as an IOC at the band's edge.
    fn taker_order(&mut self, account: AccountId, index: usize, buy: bool) {
        let market = &self.markets[index];
        let cents = draw_taker_cents(&mut self.taker_stream);
        // A cent is 10,000 micros.
        let notional = Micros::new(cents as i64 * 10_000).min(Micros::new(market.spec.max_notional));
        let side = if buy { Side::Buy } else { Side::Sell };
        let order = market.ioc(side, notional);
        let (_, item) = self.clients.place(account, order);
        self.pending.push(item);
    }

    /// A taker cluster starts (module docs, "Takers"). `TAKER` draws, in order, after the taker
    /// tick's own draw of whether a cluster starts: the cluster's size, its account, its market,
    /// its side, the first order's notional; then, if more orders follow, the gap to the next.
    fn taker_start(&mut self) {
        let takers = &POLYMARKET.takers;
        let rng = &mut self.taker_stream;
        let size = pick(takers.cluster_size_ppm, rng) as u32 + 1;
        let account = AccountId::new(TAKER_BASE + rng.below(u64::from(self.config.takers)) as u32);
        let market = pick_by_running_sums(&self.taker_weights, rng);
        let buy = rng.below(MILLION) < u64::from(takers.buy_ppm);
        self.taker_order(account, market, buy);
        if size > 1 {
            self.schedule_follow(Follow {
                account,
                market: market as u32,
                first_buy: buy,
                remaining: size - 1,
            });
        }
    }

    /// The next order of a cluster. `TAKER` draws, in order: the same market or a new one
    /// (by taker weight), the first order's side or the other, the notional; then, if more
    /// follow, the gap to the next.
    fn taker_follow(&mut self, follow: Follow) {
        let takers = &POLYMARKET.takers;
        let rng = &mut self.taker_stream;
        let market = if rng.below(MILLION) < u64::from(takers.cluster_same_market_ppm) {
            follow.market as usize
        } else {
            pick_by_running_sums(&self.taker_weights, rng)
        };
        let same_side = rng.below(MILLION) < u64::from(takers.cluster_same_side_ppm);
        let buy = if same_side { follow.first_buy } else { !follow.first_buy };
        self.taker_order(follow.account, market, buy);
        if follow.remaining > 1 {
            self.schedule_follow(Follow { market: market as u32, remaining: follow.remaining - 1, ..follow });
        }
    }

    /// Schedules `follow` a drawn gap (whole milliseconds) after now.
    fn schedule_follow(&mut self, follow: Follow) {
        let gap_ms = POLYMARKET.takers.cluster_gap_ms.draw(&mut self.taker_stream);
        self.push(self.flow_ns + u64::from(gap_ms) * 1_000_000, Event::TakerFollow(follow));
    }

    /// A shock (module docs, "Stress switches"). `SHOCK` draws, in order: how many markets,
    /// which (the first draws of a shuffle of all of them), the shared direction; then per
    /// mover, in draw order, its time within the spread, whether it keeps the direction, and
    /// its size.
    fn shock(&mut self) {
        let shock = self.config.shock.expect("shocks are scheduled only with a shock config");
        let recorded = &POLYMARKET.shocks;
        let rng = &mut self.shock_stream;
        let count = recorded.min_markets as usize + pick(recorded.markets_ppm, rng);
        let mut order: Vec<u32> = (0..self.markets.len() as u32).collect();
        for i in 0..count {
            let j = i + rng.below((order.len() - i) as u64) as usize;
            order.swap(i, j);
        }
        let up = rng.below(2) == 0;
        let mut moves = Vec::with_capacity(count);
        for &market in &order[..count] {
            let offset = rng.below(shock.spread_ns + 1);
            let (keeps, size) = match shock.size {
                ShockSize::Calibrated => (rng.below(MILLION) < 950_000, draw_shock_sigmas_milli(rng)),
                ShockSize::Stress => {
                    let [low, high] = recorded.stress_move_ppm;
                    (true, rng.in_range(i64::from(low), i64::from(high)) as u32)
                }
            };
            let up = if keeps { up } else { !up };
            moves.push((self.flow_ns + offset, ShockMove { market, up, size }));
        }
        for (time, shock_move) in moves {
            self.push(time, Event::ShockMove(shock_move));
        }
        self.movers = order[..count].to_vec();
        if shock.cascade_per_market > 0 {
            self.push(self.flow_ns + shock.every_ns / 2, Event::Rebuild);
        }
    }

    /// One mover's move: its size in engine ticks at its fair value now; its mark; its
    /// ladders quoted again.
    fn shock_move(&mut self, shock_move: ShockMove) {
        let shock = self.config.shock.expect("shock moves are scheduled only with a shock config");
        let market = &mut self.markets[shock_move.market as usize];
        let (fair, size) = (i128::from(market.fair), i128::from(shock_move.size));
        let ticks = match shock.size {
            // `sigmas × rms` bps of the price: thousandths × thousandths of a bps (10^-10 of
            // the price), so `F × sigmas_milli × rms_millibps / 10^10` engine ticks.
            ShockSize::Calibrated => fair * i128::from(market.spec.move_rms_millibps) * size / 10i128.pow(10),
            ShockSize::Stress => fair * size / i128::from(MILLION),
        };
        if let Some((from, _)) = market.shock(ticks, shock_move.up, &mut self.clients, &mut self.pending) {
            self.record_jump(shock_move.market, from);
        }
    }

    /// Half-way between shocks: every cascade account of the last shock's movers, in mover
    /// order and then in id order, enters again with one IOC on its side, at the same mark.
    fn rebuild(&mut self) {
        let per_market = self.config.shock.map_or(0, |shock| shock.cascade_per_market);
        for mover in std::mem::take(&mut self.movers) {
            for member in 0..per_market {
                let item = self.cascade_ioc(mover * per_market + member);
                self.pending.push(item);
            }
        }
    }
}

impl Iterator for PolymarketFlow {
    type Item = Item;

    /// The next timed item. Never `None`: the flow is endless.
    fn next(&mut self) -> Option<Item> {
        // Some events emit nothing (a taker tick that starts no cluster, a shock, a shock move
        // stopped at the fair value's bound), so run events until one has items.
        while self.returned == self.pending.len() {
            self.pending.clear();
            self.returned = 0;
            self.run_next_event();
        }
        let item = self.pending[self.returned];
        self.returned += 1;
        self.emitted += 1;
        Some(item)
    }
}

/// The index a weighted draw lands on: `running_sums` holds the running sums of shares
/// summing to 1,000,000, so a draw `u` in `0..1,000,000` lands on the first whose sum is above
/// it. O(log n).
fn pick_by_running_sums(running_sums: &[u32], rng: &mut SplitMix64) -> usize {
    let u = rng.below(MILLION) as u32;
    running_sums.partition_point(|&sum| sum <= u)
}

/// A taker order's notional, in cents (module docs, "Takers"): dust with its share (drawn in
/// `[$10.50, $11.60)`), a point mass with its share, else a draw of the continuous table.
/// One draw `u` walks the shares; the dust and the table draw once more.
fn draw_taker_cents(rng: &mut SplitMix64) -> u64 {
    let notional = &POLYMARKET.taker_notional;
    let mut u = rng.below(MILLION) as u32;
    if u < notional.dust_ppm {
        let [low, high] = POLYMARKET.dust_draw_cents;
        return rng.in_range(i64::from(low), i64::from(high) - 1) as u64;
    }
    u -= notional.dust_ppm;
    for mass in notional.point_masses {
        if u < mass.share_ppm {
            return mass.cents;
        }
        u -= mass.share_ppm;
    }
    u64::from(notional.continuous_cents.draw(rng))
}

/// A calibrated shock mover's size, in thousandths of its `rms` (module docs, "Stress
/// switches"): uniform in each stretch between the recorded quantiles, `[4, p50)` half the
/// time, `[p50, p90)` 40%, and `[p90, 2 × p90 − p50)` 10%. 4 is the recording's threshold for
/// a mover; the last stretch, as wide as the one before, is this flow's assumption.
fn draw_shock_sigmas_milli(rng: &mut SplitMix64) -> u32 {
    let (p50, p90) = (
        i64::from(POLYMARKET.shocks.move_sigmas_p50_milli),
        i64::from(POLYMARKET.shocks.move_sigmas_p90_milli),
    );
    let u = rng.below(MILLION);
    let (low, high) = match u {
        0..500_000 => (4_000, p50),
        500_000..900_000 => (p50, p90),
        _ => (p90, 2 * p90 - p50),
    };
    rng.in_range(low, high - 1) as u32
}

/// Setup A (module docs, "Phases").
fn setup_a(config: &PolymarketConfig) -> Vec<Item> {
    let mut commands = Vec::new();
    for spec in config.markets() {
        let market = spec.market_id();
        commands.push(Command::SetMarketParams(market_params(spec)));
        let count = spec.tiers.len() as u8;
        for (index, tier) in spec.tiers.iter().enumerate() {
            let row = SetRiskTier {
                lower_bound: Micros::new(tier.lower_bound),
                market,
                max_leverage: tier.max_leverage,
                index: index as u8,
                count,
            };
            commands.push(Command::SetRiskTier(row));
        }
        let start_price = Price::new(spec.start_price);
        commands.push(Command::SetMark(SetMark { price: start_price, market }));
    }
    commands.push(Command::Deposit(Deposit { amount: config.fund_deposit, account: FUND }));
    let accounts = config.client_accounts();
    for &account in &accounts {
        let cohort = config.cohort_of(account).expect("a client account has a cohort");
        commands.push(Command::Deposit(Deposit { amount: config.deposit_of(cohort), account }));
    }
    // Each maker in every market it quotes, in account order, then market order.
    let market_makers = config.market_makers();
    for maker in 0..config.maker_accounts() {
        let number = MAKER_BASE + maker;
        let account = AccountId::new(number);
        for (index, spec) in config.markets().iter().enumerate() {
            let (first, count) = market_makers[index];
            if (first..first + count).contains(&number) {
                let leverage = config.maker_leverage.min(spec.max_leverage);
                let market = spec.market_id();
                commands.push(Command::SetLeverage(SetLeverage { account, market, leverage }));
            }
        }
    }
    for &account in &accounts {
        if let Some(Cohort::HighLeverage { market, .. } | Cohort::Cascade { market, .. }) =
            config.cohort_of(account)
        {
            let leverage =
                POLYMARKET.market(market).expect("a cohort's market is the profile's").max_leverage;
            commands.push(Command::SetLeverage(SetLeverage { account, market, leverage }));
        }
    }
    commands.into_iter().map(Item::Operator).collect()
}

#[cfg(test)]
mod tests;
