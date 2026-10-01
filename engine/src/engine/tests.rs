//! Unit tests of the engine: `docs/RISK.md` section 13. This file has the harness, T2, T3
//! and T5, and the "further unit tests" of trading and margin; `liquidation_tests.rs` has
//! T1, T4, T6 and the tests of liquidation, the insurance fund and the band sweep. Where
//! RISK.md lists a command's events, the test asserts all of them, in order. Every number
//! was worked out by hand and checked on the spec's integer model.
//!
//! **Four engines at once.** Every test drives a [`Harness`], which applies each command to
//! `Engine<Book, Fast>` (production), `Engine<ReferenceBook, Naive>` (the reference) and
//! the two ablations, and after every command checks that all four emitted the same events
//! and hold the same snapshot, that `assert_invariants` passes on each, and that a rejected
//! command left the snapshot exactly as it was (I15). So each scenario also checks the Mode
//! seam: running totals against the book, the index walk against the naive scan. It also
//! checks the command properties of RISK.md 12 (I12, I13, I14, I17 to I20) with formulas of
//! its own (`command_properties.rs`).

use super::*;
use crate::book::{Book, OrderBook};
use crate::command::{
    CancelOrder, Deposit, ModifyOrder, PlaceOrder, SetLeverage, SetMark, SetMarketParams, SetRiskTier,
    Withdraw,
};
use crate::event::{Ack, CancelReason, Cancelled, Fill, LeverageSet, MarkPrice, Modified};
use crate::mode::{Fast, Naive, NaiveLiquidation, NaiveTotals};
use crate::money::{Tier, max_qty};
use crate::reference::ReferenceBook;
use crate::types::{Price, Qty, Side, TimeInForce, account_of, order_id};

mod command_properties;
mod liquidation_tests;

const MARKET: MarketId = 1;
const A: AccountId = 1;
const B: AccountId = 2;
const C: AccountId = 3;
const D: AccountId = 4;

// ---------------------------------------------------------------------------------------
// The harness.

/// The same commands on four engines, compared after every command. See the module docs.
struct Harness {
    fast: Engine<Book, Fast>,
    naive: Engine<ReferenceBook, Naive>,
    naive_totals: Engine<Book, NaiveTotals>,
    naive_liquidation: Engine<Book, NaiveLiquidation>,
}

/// Applies one command to one engine and returns its events.
fn run<B: OrderBook, M: Mode>(engine: &mut Engine<B, M>, command: &Command) -> Vec<Event> {
    let mut events = Vec::new();
    engine.apply(command, &mut events);
    events
}

impl Harness {
    fn new() -> Self {
        Self::with_options(EngineOptions::default())
    }

    fn with_options(options: EngineOptions) -> Self {
        Harness {
            fast: Engine::new(options),
            naive: Engine::new(options),
            naive_totals: Engine::new(options),
            naive_liquidation: Engine::new(options),
        }
    }

    /// Applies `command` to all four engines, checks that they agree (module docs), and
    /// returns the events.
    fn apply(&mut self, command: Command) -> Vec<Event> {
        let before = self.fast.snapshot();
        let events = run(&mut self.fast, &command);
        assert_eq!(run(&mut self.naive, &command), events, "Naive differs on {command:?}");
        assert_eq!(run(&mut self.naive_totals, &command), events, "NaiveTotals differs on {command:?}");
        assert_eq!(
            run(&mut self.naive_liquidation, &command),
            events,
            "NaiveLiquidation differs on {command:?}"
        );

        let after = self.fast.snapshot();
        assert_eq!(self.naive.snapshot(), after, "Naive's state differs after {command:?}");
        assert_eq!(self.naive_totals.snapshot(), after, "NaiveTotals' state differs after {command:?}");
        assert_eq!(
            self.naive_liquidation.snapshot(),
            after,
            "NaiveLiquidation's state differs after {command:?}"
        );
        self.fast.assert_invariants();
        self.naive.assert_invariants();
        self.naive_totals.assert_invariants();
        self.naive_liquidation.assert_invariants();
        if let [Event::Reject(_)] = events.as_slice() {
            assert_eq!(after, before, "I15: the rejected {command:?} changed the state");
        }
        command_properties::check(&before, &command, &events, &after);
        events
    }

    /// Sets one slot directly in all four engines (`Engine::set_slot_for_test`), for a state
    /// that commands can't reach.
    fn set_slot_for_test(&mut self, account: AccountId, pos: Qty, cost: Micros, locked: Micros) {
        self.fast.set_slot_for_test(MARKET, account, pos, cost, locked);
        self.naive.set_slot_for_test(MARKET, account, pos, cost, locked);
        self.naive_totals.set_slot_for_test(MARKET, account, pos, cost, locked);
        self.naive_liquidation.set_slot_for_test(MARKET, account, pos, cost, locked);
    }

    /// Applies a command that must be accepted, and returns its events.
    fn accept(&mut self, command: Command) -> Vec<Event> {
        let events = self.apply(command);
        assert!(!matches!(events.first(), Some(Event::Reject(_))), "{command:?} was rejected: {events:?}");
        events
    }

    fn accept_all(&mut self, commands: impl IntoIterator<Item = Command>) {
        for command in commands {
            self.accept(command);
        }
    }

    /// Applies a command that must be rejected for `reason`: exactly one `Reject`, with the
    /// order id and account RISK.md 6 gives it.
    fn assert_rejected(&mut self, command: Command, reason: RejectReason) {
        assert_eq!(self.apply(command), vec![reject_of(&command, reason)], "{command:?}");
    }

    fn snapshot(&self) -> EngineSnapshot {
        self.fast.snapshot()
    }

    fn slot(&self, account: AccountId) -> SlotSnapshot {
        let snapshot = self.snapshot();
        let market = snapshot.markets.iter().find(|m| m.params.market == MARKET).expect("market 1 exists");
        *market.slots.iter().find(|s| s.account == account).expect("the account has a slot")
    }

    fn free(&self, account: AccountId) -> Micros {
        self.snapshot().accounts.iter().find(|a| a.account == account).map_or(0, |a| a.free)
    }

    fn fees_collected(&self) -> Micros {
        self.market_1().fees_collected
    }

    fn market_1(&self) -> MarketSnapshot {
        let snapshot = self.snapshot();
        snapshot.markets.into_iter().find(|m| m.params.market == MARKET).expect("market 1 exists")
    }

    /// The fund's equity: its balance plus its unrealized PnL over all markets.
    fn fund_equity(&self) -> i128 {
        let snapshot = self.snapshot();
        i128::from(snapshot.fund_balance) + snapshot.fund_upnl_total
    }
}

/// The `Reject` RISK.md 6 gives a command: the order's id and owner for an order command,
/// order id 0 and the command's account for an account command, and order id 0 and
/// `AccountId::MAX` (the operator) for a market-level command.
fn reject_of(command: &Command, reason: RejectReason) -> Event {
    let (order_id, account) = match *command {
        Command::PlaceOrder(o) => (o.order_id, account_of(o.order_id)),
        Command::CancelOrder(c) => (c.order_id, account_of(c.order_id)),
        Command::ModifyOrder(m) => (m.order_id, account_of(m.order_id)),
        Command::Deposit(d) => (0, d.account),
        Command::Withdraw(w) => (0, w.account),
        Command::SetLeverage(l) => (0, l.account),
        Command::SetMark(_) | Command::SetMarketParams(_) | Command::SetRiskTier(_) => (0, AccountId::MAX),
    };
    Event::Reject(Reject { order_id, account, reason })
}

