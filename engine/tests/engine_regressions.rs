//! Named regression tests for the engine. Each one pins a bug or a review finding that was
//! fixed, so that it can't come back. A failing case from the property test
//! (`engine_equivalence.rs`) also ends up here, shrunk and given a name, as for the book
//! (`book_regressions.rs`, D-009). Regressions about allocation are in `no_alloc.rs`, which
//! counts allocations.

use engine::book::Book;
use engine::command::{
    CancelOrder, Command, Deposit, PlaceOrder, SetLeverage, SetMark, SetMarketParams, SetRiskTier,
};
use engine::engine::{Engine, EngineOptions, EngineSnapshot, FUND};
use engine::event::Event;
use engine::mode::{Fast, NaiveLiquidation};
use engine::money::liquidation_key;
use engine::types::{AccountId, MarketId, Price, Side, TimeInForce, order_id};

const MARKET: MarketId = 1;
const MID: Price = 100_000;
const MAX_LEVERAGE: u16 = 20;
/// Traders 1 to 300 open longs, 301 to 600 open shorts.
const TRADERS_PER_SIDE: AccountId = 300;
/// The counterparty of every trader, at leverage 1.
const MAKER: AccountId = 1_000;

/// Every command of the scenario below, on `Engine<Book, Fast>` and on
/// `Engine<Book, NaiveLiquidation>`, which differ only in the liquidation index.
#[derive(Debug)]
struct TwoEngines {
    fast: Engine<Book, Fast>,
    scan: Engine<Book, NaiveLiquidation>,
    liquidations: usize,
    ties: usize,
}

impl TwoEngines {
    fn new() -> Self {
        TwoEngines {
            fast: Engine::new(EngineOptions::default()),
            scan: Engine::new(EngineOptions::default()),
            liquidations: 0,
            ties: 0,
        }
    }

    /// Applies `command` to both engines, which must emit the same events. Returns them.
    fn apply(&mut self, command: Command) -> Vec<Event> {
        let (mut fast, mut scan) = (Vec::new(), Vec::new());
        self.fast.apply(&command, &mut fast);
        self.scan.apply(&command, &mut scan);
        assert_eq!(fast, scan, "{command:?}");
        assert!(!matches!(fast.as_slice(), [Event::Reject(_)]), "{command:?} was rejected");
        fast
    }

    /// Both engines must hold the same state and keep their invariants, which in `Fast`
    /// includes the index's heaps being in order and exact (I10).
    fn assert_same_state(&self) {
        assert!(self.fast.snapshot() == self.scan.snapshot(), "the states differ");
        self.fast.assert_invariants();
        self.scan.assert_invariants();
    }

    /// Applies a `SetMark`, and checks that it liquidated in the walk's order (RISK.md 9.4):
    /// longs by key from the highest, then shorts by key from the lowest, ties to the lower
    /// account. The keys come from the state before the mark.
    fn set_mark(&mut self, price: Price) {
        let before = self.fast.snapshot();
        let events = self.apply(Command::SetMark(SetMark { price, market: MARKET }));
        let walked: Vec<(Side, Price, AccountId)> = events
            .iter()
            .filter_map(|event| match event {
                Event::Liquidation(liquidation) => Some(key_before(&before, liquidation.account)),
                _ => None,
            })
            .collect();
        let mut in_walk_order = walked.clone();
        in_walk_order.sort_by_key(|&(side, key, account)| match side {
            Side::Buy => (0, -key, account),
            Side::Sell => (1, key, account),
        });
        assert_eq!(walked, in_walk_order, "SetMark {price}");
        self.liquidations += walked.len();
        self.ties +=
            walked.windows(2).filter(|pair| (pair[0].0, pair[0].1) == (pair[1].0, pair[1].1)).count();
        self.assert_same_state();
    }
}

/// The side and liquidation key of `account`'s slot in `state`.
fn key_before(state: &EngineSnapshot, account: AccountId) -> (Side, Price, AccountId) {
    let slot = state.markets[0].slots.iter().find(|slot| slot.account == account).expect("a slot");
    let (side, key) =
        liquidation_key(slot.pos, slot.cost, slot.locked, MAX_LEVERAGE).expect("a liquidated slot had a key");
    (side, key, account)
}

