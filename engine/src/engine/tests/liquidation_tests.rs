//! Unit tests of liquidation, the insurance fund and the band sweep: `docs/RISK.md` section
//! 13's T1, T4 and T6, and its "further unit tests" of these (H1's sequence, sweep order,
//! F3's residual risk, fund netting, the shortfall report, the flat negative-collateral
//! slot), plus the walk's order and tie rule, the walk coming before the sweep, and a
//! market emptied by netting. Same harness as `tests.rs`: all four engine modes, compared
//! after every command, so each scenario also checks the index walk against the naive scan.
//! Every number was checked on the spec's integer model.

use super::*;
use crate::event::{InsuranceAbsorb, InsuranceShortfall, Liquidation};

// ---------------------------------------------------------------------------------------
// Events, all in market 1.

/// A mark of `price` ticks.
fn mark_price(price: i64) -> Event {
    Event::MarkPrice(MarkPrice { price: px(price), market: MARKET })
}

/// The liquidation of a slot of `position` lots.
fn liquidation(account: AccountId, position: i64) -> Event {
    Event::Liquidation(Liquidation { position: lots(position), account, market: MARKET })
}

/// The fund takes on `position` lots, with `cost_basis` and `collateral` in micros.
fn absorb(position: i64, cost_basis: i64, collateral: i64) -> Event {
    Event::InsuranceAbsorb(InsuranceAbsorb {
        position: lots(position),
        cost_basis: micros(cost_basis),
        collateral: micros(collateral),
        market: MARKET,
    })
}

/// The insurance fund's position: it holds no collateral in a slot, so `locked` is 0.
fn fund_holds(fund_pos: i64, fund_cost: i64) -> Event {
    position(FUND, fund_pos, fund_cost, 0)
}

/// An uncovered bad debt of `uncovered` micros.
fn shortfall(uncovered: i64) -> Event {
    Event::InsuranceShortfall(InsuranceShortfall { uncovered: micros(uncovered) })
}

/// A liquidation's events after its cancels (RISK.md 11): the slot `(pos, cost, locked)` is
/// taken over, which leaves the fund holding `fund` with a balance of `fund_balance`. Lots
/// and micros, as bare numbers.
fn takeover(
    account: AccountId,
    (pos, cost, locked): (i64, i64, i64),
    (fund_pos, fund_cost): (i64, i64),
    fund_balance: i64,
) -> [Event; 5] {
    [
        liquidation(account, pos),
        position(account, 0, 0, 0),
        absorb(pos, cost, locked),
        fund_holds(fund_pos, fund_cost),
        balance(FUND, fund_balance),
    ]
}

/// An entry of the liquidation index: a key and the account filed under it.
type IndexEntry = (Price, AccountId);

impl Harness {
    /// The production engine's liquidation index in market 1: its first long and its first
    /// short.
    fn first_keys(&self) -> (Option<IndexEntry>, Option<IndexEntry>) {
        let index = &self.fast.market(MARKET).index;
        (index.first_long(), index.first_short())
    }

    /// True if the account has no resting order in market 1.
    fn has_no_orders(&self, account: AccountId) -> bool {
        let book = self.market_1().book;
        book.bids.iter().chain(&book.asks).all(|order| account_of(order.order_id) != account)
    }
}

// ---------------------------------------------------------------------------------------
// T1, T4, T6.

