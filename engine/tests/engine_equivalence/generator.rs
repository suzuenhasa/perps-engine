//! The generator of `docs/RISK.md` 14.3: random [`Step`]s, and the [`Scenario`] that turns
//! each step into commands. As in the book test, a step is resolved when it runs, from the
//! engine's state at that moment, so cancels and modifies go to orders that rest, prices sit
//! at the band edges of the current mark, and withdrawals land on the reserve edge.
//!
//! **Markets** (created by the setup; commands to market 3, which never exists, are
//! rejected `UnknownMarket`):
//! - market 1: 20x, fees 400 ppm (taker) and 125 ppm (maker), the widest band rule 1
//!   allows (22,100 ppm), mark from 100,000. It opens with a one-row tier table.
//! - market 2: 50x with SP500's real tier table (Polymarket's instrument list, 2026-09-29:
//!   50x from $0, 25x from $500,000, ... 1x from $100M), band 8,600 ppm and `min_price`
//!   1,004 (RISK.md 5.3), mark from 75,024 (SP500 at 7,502.4). Its maker fee is a 50 ppm
//!   rebate, not SP500's 125 ppm, so that negative fees and their rounding run too (the
//!   spec's own model runs did the same).
//!
//! Each market has a second tier table: market 1's cuts leverage to 10x from $100, market
//! 2's halves SP500's leverages. Tier steps stage either table row by row, so commits land
//! on live markets and orders arrive while a table is half staged.
//!
//! **Accounts.** Four traders (1 to 4), two twins (5 and 6), and the insurance fund, which
//! can only receive deposits. The twins act only in the twin step, which gives both the same
//! free balance and leverage, and then has each take the same quantity, at the same price,
//! from one trader's resting order. So the twins always hold identical slots, with identical
//! liquidation keys, and when a mark crosses them the `(key, AccountId)` tie rule decides
//! who goes first (RISK.md 9.3).
//!
//! **Setup.** Each scenario starts with `SetMarketParams` for both markets, their tier
//! tables and first marks, a deposit to the fund (often none, so that bad debt shows as a
//! shortfall), and a deposit and a leverage per account and market; half the leverages are
//! the market's maximum. One market in eight opens late: only an `OpenMarket` step sends its
//! table and mark, so until then its orders are rejected `NoMark` and its marks
//! `NoRiskTiers`.
//!
//! **Steps** (weights out of 97): places 44, modifies 12, marks 10, cancels 6, closes 6,
//! deposits 4, leverage changes 4, withdrawals 3, twin steps 3, tier rows 3, reconfiguring
//! or opening a market 2. Each kind has invalid variants among its choices: fund orders,
//! reused sequence numbers, zero and overflowing amounts, prices beyond the band or the
//! range, quantities that are not positive or beyond `max_qty`, leverage 0 or above the
//! maximum, tier rows out of order, and market parameters that break a rule.
//! - Prices: mostly at or near an edge of the band of the current mark, on either side
//!   (an edge through the mark trades; the far edge rests and is swept when the mark moves),
//!   some near the mark, just short of the best opposite price, anywhere in the band.
//! - Sizes: 1 to 200,000 lots, and on a tiered table, sizes whose worst case lands within 2
//!   lots of a tier bound. Deposits go up to $100,000, so some accounts can reach them.
//! - Closes: a trader with a position closes it, or flips it, at the band's edge through the
//!   mark, against a counterparty's order at the same price, as the taker or as the maker
//!   (tests T3 and T4). A flip at the market's maximum leverage ends below maintenance
//!   margin, so the post-command pass liquidates the taker's or the maker's own slot.
//! - Marks: a random walk of steps up to 0.3%, so stale orders get swept, with jumps of 5%
//!   to 15%, so positions gap past their liquidation and bankruptcy prices, and now and then
//!   a return to the starting mark.

use engine::book::RestingOrder;
use engine::command::{
    CancelOrder, Command, Deposit, ModifyOrder, PlaceOrder, SetLeverage, SetMark, SetMarketParams,
    SetRiskTier, Withdraw,
};
use engine::engine::{EngineSnapshot, FUND, MarketSnapshot, SlotSnapshot};
use engine::event::Event;
use engine::money::{NOTIONAL_LIMIT, Tier};
use engine::types::{AccountId, MarketId, Micros, OrderId, Price, Qty, Side, TimeInForce, order_id};
use proptest::prelude::*;
use proptest::sample::Index;

use super::checker::withdrawal_sums;
use super::state::{empty_slot, free_of, market_of, next_seq_of, resting_orders, slot_of};

pub const MARKET_20X: MarketId = 1;
pub const MARKET_50X: MarketId = 2;
/// Never created.
pub const NO_MARKET: MarketId = 3;
pub const TRADERS: [AccountId; 4] = [1, 2, 3, 4];
pub const TWINS: [AccountId; 2] = [5, 6];

// ---------------------------------------------------------------------------------------
// The markets.

const fn tier(lower_bound: Micros, max_leverage: u16) -> Tier {
    Tier { lower_bound, max_leverage }
}

const TABLE_20X: [Tier; 1] = [tier(0, 20)];
/// 10x from $100 (1,000 lots at mark 100,000), so tier crossings happen on market 1 too.
const TABLE_20X_CUT: [Tier; 2] = [tier(0, 20), tier(100_000_000, 10)];
/// SP500's live table, bounds in micros.
const SP500_TIERS: [Tier; 8] = [
    tier(0, 50),
    tier(500_000_000_000, 25),
    tier(1_000_000_000_000, 20),
    tier(5_000_000_000_000, 15),
    tier(10_000_000_000_000, 10),
    tier(25_000_000_000_000, 5),
    tier(50_000_000_000_000, 2),
    tier(100_000_000_000_000, 1),
];
/// SP500's bounds with each leverage halved, rounded up.
const SP500_TIERS_HALVED: [Tier; 8] = [
    tier(0, 25),
    tier(500_000_000_000, 13),
    tier(1_000_000_000_000, 10),
    tier(5_000_000_000_000, 8),
    tier(10_000_000_000_000, 5),
    tier(25_000_000_000_000, 3),
    tier(50_000_000_000_000, 1),
    tier(100_000_000_000_000, 1),
];

