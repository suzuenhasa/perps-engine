//! Tests of the Polymarket-shaped flow (`polymarket.rs`; D-034): the markets' parameters,
//! the setup phases, the same plan for the same config, pinned first items and counts, the
//! module docs' invariants over whole plans (nonces, sequences, ownership, grid, range, band,
//! an uncrossed book of 20 quotes a side), every phase accepted by the real engine, the
//! realised shape against the profile (maker mix and levels, spreads, depth level by level,
//! gaps, level distances, takers, marks, moves), jumps and shocks, the cascade, the makers
//! over the gateways and `makers_k`, and both signing schemes.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use super::*;
use crate::market_flow::profile::{BookClass, LeverageClass, Spread};
use crate::market_flow::{ClientItem, DOLLAR, FlowPhase, MarketFlowConfig};
use engine::book::Book;
use engine::command::{CancelOrder, ModifyOrder, PlaceOrder};
use engine::engine::Engine;
use engine::event::{Event as EngineEvent, RejectReason};
use engine::mode::Fast;
use engine::money::{band_edges, band_rule_1_holds, band_rule_2_holds};
use engine::types::{OrderId, OrderSeq, Qty, TimeInForce, account_of, order_id, sequence_of};

/// A price of `ticks` ticks.
fn px(ticks: i64) -> Price {
    Price::new(ticks)
}

/// A quantity of `lots` lots.
fn lots(lots: i64) -> Qty {
    Qty::new(lots)
}

/// Account number `n`.
fn acct(n: u32) -> AccountId {
    AccountId::new(n)
}

// ---------------------------------------------------------------------------------------
// The generator's view of the books, rebuilt from a plan.

/// One resting quote, as the generator last sent it.
#[derive(Clone, Copy, Debug)]
struct Resting {
    market: MarketId,
    side: Side,
    price: Price,
    size: Qty,
}

/// What the timed flow's items and books looked like.
#[derive(Debug, Default)]
struct Shape {
    /// Maker messages: places, cancels, and modifies to a larger, smaller or equal size.
    adds: u64,
    removes: u64,
    ups: u64,
    downs: u64,
    same: u64,
    /// Maker messages at levels 1–5, 6–10 and 11–20 (a cancel's level before it, a place's
    /// after it).
    bands: [u64; 3],
    /// Taker IOCs, and the cohorts' IOCs.
    takers: u64,
    cohort_iocs: u64,
    /// Per book class, sampled at each mark (so between events): the market's spread in
    /// hundredths of a bps; the notional of each of its 40 quotes in dollars, and level by
    /// level; each gap between neighbouring quotes of a side, by gap bucket (after levels
    /// 1–4, 5–9, 10–19), as all gaps, those of one real tick and those over 10; and levels
    /// 1, 5, 10 and 20's distance from the mid, in hundredths of a bps.
    spreads: [Vec<i64>; 5],
    depth: [Vec<i64>; 5],
    level_depth: [[Vec<i64>; 20]; 5],
    gaps: [[[u64; 3]; 3]; 5],
    distances: [[Vec<i64>; 4]; 5],
    /// Per maker account: its messages.
    per_maker: BTreeMap<AccountId, u64>,
}

impl Shape {
    fn makers(&self) -> u64 {
        self.adds + self.removes + self.ups + self.downs + self.same
    }
}

/// The generator's view of every book, rebuilt item by item from a plan, checking the module
/// docs' invariants as it goes (every assertion names what it found).
#[derive(Debug, Default)]
struct View {
    orders: HashMap<OrderId, Resting>,
    /// Per market, bids then asks: each quote's price and order.
    books: HashMap<MarketId, [BTreeMap<Price, OrderId>; 2]>,
    marks: HashMap<MarketId, Price>,
    last_nonce: HashMap<AccountId, u64>,
    last_sequence: HashMap<AccountId, u32>,
    /// True while applying the timed flow: only it counts in `shape`.
    timed: bool,
    shape: Shape,
}

fn side_index(side: Side) -> usize {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

fn spec(market: MarketId) -> &'static Market {
    POLYMARKET.market(market).expect("a market of the profile")
}

fn band(rank: usize) -> usize {
    match rank {
        0..=4 => 0,
        5..=9 => 1,
        _ => 2,
    }
}

/// The gap bucket of the gap after the quote at `rank`: after levels 1–4, 5–9 or 10–19.
fn gap_bucket(rank: usize) -> usize {
    match rank {
        0..=3 => 0,
        4..=8 => 1,
        _ => 2,
    }
}

impl View {
    /// Applies every item of `plan`, phase by phase. Every book holds 20 quotes a side after
    /// setup and at every mark of the timed flow, which falls between two events. (Not at the
    /// plan's end: a plan ends at a client item, which can be in the middle of an event.)
    fn of(plan: &FlowPlan<PolymarketConfig>) -> View {
        let mut view = View::default();
        for phase in FlowPhase::ALL {
            view.timed = phase == FlowPhase::Timed;
            if view.timed {
                for market in plan.config.markets() {
                    view.check_full(market.market_id());
                }
            }
            for item in plan.phase(phase) {
                view.apply(&plan.config, item);
            }
        }
        view
    }

    /// Checks that `market`'s book holds 20 quotes a side, not crossed.
    fn check_full(&self, market: MarketId) {
        let [bids, asks] = &self.books[&market];
        assert_eq!((bids.len(), asks.len()), (20, 20), "{}", spec(market).symbol);
        let (bid, ask) = self.best(market);
        assert!(bid < ask, "{}: crossed at {bid} and {ask}", spec(market).symbol);
    }

    /// The best bid and ask of `market`.
    fn best(&self, market: MarketId) -> (Price, Price) {
        let [bids, asks] = &self.books[&market];
        (*bids.keys().next_back().expect("bids"), *asks.keys().next().expect("asks"))
    }

    /// The rank (0 = best) of a quote at `price` among `side`'s other quotes.
    fn rank(&self, market: MarketId, side: Side, price: Price) -> usize {
        let book = &self.books[&market][side_index(side)];
        match side {
            Side::Buy => book.range(price + Price::ONE_TICK..).count(),
            Side::Sell => book.range(..price).count(),
        }
    }

    fn count_band(&mut self, rank: usize) {
        if self.timed {
            self.shape.bands[band(rank)] += 1;
        }
    }

    fn apply(&mut self, config: &PolymarketConfig, item: &Item) {
        let Item::Client(ClientItem { account, nonce, command }) = *item else {
            if let Command::SetMark(mark) = item.command() {
                self.mark(mark.market, mark.price);
            }
            return;
        };
        let cohort = config.cohort_of(account).unwrap_or_else(|| panic!("{account} has no cohort: {item:?}"));
        let last = self.last_nonce.entry(account).or_default();
        assert_eq!(nonce, *last + 1, "nonces are 1, 2, 3, … per account: {item:?}");
        *last = nonce;
        if self.timed && matches!(cohort, Cohort::Maker { .. }) {
            *self.shape.per_maker.entry(account).or_default() += 1;
        }
        match command {
            Command::PlaceOrder(place) => self.place(account, cohort, place),
            Command::CancelOrder(cancel) => {
                let resting = self.orders.remove(&cancel.order_id);
                let resting =
                    resting.unwrap_or_else(|| panic!("a cancel of a quote it doesn't hold: {item:?}"));
                assert_eq!(
                    (account_of(cancel.order_id), resting.market),
                    (account, cancel.market),
                    "{item:?}"
                );
                self.count_band(self.rank(resting.market, resting.side, resting.price));
                self.books.get_mut(&resting.market).expect("a book")[side_index(resting.side)]
                    .remove(&resting.price);
                self.shape.removes += u64::from(self.timed);
            }
            Command::ModifyOrder(modify) => self.modify(account, modify),
            other => panic!("a client never sends {other:?}"),
        }
    }