fn order(account: AccountId, seq: u32, side: Side, price: Price, qty: i64, tif: TimeInForce) -> Command {
    Command::PlaceOrder(PlaceOrder {
        order_id: order_id(account, seq),
        price,
        qty,
        market: MARKET,
        side,
        tif,
        post_only: false,
    })
}

fn leverage(account: AccountId, leverage: u16) -> Command {
    Command::SetLeverage(SetLeverage { account, market: MARKET, leverage })
}

/// Guards RISK.md 18, P1 (the `BTreeSet` index replaced by an indexed heap): a heap of 300
/// entries per side, 5 levels deep, re-keyed by small moves, walks in the same order as a
/// scan, ties included. The property test's six accounts only build heaps 2 levels deep.
#[test]
fn a_deep_liquidation_index_walks_in_the_same_order_as_a_scan() {
    let mut engines = TwoEngines::new();
    engines.apply(Command::SetMarketParams(SetMarketParams {
        min_price: 1_000,
        max_price: 300_000,
        maker_fee_ppm: 125,
        taker_fee_ppm: 400,
        price_band_ppm: 22_100,
        market: MARKET,
        max_leverage: MAX_LEVERAGE,
    }));
    engines.apply(Command::SetRiskTier(SetRiskTier {
        lower_bound: 0,
        market: MARKET,
        max_leverage: MAX_LEVERAGE,
        index: 0,
        count: 1,
    }));
    engines.apply(Command::SetMark(SetMark { price: MID, market: MARKET }));
    engines.apply(Command::Deposit(Deposit { amount: 1_000_000_000_000, account: FUND }));
    engines.apply(Command::Deposit(Deposit { amount: 1_000_000_000_000_000, account: MAKER }));

    // Each trader takes 10 to 70 lots from the maker at the mark, at leverage 2 to 20. The
    // pair repeats every 133 accounts, so some slots are identical and share a key.
    let traders = |side: Side| {
        let first = if side == Side::Buy { 1 } else { TRADERS_PER_SIDE + 1 };
        first..first + TRADERS_PER_SIDE
    };
    let lots = |account: AccountId| 10 + 10 * i64::from(account % 7);
    let mut seq = 0;
    for side in [Side::Buy, Side::Sell] {
        seq += 1;
        let total: i64 = traders(side).map(lots).sum();
        engines.apply(order(MAKER, seq, side.opposite(), MID, total, TimeInForce::Gtc));
        for account in traders(side) {
            engines.apply(Command::Deposit(Deposit { amount: 100_000_000, account }));
            engines.apply(leverage(account, 2 + (account % 19) as u16));
            engines.apply(order(account, 1, side, MID, lots(account), TimeInForce::Ioc));
        }
    }
    engines.assert_same_state();

    // Small key moves: a new leverage tops up or releases; a resting order away from the
    // mark tops up and its cancel releases again. Every third trader leaves its order
    // resting, for the band sweep or the liquidation to cancel.
    for account in traders(Side::Buy).chain(traders(Side::Sell)) {
        engines.apply(leverage(account, 2 + ((account * 7) % 19) as u16));
        let side = if account <= TRADERS_PER_SIDE { Side::Buy } else { Side::Sell };
        let away = if side == Side::Buy { MID - 1_000 } else { MID + 1_000 };
        engines.apply(order(account, 2, side, away, 5, TimeInForce::Gtc));
        if account % 3 != 0 {
            engines
                .apply(Command::CancelOrder(CancelOrder { order_id: order_id(account, 2), market: MARKET }));
        }
    }
    engines.assert_same_state();

    // The mark falls through the longs' keys, comes back, then rises through the shorts'.
    let falling = (0..40).map(|step| MID - 500 * step);
    let rising = (0..40).map(|step| MID + 500 * step);
    for price in falling.chain(rising) {
        engines.set_mark(price);
    }
    assert!(engines.liquidations > 400, "only {} liquidations", engines.liquidations);
    assert!(engines.ties > 10, "only {} ties", engines.ties);
}
