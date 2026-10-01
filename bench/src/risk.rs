//! Engine states for the risk-layer benchmarks (`docs/RISK.md` 14.4), and the timed loops
//! that run on them.
//!
//! **Contract.** A [`Scenario`] is an `Engine<Book, M>` brought to a known state with
//! ordinary commands only: deposits, leverage, a mark, and orders that cross to open
//! positions. It never reaches into the engine. Every setup command must be accepted, and
//! none may liquidate anything or cancel an order, or the setup panics. The engine is never
//! cloned, because a cloned `Vec` loses its spare capacity (D-010). Instead, each timed loop
//! undoes every command it times before it times the next, so every timed command meets the
//! same state:
//! - a timed place is undone by cancelling the order, and a timed cancel by placing it
//!   again. After each round the timed accounts are exactly as before, apart from their
//!   sequence numbers ([`Scenario::time_places`], [`Scenario::time_cancels`]);
//! - a `SetMark` that liquidates is undone by moving the mark back and opening the same
//!   positions again at the same prices, which gives them the same collateral and so the
//!   same liquidation keys ([`Scenario::time_liquidating_marks`]).
//!
//! **The market** (id 1). Prices 1,000 to 20,000 ticks, and the mark at 10,000, the deep
//! flow's mid (`loadgen::FlowConfig::deep`). Fees of 125 ppm (maker) and 400 ppm (taker),
//! Polymarket's $0 tier. Maximum leverage 10, in one tier. A price band of 44,600 ppm, the
//! widest that band rule 1 allows with these fees (RISK.md 5.3; C12 recommends it for load
//! generation), so the band's edges at the mark are 9,554 and 10,446. Rule 2 needs a
//! `min_price` of at least 201.
//!
//! **The accounts.** Every position is opened by an order that crosses an ask of one maker:
//! - The **maker** (account 1) keeps leverage 1, so it ends up short the whole market with
//!   full collateral. Its key (a short's) is near 19,000, far above any mark set here.
//! - The **population** (accounts from 1,000,000) holds one long of 100 lots each, at
//!   leverage 10. "Far" longs are bought just below the mark, at 400 prices from 9,600 to
//!   9,999 (keys 9,056 to 9,476). For ablation B, 100 "near" longs are bought 201 to 300
//!   ticks above the mark, each at a price of its own (keys 9,689 to 9,793: all different,
//!   and the highest in the market). So a mark at the L-th highest key liquidates exactly
//!   the L near longs bought highest.
//! - 16 **timed accounts** (101 to 116) send the orders the benchmarks time. Each is long
//!   100,000 lots bought at the mark, at leverage 10, and rests `k` bids of 1 lot spread
//!   over the 50 prices from 9,751 to 9,800. Their keys are 9,430 (for `k` = 4,096) to 9,473
//!   (for `k` = 1): among the far keys whatever `k` is, and below every near key. (With the
//!   first index, a `BTreeSet`, where an entry sat changed what re-keying it cost: at the
//!   end of the tree, moving 16 entries back and forth split and merged nodes. The indexed
//!   heap that replaced it (RISK.md 18, P1) cares less, but the timed entries still sit in
//!   the same crowd for every `k`, so that `k` is the only thing that changes.)
//! - 16 **filler accounts** (201 to 216), one beside each timed account, with no position.
//!   They make the book equally deep whatever `k` is (ablation A): the book is filled in
//!   rounds, one bid per timed account in each, and from round `k` on the filler account
//!   rests the bid its timed account doesn't. So every configuration has the same bids in
//!   the same places in the book's order storage, and only their owners differ.
//!
//! Every account but the maker uses leverage 10, the market's maximum, so that every long
//! has a key: a long at 1x never falls below maintenance margin, and isn't indexed (9.1).
//! Every bid rests at or below 9,800, inside the band of every mark set here (the band's
//! upper edge never falls below 10,121), so no mark move sweeps one (5.4).
//!
//! The keys quoted here were worked out with a small integer model of RISK.md 9.1, but
//! nothing relies on them: [`MarketView`] reads every key back from the engine's snapshot,
//! and every timed loop checks what its commands did.
//!
//! **Complexity.** Building a scenario costs about four commands per position (a deposit,
//! the leverage, an IOC buy, a share of the maker's ask): a few seconds for a million
//! positions.

use std::fmt;
use std::hint::black_box;
use std::time::{Duration, Instant};

