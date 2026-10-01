//! The command properties of `docs/RISK.md` section 12: the invariants that need more than
//! the state after a command, namely the snapshot before it, the command and its events.
//! `Harness::apply` runs [`check`] after every command of every test. (I15, "a reject
//! changes nothing", it checks itself.)
//!
//! - **I12.** After a `Liquidation`, the account has no resting orders in that market and its
//!   slot there is all zeros.
//! - **I13, I17.** An accepted place or replacing modify was strictly reducing (and got no
//!   top-up), or it left `E ≥ IM(W')`. A reported top-up `t` is at least 1 and exactly
//!   `IM(W') − E`; for `SetLeverage`, `IM(W)` at the new leverage.
//! - **I14.** Each fill's fees are `ceil(P × q × rate / 10^6)`, and sum to at least 0.
//! - **I18.** After a release, `locked ≥ IM(W)` and `E ≥ IM(W)`.
//! - **I19.** At the end of the command, every slot it touched (its own, its makers', the
//!   owners of swept orders) holds no more than it needs: `min(locked, E) ≤ IM(W)`.
//! - **I20.** An accepted withdrawal leaves `free + Σ locked` at least 10% of the account's
//!   open notional at mark; a `WithdrawalReserve` reject is one that wouldn't have.
//!
//! **Formulas of its own.** Initial margin (with its own tier lookup), equity, the
//! worst-case size, "strictly reducing" and the fee are written again here, apart from
//! `money.rs`, and read only snapshots and events. Every engine mode shares `money.rs`, so a
//! wrong formula there would give the same wrong events in all four; these checks would
//! still catch it (RISK.md 14.3).

use super::*;
use crate::book::RestingOrder;
use crate::event::Liquidation;

/// Checks every command property the command could break. See the module docs.
pub(super) fn check(before: &EngineSnapshot, command: &Command, events: &[Event], after: &EngineSnapshot) {
    if let [Event::Reject(reject)] = events {
        if reject.reason == RejectReason::WithdrawalReserve {
            check_withdrawal_reserve_reject(before, command);
        }
        return;
    }
    check_fill_fees(before, events);
    check_liquidated_slots_are_empty(after, events);
    check_top_up(before, command, events);
    check_releases(after, events);
    check_touched_slots_hold_no_excess(after, command, events);
    if let Command::Withdraw(withdraw) = command {
        let (free, locked, reserve) = withdrawal_sums(after, withdraw.account);
        assert!(free >= 0 && free + locked >= reserve, "I20: {withdraw:?} left too little behind");
    }
}

// ---------------------------------------------------------------------------------------
// The formulas, written again (RISK.md 4, 5.1, 7.2).

/// `a / b` rounded up, for a positive `b`.
fn ceil(a: i128, b: i128) -> i128 {
    -((-a).div_euclid(b))
}

/// Initial margin for `size` lots at `mark`: the notional over the effective leverage,
/// rounded up. The effective leverage is the chosen one, capped by the tier of the highest
/// bound the notional reaches.
fn initial_margin(size: i128, mark: Price, chosen_leverage: u16, tiers: &[Tier]) -> i128 {
    let notional = size * i128::from(mark);
    let tier = tiers.iter().rev().find(|tier| i128::from(tier.lower_bound) <= notional);
    let leverage = chosen_leverage.min(tier.expect("a tier table starts at 0").max_leverage);
    ceil(notional, i128::from(leverage))
}

fn equity(slot: &SlotSnapshot, mark: Price) -> i128 {
    i128::from(slot.locked) + i128::from(slot.pos) * i128::from(mark) - i128::from(slot.cost)
}

/// The largest position the slot could reach if all its orders on one side filled.
fn worst_case(pos: i128, open_buys: i128, open_sells: i128) -> i128 {
    (pos + open_buys).abs().max((pos - open_sells).abs())
}

/// The initial margin of the slot's worst-case size, at the market's mark.
fn requirement(market: &MarketSnapshot, slot: &SlotSnapshot) -> i128 {
    let size = worst_case(slot.pos.into(), slot.open_buys.into(), slot.open_sells.into());
    initial_margin(size, market.mark.expect("a market with a mark"), slot.leverage, &market.tiers)
}

// ---------------------------------------------------------------------------------------
// Looking things up in a snapshot.

fn market_of(snapshot: &EngineSnapshot, market: MarketId) -> &MarketSnapshot {
    snapshot.markets.iter().find(|m| m.params.market == market).expect("the market exists")
}

/// The account's slot, or the empty slot a first order is checked against.
fn slot_of(market: &MarketSnapshot, account: AccountId) -> SlotSnapshot {
    let empty =
        SlotSnapshot { account, pos: 0, cost: 0, locked: 0, leverage: 1, open_buys: 0, open_sells: 0 };
    market.slots.iter().find(|s| s.account == account).copied().unwrap_or(empty)
}

