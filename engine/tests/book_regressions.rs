//! Named regression tests for the order books. Each one reproduces a bug that was found and
//! fixed, so that it can't come back. A failing case from the property tests
//! (`book_equivalence.rs`) also ends up here, shrunk and given a name (`docs/DECISIONS.md`
//! D-009). Regressions about allocation are in `no_alloc.rs`, which counts allocations.

use engine::book::{Book, BookConfig, OrderBook};
use engine::command::PlaceOrder;
use engine::event::Event;
use engine::reference::ReferenceBook;
use engine::types::{AccountId, MarketId, OrderSeq, Price, Qty, Side, TimeInForce, order_id};

/// A resting buy of one lot at `price`, from account 1.
fn bid(seq: u32, price: Price) -> PlaceOrder {
    PlaceOrder {
        order_id: order_id(AccountId::new(1), OrderSeq::new(seq)),
        price,
        qty: Qty::new(1),
        market: MarketId::new(1),
        side: Side::Buy,
        tif: TimeInForce::Gtc,
        post_only: false,
    }
}

/// Regression: the reference book ranked bids by `-price`, which overflows at the lowest
/// price, `i64::MIN` ticks.
#[test]
fn a_bid_at_price_min_ranks_below_a_higher_bid_in_both_books() {
    // Both books accept a price range that starts at `i64::MIN` ticks: the fast book only
    // limits the range's width. The overflow was a panic in debug builds, and in release
    // builds it wrapped, so the reference reported the lowest price as the best bid.
    let price_min = Price::new(i64::MIN);
    let config =
        BookConfig { market: MarketId::new(1), min_price: price_min, max_price: price_min + Price::new(10) };
    let mut fast = Book::new(config);
    let mut reference = ReferenceBook::new(config);

    let higher = bid(1, price_min + Price::new(5));
    let lowest = bid(2, price_min);
    for order in [higher, lowest] {
        let mut fast_events: Vec<Event> = Vec::new();
        let mut reference_events: Vec<Event> = Vec::new();
        fast.place(&order, &mut fast_events);
        reference.place(&order, &mut reference_events);
        assert_eq!(fast_events, reference_events);
    }

    fast.assert_consistent();
    assert_eq!(fast.best_bid(), Some(price_min + Price::new(5)));
    assert_eq!(fast.snapshot().bids.first().map(|o| o.order_id), Some(higher.order_id));

    assert_eq!(reference.best_bid(), fast.best_bid());
    assert_eq!(reference.snapshot(), fast.snapshot());
}