    /// A mark: on the grid and in range. In the timed flow, first samples the market's book,
    /// which is between two events.
    fn mark(&mut self, market: MarketId, price: Price) {
        let params = market_params(spec(market));
        assert!(
            (params.min_price..=params.max_price).contains(&price) && on_grid(price),
            "mark {price} of {market}"
        );
        if self.timed {
            self.check_full(market);
            let class = spec(market).book.class as usize;
            // Spreads and distances in hundredths of a bps, on the bare numbers of ticks.
            let (bid, ask) = self.best(market);
            let (bid, ask) = (bid.ticks(), ask.ticks());
            self.shape.spreads[class].push((ask - bid) * 2 * 1_000_000 / (ask + bid));
            let [bids, asks] = &self.books[&market];
            // Each side best first.
            let sides: [Vec<Price>; 2] =
                [bids.keys().rev().copied().collect(), asks.keys().copied().collect()];
            for (book, prices) in self.books[&market].iter().zip(&sides) {
                for (rank, price) in prices.iter().enumerate() {
                    let quote = self.orders[&book[price]];
                    let usd = (quote.size * quote.price).micros() / DOLLAR.micros();
                    self.shape.depth[class].push(usd);
                    self.shape.level_depth[class][rank].push(usd);
                }
                // Gaps in real ticks, as the calibration counts them (scan.py).
                for (rank, pair) in prices.windows(2).enumerate() {
                    let ticks = (pair[1] - pair[0]).ticks().abs() / real_tick(pair[0].min(pair[1])).ticks();
                    let counts = &mut self.shape.gaps[class][gap_bucket(rank)];
                    counts[0] += 1;
                    counts[1] += u64::from(ticks == 1);
                    counts[2] += u64::from(ticks > 10);
                }
                for (i, level) in [1, 5, 10, 20].into_iter().enumerate() {
                    let distance =
                        (prices[level - 1].ticks() * 2 - (bid + ask)).abs() * 1_000_000 / (bid + ask);
                    self.shape.distances[class][i].push(distance);
                }
            }
        }
        self.marks.insert(market, price);
    }

    /// Checks a price against its market's range, grid and the latest mark's band.
    fn check_price(&self, market: MarketId, price: Price, what: &dyn std::fmt::Debug) {
        let params = market_params(spec(market));
        assert!((params.min_price..=params.max_price).contains(&price), "{what:?} outside the range");
        assert!(on_grid(price), "{what:?} off the grid");
        let mark = self.marks[&market];
        let edges = band_edges(mark, params.price_band_ppm);
        assert!((edges.lower..=edges.upper).contains(&price), "{what:?} outside the band of {mark}");
    }

    fn place(&mut self, account: AccountId, cohort: Cohort, place: PlaceOrder) {
        assert_eq!(account_of(place.order_id), account, "{place:?}");
        let sequence = self.last_sequence.entry(account).or_default();
        assert_eq!(sequence_of(place.order_id).get(), *sequence + 1, "order sequences 1, 2, 3, …: {place:?}");
        *sequence += 1;
        self.check_price(place.market, place.price, &place);
        let mark = self.marks[&place.market];
        if place.tif == TimeInForce::Ioc {
            // Takers and cohorts: worth $10 or more at the mark, at most the cap and a lot.
            assert!(!place.post_only && !matches!(cohort, Cohort::Maker { .. }), "{place:?}");
            // A lot at the mark is worth `mark` micros (D-004).
            let notional = place.qty * mark;
            let cap = Micros::new(spec(place.market).max_notional);
            assert!(notional >= dollars(10) && notional < cap + lots(1) * mark, "{place:?}");
            match cohort {
                Cohort::Taker => self.shape.takers += u64::from(self.timed),
                _ => self.shape.cohort_iocs += u64::from(self.timed),
            }
            return;
        }
        assert!(place.post_only && matches!(cohort, Cohort::Maker { .. }), "{place:?}");
        assert!(place.qty * place.price >= dollars(10), "{place:?} under $10");
        // The generator's own book is never crossed, so no post-only quote is rejected for it.
        if self.books.get(&place.market).is_some_and(|[bids, asks]| !bids.is_empty() && !asks.is_empty()) {
            let (bid, ask) = self.best(place.market);
            let fair = mark; // the fair value is the latest mark
            match place.side {
                Side::Buy => {
                    assert!(place.price < ask && place.price <= fair, "{place:?}: ask {ask}, fair {fair}")
                }
                Side::Sell => {
                    assert!(place.price > bid && place.price > fair, "{place:?}: bid {bid}, fair {fair}")
                }
            }
        }
        let book = &mut self.books.entry(place.market).or_default()[side_index(place.side)];
        assert!(book.insert(place.price, place.order_id).is_none(), "two quotes at one price: {place:?}");
        let resting = Resting { market: place.market, side: place.side, price: place.price, size: place.qty };
        self.orders.insert(place.order_id, resting);
        self.count_band(self.rank(place.market, place.side, place.price));
        self.shape.adds += u64::from(self.timed);
    }

    fn modify(&mut self, account: AccountId, modify: ModifyOrder) {
        let resting = self.orders.get(&modify.order_id).copied();
        let resting = resting.unwrap_or_else(|| panic!("a modify of a quote it doesn't hold: {modify:?}"));
        assert_eq!((account_of(modify.order_id), resting.market), (account, modify.market), "{modify:?}");
        assert_eq!(modify.new_price, resting.price, "a resize keeps its price: {modify:?}");
        self.check_price(modify.market, modify.new_price, &modify);
        assert!(modify.new_size * modify.new_price >= dollars(10), "{modify:?} under $10");
        if self.timed {
            match modify.new_size.cmp(&resting.size) {
                std::cmp::Ordering::Greater => self.shape.ups += 1,
                std::cmp::Ordering::Less => self.shape.downs += 1,
                std::cmp::Ordering::Equal => self.shape.same += 1,
            }
        }
        self.count_band(self.rank(resting.market, resting.side, resting.price));
        self.orders.insert(modify.order_id, Resting { size: modify.new_size, ..resting });
    }
}

/// `part` as a percentage of `whole`.
fn percent(part: u64, whole: u64) -> f64 {
    100.0 * part as f64 / whole as f64
}

/// The `q`-quantile of `values` (sorted here).
fn quantile(values: &mut [i64], q: f64) -> i64 {
    values.sort_unstable();
    values[((values.len() - 1) as f64 * q).round() as usize]
}

/// True if `actual` is within `tolerance` (a fraction) of `expected`.
fn near(actual: f64, expected: f64, tolerance: f64) -> bool {
    (actual / expected - 1.0).abs() <= tolerance
}

/// The default flow's plan with 1,000,000 timed client items (10 s of flow time), and its
/// view: generated once, for every test that looks at the realised shape.
fn sample() -> &'static (FlowPlan<PolymarketConfig>, View) {
    static SAMPLE: OnceLock<(FlowPlan<PolymarketConfig>, View)> = OnceLock::new();
    SAMPLE.get_or_init(|| {
        let plan = generate(&PolymarketConfig::default(), 1_000_000);
        let view = View::of(&plan);
        (plan, view)
    })
}

/// What the engine said to a list of items.
#[derive(Debug, Default)]
struct Applied {
    rejects: Vec<(Command, RejectReason)>,
    liquidations: Vec<(Command, u64)>,
}

impl Applied {
    fn liquidated(&self) -> u64 {
        self.liquidations.iter().map(|(_, count)| count).sum()
    }
}

fn apply(engine: &mut Engine<Book, Fast>, items: &[Item]) -> Applied {
    let mut applied = Applied::default();
    let mut events = Vec::new();
    for item in items {
        events.clear();
        engine.apply(item.command(), &mut events);
        let mut liquidations = 0;
        for event in &events {
            match event {
                EngineEvent::Reject(reject) => applied.rejects.push((*item.command(), reject.reason)),
                EngineEvent::Liquidation(_) => liquidations += 1,
                _ => {}
            }
        }
        if liquidations > 0 {
            applied.liquidations.push((*item.command(), liquidations));
        }
    }
    applied
}

/// An engine that has applied `plan`'s setup phases, none of which it rejected.
fn engine_after_setup(plan: &FlowPlan<PolymarketConfig>) -> Engine<Book, Fast> {
    let mut engine: Engine<Book, Fast> = Engine::new(plan.config.engine_options());
    for (phase, items) in [("A", &plan.setup_a), ("B1", &plan.setup_b1), ("B2", &plan.setup_b2)] {
        let applied = apply(&mut engine, items);
        assert!(
            applied.rejects.is_empty(),
            "phase {phase}: {:?}",
            &applied.rejects[..applied.rejects.len().min(5)]
        );
    }
    engine
}

// ---------------------------------------------------------------------------------------
// Markets and setup.

