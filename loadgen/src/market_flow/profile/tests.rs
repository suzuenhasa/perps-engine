//! Tests of the Polymarket profile (`profile.rs`; D-034): the table's shape and invariants, the
//! engine accepting every market and tier table, and the values D-034 quotes. That the table is
//! exactly what `tools/calibrate` makes from its JSON is checked by the tool, not from cargo:
//! `python3 tools/calibrate/calibrate.py generate --check` (`tools/calibrate/README.md`); cargo
//! checks that the table names the committed JSON's SHA-256.

use std::collections::HashSet;
use std::ptr;

use super::*;
use engine::book::{Book, MAX_LEVELS};
use engine::command::{Command, SetMarketParams, SetRiskTier};
use engine::engine::{Engine, EngineOptions};
use engine::event::Event;
use engine::mode::Fast;
use engine::money::{MAX_TIERS, PRICE_LIMIT};
use engine::types::{Micros, Price};

const MILLION: u32 = 1_000_000;

/// How many markets of the profile satisfy `keep`.
fn count(keep: impl Fn(&Market) -> bool) -> usize {
    POLYMARKET.markets.iter().filter(|market| keep(market)).count()
}

/// Every table of the profile, with its name.
fn tables() -> Vec<(String, Table)> {
    let p = &POLYMARKET;
    let mut all = vec![
        ("half normal".to_string(), p.half_normal_milli),
        ("jump sizes".to_string(), p.jumps.centibps),
        ("cluster gaps".to_string(), p.takers.cluster_gap_ms),
        ("taker notional".to_string(), p.taker_notional.continuous_cents),
    ];
    for shape in p.book_shapes {
        for (bucket, gaps) in shape.gaps.iter().enumerate() {
            all.push((format!("{:?} gap tail {bucket}", shape.class), gaps.tail_ticks));
        }
        for (rank, level) in shape.levels.iter().enumerate() {
            all.push((format!("{:?} level {} background", shape.class, rank + 1), level.background_usd));
        }
    }
    for market in p.markets {
        if let Spread::SplitLognormal { centibps, .. } = market.spread {
            all.push((format!("{} spread", market.symbol), *centibps));
        }
    }
    all
}

#[test]
fn the_table_has_the_88_markets_once_each_in_id_order() {
    let markets = POLYMARKET.markets;
    assert_eq!(markets.len(), 88);
    assert!(markets.windows(2).all(|pair| pair[0].id < pair[1].id), "ids strictly rising");
    let symbols: HashSet<&str> = markets.iter().map(|market| market.symbol).collect();
    assert_eq!(symbols.len(), 88);
    // Polymarket's instrument ids are ours: 1 to 90, without 12 and 51.
    assert_eq!((markets[0].id, markets[87].id), (1, 90));
    assert!(POLYMARKET.market(MarketId::new(12)).is_none() && POLYMARKET.market(MarketId::new(51)).is_none());
    assert_eq!(POLYMARKET.market(MarketId::new(7)).map(|market| market.symbol), Some("ETH-USD"));
}

#[test]
fn price_and_quantity_decimals_sum_to_six() {
    // D-004: a tick is 10^-pd dollars and a lot 10^-qd units, so ticks × lots are micros.
    for market in POLYMARKET.markets {
        assert_eq!(market.price_decimals + market.qty_decimals, 6, "{}", market.symbol);
    }
}

#[test]
fn tier_tables_start_at_zero_with_rising_bounds_and_falling_leverage() {
    for market in POLYMARKET.markets {
        let tiers = market.tiers;
        assert!((1..=MAX_TIERS).contains(&tiers.len()), "{}: {} rows", market.symbol, tiers.len());
        assert_eq!(tiers[0], Tier { lower_bound: 0, max_leverage: market.max_leverage }, "{}", market.symbol);
        for pair in tiers.windows(2) {
            assert!(pair[0].lower_bound < pair[1].lower_bound, "{}: {pair:?}", market.symbol);
            assert!(pair[0].max_leverage > pair[1].max_leverage, "{}: {pair:?}", market.symbol);
        }
        assert!(tiers.last().is_some_and(|tier| tier.max_leverage >= 1));
    }
    // D-034: 1 to 8 rows, 12 distinct tables.
    let distinct: HashSet<*const Tier> =
        POLYMARKET.markets.iter().map(|market| market.tiers.as_ptr()).collect();
    assert_eq!(distinct.len(), 12);
}

