//! A busy flow of engine commands for the allocation test (`docs/RISK.md` 14.4,
//! "Allocation"), and the script it becomes: the warm-up, the measured commands, and the
//! events the measured commands must produce.
//!
//! **The flow.** One 20x market around a mark of 100,000, `accounts` accounts with $10,000
//! each and leverage 1 to 5. Out of 1,000 commands: 450 places (four in five passive, within
//! 100 ticks of the mark; the rest through the mark by up to 2,000 ticks, so they trade and
//! any GTC remainder rests near the band's edge), 30 malformed or duplicate places, 140
//! cancels (20 of an unknown id), 280 modifies of every kind, 40 marks (three in four within
//! 0.3% of 100,000, the rest within 3%), 15 deposits, 15 withdrawals and 30 leverage changes.
//! With leverage at most 5, a position is liquidated only after a move of about 17%, so no
//! mark in this flow liquidates anything: in the modes without the index, `SetMark` scans
//! every slot, finds none crossed, and allocates nothing. The 3% marks still sweep the
//! orders resting around an earlier mark out of the band, and release their owners.
//!
//! **Warm-up.** The setup creates the market, the fund and every account's deposit and slot
//! (`SetLeverage` creates the slot), then 20 commands per account of the flow build the book
//! and the positions. So no map insert or `market.accounts` push is left for the measured
//! part, and the liquidation index already holds a key for most slots with a position.
//!
//! The script is generated on an `Engine<Book, NaiveLiquidation>`, which allocates freely
//! while generating (it takes snapshots); the measured engines are fresh ones that replay it.

use engine::book::{Book, RestingOrder};
use engine::command::{
    CancelOrder, Command, Deposit, ModifyOrder, PlaceOrder, SetLeverage, SetMark, SetMarketParams,
    SetRiskTier, Withdraw,
};
use engine::engine::{Engine, EngineOptions, FUND};
use engine::event::{CancelReason, Event};
use engine::mode::NaiveLiquidation;
use engine::types::{AccountId, MarketId, OrderId, Price, Side, TimeInForce, order_id};

use super::{DiscardingSink, Random};

const MARKET: MarketId = 1;
const MID: Price = 100_000;
const PARAMS: SetMarketParams = SetMarketParams {
    min_price: 1_000,
    max_price: 300_000,
    maker_fee_ppm: 125,
    taker_fee_ppm: 400,
    price_band_ppm: 22_100,
    market: MARKET,
    max_leverage: 20,
};
/// Commands of the flow per account in the warm-up.
const WARM_UP_PER_ACCOUNT: usize = 20;
pub const MEASURED_COMMANDS: usize = 10_000;

/// Room reserved in the engines that run the script: well above what the flow uses.
pub fn options() -> EngineOptions {
    EngineOptions {
        order_capacity: 4_096,
        id_hash_seed: 0x5EED,
        scratch_capacity: 4_096,
        account_capacity: 1_024,
        slot_capacity: 1_024,
    }
}

/// The warm-up, the measured commands, and the events the measured commands produced when
/// the script was generated.
#[derive(Debug)]
pub struct Script {
    pub warm_up: Vec<Command>,
    pub measured: Vec<Command>,
    pub expected: Vec<Event>,
}

/// Generates the script for a flow over `accounts` accounts (module docs).
pub fn script(accounts: u32) -> Script {
    let mut engine: Engine<Book, NaiveLiquidation> = Engine::new(options());
    let mut flow = Flow { random: Random(0x9E37_79B9_7F4A_7C15), accounts, seq: 0 };
    let mut warm_up = flow.setup();
    for command in &warm_up {
        engine.apply(command, &mut DiscardingSink);
    }
    for _ in 0..WARM_UP_PER_ACCOUNT * accounts as usize {
        let command = flow.next(&engine);
        engine.apply(&command, &mut DiscardingSink);
        warm_up.push(command);
    }
    let (mut measured, mut expected) = (Vec::new(), Vec::new());
    for _ in 0..MEASURED_COMMANDS {
        let command = flow.next(&engine);
        engine.apply(&command, &mut expected);
        measured.push(command);
    }
    Script { warm_up, measured, expected }
}

