//! The independent checker of `docs/RISK.md` 14.3. Every engine mode shares `money.rs`, so a
//! wrong formula there would give the same wrong events in all five engines, and agreement
//! would not notice. This checker writes the formulas again (initial margin with its own
//! tier lookup, maintenance margin, equity, the worst-case size, "strictly reducing", the
//! fee and the withdrawal reserve) and, from the snapshot before a command, the command, its
//! events and the snapshot after it, checks:
//!
//! - **I5.** No slot is left below maintenance margin.
//! - **I13, I17.** An accepted place, replacing modify or `SetLeverage` was strictly
//!   reducing (and got no top-up), or left `E + top-up >= IM(W')`; a reported top-up is at
//!   least 1 and exactly `IM(W') − E`. And a `MarginCall` or `InsufficientMargin` reject is
//!   one the margin rule really fails, with the reason 5.2 gives.
//! - **I18.** After each release, `locked >= IM(W)` and `E >= IM(W)`.
//! - **I19.** Every slot the command touched holds no more than it needs:
//!   `min(locked, E) <= IM(W)`.
//! - **I20.** An accepted withdrawal keeps the 10% reserve; a `WithdrawalReserve` reject
//!   is one that wouldn't have.
//! - **I12, I14**, which are as cheap: a liquidated slot is empty and has no orders; each
//!   fill's fees are `ceil(P × q × rate / 10^6)` and sum to at least 0.
//! - Each `Reject` carries the order id and account RISK.md 6 gives it, and is the
//!   command's only event.
//!
//! **Contract.** Reads only snapshots, commands and events, never the engine's internals.
//! Panics, naming the invariant, at the first failure.

use engine::command::Command;
use engine::engine::{EngineSnapshot, MarketSnapshot, SlotSnapshot};
use engine::event::{CancelReason, Event, RejectReason};
use engine::money::Tier;
use engine::types::{AccountId, Micros, OrderId, Price, Qty, Side, account_of};

use super::shadow_ledger::{BlockKind, split_into_blocks};
use super::state::{free_of, market_of, resting_order, resting_orders, slot_of};

/// Checks everything listed in the module docs.
pub fn check(before: &EngineSnapshot, command: &Command, events: &[Event], after: &EngineSnapshot) {
    check_reject_fields(command, events);
    check_no_slot_below_maintenance(after);
    check_margin_rule(before, command, events);
    check_releases(after, events);
    check_touched_slots_hold_no_excess(after, command, events);
    check_withdrawal_rule(before, command, events, after);
    check_fills_and_liquidations(before, events, after);
}

// ---------------------------------------------------------------------------------------
// The formulas, written again from RISK.md 4, 5.1, 6.5 and 7.2.

/// `a / b` rounded up, for a positive `b`.
fn ceil_div(a: i128, b: i128) -> i128 {
    -((-a).div_euclid(b))
}

/// The row of the tier table a notional falls in: the last row whose bound it reaches.
pub fn tier_row(tiers: &[Tier], notional: i128) -> usize {
    tiers.iter().rposition(|tier| i128::from(tier.lower_bound) <= notional).expect("a tier table starts at 0")
}

/// IM of `size` lots at `mark`: the notional over the chosen leverage, capped by the tier the
/// notional falls in, rounded up.
pub fn initial_margin(size: i128, mark: Price, chosen_leverage: u16, tiers: &[Tier]) -> i128 {
    let notional = size * i128::from(mark);
    let leverage = chosen_leverage.min(tiers[tier_row(tiers, notional)].max_leverage);
    ceil_div(notional, i128::from(leverage))
}

fn maintenance_margin(pos: Qty, mark: Price, max_leverage: u16) -> i128 {
    ceil_div(i128::from(pos).abs() * i128::from(mark), 2 * i128::from(max_leverage))
}

pub fn equity(slot: &SlotSnapshot, mark: Price) -> i128 {
    i128::from(slot.locked) + i128::from(slot.pos) * i128::from(mark) - i128::from(slot.cost)
}