/// What the generator knows about a market.
#[derive(Clone, Copy, Debug)]
pub struct MarketSpec {
    pub params: SetMarketParams,
    pub initial_mark: Price,
    /// Table 0 is the one the market opens with.
    pub tables: [&'static [Tier]; 2],
    /// The lowest `min_price` band rule 2 allows with these parameters (RISK.md 5.3).
    pub rule_2_floor: Price,
}

/// Market 1 or 2. Market 3 (never created) borrows market 1's numbers, so that its
/// commands are well formed apart from the market.
pub fn spec(market: MarketId) -> MarketSpec {
    let market_20x = MarketSpec {
        params: SetMarketParams {
            min_price: 1_000,
            max_price: 300_000,
            maker_fee_ppm: 125,
            taker_fee_ppm: 400,
            price_band_ppm: 22_100,
            market,
            max_leverage: 20,
        },
        initial_mark: 100_000,
        tables: [&TABLE_20X, &TABLE_20X_CUT],
        rule_2_floor: 402,
    };
    match market {
        MARKET_50X => MarketSpec {
            params: SetMarketParams {
                min_price: 1_004,
                max_price: 225_000,
                maker_fee_ppm: -50,
                taker_fee_ppm: 400,
                price_band_ppm: 8_600,
                market,
                max_leverage: 50,
            },
            initial_mark: 75_024,
            tables: [&SP500_TIERS, &SP500_TIERS_HALVED],
            rule_2_floor: 1_004,
        },
        _ => market_20x,
    }
}

/// A market as a step sees it: the generator's spec, and the engine's state if the market
/// exists. Without a mark, the band collapses to the starting mark (such orders are
/// rejected `NoMark` anyway).
struct MarketView<'a> {
    spec: MarketSpec,
    state: Option<&'a MarketSnapshot>,
}

impl<'a> MarketView<'a> {
    fn new(state: &'a EngineSnapshot, market: MarketId) -> Self {
        MarketView { spec: spec(market), state: market_of(state, market) }
    }

    fn has_mark(&self) -> bool {
        self.state.is_some_and(|m| m.mark.is_some())
    }

    fn mark(&self) -> Price {
        self.state.and_then(|m| m.mark).unwrap_or(self.spec.initial_mark)
    }

    fn upper(&self) -> Price {
        self.state.filter(|m| m.mark.is_some()).map_or(self.mark(), |m| m.upper)
    }

    fn lower(&self) -> Price {
        self.state.filter(|m| m.mark.is_some()).map_or(self.mark(), |m| m.lower)
    }

    fn best_bid(&self) -> Option<Price> {
        self.state.and_then(|m| m.book.bids.first()).map(|o| o.price)
    }

    fn best_ask(&self) -> Option<Price> {
        self.state.and_then(|m| m.book.asks.first()).map(|o| o.price)
    }

    fn max_qty(&self) -> Qty {
        self.state.map_or(NOTIONAL_LIMIT / self.spec.params.max_price, |m| m.max_qty)
    }

    /// The live tier table (empty before the first commit).
    fn tiers(&self) -> &[Tier] {
        self.state.map_or(&[], |m| &m.tiers)
    }

    fn slot(&self, account: AccountId) -> SlotSnapshot {
        self.state.map_or(empty_slot(account), |m| slot_of(m, account))
    }

    fn resting(&self) -> Vec<RestingOrder> {
        self.state.map_or(Vec::new(), |m| resting_orders(m).copied().collect())
    }
}

// ---------------------------------------------------------------------------------------
// Steps.

/// The scenario's first commands. See the module docs.
#[derive(Clone, Debug)]
pub struct Setup {
    /// Per market (1, then 2): opened only by a later `OpenMarket` step.
    pub opens_late: [bool; 2],
    pub fund_deposit: Micros,
    pub trader_deposits: [Micros; 4],
    /// Per trader, then per market.
    pub trader_leverages: [[LeverageChoice; 2]; 4],
    pub twin_deposit: Micros,
    /// Per market; the same for both twins.
    pub twin_leverages: [LeverageChoice; 2],
}

/// One step of a scenario. It becomes one command, or several for the twin step and the
/// tier and market steps, when it runs.
#[derive(Clone, Debug)]
pub enum Step {
    Place(PlaceStep),
    /// An order against the account's position at the band's edge, and a counterparty's
    /// order that trades with it. See [`resolve_close`].
    ClosePosition(CloseStep),
    Cancel {
        market: MarketId,
        target: Target,
    },
    Modify {
        market: MarketId,
        target: Target,
        kind: ModifyKind,
    },
    Mark {
        market: MarketId,
        movement: MarkMove,
    },
    Deposit {
        account: AccountId,
        amount: DepositAmount,
    },
    Withdraw {
        account: AccountId,
        amount: WithdrawAmount,
    },
    SetLeverage {
        account: AccountId,
        market: MarketId,
        leverage: LeverageChoice,
    },
    Twin(TwinStep),
    /// The next `rows` rows of a tier table: `table` if none is being staged, else the one
    /// being staged.
    TierRows {
        market: MarketId,
        table: usize,
        rows: usize,
    },
    BadTierRow {
        market: MarketId,
        kind: BadTierRow,
    },
    /// `SetMarketParams` again: the market's own parameters (accepted only on an empty
    /// market, which then needs a tier table and a mark again), or ones that break a rule.
    Reconfigure {
        market: MarketId,
        bad: Option<BadParams>,
    },
    /// Sends whatever the market still needs to open (the rest of its tier table, a first
    /// mark); nothing if it is open.
    OpenMarket {
        market: MarketId,
    },
}

#[derive(Clone, Debug)]
pub struct PlaceStep {
    pub account: AccountId,
    pub market: MarketId,
    pub side: Side,
    pub price: PriceChoice,
    pub qty: QtyChoice,
    pub tif: TimeInForce,
    pub post_only: bool,
    /// A sequence number below the account's next one (rejected `Duplicate`).
    pub reused_sequence: Option<Index>,
}