use engine::book::{Book, OrderBook};
use engine::command::{
    CancelOrder, Command, Deposit, PlaceOrder, SetLeverage, SetMark, SetMarketParams, SetRiskTier,
};
use engine::engine::{Engine, EngineOptions, FUND, MarketSnapshot, SlotSnapshot};
use engine::event::{CancelReason, Event, EventSink, PositionChanged};
use engine::mode::Mode;
use engine::money::{is_liquidatable, liquidation_key};
use engine::types::{AccountId, MarketId, Micros, OrderId, Price, Qty, Side, TimeInForce, order_id};

use crate::{CountingSink, Depth};

/// The benchmarks' market.
pub const MARKET: MarketId = 1;
/// The mark every scenario opens at, and always comes back to.
pub const MARK: Price = 10_000;
/// The market's maximum leverage, and the leverage of every account but the maker.
pub const LEVERAGE: u16 = 10;
/// What every account deposits, and the insurance fund too: $1 billion, far more than any
/// benchmark uses however long it runs. (Each liquidation that ablation B undoes locks a
/// little more of the maker's and the near accounts' free balance.)
pub const DEPOSIT: Micros = 1_000_000_000_000_000;
/// How many timed accounts there are. A timed round sends one command for each, and reads
/// the clock once, so the cost of reading it is shared by 16 commands.
pub const TIMED_ACCOUNTS: usize = 16;

const MAKER: AccountId = 1;
const FIRST_TIMED: AccountId = 101;
const FIRST_FILLER: AccountId = 201;
const FIRST_POPULATION: AccountId = 1_000_000;
/// The size of every population long.
const POPULATION_LOTS: Qty = 100;
/// Far longs are bought at this many prices, from one tick below the mark downwards.
const FAR_PRICES: usize = 400;
/// The price of the lowest near long. Each next one is bought one tick higher.
const NEAR_LOWEST_PRICE: Price = MARK + 201;
/// The size of each timed account's long: large next to its `k` bids, so that the collateral
/// they need barely moves its key.
const TIMED_POSITION: Qty = 100_000;
/// The price of every timed bid, and the highest price of the resting bids: 200 ticks below
/// the mark, so below every ask.
const TIMED_BID_PRICE: Price = MARK - 200;
/// The resting bids are spread over this many prices, from `TIMED_BID_PRICE` downwards.
const RESTING_BID_PRICES: usize = 50;
/// The size of a timed bid. Its top-up, 1,000,000 micros, moves the account's key by about
/// 10 ticks, so every timed place and cancel moves the account's entry in the liquidation
/// index (in the modes that keep one).
const TIMED_LOTS: Qty = 1_000;

// ---------------------------------------------------------------------------------------
// Commands shared with the other risk benchmarks.

/// The parameters of the benchmarks' market (module docs).
pub fn market_params() -> SetMarketParams {
    SetMarketParams {
        min_price: 1_000,
        max_price: 20_000,
        maker_fee_ppm: 125,
        taker_fee_ppm: 400,
        price_band_ppm: 44_600,
        market: MARKET,
        max_leverage: LEVERAGE,
    }
}

/// The commands that open the market, in the order RISK.md 3.3 requires: its parameters, a
/// one-row tier table, and the first mark.
pub fn open_market() -> [Command; 3] {
    let tier = SetRiskTier { lower_bound: 0, market: MARKET, max_leverage: LEVERAGE, index: 0, count: 1 };
    [Command::SetMarketParams(market_params()), Command::SetRiskTier(tier), set_mark(MARK)]
}

/// A deposit of [`DEPOSIT`] into `account`'s free balance (or the insurance fund's).
pub fn deposit(account: AccountId) -> Command {
    Command::Deposit(Deposit { amount: DEPOSIT, account })
}

/// The commands that fund a trading account: a deposit, and leverage 10 in the market.
pub fn fund(account: AccountId) -> [Command; 2] {
    let leverage = SetLeverage { account, market: MARKET, leverage: LEVERAGE };
    [deposit(account), Command::SetLeverage(leverage)]
}

fn set_mark(price: Price) -> Command {
    Command::SetMark(SetMark { price, market: MARKET })
}

// ---------------------------------------------------------------------------------------
// Building a scenario.

