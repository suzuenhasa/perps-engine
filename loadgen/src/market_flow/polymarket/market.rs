//! One market of the Polymarket-shaped flow as the generator sees it (`polymarket.rs`): its
//! fair value, which is also its latest mark, and its makers' two ladders of quotes. The
//! generator never sees the engine's book (open loop): a ladder holds the quotes as the
//! generator last sent them, whether takers have filled them since or not.
//!
//! **Contract.** Each method emits the items of one change to the market, in order, and
//! keeps the invariants below. It draws from the market's own streams only: `FAIR(m)` for the
//! fair value, `MM(m)` for everything its makers do. Within a method, draws happen in the
//! order the code makes them, which its docs list.
//!
//! **Invariants** (after every method; `tests.rs` checks them over whole plans):
//! - Each ladder holds 20 quotes (the profile's `levels_per_side`) at distinct prices on the
//!   market's real grid, best first.
//! - The book is never crossed: every bid is at or below the fair value, and every ask at
//!   least one real tick above it. So in plan order a post-only quote never crosses the
//!   generator's own book, and the engine's book is the generator's minus what takers filled.
//!   Through the pipeline, one caveat: the sequencer keeps order only within one account
//!   (PIPELINE.md 9.1), and a spread change, a ladder following a move or a requote cancels
//!   one maker's quote and places another maker's at or through its price, so the place can
//!   reach the engine first and be rejected `PostOnlyWouldCross` (about 0.01% of places at
//!   100k/s with shocks); the generator then holds a quote the engine lacks, whose later
//!   cancel or resize is `UnknownOrder`.
//! - Every quote is within the ladder's **reach** of the fair value: half the price band
//!   (D-015), so inside the band of the latest mark, which is the fair value, and inside the
//!   price range (the fair value stays far enough inside it, [`MarketState::new`]).
//! - The fair value is on its real grid, and every change of it is marked at once.
//!
//! **Prices.** Polymarket's grid has 5 significant figures (`profile.rs`): a *real tick* is
//! `10^max(0, digits − 5)` engine ticks ([`real_tick`]). Spreads, gaps and moves are counted
//! in real ticks of the fair value, and every price is rounded onto its own grid away from
//! the fair value; the two differ only near a power of ten. A price, and a distance between
//! prices, is a `Price` (engine ticks); a count of real ticks is a bare `i64`, turned into
//! engine ticks by [`ticks_of`] and back by [`real_ticks_in`], so the two can't be mixed up.
//!
//! **Gaps.** A ladder is its best quote, a spread from the fair value, and independent gaps
//! from its class's buckets (`profile.rs`, `Gaps`), and every change keeps it so:
//! - a new quote's gap is drawn among those that keep it within reach, so a ladder thins out
//!   toward the reach instead of stacking one tick apart at its edge;
//! - a re-price moves a quote between its two neighbours with the two gaps next to it drawn
//!   together given their sum ([`MarketState::draw_split`]), and a spread change draws the
//!   spread with the two gaps behind the best quotes ([`MarketState::spread_change`]): Gibbs
//!   steps, which leave the ladder's law as it was however often they run (each market's
//!   ladder relaxes slowly, since only its last gap is ever drawn outright);
//! - after a move of the fair value the ladders follow it whole
//!   ([`MarketState::follow_move`]), which changes no gap.
//!
//! **Complexity.** O(L) per quote placed, cancelled or resized, with `L` = 20 quotes a
//! side: finding a free price, and inserting into the ladder; O(G log G) per gap drawn, with
//! `G` at most 74 gaps a bucket can draw (1 to 10 ticks and the tail's 64 values); and
//! O(S log G) per spread change, with `S` the spread table's 64 values.

use engine::command::{Command, ModifyOrder, SetMark, SetMarketParams};
use engine::money::{band_edges, ceil_div};
use engine::types::{AccountId, MarketId, Micros, OrderId, Price, Qty, Side, TimeInForce};

use super::super::profile::{Gaps, Market, POLYMARKET, Spread, pick};
use super::super::{Clients, Item, NewOrder, stream};
use super::{MILLION, MakerMix, market_params, stream_ids};
use crate::SplitMix64;

/// Mark ticks per fair-value step: marks come every 200 ms (the profile's `mark_every_ms`), and
/// the calibrated moves are 1-s moves.
const TICKS_PER_STEP: u64 = 5;

/// The first rank (0-based) of each level band, levels 1–5, 6–10 and 11–20, and how many
/// ranks it holds (the profile's `maker_activity`).
const BAND_FIRST: [usize; 3] = [0, 5, 10];
const BAND_RANKS: [u64; 3] = [5, 5, 10];

/// Which of the profile's three gap buckets the gap after the quote at `rank` is in: after
/// levels 1–4, 5–9, or 10–19.
fn gap_bucket(rank: usize) -> usize {
    match rank {
        0..=3 => 0,
        4..=8 => 1,
        _ => 2,
    }
}