#[test]
fn start_prices_are_on_their_real_grid_and_inside_the_price_limits() {
    for market in POLYMARKET.markets {
        let (start, grid) = (market.start_price, market.price_grid);
        // 5 significant figures: 10^max(0, digits − 5) ticks.
        let digits = start.to_string().len() as u32;
        assert_eq!(grid, 10i64.pow(digits.saturating_sub(5)), "{}", market.symbol);
        assert!(start > 0 && start % grid == 0, "{}: {start} on a grid of {grid}", market.symbol);
        // D-020: prices below 2^32, and a book at most 2^24 ticks wide, even for the M3 flow's
        // range of half to twice the start price.
        let (min, max) = (start / 2, 2 * start);
        assert!(max < PRICE_LIMIT.ticks(), "{}", market.symbol);
        assert!((max - min + 1) as usize <= MAX_LEVELS, "{}", market.symbol);
    }
    // D-034: the grid is one tick for 76 markets, 10 for 11, 100 for XRP.
    assert_eq!(count(|m| m.price_grid == 1), 76);
    assert_eq!(count(|m| m.price_grid == 10), 11);
    assert_eq!(POLYMARKET.markets.iter().find(|m| m.price_grid == 100).map(|m| m.symbol), Some("XRP-USD"));
}

#[test]
fn the_engine_accepts_every_market_and_its_tier_table() {
    let options = EngineOptions {
        order_capacity: 16,
        id_hash_seed: 1,
        scratch_capacity: 16,
        account_capacity: 16,
        slot_capacity: 16,
    };
    let mut engine: Engine<Book, Fast> = Engine::new(options);
    let mut events = Vec::new();
    for market in POLYMARKET.markets {
        // Polymarket's own bands (2% at 50x to a third at 3x) break our band rule 1, so a flow
        // sets its own; the M3 flow's shape, 400,000 / Lmax ppm, passes both rules at every
        // leverage here.
        let id = market.market_id();
        let params = SetMarketParams {
            min_price: Price::new(market.start_price / 2),
            max_price: Price::new(2 * market.start_price),
            maker_fee_ppm: 100,
            taker_fee_ppm: 400,
            price_band_ppm: 400_000 / u32::from(market.max_leverage),
            market: id,
            max_leverage: market.max_leverage,
        };
        let count = market.tiers.len() as u8;
        let rows = market.tiers.iter().enumerate().map(|(index, tier)| {
            let (lower_bound, max_leverage) = (Micros::new(tier.lower_bound), tier.max_leverage);
            Command::SetRiskTier(SetRiskTier {
                lower_bound,
                market: id,
                max_leverage,
                index: index as u8,
                count,
            })
        });
        for command in std::iter::once(Command::SetMarketParams(params)).chain(rows) {
            events.clear();
            engine.apply(&command, &mut events);
            let rejects: Vec<_> = events.iter().filter(|event| matches!(event, Event::Reject(_))).collect();
            assert!(rejects.is_empty(), "{}: {command:?} -> {rejects:?}", market.symbol);
        }
    }
    engine.assert_invariants();
}

#[test]
fn classes_have_the_markets_d_034_lists() {
    let book = |class| count(|m| m.book.class == class);
    let counts = BookClass::ALL.map(book);
    assert_eq!(counts, [3, 24, 13, 42, 6], "majors, alt, long-tail, equities, macro");
    for class in BookClass::ALL {
        assert_eq!(POLYMARKET.book_shape(class).class, class);
    }
    // Max leverage: 10x on 63, 5x on 9, 20x on 8, 3x on 4, 50x on 4.
    let leverage = |x| count(|m| m.max_leverage == x);
    assert_eq!([10, 5, 20, 3, 50].map(leverage), [63, 9, 8, 4, 4]);
    for market in POLYMARKET.markets {
        let expected = match market.max_leverage {
            50 => LeverageClass::Max50,
            20 => LeverageClass::Max20,
            10 => LeverageClass::Max10,
            _ => LeverageClass::Max3To5,
        };
        assert_eq!(market.moves.class, expected, "{}", market.symbol);
        assert!(ptr::eq(POLYMARKET.moves_of(expected), market.moves));
    }
}