/// A price, relative to the band of the current mark.
#[derive(Clone, Copy, Debug)]
pub enum PriceChoice {
    /// `offset` ticks inside the band's edge through the mark: a buy near `upper`, a sell
    /// near `lower`. Trades if there is anything to trade with.
    AggressiveEdge(Price),
    /// `offset` ticks inside the other edge: a buy near `lower`, a sell near `upper`. Rests,
    /// and is the first to be swept when the mark moves toward it.
    PassiveEdge(Price),
    NearMark(Price),
    /// One tick short of the best opposite price, but inside the band: rests at the top.
    JustShortOfBest,
    InBand(Index),
    /// `by` ticks beyond the band's edge through the mark (rejected `PriceBand`).
    BeyondBand(Price),
    /// 0, or one above `max_price` (rejected `InvalidPrice`).
    OutOfRange {
        high: bool,
    },
}

/// A quantity.
#[derive(Clone, Copy, Debug)]
pub enum QtyChoice {
    Lots(Qty),
    /// A size that takes the slot's worst-case size to within `delta` lots of a bound of the
    /// live tier table (a random row above the first).
    NearTierBound {
        tier: Index,
        delta: Qty,
    },
    /// A size that takes the worst-case size to exactly `max_qty` (or one above: rejected
    /// `SizeLimit`).
    MaxQtyEdge {
        over: bool,
    },
    /// `i64::MAX` lots: rejected `SizeLimit` without overflowing.
    Enormous,
    NotPositive(Qty),
}

/// A trader closes or flips its position at the band's edge (as in tests T3 and T4),
/// trading with `counterparty`: as the taker, or with its order resting first as the maker.
#[derive(Clone, Copy, Debug)]
pub struct CloseStep {
    /// One of the traders that hold a position in the market (trader 1 if none does).
    pub account: Index,
    pub market: MarketId,
    pub size: CloseSize,
    pub counterparty: AccountId,
    pub account_takes: bool,
    pub tif: TimeInForce,
}

/// How much of its position a `ClosePosition` step trades.
#[derive(Clone, Copy, Debug)]
pub enum CloseSize {
    /// All of it: strictly reducing, if the account has no other orders on that side.
    All,
    /// Twice the position: a flip, which is margin-checked.
    Twice,
    OneLot,
}

/// Which order a cancel or modify goes to.
#[derive(Clone, Copy, Debug)]
pub enum Target {
    /// One of the market's resting orders.
    Resting(Index),
    /// Any order id accepted so far, in any market; usually one that has gone.
    AnyIssued(Index),
}