/// The chance of a gap of `ticks` real ticks under `gaps`, in units of `10^-6 / 64`: a gap of
/// 1 to 10 ticks has its share, and each of the tail's 64 equally likely values a 64th of the
/// tail's share (a value the tail holds twice, twice that); 0 for a gap `gaps` never draws.
/// At most `10^6 × 64`, so a sum of 74 products of two fits a `u64`.
fn gap_weight(gaps: &Gaps, ticks: i64) -> u64 {
    let tail = gaps.tail_ticks.0;
    match ticks {
        1..=10 => u64::from(gaps.one_to_ten_ppm[ticks as usize - 1]) * tail.len() as u64,
        11.. => {
            // The tail's values are in ascending order: those equal to `ticks` are the ones
            // from the first at least `ticks` to the first above it.
            let from = tail.partition_point(|&value| i64::from(value) < ticks);
            let to = tail.partition_point(|&value| i64::from(value) <= ticks);
            u64::from(gaps.beyond_ten_ppm) * (to - from) as u64
        }
        _ => 0,
    }
}

/// Every gap `gaps` can draw, in real ticks, in ascending order, once each: 1 to 10, then the
/// tail's values (all over 10).
fn gap_values(gaps: &Gaps) -> impl Iterator<Item = i64> + Clone {
    let tail = gaps.tail_ticks.0;
    let distinct = tail.iter().enumerate().filter(move |&(i, value)| i == 0 || tail[i - 1] != *value);
    (1..=10).chain(distinct.map(|(_, &value)| i64::from(value)))
}

/// A spread of `centibps` hundredths of a bps at the fair value `fair`, in real ticks of `fair`,
/// rounded, at least one.
fn spread_ticks(fair: Price, centibps: u32) -> i64 {
    // `c` hundredths of a bps are `F × c / 10^6` engine ticks.
    let tick = i128::from(real_tick(fair));
    let ticks = (i128::from(fair) * i128::from(centibps) + 500_000 * tick) / (1_000_000 * tick);
    i64::try_from(ticks).expect("a spread fits a price").max(1)
}

/// Every spread `spread` can draw at the fair value `fair`, in real ticks, one per equally
/// likely value: one real tick for a tick-bound market, else each of the table's 64 values
/// (so a spread that several values round to comes several times).
fn spread_values(spread: &'static Spread, fair: Price) -> impl Iterator<Item = i64> + Clone {
    let table: &'static [u32] = match spread {
        Spread::OneTick => &[],
        Spread::SplitLognormal { centibps, .. } => centibps.0,
    };
    let count = table.len().max(1);
    (0..count).map(move |i| if table.is_empty() { 1 } else { spread_ticks(fair, table[i]) })
}

/// The best bid and ask for a spread of `spread` real ticks around the fair value `fair`: the
/// bid `floor((s − 1) / 2)` ticks below it, the ask `s` ticks above the bid (onto its grid).
/// So the fair value is inside the spread: the bid at most at it, the ask above it.
fn best_for(fair: Price, spread: i64) -> (Price, Price) {
    let tick = real_tick(fair);
    let bid = fair - ticks_of((spread - 1) / 2, tick);
    (bid, snap_up(bid + ticks_of(spread, tick)))
}

/// One of `candidates`' values, drawn with the chance of its weight (one draw from `rng`);
/// `None`, drawing nothing, if every weight is 0. O(n). The draw's modulo bias
/// (`SplitMix64::below`) is at most `total / 2^64`: under 1% for the weights here, whose
/// total stays below `64 × (64 × 10^6)^2`.
fn draw_weighted(candidates: impl Iterator<Item = (i64, u64)> + Clone, rng: &mut SplitMix64) -> Option<i64> {
    let total: u64 = candidates.clone().map(|(_, weight)| weight).sum();
    if total == 0 {
        return None;
    }
    let mut u = rng.below(total);
    for (value, weight) in candidates {
        if u < weight {
            return Some(value);
        }
        u -= weight;
    }
    unreachable!("u is below the weights' total")
}

// ---------------------------------------------------------------------------------------
// The real grid.

/// Polymarket's real tick at `price`: 5 significant figures, so `10^max(0, digits − 5)`
/// engine ticks (1 below 100,000, 10 below 1,000,000, and so on).
pub fn real_tick(price: Price) -> Price {
    let mut tick = 1;
    while price.ticks() / tick >= 100_000 {
        tick *= 10;
    }
    Price::new(tick)
}

/// `real_ticks` real ticks of `tick` engine ticks each, in engine ticks.
fn ticks_of(real_ticks: i64, tick: Price) -> Price {
    Price::new(real_ticks * tick.ticks())
}

/// How many whole real ticks of `tick` engine ticks `distance` spans, rounded toward zero.
fn real_ticks_in(distance: Price, tick: Price) -> i64 {
    distance.ticks() / tick.ticks()
}

/// How many engine ticks apart two prices are: `|a − b|`.
fn ticks_apart(a: Price, b: Price) -> Price {
    Price::new((a - b).ticks().abs())
}

/// `price mod tick`, with `tick` the real tick at `price`: how far `price` is past the grid
/// price at or below it.
fn past_grid(price: Price, tick: Price) -> Price {
    Price::new(price.ticks() % tick.ticks())
}