#[test]
fn every_market_has_its_real_parameters_and_passes_both_band_rules() {
    let mut level_bytes = 0;
    for market in POLYMARKET.markets {
        let params = market_params(market);
        let fee = params.maker_fee_ppm.max(params.taker_fee_ppm);
        assert!(band_rule_1_holds(params.max_leverage, params.price_band_ppm, fee), "{params:?}");
        assert!(
            band_rule_2_holds(params.min_price, params.max_leverage, params.price_band_ppm, fee),
            "{params:?}"
        );
        assert_eq!(
            (params.min_price, params.max_price),
            (px(market.start_price / 2), px(2 * market.start_price))
        );
        assert_eq!((params.market, params.max_leverage), (market.market_id(), market.max_leverage));
        // The fair value's bounds keep the band's edges inside the range.
        let state = MarketState::new(1, market);
        for fair in [state.fair_low, state.fair_high] {
            let edges = band_edges(fair, params.price_band_ppm);
            assert!(params.min_price <= edges.lower && edges.upper <= params.max_price, "{}", market.symbol);
            assert!(on_grid(fair), "{}", market.symbol);
        }
        level_bytes += 8 * ((params.max_price - params.min_price).ticks() + 1);
    }
    // The module docs: 85.6 MB of levels for the 88 books.
    assert_eq!(level_bytes / 100_000, 856, "{level_bytes} bytes of levels");
    // Bands and fees by leverage.
    assert_eq!([50, 20, 10, 5, 3].map(band_ppm), [8_000, 20_000, 40_000, 80_000, 133_333]);
    assert_eq!(
        [50, 20, 10, 5, 3].map(fees_ppm),
        [(400, 100), (400, 125), (500, 100), (500, 100), (500, 100)]
    );
}

#[test]
fn the_real_grid_has_five_significant_figures() {
    let ticks = [1, 99_999, 100_000, 999_999, 1_000_000, 1_509_400].map(|price| real_tick(px(price)).ticks());
    assert_eq!(ticks, [1, 1, 10, 10, 100, 100]);
    let snapped = [snap_down(px(123_456)), snap_up(px(123_456)), snap_up(px(123_450))];
    assert_eq!(snapped, [123_450, 123_460, 123_450].map(px));
    // Rounding up across a power of ten lands on it, which is on every grid.
    let snapped = [snap_up(px(99_999)), snap_up(px(999_995)), snap_down(px(1_000_050))];
    assert_eq!(snapped, [99_999, 1_000_000, 1_000_000].map(px));
    assert_eq!(snap_up(px(9_999_991)), px(10_000_000));
    assert!(on_grid(px(99_999)) && on_grid(px(100_010)) && !on_grid(px(100_001)));
}

#[test]
fn setup_phases_hold_what_the_module_docs_list_in_its_order() {
    let config = PolymarketConfig::default();
    let plan = generate(&config, 0);
    let tiers: usize = POLYMARKET.markets.iter().map(|market| market.tiers.len()).sum();
    assert_eq!(tiers, 453);
    // A: per market its parameters, tier rows and mark; the fund; 1,236 deposits; the
    // leverage of 3 makers in each of the 88 markets and of 176 high-leverage accounts.
    assert_eq!(plan.setup_a.len(), 88 * 2 + 453 + 1 + (60 + 1_000 + 176) + (88 * 3 + 176));
    assert_eq!((plan.setup_b1.len(), plan.setup_b2.len()), (88 * 40, 176 * 3));
    assert!(plan.timed.is_empty());
    assert!(plan.setup_a.iter().all(|item| !item.is_client()));
    let sp500 = &POLYMARKET.markets[0];
    assert_eq!(*plan.setup_a[0].command(), Command::SetMarketParams(market_params(sp500)));
    let first_tier = SetRiskTier {
        lower_bound: Micros::ZERO,
        market: MarketId::new(1),
        max_leverage: 50,
        index: 0,
        count: 8,
    };
    assert_eq!(*plan.setup_a[1].command(), Command::SetRiskTier(first_tier));
    assert_eq!(
        *plan.setup_a[9].command(),
        Command::SetMark(SetMark { price: px(76_822), market: MarketId::new(1) })
    );
    let fund = 88 * 2 + 453;
    assert_eq!(
        *plan.setup_a[fund].command(),
        Command::Deposit(Deposit { amount: dollars(1_000), account: FUND })
    );
    let maker = Command::Deposit(Deposit { amount: dollars(1_000_000_000), account: acct(1) });
    assert_eq!(*plan.setup_a[fund + 1].command(), maker);
    // Maker 1's first market (by id) is its group's lightest-numbered one, at 5x.
    let market_makers = config.market_makers();
    let first_market = market_makers.iter().position(|&(first, _)| first == 1).expect("maker 1 quotes");
    let leverage = fund + 1 + 1_236;
    let expected = SetLeverage {
        account: acct(1),
        market: POLYMARKET.markets[first_market].market_id(),
        leverage: 5.min(POLYMARKET.markets[first_market].max_leverage),
    };
    assert_eq!(*plan.setup_a[leverage].command(), Command::SetLeverage(expected));
    // The last: the high-leverage short of the last market (NCLD-USD, id 90, 10x).
    let last = SetLeverage { account: acct(5_176), market: MarketId::new(90), leverage: 10 };
    assert_eq!(*plan.setup_a.last().expect("items").command(), Command::SetLeverage(last));

    // B1: per market its 20 bids, then its 20 asks, post-only, around the start price.
    for (m, market) in POLYMARKET.markets.iter().enumerate() {
        let quotes: Vec<&PlaceOrder> = plan.setup_b1[40 * m..40 * (m + 1)]
            .iter()
            .map(|item| match item.command() {
                Command::PlaceOrder(place) => place,
                other => panic!("B1 is places: {other:?}"),
            })
            .collect();
        let (bids, asks) = quotes.split_at(20);
        assert!(
            quotes.iter().all(|q| q.market == market.market_id() && q.post_only && q.tif == TimeInForce::Gtc)
        );
        assert!(bids.iter().all(|q| q.side == Side::Buy) && asks.iter().all(|q| q.side == Side::Sell));
        // The best first: each quote a gap behind the one before, except where the gap would
        // pass the reach (half the band): then the deepest free price within it.
        let start = px(market.start_price);
        let reach = px(market.start_price * i64::from(band_ppm(market.max_leverage)) / 2_000_000);
        assert!(bids[0].price <= start && asks[0].price > start, "{}", market.symbol);
        assert!(bids[1..].iter().all(|q| q.price < bids[0].price && start - q.price <= reach));
        assert!(asks[1..].iter().all(|q| q.price > asks[0].price && q.price - start <= reach));
        // Market m's group of 3 makers: the r-th bid is maker (r + m) mod 3's, the r-th ask
        // maker (r + 1 + m) mod 3's.
        let (first, count) = market_makers[m];
        assert_eq!(count, 3);
        for (rank, (bid, ask)) in bids.iter().zip(asks).enumerate() {
            let turn = rank as u32 + m as u32;
            assert_eq!(account_of(bid.order_id), acct(first + turn % 3));
            assert_eq!(account_of(ask.order_id), acct(first + (turn + 1) % 3));
        }
    }
    // B2: three IOCs per high-leverage account, in id order, at the band's edge of the start.
    for (i, item) in plan.setup_b2.iter().enumerate() {
        let Item::Client(client) = item else { panic!("B2 is client items") };
        let (account, market, side) = config.high_leverage((i / 3) as u32);
        let Command::PlaceOrder(place) = client.command else { panic!("B2 is places") };
        let spec = &POLYMARKET.markets[market];
        assert_eq!(
            (client.account, place.market, place.side, place.tif),
            (account, spec.market_id(), side, TimeInForce::Ioc)
        );
        let edges = band_edges(px(spec.start_price), band_ppm(spec.max_leverage));
        let edge = if side == Side::Buy { snap_down(edges.upper) } else { snap_up(edges.lower) };
        assert_eq!(place.price, edge);
        // Micros over ticks is lots (D-004).
        assert_eq!(place.qty, lots(engine::money::ceil_div(dollars(2_000).micros(), spec.start_price)));
    }
    // SP500's high-leverage pair: 5,001 long, 5,002 short.
    assert_eq!(
        config.cohort_of(acct(5_001)),
        Some(Cohort::HighLeverage { market: MarketId::new(1), side: Side::Buy })
    );
    assert_eq!(
        config.cohort_of(acct(5_002)),
        Some(Cohort::HighLeverage { market: MarketId::new(1), side: Side::Sell })
    );
    assert_eq!(
        (config.cohort_of(acct(60)), config.cohort_of(acct(61))),
        (Some(Cohort::Maker { index: 59 }), None)
    );
    assert_eq!((config.cohort_of(acct(2_000)), config.cohort_of(acct(2_001))), (Some(Cohort::Taker), None));
    assert_eq!((config.cohort_of(acct(5_177)), config.cohort_of(acct(CASCADE_BASE))), (None, None));
    assert_eq!(config.client_accounts().len(), 60 + 1_000 + 176);
}

