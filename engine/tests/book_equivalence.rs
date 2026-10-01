//! Property tests: the production book behaves exactly like the reference book.
//!
//! Random sequences of commands are applied to both books: places (GTC or IOC, some
//! post-only), cancels, modifies, and now and then a cancel of all of one account's orders
//! (what liquidation does) or of every order beyond a price on one side (what the engine's
//! price-band sweep does). After every command the two must have emitted identical events,
//! hold identical resting orders (including what each has filled), and report the same best
//! bid and ask; and the book may not be crossed. The two lookups the engine makes also
//! have to agree: `order` for every resting order and for the id the step used, and
//! `open_quantities` for every account.
//!
//! **Keeping orders interacting.** There are only four accounts, so orders often
//! self-trade. Three places in four are *passive*: if their price would reach the best
//! opposite price, it is moved to just short of it, so they rest and build queues (about 7
//! orders rest once a scenario is under way, often several at one price). The other places
//! keep their price and often trade. Cancels and modifies mostly go to an order that is
//! resting when the step runs.
//!
//! **Two price ranges**, each with scenarios of up to 200 commands.
//! - `production_book_matches_reference_book` (512 scenarios): 21 ticks, with most prices
//!   in the middle 11. Prices include both ends of the range, where an array-indexed book
//!   has its classic off-by-one.
//! - `production_book_matches_reference_book_over_a_wide_price_range` (256 scenarios):
//!   300,000 ticks, so the level index has four layers. Prices cluster within two ticks of
//!   where a word of the level index begins, of both ends of the range, and of a dense
//!   band in the middle. Orders in the same cluster trade with each other, and whenever a
//!   best level empties or an order sweeps several clusters, the book has to search across
//!   a word, a layer-1 word or a layer-2 word for the next level. (Measured: the best price
//!   moves to another layer-1 word in about 1 step in 10, and to another layer-2 word in
//!   about 1 in 30.)
//!
//! **Modifies** are generated relative to the target order's state when the step runs (see
//! `ModifyKind`), so that every modify rule is taken often: a new total size at or below
//! what has filled, a shrink in place, the same size, an increase, a new price, a price
//! that crosses the book, and a post-only order moved to a crossing price. (Measured: each
//! of these takes roughly 7-15% of modifies, except the post-only reject at about 4%.)
//!
//! **Invalid commands.** About 1 command in 7 is malformed (zero or negative quantity,
//! out-of-range price) or reuses an earlier id, which the book rejects only while that id
//! is still resting (D-008). Some cancels and modifies go to ids that have already gone,
//! so the reject paths are compared too.
//!
//! **Internal checks.** The narrow test also runs the book's full internal check
//! (`Book::assert_consistent`) after every step, so a broken link fails even before it
//! changes an event. That check walks every level, all 300,000 in the wide range, so the
//! wide test runs it once, at the end of each scenario; after every step it still compares
//! snapshots, and taking one walks the level index across every gap between clusters.
//!
//! Failure persistence is off: when this finds a bug, the shrunk failing case printed in
//! the panic message becomes a named regression test (`docs/DECISIONS.md` D-009).

use engine::book::{Book, BookConfig, OrderBook, RestingOrder};
use engine::command::PlaceOrder;
use engine::event::{CancelReason, Event};
use engine::reference::ReferenceBook;
use engine::types::{AccountId, OrderId, Price, Qty, Side, TimeInForce, account_of, order_id};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;

const MARKET: u16 = 1;

/// The narrow market: 21 ticks.
const NARROW: BookConfig = BookConfig { market: MARKET, min_price: 90, max_price: 110 };

/// The wide market: 300,000 ticks, so the level index has four layers (300,000 levels fill
/// 4,688 words; the layers above have 74, 2 and 1).
const WIDE: BookConfig = BookConfig { market: MARKET, min_price: 1, max_price: 300_000 };

/// Levels (price minus `min_price`) on either side of where a word of the level index
/// begins: a layer-0 word covers 64 levels, a layer-1 word 4,096 and a layer-2 word
/// 262,144 (`level_index.rs`).
const BOUNDARY_LEVELS: [Price; 9] = [63, 64, 65, 4_095, 4_096, 4_097, 262_143, 262_144, 262_145];