/// What a modify asks for, relative to the order's state when it runs (as in the book test),
/// with sizes as total sizes (D-008).
#[derive(Clone, Copy, Debug)]
pub enum ModifyKind {
    /// Removal: a total size at or up to `below` under what has filled.
    AtOrBelowFilled {
        below: Qty,
    },
    /// Shrink in place: `by` fewer left to fill (at least 1 left).
    Shrink {
        by: Qty,
    },
    SameSize,
    /// A replace: `by` more to fill.
    Grow {
        by: Qty,
    },
    /// A replace at a new price, same size.
    NewPrice(PriceChoice),
    /// A replace at the best opposite price, or `through` ticks past it: trades.
    Cross {
        through: Price,
    },
    /// Any price and size, either of which may be invalid.
    Raw {
        price: PriceChoice,
        size: Qty,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum MarkMove {
    /// A step of `ppm` parts per million of the current mark.
    Step(i64),
    Jump {
        percent: i64,
        up: bool,
    },
    BackToStart,
    /// 0, or one above `max_price` (rejected `InvalidPrice`).
    OutOfRange {
        high: bool,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum DepositAmount {
    Amount(Micros),
    Zero,
    /// One more than the balance can take (rejected `InvalidAmount`); 0 on an empty balance.
    Overflowing,
}

#[derive(Clone, Copy, Debug)]
pub enum WithdrawAmount {
    /// The most the withdrawal rule allows (at least 1).
    ReserveEdge,
    /// One more than that.
    PastReserveEdge,
    /// This many thousandths of the free balance (at least 1).
    Part(i128),
    MoreThanFree,
    Zero,
}

#[derive(Clone, Copy, Debug)]
pub enum LeverageChoice {
    /// 1 to the market's maximum.
    Any(Index),
    Max,
    One,
    Zero,
    AboveMax,
}

/// `counterparty` rests twice `qty` at one price, and both twins take `qty` of it on `side`.
#[derive(Clone, Debug)]
pub struct TwinStep {
    pub market: MarketId,
    pub side: Side,
    pub qty: Qty,
    /// The free balance both twins are topped up to first, if they have less.
    pub deposit: Micros,
    pub leverage: LeverageChoice,
    pub counterparty: AccountId,
}

/// A tier row that breaks a rule of RISK.md 6.9 (rejected `InvalidParams`).
#[derive(Clone, Copy, Debug)]
pub enum BadTierRow {
    SkipsAhead,
    LeverageAboveMax,
    FirstBoundNotZero,
    TooManyRows,
    BoundNotRising,
}

/// Market parameters that break a rule of RISK.md 6.8 (rejected `InvalidParams`).
#[derive(Clone, Copy, Debug)]
pub enum BadParams {
    BandTooWide,
    MinPriceBelowRule2,
    RangeTooWide,
    NoLeverage,
    NegativeTakerFee,
}

// ---------------------------------------------------------------------------------------
// Strategies.

fn trader() -> impl Strategy<Value = AccountId> {
    prop::sample::select(TRADERS.to_vec())
}

/// A trader, or the fund `fund_weight` times in 20.
fn trader_or_fund(fund_weight: u32) -> impl Strategy<Value = AccountId> {
    prop_oneof![20 => trader(), fund_weight => Just(FUND)]
}

/// Market 1 or 2, and now and then market 3, which doesn't exist.
fn any_market() -> impl Strategy<Value = MarketId> {
    prop_oneof![10 => Just(MARKET_20X), 10 => Just(MARKET_50X), 1 => Just(NO_MARKET)]
}

fn real_market() -> impl Strategy<Value = MarketId> {
    prop_oneof![Just(MARKET_20X), Just(MARKET_50X)]
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Buy), Just(Side::Sell)]
}

fn price_choice() -> impl Strategy<Value = PriceChoice> {
    prop_oneof![
        4 => (0i64..=3).prop_map(PriceChoice::AggressiveEdge),
        4 => (0i64..=3).prop_map(PriceChoice::PassiveEdge),
        3 => (-3i64..=3).prop_map(PriceChoice::NearMark),
        4 => Just(PriceChoice::JustShortOfBest),
        3 => any::<Index>().prop_map(PriceChoice::InBand),
        1 => (1i64..=3).prop_map(PriceChoice::BeyondBand),
        1 => any::<bool>().prop_map(|high| PriceChoice::OutOfRange { high }),
    ]
}

fn lots() -> impl Strategy<Value = Qty> {
    prop_oneof![3 => 1i64..=100, 3 => 1i64..=1_000, 2 => 1i64..=5_000, 1 => 5_000i64..=200_000]
}

fn qty_choice() -> impl Strategy<Value = QtyChoice> {
    prop_oneof![
        16 => lots().prop_map(QtyChoice::Lots),
        3 => (any::<Index>(), -2i64..=2).prop_map(|(tier, delta)| QtyChoice::NearTierBound { tier, delta }),
        1 => any::<bool>().prop_map(|over| QtyChoice::MaxQtyEdge { over }),
        1 => Just(QtyChoice::Enormous),
        1 => prop_oneof![Just(0i64), Just(-1i64), Just(-1_000i64)].prop_map(QtyChoice::NotPositive),
    ]
}

fn place_step() -> impl Strategy<Value = Step> {
    let tif = prop_oneof![4 => Just(TimeInForce::Gtc), 1 => Just(TimeInForce::Ioc)];
    let reused_sequence = prop::option::weighted(0.05, any::<Index>());
    (
        trader_or_fund(1),
        any_market(),
        side(),
        price_choice(),
        qty_choice(),
        tif,
        prop::bool::weighted(0.15),
        reused_sequence,
    )
        .prop_map(|(account, market, side, price, qty, tif, post_only, reused_sequence)| {
            Step::Place(PlaceStep { account, market, side, price, qty, tif, post_only, reused_sequence })
        })
}

fn close_position_step() -> impl Strategy<Value = Step> {
    let size =
        prop_oneof![2 => Just(CloseSize::All), 2 => Just(CloseSize::Twice), 1 => Just(CloseSize::OneLot)];
    let tif = prop_oneof![Just(TimeInForce::Gtc), Just(TimeInForce::Ioc)];
    (any::<Index>(), real_market(), size, trader(), any::<bool>(), tif).prop_map(
        |(account, market, size, counterparty, account_takes, tif)| {
            Step::ClosePosition(CloseStep { account, market, size, counterparty, account_takes, tif })
        },
    )
}

fn target() -> impl Strategy<Value = Target> {
    prop_oneof![
        9 => any::<Index>().prop_map(Target::Resting),
        1 => any::<Index>().prop_map(Target::AnyIssued),
    ]
}

fn modify_kind() -> impl Strategy<Value = ModifyKind> {
    prop_oneof![
        3 => (0i64..=2).prop_map(|below| ModifyKind::AtOrBelowFilled { below }),
        2 => (1i64..=500).prop_map(|by| ModifyKind::Shrink { by }),
        1 => Just(ModifyKind::SameSize),
        2 => (1i64..=2_000).prop_map(|by| ModifyKind::Grow { by }),
        3 => price_choice().prop_map(ModifyKind::NewPrice),
        3 => (0i64..=2).prop_map(|through| ModifyKind::Cross { through }),
        1 => (price_choice(), -1i64..=5_000).prop_map(|(price, size)| ModifyKind::Raw { price, size }),
    ]
}

fn mark_move() -> impl Strategy<Value = MarkMove> {
    prop_oneof![
        12 => (-3_000i64..=3_000).prop_map(MarkMove::Step),
        3 => (5i64..=15, any::<bool>()).prop_map(|(percent, up)| MarkMove::Jump { percent, up }),
        1 => Just(MarkMove::BackToStart),
        1 => any::<bool>().prop_map(|high| MarkMove::OutOfRange { high }),
    ]
}

/// $5 to $100,000.
fn deposit_value() -> impl Strategy<Value = Micros> {
    prop::sample::select(vec![
        5_000_000,
        20_000_000,
        100_000_000,
        1_000_000_000,
        20_000_000_000,
        100_000_000_000,
    ])
}

fn deposit_amount() -> impl Strategy<Value = DepositAmount> {
    prop_oneof![
        12 => deposit_value().prop_map(DepositAmount::Amount),
        1 => Just(DepositAmount::Zero),
        1 => Just(DepositAmount::Overflowing),
    ]
}

fn withdraw_amount() -> impl Strategy<Value = WithdrawAmount> {
    prop_oneof![
        4 => Just(WithdrawAmount::ReserveEdge),
        3 => Just(WithdrawAmount::PastReserveEdge),
        3 => (1i128..=1_000).prop_map(WithdrawAmount::Part),
        1 => Just(WithdrawAmount::MoreThanFree),
        1 => Just(WithdrawAmount::Zero),
    ]
}

/// Valid leverages only, for the setup and the twins: half of them the market's maximum,
/// where a flip at the band edge ends below maintenance margin (test T4).
fn valid_leverage() -> impl Strategy<Value = LeverageChoice> {
    prop_oneof![Just(LeverageChoice::Max), any::<Index>().prop_map(LeverageChoice::Any)]
}

fn leverage_choice() -> impl Strategy<Value = LeverageChoice> {
    prop_oneof![
        10 => valid_leverage(),
        // A cut to 1x, which the free balance often can't cover.
        3 => Just(LeverageChoice::One),
        1 => Just(LeverageChoice::Zero),
        1 => Just(LeverageChoice::AboveMax),
    ]
}

fn twin_step() -> impl Strategy<Value = Step> {
    let qty = prop_oneof![1i64..=300, 1i64..=5_000];
    (real_market(), side(), qty, deposit_value(), valid_leverage(), trader()).prop_map(
        |(market, side, qty, deposit, leverage, counterparty)| {
            Step::Twin(TwinStep { market, side, qty, deposit, leverage, counterparty })
        },
    )
}

fn tier_step() -> impl Strategy<Value = Step> {
    let bad_row = prop_oneof![
        Just(BadTierRow::SkipsAhead),
        Just(BadTierRow::LeverageAboveMax),
        Just(BadTierRow::FirstBoundNotZero),
        Just(BadTierRow::TooManyRows),
        Just(BadTierRow::BoundNotRising),
    ];
    prop_oneof![
        3 => (real_market(), 0usize..=1, 1usize..=8)
            .prop_map(|(market, table, rows)| Step::TierRows { market, table, rows }),
        1 => (real_market(), bad_row).prop_map(|(market, kind)| Step::BadTierRow { market, kind }),
    ]
}

fn market_lifecycle_step() -> impl Strategy<Value = Step> {
    let bad_params = prop_oneof![
        Just(BadParams::BandTooWide),
        Just(BadParams::MinPriceBelowRule2),
        Just(BadParams::RangeTooWide),
        Just(BadParams::NoLeverage),
        Just(BadParams::NegativeTakerFee),
    ];
    prop_oneof![
        2 => (real_market(), prop::option::of(bad_params))
            .prop_map(|(market, bad)| Step::Reconfigure { market, bad }),
        3 => real_market().prop_map(|market| Step::OpenMarket { market }),
    ]
}

/// One step. The weights are in the module docs.
pub fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        44 => place_step(),
        6 => close_position_step(),
        12 => (any_market(), target(), modify_kind())
            .prop_map(|(market, target, kind)| Step::Modify { market, target, kind }),
        10 => (any_market(), mark_move()).prop_map(|(market, movement)| Step::Mark { market, movement }),
        6 => (any_market(), target()).prop_map(|(market, target)| Step::Cancel { market, target }),
        4 => (trader_or_fund(10), deposit_amount())
            .prop_map(|(account, amount)| Step::Deposit { account, amount }),
        3 => (trader_or_fund(1), withdraw_amount())
            .prop_map(|(account, amount)| Step::Withdraw { account, amount }),
        4 => (trader_or_fund(1), any_market(), leverage_choice())
            .prop_map(|(account, market, leverage)| Step::SetLeverage { account, market, leverage }),
        3 => twin_step(),
        3 => tier_step(),
        2 => market_lifecycle_step(),
    ]
}