// ---------------------------------------------------------------------------------------
// Determinism.

#[test]
fn the_same_config_gives_the_same_plan_and_a_longer_plan_starts_with_a_shorter_one() {
    let config = PolymarketConfig::default();
    let a = generate(&config, 20_000);
    let b = generate(&config, 20_000);
    assert_eq!(
        (a.setup_a.clone(), a.setup_b1.clone(), a.setup_b2.clone(), a.timed.clone()),
        (b.setup_a, b.setup_b1, b.setup_b2, b.timed)
    );
    let other = generate(&PolymarketConfig { seed: 2, ..config }, 20_000);
    assert_ne!(a.setup_b1, other.setup_b1);
    assert_ne!(a.timed, other.timed);
    let longer = generate(&config, 30_000);
    assert_eq!(longer.timed[..a.timed.len()], a.timed[..]);
    assert_eq!(a.timed.iter().filter(|item| item.is_client()).count(), 20_000);
    assert!(a.timed.last().expect("items").is_client(), "the plan ends at its last client item");
    assert_eq!(a.client_items().count(), a.setup_client_items() + 20_000);
    assert_eq!(a.setup_client_items(), 88 * 40 + 176 * 3);
}

#[test]
fn the_digest_names_the_config_and_never_an_m3_flow() {
    let config = PolymarketConfig::default();
    assert_eq!(config.digest(), PolymarketConfig::default().digest());
    assert_eq!(PlanConfig::digest(&config), config.digest());
    let changed = [
        PolymarketConfig { seed: 2, ..config },
        PolymarketConfig { messages_per_flow_second: 100_001, ..config },
        PolymarketConfig { makers_k: Some(3), ..config },
        PolymarketConfig { jump_one_in: 1, ..config },
        PolymarketConfig { high_leverage_ppm: 101, ..config },
        PolymarketConfig { shock: Some(ShockConfig::real()), ..config },
        PolymarketConfig { shock: Some(ShockConfig::stress()), ..config },
        PolymarketConfig { shock: Some(ShockConfig { every_ns: 1, ..ShockConfig::stress() }), ..config },
    ];
    for other in changed {
        assert_ne!(other.digest(), config.digest(), "{other:?}");
    }
    assert_ne!(config.digest(), MarketFlowConfig::m3().digest());
    // The digest covers the profile table's content, not only its name: it hashes the JSON's
    // SHA-256 (`profile/tests.rs` checks that against the committed JSON).
    assert_eq!(POLYMARKET.sha256.len(), 64);
}

#[test]
fn the_first_items_and_the_counts_after_1m_items_are_pinned() {
    // Taken from this generator when it was written; the shape tests check the counts
    // against the profile. This one pins the exact stream, so that any change in a draw's
    // order shows. If the generator changes on purpose, bump FLOW_VERSION.
    let config = PolymarketConfig::default();
    assert_eq!(hex(&config.digest()), "b23c96288046cbdf65a1b2b6a50785f9ac81d31fdf2dd7f16ee7faac869a60ae");
    let (_, mut flow) = PolymarketFlow::start(config);
    let first: Vec<Item> = flow.by_ref().take(3).collect();
    // A resize of maker 16's 22nd order (AMD-USD), then a re-price of maker 12's 62nd
    // (MRVL-USD): its cancel, and its new quote.
    let modify = ModifyOrder {
        order_id: order_id(acct(16), OrderSeq::new(22)),
        new_price: px(60_790),
        new_size: lots(4_261),
        market: MarketId::new(22),
    };
    assert_eq!(
        first[0],
        Item::Client(ClientItem { account: acct(16), nonce: 55, command: Command::ModifyOrder(modify) })
    );
    let cancel = CancelOrder { order_id: order_id(acct(12), OrderSeq::new(62)), market: MarketId::new(86) };
    assert_eq!(
        first[1],
        Item::Client(ClientItem { account: acct(12), nonce: 68, command: Command::CancelOrder(cancel) })
    );
    let place = PlaceOrder {
        order_id: order_id(acct(12), OrderSeq::new(68)),
        price: px(26_262),
        qty: lots(6_732_428),
        market: MarketId::new(86),
        side: Side::Sell,
        tif: TimeInForce::Gtc,
        post_only: true,
    };
    assert_eq!(
        first[2],
        Item::Client(ClientItem { account: acct(12), nonce: 69, command: Command::PlaceOrder(place) })
    );

    // IOCs, quotes, cancels, modifies and marks over the first 1,000,000 items.
    let mut counts = [0u64; 5];
    let mut last = first[2];
    for item in first.iter().copied().chain(flow.by_ref().take(1_000_000 - 3)) {
        let kind = match item.command() {
            Command::PlaceOrder(place) if place.tif == TimeInForce::Ioc => 0,
            Command::PlaceOrder(_) => 1,
            Command::CancelOrder(_) => 2,
            Command::ModifyOrder(_) => 3,
            Command::SetMark(_) => 4,
            other => panic!("the timed flow never sends {other:?}"),
        };
        counts[kind] += 1;
        last = item;
    }
    assert_eq!(counts, [661, 364_186, 364_187, 266_672, 4_294]);
    let cancel = CancelOrder { order_id: order_id(acct(7), OrderSeq::new(5_576)), market: MarketId::new(55) };
    assert_eq!(
        last,
        Item::Client(ClientItem { account: acct(7), nonce: 15_241, command: Command::CancelOrder(cancel) })
    );
    assert_eq!((flow.flow_ns(), flow.jumps().len()), (9_957_320_551, 0));
}

/// Lower-case hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ---------------------------------------------------------------------------------------
// Invariants over whole plans.

#[test]
fn nonces_sequences_ownership_grid_ranges_bands_and_books_hold_over_the_default_plan() {
    // `View::of` checks them all, item by item (the module docs' invariants).
    let (plan, view) = sample();
    // Every maker and high-leverage account sent something; takers send about 60 orders a
    // second between 1,000 accounts, so in 10 s a third or more, not all.
    let config = &plan.config;
    let sent = |cohort: fn(&Cohort) -> bool| {
        view.last_nonce.keys().filter(|&&a| config.cohort_of(a).is_some_and(|c| cohort(&c))).count()
    };
    assert_eq!(sent(|c| matches!(c, Cohort::Maker { .. })), 60);
    assert_eq!(sent(|c| matches!(c, Cohort::HighLeverage { .. })), 176);
    assert!((300..1_000).contains(&sent(|c| *c == Cohort::Taker)));
}

#[test]
fn the_invariants_hold_with_every_switch_on_and_frequent_jumps() {
    for config in [
        PolymarketConfig { makers_k: Some(3), ..PolymarketConfig::default() },
        PolymarketConfig { makers_k: Some(1), jump_one_in: 20, ..PolymarketConfig::default() },
        PolymarketConfig {
            messages_per_flow_second: 20_000,
            shock: Some(ShockConfig { every_ns: SECOND_NS, ..ShockConfig::stress() }),
            ..PolymarketConfig::default()
        },
        PolymarketConfig {
            messages_per_flow_second: 20_000,
            shock: Some(ShockConfig { every_ns: SECOND_NS, ..ShockConfig::real() }),
            ..PolymarketConfig::default()
        },
    ] {
        let plan = generate(&config, 150_000);
        View::of(&plan);
    }
}

// ---------------------------------------------------------------------------------------
// The engine.

