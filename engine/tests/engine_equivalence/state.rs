//! Looking things up in an [`EngineSnapshot`]: the generator reads the state to aim its
//! commands, and the checker and the coverage table read it before and after each one.
//!
//! **Contract.** Plain lookups, no formulas. An account or slot the engine hasn't created
//! reads as the engine treats it: free balance 0, next sequence 0, and an empty slot at
//! leverage 1.

use engine::book::RestingOrder;
use engine::engine::{EngineSnapshot, MarketSnapshot, SlotSnapshot};
use engine::types::{AccountId, MarketId, Micros, OrderId, Qty};

/// The market, if `SetMarketParams` has created it.
pub fn market_of(state: &EngineSnapshot, market: MarketId) -> Option<&MarketSnapshot> {
    state.markets.iter().find(|m| m.params.market == market)
}

/// The account's slot in the market, or the empty slot a first order is checked against.
pub fn slot_of(market: &MarketSnapshot, account: AccountId) -> SlotSnapshot {
    market.slots.iter().find(|slot| slot.account == account).copied().unwrap_or(empty_slot(account))
}

/// A slot the engine hasn't created: flat, no collateral, no orders, leverage 1.
pub fn empty_slot(account: AccountId) -> SlotSnapshot {
    SlotSnapshot {
        account,
        pos: Qty::ZERO,
        cost: Micros::ZERO,
        locked: Micros::ZERO,
        leverage: 1,
        open_buys: Qty::ZERO,
        open_sells: Qty::ZERO,
    }
}

pub fn free_of(state: &EngineSnapshot, account: AccountId) -> Micros {
    state.accounts.iter().find(|a| a.account == account).map_or(Micros::ZERO, |a| a.free)
}

/// The lowest order sequence number the account may use next.
pub fn next_seq_of(state: &EngineSnapshot, account: AccountId) -> u64 {
    state.accounts.iter().find(|a| a.account == account).map_or(0, |a| a.next_seq)
}

/// Every order resting in the market's book, bids first.
pub fn resting_orders(market: &MarketSnapshot) -> impl Iterator<Item = &RestingOrder> {
    market.book.bids.iter().chain(&market.book.asks)
}

/// The order, if it rests in the market's book.
pub fn resting_order(state: &EngineSnapshot, market: MarketId, order_id: OrderId) -> Option<RestingOrder> {
    let market = market_of(state, market)?;
    resting_orders(market).find(|order| order.order_id == order_id).copied()
}
