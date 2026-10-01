//! The Milestone 3 synthetic flow: 67 markets, each with its own seeded fair-value path;
//! market makers quoting around it; takers; high-leverage accounts that jumps liquidate; a
//! thin layer of long-lived orders; and the operator's marks, deposits and withdrawals
//! (`docs/PIPELINE.md` 14.1 to 14.7; `docs/DECISIONS.md` D-027).
//!
//! **Contract.** [`generate`] turns a [`MarketFlowConfig`] into one ordered list of items,
//! a [`FlowPlan`]: client commands (account, nonce, command) and operator commands. The
//! same config gives the same list on every machine and at every offered rate. The rate
//! changes only *when* each item is sent (`schedule.rs`), never *what* is sent, so runs at
//! different rates apply the same commands, faster or slower (14.1). A longer plan starts
//! with every item of a shorter one, so one plan, and one signed arena (`presign.rs`),
//! serves every run of a session as a prefix.
//!
//! **Phases** (14.4). Setup goes through the journal like everything else but isn't
//! measured; the harness puts a barrier between phases, so that, for example, no order is
//! sequenced before its account's deposit:
//! - **A** (operator commands): each market's parameters, its one-row tier table and its
//!   first mark; the insurance fund's capital; every client's deposit, in id order; the
//!   leverage of every market maker and every high-leverage account.
//! - **B1** (client commands): every market maker's first quotes, then the thin layer.
//! - **B2** (client commands): the high-leverage accounts' first positions, 3 IOCs each, at
//!   the frozen starting mark (a stated deviation from INFO.md 7, D-027).
//! - **Timed**: the event simulation below, for as many client items as the run needs.
//!
//! **The timed flow** (14.5) is an event simulation in the generator's own *flow time*
//! (integer nanoseconds). Flow time only orders the items; it is not the send schedule.
//! Recurring events, processed in order of time, then tie rank, then market (or thin
//! order) index:
//! - `FairStep(m)`, every 15 ms per market: the fair value takes a step of up to ±3 ticks,
//!   and each market maker requotes the quotes the move has made stale (a total-size
//!   modify, or a cancel and a new post-only order). With probability 1/15,000 the step is
//!   a 2% to 6% **jump** instead: the operator's mark follows at once, and every quote of
//!   the market is pulled and placed again.
//! - `MarkTick(m)`, every 100 ms per market: the operator's `SetMark`, the fair value ±2.
//! - `Taker`, every 250 µs: an IOC through the fair value, in a random market.
//! - `HighLeverage`, every 25 ms: a high-leverage account adds to its position.
//! - `Withdrawal`, every second: the operator withdraws $1,000 for a random taker.
//! - `ThinReplace(o)`, every 30 s per thin order: cancel it and place it again.
//!
//! **Open loop.** The generator never sees the engine: it doesn't know about fills,
//! liquidations or band sweeps, only what it sent. So some cancels name orders that are
//! already gone, and the engine rejects them, as it would real flow (14.7).
//!
//! **Randomness** (14.6). Each concern draws from its own [`SplitMix64`] stream
//! ([`stream`]): one for each market's fair value, market makers and marks, and one each for
//! takers, high-leverage orders, the thin layer, operator withdrawals and the send
//! schedule. So adding a market or changing one cohort doesn't shift the others. Within a
//! stream, draws happen in exactly the order 14.5 lists them. All arithmetic is integer.
//!
//! **Invariants** (checked by the tests): per account, nonces are 1, 2, 3, … over every
//! client item, and order sequences 1, 2, 3, … over its places; every cancel and modify
//! names an order the same account placed earlier; every price is inside its market's
//! range, and every order is inside the band of the generator's latest mark.
//!
//! **Assumed parameters.** Every number here is an assumption, chosen to be plausible, not
//! calibrated (INFO.md 7 and 12.4; D-027). Each is a documented field of
//! [`MarketFlowConfig`] or of [`MARKET_CLASSES`]; [`MarketFlowConfig::m3`] holds the values
//! of section 14 and [`MarketFlowConfig::smoke`] the scaled-down smoke flow of 18.4.
//!
//! **Complexity.** Each timed event costs O(log E) for the event queue (`E` = recurring
//! events, about 1,140 in the M3 flow) plus O(1) per item it emits: about 100,000 client
//! items per second of flow time, from about 9,200 events.
//!
//! **A second flow.** [`polymarket`] generates the Polymarket-shaped flow of D-034 into the
//! same kind of [`FlowPlan`], from the calibrated [`profile`]. It reuses the items, the plan
//! and the account counters here; the M3 flow above doesn't depend on it.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use engine::command::{
    CancelOrder, Command, Deposit, ModifyOrder, PlaceOrder, SetLeverage, SetMark, SetMarketParams,
    SetRiskTier, Withdraw,
};
use engine::engine::{EngineOptions, FUND};
use engine::types::{AccountId, MarketId, Micros, OrderId, Price, Qty, Side, TimeInForce, order_id};
use k256::sha2::{Digest, Sha256};

use crate::SplitMix64;

// ---------------------------------------------------------------------------------------
// Random streams (14.6).

/// The id of each random stream (14.6). Per-market streams add the market id.
pub mod stream_ids {
    /// `FAIR(m)`: jumps and ordinary steps of market `m`'s fair value.
    pub const FAIR: u64 = 0x1_0000;
    /// `MM(m)`: market `m`'s market makers: refreshes, requote choices, quote sizes.
    pub const MM: u64 = 0x2_0000;
    /// `MARK(m)`: the noise on market `m`'s marks.
    pub const MARK: u64 = 0x3_0000;
    /// `TAKER`: every taker IOC.
    pub const TAKER: u64 = 0x4_0000;
    /// `HL`: every high-leverage IOC.
    pub const HL: u64 = 0x5_0000;
    /// `THIN`: the thin layer's orders.
    pub const THIN: u64 = 0x6_0000;
    /// `OPS`: operator withdrawals.
    pub const OPS: u64 = 0x7_0000;
    /// `SCHEDULE`: the send schedule's Poisson gaps (`schedule.rs`).
    pub const SCHEDULE: u64 = 0x8_0000;
    /// The engine's `id_hash_seed`, the first draw of this stream.
    pub const HASH_SEED: u64 = 0x9_0000;
}

/// Stream `id` of `seed`:
/// `SplitMix64::new(SplitMix64::new(seed ^ id × 0x9E37_79B9_7F4A_7C15).next_u64())`.
/// Mixing the id through one SplitMix64 step first makes streams with neighbouring ids (two
/// markets, say) start far apart in the generator's sequence.
pub fn stream(seed: u64, id: u64) -> SplitMix64 {
    SplitMix64::new(SplitMix64::new(seed ^ id.wrapping_mul(0x9E37_79B9_7F4A_7C15)).next_u64())
}

// ---------------------------------------------------------------------------------------
// Markets (14.2).

/// A market's risk parameters, by class (`market mod 3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarketClass {
    pub max_leverage: u16,
    pub band_ppm: u32,
    pub taker_fee_ppm: i32,
    pub maker_fee_ppm: i32,
}