fn worst_case(pos: i128, open_buys: i128, open_sells: i128) -> i128 {
    (pos + open_buys).abs().max((pos - open_sells).abs())
}

/// IM of the slot's worst-case size now, at the market's mark.
fn requirement(market: &MarketSnapshot, slot: &SlotSnapshot, mark: Price) -> i128 {
    let size = worst_case(slot.pos.into(), slot.open_buys.into(), slot.open_sells.into());
    initial_margin(size, mark, slot.leverage, &market.tiers)
}

/// The account's free balance, its collateral locked in every market, and the reserve a
/// withdrawal must leave: 10% of its open notional at mark, rounded up (RISK.md 6.5).
pub fn withdrawal_sums(state: &EngineSnapshot, account: AccountId) -> (i128, i128, i128) {
    let (mut locked, mut open_notional) = (0, 0);
    for market in &state.markets {
        let slot = slot_of(market, account);
        locked += i128::from(slot.locked);
        if slot.pos != Qty::ZERO {
            open_notional +=
                i128::from(slot.pos).abs() * i128::from(market.mark.expect("a position has a mark"));
        }
    }
    (i128::from(free_of(state, account)), locked, ceil_div(open_notional, 10))
}

// ---------------------------------------------------------------------------------------
// What a command asks of the margin rule (RISK.md 5.1, 5.2, 6.6).

/// A place, a replacing modify or a `SetLeverage`, seen from the state before it.
pub struct MarginRequest {
    pub account: AccountId,
    pub slot: SlotSnapshot,
    pub mark: Price,
    pub tiers: Vec<Tier>,
    /// `W'`: the slot's worst-case size with the order resting in full.
    pub size: i128,
    /// The slot's chosen leverage, or for `SetLeverage` the new one.
    pub leverage: u16,
    pub strictly_reducing: bool,
}

/// What the command asks of the margin rule, or `None` for every other command, for the
/// pure decreases (a shrink in place, a removal), and when the market, the order or the
/// mark is missing (the command is rejected before the margin rule then).
pub fn margin_request(before: &EngineSnapshot, command: &Command) -> Option<MarginRequest> {
    let (market_id, account, side, side_change, new_leverage) = match *command {
        Command::PlaceOrder(o) => (o.market, account_of(o.order_id), Some(o.side), o.qty, None),
        Command::ModifyOrder(m) => {
            let order = resting_order(before, m.market, m.order_id)?;
            let new_remaining = m.new_size - order.filled;
            if new_remaining <= Qty::ZERO || (m.new_price == order.price && new_remaining <= order.qty) {
                return None;
            }
            (m.market, account_of(m.order_id), Some(order.side), new_remaining - order.qty, None)
        }
        Command::SetLeverage(l) => (l.market, l.account, None, Qty::ZERO, Some(l.leverage)),
        _ => return None,
    };
    let market = market_of(before, market_id)?;
    let slot = slot_of(market, account);
    let (pos, mut buys, mut sells) =
        (i128::from(slot.pos), i128::from(slot.open_buys), i128::from(slot.open_sells));
    match side {
        Some(Side::Buy) => buys += i128::from(side_change),
        Some(Side::Sell) => sells += i128::from(side_change),
        None => {}
    }
    let strictly_reducing = match side {
        Some(Side::Sell) => pos > 0 && sells <= pos,
        Some(Side::Buy) => pos < 0 && buys <= -pos,
        None => false,
    };
    Some(MarginRequest {
        account,
        slot,
        mark: market.mark?,
        tiers: market.tiers.clone(),
        size: worst_case(pos, buys, sells),
        leverage: new_leverage.unwrap_or(slot.leverage),
        strictly_reducing,
    })
}

/// True if the slot is in margin call before the command: `E < IM(|pos|)` at its leverage.
pub fn in_margin_call(request: &MarginRequest) -> bool {
    let size = i128::from(request.slot.pos).abs();
    equity(&request.slot, request.mark)
        < initial_margin(size, request.mark, request.slot.leverage, &request.tiers)
}

// ---------------------------------------------------------------------------------------
// The checks.