/// What a [`Scenario`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScenarioConfig {
    /// Positions in the market in all: the maker's, the timed accounts' and the
    /// population's. At least `near_positions + 17`.
    pub positions: usize,
    /// How many of the population's longs are near longs (ablation B); the rest are far.
    pub near_positions: usize,
    /// Resting orders of each timed account (`k` in ablation A).
    pub orders_per_timed_account: usize,
    /// Resting bids in the book per timed account, its own included: filler accounts rest
    /// the others. At least `orders_per_timed_account`.
    pub book_depth_per_timed_account: usize,
}

/// An engine in the state the module docs describe, and what the timed loops need to keep
/// sending it valid commands.
#[derive(Debug)]
pub struct Scenario<M> {
    pub engine: Engine<Book, M>,
    /// The account of the lowest near long; the others follow it, one account and one tick
    /// higher each.
    first_near: AccountId,
    near_positions: usize,
    /// The last order sequence number used. One counter for every account keeps each
    /// account's sequence numbers rising, as the engine requires (RISK.md 3.1).
    last_seq: u32,
    /// One timed round's bids, and their cancels. Rebuilt in place for each round, so a
    /// round doesn't allocate.
    places: Vec<Command>,
    cancels: Vec<Command>,
}

impl<M: Mode> Scenario<M> {
    /// Builds the scenario with commands, in this order: the market and the insurance fund,
    /// the maker, the far longs, the near longs, the timed accounts' longs, and the book's
    /// bids.
    pub fn build(config: ScenarioConfig) -> Self {
        let named_positions = config.near_positions + TIMED_ACCOUNTS + 1;
        assert!(config.positions >= named_positions, "{config:?}: too few positions");
        assert!(config.book_depth_per_timed_account >= config.orders_per_timed_account, "{config:?}");
        let far_positions = config.positions - named_positions;
        let options = EngineOptions {
            // Every resting bid, a round of timed bids and the maker's one ask at a time.
            order_capacity: TIMED_ACCOUNTS * (config.book_depth_per_timed_account + 1) + 1_024,
            id_hash_seed: 0x5EED,
            scratch_capacity: 4_096,
            // Room for the filler accounts too.
            account_capacity: config.positions + 1_024,
            slot_capacity: config.positions + 1_024,
        };
        let mut scenario = Scenario {
            engine: Engine::new(options),
            first_near: FIRST_POPULATION + far_positions as AccountId,
            near_positions: config.near_positions,
            last_seq: 0,
            places: Vec::with_capacity(TIMED_ACCOUNTS),
            cancels: Vec::with_capacity(TIMED_ACCOUNTS),
        };
        for command in open_market() {
            scenario.run(command);
        }
        scenario.run(deposit(FUND));
        // The maker keeps leverage 1 (no SetLeverage): full collateral for every short.
        scenario.run(deposit(MAKER));
        scenario.open_far_longs(far_positions);
        scenario.open_near_longs();
        scenario.open_timed_longs();
        scenario.rest_bids(config.orders_per_timed_account, config.book_depth_per_timed_account);
        scenario
    }

    /// Applies a setup command, which must be accepted, and must neither liquidate anything
    /// nor cancel an order (`SetupSink`).
    pub fn run(&mut self, command: Command) {
        self.engine.apply(&command, &mut SetupSink);
    }

    /// Opens again the near long of an account that has just been liquidated (its slot is
    /// flat), at its price. The top-up and the fee are the same as the first time, so the
    /// slot gets back exactly the same position, collateral and liquidation key.
    pub fn reopen(&mut self, account: AccountId) {
        let price = self.near_price(account);
        self.buy_from_maker(&[account], POPULATION_LOTS, price);
    }

    /// The price a near account buys its long at.
    fn near_price(&self, account: AccountId) -> Price {
        let near_accounts = self.first_near..self.first_near + self.near_positions as AccountId;
        assert!(near_accounts.contains(&account), "{account} is not a near account");
        NEAR_LOWEST_PRICE + Price::from(account - self.first_near)
    }

    /// The far longs: population account `i` buys at `MARK − 1 − (i mod 400)`. Taken one
    /// price at a time, so that the maker rests one ask per price.
    fn open_far_longs(&mut self, count: usize) {
        for group in 0..FAR_PRICES.min(count) {
            let price = MARK - 1 - group as Price;
            let accounts: Vec<AccountId> =
                (group..count).step_by(FAR_PRICES).map(|i| FIRST_POPULATION + i as AccountId).collect();
            self.fund_all(&accounts);
            self.buy_from_maker(&accounts, POPULATION_LOTS, price);
        }
    }

