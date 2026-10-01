//! A deterministic flow of engine commands for the pipeline's integration tests
//! (`engine_semantics.rs`, `pipeline_e2e.rs`), and a small random-number generator.
//!
//! **Why not the M3 smoke flow.** PIPELINE.md 18.1 pins `ENGINE_SEMANTICS` on "the first
//! 100,000 commands of the smoke flow", but that flow lives in `loadgen`, which depends on
//! `pipeline`: a `pipeline` test can't use it (see PIPELINE.md 22). This flow is small and
//! self-contained, and is shaped to reach every part of the engine the journal must
//! reproduce: fills, IOC remainders, self-trade and price-band cancels, modifies of every
//! kind, rejects, deposits and withdrawals, leverage changes, and marks that jump far
//! enough to liquidate 20x positions, with an insurance fund of $1, so the first loss past
//! a bankruptcy price is a shortfall.
//!
//! **The flow.** Four markets (a 2% band around a mark that starts at 100,000 ticks, 20x at
//! most, fees of 100 and 400 ppm), the fund's $1, and 48 accounts with $10,000 each; every
//! fourth account uses 20x in every market, the others 1x to 10x. Then, per command: 36%
//! resting orders within 150 ticks of the mark on their own side, 10% IOCs through the mark
//! by up to 800 ticks, 4% post-only orders at the mark, 18% cancels and 17% modifies of one
//! of the account's recent orders (which may have filled: the engine rejects those), 8%
//! marks (a walk of up to 150 ticks, and one in 30 a jump of 6%), 7% deposits, withdrawals
//! and leverage changes. It is open loop: nothing depends on the engine's answers, as for
//! the real load generator (section 14).

#![allow(dead_code)] // each test binary uses its own part of this module

use engine::command::{
    CancelOrder, Command, Deposit, ModifyOrder, PlaceOrder, SetLeverage, SetMark, SetMarketParams,
    SetRiskTier, Withdraw,
};
use engine::engine::{EngineOptions, FUND};
use engine::types::{
    AccountId, MarketId, Micros, OrderId, OrderSeq, Price, Qty, Side, TimeInForce, order_id,
};

/// A tiny deterministic generator (xorshift64), so the tests need no dependency.
#[derive(Clone, Debug)]
pub struct XorShift(pub u64);