pub fn setup() -> impl Strategy<Value = Setup> {
    let fund_deposit = prop::sample::select(vec![0, 0, 1_000_000, 1_000_000_000, 100_000_000_000]);
    (
        prop::array::uniform2(prop::bool::weighted(0.125)),
        fund_deposit,
        prop::array::uniform4(deposit_value()),
        prop::array::uniform4(prop::array::uniform2(valid_leverage())),
        deposit_value(),
        prop::array::uniform2(valid_leverage()),
    )
        .prop_map(
            |(opens_late, fund_deposit, trader_deposits, trader_leverages, twin_deposit, twin_leverages)| {
                Setup {
                    opens_late,
                    fund_deposit,
                    trader_deposits,
                    trader_leverages,
                    twin_deposit,
                    twin_leverages,
                }
            },
        )
}

// ---------------------------------------------------------------------------------------
// Resolving choices against the state.

impl PriceChoice {
    fn resolve(self, side: Side, market: &MarketView) -> Price {
        let (mark, upper, lower) = (market.mark(), market.upper(), market.lower());
        match (self, side) {
            (PriceChoice::AggressiveEdge(offset), Side::Buy) => upper - offset,
            (PriceChoice::AggressiveEdge(offset), Side::Sell) => lower + offset,
            (PriceChoice::PassiveEdge(offset), Side::Buy) => lower + offset,
            (PriceChoice::PassiveEdge(offset), Side::Sell) => upper - offset,
            (PriceChoice::NearMark(offset), _) => mark + offset,
            (PriceChoice::JustShortOfBest, Side::Buy) => {
                market.best_ask().map_or(mark, |ask| (ask - 1).min(upper))
            }
            (PriceChoice::JustShortOfBest, Side::Sell) => {
                market.best_bid().map_or(mark, |bid| (bid + 1).max(lower))
            }
            (PriceChoice::InBand(index), _) => lower + index.index((upper - lower + 1) as usize) as Price,
            (PriceChoice::BeyondBand(by), Side::Buy) => upper + by,
            (PriceChoice::BeyondBand(by), Side::Sell) => lower - by,
            (PriceChoice::OutOfRange { high: true }, _) => market.spec.params.max_price + 1,
            (PriceChoice::OutOfRange { high: false }, _) => 0,
        }
    }
}

impl QtyChoice {
    fn resolve(self, side: Side, market: &MarketView, slot: &SlotSnapshot) -> Qty {
        // What the order's side already adds up to toward the worst-case size.
        let exposure = match side {
            Side::Buy => slot.pos + slot.open_buys,
            Side::Sell => slot.open_sells - slot.pos,
        };
        match self {
            QtyChoice::Lots(qty) | QtyChoice::NotPositive(qty) => qty,
            QtyChoice::NearTierBound { tier, delta } => {
                let tiers = market.tiers();
                if tiers.len() < 2 {
                    return 1_000;
                }
                let bound = tiers[1 + tier.index(tiers.len() - 1)].lower_bound;
                // The smallest size whose notional reaches the bound, give or take `delta`.
                let target = (-(-bound).div_euclid(market.mark()) + delta).max(1);
                let qty = target - exposure;
                if qty >= 1 { qty } else { target }
            }
            QtyChoice::MaxQtyEdge { over } => market.max_qty() + Qty::from(over) - exposure,
            QtyChoice::Enormous => Qty::MAX,
        }
    }
}