    /// The near longs, each at a price of its own, one tick apart.
    fn open_near_longs(&mut self) {
        for offset in 0..self.near_positions {
            let account = self.first_near + offset as AccountId;
            self.fund_all(&[account]);
            self.reopen(account);
        }
    }

    /// The timed accounts: each is funded and buys its long at the mark. Bought at the mark,
    /// a long has no unrealized PnL there.
    fn open_timed_longs(&mut self) {
        let accounts: Vec<AccountId> = (0..TIMED_ACCOUNTS).map(timed_account).collect();
        self.fund_all(&accounts);
        self.buy_from_maker(&accounts, TIMED_POSITION, MARK);
    }

    /// The book's resting bids, of 1 lot each, in `depth` rounds of one bid per timed
    /// account, on 50 prices. In the first `k` rounds the timed accounts rest them; after
    /// that, their filler accounts do (module docs). Taking turns spreads each account's
    /// orders through the book's order storage, as in a busy market (the naive open totals
    /// walk them).
    ///
    /// The first bid's top-up also covers the fee the timed account's purchase took from its
    /// slot, so after its `k` bids each timed slot holds exactly its initial margin,
    /// `IM(100,000 + k)`: 1,000 micros per lot at leverage 10.
    fn rest_bids(&mut self, k: usize, depth: usize) {
        if depth > k {
            let fillers: Vec<AccountId> = (0..TIMED_ACCOUNTS).map(filler_account).collect();
            self.fund_all(&fillers);
        }
        for round in 0..depth {
            let price = TIMED_BID_PRICE - (round % RESTING_BID_PRICES) as Price;
            for t in 0..TIMED_ACCOUNTS {
                let owner = if round < k { timed_account(t) } else { filler_account(t) };
                let bid = self.new_order(owner, Side::Buy, price, 1, TimeInForce::Gtc);
                self.run(bid);
            }
        }
    }

    fn fund_all(&mut self, accounts: &[AccountId]) {
        for &account in accounts {
            for command in fund(account) {
                self.run(command);
            }
        }
    }

    /// Opens a long of `lots` at `price` for each of `accounts`. The maker rests one ask for
    /// all of them, and each account takes its share with an IOC buy. Nothing else rests on
    /// the ask side, so every buy fills in full at `price` (an unfilled remainder would be a
    /// cancel, which the setup sink refuses), and the ask side is empty again afterwards.
    fn buy_from_maker(&mut self, accounts: &[AccountId], lots: Qty, price: Price) {
        let total = lots * accounts.len() as Qty;
        let ask = self.new_order(MAKER, Side::Sell, price, total, TimeInForce::Gtc);
        self.run(ask);
        for &account in accounts {
            let buy = self.new_order(account, Side::Buy, price, lots, TimeInForce::Ioc);
            self.run(buy);
        }
    }

    /// A limit order from `account` with the next sequence number.
    fn new_order(
        &mut self,
        account: AccountId,
        side: Side,
        price: Price,
        qty: Qty,
        tif: TimeInForce,
    ) -> Command {
        let order_id = self.next_order_id(account);
        Command::PlaceOrder(PlaceOrder { order_id, price, qty, market: MARKET, side, tif, post_only: false })
    }

    fn next_order_id(&mut self, account: AccountId) -> OrderId {
        self.last_seq = self.last_seq.checked_add(1).expect("the scenario ran out of sequence numbers");
        order_id(account, self.last_seq)
    }
}

/// The `t`-th timed account.
fn timed_account(t: usize) -> AccountId {
    FIRST_TIMED + t as AccountId
}

/// The filler account beside the `t`-th timed account.
fn filler_account(t: usize) -> AccountId {
    FIRST_FILLER + t as AccountId
}

/// The sink for setup commands. None may be rejected or liquidate anything, and none may
/// cancel an order: every IOC buy fills in full, and no mark move sweeps an order.
#[derive(Debug)]
struct SetupSink;