impl XorShift {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A number in `lo..=hi`.
    pub fn between(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    /// True with probability `percent` / 100.
    pub fn percent(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// How many markets and accounts the flow has: markets 1 to 4, accounts 1 to 48.
pub const MARKETS: u16 = 4;
pub const ACCOUNTS: u32 = 48;
const START_MARK: Price = Price::new(100_000);
/// Orders an account remembers for its cancels and modifies.
const RECENT: usize = 8;

/// The engine options the tests use: room well above what the flow needs.
pub fn engine_options(seed: u64) -> EngineOptions {
    EngineOptions {
        order_capacity: 4_096,
        id_hash_seed: seed,
        scratch_capacity: 4_096,
        account_capacity: 1_024,
        slot_capacity: 1_024,
    }
}

/// The flow's generator (module docs).
#[derive(Clone, Debug)]
pub struct TestFlow {
    random: XorShift,
    /// The next order sequence of each account (index 0 unused).
    next_order: Vec<u32>,
    /// Each account's most recent orders, oldest first.
    recent: Vec<Vec<(OrderId, MarketId)>>,
    /// The flow's own idea of each market's mark (index 0 unused).
    marks: Vec<Price>,
}

impl TestFlow {
    pub fn new(seed: u64) -> TestFlow {
        TestFlow {
            random: XorShift(seed | 1),
            next_order: vec![1; ACCOUNTS as usize + 1],
            recent: vec![Vec::new(); ACCOUNTS as usize + 1],
            marks: vec![START_MARK; MARKETS as usize + 1],
        }
    }

    /// The markets, the fund, and every account's deposit and leverage.
    pub fn setup(&mut self) -> Vec<Command> {
        let mut commands = Vec::new();
        for market in (1..=MARKETS).map(MarketId::new) {
            commands.push(Command::SetMarketParams(SetMarketParams {
                min_price: Price::new(1_000),
                max_price: Price::new(1_000_000),
                maker_fee_ppm: 100,
                taker_fee_ppm: 400,
                price_band_ppm: 20_000,
                market,
                max_leverage: 20,
            }));
            commands.push(Command::SetRiskTier(SetRiskTier {
                lower_bound: Micros::ZERO,
                market,
                max_leverage: 20,
                index: 0,
                count: 1,
            }));
            commands.push(Command::SetMark(SetMark { price: START_MARK, market }));
        }
        commands.push(Command::Deposit(Deposit { amount: Micros::new(1_000_000), account: FUND }));
        for account in (1..=ACCOUNTS).map(AccountId::new) {
            commands.push(Command::Deposit(Deposit { amount: Micros::new(10_000_000_000), account }));
            for market in (1..=MARKETS).map(MarketId::new) {
                let leverage =
                    if account.get().is_multiple_of(4) { 20 } else { self.random.between(1, 10) as u16 };
                commands.push(Command::SetLeverage(SetLeverage { account, market, leverage }));
            }
        }
        commands
    }

    /// The next command of the timed flow.
    pub fn next_command(&mut self) -> Command {
        let account = AccountId::new(1 + self.random.below(u64::from(ACCOUNTS)) as u32);
        let market = MarketId::new(1 + self.random.below(u64::from(MARKETS)) as u16);
        let mark = self.marks[market.index()];
        match self.random.below(100) {
            0..36 => {
                let side = self.side();
                let price = match side {
                    Side::Buy => mark - Price::new(self.random.between(1, 150)),
                    Side::Sell => mark + Price::new(self.random.between(1, 150)),
                };
                self.place(account, market, side, price, TimeInForce::Gtc, false)
            }
            36..46 => {
                let side = self.side();
                let price = match side {
                    Side::Buy => mark + Price::new(self.random.between(0, 800)),
                    Side::Sell => mark - Price::new(self.random.between(0, 800)),
                };
                self.place(account, market, side, price, TimeInForce::Ioc, false)
            }
            46..50 => {
                let side = self.side();
                self.place(account, market, side, mark, TimeInForce::Gtc, true)
            }
            50..68 => {
                let (order_id, market) = self.recent_order(account, market);
                Command::CancelOrder(CancelOrder { order_id, market })
            }
            68..85 => {
                let (order_id, market) = self.recent_order(account, market);
                let mark = self.marks[market.index()];
                let new_price = mark + Price::new(self.random.between(-200, 200));
                let new_size = Qty::new(self.random.between(0, 3_000));
                Command::ModifyOrder(ModifyOrder { order_id, new_price, new_size, market })
            }
            85..93 => self.set_mark(market),
            93..96 => Command::Deposit(Deposit {
                amount: Micros::new(self.random.between(1, 5_000_000_000)),
                account,
            }),
            96..98 => Command::Withdraw(Withdraw {
                amount: Micros::new(self.random.between(1, 3_000_000_000)),
                account,
            }),
            _ => {
                let leverage =
                    if account.get().is_multiple_of(4) { 20 } else { self.random.between(1, 12) as u16 };
                Command::SetLeverage(SetLeverage { account, market, leverage })
            }
        }
    }

    /// The setup, then `timed` commands of the flow.
    pub fn commands(seed: u64, timed: usize) -> Vec<Command> {
        let mut flow = TestFlow::new(seed);
        let mut commands = flow.setup();
        commands.extend((0..timed).map(|_| flow.next_command()));
        commands
    }

    fn side(&mut self) -> Side {
        if self.random.percent(50) { Side::Buy } else { Side::Sell }
    }

    fn place(
        &mut self,
        account: AccountId,
        market: MarketId,
        side: Side,
        price: Price,
        tif: TimeInForce,
        post_only: bool,
    ) -> Command {
        let sequence = self.next_order[account.index()];
        self.next_order[account.index()] += 1;
        let id = order_id(account, OrderSeq::new(sequence));
        if tif == TimeInForce::Gtc {
            let recent = &mut self.recent[account.index()];
            if recent.len() == RECENT {
                recent.remove(0);
            }
            recent.push((id, market));
        }
        let qty = Qty::new(self.random.between(1, if tif == TimeInForce::Ioc { 3_000 } else { 2_000 }));
        Command::PlaceOrder(PlaceOrder { order_id: id, price, qty, market, side, tif, post_only })
    }

    /// One of the account's recent orders, or, if it has none, an id it never used.
    fn recent_order(&mut self, account: AccountId, market: MarketId) -> (OrderId, MarketId) {
        let recent = &self.recent[account.index()];
        if recent.is_empty() {
            return (order_id(account, OrderSeq::new(u32::MAX)), market);
        }
        recent[self.random.below(recent.len() as u64) as usize]
    }

    /// A walk of up to 150 ticks, or, one time in 30, a jump of 6%; kept within 50,000 to
    /// 200,000.
    fn set_mark(&mut self, market: MarketId) -> Command {
        let mark = &mut self.marks[market.index()];
        let step = if self.random.below(30) == 0 {
            let jump = mark.ticks() * 6 / 100;
            if self.random.percent(50) { jump } else { -jump }
        } else {
            self.random.between(-150, 150)
        };
        *mark = (*mark + Price::new(step)).clamp(Price::new(50_000), Price::new(200_000));
        Command::SetMark(SetMark { price: *mark, market })
    }
}

/// True for the commands a client sends (place, cancel, modify).
pub fn is_client(command: &Command) -> bool {
    matches!(command, Command::PlaceOrder(_) | Command::CancelOrder(_) | Command::ModifyOrder(_))
}

/// The account that owns a client command's order.
pub fn client_account(command: &Command) -> AccountId {
    let order_id = match command {
        Command::PlaceOrder(place) => place.order_id,
        Command::CancelOrder(cancel) => cancel.order_id,
        Command::ModifyOrder(modify) => modify.order_id,
        other => panic!("{other:?} is not a client command"),
    };
    engine::types::account_of(order_id)
}