// ---------------------------------------------------------------------------------------
// Commands. Every test market is market 1 with `min_price` 1,000 (RISK.md 13).

fn params(
    max_price: Price,
    maker_fee_ppm: i32,
    taker_fee_ppm: i32,
    price_band_ppm: u32,
    max_leverage: u16,
) -> SetMarketParams {
    SetMarketParams {
        min_price: 1_000,
        max_price,
        maker_fee_ppm,
        taker_fee_ppm,
        price_band_ppm,
        market: MARKET,
        max_leverage,
    }
}

/// SP500's live parameters (Polymarket, 2026-09-29), with the largest band band rule 1
/// allows at 50x and the `min_price` band rule 2 then needs (RISK.md 5.3).
fn sp500_params() -> SetMarketParams {
    SetMarketParams { min_price: 1_004, ..params(150_000, 125, 400, 8_600, 50) }
}

/// SP500's live tier table, in micros.
const SP500_TIERS: [(Micros, u16); 8] = [
    (0, 50),
    (500_000_000_000, 25),
    (1_000_000_000_000, 20),
    (5_000_000_000_000, 15),
    (10_000_000_000_000, 10),
    (25_000_000_000_000, 5),
    (50_000_000_000_000, 2),
    (100_000_000_000_000, 1),
];

fn market_params(params: SetMarketParams) -> Command {
    Command::SetMarketParams(params)
}

fn tier_row(index: u8, count: u8, lower_bound: Micros, max_leverage: u16) -> Command {
    Command::SetRiskTier(SetRiskTier { lower_bound, market: MARKET, max_leverage, index, count })
}

/// A one-row tier table at the market's maximum leverage, as every test market has.
fn one_tier(max_leverage: u16) -> Command {
    tier_row(0, 1, 0, max_leverage)
}

/// Row `index` of SP500's 8-row table.
fn sp500_tier_row(index: u8) -> SetRiskTier {
    let (lower_bound, max_leverage) = SP500_TIERS[usize::from(index)];
    SetRiskTier { lower_bound, market: MARKET, max_leverage, index, count: 8 }
}

/// The 8 rows of SP500's table.
fn sp500_tier_rows() -> Vec<Command> {
    (0..8).map(|index| Command::SetRiskTier(sp500_tier_row(index))).collect()
}

fn deposit(account: AccountId, amount: Micros) -> Command {
    Command::Deposit(Deposit { amount, account })
}

fn withdraw(account: AccountId, amount: Micros) -> Command {
    Command::Withdraw(Withdraw { amount, account })
}

fn leverage(account: AccountId, leverage: u16) -> Command {
    Command::SetLeverage(SetLeverage { account, market: MARKET, leverage })
}

fn mark(price: Price) -> Command {
    Command::SetMark(SetMark { price, market: MARKET })
}

/// A GTC limit order `account#seq` in market 1.
fn place(account: AccountId, seq: u32, side: Side, price: Price, qty: Qty) -> PlaceOrder {
    PlaceOrder {
        order_id: order_id(account, seq),
        price,
        qty,
        market: MARKET,
        side,
        tif: TimeInForce::Gtc,
        post_only: false,
    }
}

fn buy(account: AccountId, seq: u32, price: Price, qty: Qty) -> Command {
    Command::PlaceOrder(place(account, seq, Side::Buy, price, qty))
}

fn sell(account: AccountId, seq: u32, price: Price, qty: Qty) -> Command {
    Command::PlaceOrder(place(account, seq, Side::Sell, price, qty))
}

fn cancel(account: AccountId, seq: u32) -> Command {
    Command::CancelOrder(CancelOrder { order_id: order_id(account, seq), market: MARKET })
}

fn modify(account: AccountId, seq: u32, new_price: Price, new_size: Qty) -> Command {
    Command::ModifyOrder(ModifyOrder {
        order_id: order_id(account, seq),
        new_price,
        new_size,
        market: MARKET,
    })
}

/// A harness with market 1 set up and its one-row tier table (RISK.md 13), but no mark.
fn market(params: SetMarketParams) -> Harness {
    let mut harness = Harness::new();
    harness.accept(market_params(params));
    harness.accept(one_tier(params.max_leverage));
    harness
}

/// RISK.md 13's usual market: `max_price` 1,000,000, a 2% band, 20x, and the given fees.
fn market_20x(maker_fee_ppm: i32, taker_fee_ppm: i32) -> Harness {
    market(params(1_000_000, maker_fee_ppm, taker_fee_ppm, 20_000, 20))
}

// ---------------------------------------------------------------------------------------
// Events, all in market 1.

fn ack(account: AccountId, seq: u32) -> Event {
    Event::Ack(Ack { order_id: order_id(account, seq) })
}

fn balance(account: AccountId, free: Micros) -> Event {
    Event::BalanceChanged(crate::event::BalanceChanged { free, account })
}

fn position(account: AccountId, position: Qty, cost_basis: Micros, locked: Micros) -> Event {
    Event::PositionChanged(PositionChanged { position, cost_basis, locked, account, market: MARKET })
}

/// A fill of `taker` (whose side is `taker_side`) against `maker`, each an `(account, seq)`.
fn fill(
    maker: (AccountId, u32),
    taker: (AccountId, u32),
    price: Price,
    qty: Qty,
    (maker_fee, taker_fee): (Micros, Micros),
    taker_side: Side,
) -> Event {
    Event::Fill(Fill {
        maker_order: order_id(maker.0, maker.1),
        taker_order: order_id(taker.0, taker.1),
        price,
        qty,
        maker_fee,
        taker_fee,
        market: MARKET,
        taker_side,
    })
}

fn cancelled(account: AccountId, seq: u32, remaining: Qty, side: Side, reason: CancelReason) -> Event {
    Event::Cancelled(Cancelled { order_id: order_id(account, seq), remaining, market: MARKET, reason, side })
}

fn modified(account: AccountId, seq: u32, price: Price, qty: Qty) -> Event {
    Event::Modified(Modified { order_id: order_id(account, seq), price, qty, market: MARKET })
}

// ---------------------------------------------------------------------------------------
// T2, T3 and T5.