/// The three market classes (14.2). Each satisfies band rule 1 with room to spare:
/// `20 × Lmax × (band + max fee)` is 8,400,000, 8,160,000 and 8,100,000, against a limit of
/// 9,000,000 (RISK.md 5.3). Rule 2 needs `min_price` of about `20 × Lmax` ticks, at most
/// about 1,000; the smallest `min_price` here is 50,500.
pub const MARKET_CLASSES: [MarketClass; 3] = [
    MarketClass { max_leverage: 50, band_ppm: 8_000, taker_fee_ppm: 400, maker_fee_ppm: 100 },
    MarketClass { max_leverage: 20, band_ppm: 20_000, taker_fee_ppm: 400, maker_fee_ppm: 125 },
    MarketClass { max_leverage: 10, band_ppm: 40_000, taker_fee_ppm: 500, maker_fee_ppm: 100 },
];

/// Market `market`'s class: `market mod 3`.
pub fn class_of(market: MarketId) -> MarketClass {
    MARKET_CLASSES[usize::from(market) % 3]
}

/// Market `market`'s starting fair value `F0`: `100,000 + 1,000 × market` ticks, so every
/// market has its own price level.
pub fn start_fair_value(market: MarketId) -> Price {
    100_000 + 1_000 * Price::from(market)
}

/// Market `market`'s parameters: prices from `F0 / 2` to `2 × F0`, and its class's leverage,
/// band and fees. The book's memory is 8 bytes per tick of that range: 1.2 to 2.0 MB per
/// market, about 110 MB for 67.
pub fn market_params(market: MarketId) -> SetMarketParams {
    let (fair, class) = (start_fair_value(market), class_of(market));
    SetMarketParams {
        min_price: fair / 2,
        max_price: 2 * fair,
        maker_fee_ppm: class.maker_fee_ppm,
        taker_fee_ppm: class.taker_fee_ppm,
        price_band_ppm: class.band_ppm,
        market,
        max_leverage: class.max_leverage,
    }
}

// ---------------------------------------------------------------------------------------
// Accounts (14.3).

/// Market maker `j` of market `m` is account `(m − 1) × makers_per_market + j + 1`.
pub const MAKER_BASE: AccountId = 1;
/// Takers are accounts 1,001 onwards.
pub const TAKER_BASE: AccountId = 1_001;
/// High-leverage accounts are 5,001 onwards.
pub const HIGH_LEVERAGE_BASE: AccountId = 5_001;
/// The thin layer's accounts are 7,001 onwards.
pub const THIN_BASE: AccountId = 7_001;

/// One dollar in micros (D-004).
pub const DOLLAR: Micros = 1_000_000;

/// Which cohort an account belongs to (14.3), with what the flow needs to know about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cohort {
    /// Market maker `index` (0-based) of `market`.
    Maker {
        market: MarketId,
        index: u32,
    },
    Taker,
    /// Trades only `market`, only on `side` (buy: long).
    HighLeverage {
        market: MarketId,
        side: Side,
    },
    Thin,
}

// ---------------------------------------------------------------------------------------
// The configuration.

/// Everything that shapes the flow. Every field is an **assumed parameter** (INFO.md 7):
/// plausible, not calibrated. [`MarketFlowConfig::m3`] gives section 14's values, quoted in
/// each field's docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarketFlowConfig {
    /// Seeds every random stream (14.6), the account keys (14.8) and the engine's hash seed.
    pub seed: u64,

    // ---- Markets and their fair values (14.2, 14.5).
    /// Markets `1..=markets`: 67.
    pub markets: MarketId,
    /// Flow time between two fair-value steps of one market: 15 ms. Market `m`'s steps are
    /// at `m × step_ns / markets + i × step_ns`, so the markets' steps are spread evenly.
    pub step_ns: u64,
    /// An ordinary step moves the fair value by `-max_step..=max_step` ticks: 3.
    pub max_step: Price,
    /// A step is a jump with probability `1 / jump_one_in`: 1 in 15,000, about 0.3 jumps a
    /// second over 67 markets.
    pub jump_one_in: u64,
    /// A jump moves the fair value by `jump_min_ppm..=jump_max_ppm` of itself, up or down:
    /// 20,000 to 60,000 (2% to 6%).
    pub jump_min_ppm: i64,
    pub jump_max_ppm: i64,
    /// The fair value stays at least this far inside its market's price range: 100 ticks.
    pub fair_margin: Price,

    // ---- Marks (14.5).
    /// Flow time between two marks of one market: 100 ms.
    pub mark_every_ns: u64,
    /// A mark is the fair value plus `-mark_noise..=mark_noise` ticks: 2.
    pub mark_noise: Price,

    // ---- Market makers (14.3, 14.5).
    /// Market makers per market: 4.
    pub makers_per_market: u32,
    /// Levels each quotes per side: 3. Market maker `j`'s level-`k` bid is at
    /// `F − (2 + j + 2k)` and its ask at `F + (2 + j + 2k)`: never at the fair value, and a
    /// step of at most 3 ticks can't make one maker's new quote cross another's (14.5).
    pub maker_levels: u32,
    /// A new quote's size, in lots: 200,000 to 1,000,000, 10 to 1,000 times a taker's, so a
    /// taker rarely fills a quote completely and later modifies rarely miss.
    pub maker_min_size: Qty,
    pub maker_max_size: Qty,
    /// On each step, each quote is cancelled and placed again anyway with probability
    /// `1 / refresh_one_in`: 64.
    pub refresh_one_in: u64,
    /// $1,000,000 each.
    pub maker_deposit: Micros,
    /// 5, in its own market.
    pub maker_leverage: u16,

    // ---- Takers (14.3, 14.5).
    /// 2,000 accounts.
    pub takers: u32,
    /// Flow time between two taker IOCs (over all takers): 250 µs.
    pub taker_every_ns: u64,
    /// A taker IOC's quantity: 1,000 to 20,000 lots.
    pub taker_min_qty: Qty,
    pub taker_max_qty: Qty,
    /// $1,000,000 each, at the default leverage of 1.
    pub taker_deposit: Micros,
    /// Taker and high-leverage IOCs are priced this far through the fair value: 10 ticks, so
    /// they take the best one or two quotes.
    pub cross: Price,

    // ---- High-leverage accounts (14.3, 14.5).
    /// Per market: 6, the first half long and the second half short.
    pub high_leverage_per_market: u32,
    /// Flow time between two high-leverage IOCs (over all such accounts): 25 ms, the first
    /// at half of that, 12.5 ms. Each account adds to its position about every 10 s.
    pub high_leverage_every_ns: u64,
    /// A high-leverage IOC's quantity: 15,000 to 30,000 lots.
    pub high_leverage_min_qty: Qty,
    pub high_leverage_max_qty: Qty,
    /// IOCs per account in setup phase B2: 3.
    pub high_leverage_setup_iocs: u32,
    /// $10,000 each, at the market's maximum leverage.
    pub high_leverage_deposit: Micros,

    // ---- The thin layer (14.3, 14.5).
    /// 200 accounts.
    pub thin_accounts: u32,
    /// Long-lived GTC orders per thin account: 5.
    pub thin_orders_each: u32,
    /// A thin order rests `thin_min_band_percent..=thin_max_band_percent` of the band away
    /// from the fair value, on its own side: 40 to 80, far from the top but inside the band.
    pub thin_min_band_percent: i64,
    pub thin_max_band_percent: i64,
    /// A thin order's quantity: 1,000 to 5,000 lots.
    pub thin_min_qty: Qty,
    pub thin_max_qty: Qty,
    /// Flow time between two replacements of one thin order: 30 s. Each order's first
    /// replacement is at a random offset within that, drawn in setup.
    pub thin_replace_every_ns: u64,
    /// $100,000 each.
    pub thin_deposit: Micros,

    // ---- The operator (14.3, 14.5).
    /// Flow time between two withdrawals: 1 s.
    pub withdrawal_every_ns: u64,
    /// Each withdrawal takes $1,000 from a random taker.
    pub withdrawal_amount: Micros,
    /// The insurance fund's capital: $1,000, small on purpose, so a couple of jumps on 50x
    /// markets take it below zero and the run exercises `InsuranceShortfall` (14.3).
    pub fund_deposit: Micros,
}