/// One step of a generated scenario. Order ids are resolved when the step runs, so cancels
/// and modifies usually hit orders that exist.
#[derive(Clone, Debug)]
enum Step {
    Place {
        account: AccountId,
        side: Side,
        price: Price,
        qty: Qty,
        tif: TimeInForce,
        post_only: bool,
        /// Move the price, if needed, to just short of the best opposite price, so that
        /// the order rests rather than trades. This keeps the book several orders deep.
        passive: bool,
    },
    /// Place again with an id that was already used (resting or not).
    PlaceReusedId {
        target: Index,
        side: Side,
        price: Price,
        qty: Qty,
        tif: TimeInForce,
        post_only: bool,
    },
    Cancel {
        target: Target,
    },
    Modify {
        target: Target,
        kind: ModifyKind,
    },
    /// Cancel all of one account's resting orders (what liquidation does).
    CancelAccount {
        account: AccountId,
    },
    /// Cancel every bid priced above (or ask priced below) a limit `depth` ticks inside the
    /// best price on that side (what the engine's price-band sweep does). At depth 0 the
    /// limit is the best price itself, so nothing goes.
    CancelBeyond {
        side: Side,
        depth: Price,
    },
}

/// Which order a cancel or modify goes to.
#[derive(Clone, Debug)]
enum Target {
    /// One of the orders resting when the step runs.
    Resting(Index),
    /// Any id issued so far; usually one that has already gone.
    AnyIssued(Index),
}

/// What a modify asks for, relative to the target order's state when the step runs, so
/// that each modify rule (`reference.rs` module docs) is taken often. Sizes are *total*
/// sizes: what has filled plus what is left to fill.
#[derive(Clone, Debug)]
enum ModifyKind {
    /// Same price, a total size from what has filled down to `below` less (but at least
    /// 1): nothing is left to fill, so the order is removed (`SizeBelowFilled`). Goes to a
    /// partly filled order if there is one; on an unfilled order the size is 0, which is
    /// rejected.
    AtOrBelowFilled { below: Qty },
    /// Same price, `by` less left to fill (but at least 1): updated in place.
    Shrink { by: Qty },
    /// Same price and size: updated in place, and still reported as `Modified`.
    SameSize,
    /// Same price, `by` more left to fill: back of the queue.
    Grow { by: Qty },
    /// Same size at a generated price: back of the queue at the new price. The price may
    /// also be the current one, cross the book, or be out of range.
    NewPrice { price: Price },
    /// Same size, at the best opposite price or `through` ticks past it (within the range):
    /// trades like a new order, or is rejected if the order is post-only.
    Cross { through: Price },
    /// A generated price and size, either of which may be invalid.
    Raw { price: Price, size: Qty },
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Buy), Just(Side::Sell)]
}

fn tif() -> impl Strategy<Value = TimeInForce> {
    prop_oneof![4 => Just(TimeInForce::Gtc), 1 => Just(TimeInForce::Ioc)]
}

/// Prices for the narrow market: mostly near the middle of the range (so orders interact),
/// some anywhere in the range including both ends, and a few just outside it.
fn narrow_price() -> BoxedStrategy<Price> {
    let (min, max) = (NARROW.min_price, NARROW.max_price);
    prop_oneof![
        16 => 95i64..=105,
        3 => min..=max,
        1 => Just(min),
        1 => Just(max),
        1 => prop_oneof![Just(min - 1), Just(max + 1), Just(0i64)],
    ]
    .boxed()
}

/// Prices for the wide market: mostly within two ticks of a level-index word boundary or
/// of either end of the range (a few of those fall just outside it), often in a dense band
/// in the middle, now and then anywhere.
fn wide_price() -> BoxedStrategy<Price> {
    let (min, max) = (WIDE.min_price, WIDE.max_price);
    let mut anchors: Vec<Price> = BOUNDARY_LEVELS.iter().map(|level| min + level).collect();
    anchors.extend([min, max]);
    prop_oneof![
        6 => (prop::sample::select(anchors), -2i64..=2).prop_map(|(anchor, offset)| anchor + offset),
        3 => 150_000i64..=150_020,
        1 => min..=max,
    ]
    .boxed()
}

/// Mostly valid quantities, sometimes zero or negative.
fn qty() -> impl Strategy<Value = Qty> {
    prop_oneof![20 => 1i64..=10, 1 => -1i64..=0]
}