#[test]
fn t2_the_withdrawal_reserve_is_ten_percent_of_the_open_notional() {
    // "A $100 position at 20x needs $5 initial margin but a $10 withdrawal reserve."
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(A, 20_000_000), deposit(B, 100_000_000), leverage(A, 20), mark(100_000)]);
    // B at 1x: a top-up of all of its 100,000,000.
    assert_eq!(
        h.accept(sell(B, 1, 100_000, 1_000)),
        vec![ack(B, 1), balance(B, 0), position(B, 0, 0, 100_000_000)]
    );
    // A at 20x: a top-up of 5,000,000 ($5), then the fill.
    assert_eq!(
        h.accept(buy(A, 1, 100_000, 1_000)),
        vec![
            ack(A, 1),
            balance(A, 15_000_000),
            position(A, 0, 0, 5_000_000),
            fill((B, 1), (A, 1), 100_000, 1_000, (0, 0), Side::Buy),
            position(B, -1_000, -100_000_000, 100_000_000),
            position(A, 1_000, 100_000_000, 5_000_000),
        ]
    );
    // 4,999,999 + 5,000,000 locked < 10,000,000 = 10% of $100.
    h.assert_rejected(withdraw(A, 10_000_001), RejectReason::WithdrawalReserve);
    assert_eq!(h.accept(withdraw(A, 10_000_000)), vec![balance(A, 5_000_000)]);
    h.assert_rejected(withdraw(A, 1), RejectReason::WithdrawalReserve);
}

#[test]
fn t3_a_margin_called_slot_with_no_free_balance_can_close_and_it_fills() {
    let mut h = market_20x(125, 400);
    h.accept_all([deposit(A, 5_000_000), deposit(B, 1_000_000_000), deposit(C, 1_000_000_000)]);
    h.accept_all([leverage(A, 20), mark(100_000)]);

    // 1. B at 1x: a top-up of 100,000,000.
    assert_eq!(
        h.accept(sell(B, 1, 101_000, 1_000)),
        vec![ack(B, 1), balance(B, 900_000_000), position(B, 0, 0, 100_000_000)]
    );
    // 2. A buys inside the band (upper edge 102,000), with all of its free balance.
    assert_eq!(
        h.accept(buy(A, 1, 101_000, 1_000)),
        vec![
            ack(A, 1),
            balance(A, 0),
            position(A, 0, 0, 5_000_000),
            fill((B, 1), (A, 1), 101_000, 1_000, (12_625, 40_400), Side::Buy),
            position(B, -1_000, -101_000_000, 99_987_375),
            position(A, 1_000, 101_000_000, 4_959_600),
        ]
    );
    // A now has E = 3,959,600, IM = 5,000,000 and MM = 2,500,000: a margin call, free 0.
    // 3. Adding to the position needs 1,090,400 more, which A doesn't have.
    h.assert_rejected(buy(A, 2, 100_000, 10), RejectReason::MarginCall);
    // 4. C at 1x: a top-up of 100,000,000.
    h.accept(buy(C, 1, 99_500, 1_000));
    // 5. Strictly reducing (open sells 1,000 <= pos), so no margin check. A realizes a loss
    //    of 1,500,000 and pays 80,200 of fees, all from `locked`, and gets the rest back.
    assert_eq!(
        h.accept(sell(A, 3, 99_500, 1_000)),
        vec![
            ack(A, 3),
            fill((C, 1), (A, 3), 99_500, 1_000, (12_438, 39_800), Side::Sell),
            position(C, 1_000, 99_500_000, 99_987_562),
            position(A, 0, 0, 3_419_800),
            position(A, 0, 0, 0),
            balance(A, 3_419_800),
        ]
    );
    assert_eq!(h.fees_collected(), 105_263);
}

#[test]
fn t5_a_collusive_flip_is_rejected_and_the_fund_is_unchanged() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(FUND, 1_000_000_000), deposit(A, 5_000_000), deposit(C, 1_000_000_000)]);
    h.accept_all([leverage(A, 20), mark(100_000)]);
    // 1. A buys at the band's upper edge, into margin call: E = 3,000,000 < IM = 5,000,000.
    h.accept(sell(C, 1, 102_000, 1_000));
    h.accept(buy(A, 1, 102_000, 1_000));
    let a = h.slot(A);
    assert_eq!((a.pos, a.cost, a.locked, h.free(A)), (1_000, 102_000_000, 5_000_000, 0));
    // 2. The colluder bids at the lower edge; its short covers the bid, so no top-up.
    assert_eq!(h.accept(buy(C, 2, 98_000, 2_000)), vec![ack(C, 2)]);
    // 3. A tries to flip into it: W' stays 1,000, but the flip is not strictly reducing, so
    //    it needs 2,000,000 more, and A is in margin call.
    let fund_before = h.fund_equity();
    h.assert_rejected(sell(A, 2, 98_000, 2_000), RejectReason::MarginCall);
    assert_eq!((fund_before, h.fund_equity()), (1_000_000_000, 1_000_000_000));
}

// ---------------------------------------------------------------------------------------
// Validation (RISK.md 6).

#[test]
fn a_rejected_order_keeps_its_sequence_and_an_accepted_one_blocks_it_in_every_market() {
    let mut h = market_20x(0, 0);
    let market_2 = SetMarketParams { market: 2, ..params(1_000_000, 0, 0, 20_000, 20) };
    h.accept(market_params(market_2));
    h.accept(Command::SetRiskTier(SetRiskTier {
        lower_bound: 0,
        market: 2,
        max_leverage: 20,
        index: 0,
        count: 1,
    }));
    h.accept(Command::SetMark(SetMark { price: 100_000, market: 2 }));
    h.accept_all([deposit(A, 1_000_000_000), mark(100_000)]);
    let in_market_2 = |command: Command| match command {
        Command::PlaceOrder(order) => Command::PlaceOrder(PlaceOrder { market: 2, ..order }),
        _ => unreachable!(),
    };

    // A#5 is rejected (above the band), so it can be sent again.
    h.assert_rejected(buy(A, 5, 102_001, 1), RejectReason::PriceBand);
    h.accept(buy(A, 5, 99_000, 1));
    assert_eq!(h.snapshot().accounts[0].next_seq, 6);
    // Now 5 and everything below it are used up, in this market and the other.
    h.assert_rejected(buy(A, 5, 99_000, 1), RejectReason::Duplicate);
    h.assert_rejected(in_market_2(buy(A, 5, 99_000, 1)), RejectReason::Duplicate);
    h.assert_rejected(in_market_2(buy(A, 0, 99_000, 1)), RejectReason::Duplicate);
    h.accept(in_market_2(buy(A, 6, 99_000, 1)));
    // The last sequence number still leaves room: next_seq is a u64.
    h.accept(buy(A, u32::MAX, 99_000, 1));
    assert_eq!(h.snapshot().accounts[0].next_seq, 1 << 32);
    h.assert_rejected(in_market_2(buy(A, u32::MAX, 99_000, 1)), RejectReason::Duplicate);
}