/// Bumped whenever the generator's algorithm changes, so a `presigned.bin` made by an older
/// generator is refused (its config digest no longer matches).
pub const FLOW_VERSION: u64 = 1;

/// Engine options for M3 runs (14.3): room for about 40 resting orders per market with a
/// wide margin, a slot per account that can trade a market, and every account. With these,
/// the engine allocates nothing after setup (D-010, RISK.md 3.5).
const ENGINE_CAPACITY: usize = 4_096;

impl MarketFlowConfig {
    /// Section 14's flow: 67 markets, 2,870 client accounts, about 100,000 client commands
    /// per second of flow time.
    pub fn m3() -> MarketFlowConfig {
        MarketFlowConfig {
            seed: 1,
            markets: 67,
            step_ns: 15_000_000,
            max_step: 3,
            jump_one_in: 15_000,
            jump_min_ppm: 20_000,
            jump_max_ppm: 60_000,
            fair_margin: 100,
            mark_every_ns: 100_000_000,
            mark_noise: 2,
            makers_per_market: 4,
            maker_levels: 3,
            maker_min_size: 200_000,
            maker_max_size: 1_000_000,
            refresh_one_in: 64,
            maker_deposit: 1_000_000 * DOLLAR,
            maker_leverage: 5,
            takers: 2_000,
            taker_every_ns: 250_000,
            taker_min_qty: 1_000,
            taker_max_qty: 20_000,
            taker_deposit: 1_000_000 * DOLLAR,
            cross: 10,
            high_leverage_per_market: 6,
            high_leverage_every_ns: 25_000_000,
            high_leverage_min_qty: 15_000,
            high_leverage_max_qty: 30_000,
            high_leverage_setup_iocs: 3,
            high_leverage_deposit: 10_000 * DOLLAR,
            thin_accounts: 200,
            thin_orders_each: 5,
            thin_min_band_percent: 40,
            thin_max_band_percent: 80,
            thin_min_qty: 1_000,
            thin_max_qty: 5_000,
            thin_replace_every_ns: 30_000_000_000,
            thin_deposit: 100_000 * DOLLAR,
            withdrawal_every_ns: 1_000_000_000,
            withdrawal_amount: 1_000 * DOLLAR,
            fund_deposit: 1_000 * DOLLAR,
        }
    }

    /// The smoke flow of 18.4, for end-to-end tests that must finish in seconds: 6 markets
    /// (two of each class), 4 market makers each, 40 takers, 2 high-leverage accounts per
    /// market (one long, one short), 10 thin accounts, and a fund of $1, so the first loss
    /// past a bankruptcy price is a shortfall.
    ///
    /// Jumps come 1 step in 20, not 1 in 500 as 18.4 suggested (PIPELINE.md 22). This flow
    /// sends about 13,000 client items per second of flow time, so the signed smoke run's
    /// 3,000 messages cover about 0.23 s of it: 91 steps at 6 markets (one every 2.5 ms).
    /// At 1 in 500 that run would see a jump, and so a liquidation, about one time in six.
    /// At 1 in 20 it sees one in 99% of seeds; with seed 1 the first jump is at client item
    /// 318 and liquidates at once, with a shortfall (a test checks it). The pre-verified
    /// smoke run's 30,000 items see about 45 jumps.
    pub fn smoke() -> MarketFlowConfig {
        MarketFlowConfig {
            markets: 6,
            jump_one_in: 20,
            takers: 40,
            high_leverage_per_market: 2,
            thin_accounts: 10,
            fund_deposit: DOLLAR,
            ..MarketFlowConfig::m3()
        }
    }

    /// The engine options for runs of this flow (14.3), with the hash seed drawn from the
    /// flow's seed (14.6).
    pub fn engine_options(&self) -> EngineOptions {
        EngineOptions {
            order_capacity: ENGINE_CAPACITY,
            id_hash_seed: stream(self.seed, stream_ids::HASH_SEED).next_u64(),
            scratch_capacity: ENGINE_CAPACITY,
            account_capacity: ENGINE_CAPACITY,
            slot_capacity: ENGINE_CAPACITY,
        }
    }

    /// Refuses a config the generator can't run: a cohort too large for its id range, an
    /// empty cohort that a stream would draw an index from, an empty range, or a zero period.
    pub fn check(&self) -> Result<(), String> {
        let c = self;
        let makers = u64::from(c.markets) * u64::from(c.makers_per_market);
        let high_leverage = u64::from(c.markets) * u64::from(c.high_leverage_per_market);
        let ranges = [
            (c.jump_min_ppm, c.jump_max_ppm),
            (c.maker_min_size, c.maker_max_size),
            (c.taker_min_qty, c.taker_max_qty),
            (c.high_leverage_min_qty, c.high_leverage_max_qty),
            (c.thin_min_band_percent, c.thin_max_band_percent),
            (c.thin_min_qty, c.thin_max_qty),
        ];
        let periods =
            [c.step_ns, c.mark_every_ns, c.taker_every_ns, c.high_leverage_every_ns, c.withdrawal_every_ns];
        let rules = [
            (c.markets >= 1, "at least one market"),
            (c.makers_per_market >= 1 && c.maker_levels >= 1, "at least one market maker and level"),
            (makers <= u64::from(TAKER_BASE - MAKER_BASE), "at most 1,000 market makers"),
            ((1..=HIGH_LEVERAGE_BASE - TAKER_BASE).contains(&c.takers), "1 to 4,000 takers"),
            (c.high_leverage_per_market >= 1, "at least one high-leverage account per market"),
            (
                high_leverage <= u64::from(THIN_BASE - HIGH_LEVERAGE_BASE),
                "at most 2,000 high-leverage accounts",
            ),
            (c.thin_accounts <= FUND - THIN_BASE, "thin accounts below the fund's id"),
            (c.jump_one_in >= 1 && c.refresh_one_in >= 1, "probabilities of 1 in at least 1"),
            (ranges.iter().all(|(low, high)| low <= high), "every range's low end at or below its high end"),
            (periods.iter().all(|&period| period > 0), "periods above zero"),
            // A thin order's first replacement is drawn in whole milliseconds of the period.
            (c.thin_replace_every_ns >= 1_000_000, "thin replacements at least 1 ms apart"),
        ];
        match rules.iter().find(|(holds, _)| !holds) {
            Some((_, rule)) => Err(format!("invalid flow config: needs {rule}: {c:?}")),
            None => Ok(()),
        }
    }

