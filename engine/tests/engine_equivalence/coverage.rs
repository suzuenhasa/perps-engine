//! The coverage table of `docs/RISK.md` 14.3: how often each rare rule ran over the whole
//! property run. The test prints it and fails if any row is below its minimum, so that a
//! change to the generator (or to the engine) can't quietly stop testing a rule.
//!
//! **Contract.** [`Coverage::record`] reads the snapshot before a command, the command and
//! its events, and counts; it checks nothing. Each minimum is about a quarter of the lowest
//! count seen in runs of 256 scenarios, so a run with other random scenarios clears it with
//! a wide margin. Two slots with the same liquidation key are recognised with the engine's
//! own `liquidation_key`: this module only counts, so it needn't be independent.

use std::collections::BTreeMap;

use engine::command::{Command, ModifyOrder, PlaceOrder};
use engine::engine::{EngineSnapshot, FUND};
use engine::event::{CancelReason, Event, RejectReason};
use engine::types::{MarketId, Micros, Qty, Side, TimeInForce, account_of};

use super::checker::{in_margin_call, margin_request, tier_row};
use super::shadow_ledger::{BlockKind, split_into_blocks};
use super::state::{market_of, slot_of};

/// Every reject reason: each must occur (RISK.md 14.3, "each reject reason").
const REJECT_REASONS: [RejectReason; 19] = [
    RejectReason::InvalidPrice,
    RejectReason::InvalidQty,
    RejectReason::Duplicate,
    RejectReason::UnknownOrder,
    RejectReason::PostOnlyWouldCross,
    RejectReason::UnknownMarket,
    RejectReason::NoMark,
    RejectReason::PriceBand,
    RejectReason::MarginCall,
    RejectReason::InsufficientMargin,
    RejectReason::InsufficientBalance,
    RejectReason::MarketNotEmpty,
    RejectReason::InvalidLeverage,
    RejectReason::ReservedAccount,
    RejectReason::SizeLimit,
    RejectReason::InvalidAmount,
    RejectReason::InvalidParams,
    RejectReason::WithdrawalReserve,
    RejectReason::NoRiskTiers,
];

/// The rows of RISK.md 14.3 other than the reject reasons, with their minimums.
const ROWS: [(&str, u64); 21] = [
    ("fill", 1_000),
    ("top-up (place or replace)", 2_000),
    ("release", 2_000),
    ("strictly reducing order accepted in margin call", 60),
    ("pass liquidation of the own slot", 10),
    ("pass liquidation of a maker", 6),
    ("SetMark liquidation of a long", 60),
    ("SetMark liquidation of a short", 60),
    ("SetMark liquidations with the same key", 30),
    ("swept bid", 150),
    ("swept ask", 100),
    ("shortfall rising", 20),
    ("shortfall back to 0", 6),
    ("fund netting: reduce", 12),
    ("fund netting: flip", 8),
    ("tier crossing", 80),
    ("tier commit on a live market", 100),
    ("IOC top-up released", 400),
    ("SetLeverage top-up", 130),
    ("SetLeverage release", 120),
    ("SetLeverage reject (InsufficientMargin)", 15),
];

/// Minimum count of each reject reason. The rarest, `WithdrawalReserve`, counts about 100.
const REJECT_MINIMUM: u64 = 20;

/// Counts per row. See the module docs.
#[derive(Debug, Default)]
pub struct Coverage {
    counts: BTreeMap<String, u64>,
    commands: u64,
    scenarios: u64,
}

impl Coverage {
    fn count(&mut self, row: &str) {
        *self.counts.entry(row.to_string()).or_default() += 1;
    }

    pub fn start_scenario(&mut self) {
        self.scenarios += 1;
    }

    /// Counts what one command did.
    pub fn record(&mut self, before: &EngineSnapshot, command: &Command, events: &[Event]) {
        self.commands += 1;
        if let [Event::Reject(reject)] = events {
            self.count(&format!("reject: {:?}", reject.reason));
            if matches!(command, Command::SetLeverage(_)) && reject.reason == RejectReason::InsufficientMargin
            {
                self.count("SetLeverage reject (InsufficientMargin)");
            }
            return;
        }
        self.record_collateral(command, events);
        self.record_margin_checks(before, command);
        self.record_liquidations(before, command, events);
        self.record_fund(before, events);
        for event in events {
            match event {
                Event::Fill(_) => self.count("fill"),
                Event::Cancelled(c) if c.reason == CancelReason::PriceBand => {
                    self.count(if c.side == Side::Buy { "swept bid" } else { "swept ask" });
                }
                _ => {}
            }
        }
        if let Command::SetRiskTier(row) = command {
            let market = market_of(before, row.market).expect("an accepted row is for a market");
            let book = &market.book;
            let live = !book.bids.is_empty() || !book.asks.is_empty() || market.nonzero_positions > 0;
            if row.index + 1 == row.count && live {
                self.count("tier commit on a live market");
            }
        }
    }