#[test]
fn an_order_failing_several_checks_gets_the_first_reason_and_changes_nothing() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(A, 1_000_000_000), deposit(B, 1_000_000_000), mark(100_000)]);
    h.accept(sell(B, 1, 100_500, 10));
    let limit = max_qty(1_000_000);
    let post_only = |command: Command| match command {
        Command::PlaceOrder(order) => Command::PlaceOrder(PlaceOrder { post_only: true, ..order }),
        _ => unreachable!(),
    };

    // Each order fails the check named and every check after it (RISK.md 6.1's order).
    let fund_order = PlaceOrder { market: 9, qty: 0, ..place(FUND, 1, Side::Buy, 0, 0) };
    h.assert_rejected(Command::PlaceOrder(fund_order), RejectReason::UnknownMarket);
    h.assert_rejected(
        Command::PlaceOrder(PlaceOrder { market: MARKET, ..fund_order }),
        RejectReason::ReservedAccount,
    );
    h.assert_rejected(buy(A, 1, 5_000_000, 0), RejectReason::InvalidQty);
    h.accept(buy(A, 5, 99_000, 1));
    h.assert_rejected(post_only(buy(A, 5, 999, 1)), RejectReason::InvalidPrice);
    h.assert_rejected(post_only(buy(A, 5, 100_500, 1)), RejectReason::Duplicate);
    h.assert_rejected(post_only(buy(A, 6, 102_500, limit)), RejectReason::PostOnlyWouldCross);
    h.assert_rejected(buy(A, 6, 102_001, limit), RejectReason::PriceBand);
    h.assert_rejected(sell(A, 6, 97_999, 1), RejectReason::PriceBand);
    // A has 1 lot resting on the bid, so max_qty more is one too many.
    h.assert_rejected(buy(A, 6, 99_000, limit), RejectReason::SizeLimit);
    h.assert_rejected(buy(A, 6, 99_000, limit - 1), RejectReason::InsufficientMargin);

    // A market without a mark: NoMark comes before the band, which needs a mark.
    let mut h = market_20x(0, 0);
    h.accept(deposit(A, 1_000_000_000));
    h.assert_rejected(buy(A, 1, 999_999, 1), RejectReason::NoMark);
}

#[test]
fn a_modify_failing_several_checks_gets_the_first_reason_and_changes_nothing() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(A, 1_000_000_000), deposit(B, 1_000_000_000), mark(100_000)]);
    h.accept(sell(B, 1, 100_500, 10));
    h.accept(buy(A, 1, 99_000, 1));
    let a_post_only = PlaceOrder { post_only: true, ..place(A, 2, Side::Buy, 98_000, 1) };
    h.accept(Command::PlaceOrder(a_post_only));

    let wrong_market = ModifyOrder { order_id: order_id(A, 1), new_price: 0, new_size: 0, market: 9 };
    h.assert_rejected(Command::ModifyOrder(wrong_market), RejectReason::UnknownMarket);
    h.assert_rejected(modify(A, 9, 0, 0), RejectReason::UnknownOrder);
    h.assert_rejected(modify(A, 1, 0, 0), RejectReason::InvalidQty);
    h.assert_rejected(modify(A, 1, 1_000_001, 2), RejectReason::InvalidPrice);
    // A replace of a post-only order that would cross is refused before the band.
    h.assert_rejected(modify(A, 2, 102_500, i64::MAX), RejectReason::PostOnlyWouldCross);
    h.assert_rejected(modify(A, 1, 102_001, i64::MAX), RejectReason::PriceBand);
    h.assert_rejected(modify(A, 1, 99_000, i64::MAX), RejectReason::SizeLimit);
    h.assert_rejected(modify(A, 1, 99_500, 10_000_000), RejectReason::InsufficientMargin);

    // A cancel is checked for its market, then for the order.
    h.assert_rejected(
        Command::CancelOrder(CancelOrder { order_id: order_id(A, 1), market: 9 }),
        RejectReason::UnknownMarket,
    );
    h.assert_rejected(cancel(A, 9), RejectReason::UnknownOrder);
    h.accept(cancel(A, 1));
    h.assert_rejected(cancel(A, 1), RejectReason::UnknownOrder);
}

#[test]
fn the_fund_cannot_trade_withdraw_or_set_leverage() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(FUND, 1_000_000_000), mark(100_000)]);
    h.assert_rejected(buy(FUND, 1, 99_000, 1), RejectReason::ReservedAccount);
    h.assert_rejected(sell(FUND, 1, 101_000, 1), RejectReason::ReservedAccount);
    h.assert_rejected(withdraw(FUND, 1), RejectReason::ReservedAccount);
    h.assert_rejected(leverage(FUND, 20), RejectReason::ReservedAccount);
    // Its deposit went to the fund balance, not to an account.
    let snapshot = h.snapshot();
    assert_eq!((snapshot.fund_balance, snapshot.accounts.len()), (1_000_000_000, 0));
}

#[test]
fn an_unknown_account_gets_insufficient_margin_and_is_not_created() {
    let mut h = market_20x(0, 0);
    h.accept(mark(100_000));
    // Its slot would be flat, so this is not a margin call. `Harness::apply` checks that
    // the snapshot, which lists every account, didn't change.
    h.assert_rejected(buy(A, 1, 99_000, 1), RejectReason::InsufficientMargin);
    h.assert_rejected(withdraw(A, 1), RejectReason::InsufficientBalance);
    assert!(h.snapshot().accounts.is_empty());
}

#[test]
fn orders_need_a_mark_and_a_mark_needs_a_tier_table() {
    let mut h = Harness::new();
    h.accept(market_params(params(1_000_000, 0, 0, 20_000, 20)));
    h.assert_rejected(mark(100_000), RejectReason::NoRiskTiers);
    // A staged row is not a table yet.
    h.accept(tier_row(0, 2, 0, 20));
    h.assert_rejected(mark(100_000), RejectReason::NoRiskTiers);
    h.accept(tier_row(1, 2, 1_000_000_000_000, 10));
    h.accept(deposit(A, 1_000_000_000));
    h.assert_rejected(buy(A, 1, 99_000, 1), RejectReason::NoMark);
    assert_eq!(h.accept(mark(100_000)), vec![Event::MarkPrice(MarkPrice { price: 100_000, market: MARKET })]);
    h.accept(buy(A, 1, 99_000, 1));
    h.accept(cancel(A, 1));
    // New parameters clear the mark and the table, so the market must open again.
    h.accept(market_params(params(1_000_000, 0, 0, 20_000, 20)));
    h.assert_rejected(buy(A, 2, 99_000, 1), RejectReason::NoMark);
    h.assert_rejected(mark(100_000), RejectReason::NoRiskTiers);
}

#[test]
fn market_level_rejects_carry_the_operator_account() {
    let mut h = Harness::new();
    let operator_reject = |reason| Event::Reject(Reject { order_id: 0, account: AccountId::MAX, reason });
    assert_eq!(h.apply(mark(100_000)), vec![operator_reject(RejectReason::UnknownMarket)]);
    assert_eq!(h.apply(one_tier(20)), vec![operator_reject(RejectReason::UnknownMarket)]);
    let invalid = SetMarketParams { max_leverage: 0, ..params(1_000_000, 0, 0, 20_000, 20) };
    assert_eq!(h.apply(market_params(invalid)), vec![operator_reject(RejectReason::InvalidParams)]);
    // An account command's reject carries its account.
    assert_eq!(
        h.apply(leverage(A, 1)),
        vec![Event::Reject(Reject { order_id: 0, account: A, reason: RejectReason::UnknownMarket })]
    );
}

#[test]
fn set_mark_needs_a_price_in_the_markets_range() {
    let mut h = market_20x(0, 0);
    h.assert_rejected(mark(999), RejectReason::InvalidPrice);
    h.assert_rejected(mark(1_000_001), RejectReason::InvalidPrice);
    h.accept(mark(1_000));
    h.accept(mark(1_000_000));
}

// ---------------------------------------------------------------------------------------
// The price band and market parameters (RISK.md 5.3, 6.8).