#[test]
fn t1_the_sp500_long_is_liquidated_at_7310_0_and_not_at_7310_1() {
    // INFO.md's example on a 20x market (RISK.md 9.2, C26): a 20x long of one unit at 7,502.4.
    let mut h = market(params(200_000, 0, 0, 20_000, 20));
    h.accept_all([deposit(FUND, 1_000_000_000), deposit(A, 1_000_000_000), deposit(B, 1_000_000_000)]);
    h.accept_all([leverage(A, 20), leverage(B, 20), mark(75_024)]);
    assert_eq!(
        h.accept(sell(B, 1, 75_024, 100_000)),
        vec![ack(B, 1), balance(B, 624_880_000), position(B, 0, 0, 375_120_000)]
    );
    assert_eq!(
        h.accept(buy(A, 1, 75_024, 100_000)),
        vec![
            ack(A, 1),
            balance(A, 624_880_000),
            position(A, 0, 0, 375_120_000),
            fill((B, 1), (A, 1), 75_024, 100_000, (0, 0), Side::Buy),
            position(B, -100_000, -7_502_400_000, 375_120_000),
            position(A, 100_000, 7_502_400_000, 375_120_000),
        ]
    );
    // A's key is 73,100 (7,310.0); B's is 76,854 (7,685.4).
    assert_eq!(h.first_keys(), (Some((px(73_100), A)), Some((px(76_854), B))));

    // At 7,310.1 A's equity 182,820,000 is still at least MM 182,752,500.
    assert_eq!(h.accept(mark(73_101)), vec![mark_price(73_101)]);
    // At 7,310.0 it is 182,720,000, below MM 182,750,000: the fund takes A over.
    assert_eq!(
        h.accept(mark(73_100)),
        vec![
            mark_price(73_100),
            liquidation(A, 100_000),
            position(A, 0, 0, 0),
            absorb(100_000, 7_502_400_000, 375_120_000),
            fund_holds(100_000, 7_502_400_000),
            balance(FUND, 1_375_120_000),
        ]
    );
    // The fund gained A's remaining equity of 182,720,000; no shortfall.
    assert_eq!(h.fund_equity(), 1_182_720_000);
    assert_eq!(h.first_keys(), (None, Some((px(76_854), B))));
}

#[test]
fn t4_a_taker_whose_flip_lands_it_between_zero_and_mm_is_liquidated_after_matching() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(FUND, 1_000_000_000), deposit(A, 5_500_000)]);
    h.accept_all([deposit(B, 1_000_000_000), deposit(C, 1_000_000_000), leverage(A, 20), mark(100_000)]);
    // 1. A is long 1,000 at 100,000 with IM(1,000) locked, and 500,000 left free.
    h.accept_all([sell(B, 1, 100_000, 1_000), buy(A, 1, 100_000, 1_000)]);
    let a = h.slot(A);
    assert_eq!(
        (a.pos, a.cost, a.locked, h.free(A)),
        (lots(1_000), micros(100_000_000), micros(5_000_000), micros(500_000))
    );
    // 2. A rests a buy of 100: W' = 1,100, a top-up of the last 500,000.
    assert_eq!(
        h.accept(buy(A, 2, 97_000, 100)),
        vec![ack(A, 2), balance(A, 0), position(A, 1_000, 100_000_000, 5_500_000)]
    );
    // 3. C bids at the band's lower edge.
    h.accept(buy(C, 1, 98_000, 2_000));
    // 4. A flips there with an IOC. Not strictly reducing, but W' = max(1,100, 1,000) needs
    //    no top-up. After the fill A is short 1,000 at 98,000 with E = 1,500,000 < MM =
    //    2,500,000, so the pass liquidates it, cancelling its resting buy first. C, visited
    //    next, is not released: min(locked, E) is exactly its IM.
    let ioc = PlaceOrder { tif: TimeInForce::Ioc, ..place(A, 3, Side::Sell, 98_000, 2_000) };
    assert_eq!(
        h.accept(Command::PlaceOrder(ioc)),
        vec![
            ack(A, 3),
            fill((C, 1), (A, 3), 98_000, 2_000, (0, 0), Side::Sell),
            position(C, 2_000, 196_000_000, 200_000_000),
            position(A, -1_000, -98_000_000, 3_500_000),
            cancelled(A, 2, 100, Side::Buy, CancelReason::Liquidation),
            liquidation(A, -1_000),
            position(A, 0, 0, 0),
            absorb(-1_000, -98_000_000, 3_500_000),
            fund_holds(-1_000, -98_000_000),
            balance(FUND, 1_003_500_000),
        ]
    );
    // A has no orders left, and the fund gained A's non-negative equity of 1,500,000.
    assert!(h.has_no_orders(A));
    assert_eq!(h.fund_equity(), 1_001_500_000);
}