    /// Top-ups and releases, by command.
    fn record_collateral(&mut self, command: &Command, events: &[Event]) {
        let blocks = split_into_blocks(events);
        let top_up = blocks.iter().any(|b| matches!(b.kind, BlockKind::TopUp { .. }));
        let releases = blocks.iter().filter(|b| matches!(b.kind, BlockKind::Release { .. })).count();
        for _ in 0..releases {
            self.count("release");
        }
        match command {
            Command::SetLeverage(_) if top_up => self.count("SetLeverage top-up"),
            Command::SetLeverage(_) if releases > 0 => self.count("SetLeverage release"),
            Command::PlaceOrder(_) | Command::ModifyOrder(_) if top_up => {
                self.count("top-up (place or replace)")
            }
            _ => {}
        }
        // An IOC that was topped up, left a remainder, and had its own slot released.
        if let Command::PlaceOrder(order) = command {
            let owner = account_of(order.order_id);
            let remainder = events
                .iter()
                .any(|e| matches!(e, Event::Cancelled(c) if c.reason == CancelReason::IocRemainder));
            let released = blocks
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Release { account, .. } if account == owner));
            if order.tif == TimeInForce::Ioc && top_up && remainder && released {
                self.count("IOC top-up released");
            }
        }
    }

    /// An accepted place or replace that was strictly reducing in margin call, or that took
    /// the slot's worst-case notional into a higher tier.
    fn record_margin_checks(&mut self, before: &EngineSnapshot, command: &Command) {
        if !matches!(command, Command::PlaceOrder(_) | Command::ModifyOrder(_)) {
            return;
        }
        let Some(request) = margin_request(before, command) else { return };
        if request.strictly_reducing && in_margin_call(&request) {
            self.count("strictly reducing order accepted in margin call");
        }
        let slot = request.slot;
        let size_now = (slot.pos + slot.open_buys).abs().max((slot.pos - slot.open_sells).abs());
        let mark = i128::from(request.mark);
        let tier_now = tier_row(&request.tiers, i128::from(size_now) * mark);
        if !request.strictly_reducing && tier_row(&request.tiers, request.size * mark) > tier_now {
            self.count("tier crossing");
        }
    }

    /// Liquidations in the post-command pass, and in the `SetMark` walk, where two slots
    /// with the same key mean the `(key, AccountId)` tie rule decided their order.
    fn record_liquidations(&mut self, before: &EngineSnapshot, command: &Command, events: &[Event]) {
        let liquidated = events.iter().filter_map(|event| match event {
            Event::Liquidation(l) => Some(*l),
            _ => None,
        });
        match *command {
            Command::PlaceOrder(PlaceOrder { order_id, .. })
            | Command::ModifyOrder(ModifyOrder { order_id, .. }) => {
                for liquidation in liquidated {
                    let row =
                        if liquidation.account == account_of(order_id) { "the own slot" } else { "a maker" };
                    self.count(&format!("pass liquidation of {row}"));
                }
            }
            Command::SetMark(set) => {
                let market = market_of(before, set.market).expect("an accepted mark is for a market");
                let mut keys = Vec::new();
                for liquidation in liquidated {
                    let side = if liquidation.position > Qty::ZERO { "long" } else { "short" };
                    self.count(&format!("SetMark liquidation of a {side}"));
                    let slot = slot_of(market, liquidation.account);
                    keys.push(slot.money().liquidation_key(market.params.max_leverage));
                }
                keys.sort_unstable_by_key(|key| key.map(|(side, price)| (side as u8, price)));
                if keys.windows(2).any(|pair| pair[0] == pair[1]) {
                    self.count("SetMark liquidations with the same key");
                }
            }
            _ => {}
        }
    }

    /// The shortfall report, and how each absorb netted into the fund's position.
    fn record_fund(&mut self, before: &EngineSnapshot, events: &[Event]) {
        let mut fund_positions: BTreeMap<MarketId, Qty> =
            before.markets.iter().map(|m| (m.params.market, m.fund_pos)).collect();
        for event in events {
            match *event {
                Event::InsuranceShortfall(shortfall) => {
                    if shortfall.uncovered > before.last_reported_uncovered {
                        self.count("shortfall rising");
                    }
                    if shortfall.uncovered == Micros::ZERO {
                        self.count("shortfall back to 0");
                    }
                }
                Event::InsuranceAbsorb(absorb) => {
                    let fund = fund_positions.get(&absorb.market).copied().unwrap_or(Qty::ZERO);
                    let opposite = fund.lots().signum() != absorb.position.lots().signum();
                    if fund != Qty::ZERO && absorb.position != Qty::ZERO && opposite {
                        let kind = if absorb.position.abs() <= fund.abs() { "reduce" } else { "flip" };
                        self.count(&format!("fund netting: {kind}"));
                    }
                }
                Event::PositionChanged(p) if p.account == FUND => {
                    fund_positions.insert(p.market, p.position);
                }
                _ => {}
            }
        }
    }

    /// Prints the table: every row with its count and minimum.
    pub fn print(&self) {
        println!("\nCoverage over {} scenarios, {} commands:", self.scenarios, self.commands);
        for (row, minimum) in self.rows() {
            println!("  {row:<50} {:>7}   (minimum {minimum})", self.get(&row));
        }
    }

    /// Panics, naming each row that is below its minimum.
    pub fn assert_minimums(&self) {
        let short: Vec<String> = self
            .rows()
            .into_iter()
            .filter(|(row, minimum)| self.get(row) < *minimum)
            .map(|(row, minimum)| format!("{row}: {} < {minimum}", self.get(&row)))
            .collect();
        assert!(short.is_empty(), "coverage rows below their minimum: {short:#?}");
    }

    fn rows(&self) -> Vec<(String, u64)> {
        let rows = ROWS.iter().map(|&(row, minimum)| (row.to_string(), minimum));
        rows.chain(REJECT_REASONS.iter().map(|reason| (format!("reject: {reason:?}"), REJECT_MINIMUM)))
            .collect()
    }

    fn get(&self, row: &str) -> u64 {
        self.counts.get(row).copied().unwrap_or(0)
    }
}