#[test]
fn band_edges_at_sp500s_mark_accept_the_edge_and_reject_one_tick_past_it() {
    let mut h = market(sp500_params());
    h.accept_all([deposit(A, 1_000_000_000), deposit(B, 1_000_000_000), mark(75_024)]);
    // upper = floor(75,024 x 1.0086) = 75,669; lower = ceil(75,024 x 0.9914) = 74,379.
    h.accept(buy(A, 1, 75_669, 1));
    h.assert_rejected(buy(A, 2, 75_670, 1), RejectReason::PriceBand);
    h.assert_rejected(sell(B, 1, 74_378, 1), RejectReason::PriceBand);
    // At the lower edge the sell is accepted, and it trades with A's bid at A's price.
    let events = h.accept(sell(B, 2, 74_379, 1));
    assert_eq!(events[3], fill((A, 1), (B, 2), 75_669, 1, (10, 31), Side::Sell));
}

#[test]
fn set_market_params_applies_both_band_rules_with_the_larger_fee() {
    let mut h = Harness::new();
    let sp500 = sp500_params();
    assert_eq!(h.accept(market_params(sp500)), vec![Event::MarketParamsSet(sp500)]);
    // Rule 1: at 50x with a 400 ppm fee, 8,600 is the largest band.
    h.assert_rejected(
        market_params(SetMarketParams { price_band_ppm: 8_601, ..sp500 }),
        RejectReason::InvalidParams,
    );
    // Rule 2: with that band, min_price 1,004 is the lowest.
    h.assert_rejected(
        market_params(SetMarketParams { min_price: 1_003, ..sp500 }),
        RejectReason::InvalidParams,
    );
    // Review L1: 1x with band and fee of 225,000 each passes rule 1, and rule 2 refuses it.
    let l1 = SetMarketParams {
        maker_fee_ppm: 0,
        taker_fee_ppm: 225_000,
        price_band_ppm: 225_000,
        max_leverage: 1,
        ..sp500
    };
    h.assert_rejected(market_params(l1), RejectReason::InvalidParams);
    // The rules use the larger of the two fees, here the maker's: with it, rule 1 allows a
    // band of 8,500 and rule 2 then needs a min_price of 1,005 (1,004 with the taker's).
    let maker_larger = SetMarketParams {
        min_price: 1_005,
        maker_fee_ppm: 500,
        taker_fee_ppm: 400,
        price_band_ppm: 8_500,
        ..sp500
    };
    h.accept(market_params(maker_larger));
    h.assert_rejected(
        market_params(SetMarketParams { price_band_ppm: 8_501, ..maker_larger }),
        RejectReason::InvalidParams,
    );
    h.assert_rejected(
        market_params(SetMarketParams { min_price: 1_004, ..maker_larger }),
        RejectReason::InvalidParams,
    );
}

#[test]
fn set_market_params_rejects_each_invalid_field() {
    let mut h = Harness::new();
    let good = params(1_000_000, 0, 400, 20_000, 20);
    let invalid = [
        SetMarketParams { min_price: 0, ..good },
        SetMarketParams { min_price: 1_000_001, ..good },
        // The price limit 2^32, with a narrow range so the width is fine.
        SetMarketParams { min_price: (1 << 32) - 10, max_price: 1 << 32, ..good },
        // One tick wider than the book's 2^24 levels.
        SetMarketParams { max_price: 1_000 + (1 << 24), ..good },
        SetMarketParams { max_leverage: 0, ..good },
        SetMarketParams { taker_fee_ppm: -1, ..good },
        SetMarketParams { maker_fee_ppm: -401, ..good },
        // Rule 1 at 20x with a 2% band: the fee can be at most 2,500 ppm.
        SetMarketParams { taker_fee_ppm: 2_501, ..good },
    ];
    for params in invalid {
        h.assert_rejected(market_params(params), RejectReason::InvalidParams);
    }
    let valid = [
        SetMarketParams { min_price: (1 << 32) - 11, max_price: (1 << 32) - 1, ..good },
        SetMarketParams { maker_fee_ppm: -400, ..good },
        SetMarketParams { taker_fee_ppm: 2_500, ..good },
    ];
    for params in valid {
        h.accept(market_params(params));
    }
}

#[test]
fn set_market_params_needs_an_empty_market_and_keeps_its_slots_and_fees() {
    let mut h = market_20x(125, 400);
    h.accept_all([deposit(A, 1_000_000_000), deposit(B, 1_000_000_000), leverage(A, 20), mark(100_000)]);
    let new_params = params(500_000, 0, 0, 10_000, 10);

    h.accept(buy(A, 1, 99_000, 10));
    h.assert_rejected(market_params(new_params), RejectReason::MarketNotEmpty);
    // The order fills: the book is empty, but there are positions.
    h.accept(sell(B, 1, 99_000, 10));
    h.assert_rejected(market_params(new_params), RejectReason::MarketNotEmpty);
    // Both close, and get all their collateral back.
    h.accept(sell(A, 2, 99_500, 10));
    let events = h.accept(buy(B, 2, 99_500, 10));
    assert_eq!(
        events[4..],
        [position(B, 0, 0, 0), balance(B, 999_994_206), position(A, 0, 0, 0), balance(A, 1_000_004_751)]
    );
    assert_eq!(h.fees_collected(), 1_043);

    assert_eq!(h.accept(market_params(new_params)), vec![Event::MarketParamsSet(new_params)]);
    let snapshot = h.snapshot();
    let market = &snapshot.markets[0];
    assert_eq!((market.mark, market.tiers.len(), market.max_qty), (None, 0, max_qty(500_000)));
    assert_eq!(market.fees_collected, 1_043, "fees are kept");
    assert_eq!(market.accounts, vec![A, B]);
    assert_eq!(h.slot(A).leverage, 20, "a stored leverage above the new maximum is capped by lev_eff");
}

// ---------------------------------------------------------------------------------------
// The size limit and tiers (RISK.md 2.4, 4.3, 6.9).

#[test]
fn a_worst_case_size_above_max_qty_is_rejected_without_overflowing() {
    let mut h = market_20x(0, 0);
    let limit = max_qty(1_000_000);
    assert_eq!(limit, 9_007_199_254);
    // At mark 1,000 and 1x, max_qty lots need 9,007,199,254,000 of IM.
    h.accept_all([deposit(A, 10_000_000_000_000), mark(1_000)]);
    h.assert_rejected(buy(A, 1, 1_000, limit + 1), RejectReason::SizeLimit);
    h.assert_rejected(buy(A, 1, 1_000, i64::MAX), RejectReason::SizeLimit);
    assert_eq!(
        h.accept(buy(A, 1, 1_000, limit)),
        vec![ack(A, 1), balance(A, 992_800_746_000), position(A, 0, 0, 9_007_199_254_000)]
    );
    h.assert_rejected(buy(A, 2, 1_000, 1), RejectReason::SizeLimit);
    h.assert_rejected(sell(A, 2, 1_000, i64::MAX), RejectReason::SizeLimit);
    h.assert_rejected(modify(A, 1, 1_000, i64::MAX), RejectReason::SizeLimit);
    // A sell alongside it leaves W at max_qty: accepted, and it needs no more collateral.
    assert_eq!(h.accept(sell(A, 2, 1_001, 1)), vec![ack(A, 2)]);
}