impl EventSink for SetupSink {
    fn emit(&mut self, event: Event) {
        match event {
            Event::Reject(reject) => panic!("a setup command was rejected: {reject:?}"),
            Event::Liquidation(liquidation) => panic!("a setup command liquidated {liquidation:?}"),
            Event::Cancelled(cancelled) => panic!("a setup command cancelled {cancelled:?}"),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------
// Timing orders (ablation A, and ablation B's time per order).

impl<M: Mode> Scenario<M> {
    /// Times `iterations` timed bids: each is placed by a timed account with its `k` resting
    /// orders, is margin-checked (a buy adds to a long), tops up, and rests without filling.
    /// They go in rounds of up to 16, one per timed account, and a round is timed as a whole.
    /// After the clock stops, the round's bids are cancelled, which puts every account back
    /// as it was. Returns the time spent placing.
    pub fn time_places(&mut self, iterations: u64) -> Duration {
        let mut sink = CountingSink::default();
        let mut timed = Duration::ZERO;
        let mut left = iterations;
        while left > 0 {
            let round = self.prepare_round(left);
            let start = Instant::now();
            for place in &self.places {
                self.engine.apply(place, &mut sink);
            }
            timed += start.elapsed();
            for cancel in &self.cancels {
                self.engine.apply(cancel, &mut sink);
            }
            left -= round;
        }
        assert_nothing_went_wrong(&sink);
        timed
    }

    /// Times `iterations` cancels of timed bids, the same way: each round's bids are placed
    /// before the clock starts, and the round's cancels are timed as a whole. Each cancel
    /// releases the bid's top-up. Returns the time spent cancelling.
    pub fn time_cancels(&mut self, iterations: u64) -> Duration {
        let mut sink = CountingSink::default();
        let mut timed = Duration::ZERO;
        let mut left = iterations;
        while left > 0 {
            let round = self.prepare_round(left);
            for place in &self.places {
                self.engine.apply(place, &mut sink);
            }
            let start = Instant::now();
            for cancel in &self.cancels {
                self.engine.apply(cancel, &mut sink);
            }
            timed += start.elapsed();
            left -= round;
        }
        assert_nothing_went_wrong(&sink);
        timed
    }

    /// Fills `places` and `cancels` with the next round: a new bid from each of the first
    /// `min(left, 16)` timed accounts, and its cancel. Returns the round's size.
    fn prepare_round(&mut self, left: u64) -> u64 {
        let round = left.min(TIMED_ACCOUNTS as u64);
        self.places.clear();
        self.cancels.clear();
        for t in 0..round as usize {
            let order_id = self.next_order_id(timed_account(t));
            self.places.push(Command::PlaceOrder(PlaceOrder {
                order_id,
                price: TIMED_BID_PRICE,
                qty: TIMED_LOTS,
                market: MARKET,
                side: Side::Buy,
                tif: TimeInForce::Gtc,
                post_only: false,
            }));
            self.cancels.push(Command::CancelOrder(CancelOrder { order_id, market: MARKET }));
        }
        round
    }

    /// Runs one round of timed bids and their cancels outside any timing, with every event
    /// recorded, and checks that each did what the timed loops rely on. Every place is
    /// accepted with a top-up and nothing else (`Ack`, the free balance, the slot); every
    /// cancel releases the same amount (`Cancelled`, the slot, the free balance), which puts
    /// the account back as it was; the top-up moves the slot's liquidation key; and all 16
    /// timed accounts do exactly the same. Returns what one of them did, for the output.
    pub fn probe_round(&mut self) -> RoundProbe {
        self.prepare_round(TIMED_ACCOUNTS as u64);
        let mut probes = Vec::new();
        for (place, cancel) in self.places.iter().zip(&self.cancels) {
            let placed = events_of(&mut self.engine, place);
            let cancelled = events_of(&mut self.engine, cancel);
            probes.push(RoundProbe::from_events(&placed, &cancelled));
        }
        assert!(probes.windows(2).all(|pair| pair[0] == pair[1]), "the timed accounts differ: {probes:?}");
        probes[0]
    }
}

/// What one timed bid and its cancel did to the account's slot, read from their events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoundProbe {
    /// The collateral the bid moved from the free balance into the slot, and its cancel
    /// moved back.
    pub top_up: Micros,
    /// The slot's liquidation key without the bid, and with it.
    pub key_without: Option<Price>,
    pub key_with: Option<Price>,
}

impl RoundProbe {
    /// Reads a probe from the events of one timed place and of its cancel, and panics
    /// unless they are exactly the ones `probe_round` expects.
    fn from_events(placed: &[Event], cancelled: &[Event]) -> RoundProbe {
        let &[Event::Ack(_), Event::BalanceChanged(free_during), Event::PositionChanged(with)] = placed
        else {
            panic!("a timed place emitted {placed:?}, not an Ack and a top-up")
        };
        let &[Event::Cancelled(cancel), Event::PositionChanged(without), Event::BalanceChanged(free_after)] =
            cancelled
        else {
            panic!("a timed cancel emitted {cancelled:?}, not a Cancelled and a release")
        };
        assert_eq!(cancel.reason, CancelReason::UserRequested);
        let top_up = with.locked - without.locked;
        assert_eq!(
            free_after.free - free_during.free,
            top_up,
            "the cancel released more or less than the top-up"
        );
        let probe = RoundProbe { top_up, key_without: long_key(&without), key_with: long_key(&with) };
        assert!(
            top_up > 0 && probe.key_without != probe.key_with,
            "the top-up didn't move the key: {probe:?}"
        );
        probe
    }
}

/// "a timed bid tops up 1000000 and moves its account's key from 9473 to 9463; its cancel
/// releases the 1000000 and moves the key back".
impl fmt::Display for RoundProbe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key = |key: Option<Price>| key.map_or("none".to_string(), |key| key.to_string());
        write!(
            f,
            "a timed bid tops up {} and moves its account's key from {} to {}; its cancel releases the {} \
             and moves the key back",
            self.top_up,
            key(self.key_without),
            key(self.key_with),
            self.top_up
        )
    }
}