#[test]
fn every_setup_phase_is_accepted_by_the_engine_and_the_timed_flow_keeps_rejects_rare() {
    let (plan, _) = sample();
    let mut engine = engine_after_setup(plan);
    // Every high-leverage account holds its position.
    let positions = engine
        .snapshot()
        .markets
        .iter()
        .flat_map(|market| &market.slots)
        .filter(|slot| slot.pos != Qty::ZERO)
        .count();
    assert!(positions > 176, "{positions} open positions");
    let timed = &plan.timed[..400_000];
    let applied = apply(&mut engine, timed);
    let clients = timed.iter().filter(|item| item.is_client()).count() as u64;
    let share = percent(applied.rejects.len() as u64, clients);
    // Only quotes takers filled: open loop, as real flow (module docs). In plan order never a
    // post-only cross (through the pipeline one maker's place can overtake another's cancel,
    // `market.rs`), nor a band, margin or nonce problem.
    assert!(share < 0.5, "{share:.3}% of client commands rejected");
    assert!(
        applied.rejects.iter().all(|(_, reason)| *reason == RejectReason::UnknownOrder),
        "{:?}",
        applied.rejects
    );
    engine.assert_invariants();
}

// ---------------------------------------------------------------------------------------
// The realised shape, against the profile.

#[test]
fn maker_messages_split_and_spread_over_the_levels_as_recorded() {
    let s = &sample().1.shape;
    let makers = s.makers();
    // 36.6% adds, 36.6% removes, 13.4% size ups, 13.4% size downs (D-034): adds and removes
    // exactly, by the running split; ups and downs within 0.5 points; a resize that draws the
    // size it had is rare.
    let mix = [s.adds, s.removes, s.ups, s.downs].map(|count| percent(count, makers));
    for (actual, expected) in mix.iter().zip([36.6, 36.6, 13.4, 13.4]) {
        assert!((actual - expected).abs() < 0.5, "{mix:.2?}");
    }
    assert!(percent(s.same, makers) < 0.5);
    // 52% at levels 1–5, 27.7% at 6–10, 20.3% at 11–20, within 1 point (a spread change also
    // moves the quotes ahead of the new best, near the top).
    let bands = s.bands.map(|count| percent(count, s.bands.iter().sum()));
    for (actual, expected) in bands.iter().zip([52.0, 27.7, 20.3]) {
        assert!((actual - expected).abs() < 1.0, "{bands:.2?}");
    }
    // The mix's derivation (polymarket.rs, `MakerMix`).
    let mix = MakerMix::of(&POLYMARKET);
    assert_eq!(mix.moves_ppm, 732_000);
    // Error diffusion: a re-price while moves are below 73.2% of the maker messages so far.
    assert!(mix.reprice_next(0, 1) && mix.reprice_next(731, 269) && !mix.reprice_next(732, 268));
    assert_eq!(mix.resize_bands, [520_000, 277_000, 203_000]);
    assert_eq!(mix.reprice_bands.iter().sum::<u32>(), 1_000_000);
}

#[test]
fn spreads_follow_the_profile_and_depth_the_recorded_books() {
    let s = &mut sample_shape();
    // Each class's spread, sampled between events, against its table's 10th, 50th and 90th
    // percentiles, within 20%. A spread change draws the spread given where the second quotes
    // are (`market.rs`), so each market's spread relaxes slowly around its law, and 10 s of
    // flow hold only a few independent spreads per market (over 2M events, one market's
    // quantiles come within 10% of its table's); the rounding to whole real ticks also widens
    // the coarsest markets'.
    for class in [BookClass::AltCrypto, BookClass::LongTailCrypto, BookClass::TradfiEquities] {
        let Spread::SplitLognormal { centibps, .. } = POLYMARKET.book_shape(class).spread else {
            panic!("{class:?} has a split lognormal");
        };
        let spreads = &mut s.spreads[class as usize];
        for (q, index) in [(0.1, 6), (0.5, 32), (0.9, 57)] {
            let (actual, expected) = (quantile(spreads, q), i64::from(centibps.0[index]));
            assert!(
                near(actual as f64, expected as f64, 0.2),
                "{class:?} p{}: {actual} against {expected}",
                q * 100.0
            );
        }
    }
    // The tick-bound majors: always one real tick, 0.12 bps for BTC to 0.84 for SOL.
    let majors = &mut s.spreads[BookClass::Majors as usize];
    assert!((11..=85).contains(&quantile(majors, 0.0)) && quantile(majors, 1.0) <= 85, "{:?}", &majors[..10]);
    // Depth per quote: the recorded books' 10th, 50th and 90th percentiles in dollars
    // (calibration, book.md section 4): p50 within 15%, p90 within 30%, p10 under $100 where
    // the recorded one is (dust and small orders), else within 30%.
    let recorded = [
        (BookClass::Majors, [1_330, 12_300, 130_000]),
        (BookClass::AltCrypto, [11, 6_420, 71_100]),
        (BookClass::LongTailCrypto, [11, 3_020, 29_900]),
        (BookClass::TradfiEquities, [50, 24_900, 159_000]),
        (BookClass::TradfiMacro, [1_500, 51_100, 251_000]),
    ];
    for (class, [p10, p50, p90]) in recorded {
        let depth = &mut s.depth[class as usize];
        let actual = [0.1, 0.5, 0.9].map(|q| quantile(depth, q));
        let p10_ok = if p10 < 100 { actual[0] < 100 } else { near(actual[0] as f64, p10 as f64, 0.3) };
        assert!(p10_ok, "{class:?}: {actual:?} against {:?}", [p10, p50, p90]);
        assert!(
            near(actual[1] as f64, p50 as f64, 0.15),
            "{class:?}: {actual:?} against {:?}",
            [p10, p50, p90]
        );
        assert!(
            near(actual[2] as f64, p90 as f64, 0.3),
            "{class:?}: {actual:?} against {:?}",
            [p10, p50, p90]
        );
        // The dust share of quotes against the class's (each level weighs the same in both),
        // within a fifth and 0.3 points.
        let dust = depth.iter().filter(|&&usd| usd < 12).count() as f64 / depth.len() as f64;
        let expected = f64::from(POLYMARKET.book_shape(class).dust_ppm) / 1e6;
        assert!(
            (dust - expected).abs() <= 0.2 * expected + 0.003,
            "{class:?}: dust {dust:.3} against {expected:.3}"
        );
    }
}

/// The sample's shape, cloned: tests that sort its samples need their own copy.
fn sample_shape() -> Shape {
    let s = &sample().1.shape;
    Shape {
        spreads: s.spreads.clone(),
        depth: s.depth.clone(),
        level_depth: s.level_depth.clone(),
        distances: s.distances.clone(),
        ..Shape::default()
    }
}

#[test]
fn depth_by_level_is_the_profiles() {
    // The median notional at levels 1, 3, 5, 10 and 20, sampled at every mark, against the
    // profile's median for that level (`profile/tests.rs` checks those against the recorded
    // books, book.md section 4), within 25%: level 1 is thin ($1k to $3.7k) and level 3
    // holds 1.3 to 20 times more. With a size drawn alike at every level, level 1 was 3 to 17
    // times too deep (review finding [depth-by-level]). A quote keeps its size until it is
    // resized or moved, so the majors' and tradfi macro's 3 and 6 markets, whose sizes spread
    // over two decades, are held to 40%.
    let s = &mut sample_shape();
    for class in BookClass::ALL {
        let shape = POLYMARKET.book_shape(class);
        let tolerance = if matches!(class, BookClass::Majors | BookClass::TradfiMacro) { 0.4 } else { 0.25 };
        for level in [1, 3, 5, 10, 20] {
            let actual = quantile(&mut s.level_depth[class as usize][level - 1], 0.5);
            let expected = shape.median_usd(level);
            assert!(
                near(actual as f64, f64::from(expected), tolerance),
                "{class:?} level {level}: {actual} against {expected}"
            );
        }
    }
}