fn check_reject_fields(command: &Command, events: &[Event]) {
    let [Event::Reject(reject)] = events else {
        assert!(
            !events.iter().any(|e| matches!(e, Event::Reject(_))),
            "a Reject among other events: {events:?}"
        );
        return;
    };
    let expected = match *command {
        Command::PlaceOrder(o) => (o.order_id, account_of(o.order_id)),
        Command::CancelOrder(c) => (c.order_id, account_of(c.order_id)),
        Command::ModifyOrder(m) => (m.order_id, account_of(m.order_id)),
        Command::Deposit(d) => (OrderId::new(0), d.account),
        Command::Withdraw(w) => (OrderId::new(0), w.account),
        Command::SetLeverage(l) => (OrderId::new(0), l.account),
        Command::SetMark(_) | Command::SetMarketParams(_) | Command::SetRiskTier(_) => {
            (OrderId::new(0), AccountId::MAX)
        }
    };
    assert_eq!((reject.order_id, reject.account), expected, "RISK.md 6: the Reject of {command:?}");
}

/// I5.
fn check_no_slot_below_maintenance(after: &EngineSnapshot) {
    for market in &after.markets {
        let Some(mark) = market.mark else { continue };
        for slot in &market.slots {
            let mm = maintenance_margin(slot.pos, mark, market.params.max_leverage);
            assert!(equity(slot, mark) >= mm, "I5: {slot:?} is below maintenance margin at {mark}");
        }
    }
}

/// I13 and I17, and the reason of a margin reject. A top-up is reported right after the
/// command's first event (`Ack`, `Modified` or `LeverageSet`): the free balance, then the slot.
fn check_margin_rule(before: &EngineSnapshot, command: &Command, events: &[Event]) {
    let Some(request) = margin_request(before, command) else { return };
    // Rejected by an earlier check, the margin rule never ran (the leverage may be 0).
    let margin_reject = match events {
        [Event::Reject(r)]
            if matches!(r.reason, RejectReason::MarginCall | RejectReason::InsufficientMargin) =>
        {
            Some(r.reason)
        }
        [Event::Reject(_)] => return,
        _ => None,
    };
    let equity = equity(&request.slot, request.mark);
    let required = initial_margin(request.size, request.mark, request.leverage, &request.tiers);
    let free = i128::from(free_of(before, request.account));
    if let Some(reason) = margin_reject {
        assert!(
            !request.strictly_reducing && required - equity > free,
            "5.2: {command:?} passes the margin rule"
        );
        let expected = match command {
            Command::SetLeverage(_) => RejectReason::InsufficientMargin,
            _ if in_margin_call(&request) => RejectReason::MarginCall,
            _ => RejectReason::InsufficientMargin,
        };
        assert_eq!(reason, expected, "5.2: the reason {command:?} was rejected for");
        return;
    }
    let top_up = match events.get(1) {
        Some(Event::BalanceChanged(b)) if b.account == request.account => free - i128::from(b.free),
        _ => 0,
    };
    if request.strictly_reducing {
        assert_eq!(top_up, 0, "I13: the strictly reducing {command:?} was topped up");
    } else {
        assert!(equity + top_up >= required, "I13: {command:?} left E + top-up below IM(W')");
        let exact = top_up == 0 || (top_up >= 1 && equity + top_up == required);
        assert!(exact, "I17: the top-up of {top_up} for {command:?} is not IM(W') − E");
    }
}

/// I18. A release is the last change to its slot in the command, so the snapshot after the
/// command shows the slot as the release left it.
fn check_releases(after: &EngineSnapshot, events: &[Event]) {
    for block in split_into_blocks(events) {
        let BlockKind::Release { account, market } = block.kind else { continue };
        let market = market_of(after, market).expect("a release is in a market");
        let (slot, mark) = (slot_of(market, account), market.mark.expect("a release is at a mark"));
        let needed = requirement(market, &slot, mark);
        assert!(i128::from(slot.locked) >= needed && equity(&slot, mark) >= needed, "I18: {slot:?}");
    }
}