    /// SHA-256 of the generator's version and every field, in order: two configs with the
    /// same digest generate the same plan. `presigned.bin` records it (14.8).
    pub fn digest(&self) -> [u8; 32] {
        // Destructured, so that a new field that isn't hashed here is a compile error.
        let MarketFlowConfig {
            seed,
            markets,
            step_ns,
            max_step,
            jump_one_in,
            jump_min_ppm,
            jump_max_ppm,
            fair_margin,
            mark_every_ns,
            mark_noise,
            makers_per_market,
            maker_levels,
            maker_min_size,
            maker_max_size,
            refresh_one_in,
            maker_deposit,
            maker_leverage,
            takers,
            taker_every_ns,
            taker_min_qty,
            taker_max_qty,
            taker_deposit,
            cross,
            high_leverage_per_market,
            high_leverage_every_ns,
            high_leverage_min_qty,
            high_leverage_max_qty,
            high_leverage_setup_iocs,
            high_leverage_deposit,
            thin_accounts,
            thin_orders_each,
            thin_min_band_percent,
            thin_max_band_percent,
            thin_min_qty,
            thin_max_qty,
            thin_replace_every_ns,
            thin_deposit,
            withdrawal_every_ns,
            withdrawal_amount,
            fund_deposit,
        } = *self;
        let fields: [u64; 41] = [
            FLOW_VERSION,
            seed,
            u64::from(markets),
            step_ns,
            max_step as u64,
            jump_one_in,
            jump_min_ppm as u64,
            jump_max_ppm as u64,
            fair_margin as u64,
            mark_every_ns,
            mark_noise as u64,
            u64::from(makers_per_market),
            u64::from(maker_levels),
            maker_min_size as u64,
            maker_max_size as u64,
            refresh_one_in,
            maker_deposit as u64,
            u64::from(maker_leverage),
            u64::from(takers),
            taker_every_ns,
            taker_min_qty as u64,
            taker_max_qty as u64,
            taker_deposit as u64,
            cross as u64,
            u64::from(high_leverage_per_market),
            high_leverage_every_ns,
            high_leverage_min_qty as u64,
            high_leverage_max_qty as u64,
            u64::from(high_leverage_setup_iocs),
            high_leverage_deposit as u64,
            u64::from(thin_accounts),
            u64::from(thin_orders_each),
            thin_min_band_percent as u64,
            thin_max_band_percent as u64,
            thin_min_qty as u64,
            thin_max_qty as u64,
            thin_replace_every_ns,
            thin_deposit as u64,
            withdrawal_every_ns,
            withdrawal_amount as u64,
            fund_deposit as u64,
        ];
        let mut hasher = Sha256::new();
        hasher.update(b"perps-loadgen market flow");
        for field in fields {
            hasher.update(field.to_le_bytes());
        }
        hasher.finalize().into()
    }

    /// Market maker `index` of `market`.
    pub fn maker(&self, market: MarketId, index: u32) -> AccountId {
        MAKER_BASE + (u32::from(market) - 1) * self.makers_per_market + index
    }

    /// High-leverage account `index` (0-based, `0..markets × per market`): its account, and
    /// its market and side. Index `i` trades market `i / per market + 1`, long if
    /// `i mod per market` is in the first half.
    pub fn high_leverage(&self, index: u32) -> (AccountId, MarketId, Side) {
        let per_market = self.high_leverage_per_market;
        let market = MarketId::try_from(index / per_market + 1).expect("markets fit a MarketId");
        let side = if index % per_market < per_market / 2 { Side::Buy } else { Side::Sell };
        (HIGH_LEVERAGE_BASE + index, market, side)
    }

    /// The cohort of `account`, or `None` if the flow doesn't use it.
    pub fn cohort_of(&self, account: AccountId) -> Option<Cohort> {
        let makers = u32::from(self.markets) * self.makers_per_market;
        let high_leverage = u32::from(self.markets) * self.high_leverage_per_market;
        if (MAKER_BASE..MAKER_BASE + makers).contains(&account) {
            let i = account - MAKER_BASE;
            let market = MarketId::try_from(i / self.makers_per_market + 1).expect("markets fit a MarketId");
            Some(Cohort::Maker { market, index: i % self.makers_per_market })
        } else if (TAKER_BASE..TAKER_BASE + self.takers).contains(&account) {
            Some(Cohort::Taker)
        } else if (HIGH_LEVERAGE_BASE..HIGH_LEVERAGE_BASE + high_leverage).contains(&account) {
            let (_, market, side) = self.high_leverage(account - HIGH_LEVERAGE_BASE);
            Some(Cohort::HighLeverage { market, side })
        } else if (THIN_BASE..THIN_BASE + self.thin_accounts).contains(&account) {
            Some(Cohort::Thin)
        } else {
            None
        }
    }

    /// Every client account, in id order: the market makers, the takers, the high-leverage
    /// accounts and the thin layer. Each needs a key (14.8). Not the fund, which can't trade.
    pub fn client_accounts(&self) -> Vec<AccountId> {
        let makers = u32::from(self.markets) * self.makers_per_market;
        let high_leverage = u32::from(self.markets) * self.high_leverage_per_market;
        let mut accounts: Vec<AccountId> = (MAKER_BASE..MAKER_BASE + makers).collect();
        accounts.extend(TAKER_BASE..TAKER_BASE + self.takers);
        accounts.extend(HIGH_LEVERAGE_BASE..HIGH_LEVERAGE_BASE + high_leverage);
        accounts.extend(THIN_BASE..THIN_BASE + self.thin_accounts);
        accounts
    }

    /// The deposit of a client account, by its cohort.
    fn deposit_of(&self, cohort: Cohort) -> Micros {
        match cohort {
            Cohort::Maker { .. } => self.maker_deposit,
            Cohort::Taker => self.taker_deposit,
            Cohort::HighLeverage { .. } => self.high_leverage_deposit,
            Cohort::Thin => self.thin_deposit,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Items and the plan.

/// A client command as the client sends it: signed by `account` with `nonce` (5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientItem {
    pub account: AccountId,
    /// The account's next nonce: 1, 2, 3, … over all its client items (6.1).
    pub nonce: u64,
    /// A place, a cancel or a modify.
    pub command: Command,
}

/// One item of the plan: a client command, or an operator command (section 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Client(ClientItem),
    Operator(Command),
}

impl Item {
    pub fn is_client(&self) -> bool {
        matches!(self, Item::Client(_))
    }