/// True if `price` is on its real grid.
pub fn on_grid(price: Price) -> bool {
    past_grid(price, real_tick(price)) == Price::ZERO
}

/// `price` rounded down onto its real grid. The result is on its own grid too: it has at most
/// as many digits, so its grid is at most as coarse.
pub fn snap_down(price: Price) -> Price {
    price - past_grid(price, real_tick(price))
}

/// `price` rounded up onto its real grid. If that crosses a power of ten, the result is that
/// power of ten, which is on every grid.
pub fn snap_up(price: Price) -> Price {
    let tick = real_tick(price);
    let past = past_grid(price, tick);
    if past == Price::ZERO { price } else { price - past + tick }
}

/// `price` moved `distance` engine ticks away from the fair value, on `side`'s side of the
/// book: down for a bid, up for an ask.
fn away(side: Side, price: Price, distance: Price) -> Price {
    match side {
        Side::Buy => price - distance,
        Side::Sell => price + distance,
    }
}

/// `price` rounded onto its grid away from the fair value (down for a bid, up for an ask), so
/// rounding never moves a quote toward the other side.
fn snap_away(side: Side, price: Price) -> Price {
    match side {
        Side::Buy => snap_down(price),
        Side::Sell => snap_up(price),
    }
}

/// The grid price one real tick further from the fair value than `price` (itself on the grid).
fn step_away(side: Side, price: Price) -> Price {
    match side {
        Side::Buy => price - real_tick(price - Price::ONE_TICK),
        Side::Sell => price + real_tick(price),
    }
}

/// The grid price one real tick nearer the fair value than `price` (itself on the grid).
fn step_toward(side: Side, price: Price) -> Price {
    match side {
        Side::Buy => price + real_tick(price),
        Side::Sell => price - real_tick(price - Price::ONE_TICK),
    }
}

/// True if `a` is a better price than `b` for `side`: higher for a bid, lower for an ask.
fn better(side: Side, a: Price, b: Price) -> bool {
    match side {
        Side::Buy => a > b,
        Side::Sell => a < b,
    }
}

// ---------------------------------------------------------------------------------------
// The market.

/// One maker quote, as the generator last sent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Quote {
    pub price: Price,
    /// Its total size as last sent: a resize sends a new total (D-008: the book subtracts
    /// what has filled).
    pub size: Qty,
    pub order_id: OrderId,
    pub maker: AccountId,
}

/// What a fair-value step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// No move (the calibrated share of seconds), or a move stopped at the fair value's bound.
    Still,
    /// An ordinary 1-s move: the ladders follow it.
    Move,
    /// A persistent jump: the ladders are pulled and quoted again.
    Jump,
}

/// One market (module docs).
#[derive(Clone, Debug)]
pub(super) struct MarketState {
    pub spec: &'static Market,
    pub params: SetMarketParams,
    /// The fair value, on its real grid; also the latest mark (module docs).
    pub fair: Price,
    /// The fair value stays in `fair_low..=fair_high`, so that the band's edges stay inside
    /// the price range.
    pub fair_low: Price,
    pub fair_high: Price,
    /// The makers' quotes, best first.
    pub bids: Vec<Quote>,
    pub asks: Vec<Quote>,
    /// Mark ticks so far: every [`TICKS_PER_STEP`]-th steps the fair value.
    ticks: u64,
    fair_stream: SplitMix64,
    maker_stream: SplitMix64,
}

impl MarketState {
    /// This market's id, as its parameters carry it.
    pub fn id(&self) -> MarketId {
        self.params.market
    }

    /// Market `spec` at its start price, with no quotes yet; its streams are `FAIR(m)` and
    /// `MM(m)` of `seed`, `m` its id.
    pub fn new(seed: u64, spec: &'static Market) -> MarketState {
        let params = market_params(spec);
        // The band's edges at the fair value, `F × (1 ± band)`, stay inside the price range:
        // `F ≥ min_price / (1 − band)` and `F ≤ max_price / (1 + band)`, rounded inward.
        // Prices scaled by a rate: on the bare numbers of ticks.
        let band = i64::from(params.price_band_ppm);
        let (min_price, max_price) = (params.min_price.ticks(), params.max_price.ticks());
        let low = Price::new(ceil_div(min_price * 1_000_000, 1_000_000 - band));
        let high = Price::new(max_price * 1_000_000 / (1_000_000 + band));
        let m = u64::from(spec.id);
        MarketState {
            spec,
            params,
            fair: Price::new(spec.start_price),
            fair_low: snap_up(low),
            fair_high: snap_down(high),
            bids: Vec::new(),
            asks: Vec::new(),
            ticks: 0,
            fair_stream: stream(seed, stream_ids::FAIR + m),
            maker_stream: stream(seed, stream_ids::MM + m),
        }
    }

    /// The real tick at the fair value: the unit of spreads, gaps and moves.
    fn tick(&self) -> Price {
        real_tick(self.fair)
    }