fn target() -> impl Strategy<Value = Target> {
    prop_oneof![
        9 => any::<Index>().prop_map(Target::Resting),
        1 => any::<Index>().prop_map(Target::AnyIssued),
    ]
}

fn modify_kind(price: BoxedStrategy<Price>) -> impl Strategy<Value = ModifyKind> {
    prop_oneof![
        3 => (0i64..=2).prop_map(|below| ModifyKind::AtOrBelowFilled { below }),
        2 => (1i64..=5).prop_map(|by| ModifyKind::Shrink { by }),
        1 => Just(ModifyKind::SameSize),
        2 => (1i64..=5).prop_map(|by| ModifyKind::Grow { by }),
        3 => price.clone().prop_map(|price| ModifyKind::NewPrice { price }),
        3 => (0i64..=2).prop_map(|through| ModifyKind::Cross { through }),
        1 => (price, qty()).prop_map(|(price, size)| ModifyKind::Raw { price, size }),
    ]
}

/// A new order: 20% post-only, and three in four passive.
fn place(price: BoxedStrategy<Price>) -> impl Strategy<Value = Step> {
    let (post_only, passive) = (prop::bool::weighted(0.2), prop::bool::weighted(0.75));
    (1u32..=4, side(), price, qty(), tif(), post_only, passive).prop_map(
        |(account, side, price, qty, tif, post_only, passive)| Step::Place {
            account,
            side,
            price,
            qty,
            tif,
            post_only,
            passive,
        },
    )
}

fn place_reused_id(price: BoxedStrategy<Price>) -> impl Strategy<Value = Step> {
    (any::<Index>(), side(), price, qty(), tif(), prop::bool::weighted(0.2)).prop_map(
        |(target, side, price, qty, tif, post_only)| Step::PlaceReusedId {
            target,
            side,
            price,
            qty,
            tif,
            post_only,
        },
    )
}

/// One step, with prices drawn from `price`. About half the steps are places and 30%
/// modifies; cancels of one account's orders, and cancels beyond a price, are kept to 1
/// step in 21 each, because each one can remove a good part of the book.
fn step(price: BoxedStrategy<Price>) -> impl Strategy<Value = Step> {
    prop_oneof![
        10 => place(price.clone()),
        1 => place_reused_id(price.clone()),
        2 => target().prop_map(|target| Step::Cancel { target }),
        6 => (target(), modify_kind(price)).prop_map(|(target, kind)| Step::Modify { target, kind }),
        1 => (1u32..=4).prop_map(|account| Step::CancelAccount { account }),
        1 => (side(), 0i64..=3).prop_map(|(side, depth)| Step::CancelBeyond { side, depth }),
    ]
}

/// A step with its order id resolved, ready to apply to any book.
#[derive(Clone, Copy, Debug)]
enum Call {
    Place(PlaceOrder),
    Cancel(OrderId),
    Modify(OrderId, Price, Qty),
    CancelAccount(AccountId),
    CancelBeyond(Side, Price),
}

impl Call {
    fn apply(&self, book: &mut impl OrderBook, events: &mut Vec<Event>) {
        match *self {
            Call::Place(order) => book.place(&order, events),
            Call::Cancel(id) => book.cancel(id, events),
            Call::Modify(id, price, size) => book.modify(id, price, size, events),
            Call::CancelAccount(account) => book.cancel_account(account, CancelReason::Liquidation, events),
            Call::CancelBeyond(side, limit) => {
                book.cancel_beyond(side, limit, CancelReason::PriceBand, events)
            }
        }
    }

    /// The order id the call refers to, if any.
    fn order_id(&self) -> Option<OrderId> {
        match *self {
            Call::Place(order) => Some(order.order_id),
            Call::Cancel(id) | Call::Modify(id, _, _) => Some(id),
            Call::CancelAccount(_) | Call::CancelBeyond(_, _) => None,
        }
    }
}

/// Resolves generated steps into calls, tracking which ids have been issued so far.
struct Scenario {
    config: BookConfig,
    issued: Vec<OrderId>,
    next_seq: [u32; 5],
}

impl Scenario {
    fn new(config: BookConfig) -> Self {
        Scenario { config, issued: Vec::new(), next_seq: [0; 5] }
    }