    /// The command, whoever sends it.
    pub fn command(&self) -> &Command {
        match self {
            Item::Client(client) => &client.command,
            Item::Operator(command) => command,
        }
    }
}

/// The four phases of a run (14.4), in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowPhase {
    /// Setup A: markets, deposits, leverage (operator commands).
    SetupA,
    /// Setup B1: the first quotes and the thin layer.
    SetupB1,
    /// Setup B2: the high-leverage accounts' first positions.
    SetupB2,
    /// The timed flow: warm-up, then the measured window (15.5).
    Timed,
}

impl FlowPhase {
    pub const ALL: [FlowPhase; 4] =
        [FlowPhase::SetupA, FlowPhase::SetupB1, FlowPhase::SetupB2, FlowPhase::Timed];

    pub fn name(self) -> &'static str {
        match self {
            FlowPhase::SetupA => "setup A",
            FlowPhase::SetupB1 => "setup B1",
            FlowPhase::SetupB2 => "setup B2",
            FlowPhase::Timed => "timed",
        }
    }
}

/// A jump of one market's fair value, for the report: which jumps fell inside a run's
/// window (14.1, 15.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Jump {
    /// Index, in the timed items, of the jump's `SetMark`.
    pub item: usize,
    /// Flow time of the step.
    pub flow_ns: u64,
    pub market: MarketId,
    /// The fair value before and after.
    pub from: Price,
    pub to: Price,
}

/// Everything a run sends, in order (14.1): the three setup phases and the timed flow.
///
/// `C` is the config the plan was generated from: [`MarketFlowConfig`] for the M3 flow (the
/// default, so `FlowPlan` alone is the M3 flow's plan), or
/// [`PolymarketConfig`](polymarket::PolymarketConfig) for the Polymarket-shaped flow (D-034).
/// The signer and the sender take either, through [`PlanConfig`].
#[derive(Clone, Debug)]
pub struct FlowPlan<C = MarketFlowConfig> {
    pub config: C,
    pub setup_a: Vec<Item>,
    pub setup_b1: Vec<Item>,
    pub setup_b2: Vec<Item>,
    pub timed: Vec<Item>,
    /// Every jump among the timed items (in the Polymarket-shaped flow, every shock's move too).
    pub jumps: Vec<Jump>,
    /// Flow time of the last timed item's event.
    pub flow_ns: u64,
}

/// What the signer (`presign.rs`), the sender (`sender.rs`) and the harness need from the
/// config a plan was generated from, whichever flow made it.
pub trait PlanConfig {
    /// Seeds the account keys (14.8) and the send schedule (14.9).
    fn seed(&self) -> u64;
    /// Names the flow: two configs with the same digest generate the same plan, and an arena
    /// records the digest of the flow it was built from (14.8).
    fn digest(&self) -> [u8; 32];
    /// Every client account, in id order: each needs a key (14.8).
    fn client_accounts(&self) -> Vec<AccountId>;
    /// The engine options for runs of this flow.
    fn engine_options(&self) -> EngineOptions;
}

/// The M3 flow's config is a [`PlanConfig`] through its own methods, unchanged.
impl PlanConfig for MarketFlowConfig {
    fn seed(&self) -> u64 {
        self.seed
    }

    fn digest(&self) -> [u8; 32] {
        MarketFlowConfig::digest(self)
    }

    fn client_accounts(&self) -> Vec<AccountId> {
        MarketFlowConfig::client_accounts(self)
    }

    fn engine_options(&self) -> EngineOptions {
        MarketFlowConfig::engine_options(self)
    }
}

impl<C> FlowPlan<C> {
    /// The items of one phase.
    pub fn phase(&self, phase: FlowPhase) -> &[Item] {
        match phase {
            FlowPhase::SetupA => &self.setup_a,
            FlowPhase::SetupB1 => &self.setup_b1,
            FlowPhase::SetupB2 => &self.setup_b2,
            FlowPhase::Timed => &self.timed,
        }
    }

    /// Every client item, in plan order: B1, B2, then the timed flow. The signed and compact
    /// arenas (`presign.rs`) hold one message per client item, in this order.
    pub fn client_items(&self) -> impl Iterator<Item = &ClientItem> {
        FlowPhase::ALL.into_iter().flat_map(|phase| self.phase(phase)).filter_map(|item| match item {
            Item::Client(client) => Some(client),
            Item::Operator(_) => None,
        })
    }

    /// Client items in the setup phases: the arena's index of the first timed client item.
    pub fn setup_client_items(&self) -> usize {
        [&self.setup_a, &self.setup_b1, &self.setup_b2]
            .iter()
            .flat_map(|items| items.iter())
            .filter(|i| i.is_client())
            .count()
    }
}