    /// How far from the fair value a quote may be: half the band, in engine ticks. A price
    /// scaled by a rate: on the bare number of ticks.
    pub fn reach(&self) -> Price {
        Price::new(self.fair.ticks() * i64::from(self.params.price_band_ppm) / 2_000_000)
    }

    fn ladder(&self, side: Side) -> &[Quote] {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    fn ladder_mut(&mut self, side: Side) -> &mut Vec<Quote> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    /// True if `price` is within reach of the fair value.
    fn within_reach(&self, price: Price) -> bool {
        ticks_apart(price, self.fair) <= self.reach()
    }

    /// The price furthest from the fair value that is within reach on `side`, on the grid.
    fn deepest(&self, side: Side) -> Price {
        match side {
            Side::Buy => snap_up(self.fair - self.reach()),
            Side::Sell => snap_down(self.fair + self.reach()),
        }
    }

    /// True if one of `side`'s quotes is at `price`.
    fn taken(&self, side: Side, price: Price) -> bool {
        self.ladder(side).iter().any(|quote| quote.price == price)
    }

    /// The first free grid price at `start` or further from the fair value, within reach.
    /// If there is none, the deepest free one within reach, searching back toward the fair
    /// value. There always is one behind the side's best quote, for 20 quotes: the reach is
    /// at least 87 real ticks at the start prices (KPEPE-USD's, the coarsest, at 2.3 bps a
    /// tick), and at least 45 anywhere in the fair value's range, whose low end is about half
    /// the start price.
    fn free_price(&self, side: Side, start: Price) -> Price {
        let mut price = snap_away(side, start);
        while self.within_reach(price) {
            if !self.taken(side, price) {
                return price;
            }
            price = step_away(side, price);
        }
        let best = self.ladder(side).first().map(|quote| quote.price);
        let mut price = self.deepest(side);
        while self.taken(side, price) {
            price = step_toward(side, price);
            assert!(best.is_none_or(|best| better(side, best, price)), "no free price behind the best quote");
        }
        price
    }

    /// A price for a new quote behind the one at `price` (rank `rank`) on `side`: a gap from
    /// `rank`'s bucket drawn among those that stay within reach, then the first free price
    /// there or further ([`MarketState::free_price`]). If `price` is at the reach's edge, so
    /// that no gap fits, the deepest free price within reach. Draws: the gap.
    fn next_price(&mut self, side: Side, price: Price, rank: usize) -> Price {
        let tick = self.tick();
        let room = real_ticks_in(self.reach() - ticks_apart(price, self.fair), tick);
        match self.draw_gap_within(rank, room) {
            Some(gap) => self.free_price(side, away(side, price, ticks_of(gap, tick))),
            None => self.free_price(side, self.deepest(side)),
        }
    }

    /// The operator's mark at the fair value.
    pub fn mark(&self) -> Item {
        Item::Operator(Command::SetMark(SetMark { price: self.fair, market: self.id() }))
    }

    // -----------------------------------------------------------------------------------
    // Draws from `MM(m)`.

    /// A spread, in real ticks, at least one: one real tick for a tick-bound market, else a
    /// draw of its spread table (bps) at the fair value, rounded ([`spread_ticks`]).
    fn draw_spread(&mut self) -> i64 {
        match self.spec.spread {
            Spread::OneTick => 1,
            Spread::SplitLognormal { centibps, .. } => {
                spread_ticks(self.fair, centibps.draw(&mut self.maker_stream))
            }
        }
    }

    /// A gap after the quote at `rank`, in real ticks, drawn from its bucket's gaps among
    /// those of at most `room` ticks (see [`gap_weight`]); `None` if none is that short.
    fn draw_gap_within(&mut self, rank: usize, room: i64) -> Option<i64> {
        let gaps = &self.spec.book.gaps[gap_bucket(rank)];
        let candidates =
            gap_values(gaps).take_while(|&gap| gap <= room).map(|gap| (gap, gap_weight(gaps, gap)));
        draw_weighted(candidates, &mut self.maker_stream)
    }

    /// The gap from the quote at `rank − 1` to a quote at `rank`, when the quote at `rank + 1`
    /// is `span` real ticks from the first: drawn with the chance that two independent gaps of
    /// their buckets are that gap and `span` minus it. So a quote moved between two others
    /// leaves both gaps next to it distributed as the profile's (a Gibbs step: whatever the
    /// order of re-prices, the ladder keeps the recorded gaps). If no two gaps of the profile
    /// make `span` (a stretch a fresh spread or a pull left), the gap before is drawn among
    /// those shorter than `span`; `None` if `span` has no room for a quote.
    fn draw_split(&mut self, rank: usize, span: i64) -> Option<i64> {
        let book = self.spec.book;
        let (before, after) = (&book.gaps[gap_bucket(rank - 1)], &book.gaps[gap_bucket(rank)]);
        let pairs = gap_values(before)
            .take_while(|&gap| gap < span)
            .map(|gap| (gap, gap_weight(before, gap) * gap_weight(after, span - gap)));
        let rng = &mut self.maker_stream;
        draw_weighted(pairs, rng).or_else(|| {
            let shorter =
                gap_values(before).take_while(|&gap| gap < span).map(|gap| (gap, gap_weight(before, gap)));
            draw_weighted(shorter, rng)
        })
    }

    /// A size for a quote at `rank` and `price`, in lots: a notional drawn for its level
    /// ([`MarketState::draw_notional`]), at least the profile's $10 minimum, with lots rounded
    /// up so that the quote is worth at least that.
    fn draw_size(&mut self, rank: usize, price: Price) -> Qty {
        let notional = self.draw_notional(rank).max(Micros::new(POLYMARKET.min_notional));
        // Micros over ticks is lots (D-004's identity read backwards), on the bare numbers.
        Qty::new(ceil_div(notional.micros(), price.ticks()))
    }

    /// A notional for a quote at `rank`, in micros (profile.rs, `LevelSizes`), with that
    /// level's shares: dust (scaled by the market's own dust share against its class's, so a
    /// market with little dust has little at every level); else a clip of the class's menu,
    /// jittered by up to 3% either way; else a draw of that level's background sizes.
    fn draw_notional(&mut self, rank: usize) -> Micros {
        let shape = self.spec.book;
        let level = &shape.levels[rank];
        let clips: u64 = level.clips_ppm.iter().map(|&share| u64::from(share)).sum();
        let dust =
            u64::from(level.dust_ppm) * u64::from(self.spec.dust_ppm) / u64::from(shape.dust_ppm).max(1);
        let dust = dust.min(MILLION - clips);
        let rng = &mut self.maker_stream;
        // One draw `u` in 0..10^6 walks the shares: dust first, then each clip.
        let mut u = rng.below(MILLION);
        if u < dust {
            let [low, high] = POLYMARKET.dust_draw_cents;
            return Micros::new(rng.in_range(i64::from(low), i64::from(high) - 1) * 10_000);
        }
        u -= dust;
        for (&usd, &share) in shape.clips_usd.iter().zip(level.clips_ppm) {
            if u < u64::from(share) {
                // `usd × 10^6` micros, times `(10^6 + jitter) / 10^6`.
                let jitter = i64::from(shape.clip_jitter_ppm);
                return Micros::new(i64::from(usd) * (1_000_000 + rng.in_range(-jitter, jitter)));
            }
            u -= u64::from(share);
        }
        Micros::new(i64::from(level.background_usd.draw(rng)) * 1_000_000)
    }

    // -----------------------------------------------------------------------------------
    // Quotes.

    /// The rank a quote at `price` takes on `side`: how many quotes are better.
    fn rank_of(&self, side: Side, price: Price) -> usize {
        self.ladder(side).iter().take_while(|quote| better(side, quote.price, price)).count()
    }

    /// A new post-only GTC quote from `maker` at `price` (free, within reach), with a size for
    /// the rank it takes. Draws: the size.
    fn place(
        &mut self,
        side: Side,
        price: Price,
        maker: AccountId,
        clients: &mut Clients,
        out: &mut Vec<Item>,
    ) {
        let size = self.draw_size(self.rank_of(side, price), price);
        self.place_sized(side, price, maker, size, clients, out);
    }

    /// A new post-only GTC quote from `maker` at `price` (free, within reach) of `size` lots.
    fn place_sized(
        &mut self,
        side: Side,
        price: Price,
        maker: AccountId,
        size: Qty,
        clients: &mut Clients,
        out: &mut Vec<Item>,
    ) {
        let rank = self.rank_of(side, price);
        let order =
            NewOrder { market: self.id(), side, price, qty: size, tif: TimeInForce::Gtc, post_only: true };
        let (order_id, item) = clients.place(maker, order);
        out.push(item);
        self.ladder_mut(side).insert(rank, Quote { price, size, order_id, maker });
    }

    /// Cancels the quote at `rank`, and returns it.
    fn cancel(&mut self, side: Side, rank: usize, clients: &mut Clients, out: &mut Vec<Item>) -> Quote {
        let quote = self.ladder_mut(side).remove(rank);
        out.push(clients.cancel(quote.maker, quote.order_id, self.id()));
        quote
    }

    /// Builds both ladders around the fair value, into empty ladders: `bid_makers[r]` and
    /// `ask_makers[r]` own the quotes of rank `r`, each a gap behind the one before
    /// ([`MarketState::next_price`]). Draws: a spread, then per side (bids first) and per rank
    /// a gap (from rank 1) and a size.
    pub fn build(
        &mut self,
        bid_makers: &[AccountId],
        ask_makers: &[AccountId],
        clients: &mut Clients,
        out: &mut Vec<Item>,
    ) {
        let (best_bid, best_ask) = self.best_prices();
        for (side, best, makers) in [(Side::Buy, best_bid, bid_makers), (Side::Sell, best_ask, ask_makers)] {
            let mut price = best;
            for (rank, &maker) in makers.iter().enumerate() {
                if rank > 0 {
                    price = self.next_price(side, price, rank - 1);
                }
                self.place(side, price, maker, clients, out);
            }
        }
    }

    /// The best bid and ask for a fresh spread draw ([`best_for`]).
    fn best_prices(&mut self) -> (Price, Price) {
        let spread = self.draw_spread();
        let (best_bid, best_ask) = best_for(self.fair, spread);
        assert!(
            self.within_reach(best_bid) && self.within_reach(best_ask),
            "{}: a spread of {spread} real ticks is beyond the reach",
            self.spec.symbol
        );
        (best_bid, best_ask)
    }

    /// One maker event (`polymarket.rs`, "Makers"): a re-price if `reprice`, else a resize.
    /// Draws: the side, the level band, the rank within it; then the change's own draws.
    pub fn maker_event(&mut self, reprice: bool, mix: &MakerMix, clients: &mut Clients, out: &mut Vec<Item>) {
        let rng = &mut self.maker_stream;
        let side = if rng.below(2) == 0 { Side::Buy } else { Side::Sell };
        let band = pick(if reprice { &mix.reprice_bands } else { &mix.resize_bands }, rng);
        let rank = BAND_FIRST[band] + rng.below(BAND_RANKS[band]) as usize;
        match (reprice, rank) {
            (true, 0) => self.spread_change(clients, out),
            (true, _) => self.reprice(side, rank, clients, out),
            (false, _) => self.resize(side, rank, clients, out),
        }
    }

    /// A resize: the quote at `rank` gets a new total size (a modify at its price). Draws: the
    /// size.
    fn resize(&mut self, side: Side, rank: usize, clients: &mut Clients, out: &mut Vec<Item>) {
        let quote = self.ladder(side)[rank];
        let size = self.draw_size(rank, quote.price);
        let modify = ModifyOrder {
            order_id: quote.order_id,
            new_price: quote.price,
            new_size: size,
            market: self.id(),
        };
        out.push(clients.send(quote.maker, Command::ModifyOrder(modify)));
        self.ladder_mut(side)[rank].size = size;
    }

    /// A re-price of the quote at `rank` (not the best): it is cancelled, and its maker
    /// places a new quote between the quotes around it, the two gaps next to it drawn together
    /// ([`MarketState::draw_split`]), so the ladder keeps the profile's gaps however often it
    /// is re-priced; back at its old price if there is no room between them. The last quote,
    /// with none behind it, goes a gap behind the one before, within reach. Draws: the gap,
    /// then the size.
    fn reprice(&mut self, side: Side, rank: usize, clients: &mut Clients, out: &mut Vec<Item>) {
        let old = self.cancel(side, rank, clients, out);
        let anchor = self.ladder(side)[rank - 1].price;
        let price = match self.ladder(side).get(rank).map(|next| next.price) {
            Some(next) => {
                let tick = self.tick();
                let span = real_ticks_in(ticks_apart(next, anchor), tick);
                match self.draw_split(rank, span) {
                    Some(gap) => self.free_price(side, away(side, anchor, ticks_of(gap, tick))),
                    None => old.price,
                }
            }
            None => self.next_price(side, anchor, rank - 1),
        };
        self.place(side, price, old.maker, clients, out);
    }

    /// A spread change, the re-price of a best quote (a maker event at rank 0): both best
    /// quotes move to a new spread around the fair value, the second quotes staying. The
    /// spread is drawn together with the two gaps it leaves behind the best quotes: each
    /// spread its model can draw ([`spread_values`]) with its chance times each gap's under the
    /// profile ([`gap_weight`]), 0 for a spread that doesn't fit ahead of the second quotes.
    /// Like a re-price, that keeps the book's spread and gaps distributed as the profile's
    /// however often it runs (a Gibbs step), where a fresh spread would stretch or squeeze
    /// the gaps behind the best quotes each time. If no spread fits, a fresh one
    /// ([`MarketState::respread`]). Both cancels come before both places. Draws: the spread,
    /// then the two sizes (bid first).
    fn spread_change(&mut self, clients: &mut Clients, out: &mut Vec<Item>) {
        let (fair, tick) = (self.fair, self.tick());
        let (second_bid, second_ask) = (self.bids[1].price, self.asks[1].price);
        let gaps = &self.spec.book.gaps[gap_bucket(0)];
        let candidates = spread_values(self.spec.spread, fair).map(|spread| {
            let (bid, ask) = best_for(fair, spread);
            let weight = if bid > second_bid && ask < second_ask {
                gap_weight(gaps, real_ticks_in(bid - second_bid, tick))
                    * gap_weight(gaps, real_ticks_in(second_ask - ask, tick))
            } else {
                0
            };
            (spread, weight)
        });
        let Some(spread) = draw_weighted(candidates, &mut self.maker_stream) else {
            return self.respread(clients, out);
        };
        let (best_bid, best_ask) = best_for(fair, spread);
        let bid_maker = self.cancel(Side::Buy, 0, clients, out).maker;
        let ask_maker = self.cancel(Side::Sell, 0, clients, out).maker;
        self.place(Side::Buy, best_bid, bid_maker, clients, out);
        self.place(Side::Sell, best_ask, ask_maker, clients, out);
    }

    /// Both best quotes move to a fresh spread draw around the fair value, where
    /// [`MarketState::spread_change`] finds no spread that fits ahead of the second quotes.
    /// Any other quote at or ahead of a new best is moved behind it too, each a drawn gap
    /// behind the one before. Every cancel comes before every place. Draws: the spread, then
    /// per side (bids first) the moved quotes' gaps and sizes, in rank order.
    fn respread(&mut self, clients: &mut Clients, out: &mut Vec<Item>) {
        let (best_bid, best_ask) = self.best_prices();
        let bid_makers = self.clear_front(Side::Buy, best_bid, clients, out);
        let ask_makers = self.clear_front(Side::Sell, best_ask, clients, out);
        self.restack(Side::Buy, best_bid, &bid_makers, clients, out);
        self.restack(Side::Sell, best_ask, &ask_makers, clients, out);
    }

    /// Cancels `side`'s best quote and every quote left at or ahead of `new_best`; returns
    /// their makers, best first.
    fn clear_front(
        &mut self,
        side: Side,
        new_best: Price,
        clients: &mut Clients,
        out: &mut Vec<Item>,
    ) -> Vec<AccountId> {
        let mut makers = vec![self.cancel(side, 0, clients, out).maker];
        while self.ladder(side).first().is_some_and(|quote| !better(side, new_best, quote.price)) {
            makers.push(self.cancel(side, 0, clients, out).maker);
        }
        makers
    }

    /// Places `makers[0]`'s quote at `best`, and each further maker's a drawn gap behind the
    /// one before ([`MarketState::next_price`]).
    fn restack(
        &mut self,
        side: Side,
        best: Price,
        makers: &[AccountId],
        clients: &mut Clients,
        out: &mut Vec<Item>,
    ) {
        let mut price = best;
        for (rank, &maker) in makers.iter().enumerate() {
            if rank > 0 {
                price = self.next_price(side, price, rank - 1);
            }
            self.place(side, price, maker, clients, out);
        }
    }

    // -----------------------------------------------------------------------------------
    // The fair value.

    /// `MarkTick(m)` (`polymarket.rs`): every [`TICKS_PER_STEP`]-th tick the fair value first
    /// takes its 1-s step; then the mark, at the fair value; then, after a move, the ladders
    /// follow it ([`MarketState::follow_move`], and any quote now beyond reach is moved
    /// within it), and after a jump they are pulled and quoted again. Returns the fair value
    /// before a jump, if the step was one.
    pub fn mark_tick(
        &mut self,
        jump_one_in: u64,
        clients: &mut Clients,
        out: &mut Vec<Item>,
    ) -> Option<Price> {
        self.ticks += 1;
        let before = self.fair;
        let step =
            if self.ticks.is_multiple_of(TICKS_PER_STEP) { self.step(jump_one_in) } else { Step::Still };
        out.push(self.mark());
        match step {
            Step::Still => {}
            Step::Move => {
                self.follow_move(before, clients, out);
                self.pull_within_reach(Side::Buy, clients, out);
                self.pull_within_reach(Side::Sell, clients, out);
            }
            Step::Jump => self.requote_all(clients, out),
        }
        (step == Step::Jump).then_some(before)
    }

    /// The fair value's 1-s step (`polymarket.rs`, "Prices"). Draws from `FAIR(m)`: whether it
    /// is a jump (1 in `jump_one_in`), then for a jump its size and direction; otherwise
    /// whether it moves at all, the mixture's component, `|z|` and the direction.
    fn step(&mut self, jump_one_in: u64) -> Step {
        let rng = &mut self.fair_stream;
        if rng.below(jump_one_in) == 0 {
            let centibps = POLYMARKET.jumps.centibps.draw(rng);
            let up = rng.below(2) == 0;
            // `c` hundredths of a bps of the price.
            let ticks = i128::from(self.fair) * i128::from(centibps) / 1_000_000;
            return if self.move_fair(ticks, up) { Step::Jump } else { Step::Still };
        }
        let moves = self.spec.moves;
        if rng.below(MILLION) < u64::from(moves.no_move_ppm) {
            return Step::Still;
        }
        let component = pick(&moves.weights_ppm, rng);
        let z = POLYMARKET.half_normal_milli.draw(rng);
        let up = rng.below(2) == 0;
        // `|move| = rms × sd_k × |z|` bps of the price: in engine ticks,
        // `F × millibps × milli × milli / 10^13`.
        let ticks = i128::from(self.fair)
            * i128::from(self.spec.move_rms_millibps)
            * i128::from(moves.sd_milli[component])
            * i128::from(z)
            / 10i128.pow(13);
        // In whole real ticks, at least one: the calibration counts only moves of the mark.
        let tick = i128::from(self.tick());
        let real_ticks = ((ticks + tick / 2) / tick).max(1);
        if self.move_fair(real_ticks * tick, up) { Step::Move } else { Step::Still }
    }

    /// Moves the fair value `ticks` up or down, onto its grid and inside its bounds. Returns
    /// false if that left it where it was (at a bound).
    fn move_fair(&mut self, ticks: i128, up: bool) -> bool {
        let ticks = Price::new(i64::try_from(ticks).expect("a move fits a price"));
        let target = if up { self.fair + ticks } else { self.fair - ticks };
        let fair = snap_down(target.clamp(self.fair_low, self.fair_high));
        let moved = fair != self.fair;
        self.fair = fair;
        moved
    }

    /// After a move of the fair value from `from`: both ladders follow it, every quote moving
    /// by the move, with its maker and size, onto its grid away from the fair value (to the
    /// next free price if two meet there, near a power of ten). So the book keeps its spread
    /// and its gaps, as makers move their ladders with the price; moving only the best quotes
    /// would stretch the gaps behind them on one side and crowd the other, and nothing but
    /// the ladder's end would ever take the stretch out (review finding [gaps-drift]). Every
    /// cancel comes before every place, bids first, best first. Draws nothing.
    fn follow_move(&mut self, from: Price, clients: &mut Clients, out: &mut Vec<Item>) {
        let shift = self.fair - from;
        let mut moved = Vec::with_capacity(self.bids.len() + self.asks.len());
        for side in [Side::Buy, Side::Sell] {
            while !self.ladder(side).is_empty() {
                moved.push((side, self.cancel(side, 0, clients, out)));
            }
        }
        for (side, quote) in moved {
            let price = self.free_price(side, quote.price + shift);
            self.place_sized(side, price, quote.maker, quote.size, clients, out);
        }
    }

    /// Moves every quote of `side` beyond reach of the fair value back within it: cancels
    /// first, deepest first; then each goes a drawn gap behind the last quote left
    /// ([`MarketState::next_price`]), the shallowest first. Draws: per quote, its gap and
    /// size. (The side's best quote is always within reach: a spread change comes first.)
    fn pull_within_reach(&mut self, side: Side, clients: &mut Clients, out: &mut Vec<Item>) {
        let mut makers = Vec::new();
        while self.ladder(side).last().is_some_and(|quote| !self.within_reach(quote.price)) {
            let deepest = self.ladder(side).len() - 1;
            makers.push(self.cancel(side, deepest, clients, out).maker);
        }
        for maker in makers.into_iter().rev() {
            let last = self.ladder(side).last().expect("the best quote is within reach");
            let (price, rank) = (last.price, self.ladder(side).len() - 1);
            let price = self.next_price(side, price, rank);
            self.place(side, price, maker, clients, out);
        }
    }

    /// Pulls every quote (bids, then asks, best first), then builds both ladders again around
    /// the fair value, each rank keeping its maker. For jumps and shocks, which make every old
    /// quote stale: requoting them one by one would cross the other side.
    fn requote_all(&mut self, clients: &mut Clients, out: &mut Vec<Item>) {
        let bid_makers: Vec<AccountId> = self.bids.iter().map(|quote| quote.maker).collect();
        let ask_makers: Vec<AccountId> = self.asks.iter().map(|quote| quote.maker).collect();
        for side in [Side::Buy, Side::Sell] {
            while !self.ladder(side).is_empty() {
                self.cancel(side, 0, clients, out);
            }
        }
        self.build(&bid_makers, &ask_makers, clients, out);
    }

    /// A shock's move of this market (`polymarket.rs`, "Stress switches"): the fair value
    /// moves `ticks` up or down, the mark follows at once, and the ladders are pulled and
    /// quoted again. Returns the fair value before and after, or `None` if the move left it
    /// where it was (at a bound), in which case nothing is emitted.
    pub fn shock(
        &mut self,
        ticks: i128,
        up: bool,
        clients: &mut Clients,
        out: &mut Vec<Item>,
    ) -> Option<(Price, Price)> {
        let before = self.fair;
        if !self.move_fair(ticks, up) {
            return None;
        }
        out.push(self.mark());
        self.requote_all(clients, out);
        Some((before, self.fair))
    }

    // -----------------------------------------------------------------------------------
    // Takers and cohorts.

    /// An IOC on `side` worth `notional` (at least the profile's $10 minimum) at the fair
    /// value, priced at the band's edge of the latest mark (rounded onto the grid, inward), so
    /// it sweeps the book until it is filled: its fills come from the book's real depth. Its
    /// lots are rounded up, so it is worth at least that notional.
    pub fn ioc(&self, side: Side, notional: Micros) -> NewOrder {
        let edges = band_edges(self.fair, self.params.price_band_ppm);
        let price = match side {
            Side::Buy => snap_down(edges.upper),
            Side::Sell => snap_up(edges.lower),
        };
        let notional = notional.max(Micros::new(POLYMARKET.min_notional));
        // Micros over ticks is lots (D-004's identity read backwards), on the bare numbers.
        let qty = Qty::new(ceil_div(notional.micros(), self.fair.ticks()).max(1));
        NewOrder { market: self.id(), side, price, qty, tif: TimeInForce::Ioc, post_only: false }
    }
}
