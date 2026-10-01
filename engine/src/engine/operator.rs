//! The operator's commands: `Deposit`, `Withdraw`, `SetLeverage`, `SetMark`,
//! `SetMarketParams` and `SetRiskTier` (`docs/RISK.md` 6.4 to 6.9).
//!
//! **Contract.** As for the client commands: the checks run in RISK.md's order, the first
//! failure is the single `Reject`, and a reject changes nothing. A rejected market-level
//! command (`SetMark`, `SetMarketParams`, `SetRiskTier`) carries `AccountId::MAX` as its
//! account, which consumers read as "the operator".
//!
//! **Complexity.** O(1), except `Withdraw`, which loops over the markets (about 88) to sum
//! the account's collateral and open notional, and `SetMarketParams`, which builds a new
//! book. Neither is on the hot path.

use crate::book::{BookConfig, BookOptions, MAX_LEVELS, OrderBook};
use crate::command::{Deposit, SetLeverage, SetMark, SetMarketParams, SetRiskTier, Withdraw};
use crate::event::{Event, EventSink, LeverageSet, MarkPrice, RejectReason};
use crate::id_hash::IdBuildHasher;
use crate::mode::Mode;
use crate::money::{
    MAX_TIERS, PRICE_LIMIT, band_edges, band_rule_1_holds, band_rule_2_holds, initial_margin,
    withdrawal_reserve,
};
use crate::state::{Account, Market};
use crate::types::{AccountId, Micros, Qty};

use super::{Engine, FUND, NO_ORDER, OPERATOR, balance_event, reject};

impl<B: OrderBook, M: Mode> Engine<B, M> {
    // -----------------------------------------------------------------------------------
    // Deposit (RISK.md 6.4).

    pub(super) fn deposit(&mut self, deposit: &Deposit, events: &mut impl EventSink) {
        let new_balance = match self.check_deposit(deposit) {
            Ok(balance) => balance,
            Err(reason) => return reject(events, NO_ORDER, deposit.account, reason),
        };
        self.start_command();
        self.net_deposits += i128::from(deposit.amount);
        if deposit.account == FUND {
            // The only way money reaches the insurance fund from outside.
            self.fund_balance = new_balance;
            events.emit(balance_event(FUND, new_balance));
            self.report_shortfall(events);
        } else {
            self.accounts.entry(deposit.account).or_insert(Account::NEW).free = new_balance;
            events.emit(balance_event(deposit.account, new_balance));
        }
    }

    /// Check 1 of RISK.md 6.4: an amount of at least 1 that doesn't overflow the balance
    /// (the account's free balance, or the fund's). Returns the new balance.
    fn check_deposit(&self, deposit: &Deposit) -> Result<Micros, RejectReason> {
        let balance =
            if deposit.account == FUND { self.fund_balance } else { self.free_balance(deposit.account) };
        if deposit.amount < Micros::new(1) {
            return Err(RejectReason::InvalidAmount);
        }
        balance.checked_add(deposit.amount).ok_or(RejectReason::InvalidAmount)
    }

    // -----------------------------------------------------------------------------------
    // Withdraw (RISK.md 6.5).

    pub(super) fn withdraw(&mut self, withdraw: &Withdraw, events: &mut impl EventSink) {
        let free_after = match self.check_withdraw(withdraw) {
            Ok(free) => free,
            Err(reason) => return reject(events, NO_ORDER, withdraw.account, reason),
        };
        self.start_command();
        self.account_mut(withdraw.account).free = free_after;
        self.net_deposits -= i128::from(withdraw.amount);
        events.emit(balance_event(withdraw.account, free_after));
    }

    /// Checks 1 to 4 of RISK.md 6.5. Afterwards the free balance must be non-negative, and
    /// free plus locked collateral (Polymarket's "existing collateral reservations": each
    /// slot's IM is held in it) at least 10% of the open notional at mark. Unrealized PnL
    /// doesn't count, either way. Returns the free balance after the withdrawal.
    fn check_withdraw(&self, withdraw: &Withdraw) -> Result<Micros, RejectReason> {
        if withdraw.account == FUND {
            return Err(RejectReason::ReservedAccount);
        }
        if withdraw.amount < Micros::new(1) {
            return Err(RejectReason::InvalidAmount);
        }
        // Can't overflow: the free balance is at least 0 and the amount at least 1.
        let free_after = self.free_balance(withdraw.account) - withdraw.amount;
        if free_after < Micros::ZERO {
            return Err(RejectReason::InsufficientBalance);
        }
        let (locked, open_notional) = self.locked_and_open_notional(withdraw.account);
        if i128::from(free_after) + locked < withdrawal_reserve(open_notional) {
            return Err(RejectReason::WithdrawalReserve);
        }
        Ok(free_after)
    }