impl Script {
    /// Panics unless the measured commands took the paths the test is meant to cover, and
    /// liquidated nothing (module docs).
    pub fn assert_covers_the_flow(&self) {
        let count = |is: &dyn Fn(&Event) -> bool| self.expected.iter().filter(|e| is(e)).count();
        let cancelled = |reason| count(&|e| matches!(e, Event::Cancelled(c) if c.reason == reason));
        assert!(count(&|e| matches!(e, Event::Fill(_))) > 100, "fills");
        assert!(cancelled(CancelReason::PriceBand) > 0, "orders swept by a mark");
        assert!(cancelled(CancelReason::SelfTrade) > 0, "self-trade cancels");
        assert!(cancelled(CancelReason::IocRemainder) > 0, "IOC remainders");
        assert!(cancelled(CancelReason::SizeBelowFilled) > 0, "removals by modify");
        assert!(count(&|e| matches!(e, Event::Modified(_))) > 0, "modifies");
        assert!(count(&|e| matches!(e, Event::LeverageSet(_))) > 0, "leverage changes");
        assert!(count(&|e| matches!(e, Event::Reject(_))) > 0, "rejects");
        // Top-ups (free balance, then slot) and releases (slot, then free balance).
        let pairs = |first: fn(&Event) -> bool, second: fn(&Event) -> bool| {
            self.expected.windows(2).filter(|pair| first(&pair[0]) && second(&pair[1])).count()
        };
        let top_ups = pairs(is_balance_change, is_slot_change);
        let releases = pairs(is_slot_change, is_balance_change);
        assert!(top_ups > 100 && releases > 100, "top-ups and releases");
        assert_eq!(count(&|e| matches!(e, Event::Liquidation(_))), 0, "the flow liquidates nothing");
    }
}

fn is_slot_change(event: &Event) -> bool {
    matches!(event, Event::PositionChanged(p) if p.account != FUND)
}

fn is_balance_change(event: &Event) -> bool {
    matches!(event, Event::BalanceChanged(b) if b.account != FUND)
}

/// Generates the flow's commands, choosing cancels and modifies from the engine's resting
/// orders.
#[derive(Debug)]
struct Flow {
    random: Random,
    accounts: u32,
    /// The last sequence number used. One counter for all accounts keeps every id unique and
    /// each account's sequence rising.
    seq: u32,
}

impl Flow {
    /// The market, the fund, and every account with its deposit and slot.
    fn setup(&mut self) -> Vec<Command> {
        let mut commands = vec![
            Command::SetMarketParams(PARAMS),
            Command::SetRiskTier(SetRiskTier {
                lower_bound: 0,
                market: MARKET,
                max_leverage: 20,
                index: 0,
                count: 1,
            }),
            Command::SetMark(SetMark { price: MID, market: MARKET }),
            Command::Deposit(Deposit { amount: 1_000_000_000_000, account: FUND }),
        ];
        for account in 1..=self.accounts {
            commands.push(Command::Deposit(Deposit { amount: 10_000_000_000, account }));
            commands.push(self.set_leverage(account));
        }
        commands
    }

    fn account(&mut self) -> AccountId {
        1 + self.random.below(u64::from(self.accounts)) as AccountId
    }

    fn set_leverage(&mut self, account: AccountId) -> Command {
        let leverage = self.random.between(1, 5) as u16;
        Command::SetLeverage(SetLeverage { account, market: MARKET, leverage })
    }

    fn new_id(&mut self, account: AccountId) -> OrderId {
        self.seq += 1;
        order_id(account, self.seq)
    }

    /// A new order: four in five passive, within 100 ticks of the mark on its own side; the
    /// rest through the mark by up to 2,000 ticks (the band is 2,210 at 100,000). 15% IOC,
    /// 10% post-only.
    fn new_order(&mut self, mark: Price) -> PlaceOrder {
        let account = self.account();
        let side = if self.random.percent(50) { Side::Buy } else { Side::Sell };
        let distance = if self.random.percent(80) {
            -self.random.between(1, 100)
        } else {
            self.random.between(0, 2_000)
        };
        let price = match side {
            Side::Buy => mark + distance,
            Side::Sell => mark - distance,
        };
        PlaceOrder {
            order_id: self.new_id(account),
            price,
            qty: self.random.between(1, 50),
            market: MARKET,
            side,
            tif: if self.random.percent(15) { TimeInForce::Ioc } else { TimeInForce::Gtc },
            post_only: self.random.percent(10),
        }
    }