/// I19, over the slots the post-command pass visited (RISK.md 8.1): the command's own, its
/// makers', and for `SetMark` the owners of swept orders.
fn check_touched_slots_hold_no_excess(after: &EngineSnapshot, command: &Command, events: &[Event]) {
    let (market_id, own) = match *command {
        Command::PlaceOrder(o) => (o.market, Some(account_of(o.order_id))),
        Command::ModifyOrder(m) => (m.market, Some(account_of(m.order_id))),
        Command::CancelOrder(c) => (c.market, Some(account_of(c.order_id))),
        Command::SetLeverage(l) => (l.market, Some(l.account)),
        Command::SetMark(s) => (s.market, None),
        _ => return,
    };
    let touched_by_events = events.iter().filter_map(|event| match event {
        Event::Fill(fill) => Some(account_of(fill.maker_order)),
        Event::Cancelled(c) if c.reason == CancelReason::PriceBand => Some(account_of(c.order_id)),
        _ => None,
    });
    let Some(market) = market_of(after, market_id) else { return };
    let (Some(mark), false) = (market.mark, matches!(events, [Event::Reject(_)])) else { return };
    for account in own.into_iter().chain(touched_by_events) {
        let slot = slot_of(market, account);
        let held = i128::from(slot.locked).min(equity(&slot, mark));
        assert!(held <= requirement(market, &slot, mark), "I19: {slot:?} holds more than IM(W)");
    }
}

/// I20, and the other withdrawal rejects.
fn check_withdrawal_rule(
    before: &EngineSnapshot,
    command: &Command,
    events: &[Event],
    after: &EngineSnapshot,
) {
    let Command::Withdraw(withdraw) = command else { return };
    let (free, locked, reserve) = withdrawal_sums(before, withdraw.account);
    let free_after = free - i128::from(withdraw.amount);
    match events {
        [Event::Reject(r)] if r.reason == RejectReason::WithdrawalReserve => {
            assert!(free_after >= 0 && free_after + locked < reserve, "I20: {withdraw:?} keeps the reserve");
        }
        [Event::Reject(r)] if r.reason == RejectReason::InsufficientBalance => {
            assert!(free_after < 0, "6.5: {withdraw:?} is covered by the free balance");
        }
        [Event::Reject(_)] => {}
        _ => {
            let (free, locked, reserve) = withdrawal_sums(after, withdraw.account);
            assert!(free >= 0 && free + locked >= reserve, "I20: {withdraw:?} left too little behind");
        }
    }
}

/// I14 and I12.
fn check_fills_and_liquidations(before: &EngineSnapshot, events: &[Event], after: &EngineSnapshot) {
    for event in events {
        match *event {
            Event::Fill(fill) => {
                let params = market_of(before, fill.market).expect("a fill is in a market").params;
                let notional = i128::from(fill.price) * i128::from(fill.qty);
                let fee = |rate: i32| ceil_div(notional * i128::from(rate), 1_000_000);
                assert_eq!(i128::from(fill.maker_fee), fee(params.maker_fee_ppm), "I14: {fill:?}");
                assert_eq!(i128::from(fill.taker_fee), fee(params.taker_fee_ppm), "I14: {fill:?}");
                assert!(fill.maker_fee + fill.taker_fee >= Micros::ZERO, "I14: {fill:?}'s fees sum below 0");
            }
            Event::Liquidation(liquidation) => {
                let market = market_of(after, liquidation.market).expect("a liquidation is in a market");
                let slot = slot_of(market, liquidation.account);
                let values = (slot.pos, slot.cost, slot.locked, slot.open_buys, slot.open_sells);
                let empty = (Qty::ZERO, Micros::ZERO, Micros::ZERO, Qty::ZERO, Qty::ZERO);
                assert_eq!(values, empty, "I12: {liquidation:?} left the slot non-empty");
                let none_left =
                    resting_orders(market).all(|order| account_of(order.order_id) != liquidation.account);
                assert!(none_left, "I12: {liquidation:?} left orders resting");
            }
            _ => {}
        }
    }
}