/// The liquidation key of a long, from the slot's state in a `PositionChanged`.
fn long_key(slot: &PositionChanged) -> Option<Price> {
    liquidation_key(slot.position, slot.cost_basis, slot.locked, LEVERAGE).map(|(_, key)| key)
}

/// Applies one command and returns its events. Allocates: outside timing only.
fn events_of<M: Mode>(engine: &mut Engine<Book, M>, command: &Command) -> Vec<Event> {
    let mut events = Vec::new();
    engine.apply(command, &mut events);
    events
}

/// After a timed loop: none of its commands was rejected or liquidated anything.
fn assert_nothing_went_wrong(sink: &CountingSink) {
    assert_eq!(sink.rejects, 0, "a timed command was rejected: {}", sink.reject_reasons);
    assert_eq!(sink.liquidations, 0, "a timed command liquidated something");
}

// ---------------------------------------------------------------------------------------
// Timing marks (ablation B).

impl<M: Mode> Scenario<M> {
    /// Times `iterations` `SetMark`s that liquidate nothing: to `mark` and back to the
    /// scenario's mark, one after the other, each counted as one. All of them are timed
    /// together, since each undoes the one before. Ends at the scenario's mark.
    pub fn time_quiet_marks(&mut self, mark: Price, iterations: u64) -> Duration {
        let (away, back) = (set_mark(mark), set_mark(MARK));
        let mut sink = CountingSink::default();
        let start = Instant::now();
        for i in 0..iterations {
            let command = if i % 2 == 0 { &away } else { &back };
            self.engine.apply(command, &mut sink);
        }
        let timed = start.elapsed();
        if iterations % 2 == 1 {
            self.run(back);
        }
        assert_nothing_went_wrong(&sink);
        assert_eq!(sink.cancels, 0, "a quiet mark swept an order");
        timed
    }

    /// Times `iterations` `SetMark`s to `mark`, each of which must liquidate exactly
    /// `liquidations` near longs. Each is timed on its own, because it has to be undone
    /// before the next: with the clock stopped, the mark goes back to the scenario's mark,
    /// and the liquidated longs are opened again ([`Scenario::reopen`]). So every timed
    /// `SetMark` meets the same positions. Ends at the scenario's mark.
    pub fn time_liquidating_marks(&mut self, mark: Price, liquidations: usize, iterations: u64) -> Duration {
        let (away, back) = (set_mark(mark), set_mark(MARK));
        let mut sink = LiquidationSink::with_room(liquidations);
        let mut timed = Duration::ZERO;
        for _ in 0..iterations {
            sink.liquidated.clear();
            let start = Instant::now();
            self.engine.apply(&away, &mut sink);
            timed += start.elapsed();
            assert_eq!(
                sink.liquidated.len(),
                liquidations,
                "a SetMark to {mark} liquidated {:?}",
                sink.liquidated
            );
            self.run(back);
            for &account in &sink.liquidated {
                self.reopen(account);
            }
        }
        assert_eq!(sink.cancels, 0, "a liquidating mark cancelled an order");
        assert_eq!(sink.shortfalls, 0, "a liquidating mark left the insurance fund short");
        timed
    }
}