#[test]
fn gaps_between_levels_keep_the_profiles_shares() {
    // Sampled at every mark over 10 s of flow, per class and gap bucket: the share of gaps of
    // one real tick within 5 points of the profile's, and of gaps over 10 ticks within 4
    // points. Re-prices and spread changes draw the gaps next to the quote they move together
    // and ladders follow a move whole (`market.rs`, "Gaps"), so the ladder keeps its gaps
    // however often it is re-priced; a fresh gap before a moved quote left the one after it a
    // remainder and cut the top 1-tick share by up to 20 points (review finding
    // [gaps-drift]). Levels 10 to 19 may hold up to 10 points fewer gaps over 10 ticks than
    // recorded: a gap is drawn among those that stay within reach, and the recorded books'
    // far orders don't. A ladder relaxes slowly (only its last gap is ever drawn afresh), so
    // the majors' and tradfi macro's 3 and 6 markets are held to 8 points.
    let s = &sample().1.shape;
    for class in BookClass::ALL {
        let few_markets = matches!(class, BookClass::Majors | BookClass::TradfiMacro);
        let (one_tolerance, over_tolerance) = if few_markets { (0.08, 0.08) } else { (0.05, 0.04) };
        for bucket in 0..3 {
            let [all, one, over] = s.gaps[class as usize][bucket];
            let gaps = &POLYMARKET.book_shape(class).gaps[bucket];
            let (one, over) = (one as f64 / all as f64, over as f64 / all as f64);
            let (one_expected, over_expected) =
                (f64::from(gaps.one_to_ten_ppm[0]) / 1e6, f64::from(gaps.beyond_ten_ppm) / 1e6);
            assert!(
                (one - one_expected).abs() < one_tolerance,
                "{class:?} {bucket}: 1 tick {one:.3}, not {one_expected:.3}"
            );
            let low = if bucket == 2 { 0.1 } else { over_tolerance };
            assert!(
                over < over_expected + over_tolerance && over > over_expected - low,
                "{class:?} {bucket}: over 10 ticks {over:.3}, not {over_expected:.3}"
            );
        }
    }
}

#[test]
fn levels_sit_about_as_far_from_the_mid_as_recorded() {
    // The median distance from the mid of levels 1, 5, 10 and 20, in hundredths of a bps,
    // against the recorded books' (book.md, section 3, other hours). Levels 1, 5 and 10
    // within 50% (75% for the majors' and tradfi macro's 3 and 6 slowly relaxing ladders):
    // the recorded ones mix the thin sides; tradfi macro's level 1 is left out, since three of
    // its six markets' spread is one real tick here, and 1 to 44 recorded.
    // Level 20 within 2.5 times, and inside the reach: the gaps are drawn independently,
    // while the recorded sides are either compact or sparse (half of the equities' 20-level
    // sides have no gap over 10 ticks after level 10, against 15% if independent), so level
    // 20 sits further out at the median than recorded. Before the gap model was fitted on the
    // sides that show all 20 levels, it sat at the reach, 3 to 10 times too far (review
    // finding [ladder-reach]).
    let s = &mut sample_shape();
    let recorded = [
        (BookClass::Majors, [18, 313, 530, 1_410]),
        (BookClass::AltCrypto, [252, 601, 1_350, 6_940]),
        (BookClass::LongTailCrypto, [275, 672, 1_570, 9_850]),
        (BookClass::TradfiEquities, [222, 549, 985, 2_150]),
        (BookClass::TradfiMacro, [52, 225, 390, 887]),
    ];
    for (class, levels) in recorded {
        for (i, expected) in levels.into_iter().enumerate() {
            let actual = quantile(&mut s.distances[class as usize][i], 0.5);
            let level = [1, 5, 10, 20][i];
            if level == 1 && class == BookClass::TradfiMacro {
                continue;
            }
            let few_markets = matches!(class, BookClass::Majors | BookClass::TradfiMacro);
            let tolerance = match (level, few_markets) {
                (20, _) => 1.5,
                (_, true) => 0.75,
                (_, false) => 0.5,
            };
            assert!(
                near(actual as f64, expected as f64, tolerance),
                "{class:?}: level {level} at {actual} against {expected}"
            );
        }
    }
}

#[test]
fn takers_are_the_recorded_share_and_notional() {
    let s = &sample().1.shape;
    // IOCs are 695 ppm of client messages, within 12% (about 700 in the sample): the
    // high-leverage adds' 100 ppm (a clock, so exact), and the takers' 595 (random clusters).
    // The adds used to come on top of the takers' 695 (review finding [iocs-share]).
    let clients = s.makers() + s.takers + s.cohort_iocs;
    let [takers, cohort] = [s.takers, s.cohort_iocs].map(|count| count as f64 * 1e6 / clients as f64);
    assert!(near(takers + cohort, 695.0, 0.12), "{takers:.0} ppm takers and {cohort:.0} ppm adds");
    assert!(near(cohort, 100.0, 0.01), "{cohort:.1} ppm high-leverage adds");
    // The notional sampler against the calibration's (flow.md, "QQ check": the mixture
    // sampler's quantiles, which the table reproduces), and its point masses.
    let mut rng = SplitMix64::new(3);
    let mut cents: Vec<i64> = (0..200_000).map(|_| draw_taker_cents(&mut rng) as i64).collect();
    let share = |keep: &dyn Fn(i64) -> bool| cents.iter().filter(|&&c| keep(c)).count() as f64 / 200_000.0;
    assert!((share(&|c| (1_050..1_160).contains(&c)) - 0.146).abs() < 0.005, "dust");
    assert!((share(&|c| c == 100_000) - 0.0665).abs() < 0.003, "$1,000");
    assert!(cents.iter().all(|&c| c >= 1_050), "at least $10.50");
    let [p50, p90, p99] = [0.5, 0.9, 0.99].map(|q| quantile(&mut cents, q) as f64 / 100.0);
    assert!(
        near(p50, 398.0, 0.1) && near(p90, 2_947.0, 0.1) && near(p99, 19_839.0, 0.15),
        "{p50} {p90} {p99}"
    );
}

#[test]
fn marks_come_every_200_ms_per_market_and_move_once_a_second() {
    let (_, mut flow) = PolymarketFlow::start(PolymarketConfig::default());
    let mut last: HashMap<MarketId, (u64, Price)> = HashMap::new();
    let mut changes = 0;
    while flow.flow_ns() < 3 * SECOND_NS {
        let item = flow.next().expect("endless");
        let Command::SetMark(mark) = item.command() else { continue };
        if let Some((time, price)) = last.insert(mark.market, (flow.flow_ns(), mark.price)) {
            assert_eq!(flow.flow_ns() - time, 200_000_000, "{mark:?}");
            // Only every 5th tick moves: those are whole seconds after the market's first.
            if price != mark.price {
                changes += 1;
            }
        }
    }
    assert_eq!(last.len(), 88);
    // 88 markets, 2 steps each so far, 62% to 74% of them still.
    assert!((20..=90).contains(&changes), "{changes} moves");
}

#[test]
fn fair_values_move_as_the_recorded_marks_do() {
    // 300 s of flow at a low message rate: 26,400 steps. Per leverage class, the share of
    // seconds without a move and the RMS of the moves in units of each market's `rms`.
    let config = PolymarketConfig { messages_per_flow_second: 1_000, ..PolymarketConfig::default() };
    let (_, mut flow) = PolymarketFlow::start(config);
    let mut last: HashMap<MarketId, Price> = HashMap::new();
    // Per class: steps, steps with a move, and the sum of (move / rms)^2.
    let mut per_class = [(0u64, 0u64, 0f64); 4];
    let mut ticks: HashMap<MarketId, u64> = HashMap::new();
    while flow.flow_ns() < 300 * SECOND_NS {
        let item = flow.next().expect("endless");
        let Command::SetMark(mark) = item.command() else { continue };
        let tick = ticks.entry(mark.market).or_default();
        *tick += 1;
        let previous = last.insert(mark.market, mark.price);
        if !tick.is_multiple_of(5) {
            continue; // not a step
        }
        let market = spec(mark.market);
        let class = market.moves.class as usize;
        let before = previous.expect("a mark before");
        per_class[class].0 += 1;
        if mark.price != before {
            per_class[class].1 += 1;
            let bps = (mark.price - before).ticks() as f64 / before.ticks() as f64 * 1e4;
            per_class[class].2 += (bps / (f64::from(market.move_rms_millibps) / 1e3)).powi(2);
        }
    }
    for class in LeverageClass::ALL {
        let (steps, moves, squares) = per_class[class as usize];
        let still = 1.0 - moves as f64 / steps as f64;
        let expected = f64::from(POLYMARKET.moves_of(class).no_move_ppm) / 1e6;
        let rms = (squares / moves as f64).sqrt();
        assert!((still - expected).abs() < 0.04, "{class:?}: still {still:.3} against {expected:.3}");
        // At least one real tick per move lifts the smallest moves: within 25% of 1.
        assert!((0.75..1.35).contains(&rms), "{class:?}: rms {rms:.2}");
    }
}