#[test]
fn a_notional_exactly_at_a_tier_bound_is_margined_at_that_tier() {
    let mut h = market(sp500_params());
    h.accept_all(sp500_tier_rows());
    h.accept_all([deposit(A, 100_000_000_000), deposit(B, 100_000_000_000), deposit(C, 100_000_000_000)]);
    h.accept_all([leverage(A, 50), leverage(B, 50), leverage(C, 20), mark(100_000)]);
    // 5,000,000 lots at 100,000 is exactly $500,000: the 25x tier, 4% of the whole notional.
    assert_eq!(
        h.accept(buy(A, 1, 99_000, 5_000_000)),
        vec![ack(A, 1), balance(A, 80_000_000_000), position(A, 0, 0, 20_000_000_000)]
    );
    // One lot less is in the 50x tier.
    assert_eq!(
        h.accept(buy(B, 1, 99_000, 4_999_999)),
        vec![ack(B, 1), balance(B, 90_000_002_000), position(B, 0, 0, 9_999_998_000)]
    );
    // A chosen leverage of 20 is below the tier's 25x, so it binds.
    assert_eq!(
        h.accept(buy(C, 1, 99_000, 5_000_000)),
        vec![ack(C, 1), balance(C, 75_000_000_000), position(C, 0, 0, 25_000_000_000)]
    );
}

#[test]
fn a_tier_table_takes_effect_only_when_its_last_row_is_accepted() {
    let mut h = market(sp500_params());
    h.accept_all([deposit(A, 100_000_000_000), deposit(B, 100_000_000_000), leverage(A, 50), mark(100_000)]);

    // Rows 0 to 3 are staged, and invisible: an order is margined with the old table
    // (50x for every size), 10,000,000,000 rather than the new table's 20,000,000,000.
    for index in 0..4 {
        let row = sp500_tier_row(index);
        assert_eq!(h.accept(Command::SetRiskTier(row)), vec![Event::RiskTierSet(row)]);
    }
    let market = &h.snapshot().markets[0];
    assert_eq!((market.tiers.len(), market.staged_rows, market.staged_count), (1, 4, 8));
    assert_eq!(
        h.accept(buy(A, 1, 99_500, 5_000_000)),
        vec![ack(A, 1), balance(A, 90_000_000_000), position(A, 0, 0, 10_000_000_000)]
    );
    // A position too, so the commit below happens on a market with orders and positions.
    h.accept(sell(B, 1, 99_500, 10));
    assert_eq!(h.slot(A).pos, 10);
    h.accept_all((4..7).map(|index| Command::SetRiskTier(sp500_tier_row(index))));
    assert_eq!(h.snapshot().markets[0].tiers.len(), 1);
    h.accept(Command::SetRiskTier(sp500_tier_row(7)));
    let market = &h.snapshot().markets[0];
    assert_eq!((market.tiers.len(), market.staged_rows), (8, 0));
    assert_eq!(market.tiers[1], Tier { lower_bound: 500_000_000_000, max_leverage: 25 });

    // The next check of A's slot uses the new table: W' = 5,000,001 lots is in the 25x
    // tier, so IM(W') = 20,000,004,000 and the top-up is IM(W') minus A's equity.
    assert_eq!(
        h.accept(buy(A, 2, 99_500, 1)),
        vec![ack(A, 2), balance(A, 80_000_000_875), position(A, 10, 995_000, 19_999_999_000)]
    );
}

#[test]
fn tier_rows_out_of_order_or_inconsistent_are_rejected_and_row_0_restarts_the_batch() {
    let mut h = market_20x(0, 0);
    h.accept(tier_row(0, 3, 0, 20));
    // Row 2 right after row 0: rejected, and `Harness::apply` checks that the staged row
    // is still there.
    h.assert_rejected(tier_row(2, 3, 2_000, 5), RejectReason::InvalidParams);
    let inconsistent = [
        tier_row(1, 4, 1_000, 10), // a different count
        tier_row(1, 3, 0, 10),     // a bound not above the row before
        tier_row(1, 3, 1_000, 21), // above the market's maximum leverage
        tier_row(1, 3, 1_000, 0),  // leverage 0
        tier_row(3, 3, 3_000, 5),  // index not below count
        tier_row(0, 0, 0, 20),     // no rows
        tier_row(0, 9, 0, 20),     // more than 8 rows
        tier_row(0, 3, 1, 20),     // row 0 must start at 0
    ];
    for row in inconsistent {
        h.assert_rejected(row, RejectReason::InvalidParams);
    }
    h.accept(tier_row(1, 3, 1_000, 10));
    h.assert_rejected(tier_row(2, 3, 2_000, 11), RejectReason::InvalidParams); // leverage rises
    // A new row 0 drops the staged rows and starts again: a 2-row table.
    h.accept(tier_row(0, 2, 0, 15));
    h.assert_rejected(tier_row(2, 3, 2_000, 5), RejectReason::InvalidParams);
    h.accept(tier_row(1, 2, 7_000, 3));
    let market = &h.snapshot().markets[0];
    let committed =
        vec![Tier { lower_bound: 0, max_leverage: 15 }, Tier { lower_bound: 7_000, max_leverage: 3 }];
    assert_eq!((&market.tiers, market.staged_rows), (&committed, 0));
}

// ---------------------------------------------------------------------------------------
// Fees, releases, modifies, leverage (RISK.md 7, 8, 6.3, 6.6).

#[test]
fn fees_come_out_of_locked_collateral_and_rebates_round_down_in_size() {
    // A maker rebate of 50 ppm (the fees still sum to at least 0).
    let mut h = market_20x(-50, 400);
    h.accept_all([deposit(A, 1_000_000_000), deposit(B, 1_000_000_000), mark(100_000)]);
    h.accept(sell(B, 1, 99_500, 1_000));
    assert_eq!(
        h.accept(buy(A, 1, 99_500, 1_000))[3..],
        [
            fill((B, 1), (A, 1), 99_500, 1_000, (-4_975, 39_800), Side::Buy),
            position(B, -1_000, -99_500_000, 100_004_975),
            position(A, 1_000, 99_500_000, 99_960_200),
        ]
    );
    assert_eq!(h.fees_collected(), 34_825, "fees net of the rebate");

    // A rebate of 0.61725 micros rounds to none; the taker's 4.938 rounds up to 5.
    let mut h = market_20x(-50, 400);
    h.accept_all([deposit(A, 1_000_000_000), deposit(B, 1_000_000_000), mark(12_345)]);
    h.accept(sell(B, 1, 12_345, 1));
    assert_eq!(h.accept(buy(A, 1, 12_345, 1))[3], fill((B, 1), (A, 1), 12_345, 1, (0, 5), Side::Buy));
}

/// RISK.md 8.2's setup: A is long 1,000 lots bought at 100,000 at 20x, and has a buy of 500
/// resting at 99,000, so `W` is 1,500 and A holds `IM(1,500)` = 7,500,000.
fn long_with_a_resting_buy() -> Harness {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(A, 10_000_000), deposit(B, 1_000_000_000), leverage(A, 20), mark(100_000)]);
    h.accept(sell(B, 1, 100_000, 1_000));
    h.accept(buy(A, 1, 100_000, 1_000));
    assert_eq!(
        h.accept(buy(A, 2, 99_000, 500)),
        vec![ack(A, 2), balance(A, 2_500_000), position(A, 1_000, 100_000_000, 7_500_000)]
    );
    h
}