#[test]
fn weights_and_every_split_sum_to_a_million() {
    let p = &POLYMARKET;
    let sum = |shares: &[u32]| shares.iter().sum::<u32>();
    let maker: Vec<u32> = p.markets.iter().map(|m| m.maker_weight_ppm).collect();
    let taker: Vec<u32> = p.markets.iter().map(|m| m.taker_weight_ppm).collect();
    assert_eq!((sum(&maker), sum(&taker)), (MILLION, MILLION));
    assert!(maker.iter().chain(&taker).all(|&w| w > 0), "every market is active");
    for shape in p.book_shapes {
        for gaps in &shape.gaps {
            assert_eq!(sum(&gaps.one_to_ten_ppm) + gaps.beyond_ten_ppm, MILLION, "{:?}", shape.class);
        }
        assert_eq!(shape.levels.len() as u32, p.levels_per_side, "{:?}", shape.class);
        for level in shape.levels {
            assert_eq!(level.clips_ppm.len(), shape.clips_usd.len(), "{:?}", shape.class);
            let clips: u32 = level.clips_ppm.iter().sum();
            assert!(level.dust_ppm + clips < MILLION, "{:?}: room for the background", shape.class);
        }
    }
    for moves in p.moves {
        assert_eq!(sum(&moves.weights_ppm), MILLION);
    }
    let a = &p.maker_activity;
    assert_eq!(a.add_ppm + a.remove_ppm + a.size_up_ppm + a.size_down_ppm, MILLION);
    assert_eq!(a.levels_1_5_ppm + a.levels_6_10_ppm + a.levels_11_20_ppm, MILLION);
    let n = &p.taker_notional;
    let masses: u32 = n.point_masses.iter().map(|mass| mass.share_ppm).sum();
    assert_eq!(n.dust_ppm + masses + n.continuous_ppm, MILLION);
    assert_eq!(sum(p.takers.cluster_size_ppm), MILLION);
    let shocks = &p.shocks;
    assert_eq!(sum(shocks.markets_ppm), MILLION);
    assert_eq!(shocks.markets_ppm.len() as u32, shocks.max_markets - shocks.min_markets + 1);
}

#[test]
fn every_table_is_in_ascending_order() {
    for (name, table) in tables() {
        assert!(!table.0.is_empty(), "{name}");
        assert!(table.0.windows(2).all(|pair| pair[0] <= pair[1]), "{name}: {:?}", table.0);
    }
    let p = &POLYMARKET;
    assert_eq!(p.half_normal_milli.0.len(), 1_024);
    assert_eq!(p.taker_notional.continuous_cents.0.len(), 1_024);
    for shape in p.book_shapes {
        assert!(shape.gaps.iter().all(|gaps| gaps.tail_ticks.0[0] > 10), "the tail is beyond 10 ticks");
        assert!(shape.levels.iter().all(|level| level.background_usd.0[0] >= 10), "the $10 minimum");
    }
    assert!(p.taker_notional.continuous_cents.0[0] >= p.taker_notional.truncate_below_cents);
}

#[test]
fn spreads_follow_the_classes_and_the_values_of_d_034() {
    let p = &POLYMARKET;
    for market in p.markets {
        let tick_bound = ["BTC-USD", "ETH-USD", "SOL-USD", "SP500-USD", "NAS100-USD", "GOLD-USD"];
        let own = ["SILVER-USD", "WTIOIL-USD", "BRENTOIL-USD"];
        if tick_bound.contains(&market.symbol) {
            assert_eq!(*market.spread, Spread::OneTick, "{}", market.symbol);
        } else if own.contains(&market.symbol) {
            assert!(matches!(market.spread, Spread::SplitLognormal { .. }), "{}", market.symbol);
            assert!(!ptr::eq(market.spread, market.book.spread), "{}", market.symbol);
        } else {
            assert!(ptr::eq(market.spread, market.book.spread), "{}", market.symbol);
        }
    }
    // Median bps and the log-sd below and above: alt 5.14/0.85/0.25, long-tail 5.30/0.62/0.95,
    // equities 4.17/0.66/0.34.
    let fitted = |class| match *p.book_shape(class).spread {
        Spread::SplitLognormal { median_centibps, sigma_below_milli, sigma_above_milli, centibps } => {
            // The table's middle value is the median, to the rounding of its knots.
            assert!(centibps.median().abs_diff(median_centibps) <= median_centibps / 20, "{class:?}");
            (median_centibps, (sigma_below_milli + 5) / 10, (sigma_above_milli + 5) / 10)
        }
        Spread::OneTick => panic!("{class:?} has a split lognormal"),
    };
    assert_eq!(fitted(BookClass::AltCrypto), (514, 85, 25));
    assert_eq!(fitted(BookClass::LongTailCrypto), (530, 62, 95));
    assert_eq!(fitted(BookClass::TradfiEquities), (417, 66, 34));
    assert_eq!(*p.book_shape(BookClass::Majors).spread, Spread::OneTick);
}