impl MarkMove {
    fn resolve(self, market: &MarketView) -> Price {
        let (base, params) = (market.mark(), market.spec.params);
        let price = match self {
            MarkMove::Step(ppm) => base + base * ppm / 1_000_000,
            MarkMove::Jump { percent, up: true } => base + base * percent / 100,
            MarkMove::Jump { percent, up: false } => base - base * percent / 100,
            MarkMove::BackToStart => market.spec.initial_mark,
            MarkMove::OutOfRange { high } => return if high { params.max_price + 1 } else { 0 },
        };
        price.clamp(params.min_price, params.max_price)
    }
}

impl LeverageChoice {
    fn resolve(self, max_leverage: u16) -> u16 {
        match self {
            LeverageChoice::Any(index) => 1 + index.index(usize::from(max_leverage)) as u16,
            LeverageChoice::Max => max_leverage,
            LeverageChoice::One => 1,
            LeverageChoice::Zero => 0,
            LeverageChoice::AboveMax => max_leverage + 1,
        }
    }
}

impl DepositAmount {
    fn resolve(self, state: &EngineSnapshot, account: AccountId) -> Micros {
        let balance = if account == FUND { state.fund_balance } else { free_of(state, account) };
        match self {
            DepositAmount::Amount(amount) => amount,
            DepositAmount::Zero => 0,
            DepositAmount::Overflowing if balance > 0 => Micros::MAX - balance + 1,
            DepositAmount::Overflowing => 0,
        }
    }
}

impl WithdrawAmount {
    fn resolve(self, state: &EngineSnapshot, account: AccountId) -> Micros {
        let (free, locked, reserve) = withdrawal_sums(state, account);
        // The most a withdrawal may take: all of the free balance, unless the reserve binds.
        let room = free.min(free + locked - reserve);
        let amount = match self {
            WithdrawAmount::ReserveEdge => room.max(1),
            WithdrawAmount::PastReserveEdge => room.max(0) + 1,
            WithdrawAmount::Part(permille) => (free * permille / 1_000).max(1),
            WithdrawAmount::MoreThanFree => free + 1,
            WithdrawAmount::Zero => 0,
        };
        Micros::try_from(amount).expect("a withdrawal amount fits in i64")
    }
}

// ---------------------------------------------------------------------------------------
// Commands.

fn place(
    account: AccountId,
    seq: u64,
    market: MarketId,
    side: Side,
    price: Price,
    qty: Qty,
    tif: TimeInForce,
) -> Command {
    let seq = u32::try_from(seq).expect("sequence numbers stay small");
    Command::PlaceOrder(PlaceOrder {
        order_id: order_id(account, seq),
        price,
        qty,
        market,
        side,
        tif,
        post_only: false,
    })
}

fn cancel(order_id: OrderId, market: MarketId) -> Command {
    Command::CancelOrder(CancelOrder { order_id, market })
}

fn deposit(account: AccountId, amount: Micros) -> Command {
    Command::Deposit(Deposit { amount, account })
}

fn set_leverage(account: AccountId, market: MarketId, leverage: u16) -> Command {
    Command::SetLeverage(SetLeverage { account, market, leverage })
}

fn set_mark(market: MarketId, price: Price) -> Command {
    Command::SetMark(SetMark { price, market })
}

fn tier_row(market: MarketId, table: &[Tier], index: usize) -> Command {
    let count = u8::try_from(table.len()).expect("at most 8 rows");
    let index = u8::try_from(index).expect("at most 8 rows");
    let row = table[usize::from(index)];
    Command::SetRiskTier(SetRiskTier {
        lower_bound: row.lower_bound,
        market,
        max_leverage: row.max_leverage,
        index,
        count,
    })
}

// ---------------------------------------------------------------------------------------
// The scenario: resolving steps into commands.

/// What a scenario remembers between steps. See the module docs.
#[derive(Debug, Default)]
pub struct Scenario {
    /// Every order id accepted so far.
    issued: Vec<OrderId>,
    /// Per market id: which of its two tier tables is being staged.
    staging: [usize; 3],
}

impl Scenario {
    /// The scenario's first commands (module docs, "Setup").
    pub fn setup_commands(setup: &Setup) -> Vec<Command> {
        let markets = [MARKET_20X, MARKET_50X];
        let mut commands: Vec<Command> =
            markets.iter().map(|&m| Command::SetMarketParams(spec(m).params)).collect();
        for (&market, &late) in markets.iter().zip(&setup.opens_late) {
            if !late {
                let table = spec(market).tables[0];
                commands.extend((0..table.len()).map(|index| tier_row(market, table, index)));
                commands.push(set_mark(market, spec(market).initial_mark));
            }
        }
        if setup.fund_deposit > 0 {
            commands.push(deposit(FUND, setup.fund_deposit));
        }
        for (i, &trader) in TRADERS.iter().enumerate() {
            commands.push(deposit(trader, setup.trader_deposits[i]));
            for (&market, leverage) in markets.iter().zip(setup.trader_leverages[i]) {
                commands.push(set_leverage(
                    trader,
                    market,
                    leverage.resolve(spec(market).params.max_leverage),
                ));
            }
        }
        for twin in TWINS {
            commands.push(deposit(twin, setup.twin_deposit));
            for (&market, leverage) in markets.iter().zip(setup.twin_leverages) {
                commands.push(set_leverage(twin, market, leverage.resolve(spec(market).params.max_leverage)));
            }
        }
        commands
    }