fn free_of(snapshot: &EngineSnapshot, account: AccountId) -> Micros {
    snapshot.accounts.iter().find(|a| a.account == account).map_or(0, |a| a.free)
}

fn resting_order(market: &MarketSnapshot, order_id: OrderId) -> RestingOrder {
    let mut orders = market.book.bids.iter().chain(&market.book.asks);
    *orders.find(|o| o.order_id == order_id).expect("an accepted modify's order was resting")
}

// ---------------------------------------------------------------------------------------
// The checks.

/// I14: each fill's two fees, recomputed from the market's rates.
fn check_fill_fees(before: &EngineSnapshot, events: &[Event]) {
    for event in events {
        let Event::Fill(fill) = event else { continue };
        let params = market_of(before, fill.market).params;
        let fee =
            |rate: i32| ceil(i128::from(fill.price) * i128::from(fill.qty) * i128::from(rate), 1_000_000);
        assert_eq!(i128::from(fill.maker_fee), fee(params.maker_fee_ppm), "I14: maker fee of {fill:?}");
        assert_eq!(i128::from(fill.taker_fee), fee(params.taker_fee_ppm), "I14: taker fee of {fill:?}");
        assert!(fill.maker_fee + fill.taker_fee >= 0, "I14: a fill's fees sum below 0: {fill:?}");
    }
}

/// I12: a liquidated account has no orders left in the market, and its slot there is all
/// zeros.
fn check_liquidated_slots_are_empty(after: &EngineSnapshot, events: &[Event]) {
    for event in events {
        let &Event::Liquidation(Liquidation { account, market, .. }) = event else { continue };
        let market = market_of(after, market);
        let slot = slot_of(market, account);
        let values = (slot.pos, slot.cost, slot.locked, slot.open_buys, slot.open_sells);
        assert_eq!(values, (0, 0, 0, 0, 0), "I12: account {account}'s liquidated slot is not empty");
        let mut orders = market.book.bids.iter().chain(&market.book.asks);
        assert!(
            orders.all(|order| account_of(order.order_id) != account),
            "I12: account {account} still has orders after its liquidation"
        );
    }
}

/// What an accepted command asks the margin rule for: the worst-case size of the account's
/// slot in the market, margined at `leverage`.
struct MarginedSize {
    account: AccountId,
    market: MarketId,
    size: i128,
    leverage: u16,
    strictly_reducing: bool,
}

/// The size a place, replacing modify or `SetLeverage` is margined for (RISK.md 5.1, 6.6),
/// from the state before it; `None` for every other command, including the pure decreases.
fn margined_size(before: &EngineSnapshot, command: &Command) -> Option<MarginedSize> {
    let (account, market_id, side, side_change) = match *command {
        Command::PlaceOrder(order) => {
            (account_of(order.order_id), order.market, order.side, i128::from(order.qty))
        }
        Command::ModifyOrder(modify) => {
            let order = resting_order(market_of(before, modify.market), modify.order_id);
            let new_remaining = modify.new_size - order.filled;
            let decrease =
                new_remaining <= 0 || (modify.new_price == order.price && new_remaining <= order.qty);
            if decrease {
                return None;
            }
            (account_of(modify.order_id), modify.market, order.side, i128::from(new_remaining - order.qty))
        }
        Command::SetLeverage(set) => {
            // Margined again at the new leverage, for the worst-case size it already has.
            let slot = slot_of(market_of(before, set.market), set.account);
            let size = worst_case(slot.pos.into(), slot.open_buys.into(), slot.open_sells.into());
            return (size > 0).then_some(MarginedSize {
                account: set.account,
                market: set.market,
                size,
                leverage: set.leverage,
                strictly_reducing: false,
            });
        }
        _ => return None,
    };
    let slot = slot_of(market_of(before, market_id), account);
    let pos = i128::from(slot.pos);
    let (buys, sells) = match side {
        Side::Buy => (i128::from(slot.open_buys) + side_change, i128::from(slot.open_sells)),
        Side::Sell => (i128::from(slot.open_buys), i128::from(slot.open_sells) + side_change),
    };
    let strictly_reducing = match side {
        Side::Sell => pos > 0 && sells <= pos,
        Side::Buy => pos < 0 && buys <= -pos,
    };
    Some(MarginedSize {
        account,
        market: market_id,
        size: worst_case(pos, buys, sells),
        leverage: slot.leverage,
        strictly_reducing,
    })
}