/// The plan for `config`: the setup phases, then the timed flow up to and including its
/// `timed_client_items`-th client item. Panics if the config fails [`MarketFlowConfig::check`].
pub fn generate(config: &MarketFlowConfig, timed_client_items: usize) -> FlowPlan {
    let (setup, mut flow) = MarketFlow::start(*config);
    // Operator items are under 1% of the timed flow (14.7): 670 marks a second in 100,000.
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

// ---------------------------------------------------------------------------------------
// The generator's state.

/// Each account's counters, indexed by account id.
#[derive(Clone, Debug)]
struct Clients {
    /// The last order sequence used (places only).
    last_order: Vec<u32>,
    /// The last nonce used (every client item).
    last_nonce: Vec<u64>,
}

impl Clients {
    fn new(highest_account: AccountId) -> Clients {
        let len = highest_account as usize + 1;
        Clients { last_order: vec![0; len], last_nonce: vec![0; len] }
    }

    /// `command` as `account`'s next client item: it takes the account's next nonce.
    fn send(&mut self, account: AccountId, command: Command) -> Item {
        let nonce = &mut self.last_nonce[account as usize];
        *nonce += 1;
        Item::Client(ClientItem { account, nonce: *nonce, command })
    }

    /// The id of `account`'s next order: its next sequence number (RISK.md 3.1).
    fn next_order_id(&mut self, account: AccountId) -> OrderId {
        let sequence = &mut self.last_order[account as usize];
        *sequence += 1;
        order_id(account, *sequence)
    }

    /// A new limit order from `account`; returns its id and the item.
    fn place(&mut self, account: AccountId, order: NewOrder) -> (OrderId, Item) {
        let id = self.next_order_id(account);
        let place = PlaceOrder {
            order_id: id,
            price: order.price,
            qty: order.qty,
            market: order.market,
            side: order.side,
            tif: order.tif,
            post_only: order.post_only,
        };
        (id, self.send(account, Command::PlaceOrder(place)))
    }

    fn cancel(&mut self, account: AccountId, order_id: OrderId, market: MarketId) -> Item {
        self.send(account, Command::CancelOrder(CancelOrder { order_id, market }))
    }
}

/// What a new order says, before it has an id.
#[derive(Clone, Copy, Debug)]
struct NewOrder {
    market: MarketId,
    side: Side,
    price: Price,
    qty: Qty,
    tif: TimeInForce,
    post_only: bool,
}

/// One market maker quote, as the generator last sent it (14.5).
#[derive(Clone, Copy, Debug)]
struct Quote {
    maker: AccountId,
    /// `j`: the maker's index in its market.
    maker_index: u32,
    side: Side,
    /// `k`: 0 is the level nearest the fair value.
    level: u32,
    order_id: OrderId,
    price: Price,
    /// The total size last sent: a modify repeats it, since fills the generator can't see
    /// are the book's to subtract (D-008).
    size: Qty,
}

impl Quote {
    /// Ticks from the fair value: `2 + j + 2k`.
    fn distance(&self) -> Price {
        Price::from(2 + self.maker_index + 2 * self.level)
    }
}

/// One market as the generator sees it: its own fair value and quotes, never the engine's
/// book (open loop).
#[derive(Clone, Debug)]
struct MarketState {
    id: MarketId,
    params: SetMarketParams,
    fair: Price,
    /// Every quote, in the order 14.5 walks them: maker `j`, then bid before ask, then
    /// level `k`.
    quotes: Vec<Quote>,
    fair_stream: SplitMix64,
    maker_stream: SplitMix64,
    mark_stream: SplitMix64,
}

impl MarketState {
    fn new(config: &MarketFlowConfig, id: MarketId) -> MarketState {
        // Each market has its own three streams: the stream's id plus the market's (14.6).
        let m = u64::from(id);
        MarketState {
            id,
            params: market_params(id),
            fair: start_fair_value(id),
            quotes: Vec::new(),
            fair_stream: stream(config.seed, stream_ids::FAIR + m),
            maker_stream: stream(config.seed, stream_ids::MM + m),
            mark_stream: stream(config.seed, stream_ids::MARK + m),
        }
    }

    /// `price`, kept inside the market's range.
    fn clamp(&self, price: Price) -> Price {
        price.clamp(self.params.min_price, self.params.max_price)
    }

    /// A new fair value, kept `fair_margin` ticks inside the market's range.
    fn set_fair(&mut self, config: &MarketFlowConfig, fair: Price) {
        let margin = config.fair_margin;
        self.fair = fair.clamp(self.params.min_price + margin, self.params.max_price - margin);
    }

    /// Where a quote belongs at the current fair value: below it for a bid, above for an ask.
    fn target(&self, quote: &Quote) -> Price {
        match quote.side {
            Side::Buy => self.clamp(self.fair - quote.distance()),
            Side::Sell => self.clamp(self.fair + quote.distance()),
        }
    }

    /// An IOC from `side`, priced `cross` ticks through the fair value.
    fn ioc(&self, config: &MarketFlowConfig, side: Side, qty: Qty) -> NewOrder {
        let price = match side {
            Side::Buy => self.fair + config.cross,
            Side::Sell => self.fair - config.cross,
        };
        NewOrder {
            market: self.id,
            side,
            price: self.clamp(price),
            qty,
            tif: TimeInForce::Ioc,
            post_only: false,
        }
    }

    /// A thin-layer order `percent`% of the band away from the fair value, on its own side:
    /// `d = (F × band_ppm / 1,000,000) × percent / 100` (14.5).
    fn thin_order(&self, side: Side, percent: i64, qty: Qty) -> NewOrder {
        let distance = self.fair * Price::from(self.params.price_band_ppm) / 1_000_000 * percent / 100;
        let price = match side {
            Side::Buy => self.fair - distance,
            Side::Sell => self.fair + distance,
        };
        NewOrder {
            market: self.id,
            side,
            price: self.clamp(price),
            qty,
            tif: TimeInForce::Gtc,
            post_only: false,
        }
    }

    /// A new size for a quote, from the `MM(m)` stream.
    fn draw_quote_size(&mut self, config: &MarketFlowConfig) -> Qty {
        self.maker_stream.in_range(config.maker_min_size, config.maker_max_size)
    }

    /// Places quote `q` at its target with a new size, as a post-only GTC order.
    fn place_quote(
        &mut self,
        config: &MarketFlowConfig,
        q: usize,
        clients: &mut Clients,
        out: &mut Vec<Item>,
    ) {
        let size = self.draw_quote_size(config);
        let price = self.target(&self.quotes[q]);
        let quote = &mut self.quotes[q];
        let order = NewOrder {
            market: self.id,
            side: quote.side,
            price,
            qty: size,
            tif: TimeInForce::Gtc,
            post_only: true,
        };
        let (id, item) = clients.place(quote.maker, order);
        (quote.order_id, quote.price, quote.size) = (id, price, size);
        out.push(item);
    }

    /// Cancels quote `q`, then places it again at its target with a new size.
    fn replace_quote(
        &mut self,
        config: &MarketFlowConfig,
        q: usize,
        clients: &mut Clients,
        out: &mut Vec<Item>,
    ) {
        let quote = self.quotes[q];
        out.push(clients.cancel(quote.maker, quote.order_id, self.id));
        self.place_quote(config, q, clients, out);
    }

    /// Setup B1: every maker's first quotes (14.4). The quotes are created here, in the
    /// order 14.5 walks them.
    fn first_quotes(&mut self, config: &MarketFlowConfig, clients: &mut Clients, out: &mut Vec<Item>) {
        for maker_index in 0..config.makers_per_market {
            for side in [Side::Buy, Side::Sell] {
                for level in 0..config.maker_levels {
                    let maker = config.maker(self.id, maker_index);
                    // The order id, price and size are filled in when the quote is placed.
                    self.quotes.push(Quote {
                        maker,
                        maker_index,
                        side,
                        level,
                        order_id: 0,
                        price: 0,
                        size: 0,
                    });
                    self.place_quote(config, self.quotes.len() - 1, clients, out);
                }
            }
        }
    }

    /// `FairStep(m)` (14.5). Returns the fair value before a jump, if the step was one.
    fn fair_step(
        &mut self,
        config: &MarketFlowConfig,
        clients: &mut Clients,
        out: &mut Vec<Item>,
    ) -> Option<Price> {
        if self.fair_stream.below(config.jump_one_in) == 0 {
            let before = self.fair;
            self.jump(config, clients, out);
            return Some(before);
        }
        let step = self.fair_stream.in_range(-config.max_step, config.max_step);
        self.set_fair(config, self.fair + step);
        for q in 0..self.quotes.len() {
            self.requote_if_stale(config, q, clients, out);
        }
        None
    }

    /// A jump of 2% to 6% (14.5, step 1): the mark follows at once, with no noise; every
    /// quote is cancelled, then placed again at its new target. After a jump every old quote
    /// is stale, and modifying them one by one would cross the other makers' quotes.
    fn jump(&mut self, config: &MarketFlowConfig, clients: &mut Clients, out: &mut Vec<Item>) {
        let ppm = self.fair_stream.in_range(config.jump_min_ppm, config.jump_max_ppm);
        let size = self.fair * ppm / 1_000_000;
        let up = self.fair_stream.below(2) == 0;
        self.set_fair(config, if up { self.fair + size } else { self.fair - size });
        out.push(Item::Operator(Command::SetMark(SetMark { price: self.fair, market: self.id })));
        for quote in &self.quotes {
            out.push(clients.cancel(quote.maker, quote.order_id, self.id));
        }
        for q in 0..self.quotes.len() {
            self.place_quote(config, q, clients, out);
        }
    }

    /// Step 3 of `FairStep` (14.5) for quote `q`: a refresh with probability
    /// `1 / refresh_one_in`; otherwise, if the fair value has moved it more than `k` ticks
    /// from its target (level 0 on any move, deeper levels only once the move has taken
    /// them further away), a requote: half the time a total-size modify, half the time a
    /// cancel and a new order.
    fn requote_if_stale(
        &mut self,
        config: &MarketFlowConfig,
        q: usize,
        clients: &mut Clients,
        out: &mut Vec<Item>,
    ) {
        if self.maker_stream.below(config.refresh_one_in) == 0 {
            self.replace_quote(config, q, clients, out);
            return;
        }
        let target = self.target(&self.quotes[q]);
        let quote = self.quotes[q];
        if (quote.price - target).abs() <= Price::from(quote.level) {
            return;
        }
        if self.maker_stream.below(2) == 0 {
            let modify = ModifyOrder {
                order_id: quote.order_id,
                new_price: target,
                new_size: quote.size,
                market: self.id,
            };
            out.push(clients.send(quote.maker, Command::ModifyOrder(modify)));
            self.quotes[q].price = target;
        } else {
            self.replace_quote(config, q, clients, out);
        }
    }

    /// `MarkTick(m)`: the operator's mark, the fair value plus a little noise.
    fn mark_tick(&mut self, config: &MarketFlowConfig) -> Item {
        let noise = self.mark_stream.in_range(-config.mark_noise, config.mark_noise);
        Item::Operator(Command::SetMark(SetMark { price: self.clamp(self.fair + noise), market: self.id }))
    }
}

/// A thin-layer order, as the generator last sent it.
#[derive(Clone, Copy, Debug)]
struct ThinOrder {
    account: AccountId,
    /// Drawn once, in setup; a replacement stays in the same market.
    market: MarketId,
    order_id: OrderId,
}

/// The recurring events of 14.5, declared in tie-rank order: at equal flow times a
/// `FairStep` goes before a `MarkTick`, and so on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Recurring {
    FairStep,
    MarkTick,
    Taker,
    HighLeverage,
    Withdrawal,
    ThinReplace,
}