#[test]
fn t6_a_stale_bid_is_swept_when_the_mark_moves_away_from_it() {
    // Review F1: without the sweep, A's bid would still rest at 100,000 after the mark
    // drifted to 80,000, and a colluder selling into it would cost the fund 15,000,000.
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(FUND, 1_000_000_000), deposit(A, 5_000_000), deposit(B, 1_000_000_000)]);
    h.accept_all([leverage(A, 20), mark(100_000)]);
    // 1. and 2. One IM backs both sides: W' = max(1,000, 1,000).
    assert_eq!(
        h.accept(buy(A, 1, 100_000, 1_000)),
        vec![ack(A, 1), balance(A, 0), position(A, 0, 0, 5_000_000)]
    );
    assert_eq!(h.accept(sell(A, 2, 100_001, 1_000)), vec![ack(A, 2)]);
    // 3. At 98,040 the upper edge is exactly 100,000, so the bid stays.
    assert_eq!(h.accept(mark(98_040)), vec![mark_price(98_040)]);
    // 4. At 98,039 it is 99,999: the bid is swept. The ask (above the lower edge 96,079)
    //    stays, so W is still 1,000 and the release is 5,000,000 − IM(1,000) = 98,050.
    assert_eq!(
        h.accept(mark(98_039)),
        vec![
            mark_price(98_039),
            cancelled(A, 1, 1_000, Side::Buy, CancelReason::PriceBand),
            position(A, 0, 0, 4_901_950),
            balance(A, 98_050),
        ]
    );
    // 5. B's sell at 100,000 finds nothing to fill, and rests.
    assert_eq!(
        h.accept(sell(B, 1, 100_000, 1_000)),
        vec![ack(B, 1), balance(B, 901_961_000), position(B, 0, 0, 98_039_000)]
    );
    assert_eq!(h.fund_equity(), 1_000_000_000);
}

// ---------------------------------------------------------------------------------------
// The band sweep (RISK.md 5.4).

#[test]
fn h1_a_stale_reducing_sell_is_swept_when_the_mark_comes_back() {
    // Review H1 (20x, fees 125/400, band 22,100). Without the sweep, the fund would lose
    // 7,000,002 at the end, at an unchanged mark.
    let mut h = market(params(1_000_000, 125, 400, 22_100, 20));
    h.accept_all([deposit(FUND, 1_000_000_000), deposit(A, 20_100_000)]);
    h.accept_all([deposit(B, 1_000_000_000), deposit(C, 1_000_000_000), leverage(A, 20), mark(100_000)]);
    h.accept_all([sell(B, 1, 100_000, 1_000), buy(A, 1, 100_000, 1_000)]);
    assert_eq!(
        h.accept(buy(A, 2, 80_000, 3_000)),
        vec![ack(A, 2), balance(A, 60_000), position(A, 1_000, 100_000_000, 20_000_000)]
    );
    // At 90,000 the band is 88,011 to 91,989. A rests a strictly reducing sell at its lower
    // edge.
    assert_eq!(h.accept(mark(90_000)), vec![mark_price(90_000)]);
    assert_eq!(h.accept(sell(A, 3, 88_011, 1_000)), vec![ack(A, 3)]);
    // Back at 100,000 the lower edge is 97,790: the sell is swept. Nothing is released,
    // because A's resting buy keeps W at 4,000.
    assert_eq!(
        h.accept(mark(100_000)),
        vec![mark_price(100_000), cancelled(A, 3, 1_000, Side::Sell, CancelReason::PriceBand)]
    );
    // A cancels the buy and gets the 15,000,000 behind it back.
    assert_eq!(
        h.accept(cancel(A, 2)),
        vec![
            cancelled(A, 2, 3_000, Side::Buy, CancelReason::UserRequested),
            position(A, 1_000, 100_000_000, 5_000_000),
            balance(A, 15_060_000),
        ]
    );
    // The colluder's buy finds no stale sell to fill.
    assert_eq!(
        h.accept(buy(C, 1, 100_000, 1_000)),
        vec![ack(C, 1), balance(C, 900_000_000), position(C, 0, 0, 100_000_000)]
    );
    assert_eq!(h.fund_equity(), 1_000_000_000);
}

