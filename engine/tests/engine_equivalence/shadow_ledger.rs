//! The shadow ledger of `docs/RISK.md` 14.3: every balance and position, rebuilt from the
//! event stream alone. `PositionChanged` gives a slot's position, cost basis and collateral
//! (the fund's position and cost basis, for `FUND`); `BalanceChanged` gives a free balance
//! (the fund's balance, for `FUND`); `Fill` gives the fees; and the commands give the net
//! deposits.
//!
//! **Contract.** [`ShadowLedger::apply`] walks one command's events block by block
//! ([`split_into_blocks`]) and checks, after every block, that positions sum to zero in
//! every market (I2) and that money is conserved exactly (I3). Inside a block one side of a
//! transfer has been reported and the other not yet (RISK.md 12), so the check waits for
//! its end. [`ShadowLedger::assert_matches`] then checks that the ledger equals the engine's
//! snapshot. Together they show that the event stream alone rebuilds every balance and
//! position, which is what M3's replay and private feeds need.
//!
//! **Blocks** (RISK.md 11): a top-up is the free balance then the slot; a fill is the
//! `Fill` then the maker's and the taker's positions; a release is the slot then the free
//! balance; a liquidation is `Liquidation`, the slot, `InsuranceAbsorb`, the fund's position
//! and the fund's balance. Every other event is a block on its own. The splitter asserts
//! each block's shape, so a money event out of place fails here.

use std::collections::BTreeMap;

use engine::command::Command;
use engine::engine::{EngineSnapshot, FUND};
use engine::event::{Event, Fill};
use engine::types::{AccountId, MarketId, Qty, account_of};

/// One block of events. See the module docs.
#[derive(Clone, Copy, Debug)]
pub struct Block<'a> {
    pub kind: BlockKind,
    pub events: &'a [Event],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    TopUp {
        account: AccountId,
        market: MarketId,
    },
    Fill(Fill),
    Release {
        account: AccountId,
        market: MarketId,
    },
    Liquidation {
        account: AccountId,
        market: MarketId,
    },
    /// Any other event, alone.
    Single,
}

/// Splits one command's events into blocks, asserting each money block's shape.
pub fn split_into_blocks(events: &[Event]) -> Vec<Block<'_>> {
    let mut blocks = Vec::new();
    let mut rest = events;
    while !rest.is_empty() {
        let (kind, len) = match *rest {
            [Event::Fill(fill), Event::PositionChanged(maker), Event::PositionChanged(taker), ..] => {
                let parties = (maker.account, taker.account);
                assert_eq!(parties, (account_of(fill.maker_order), account_of(fill.taker_order)), "{fill:?}");
                (BlockKind::Fill(fill), 3)
            }
            [Event::Fill(fill), ..] => {
                panic!("{fill:?} is not followed by the maker's and taker's positions")
            }
            [
                Event::Liquidation(liquidation),
                Event::PositionChanged(slot),
                Event::InsuranceAbsorb(_),
                Event::PositionChanged(fund),
                Event::BalanceChanged(fund_balance),
                ..,
            ] => {
                assert_eq!(slot.account, liquidation.account, "{liquidation:?}");
                assert_eq!((fund.account, fund_balance.account), (FUND, FUND), "{liquidation:?}");
                (BlockKind::Liquidation { account: liquidation.account, market: liquidation.market }, 5)
            }
            [Event::Liquidation(liquidation), ..] => {
                panic!("{liquidation:?} is not followed by the takeover")
            }
            [Event::BalanceChanged(free), Event::PositionChanged(slot), ..]
                if free.account == slot.account && free.account != FUND =>
            {
                (BlockKind::TopUp { account: slot.account, market: slot.market }, 2)
            }
            [Event::PositionChanged(slot), Event::BalanceChanged(free), ..]
                if free.account == slot.account && free.account != FUND =>
            {
                (BlockKind::Release { account: slot.account, market: slot.market }, 2)
            }
            _ => (BlockKind::Single, 1),
        };
        let (block, remaining) = rest.split_at(len);
        blocks.push(Block { kind, events: block });
        rest = remaining;
    }
    blocks
}

/// See the module docs. Every money value is `i128`, so a sum can't overflow.
#[derive(Debug, Default)]
pub struct ShadowLedger {
    free: BTreeMap<AccountId, i128>,
    fund_balance: i128,
    /// `(pos, cost, locked)` per `(market, account)`.
    slots: BTreeMap<(MarketId, AccountId), (Qty, i128, i128)>,
    /// The fund's `(pos, cost)` per market.
    fund: BTreeMap<MarketId, (Qty, i128)>,
    fees: BTreeMap<MarketId, i128>,
    net_deposits: i128,
}