/// An event due at flow time `time`. The derived order compares `time`, then the event's
/// tie rank, then `index` (the market, or the thin order): exactly 14.5's processing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Due {
    time: u64,
    event: Recurring,
    index: u32,
}

// ---------------------------------------------------------------------------------------
// The generator.

/// The three setup phases' items (14.4).
#[derive(Clone, Debug)]
pub struct SetupPhases {
    pub a: Vec<Item>,
    pub b1: Vec<Item>,
    pub b2: Vec<Item>,
}

/// The timed flow, as an endless iterator of items (14.5). It exists only after setup,
/// which [`MarketFlow::start`] generates first: the timed flow continues the setup's
/// streams, quotes, thin orders, nonces and order sequences.
#[derive(Clone, Debug)]
pub struct MarketFlow {
    config: MarketFlowConfig,
    /// Index `m − 1` is market `m`.
    markets: Vec<MarketState>,
    clients: Clients,
    thin: Vec<ThinOrder>,
    taker_stream: SplitMix64,
    high_leverage_stream: SplitMix64,
    thin_stream: SplitMix64,
    ops_stream: SplitMix64,
    /// Every recurring event, each at its next time; a min-heap.
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

impl MarketFlow {
    /// Generates the setup phases, and returns them with the timed flow that follows them.
    /// Panics if the config fails [`MarketFlowConfig::check`].
    pub fn start(config: MarketFlowConfig) -> (SetupPhases, MarketFlow) {
        if let Err(problem) = config.check() {
            panic!("{problem}");
        }
        let highest_account = config.client_accounts().last().copied().unwrap_or(0);
        let mut flow = MarketFlow {
            config,
            markets: (1..=config.markets).map(|m| MarketState::new(&config, m)).collect(),
            clients: Clients::new(highest_account),
            thin: Vec::new(),
            taker_stream: stream(config.seed, stream_ids::TAKER),
            high_leverage_stream: stream(config.seed, stream_ids::HL),
            thin_stream: stream(config.seed, stream_ids::THIN),
            ops_stream: stream(config.seed, stream_ids::OPS),
            due: BinaryHeap::new(),
            flow_ns: 0,
            pending: Vec::new(),
            returned: 0,
            emitted: 0,
            jumps: Vec::new(),
        };
        let a = setup_a(&config);
        let mut b1 = Vec::new();
        let thin_offsets = flow.setup_b1(&mut b1);
        let b2 = flow.setup_b2();
        flow.schedule_first_events(&thin_offsets);
        (SetupPhases { a, b1, b2 }, flow)
    }

    /// Flow time of the event that produced the last item returned.
    pub fn flow_ns(&self) -> u64 {
        self.flow_ns
    }

    /// Every jump so far, in order.
    pub fn jumps(&self) -> &[Jump] {
        &self.jumps
    }

    /// Setup B1 (14.4): every maker's quotes, market by market, then the thin layer's
    /// orders. Returns each thin order's first replacement time, drawn here.
    fn setup_b1(&mut self, out: &mut Vec<Item>) -> Vec<u64> {
        let config = self.config;
        for market in &mut self.markets {
            market.first_quotes(&config, &mut self.clients, out);
        }
        let replace_every_ms = config.thin_replace_every_ns / 1_000_000;
        let mut offsets = Vec::new();
        for account in (0..config.thin_accounts).map(|i| THIN_BASE + i) {
            for _ in 0..config.thin_orders_each {
                // THIN draws, in 14.5's order: market, side, band share, quantity, offset.
                let rng = &mut self.thin_stream;
                let market = 1 + rng.below(u64::from(config.markets)) as MarketId;
                let side = if rng.below(2) == 0 { Side::Buy } else { Side::Sell };
                let percent = rng.in_range(config.thin_min_band_percent, config.thin_max_band_percent);
                let qty = rng.in_range(config.thin_min_qty, config.thin_max_qty);
                offsets.push(rng.below(replace_every_ms) * 1_000_000);
                let order = self.markets[usize::from(market) - 1].thin_order(side, percent, qty);
                let (order_id, item) = self.clients.place(account, order);
                self.thin.push(ThinOrder { account, market, order_id });
                out.push(item);
            }
        }
        offsets
    }

    /// Setup B2 (14.4): each high-leverage account, in id order, sends its setup IOCs at the
    /// starting fair value.
    fn setup_b2(&mut self) -> Vec<Item> {
        let config = self.config;
        let accounts = u32::from(config.markets) * config.high_leverage_per_market;
        let mut out = Vec::new();
        for index in 0..accounts {
            for _ in 0..config.high_leverage_setup_iocs {
                out.push(self.high_leverage_ioc(index));
            }
        }
        out
    }

    /// Account `HIGH_LEVERAGE_BASE + index` adds to its position: an IOC on its own side,
    /// with a quantity from the `HL` stream.
    fn high_leverage_ioc(&mut self, index: u32) -> Item {
        let config = self.config;
        let qty =
            self.high_leverage_stream.in_range(config.high_leverage_min_qty, config.high_leverage_max_qty);
        let (account, market, side) = config.high_leverage(index);
        let order = self.markets[usize::from(market) - 1].ioc(&config, side, qty);
        self.clients.place(account, order).1
    }