    /// Turns one step into commands, from the state before it runs.
    pub fn resolve(&mut self, step: &Step, state: &EngineSnapshot) -> Vec<Command> {
        match step {
            Step::Place(place) => vec![self.resolve_place(place, state)],
            Step::ClosePosition(close) => resolve_close(close, state),
            Step::Cancel { market, target } => {
                let view = MarketView::new(state, *market);
                vec![cancel(self.target_id(*target, &view.resting(), false), *market)]
            }
            Step::Modify { market, target, kind } => {
                vec![self.resolve_modify(*market, *target, *kind, state)]
            }
            Step::Mark { market, movement } => {
                vec![set_mark(*market, movement.resolve(&MarketView::new(state, *market)))]
            }
            Step::Deposit { account, amount } => vec![deposit(*account, amount.resolve(state, *account))],
            Step::Withdraw { account, amount } => {
                vec![Command::Withdraw(Withdraw {
                    amount: amount.resolve(state, *account),
                    account: *account,
                })]
            }
            Step::SetLeverage { account, market, leverage } => {
                vec![set_leverage(*account, *market, leverage.resolve(spec(*market).params.max_leverage))]
            }
            Step::Twin(twin) => resolve_twin(twin, state),
            Step::TierRows { market, table, rows } => self.tier_rows(*market, *table, *rows, state),
            Step::BadTierRow { market, kind } => vec![bad_tier_row(*market, *kind, state)],
            Step::Reconfigure { market, bad } => vec![Command::SetMarketParams(market_params(*market, *bad))],
            Step::OpenMarket { market } => self.open_market(*market, state),
        }
    }

    /// Remembers the ids of accepted orders, for later cancels and modifies.
    pub fn record(&mut self, command: &Command, events: &[Event]) {
        if let (Command::PlaceOrder(order), Some(Event::Ack(_))) = (command, events.first()) {
            self.issued.push(order.order_id);
        }
    }

    fn resolve_place(&self, step: &PlaceStep, state: &EngineSnapshot) -> Command {
        let market = MarketView::new(state, step.market);
        let price = step.price.resolve(step.side, &market);
        let qty = step.qty.resolve(step.side, &market, &market.slot(step.account));
        let next = next_seq_of(state, step.account);
        let seq = match step.reused_sequence {
            Some(index) if next > 0 => index.index(next as usize) as u64,
            _ => next,
        };
        let seq = u32::try_from(seq).expect("sequence numbers stay small");
        Command::PlaceOrder(PlaceOrder {
            order_id: order_id(step.account, seq),
            price,
            qty,
            market: step.market,
            side: step.side,
            tif: step.tif,
            post_only: step.post_only,
        })
    }

    /// The id a cancel or modify goes to. With `prefer_partly_filled`, a resting target is
    /// one that has filled something, if there is one (only those can be sized down to
    /// their fills).
    fn target_id(&self, target: Target, resting: &[RestingOrder], prefer_partly_filled: bool) -> OrderId {
        let partly_filled: Vec<&RestingOrder> = resting.iter().filter(|o| o.filled > 0).collect();
        match target {
            Target::Resting(index) if prefer_partly_filled && !partly_filled.is_empty() => {
                partly_filled[index.index(partly_filled.len())].order_id
            }
            Target::Resting(index) if !resting.is_empty() => resting[index.index(resting.len())].order_id,
            Target::Resting(index) | Target::AnyIssued(index) if !self.issued.is_empty() => {
                self.issued[index.index(self.issued.len())]
            }
            // Nothing issued yet: an id that never existed.
            _ => order_id(TRADERS[0], 999_999),
        }
    }

    fn resolve_modify(
        &self,
        market_id: MarketId,
        target: Target,
        kind: ModifyKind,
        state: &EngineSnapshot,
    ) -> Command {
        let market = MarketView::new(state, market_id);
        let resting = market.resting();
        let id = self.target_id(target, &resting, matches!(kind, ModifyKind::AtOrBelowFilled { .. }));
        let modify = |new_price, new_size| {
            Command::ModifyOrder(ModifyOrder { order_id: id, new_price, new_size, market: market_id })
        };
        let Some(order) = resting.iter().find(|o| o.order_id == id) else {
            // Not resting here: rejected `UnknownOrder`, whatever the price and size.
            return match kind {
                ModifyKind::Raw { price, size } => modify(price.resolve(Side::Buy, &market), size),
                _ => modify(market.mark(), 1),
            };
        };
        let total = order.filled + order.qty;
        match kind {
            // With nothing filled, the size is 0: rejected `InvalidQty`.
            ModifyKind::AtOrBelowFilled { below } => {
                modify(order.price, if order.filled == 0 { 0 } else { (order.filled - below).max(1) })
            }
            ModifyKind::Shrink { by } => modify(order.price, order.filled + (order.qty - by).max(1)),
            ModifyKind::SameSize => modify(order.price, total),
            ModifyKind::Grow { by } => modify(order.price, total + by),
            ModifyKind::NewPrice(price) => modify(price.resolve(order.side, &market), total),
            ModifyKind::Cross { through } => {
                let crossing = match order.side {
                    Side::Buy => market.best_ask().map(|ask| ask + through),
                    Side::Sell => market.best_bid().map(|bid| bid - through),
                };
                modify(crossing.unwrap_or(order.price), total)
            }
            ModifyKind::Raw { price, size } => modify(price.resolve(order.side, &market), size),
        }
    }

    /// Up to `rows` rows of the table being staged, or of `table` if none is.
    fn tier_rows(
        &mut self,
        market: MarketId,
        table: usize,
        rows: usize,
        state: &EngineSnapshot,
    ) -> Vec<Command> {
        let Some(snapshot) = market_of(state, market) else { return Vec::new() };
        let staged = usize::from(snapshot.staged_rows);
        if staged == 0 {
            self.staging[usize::from(market)] = table;
        }
        let table = spec(market).tables[self.staging[usize::from(market)]];
        (staged..table.len()).take(rows).map(|index| tier_row(market, table, index)).collect()
    }