impl ShadowLedger {
    /// Applies one command and its events, checking I2 and I3 after every block.
    pub fn apply(&mut self, command: &Command, events: &[Event]) {
        let accepted = !matches!(events, [Event::Reject(_)]);
        match *command {
            Command::Deposit(deposit) if accepted => self.net_deposits += i128::from(deposit.amount),
            Command::Withdraw(withdraw) if accepted => self.net_deposits -= i128::from(withdraw.amount),
            _ => {}
        }
        for block in split_into_blocks(events) {
            for event in block.events {
                self.apply_event(event);
            }
            self.assert_positions_sum_to_zero(command);
            self.assert_money_is_conserved(command);
        }
    }

    fn apply_event(&mut self, event: &Event) {
        match *event {
            Event::PositionChanged(p) if p.account == FUND => {
                assert_eq!(p.locked, 0, "the fund's position carries no collateral: {p:?}");
                self.fund.insert(p.market, (p.position, p.cost_basis.into()));
            }
            Event::PositionChanged(p) => {
                self.slots.insert((p.market, p.account), (p.position, p.cost_basis.into(), p.locked.into()));
            }
            Event::BalanceChanged(b) if b.account == FUND => self.fund_balance = b.free.into(),
            Event::BalanceChanged(b) => {
                self.free.insert(b.account, b.free.into());
            }
            Event::Fill(fill) => {
                *self.fees.entry(fill.market).or_default() +=
                    i128::from(fill.maker_fee) + i128::from(fill.taker_fee);
            }
            _ => {}
        }
    }

    /// I2: in every market, the positions and the fund's position sum to zero.
    fn assert_positions_sum_to_zero(&self, command: &Command) {
        let mut sums: BTreeMap<MarketId, i128> = BTreeMap::new();
        for (&(market, _), &(pos, _, _)) in &self.slots {
            *sums.entry(market).or_default() += i128::from(pos);
        }
        for (&market, &(pos, _)) in &self.fund {
            *sums.entry(market).or_default() += i128::from(pos);
        }
        for (market, sum) in sums {
            assert_eq!(sum, 0, "I2: positions in market {market} sum to {sum} during {command:?}");
        }
    }

    /// I3: `Σ free + Σ locked + fund balance + Σ fees − Σ cost − Σ fund cost = net deposits`.
    fn assert_money_is_conserved(&self, command: &Command) {
        let slots: i128 = self.slots.values().map(|&(_, cost, locked)| locked - cost).sum();
        let fund_cost: i128 = self.fund.values().map(|&(_, cost)| cost).sum();
        let total =
            self.free.values().sum::<i128>() + slots + self.fund_balance + self.fees.values().sum::<i128>()
                - fund_cost;
        assert_eq!(total, self.net_deposits, "I3: money is not conserved during {command:?}");
    }

    /// The ledger equals the engine's snapshot: every free balance, the fund's balance, every
    /// slot's position, cost basis and collateral, the fund's positions, the fees and the net
    /// deposits. Accounts and slots created without an event (by `SetLeverage`) hold zeros.
    pub fn assert_matches(&self, snapshot: &EngineSnapshot) {
        let free = |account| self.free.get(&account).copied().unwrap_or(0);
        for account in &snapshot.accounts {
            assert_eq!(
                i128::from(account.free),
                free(account.account),
                "free balance of {}",
                account.account
            );
        }
        for account in self.free.keys() {
            assert!(
                snapshot.accounts.iter().any(|a| a.account == *account),
                "account {account} doesn't exist"
            );
        }
        assert_eq!(i128::from(snapshot.fund_balance), self.fund_balance, "the fund's balance");
        assert_eq!(snapshot.net_deposits, self.net_deposits, "net deposits");

        let mut slots_seen = 0;
        for market in &snapshot.markets {
            let id = market.params.market;
            let fund = self.fund.get(&id).copied().unwrap_or((0, 0));
            assert_eq!((market.fund_pos, i128::from(market.fund_cost)), fund, "the fund's position in {id}");
            let fees = self.fees.get(&id).copied().unwrap_or(0);
            assert_eq!(i128::from(market.fees_collected), fees, "fees collected in market {id}");
            for slot in &market.slots {
                let expected = self.slots.get(&(id, slot.account)).copied().unwrap_or((0, 0, 0));
                slots_seen += usize::from(self.slots.contains_key(&(id, slot.account)));
                let actual = (slot.pos, i128::from(slot.cost), i128::from(slot.locked));
                assert_eq!(actual, expected, "slot of account {} in market {id}", slot.account);
            }
        }
        assert_eq!(slots_seen, self.slots.len(), "the events reported a slot the engine doesn't have");
    }
}