    fn pick(&mut self, orders: &[RestingOrder]) -> RestingOrder {
        orders[self.random.below(orders.len() as u64) as usize]
    }

    /// The next command (module docs, "The flow").
    fn next(&mut self, engine: &Engine<Book, NaiveLiquidation>) -> Command {
        let state = engine.snapshot();
        let market = &state.markets[0];
        let mark = market.mark.expect("the setup set a mark");
        let resting: Vec<RestingOrder> = market.book.bids.iter().chain(&market.book.asks).copied().collect();
        let roll = self.random.below(1_000);
        match roll {
            _ if resting.is_empty() && roll < 900 => Command::PlaceOrder(self.new_order(mark)),
            0..450 => Command::PlaceOrder(self.new_order(mark)),
            450..480 => {
                let order = self.new_order(mark);
                let malformed = match self.random.below(3) {
                    0 => PlaceOrder { qty: 0, ..order },
                    1 => PlaceOrder { price: PARAMS.max_price + 1, ..order },
                    _ => PlaceOrder { order_id: self.pick(&resting).order_id, ..order },
                };
                Command::PlaceOrder(malformed)
            }
            480..600 => {
                Command::CancelOrder(CancelOrder { order_id: self.pick(&resting).order_id, market: MARKET })
            }
            600..620 => Command::CancelOrder(CancelOrder { order_id: order_id(1, u32::MAX), market: MARKET }),
            620..900 => {
                let best =
                    (market.book.bids.first().map(|o| o.price), market.book.asks.first().map(|o| o.price));
                self.modify(&resting, mark, best)
            }
            900..940 => {
                // Mostly within 0.3% of 100,000, and one in four within 3%: far enough to sweep
                // orders placed around an earlier mark out of the band.
                let spread = if self.random.percent(75) { 300 } else { 3_000 };
                Command::SetMark(SetMark {
                    price: MID + self.random.between(-spread, spread),
                    market: MARKET,
                })
            }
            940..955 => {
                let account = self.account();
                Command::Deposit(Deposit { amount: self.random.between(1, 1_000_000), account })
            }
            955..970 => {
                let account = self.account();
                Command::Withdraw(Withdraw { amount: self.random.between(1, 1_000_000), account })
            }
            _ => {
                let account = self.account();
                self.set_leverage(account)
            }
        }
    }

    /// A modify of one of the book flow's kinds, with sizes as total sizes (D-008).
    fn modify(
        &mut self,
        resting: &[RestingOrder],
        mark: Price,
        (best_bid, best_ask): (Option<Price>, Option<Price>),
    ) -> Command {
        let order = self.pick(resting);
        let total = order.filled + order.qty;
        let (id, price, size) = match self.random.below(8) {
            // At or below what has filled: removed (an unfilled order gets size 0: rejected).
            0 => (order.order_id, order.price, (order.filled - self.random.between(0, 2)).max(0)),
            1 => (order.order_id, order.price, order.filled + (order.qty - self.random.between(1, 5)).max(1)),
            2 => (order.order_id, order.price, total),
            3 => (order.order_id, order.price, total + self.random.between(1, 5)),
            4 => {
                let distance = self.random.between(1, 100);
                let price = if order.side == Side::Buy { mark - distance } else { mark + distance };
                (order.order_id, price, total)
            }
            5 => {
                // At the best opposite price: trades, or, if post-only, is rejected.
                let crossing = if order.side == Side::Buy { best_ask } else { best_bid };
                (order.order_id, crossing.unwrap_or(order.price), total)
            }
            6 => (order.order_id, PARAMS.max_price + 1, total),
            _ => (order_id(1, u32::MAX), order.price, total),
        };
        Command::ModifyOrder(ModifyOrder { order_id: id, new_price: price, new_size: size, market: MARKET })
    }
}