    /// The account's locked collateral and its open notional at mark (`|pos| × mark`),
    /// summed over its slots in every market, in market order. O(number of markets).
    fn locked_and_open_notional(&self, account: AccountId) -> (i128, i128) {
        let (mut locked, mut open_notional) = (0, 0);
        for market in self.markets.iter().flatten() {
            let Some(slot) = market.find_slot(account) else { continue };
            locked += i128::from(slot.locked);
            if slot.pos != Qty::ZERO {
                let mark = market.mark.expect("a market where someone holds a position has a mark");
                open_notional += i128::from(slot.pos.abs()) * i128::from(mark);
            }
        }
        (locked, open_notional)
    }

    // -----------------------------------------------------------------------------------
    // SetLeverage (RISK.md 6.6).

    pub(super) fn set_leverage(&mut self, command: &SetLeverage, events: &mut impl EventSink) {
        let (market_id, account) = (command.market, command.account);
        let margined = match self.check_set_leverage(command) {
            Ok(margined) => margined,
            Err(reason) => return reject(events, NO_ORDER, account, reason),
        };
        self.start_command();
        self.accounts.entry(account).or_insert(Account::NEW);
        self.market_mut(market_id).slot_or_create(account).leverage = command.leverage;
        events.emit(Event::LeverageSet(LeverageSet {
            account,
            market: market_id,
            leverage: command.leverage,
        }));

        // A slot with orders or a position is margined again at the new leverage, like an
        // order: topped up now, or released by the pass if the requirement fell. Never a
        // margin call, so with the same leverage this works as "add margin".
        if let Some(top_up) = margined {
            self.touch(market_id, account);
            if let Some(top_up) = self.move_to_slot(market_id, account, top_up) {
                top_up.emit(events);
            }
            self.run_post_command_pass(market_id, events);
        }
    }

    /// Checks 1 to 4 of RISK.md 6.6. Returns `None` if the slot has nothing to margin (no
    /// slot yet, or `W = 0`), or else the top-up the new leverage needs (0 if none).
    fn check_set_leverage(&self, command: &SetLeverage) -> Result<Option<Micros>, RejectReason> {
        let market = self.find_market(command.market).ok_or(RejectReason::UnknownMarket)?;
        if command.account == FUND {
            return Err(RejectReason::ReservedAccount);
        }
        if !(1..=market.params.max_leverage).contains(&command.leverage) {
            return Err(RejectReason::InvalidLeverage);
        }
        let Some(slot) = market.find_slot(command.account) else { return Ok(None) };
        let size = Self::worst_case_size_of(market, command.account, slot);
        if size == Qty::ZERO {
            return Ok(None);
        }
        let mark = market.mark.expect("a slot with orders or a position is in a market with a mark");
        let requirement = initial_margin(size, mark, command.leverage, market.live_tiers());
        let need = slot.money().top_up_needed(mark, requirement);
        if need <= self.free_balance(command.account) {
            Ok(Some(need))
        } else {
            Err(RejectReason::InsufficientMargin)
        }
    }

    // -----------------------------------------------------------------------------------
    // SetMark (RISK.md 6.7).

    pub(super) fn set_mark(&mut self, command: &SetMark, events: &mut impl EventSink) {
        if let Err(reason) = self.check_set_mark(command) {
            return reject(events, NO_ORDER, OPERATOR, reason);
        }
        let market_id = command.market;
        self.start_command();
        let market = self.market_mut(market_id);
        let edges = band_edges(command.price, market.params.price_band_ppm);
        let previous_mark = market.mark.replace(command.price);
        market.upper = edges.upper;
        market.lower = edges.lower;
        events.emit(Event::MarkPrice(MarkPrice { price: command.price, market: market_id }));

        self.revalue_fund_position(market_id);
        // Longs the mark fell through, then shorts it rose through (9.4). Before the sweep,
        // so that a liquidated account's orders go as `Liquidation`, not `PriceBand`.
        self.liquidate_crossed_slots(market_id, previous_mark, events);
        self.sweep_out_of_band_orders(market_id, events);
        // Over the owners of swept orders only: it releases and re-keys, and never
        // liquidates, because cancels don't change equity and the walk has just removed
        // every slot below maintenance margin. `SetMark` never visits any other slot.
        self.run_post_command_pass(market_id, events);
        self.report_shortfall(events);
    }