    fn new_id(&mut self, account: AccountId) -> OrderId {
        let seq = &mut self.next_seq[account as usize];
        *seq += 1;
        let id = order_id(account, *seq);
        self.issued.push(id);
        id
    }

    /// An id that was issued earlier, or one that never existed if none were.
    fn existing_id(&self, target: &Index) -> OrderId {
        if self.issued.is_empty() {
            order_id(4, 999_999)
        } else {
            self.issued[target.index(self.issued.len())]
        }
    }

    /// Turns a step into a call. `book` is the reference book as it is before the step
    /// runs; modifies are sized and priced from its resting orders.
    fn resolve(&mut self, step: &Step, book: &ReferenceBook) -> Call {
        let place = |order_id, side, price, qty, tif, post_only| {
            Call::Place(PlaceOrder { order_id, price, qty, market: MARKET, side, tif, post_only })
        };
        match *step {
            Step::Place { account, side, price, qty, tif, post_only, passive } => {
                let price = if passive { passive_price(side, price, book) } else { price };
                place(self.new_id(account), side, price, qty, tif, post_only)
            }
            Step::PlaceReusedId { ref target, side, price, qty, tif, post_only } => {
                place(self.existing_id(target), side, price, qty, tif, post_only)
            }
            Step::Cancel { ref target } => Call::Cancel(self.target_id(target, &resting_orders(book), false)),
            Step::Modify { ref target, ref kind } => self.resolve_modify(target, kind, book),
            Step::CancelAccount { account } => Call::CancelAccount(account),
            Step::CancelBeyond { side, depth } => {
                // With that side empty, a limit at the end of the range.
                let limit = match side {
                    Side::Buy => book.best_bid().map_or(self.config.min_price, |bid| bid - depth),
                    Side::Sell => book.best_ask().map_or(self.config.max_price, |ask| ask + depth),
                };
                Call::CancelBeyond(side, limit)
            }
        }
    }

    /// The id a cancel or modify goes to. `resting` is every resting order; with
    /// `prefer_partly_filled`, a [`Target::Resting`] picks one that has filled something if
    /// there is one.
    fn target_id(&self, target: &Target, resting: &[RestingOrder], prefer_partly_filled: bool) -> OrderId {
        match target {
            Target::Resting(index) if !resting.is_empty() => {
                let partly_filled: Vec<&RestingOrder> =
                    resting.iter().filter(|order| order.filled > 0).collect();
                if prefer_partly_filled && !partly_filled.is_empty() {
                    partly_filled[index.index(partly_filled.len())].order_id
                } else {
                    resting[index.index(resting.len())].order_id
                }
            }
            // Nothing is resting, or the step asked for any issued id.
            Target::Resting(index) | Target::AnyIssued(index) => self.existing_id(index),
        }
    }

    fn resolve_modify(&self, target: &Target, kind: &ModifyKind, book: &ReferenceBook) -> Call {
        let resting = resting_orders(book);
        // Only an order that has filled something can be sized down to its fills.
        let prefer_partly_filled = matches!(kind, ModifyKind::AtOrBelowFilled { .. });
        let id = self.target_id(target, &resting, prefer_partly_filled);

        let Some(&order) = resting.iter().find(|order| order.order_id == id) else {
            // Not resting: whatever the price and size, the book must reject it as
            // `UnknownOrder` (the `Raw` kind sends invalid ones too).
            let (price, size) = match *kind {
                ModifyKind::Raw { price, size } => (price, size),
                _ => (self.config.min_price, 1),
            };
            return Call::Modify(id, price, size);
        };

        let total = order.filled + order.qty;
        let (price, size) = match *kind {
            ModifyKind::AtOrBelowFilled { below } => {
                let size = if order.filled == 0 { 0 } else { (order.filled - below).max(1) };
                (order.price, size)
            }
            ModifyKind::Shrink { by } => (order.price, order.filled + (order.qty - by).max(1)),
            ModifyKind::SameSize => (order.price, total),
            ModifyKind::Grow { by } => (order.price, total + by),
            ModifyKind::NewPrice { price } => (price, total),
            ModifyKind::Cross { through } => {
                let crossing = match order.side {
                    Side::Buy => book.best_ask().map(|ask| (ask + through).min(self.config.max_price)),
                    Side::Sell => book.best_bid().map(|bid| (bid - through).max(self.config.min_price)),
                };
                // With nothing on the other side to cross, the order stays where it is.
                (crossing.unwrap_or(order.price), total)
            }
            ModifyKind::Raw { price, size } => (price, size),
        };
        Call::Modify(id, price, size)
    }
}