// ---------------------------------------------------------------------------------------
// Jumps, shocks and the cohorts they liquidate.

/// Checks that a jump or shock move's `SetMark` is followed by the pull of every quote of its
/// market (40 cancels) and 40 new post-only quotes; unless the plan ends first, at its last
/// client item (module docs).
fn check_requote(plan: &FlowPlan<PolymarketConfig>, jump: &Jump) {
    if jump.item + 81 > plan.timed.len() {
        return;
    }
    assert_eq!(
        *plan.timed[jump.item].command(),
        Command::SetMark(SetMark { price: jump.to, market: jump.market })
    );
    let pulled = &plan.timed[jump.item + 1..jump.item + 41];
    assert!(
        pulled
            .iter()
            .all(|item| matches!(item.command(), Command::CancelOrder(c) if c.market == jump.market))
    );
    let placed = &plan.timed[jump.item + 41..jump.item + 81];
    assert!(placed.iter().all(
        |item| matches!(item.command(), Command::PlaceOrder(p) if p.post_only && p.market == jump.market)
    ));
}

#[test]
fn every_jump_is_marked_at_once_and_requotes_its_market() {
    let config =
        PolymarketConfig { messages_per_flow_second: 5_000, jump_one_in: 50, ..PolymarketConfig::default() };
    let plan = generate(&config, 100_000);
    assert!(plan.jumps.len() > 20, "{} jumps", plan.jumps.len());
    for jump in &plan.jumps {
        check_requote(&plan, jump);
        // 50 to 123 bps (the recorded sizes), to the grid.
        let bps = (jump.to - jump.from).ticks().abs() as f64 / jump.from.ticks() as f64 * 1e4;
        assert!((49.5..124.0).contains(&bps), "{jump:?}: {bps:.1} bps");
        // Jumps land on whole seconds of their market's marks: every 5th tick.
        let index = POLYMARKET.markets.iter().position(|m| m.market_id() == jump.market).expect("a market");
        let phase = index as u64 * 200_000_000 / 88;
        assert_eq!((jump.flow_ns - phase) % SECOND_NS, 0, "{jump:?}");
    }
}

#[test]
fn shocks_move_many_markets_together_as_their_preset_says() {
    for shock in [ShockConfig::real(), ShockConfig::stress()] {
        let shock = ShockConfig { every_ns: SECOND_NS, ..shock };
        let config = PolymarketConfig {
            messages_per_flow_second: 20_000,
            shock: Some(shock),
            ..PolymarketConfig::default()
        };
        let plan = generate(&config, 200_000);
        // Group the moves by shock: each within 52 ms of a whole second.
        let mut shocks: BTreeMap<u64, Vec<&Jump>> = BTreeMap::new();
        for jump in &plan.jumps {
            check_requote(&plan, jump);
            let second = jump.flow_ns / SECOND_NS;
            assert!(jump.flow_ns - second * SECOND_NS <= 52_000_000, "{jump:?}");
            shocks.entry(second).or_default().push(jump);
        }
        assert!(shocks.len() >= 8, "{} shocks", shocks.len());
        for moves in shocks.values() {
            assert!((14..=88).contains(&moves.len()), "{} movers", moves.len());
            let ups = moves.iter().filter(|jump| jump.to > jump.from).count();
            for jump in moves {
                let size = (jump.to - jump.from).ticks().abs() as f64 / jump.from.ticks() as f64;
                match shock.size {
                    ShockSize::Stress => {
                        assert!(ups == 0 || ups == moves.len(), "all one way");
                        assert!((0.0195..0.0605).contains(&size), "{jump:?}");
                    }
                    ShockSize::Calibrated => {
                        let rms = f64::from(spec(jump.market).move_rms_millibps) / 1e7;
                        let sigmas = size / rms;
                        // 4 to 8 of its rms, to within a real tick.
                        let tick = real_tick(jump.from).ticks() as f64 / jump.from.ticks() as f64 / rms;
                        assert!(sigmas > 4.0 - tick && sigmas < 8.0 + tick, "{jump:?}: {sigmas:.2} sigmas");
                    }
                }
            }
            if shock.size == ShockSize::Calibrated {
                // Each mover keeps the shared direction with 95% chance.
                let majority = ups.max(moves.len() - ups);
                assert!(majority * 100 >= moves.len() * 70, "{ups} up of {}", moves.len());
            }
        }
    }
}

#[test]
fn the_stress_shock_liquidates_the_cascade_cohort_together() {
    let shock = ShockConfig { every_ns: 2 * SECOND_NS, ..ShockConfig::stress() };
    let config = PolymarketConfig {
        messages_per_flow_second: 20_000,
        shock: Some(shock),
        ..PolymarketConfig::default()
    };
    let plan = generate(&config, 200_000); // 10 s: 5 shocks
    assert_eq!(plan.setup_b2.len(), 176 * 3 + 88 * 8);
    assert_eq!(
        config.cohort_of(acct(CASCADE_BASE + 3)),
        Some(Cohort::Cascade { market: MarketId::new(1), side: Side::Buy })
    );
    assert_eq!(
        config.cohort_of(acct(CASCADE_BASE + 4)),
        Some(Cohort::Cascade { market: MarketId::new(1), side: Side::Sell })
    );
    let mut engine = engine_after_setup(&plan);
    let applied = apply(&mut engine, &plan.timed);
    // One shock's mark liquidates the wrong side of a market's cohort together: 4 accounts,
    // and the high-leverage one.
    let most = applied.liquidations.iter().map(|(_, count)| *count).max().unwrap_or(0);
    assert!(most >= 4, "at most {most} liquidations in one command");
    assert!(applied.liquidated() >= 50, "{} liquidations", applied.liquidated());
    assert!(applied.liquidations.iter().all(|(command, _)| matches!(command, Command::SetMark(_))));
    let clients = plan.timed.iter().filter(|item| item.is_client()).count() as u64;
    assert!(percent(applied.rejects.len() as u64, clients) < 5.0, "{} rejects", applied.rejects.len());
    engine.assert_invariants();
}

#[test]
fn a_large_jump_liquidates_the_wrong_side_high_leverage_account() {
    // Every step a jump: the 50x markets' jumps of 1% or more liquidate their wrong side.
    let config =
        PolymarketConfig { messages_per_flow_second: 2_000, jump_one_in: 1, ..PolymarketConfig::default() };
    let plan = generate(&config, 40_000); // 20 s
    let mut engine = engine_after_setup(&plan);
    let applied = apply(&mut engine, &plan.timed);
    assert!(applied.liquidated() >= 1);
    engine.assert_invariants();
}

// ---------------------------------------------------------------------------------------
// Makers.

#[test]
fn the_default_makers_spread_evenly_over_the_gateways() {
    // Default: 60 makers, each 1.3% to 2.1% of maker messages (1/60 is 1.67%): the groups
    // carry about the same weight, and the best quotes turn over the makers.
    let (plan, view) = sample();
    let per_maker = &view.shape.per_maker;
    let total: u64 = per_maker.values().sum();
    assert_eq!(per_maker.len(), 60);
    assert!(per_maker.values().all(|&count| (1.3..2.1).contains(&percent(count, total))), "{per_maker:?}");
    // An account verifies on gateway `account mod N`. With 60 makers, every gateway count
    // that divides 60 gets within 5% of the mean load, and 7 or 8 gateways within 10%; with
    // 12 makers, 10 gateways put 17.4% and 15.8% of the messages on gateways 1 and 2 (review
    // finding [makers-per-gateway]).
    for (gateways, tolerance) in
        [(2, 0.05), (3, 0.05), (4, 0.05), (5, 0.05), (6, 0.05), (7, 0.1), (8, 0.1), (10, 0.05)]
    {
        let mut load = vec![0u64; gateways];
        for item in &plan.timed {
            if let Item::Client(client) = item {
                load[(client.account.get() % gateways as u32) as usize] += 1;
            }
        }
        let mean = load.iter().sum::<u64>() as f64 / gateways as f64;
        assert!(load.iter().all(|&n| near(n as f64, mean, tolerance)), "{gateways} gateways: {load:?}");
    }
}