/// The sink for the timed `SetMark`s that liquidate. It records which accounts were
/// liquidated, into room reserved up front so that recording doesn't allocate while the
/// clock runs, and counts the events that must not happen.
#[derive(Debug)]
struct LiquidationSink {
    liquidated: Vec<AccountId>,
    cancels: u64,
    shortfalls: u64,
}

impl LiquidationSink {
    fn with_room(liquidations: usize) -> Self {
        LiquidationSink { liquidated: Vec::with_capacity(liquidations), cancels: 0, shortfalls: 0 }
    }
}

impl EventSink for LiquidationSink {
    fn emit(&mut self, event: Event) {
        match black_box(event) {
            Event::Liquidation(liquidation) => self.liquidated.push(liquidation.account),
            Event::Cancelled(_) => self.cancels += 1,
            Event::InsuranceShortfall(_) => self.shortfalls += 1,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------
// Counting what a market holds.

/// The benchmarks' market, as it is in an engine's snapshot: for counting what a scenario
/// holds, and for finding the marks ablation B sets. A snapshot walks everything and
/// allocates, so this is for outside timing only.
#[derive(Debug)]
pub struct MarketView {
    market: MarketSnapshot,
}

impl MarketView {
    pub fn of<B: OrderBook, M: Mode>(engine: &Engine<B, M>) -> MarketView {
        let snapshot = engine.snapshot();
        let market = snapshot.markets.into_iter().find(|market| market.params.market == MARKET);
        MarketView { market: market.expect("the benchmarks' market exists") }
    }

    /// Every slot in the market, flat ones included. The naive `SetMark` reads each of them.
    pub fn slots(&self) -> usize {
        self.market.slots.len()
    }

    /// Slots with a position. (The insurance fund has no slot.)
    pub fn positions(&self) -> usize {
        self.market.slots.iter().filter(|slot| slot.pos != 0).count()
    }

    /// Slots with a liquidation key: the entries of the liquidation index, in the modes that
    /// keep one.
    pub fn keyed_positions(&self) -> usize {
        self.market.slots.iter().filter(|slot| self.key(slot).is_some()).count()
    }

    pub fn resting_orders(&self) -> usize {
        self.market.book.bids.len() + self.market.book.asks.len()
    }

    pub fn depth(&self) -> Depth {
        Depth::of_snapshot(&self.market.book)
    }

    /// Every long's liquidation key, highest first.
    pub fn long_keys(&self) -> Vec<Price> {
        let mut keys: Vec<Price> = self
            .market
            .slots
            .iter()
            .filter_map(|slot| match self.key(slot) {
                Some((Side::Buy, key)) => Some(key),
                _ => None,
            })
            .collect();
        keys.sort_unstable_by(|a, b| b.cmp(a));
        keys
    }

    /// How many slots are below maintenance margin at `mark`: the direct check (RISK.md 4.5),
    /// independent of the keys.
    pub fn liquidatable_at(&self, mark: Price) -> usize {
        let max_leverage = self.market.params.max_leverage;
        let slots = self.market.slots.iter();
        slots.filter(|slot| is_liquidatable(slot.pos, slot.cost, slot.locked, mark, max_leverage)).count()
    }

    /// A mark at which exactly `count` slots are liquidatable, all of them longs: one tick
    /// above the highest long key for 0, and the `count`-th highest key otherwise (a long is
    /// crossed exactly when its key is at or above the mark, RISK.md 9.1). Panics if the
    /// `count`-th key and the next are equal, since no mark then crosses exactly `count`,
    /// or if the direct check on every slot disagrees.
    pub fn mark_liquidating(&self, count: usize) -> Price {
        let keys = self.long_keys();
        let mark = if count == 0 {
            keys[0] + 1
        } else {
            let exact = keys.get(count).is_none_or(|&next| next < keys[count - 1]);
            assert!(exact, "the {count}th and the next highest long keys are equal");
            keys[count - 1]
        };
        assert_eq!(self.liquidatable_at(mark), count, "the direct check disagrees with the keys at {mark}");
        mark
    }

    fn key(&self, slot: &SlotSnapshot) -> Option<(Side, Price)> {
        liquidation_key(slot.pos, slot.cost, slot.locked, self.market.params.max_leverage)
    }
}

/// "10,000 positions (10,000 with a liquidation key) in 10,000 slots, 4,096 resting orders".
impl fmt::Display for MarketView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} positions ({} with a liquidation key) in {} slots, {} resting orders",
            self.positions(),
            self.keyed_positions(),
            self.slots(),
            self.resting_orders()
        )?;
        if self.market.fund_pos != 0 {
            write!(f, ", and the insurance fund holds {} lots", self.market.fund_pos)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::engine::EngineSnapshot;
    use engine::mode::{Fast, NaiveLiquidation, NaiveTotals};

    /// Small enough for a debug build: 483 far longs, 100 near longs, 16 timed accounts with
    /// 3 resting bids each and 2 more from each filler account, and the maker.
    const SMALL: ScenarioConfig = ScenarioConfig {
        positions: 600,
        near_positions: 100,
        orders_per_timed_account: 3,
        book_depth_per_timed_account: 5,
    };

    #[test]
    fn a_scenario_holds_the_positions_and_orders_its_config_asks_for() {
        fn check<M: Mode>() {
            let scenario = Scenario::<M>::build(SMALL);
            let view = MarketView::of(&scenario.engine);
            assert_eq!((view.positions(), view.keyed_positions()), (600, 600));
            // The 16 filler accounts have slots, but no position.
            assert_eq!((view.slots(), view.resting_orders()), (616, 16 * 5));
            scenario.engine.assert_invariants();

            // Without filler accounts.
            let config = ScenarioConfig { book_depth_per_timed_account: 3, ..SMALL };
            let view = MarketView::of(&Scenario::<M>::build(config).engine);
            assert_eq!((view.slots(), view.resting_orders()), (600, 16 * 3));
        }
        check::<Fast>();
        check::<NaiveTotals>();
        check::<NaiveLiquidation>();
    }

    /// The accounts' free balances, and everything about the market: what a timed loop
    /// must leave as it found it. (The accounts' next sequence numbers do move on.)
    fn state_without_sequences(snapshot: EngineSnapshot) -> (Vec<(AccountId, Micros)>, Vec<MarketSnapshot>) {
        let free = snapshot.accounts.iter().map(|account| (account.account, account.free)).collect();
        (free, snapshot.markets)
    }

    #[test]
    fn a_timed_bid_tops_up_and_moves_the_key_and_timed_rounds_leave_the_state_as_it_was() {
        fn check<M: Mode>() {
            let mut scenario = Scenario::<M>::build(SMALL);
            let probe = scenario.probe_round();
            // IM(1,000 lots) at leverage 10; the key of a long of 100,000 holding IM(100,003).
            assert_eq!(probe.top_up, 1_000_000);
            assert_eq!((probe.key_without, probe.key_with), (Some(9_473), Some(9_463)));

            let before = state_without_sequences(scenario.engine.snapshot());
            // 40 is not a multiple of 16, so the last round of each loop is a partial one.
            scenario.time_places(40);
            scenario.time_cancels(40);
            assert_eq!(state_without_sequences(scenario.engine.snapshot()), before);
            scenario.engine.assert_invariants();
        }
        check::<Fast>();
        check::<NaiveTotals>();
        check::<NaiveLiquidation>();
    }

    #[test]
    fn marks_liquidate_exactly_the_highest_near_longs_and_reopening_restores_them() {
        fn check<M: Mode>() {
            let mut scenario = Scenario::<M>::build(SMALL);
            let view = MarketView::of(&scenario.engine);
            let keys = view.long_keys();
            // The near keys, 9,793 down to 9,689, are the 100 highest.
            assert_eq!((keys[0], keys[99]), (9_793, 9_689));
            assert!(keys[100] < 9_689);

            for liquidations in [1, 100] {
                let mark = view.mark_liquidating(liquidations);
                scenario.time_liquidating_marks(mark, liquidations, 3);
            }
            // 5 is odd, so the loop ends with an untimed move back.
            scenario.time_quiet_marks(view.mark_liquidating(0), 5);

            let after = MarketView::of(&scenario.engine);
            assert_eq!(after.market.mark, Some(MARK));
            assert_eq!(after.long_keys(), keys, "reopening gave the longs other keys");
            // The fund now holds what it took over: 3 × (1 + 100) longs of 100 lots.
            assert_eq!(after.market.fund_pos, 3 * 101 * 100);
            scenario.engine.assert_invariants();
        }
        check::<Fast>();
        check::<NaiveLiquidation>();
    }
}