    /// Every recurring event at its first time (14.5's table).
    fn schedule_first_events(&mut self, thin_offsets: &[u64]) {
        let c = self.config;
        let mut first =
            |time: u64, event: Recurring, index: u32| self.due.push(Reverse(Due { time, event, index }));
        for m in 1..=c.markets {
            let phase = u64::from(m) * c.step_ns / u64::from(c.markets);
            first(phase, Recurring::FairStep, u32::from(m));
            first(phase + c.mark_every_ns, Recurring::MarkTick, u32::from(m));
        }
        first(c.taker_every_ns, Recurring::Taker, 0);
        first(c.high_leverage_every_ns / 2, Recurring::HighLeverage, 0);
        first(c.withdrawal_every_ns, Recurring::Withdrawal, 0);
        for (o, &offset) in thin_offsets.iter().enumerate() {
            first(offset, Recurring::ThinReplace, u32::try_from(o).expect("thin orders fit a u32"));
        }
    }

    /// How often `event` recurs.
    fn period(&self, event: Recurring) -> u64 {
        let c = &self.config;
        match event {
            Recurring::FairStep => c.step_ns,
            Recurring::MarkTick => c.mark_every_ns,
            Recurring::Taker => c.taker_every_ns,
            Recurring::HighLeverage => c.high_leverage_every_ns,
            Recurring::Withdrawal => c.withdrawal_every_ns,
            Recurring::ThinReplace => c.thin_replace_every_ns,
        }
    }

    /// Runs the next event: its items go to `pending`.
    fn run_next_event(&mut self) {
        let Reverse(due) = self.due.pop().expect("recurring events never run out");
        self.due.push(Reverse(Due { time: due.time + self.period(due.event), ..due }));
        self.flow_ns = due.time;
        let index = due.index as usize;
        match due.event {
            Recurring::FairStep => self.fair_step(index),
            Recurring::MarkTick => {
                let item = self.markets[index - 1].mark_tick(&self.config);
                self.pending.push(item);
            }
            Recurring::Taker => self.taker(),
            Recurring::HighLeverage => {
                let c = &self.config;
                let accounts = u64::from(c.markets) * u64::from(c.high_leverage_per_market);
                let index = self.high_leverage_stream.below(accounts) as u32;
                let item = self.high_leverage_ioc(index);
                self.pending.push(item);
            }
            Recurring::Withdrawal => {
                let c = &self.config;
                let account = TAKER_BASE + self.ops_stream.below(u64::from(c.takers)) as AccountId;
                let withdraw = Withdraw { amount: c.withdrawal_amount, account };
                self.pending.push(Item::Operator(Command::Withdraw(withdraw)));
            }
            Recurring::ThinReplace => self.thin_replace(index),
        }
    }

    /// `FairStep(m)`, recording the jump if it is one.
    fn fair_step(&mut self, m: usize) {
        let market = &mut self.markets[m - 1];
        if let Some(from) = market.fair_step(&self.config, &mut self.clients, &mut self.pending) {
            // A jump's `SetMark` is its first item, so it is the next one returned.
            let jump =
                Jump { item: self.emitted, flow_ns: self.flow_ns, market: market.id, from, to: market.fair };
            self.jumps.push(jump);
        }
    }

    /// `Taker`: an IOC through the fair value, from a random taker in a random market.
    fn taker(&mut self) {
        let config = self.config;
        // TAKER draws, in 14.5's order: account, market, side, quantity.
        let rng = &mut self.taker_stream;
        let account = TAKER_BASE + rng.below(u64::from(config.takers)) as AccountId;
        let market = rng.below(u64::from(config.markets)) as usize;
        let side = if rng.below(2) == 0 { Side::Buy } else { Side::Sell };
        let qty = rng.in_range(config.taker_min_qty, config.taker_max_qty);
        let order = self.markets[market].ioc(&config, side, qty);
        let (_, item) = self.clients.place(account, order);
        self.pending.push(item);
    }

    /// `ThinReplace(o)`: cancel the order, then place it again in the same market with a
    /// new side, band share and quantity from the `THIN` stream.
    fn thin_replace(&mut self, o: usize) {
        let config = self.config;
        let thin = self.thin[o];
        self.pending.push(self.clients.cancel(thin.account, thin.order_id, thin.market));
        let rng = &mut self.thin_stream;
        let side = if rng.below(2) == 0 { Side::Buy } else { Side::Sell };
        let percent = rng.in_range(config.thin_min_band_percent, config.thin_max_band_percent);
        let qty = rng.in_range(config.thin_min_qty, config.thin_max_qty);
        let order = self.markets[usize::from(thin.market) - 1].thin_order(side, percent, qty);
        let (order_id, item) = self.clients.place(thin.account, order);
        self.thin[o].order_id = order_id;
        self.pending.push(item);
    }
}

impl Iterator for MarketFlow {
    type Item = Item;

    /// The next timed item. Never `None`: the flow is endless.
    fn next(&mut self) -> Option<Item> {
        // Some events emit nothing (a step that moves no quote), so run events until one
        // has items.
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

/// Setup A (14.4): per market in id order, its parameters, its one-row tier table and its
/// first mark; the fund's capital; every client's deposit in id order; then the leverage
/// of every market maker (in its market) and every high-leverage account (the market's
/// maximum).
fn setup_a(config: &MarketFlowConfig) -> Vec<Item> {
    let mut commands = Vec::new();
    for market in 1..=config.markets {
        let params = market_params(market);
        commands.push(Command::SetMarketParams(params));
        let tier =
            SetRiskTier { lower_bound: 0, market, max_leverage: params.max_leverage, index: 0, count: 1 };
        commands.push(Command::SetRiskTier(tier));
        commands.push(Command::SetMark(SetMark { price: start_fair_value(market), market }));
    }
    commands.push(Command::Deposit(Deposit { amount: config.fund_deposit, account: FUND }));
    let accounts = config.client_accounts();
    for &account in &accounts {
        let cohort = config.cohort_of(account).expect("a client account has a cohort");
        commands.push(Command::Deposit(Deposit { amount: config.deposit_of(cohort), account }));
    }
    for &account in &accounts {
        let leverage = match config.cohort_of(account) {
            Some(Cohort::Maker { market, .. }) => Some((market, config.maker_leverage)),
            Some(Cohort::HighLeverage { market, .. }) => Some((market, class_of(market).max_leverage)),
            _ => None,
        };
        if let Some((market, leverage)) = leverage {
            commands.push(Command::SetLeverage(SetLeverage { account, market, leverage }));
        }
    }
    commands.into_iter().map(Item::Operator).collect()
}

// ---------------------------------------------------------------------------------------
// The Polymarket-shaped flow (D-034): its calibrated profile (the types, and the table that
// `tools/calibrate` generates) and its generator, which reuses the items, the plan and the
// account counters above. Nothing above uses them: the M3 flow is unchanged.

pub mod polymarket;
mod polymarket_profile;
pub mod profile;

#[cfg(test)]
mod tests;