#[test]
fn makers_k_concentrates_market_making_in_k_accounts() {
    // K = 3: three accounts quote every market, each about a third of all traffic.
    let config = PolymarketConfig { makers_k: Some(3), ..PolymarketConfig::default() };
    let plan = generate(&config, 200_000);
    let view = View::of(&plan);
    let per_maker = &view.shape.per_maker;
    let total: u64 = per_maker.values().sum();
    assert_eq!(per_maker.keys().copied().collect::<Vec<_>>(), [1, 2, 3].map(acct));
    assert!(per_maker.values().all(|&count| (30.0..37.0).contains(&percent(count, total))), "{per_maker:?}");
    assert_eq!(config.client_accounts().len(), 3 + 1_000 + 176);
    let leverage = plan
        .setup_a
        .iter()
        .filter(|item| matches!(item.command(), Command::SetLeverage(l) if l.account <= acct(3)));
    assert_eq!(leverage.count(), 3 * 88);
    // K = 1: one account sends every maker message.
    let one = generate(&PolymarketConfig { makers_k: Some(1), ..PolymarketConfig::default() }, 20_000);
    assert!(one.client_items().all(|item| item.account != acct(2)));
    // K = 20, the most: every maker holds one bid and one ask rank in every market, and sends.
    let twenty = generate(&PolymarketConfig { makers_k: Some(20), ..PolymarketConfig::default() }, 20_000);
    let makers: std::collections::BTreeSet<AccountId> = twenty
        .timed
        .iter()
        .filter_map(|item| match item {
            Item::Client(client) if client.account <= acct(20) => Some(client.account),
            _ => None,
        })
        .collect();
    assert_eq!(makers.len(), 20);
}

#[test]
fn check_refuses_a_config_the_generator_cannot_run() {
    let config = PolymarketConfig::default();
    assert_eq!(config.check(), Ok(()));
    let stress = PolymarketConfig { shock: Some(ShockConfig::stress()), ..config };
    assert_eq!(stress.check(), Ok(()));
    for bad in [
        PolymarketConfig { messages_per_flow_second: 999, ..config },
        PolymarketConfig { messages_per_flow_second: 100_000_000, ..config },
        PolymarketConfig { makers: 13, ..config }, // not whole groups of 3
        PolymarketConfig { makers: 300, ..config }, // 100 groups for 88 markets
        PolymarketConfig { makers: 42, makers_per_market: 21, ..config }, // a 21st maker never quotes
        PolymarketConfig { makers_k: Some(0), ..config },
        PolymarketConfig { makers_k: Some(21), ..config }, // makers 22 on would never quote
        PolymarketConfig { high_leverage_ppm: 695, ..config }, // no room left for the takers
        PolymarketConfig { takers: 0, ..config },
        PolymarketConfig { jump_one_in: 0, ..config },
        PolymarketConfig { high_leverage_per_market: 23, ..config }, // 2,024 accounts
        PolymarketConfig { high_leverage_per_market: u32::MAX, messages_per_flow_second: u64::MAX, ..config },
        PolymarketConfig { high_leverage_notional: DOLLAR, ..config },
        PolymarketConfig { high_leverage_ppm: 0, ..config },
        PolymarketConfig {
            shock: Some(ShockConfig { every_ns: 100_000_000, ..ShockConfig::real() }),
            ..config
        },
        PolymarketConfig {
            shock: Some(ShockConfig { cascade_per_market: 101, ..ShockConfig::stress() }),
            ..config
        },
        // Absurd sizes are refused, not overflowed.
        PolymarketConfig {
            shock: Some(ShockConfig {
                cascade_per_market: u32::MAX,
                spread_ns: u64::MAX,
                ..ShockConfig::stress()
            }),
            ..config
        },
        PolymarketConfig { takers: 4_000, shock: Some(ShockConfig::stress()), ..config }, // 5,880 accounts
    ] {
        assert!(bad.check().is_err(), "{bad:?}");
    }
}

// ---------------------------------------------------------------------------------------
// Signing and sending.

#[test]
fn both_signing_schemes_sign_the_flow_and_the_sender_takes_it() {
    use crate::keys::signing_key;
    use crate::presign::{message_account, presign, presign_eip712, preverified};
    use crate::sender::{Messages, SenderPlan, Timing};
    use gateway::eip712::Domain;
    use gateway::wire::{decode, decode_eip712, signature, signed_part, verify_signature};
    use gateway::{PublicKey, VerifierKind};
    use pipeline::records::AuthScheme;

    let plan = generate(&PolymarketConfig::default(), 300);
    let clients: Vec<&ClientItem> = plan.client_items().collect();
    let perp = presign(&plan, 77, 4);
    let eip712 = presign_eip712(&plan, 77, 1_790_000_000_000, 4);
    assert_eq!((perp.len(), eip712.len()), (clients.len(), clients.len()));
    assert_eq!((perp.header().flow_digest, eip712.header().auth), (plan.config.digest(), AuthScheme::Eip712));
    let domain = Domain::new(77);
    // Every 7th message: decoded to its item, signed by its account's key.
    for (index, item) in clients.iter().enumerate().step_by(7) {
        let key = PublicKey::K256(*signing_key(plan.config.seed, item.account).verifying_key());
        let message = perp.message_bytes(index);
        let decoded = decode(&message).expect("the gateway decodes it");
        assert_eq!(
            (decoded.account, decoded.nonce, decoded.command),
            (item.account, item.nonce, item.command)
        );
        assert_eq!(verify_signature(&key, signed_part(&message), signature(&message)), Ok(()));
        let message = eip712.message_bytes(index);
        let decoded = decode_eip712(&message).expect("the gateway decodes it");
        assert_eq!(
            (decoded.account, decoded.salt, decoded.command),
            (item.account, item.nonce, item.command)
        );
        let check = gateway::wire::check_signer(
            VerifierKind::K256,
            &domain,
            &decoded,
            signature(&message),
            &key.address(),
        );
        assert_eq!(check, Ok(()));
        assert_eq!(message_account(eip712.message(index)), item.account);
    }
    // The sender plans the flow like the M3 flow's.
    let timing = Timing {
        arrivals: crate::schedule::Arrivals::Cox(crate::schedule::Bursts::Median),
        setup_rate: 20_000,
        rate: 100_000,
        timed_clients: 300,
        warmup_ns: 0,
        window_ns: u64::MAX,
    };
    let sender =
        SenderPlan::new(&plan, Messages::PreVerified(std::sync::Arc::new(preverified(&plan))), &timing);
    assert_eq!(sender.phases.iter().map(|phase| phase.clients()).sum::<usize>(), clients.len());
}

#[test]
fn the_generator_keeps_its_ladders_whole_and_uncrossed_between_events() {
    // `market.rs`'s invariants, on the generator's own state, with shocks and frequent jumps
    // so that every path runs: checked after every 97th event, in every market.
    let shock = ShockConfig { every_ns: SECOND_NS, ..ShockConfig::stress() };
    let config = PolymarketConfig {
        messages_per_flow_second: 20_000,
        jump_one_in: 20,
        shock: Some(shock),
        ..PolymarketConfig::default()
    };
    let (_, mut flow) = PolymarketFlow::start(config);
    let mut events: u64 = 0;
    for _ in 0..300_000 {
        flow.next().expect("endless");
        if flow.returned < flow.pending.len() {
            continue; // in the middle of an event
        }
        events += 1;
        if !events.is_multiple_of(97) {
            continue;
        }
        for market in &flow.markets {
            let symbol = market.spec.symbol;
            assert!(
                on_grid(market.fair) && (market.fair_low..=market.fair_high).contains(&market.fair),
                "{symbol}"
            );
            assert_eq!((market.bids.len(), market.asks.len()), (20, 20), "{symbol}");
            assert!(market.bids.windows(2).all(|pair| pair[0].price > pair[1].price), "{symbol}");
            assert!(market.asks.windows(2).all(|pair| pair[0].price < pair[1].price), "{symbol}");
            assert!(market.bids[0].price <= market.fair && market.asks[0].price > market.fair, "{symbol}");
            for quote in market.bids.iter().chain(&market.asks) {
                assert!(
                    on_grid(quote.price)
                        && (quote.price - market.fair).ticks().abs() <= market.reach().ticks(),
                    "{symbol}: {quote:?}"
                );
            }
        }
    }
    assert!(flow.jumps().len() > 100, "{} jumps and shock moves", flow.jumps().len());
}