#[test]
fn the_sweep_cancels_best_price_first_oldest_first_and_releases_each_owner_once() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(A, 1_000_000_000), deposit(B, 1_000_000_000), deposit(C, 1_000_000_000)]);
    h.accept(mark(100_000));
    // Bids of 10 lots at 1x (IM 1,000,000 each): two at 101,500 (B's first), one at 101,000,
    // and C's at 97,000, which no move below will reach.
    h.accept_all([
        buy(A, 1, 101_000, 10),
        buy(B, 1, 101_500, 10),
        buy(A, 2, 101_500, 10),
        buy(C, 1, 97_000, 10),
    ]);
    // The mark falls to 99,000: the upper edge 100,980 is below three bids. Highest price
    // first, oldest first within it; then the pass releases each owner once, in the order
    // of its first swept order.
    assert_eq!(
        h.accept(mark(99_000)),
        vec![
            mark_price(99_000),
            cancelled(B, 1, 10, Side::Buy, CancelReason::PriceBand),
            cancelled(A, 2, 10, Side::Buy, CancelReason::PriceBand),
            cancelled(A, 1, 10, Side::Buy, CancelReason::PriceBand),
            position(B, 0, 0, 0),
            balance(B, 1_000_000_000),
            position(A, 0, 0, 0),
            balance(A, 1_000_000_000),
        ]
    );
    // Asks of 10 lots (IM 990,000 at 99,000): A's at 98,000, then two at 97,500 (B's first).
    // C's ask needs nothing more than its bid's IM, and gets 10,000 back.
    h.accept_all([sell(A, 3, 98_000, 10), sell(B, 2, 97_500, 10)]);
    assert_eq!(
        h.accept(sell(C, 2, 97_500, 10)),
        vec![ack(C, 2), position(C, 0, 0, 990_000), balance(C, 999_010_000)]
    );
    // The mark rises to 101,000: the lower edge 98,980 is above all three asks. Lowest price
    // first, oldest first. C keeps its bid, so W stays 10 and it gets nothing back.
    assert_eq!(
        h.accept(mark(101_000)),
        vec![
            mark_price(101_000),
            cancelled(B, 2, 10, Side::Sell, CancelReason::PriceBand),
            cancelled(C, 2, 10, Side::Sell, CancelReason::PriceBand),
            cancelled(A, 3, 10, Side::Sell, CancelReason::PriceBand),
            position(B, 0, 0, 0),
            balance(B, 1_000_000_000),
            position(A, 0, 0, 0),
            balance(A, 1_000_000_000),
        ]
    );
    assert!(!h.has_no_orders(C));
}

// ---------------------------------------------------------------------------------------
// The SetMark walk (RISK.md 9.4, 14.2).

/// Accounts D (20x), B (10x) and A (20x) each open 1,000 lots at 100,000 on `side` against
/// C (1x). Their slots are created in the order D, B, A, and C's last, so the naive scan
/// meets them in an order that is neither the walk's nor the account ids'.
fn three_positions(side: Side) -> Harness {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(FUND, 1_000_000_000), deposit(A, 1_000_000_000), deposit(B, 1_000_000_000)]);
    h.accept_all([deposit(C, 1_000_000_000), deposit(D, 1_000_000_000)]);
    h.accept_all([leverage(D, 20), leverage(B, 10), leverage(A, 20), mark(100_000)]);
    h.accept(Command::PlaceOrder(place(C, 1, side.opposite(), 100_000, 3_000)));
    for account in [D, B, A] {
        h.accept(Command::PlaceOrder(place(account, 1, side, 100_000, 1_000)));
    }
    assert_eq!(h.market_1().accounts, vec![D, B, A, C]);
    h
}

#[test]
fn the_walk_takes_longs_highest_key_first_and_ties_go_to_the_lower_account() {
    let mut h = three_positions(Side::Buy);
    // A and D (20x) share the key 97,435; B (10x, twice the collateral) has 92,307.
    assert_eq!(h.first_keys().0, Some((px(97_435), A)));
    assert_eq!(h.accept(mark(97_436)), vec![mark_price(97_436)]);
    // A gap to 90,000 crosses all three: A before D on the tie, then B. A and D are past
    // their bankruptcy price of 95,000 (equity −5,000,000 each), B is exactly at its own.
    let mut expected = vec![mark_price(90_000)];
    expected.extend(takeover(A, (1_000, 100_000_000, 5_000_000), (1_000, 100_000_000), 1_005_000_000));
    expected.extend(takeover(D, (1_000, 100_000_000, 5_000_000), (2_000, 200_000_000), 1_010_000_000));
    expected.extend(takeover(B, (1_000, 100_000_000, 10_000_000), (3_000, 300_000_000), 1_020_000_000));
    assert_eq!(h.accept(mark(90_000)), expected);
    // Their free balances are untouched; the fund lost the 10,000,000 of bad debt.
    assert_eq!(
        (h.free(A), h.free(B), h.free(D)),
        (micros(995_000_000), micros(990_000_000), micros(995_000_000))
    );
    assert_eq!(h.fund_equity(), 990_000_000);
    // Only C's short is left in the index.
    assert_eq!(h.first_keys(), (None, Some((px(195_122), C))));
}