/// I13 and I17. The top-up is reported right after the command's first event (`Ack`,
/// `Modified` or `LeverageSet`): the free balance first, then the slot.
fn check_top_up(before: &EngineSnapshot, command: &Command, events: &[Event]) {
    let Some(margined) = margined_size(before, command) else { return };
    let market = market_of(before, margined.market);
    let slot = slot_of(market, margined.account);
    let top_up = match events.get(1) {
        Some(Event::BalanceChanged(free)) if free.account == margined.account => {
            let top_up = i128::from(free_of(before, margined.account) - free.free);
            assert!(top_up >= 1, "I17: a top-up of {top_up} was reported for {command:?}");
            top_up
        }
        _ => 0,
    };
    if margined.strictly_reducing {
        assert_eq!(top_up, 0, "I13: the strictly reducing {command:?} was topped up");
        return;
    }
    let mark = market.mark.expect("a margin-checked command is in a market with a mark");
    let needed = initial_margin(margined.size, mark, margined.leverage, &market.tiers);
    let equity = equity(&slot, mark);
    assert!(equity + top_up >= needed, "I13: {command:?} left E + top-up below IM(W')");
    if top_up > 0 {
        assert_eq!(equity + top_up, needed, "I17: the top-up for {command:?} is not exactly IM(W') - E");
    }
}

/// I18. A release is the slot's `PositionChanged` followed by its account's
/// `BalanceChanged` (a top-up is the other way round). It is the last change to the slot in
/// its command, so the snapshot after the command shows the slot as the release left it.
fn check_releases(after: &EngineSnapshot, events: &[Event]) {
    for pair in events.windows(2) {
        let [Event::PositionChanged(slot_event), Event::BalanceChanged(free_event)] = *pair else { continue };
        if slot_event.account != free_event.account || slot_event.account == FUND {
            continue;
        }
        let market = market_of(after, slot_event.market);
        let slot = slot_of(market, slot_event.account);
        let needed = requirement(market, &slot);
        assert!(
            i128::from(slot.locked) >= needed && equity(&slot, market.mark.expect("a mark")) >= needed,
            "I18: account {}'s release took it below IM(W)",
            slot_event.account
        );
    }
}

/// I19, over the slots the post-command pass visited (RISK.md 8.1): the command's own,
/// its makers', or for `SetMark` the owners of swept orders.
fn check_touched_slots_hold_no_excess(after: &EngineSnapshot, command: &Command, events: &[Event]) {
    let makers = || {
        events.iter().filter_map(|event| match event {
            Event::Fill(fill) => Some(account_of(fill.maker_order)),
            _ => None,
        })
    };
    let (market_id, touched): (MarketId, Vec<AccountId>) = match *command {
        Command::PlaceOrder(order) => (order.market, makers().chain([account_of(order.order_id)]).collect()),
        Command::ModifyOrder(modify) => {
            (modify.market, makers().chain([account_of(modify.order_id)]).collect())
        }
        Command::CancelOrder(cancel) => (cancel.market, vec![account_of(cancel.order_id)]),
        Command::SetLeverage(set) => (set.market, vec![set.account]),
        Command::SetMark(mark) => {
            let swept = events.iter().filter_map(|event| match event {
                Event::Cancelled(c) if c.reason == CancelReason::PriceBand => Some(account_of(c.order_id)),
                _ => None,
            });
            (mark.market, swept.collect())
        }
        _ => return,
    };
    let market = market_of(after, market_id);
    let Some(mark) = market.mark else { return };
    for account in touched {
        let slot = slot_of(market, account);
        let held = i128::from(slot.locked).min(equity(&slot, mark));
        assert!(
            held <= requirement(market, &slot),
            "I19: account {account} was left holding more than IM(W)"
        );
    }
}

/// The account's free balance, its collateral locked in all markets, and the reserve a
/// withdrawal must leave: 10% of its open notional at mark, rounded up (RISK.md 6.5).
fn withdrawal_sums(snapshot: &EngineSnapshot, account: AccountId) -> (i128, i128, i128) {
    let (mut locked, mut open_notional) = (0, 0);
    for market in &snapshot.markets {
        let Some(slot) = market.slots.iter().find(|s| s.account == account) else { continue };
        locked += i128::from(slot.locked);
        if slot.pos != 0 {
            open_notional += i128::from(slot.pos).abs() * i128::from(market.mark.expect("a mark"));
        }
    }
    (i128::from(free_of(snapshot, account)), locked, ceil(open_notional, 10))
}

/// I20's other half: a `WithdrawalReserve` reject is one that would have broken the rule.
fn check_withdrawal_reserve_reject(before: &EngineSnapshot, command: &Command) {
    let Command::Withdraw(withdraw) = command else { panic!("WithdrawalReserve for {command:?}") };
    let (free, locked, reserve) = withdrawal_sums(before, withdraw.account);
    let free_after = free - i128::from(withdraw.amount);
    assert!(free_after + locked < reserve, "I20: {withdraw:?} was rejected but keeps the reserve");
}