/// `price`, moved if needed to just short of the best opposite price in `book`, so that an
/// order on `side` at that price rests rather than trades.
fn passive_price(side: Side, price: Price, book: &ReferenceBook) -> Price {
    match side {
        Side::Buy => book.best_ask().map_or(price, |ask| price.min(ask - 1)),
        Side::Sell => book.best_bid().map_or(price, |bid| price.max(bid + 1)),
    }
}

/// Every order resting in `book`, bids first.
fn resting_orders(book: &ReferenceBook) -> Vec<RestingOrder> {
    let snapshot = book.snapshot();
    snapshot.bids.into_iter().chain(snapshot.asks).collect()
}

/// Applies `steps` to a reference book and a production book over `config`, and checks
/// after every step that they agree (see the module docs). `assert_consistent_every_step`
/// runs the production book's full internal check after every step rather than only at
/// the end.
fn check_books_agree(
    config: BookConfig,
    steps: &[Step],
    assert_consistent_every_step: bool,
) -> Result<(), TestCaseError> {
    let mut reference = ReferenceBook::new(config);
    let mut production = Book::new(config);
    let mut scenario = Scenario::new(config);

    for (n, step) in steps.iter().enumerate() {
        let call = scenario.resolve(step, &reference);
        let mut expected = Vec::new();
        let mut actual = Vec::new();
        call.apply(&mut reference, &mut expected);
        call.apply(&mut production, &mut actual);

        prop_assert_eq!(&actual, &expected, "events differ at step {}: {:?}", n, call);
        let snapshot = production.snapshot();
        prop_assert_eq!(&snapshot, &reference.snapshot(), "books differ after step {}: {:?}", n, call);
        prop_assert!(!snapshot.is_crossed(), "book crossed after step {}: {:?}", n, call);
        if assert_consistent_every_step {
            production.assert_consistent();
        }
        // The fast book caches its best prices; they must match both the reference and
        // its own resting orders.
        prop_assert_eq!(production.best_bid(), reference.best_bid(), "best bid differs after step {}", n);
        prop_assert_eq!(production.best_ask(), reference.best_ask(), "best ask differs after step {}", n);
        prop_assert_eq!(production.best_bid(), snapshot.bids.first().map(|o| o.price));
        prop_assert_eq!(production.best_ask(), snapshot.asks.first().map(|o| o.price));
        check_lookups_agree(&production, &reference, call.order_id())?;
    }
    production.assert_consistent();
    Ok(())
}

/// The lookups the engine makes agree between the books and with the snapshot: `order`
/// for every resting order and for `step_id` (the id the step used, which may be gone), and
/// `open_quantities` for every account, which must sum the snapshot's orders.
fn check_lookups_agree(
    production: &Book,
    reference: &ReferenceBook,
    step_id: Option<OrderId>,
) -> Result<(), TestCaseError> {
    let resting = resting_orders(reference);
    for order in &resting {
        prop_assert_eq!(production.order(order.order_id), Some(*order));
    }
    if let Some(id) = step_id {
        prop_assert_eq!(production.order(id), reference.order(id), "order({:#x})", id);
    }
    for account in 1..=4 {
        let own = |side| {
            let orders = resting.iter().filter(|o| account_of(o.order_id) == account && o.side == side);
            orders.map(|o| o.qty).sum::<Qty>()
        };
        let expected = (own(Side::Buy), own(Side::Sell));
        prop_assert_eq!(production.open_quantities(account), expected, "account {}", account);
        prop_assert_eq!(reference.open_quantities(account), expected, "account {}", account);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn production_book_matches_reference_book(steps in prop::collection::vec(step(narrow_price()), 1..=200)) {
        check_books_agree(NARROW, &steps, true)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn production_book_matches_reference_book_over_a_wide_price_range(
        steps in prop::collection::vec(step(wide_price()), 1..=200)
    ) {
        check_books_agree(WIDE, &steps, false)?;
    }
}