/// The mean over `levels` (1-based, inclusive) of `share`: each level weighs the same.
fn mean_over(
    shape: &BookShape,
    levels: std::ops::RangeInclusive<usize>,
    share: impl Fn(&LevelSizes) -> u32,
) -> u32 {
    let count = levels.clone().count() as u32;
    levels.map(|level| share(&shape.levels[level - 1])).sum::<u32>() / count
}

#[test]
fn book_shapes_hold_the_clip_menus_dust_and_depth_by_level_of_d_034() {
    let p = &POLYMARKET;
    let clips = |class| p.book_shape(class).clips_usd;
    assert_eq!(clips(BookClass::Majors), [] as [u32; 0]);
    assert_eq!(clips(BookClass::AltCrypto), [6_250]);
    assert_eq!(clips(BookClass::LongTailCrypto), [6_250]);
    assert_eq!(clips(BookClass::TradfiEquities), [25_000, 50_000, 100_000]);
    assert_eq!(clips(BookClass::TradfiMacro), [25_000, 50_000, 100_000]);
    // The alt crypto clip is about 13% of its levels, few at level 1 and a quarter at levels 8
    // to 10; dust about 9% of all levels, more of it deep.
    let alt = p.book_shape(BookClass::AltCrypto);
    let clip = mean_over(alt, 1..=20, |level| level.clips_ppm[0]);
    assert!((110_000..160_000).contains(&clip), "{clip} ppm");
    assert!(alt.levels[0].clips_ppm[0] < 10_000 && alt.levels[8].clips_ppm[0] > 200_000);
    let dust: u64 = p.markets.iter().map(|m| u64::from(m.dust_ppm)).sum::<u64>() / 88;
    assert!((60_000..120_000).contains(&dust), "mean dust share {dust} ppm");
    for class in [BookClass::AltCrypto, BookClass::LongTailCrypto, BookClass::TradfiEquities] {
        let shape = p.book_shape(class);
        let dust = |levels| mean_over(shape, levels, |level| level.dust_ppm);
        let (top, middle, deep) = (dust(1..=5), dust(6..=10), dust(11..=20));
        assert!(top < middle && middle < deep, "{class:?}: {top}, {middle}, {deep}");
    }
    // Depth by level, against the recorded books' medians (book.md, section 4, taken from
    // other hours of the same day than the profile's): level 1 thin, $0.99k to $2.92k
    // recorded, within 30%; level 3 from $3.9k to $50.7k, within 50%.
    let recorded = [
        (BookClass::Majors, 2_920, 3_930),
        (BookClass::AltCrypto, 1_780, 18_900),
        (BookClass::LongTailCrypto, 1_500, 14_000),
        (BookClass::TradfiEquities, 987, 16_000),
        (BookClass::TradfiMacro, 2_920, 50_700),
    ];
    for (class, first, third) in recorded {
        let shape = p.book_shape(class);
        let (one, three) = (shape.median_usd(1), shape.median_usd(3));
        let near = |actual: u32, expected: u32, tolerance: f64| {
            (f64::from(actual) / f64::from(expected) - 1.0).abs() < tolerance
        };
        assert!(near(one, first, 0.3), "{class:?}: level 1's median ${one}, recorded ${first}");
        assert!(near(three, third, 0.5), "{class:?}: level 3's median ${three}, recorded ${third}");
    }
    assert_eq!((p.levels_per_side, p.dust_draw_cents), (20, [1_050, 1_160]));
}

#[test]
fn moves_jumps_and_takers_hold_the_values_of_d_034() {
    let p = &POLYMARKET;
    // 10x: weights 0.63/0.34/0.027, sds 0.47/1.17/3.88 of the market's RMS of nonzero 1-s
    // moves (`move_rms_millibps`); no move in 62% to 74% of seconds.
    let ten = p.moves_of(LeverageClass::Max10);
    assert_eq!(ten.weights_ppm.map(|w| (w + 500) / 1_000), [630, 343, 27]);
    assert_eq!(ten.sd_milli.map(|sd| (sd + 5) / 10), [47, 117, 388]);
    for moves in p.moves {
        assert!((620_000..=745_000).contains(&moves.no_move_ppm), "{:?}", moves.class);
        // The mixture's mean square is 1: a nonzero move has the market's RMS.
        let mean_square: u64 =
            (0..3).map(|k| u64::from(moves.weights_ppm[k]) * u64::from(moves.sd_milli[k]).pow(2)).sum();
        assert!(mean_square.abs_diff(1_000_000 * 1_000_000) < 30_000 * 1_000_000, "{:?}", moves.class);
    }
    // Persistent jumps over 50 bps: about 0.05 per market-hour.
    assert_eq!((p.jumps.over_bps, p.jumps.per_market_hour_milli), (50, 51));
    assert!(p.jumps.centibps.0[0] > 5_000);
    // Takers: 0.07% of messages, 51% buys, clusters 0.451 a second of which 84% single.
    let t = &p.takers;
    assert_eq!((t.share_of_messages_ppm + 50) / 100, 7);
    assert_eq!(t.buy_ppm / 10_000, 51);
    assert_eq!(t.cluster_starts_per_s_milli, 451);
    assert_eq!(t.cluster_size_ppm[0] / 10_000, 84);
    assert_eq!(t.cluster_gap_ms.median(), 8, "p50 7 ms, as the upper middle of 16 values");
    // Taker notional: 14.6% dust, $1,000 on 6.65%, the mixture of D-034.
    let n = &p.taker_notional;
    assert_eq!(n.dust_ppm / 1_000, 146);
    assert_eq!(n.point_masses[0], PointMass { cents: 100_000, share_ppm: 66_455 });
    assert_eq!(
        n.mixture.map(|c| (c.weight_ppm / 1_000, c.mu_milli, c.sd_milli)),
        [(668, 5_714, 2_006), (331, 6_694, 1_046)]
    );
}