    /// Whatever the market still needs to open: the rest of a tier table, then a first mark.
    fn open_market(&mut self, market: MarketId, state: &EngineSnapshot) -> Vec<Command> {
        let Some(snapshot) = market_of(state, market) else { return Vec::new() };
        let mut commands = Vec::new();
        if snapshot.tiers.is_empty() {
            commands = self.tier_rows(market, 0, 8, state);
        }
        if snapshot.mark.is_none() {
            commands.push(set_mark(market, spec(market).initial_mark));
        }
        commands
    }
}

/// The twin step (module docs, "Accounts"). The twins are topped up to the same free
/// balance and set to the same leverage, if they differ from it. Then the counterparty rests
/// twice the size at a price strictly inside the spread, so its order is the only one there,
/// and each twin takes half of it with an IOC. If the counterparty's order was rejected, both
/// IOCs find nothing and leave nothing resting, so the twins stay identical either way.
fn resolve_twin(step: &TwinStep, state: &EngineSnapshot) -> Vec<Command> {
    let market = MarketView::new(state, step.market);
    let leverage = step.leverage.resolve(market.spec.params.max_leverage);
    let mut commands = Vec::new();
    for twin in TWINS {
        let free = free_of(state, twin);
        if free < step.deposit {
            commands.push(deposit(twin, step.deposit - free));
        }
        if market.slot(twin).leverage != leverage {
            commands.push(set_leverage(twin, step.market, leverage));
        }
    }
    let low = market.best_bid().map_or(market.lower(), |bid| bid + 1).max(market.lower());
    let high = market.best_ask().map_or(market.upper(), |ask| ask - 1).min(market.upper());
    if !market.has_mark() || low > high {
        return commands;
    }
    // The end of the spread that is worse for the twins, so their keys come near the mark.
    let price = match step.side {
        Side::Buy => high,
        Side::Sell => low,
    };
    let seq = next_seq_of(state, step.counterparty);
    let resting = place(
        step.counterparty,
        seq,
        step.market,
        step.side.opposite(),
        price,
        2 * step.qty,
        TimeInForce::Gtc,
    );
    commands.push(resting);
    for twin in TWINS {
        let seq = next_seq_of(state, twin);
        commands.push(place(twin, seq, step.market, step.side, price, step.qty, TimeInForce::Ioc));
    }
    commands
}

/// The close step: the account's order against its position at the band's edge through the
/// mark (a sell at `lower` for a long, a buy at `upper` for a short), and the counterparty's
/// order for the same size at the same price. If the account takes, the counterparty's order
/// rests first (GTC) and the account's follows with the step's time in force; otherwise the
/// account's rests first and the counterparty's IOC takes it. A flat account, or one that is
/// its own counterparty, just sends its order.
fn resolve_close(step: &CloseStep, state: &EngineSnapshot) -> Vec<Command> {
    let market = MarketView::new(state, step.market);
    let holders: Vec<AccountId> = TRADERS.into_iter().filter(|&t| market.slot(t).pos != 0).collect();
    let account = if holders.is_empty() { TRADERS[0] } else { holders[step.account.index(holders.len())] };
    let pos = market.slot(account).pos;
    let side = if pos > 0 { Side::Sell } else { Side::Buy };
    let price = PriceChoice::AggressiveEdge(0).resolve(side, &market);
    let qty = match step.size {
        CloseSize::All => pos.abs(),
        CloseSize::Twice => 2 * pos.abs(),
        CloseSize::OneLot => 1,
    };
    let seq = next_seq_of(state, account);
    let (other, other_seq) = (step.counterparty, next_seq_of(state, step.counterparty));
    let (market_id, qty) = (step.market, qty.max(1));
    if pos == 0 || other == account {
        return vec![place(account, seq, market_id, side, price, qty, step.tif)];
    }
    let counterparty = |tif| place(other, other_seq, market_id, side.opposite(), price, qty, tif);
    if step.account_takes {
        vec![counterparty(TimeInForce::Gtc), place(account, seq, market_id, side, price, qty, step.tif)]
    } else {
        vec![
            place(account, seq, market_id, side, price, qty, TimeInForce::Gtc),
            counterparty(TimeInForce::Ioc),
        ]
    }
}

fn bad_tier_row(market: MarketId, kind: BadTierRow, state: &EngineSnapshot) -> Command {
    let max_leverage = spec(market).params.max_leverage;
    let (staged, count, staged_rows) = market_of(state, market)
        .map_or((0, 8, Vec::new()), |m| (m.staged_rows, m.staged_count, m.staged.clone()));
    let row = |index: u8, count: u8, lower_bound: Micros, max_leverage: u16| {
        Command::SetRiskTier(SetRiskTier { lower_bound, market, max_leverage, index, count })
    };
    match kind {
        BadTierRow::SkipsAhead if staged == 0 => row(1, 8, 1, 1),
        BadTierRow::SkipsAhead => row(staged + 1, count, Micros::MAX, 1),
        BadTierRow::LeverageAboveMax => row(0, 1, 0, max_leverage + 1),
        BadTierRow::TooManyRows => row(0, 9, 0, max_leverage),
        BadTierRow::BoundNotRising if staged > 0 => {
            let previous = staged_rows[usize::from(staged) - 1];
            row(staged, count, previous.lower_bound, previous.max_leverage)
        }
        BadTierRow::FirstBoundNotZero | BadTierRow::BoundNotRising => row(0, 1, 1, max_leverage),
    }
}

fn market_params(market: MarketId, bad: Option<BadParams>) -> SetMarketParams {
    let MarketSpec { params, rule_2_floor, .. } = spec(market);
    match bad {
        None => params,
        Some(BadParams::BandTooWide) => {
            SetMarketParams { price_band_ppm: params.price_band_ppm + 1, ..params }
        }
        Some(BadParams::MinPriceBelowRule2) => SetMarketParams { min_price: rule_2_floor - 1, ..params },
        Some(BadParams::RangeTooWide) => {
            SetMarketParams { max_price: params.min_price + (1 << 24), ..params }
        }
        Some(BadParams::NoLeverage) => SetMarketParams { max_leverage: 0, ..params },
        Some(BadParams::NegativeTakerFee) => SetMarketParams { taker_fee_ppm: -1, ..params },
    }
}