    /// Checks 1 to 3 of RISK.md 6.7. A market needs a committed tier table before its first
    /// mark, so no order is ever checked against a missing table.
    fn check_set_mark(&self, command: &SetMark) -> Result<(), RejectReason> {
        let market = self.find_market(command.market).ok_or(RejectReason::UnknownMarket)?;
        if market.tier_count == 0 {
            return Err(RejectReason::NoRiskTiers);
        }
        if !market.price_in_range(command.price) {
            return Err(RejectReason::InvalidPrice);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------
    // SetMarketParams (RISK.md 6.8).

    pub(super) fn set_market_params(&mut self, params: &SetMarketParams, events: &mut impl EventSink) {
        if let Err(reason) = self.check_market_params(params) {
            return reject(events, NO_ORDER, OPERATOR, reason);
        }
        self.start_command();
        let config =
            BookConfig { market: params.market, min_price: params.min_price, max_price: params.max_price };
        let book_options = BookOptions {
            order_capacity: self.options.order_capacity,
            id_hash_seed: self.options.id_hash_seed,
        };
        let book = B::with_config(config, book_options);

        let index = params.market.index();
        if self.markets.len() <= index {
            self.markets.resize_with(index + 1, || None);
        }
        if let Some(market) = self.markets[index].as_mut() {
            market.reconfigure(*params, book);
        } else {
            let hasher = IdBuildHasher::new(self.options.id_hash_seed);
            self.markets[index] = Some(Market::new(*params, book, self.options.slot_capacity, hasher));
        }
        events.emit(Event::MarketParamsSet(*params));
    }

    /// Checks 1 and 2 of RISK.md 6.8.
    fn check_market_params(&self, params: &SetMarketParams) -> Result<(), RejectReason> {
        if !market_params_are_valid(params) {
            return Err(RejectReason::InvalidParams);
        }
        if self.find_market(params.market).is_some_and(|market| !market.has_no_orders_or_positions()) {
            return Err(RejectReason::MarketNotEmpty);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------
    // SetRiskTier (RISK.md 6.9).

    pub(super) fn set_risk_tier(&mut self, row: &SetRiskTier, events: &mut impl EventSink) {
        if let Err(reason) = self.check_risk_tier(row) {
            return reject(events, NO_ORDER, OPERATOR, reason);
        }
        self.start_command();
        // Committing a table on a live market is allowed: tiers change only IM, not MM or
        // any liquidation key, so no slot needs visiting. Each slot meets the new table at
        // its next check or release.
        self.market_mut(row.market).stage_tier_row(row);
        events.emit(Event::RiskTierSet(*row));
    }

    /// Checks 1 and 2 of RISK.md 6.9.
    fn check_risk_tier(&self, row: &SetRiskTier) -> Result<(), RejectReason> {
        let market = self.find_market(row.market).ok_or(RejectReason::UnknownMarket)?;
        if tier_row_is_valid(row, market) { Ok(()) } else { Err(RejectReason::InvalidParams) }
    }
}

/// Check 1 of RISK.md 6.8, every part in `i128` after widening each field: a price range
/// the book can hold (`1 <= min <= max < 2^32`, at most 2^24 ticks wide), a maximum
/// leverage of at least 1, a non-negative taker fee, non-negative fees in total (so a fill's
/// net fee is never negative and `fees_collected` never falls), and both band rules (5.3),
/// with the larger fee rate. The conditions are joined with `&&`, so the band rules only
/// see a valid range and leverage.
fn market_params_are_valid(params: &SetMarketParams) -> bool {
    let (min, max) = (i128::from(params.min_price), i128::from(params.max_price));
    let ticks = max - min + 1;
    let valid_range = 1 <= min && min <= max && max < i128::from(PRICE_LIMIT) && ticks <= MAX_LEVELS as i128;
    let (maker, taker) = (params.maker_fee_ppm, params.taker_fee_ppm);
    let valid_fees = taker >= 0 && i128::from(maker) + i128::from(taker) >= 0;
    let fee = maker.max(taker);
    valid_range
        && params.max_leverage >= 1
        && valid_fees
        && band_rule_1_holds(params.max_leverage, params.price_band_ppm, fee)
        && band_rule_2_holds(params.min_price, params.max_leverage, params.price_band_ppm, fee)
}

/// Check 2 of RISK.md 6.9. A table has 1 to 8 rows, sent in order; its leverage never
/// exceeds the market's maximum. Row 0 starts at a lower bound of 0. Every later row
/// continues the table being staged (the next index, the same `count`), with a higher bound
/// and no higher leverage than the row before. So a committed table always starts at 0,
/// with bounds rising and leverage never rising.
fn tier_row_is_valid<B>(row: &SetRiskTier, market: &Market<B>) -> bool {
    let (index, count) = (usize::from(row.index), usize::from(row.count));
    if !(1..=MAX_TIERS).contains(&count) || index >= count {
        return false;
    }
    if !(1..=market.params.max_leverage).contains(&row.max_leverage) {
        return false;
    }
    if index == 0 {
        return row.lower_bound == Micros::ZERO;
    }
    let previous = market.staged[index - 1];
    row.index == market.staged_rows
        && row.count == market.staged_count
        && row.lower_bound > previous.lower_bound
        && row.max_leverage <= previous.max_leverage
}