#[test]
fn bursts_and_shocks_hold_the_values_of_d_034() {
    let p = &POLYMARKET;
    let median = BurstModel {
        fast: Ar1 { phi: 0.35822, innovation_sd: 0.36432 },
        slow: Ar1 { phi: 0.99788, innovation_sd: 0.012868 },
    };
    let busiest = BurstModel {
        fast: Ar1 { phi: 0.25128, innovation_sd: 0.21795 },
        slow: Ar1 { phi: 0.99913, innovation_sd: 0.0054028 },
    };
    assert_eq!((p.bursts_median, p.bursts_busiest), (median, busiest));
    let s = &p.shocks;
    assert_eq!((s.markets_pareto_alpha_milli, s.min_markets, s.max_markets), (1_413, 14, 88));
    assert_eq!(s.stress_move_ppm, [20_000, 60_000]);
    assert_eq!(s.spread_ms_p50, 52);
    // The Pareto clamped to 14 to 88 (derive.py): every draw up to 14 lands on 14, the
    // median, and every draw from 88 on "all 88"; the 90th percentile is 38 (recorded: 14
    // and 31, at most 71). 4.1% of shocks move more than 71 markets, 3.1% all 88.
    let at = |n: u32| s.markets_ppm[(n - s.min_markets) as usize];
    assert!(at(14) > 600_000 && at(88) > at(87), "{:?}", s.markets_ppm);
    let up_to = |n: u32| (s.min_markets..=n).map(at).sum::<u32>();
    assert!(up_to(37) < 900_000 && up_to(38) >= 900_000);
    assert_eq!((MILLION - up_to(71)) / 1_000, 41);
    assert_eq!(at(88) / 1_000, 31);
}

#[test]
fn the_table_names_the_sha_256_of_the_committed_json() {
    // `generate` writes the JSON's SHA-256 into the table, and the flow's digest hashes it
    // (`polymarket.rs`): a JSON changed without regenerating the table fails here.
    use k256::sha2::{Digest, Sha256};
    let json = include_bytes!("../../../../tools/calibrate/profile-2026-09-30.json");
    let hex: String = Sha256::digest(json).iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(POLYMARKET.sha256, hex);
}

#[test]
fn draws_follow_the_tables_and_the_shares() {
    let mut rng = SplitMix64::new(7);
    // A table draw takes each value equally often.
    let table = Table(&[10, 20, 30, 40]);
    let mean = (0..40_000).map(|_| u64::from(table.draw(&mut rng))).sum::<u64>() / 40_000;
    assert!((24..=26).contains(&mean), "mean {mean}");
    // A pick follows the shares.
    let shares = [100_000, 0, 900_000];
    let mut hits = [0u32; 3];
    for _ in 0..100_000 {
        hits[pick(&shares, &mut rng)] += 1;
    }
    assert_eq!(hits[1], 0);
    assert!((9_000..11_000).contains(&hits[0]), "{hits:?}");
    // Taker weights: ETH, the busiest, takes about 8.5% of taker orders.
    let weights: Vec<u32> = POLYMARKET.markets.iter().map(|m| m.taker_weight_ppm).collect();
    let eth =
        (0..200_000).filter(|_| POLYMARKET.markets[pick(&weights, &mut rng)].symbol == "ETH-USD").count();
    assert!((16_000..18_000).contains(&eth), "{eth} of 200,000");
}