#[test]
fn a_release_keeps_unrealized_profit_in_the_slot() {
    let mut h = long_with_a_resting_buy();
    // At 102,000: E = 9,500,000 and IM(1,000) = 5,100,000. The minimum is `locked`, so the
    // release is 2,400,000 and the 2,000,000 of unrealized profit stays in the slot.
    assert_eq!(h.accept(mark(102_000)).len(), 1, "a mark move releases nothing by itself");
    assert_eq!(
        h.accept(cancel(A, 2)),
        vec![
            cancelled(A, 2, 500, Side::Buy, CancelReason::UserRequested),
            position(A, 1_000, 100_000_000, 5_100_000),
            balance(A, 4_900_000),
        ]
    );
}

#[test]
fn a_release_never_takes_equity_below_initial_margin() {
    let mut h = long_with_a_resting_buy();
    // At 98,000: E = 5,500,000 and IM(1,000) = 4,900,000. The minimum is E, so the
    // release is 600,000 and E ends exactly at IM.
    h.accept(mark(98_000));
    assert_eq!(
        h.accept(cancel(A, 2)),
        vec![
            cancelled(A, 2, 500, Side::Buy, CancelReason::UserRequested),
            position(A, 1_000, 100_000_000, 6_900_000),
            balance(A, 3_100_000),
        ]
    );
}

#[test]
fn an_unfilled_iocs_top_up_is_released_in_the_same_command() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(A, 10_000_000), leverage(A, 20), mark(100_000)]);
    let ioc = PlaceOrder { tif: TimeInForce::Ioc, ..place(A, 1, Side::Buy, 100_000, 100) };
    assert_eq!(
        h.accept(Command::PlaceOrder(ioc)),
        vec![
            ack(A, 1),
            balance(A, 9_500_000),
            position(A, 0, 0, 500_000),
            cancelled(A, 1, 100, Side::Buy, CancelReason::IocRemainder),
            position(A, 0, 0, 0),
            balance(A, 10_000_000),
        ]
    );
}

#[test]
fn a_self_trade_cancel_comes_off_the_accounts_open_total() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(A, 100_000_000), deposit(B, 100_000_000), mark(100_000)]);
    h.accept(sell(A, 1, 100_500, 7));
    h.accept(sell(B, 1, 101_000, 5));
    // The buy needs no top-up: with 7 lots offered, W' stays 7 and A holds IM(7) already.
    let events = h.accept(buy(A, 2, 101_000, 5));
    assert_eq!(
        events[..3],
        [
            ack(A, 2),
            cancelled(A, 1, 7, Side::Sell, CancelReason::SelfTrade),
            fill((B, 1), (A, 2), 101_000, 5, (0, 0), Side::Buy),
        ]
    );
    let a = h.slot(A);
    assert_eq!((a.pos, a.open_buys, a.open_sells), (5, 0, 0));
}

#[test]
fn the_pass_visits_the_taker_first_then_makers_in_the_order_of_their_first_fill() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(A, 1_000_000_000), deposit(B, 1_000_000_000), deposit(C, 1_000_000_000)]);
    h.accept_all([deposit(D, 1_000_000_000), mark(100_000)]);
    // B and C are each short 100 at 100,000, with IM(100) = 10,000,000 locked at 1x.
    h.accept_all([
        sell(B, 1, 100_000, 100),
        buy(D, 1, 100_000, 100),
        sell(C, 1, 100_000, 100),
        buy(D, 2, 100_000, 100),
    ]);
    // Each rests a strictly reducing buy; C's is the better price, so it fills first.
    assert_eq!(h.accept(buy(B, 2, 100_000, 100)), vec![ack(B, 2)]);
    assert_eq!(h.accept(buy(C, 2, 100_500, 100)), vec![ack(C, 2)]);
    // A sells into both. The pass: A (nothing to release), then C, then B, each flat now.
    assert_eq!(
        h.accept(sell(A, 1, 100_000, 200)),
        vec![
            ack(A, 1),
            balance(A, 980_000_000),
            position(A, 0, 0, 20_000_000),
            fill((C, 2), (A, 1), 100_500, 100, (0, 0), Side::Sell),
            position(C, 0, 0, 9_950_000),
            position(A, -100, -10_050_000, 20_000_000),
            fill((B, 2), (A, 1), 100_000, 100, (0, 0), Side::Sell),
            position(B, 0, 0, 10_000_000),
            position(A, -200, -20_050_000, 20_000_000),
            position(C, 0, 0, 0),
            balance(C, 999_950_000),
            position(B, 0, 0, 0),
            balance(B, 1_000_000_000),
        ]
    );
}

#[test]
fn in_a_margin_call_decreases_and_strictly_reducing_replaces_are_accepted() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(A, 5_500_000), deposit(B, 1_000_000_000), deposit(C, 1_000_000_000)]);
    h.accept_all([leverage(A, 20), mark(100_000)]);
    // A: long 1,000 at 100,000, a buy of 100 at 99,000 of which C fills 50, and a strictly
    // reducing sell of 100 at 101,000. Free 0, locked IM(1,100) = 5,500,000.
    h.accept_all([sell(B, 1, 100_000, 1_000), buy(A, 1, 100_000, 1_000), buy(A, 2, 99_000, 100)]);
    h.accept(sell(C, 1, 99_000, 50));
    assert_eq!(h.accept(sell(A, 3, 101_000, 100)), vec![ack(A, 3)]);
    let a = h.slot(A);
    assert_eq!(
        (a.pos, a.cost, a.locked, a.open_buys, a.open_sells),
        (1_050, 104_950_000, 5_500_000, 50, 100)
    );

    // At 98,000: E = 3,450,000 is below IM(1,050) = 5,145,000 and above MM = 2,572,500.
    h.accept(mark(98_000));
    h.assert_rejected(buy(A, 4, 97_000, 10), RejectReason::MarginCall);
    h.assert_rejected(modify(A, 2, 99_000, 200), RejectReason::MarginCall);
    // Decreases are always accepted. Nothing is released: in margin call E < IM(|pos|),
    // which is at most IM(W).
    assert_eq!(h.accept(modify(A, 2, 99_000, 80)), vec![modified(A, 2, 99_000, 30)]);
    assert_eq!(
        h.accept(modify(A, 2, 99_000, 50)),
        vec![cancelled(A, 2, 30, Side::Buy, CancelReason::SizeBelowFilled)]
    );
    // A replace that is strictly reducing skips the margin rule.
    assert_eq!(h.accept(modify(A, 3, 100_000, 100)), vec![modified(A, 3, 100_000, 100)]);

    // Back at 100,000 A is healthy, and a shrink releases what W no longer needs.
    h.accept(mark(100_000));
    assert_eq!(
        h.accept(modify(A, 3, 100_000, 40)),
        vec![modified(A, 3, 100_000, 40), position(A, 1_050, 104_950_000, 5_250_000), balance(A, 250_000)]
    );
}