#[test]
fn the_walk_takes_shorts_lowest_key_first_and_ties_go_to_the_lower_account() {
    let mut h = three_positions(Side::Sell);
    // A and D (20x) share the key 102,440; B (10x) has 107,318.
    assert_eq!(h.first_keys().1, Some((px(102_440), A)));
    assert_eq!(h.accept(mark(102_439)), vec![mark_price(102_439)]);
    let mut expected = vec![mark_price(110_000)];
    expected.extend(takeover(A, (-1_000, -100_000_000, 5_000_000), (-1_000, -100_000_000), 1_005_000_000));
    expected.extend(takeover(D, (-1_000, -100_000_000, 5_000_000), (-2_000, -200_000_000), 1_010_000_000));
    expected.extend(takeover(B, (-1_000, -100_000_000, 10_000_000), (-3_000, -300_000_000), 1_020_000_000));
    assert_eq!(h.accept(mark(110_000)), expected);
    assert_eq!(h.fund_equity(), 990_000_000);
}

#[test]
fn the_walk_comes_before_the_sweep_so_a_liquidated_accounts_orders_go_as_liquidation() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(FUND, 1_000_000_000), deposit(A, 1_000_000_000), deposit(B, 1_000_000_000)]);
    h.accept_all([leverage(A, 20), mark(100_000), sell(B, 1, 100_000, 1_000), buy(A, 1, 100_000, 1_000)]);
    // A also bids above the mark: W' = 1,100, a top-up to 5,500,000, and a key of 96,923.
    assert_eq!(
        h.accept(buy(A, 2, 101_500, 100)),
        vec![ack(A, 2), balance(A, 994_500_000), position(A, 1_000, 100_000_000, 5_500_000)]
    );
    // At 96,000 the bid is out of the band (upper edge 97,920), but A is crossed too. The walk
    // goes first: the bid is cancelled by the liquidation, with nothing released, and the
    // sweep finds nothing left to cancel.
    assert_eq!(
        h.accept(mark(96_000)),
        vec![
            mark_price(96_000),
            cancelled(A, 2, 100, Side::Buy, CancelReason::Liquidation),
            liquidation(A, 1_000),
            position(A, 0, 0, 0),
            absorb(1_000, 100_000_000, 5_500_000),
            fund_holds(1_000, 100_000_000),
            balance(FUND, 1_005_500_000),
        ]
    );
    assert_eq!(h.fund_equity(), 1_001_500_000);
}

// ---------------------------------------------------------------------------------------
// The insurance fund (RISK.md 10).

/// RISK.md 10.2's setup: A (20x) buys `long_lots` at 100,000 from B and is liquidated at its
/// key 97,435, so the fund is long `long_lots` at an average of 100,000. Then C (20x) sells
/// `short_lots` at `short_price` (in ticks) to D, with `IM` at 97,435 locked.
fn fund_long_then_a_short(long_lots: i64, short_lots: i64, short_price: i64) -> Harness {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(FUND, 1_000_000_000), deposit(A, 1_000_000_000), deposit(B, 1_000_000_000)]);
    h.accept_all([deposit(C, 1_000_000_000), deposit(D, 1_000_000_000)]);
    h.accept_all([leverage(A, 20), leverage(C, 20), mark(100_000)]);
    h.accept_all([sell(B, 1, 100_000, long_lots), buy(A, 1, 100_000, long_lots)]);
    h.accept(mark(97_435));
    let market = h.market_1();
    assert_eq!((market.fund_pos, market.fund_cost), (lots(long_lots), lots(long_lots) * px(100_000)));
    h.accept_all([sell(C, 1, short_price, short_lots), buy(D, 1, short_price, short_lots)]);
    h
}

#[test]
fn fund_netting_reduces_the_funds_position_with_the_fills_arithmetic() {
    // RISK.md 10.2, reducing: the fund is long 1,000 at 100,000 and absorbs a short of 400 at
    // 97,500. `removed` = 40,000,000 of its cost basis, so it realizes 39,000,000 − 40,000,000
    // = −1,000,000: it bought at 100,000 and sold at 97,500.
    let mut h = fund_long_then_a_short(1_000, 400, 97_500);
    // C's key is 99,875. The fund's balance gains C's collateral and the realized PnL.
    assert_eq!(h.accept(mark(99_874)), vec![mark_price(99_874)]);
    assert_eq!(
        h.accept(mark(99_875)),
        vec![
            mark_price(99_875),
            liquidation(C, -400),
            position(C, 0, 0, 0),
            absorb(-400, -39_000_000, 1_948_700),
            fund_holds(600, 60_000_000),
            balance(FUND, 1_005_000_000 + 1_948_700 - 1_000_000),
        ]
    );
}

#[test]
fn fund_netting_flips_the_funds_position_with_the_fills_arithmetic() {
    // RISK.md 10.2, flipping: the fund is long 300 at 100,000 and absorbs a short of 500 at
    // 98,000. It closes its 300 at 98,000 (D_close = −29,400,000, realizing −600,000) and is
    // left short 200 at 98,000.
    let mut h = fund_long_then_a_short(300, 500, 98_000);
    assert_eq!(h.accept(mark(100_362)), vec![mark_price(100_362)]);
    assert_eq!(
        h.accept(mark(100_363)),
        vec![
            mark_price(100_363),
            liquidation(C, -500),
            position(C, 0, 0, 0),
            absorb(-500, -49_000_000, 2_435_875),
            fund_holds(-200, -19_600_000),
            balance(FUND, 1_001_500_000 + 2_435_875 - 600_000),
        ]
    );
}

#[test]
fn the_shortfall_is_reported_once_per_command_when_it_rises_changes_and_returns_to_zero() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(FUND, 1_000_000), deposit(A, 1_000_000_000), deposit(B, 1_000_000_000)]);
    h.accept_all([deposit(D, 1_000_000_000), leverage(D, 20), leverage(A, 20), mark(100_000)]);
    // Twins A and D, each long 1,000 at 100,000 at 20x: bankrupt at 95,000.
    h.accept_all([sell(B, 1, 100_000, 2_000), buy(D, 1, 100_000, 1_000), buy(A, 1, 100_000, 1_000)]);
    // A gap to 90,000 liquidates both at −5,000,000. The fund's equity becomes
    // 11,000,000 + (180,000,000 − 200,000,000) = −9,000,000: one report, after both.
    let mut expected = vec![mark_price(90_000)];
    expected.extend(takeover(A, (1_000, 100_000_000, 5_000_000), (1_000, 100_000_000), 6_000_000));
    expected.extend(takeover(D, (1_000, 100_000_000, 5_000_000), (2_000, 200_000_000), 11_000_000));
    expected.push(shortfall(9_000_000));
    assert_eq!(h.accept(mark(90_000)), expected);
    // The fund's position loses 2,000,000 more; the same mark again changes nothing.
    assert_eq!(h.accept(mark(89_000)), vec![mark_price(89_000), shortfall(11_000_000)]);
    assert_eq!(h.accept(mark(89_000)), vec![mark_price(89_000)]);
    // Deposits to the fund cover it, in part, then in full.
    assert_eq!(h.accept(deposit(FUND, 4_000_000)), vec![balance(FUND, 15_000_000), shortfall(7_000_000)]);
    assert_eq!(h.accept(deposit(FUND, 7_000_000)), vec![balance(FUND, 22_000_000), shortfall(0)]);
    // Mark moves alone open and close it again.
    assert_eq!(h.accept(mark(88_000)), vec![mark_price(88_000), shortfall(2_000_000)]);
    assert_eq!(h.accept(mark(95_000)), vec![mark_price(95_000), shortfall(0)]);
    assert_eq!(h.accept(deposit(FUND, 1)), vec![balance(FUND, 22_000_001)]);
}

#[test]
fn a_fund_position_keeps_the_market_non_empty_until_netting_flattens_it() {
    // By I2 the fund can't be the only one with a position: someone holds the other side.
    // Its position still counts, and the market empties only when a second absorb nets it
    // to zero.
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(FUND, 1_000_000_000), deposit(A, 1_000_000_000), deposit(B, 1_000_000_000)]);
    h.accept_all([leverage(A, 20), leverage(B, 20), mark(100_000)]);
    h.accept_all([sell(B, 1, 100_000, 1_000), buy(A, 1, 100_000, 1_000)]);
    assert_eq!(h.first_keys(), (Some((px(97_435), A)), Some((px(102_440), B))));
    h.accept(mark(97_435));
    let market = h.market_1();
    assert_eq!(
        (market.fund_pos, market.nonzero_positions),
        (lots(1_000), 2),
        "B's short and the fund's long"
    );
    let new_params = params(500_000, 0, 0, 10_000, 10);
    h.assert_rejected(market_params(new_params), RejectReason::MarketNotEmpty);
    // B's short is taken over at its key; the fund's long and short cancel out exactly.
    assert_eq!(
        h.accept(mark(102_440)),
        vec![
            mark_price(102_440),
            liquidation(B, -1_000),
            position(B, 0, 0, 0),
            absorb(-1_000, -100_000_000, 5_000_000),
            fund_holds(0, 0),
            balance(FUND, 1_010_000_000),
        ]
    );
    assert_eq!(h.market_1().nonzero_positions, 0);
    h.accept(market_params(new_params));
    assert_eq!(h.fund_equity(), 1_010_000_000);
}