#[test]
fn set_leverage_raises_release_cuts_top_up_and_an_uncovered_cut_is_rejected() {
    let mut h = market_20x(0, 0);
    h.accept_all([deposit(A, 10_000_000), deposit(B, 1_000_000_000), leverage(A, 10), mark(100_000)]);
    h.accept(sell(B, 1, 100_000, 1_000));
    h.accept(buy(A, 1, 100_000, 1_000));
    assert_eq!((h.slot(A).locked, h.free(A)), (10_000_000, 0));
    let leverage_set = |leverage| Event::LeverageSet(LeverageSet { account: A, market: MARKET, leverage });

    // 20x: IM(1,000) falls to 5,000,000, and the pass releases the rest.
    assert_eq!(
        h.accept(leverage(A, 20)),
        vec![leverage_set(20), position(A, 1_000, 100_000_000, 5_000_000), balance(A, 5_000_000)]
    );
    // Back to 10x: a top-up of 5,000,000, source first.
    assert_eq!(
        h.accept(leverage(A, 10)),
        vec![leverage_set(10), balance(A, 0), position(A, 1_000, 100_000_000, 10_000_000)]
    );
    // 5x needs 10,000,000 more, which A doesn't have: rejected, and the leverage stays 10.
    h.assert_rejected(leverage(A, 5), RejectReason::InsufficientMargin);
    assert_eq!(h.slot(A).leverage, 10);
    h.assert_rejected(leverage(A, 0), RejectReason::InvalidLeverage);
    h.assert_rejected(leverage(A, 21), RejectReason::InvalidLeverage);
}

#[test]
fn set_leverage_on_a_new_slot_creates_the_account_and_only_echoes() {
    let mut h = market_20x(0, 0);
    assert_eq!(
        h.accept(leverage(C, 7)),
        vec![Event::LeverageSet(LeverageSet { account: C, market: MARKET, leverage: 7 })]
    );
    let snapshot = h.snapshot();
    assert_eq!(snapshot.accounts, vec![AccountSnapshot { account: C, free: 0, next_seq: 0 }]);
    assert_eq!(h.slot(C).leverage, 7);
}

// ---------------------------------------------------------------------------------------
// Deposits and withdrawals (RISK.md 6.4, 6.5).

#[test]
fn deposits_and_withdrawals_check_their_amounts() {
    let mut h = Harness::new();
    h.assert_rejected(deposit(A, 0), RejectReason::InvalidAmount);
    h.assert_rejected(deposit(A, -5), RejectReason::InvalidAmount);
    assert_eq!(h.accept(deposit(A, i64::MAX - 1)), vec![balance(A, i64::MAX - 1)]);
    h.assert_rejected(deposit(A, 2), RejectReason::InvalidAmount);
    assert_eq!(h.accept(deposit(A, 1)), vec![balance(A, i64::MAX)]);
    h.assert_rejected(withdraw(A, 0), RejectReason::InvalidAmount);
    h.assert_rejected(withdraw(B, 1), RejectReason::InsufficientBalance);
    assert_eq!(h.accept(withdraw(A, i64::MAX)), vec![balance(A, 0)]);
    h.assert_rejected(withdraw(A, 1), RejectReason::InsufficientBalance);

    // The fund's balance is separate, and has the same overflow check.
    assert_eq!(h.accept(deposit(FUND, i64::MAX)), vec![balance(FUND, i64::MAX)]);
    h.assert_rejected(deposit(FUND, 1), RejectReason::InvalidAmount);
    let snapshot = h.snapshot();
    assert_eq!(snapshot.net_deposits, i128::from(i64::MAX));
    assert_eq!(snapshot.accounts, vec![AccountSnapshot { account: A, free: 0, next_seq: 0 }]);
}

// ---------------------------------------------------------------------------------------
// Engine plumbing.

#[test]
fn capacities_and_the_hash_seed_change_nothing_but_speed() {
    // RISK.md 14.3's engine seed and capacity test: scratch and touched capacities of 0 and
    // 1 make both grow; other seeds put the ids in other buckets.
    let commands = {
        let mut commands = vec![market_params(params(1_000_000, 125, 400, 20_000, 20)), one_tier(20)];
        commands.extend([deposit(A, 5_000_000), deposit(B, 1_000_000_000), deposit(C, 1_000_000_000)]);
        commands.extend([
            leverage(A, 20),
            mark(100_000),
            sell(B, 1, 101_000, 1_000),
            buy(A, 1, 101_000, 1_000),
        ]);
        commands.extend([buy(A, 2, 100_000, 10), buy(C, 1, 99_500, 1_000), sell(A, 3, 99_500, 1_000)]);
        commands.extend([
            buy(C, 2, 99_000, 5),
            sell(B, 2, 99_000, 20),
            modify(C, 1, 99_000, 1_000),
            cancel(B, 2),
        ]);
        commands
    };
    let default = EngineOptions::default();
    let variants = [
        EngineOptions { scratch_capacity: 0, ..default },
        EngineOptions { scratch_capacity: 1, id_hash_seed: 0xDEAD_BEEF, ..default },
        EngineOptions {
            order_capacity: 0,
            account_capacity: 0,
            slot_capacity: 0,
            id_hash_seed: u64::MAX,
            ..default
        },
    ];
    let mut reference = Harness::new();
    let expected: Vec<Vec<Event>> = commands.iter().map(|&command| reference.apply(command)).collect();
    for options in variants {
        let mut h = Harness::with_options(options);
        for (command, expected) in commands.iter().zip(&expected) {
            assert_eq!(&h.apply(*command), expected, "{options:?}: {command:?}");
        }
        assert_eq!(h.snapshot(), reference.snapshot());
    }
}

#[test]
fn engine_options_debug_output_leaves_the_seed_out() {
    let options = EngineOptions { id_hash_seed: 0x5EC2_E7A1, ..EngineOptions::default() };
    let text = format!("{options:?}");
    assert!(text.contains("scratch_capacity"), "{text}");
    assert!(!text.contains("seed") && !text.contains(&options.id_hash_seed.to_string()), "{text}");
}

#[test]
fn set_slot_for_test_keeps_the_position_count_and_the_index_in_step() {
    let mut engine: Engine<Book, Fast> = Engine::new(EngineOptions::default());
    let mut events = Vec::new();
    engine.apply(&market_params(params(1_000_000, 0, 0, 20_000, 20)), &mut events);
    engine.apply(&one_tier(20), &mut events);
    engine.apply(&mark(100_000), &mut events);
    // T1's long, on a 20x market: key 73,100.
    engine.set_slot_for_test(MARKET, A, 100_000, 7_502_400_000, 375_120_000);
    let market = engine.market(MARKET);
    assert_eq!(market.nonzero_positions, 1);
    assert_eq!(market.index.first_long(), Some((73_100, A)));
    // A flat slot with negative collateral: not indexed.
    engine.set_slot_for_test(MARKET, A, 0, 0, -5);
    let market = engine.market(MARKET);
    assert_eq!((market.nonzero_positions, market.index.first_long()), (0, None));
    assert_eq!(market.slot(A).locked, -5);
}