#[test]
fn a_flat_slot_with_negative_collateral_is_liquidated_and_the_fund_absorbs_it() {
    // RISK.md 10.1: commands can't produce this slot (claim 2 of 5.3), so the test sets it
    // directly. The rule exists so that I5 has no exception: its equity, −5, is below its
    // MM of 0.
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(FUND, 1_000_000_000), deposit(A, 10_000_000), leverage(A, 20), mark(100_000)]);
    assert_eq!(
        h.accept(buy(A, 1, 99_000, 100)),
        vec![ack(A, 1), balance(A, 9_500_000), position(A, 0, 0, 500_000)]
    );
    h.set_slot_for_test(A, 0, 0, -5);
    // Any command that visits the slot in its pass liquidates it: here A's cancel.
    assert_eq!(
        h.accept(cancel(A, 1)),
        vec![
            cancelled(A, 1, 100, Side::Buy, CancelReason::UserRequested),
            liquidation(A, 0),
            position(A, 0, 0, 0),
            absorb(0, 0, -5),
            fund_holds(0, 0),
            balance(FUND, 999_999_995),
        ]
    );
    // The account's free balance is untouched.
    assert_eq!(h.free(A), micros(9_500_000));
}

// ---------------------------------------------------------------------------------------
// The residual risk (RISK.md 5.3, claim 3).

#[test]
fn f3_orders_that_grow_a_position_can_still_cost_the_fund_after_a_mark_move() {
    // Review F3, on SP500's live parameters (50x, band 8,600, fees 125/400): the accepted
    // residual risk, pinned. If a later milestone adds the margin-call index, this test
    // changes on purpose.
    let mut h = market(sp500_params());
    h.accept_all([deposit(FUND, 1_000_000_000_000), deposit(A, 1_000_000_000_000)]);
    h.accept_all([deposit(B, 100_000_000_000_000), deposit(C, 100_000_000_000_000)]);
    h.accept_all([leverage(A, 50), mark(75_024), sell(C, 1, 75_024, 100_000), buy(A, 1, 75_024, 100_000)]);
    // A, long 100,000 at 75,024, rests a bid of 500,000 at 67,261: a top-up to IM(600,000).
    assert_eq!(
        h.accept(buy(A, 2, 67_261, 500_000)),
        vec![ack(A, 2), balance(A, 999_096_711_040), position(A, 100_000, 7_502_400_000, 900_288_000)]
    );
    // The mark falls 11.1% in steps of 700, to 66,688, where A's equity is exactly its MM
    // (66,688,000) and the bid is still inside the band (upper edge 67,261).
    let mut price = 75_024;
    while price > 66_688 {
        price = (price - 700).max(66_688);
        assert_eq!(h.accept(mark(price)), vec![mark_price(price)]);
    }
    // A colluder sells into the bid. A ends below zero, and the fund pays the difference.
    let fund_before = h.fund_equity();
    assert_eq!(
        h.accept(sell(B, 1, 67_261, 500_000)),
        vec![
            ack(B, 1),
            balance(B, 99_966_656_000_000),
            position(B, 0, 0, 33_344_000_000),
            fill((A, 2), (B, 1), 67_261, 500_000, (4_203_813, 13_452_200), Side::Sell),
            position(A, 600_000, 41_132_900_000, 896_084_187),
            position(B, -500_000, -33_630_500_000, 33_330_547_800),
            liquidation(A, 600_000),
            position(A, 0, 0, 0),
            absorb(600_000, 41_132_900_000, 896_084_187),
            fund_holds(600_000, 41_132_900_000),
            balance(FUND, 1_000_896_084_187),
        ]
    );
    // 0.67% of the order's notional.
    assert_eq!(fund_before - h.fund_equity(), 224_015_813);
    assert_eq!(h.fees_collected(), micros(21_594_773));
}
