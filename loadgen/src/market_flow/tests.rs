//! Tests of the M3 flow (`docs/PIPELINE.md` 18.1, "Loadgen"): the same plan for the same
//! seed, pinned first items and counts, the mix of 14.7, the per-account rules (nonces,
//! order sequences, ownership, prices inside the range and the band), and every setup phase
//! accepted by the real engine.

use std::collections::HashMap;

use super::*;
use engine::book::Book;
use engine::engine::Engine;
use engine::event::{Event, RejectReason};
use engine::mode::Fast;
use engine::money::{band_edges, band_rule_1_holds, band_rule_2_holds};
use engine::types::{account_of, sequence_of};

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

/// Items of each kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Mix {
    /// Resting places: the market makers' post-only quotes and the thin layer's orders.
    gtc: u64,
    /// Taker and high-leverage IOCs.
    ioc: u64,
    cancels: u64,
    modifies: u64,
    marks: u64,
    withdrawals: u64,
}

impl Mix {
    fn add(&mut self, item: &Item) {
        match item.command() {
            Command::PlaceOrder(place) if place.tif == TimeInForce::Ioc => self.ioc += 1,
            Command::PlaceOrder(_) => self.gtc += 1,
            Command::CancelOrder(_) => self.cancels += 1,
            Command::ModifyOrder(_) => self.modifies += 1,
            Command::SetMark(_) => self.marks += 1,
            Command::Withdraw(_) => self.withdrawals += 1,
            other => panic!("the timed flow never sends {other:?}"),
        }
    }

    fn clients(&self) -> u64 {
        self.gtc + self.ioc + self.cancels + self.modifies
    }
}

/// `part` as a percentage of `whole`.
fn percent(part: u64, whole: u64) -> f64 {
    100.0 * part as f64 / whole as f64
}

/// What the engine said to a list of items.
#[derive(Debug, Default)]
struct Applied {
    rejects: Vec<(Command, RejectReason)>,
    liquidations: u64,
    shortfalls: u64,
}

fn apply(engine: &mut Engine<Book, Fast>, items: &[Item]) -> Applied {
    let mut applied = Applied::default();
    let mut events = Vec::new();
    for item in items {
        events.clear();
        engine.apply(item.command(), &mut events);
        for event in &events {
            match event {
                Event::Reject(reject) => applied.rejects.push((*item.command(), reject.reason)),
                Event::Liquidation(_) => applied.liquidations += 1,
                Event::InsuranceShortfall(_) => applied.shortfalls += 1,
                _ => {}
            }
        }
    }
    applied
}

fn place(item: &Item) -> Option<&PlaceOrder> {
    match item.command() {
        Command::PlaceOrder(place) => Some(place),
        _ => None,
    }
}

#[test]
fn streams_follow_the_formula_of_14_6() {
    let mut expected =
        SplitMix64::new(SplitMix64::new(7 ^ 0x2_0003u64.wrapping_mul(0x9E37_79B9_7F4A_7C15)).next_u64());
    let mut actual = stream(7, stream_ids::MM + 3);
    assert_eq!(actual.next_u64(), expected.next_u64());
    // Neighbouring ids start far apart.
    assert_ne!(stream(7, stream_ids::MM + 3).next_u64(), stream(7, stream_ids::MM + 4).next_u64());
    let options = MarketFlowConfig::m3().engine_options();
    assert_eq!(options.id_hash_seed, stream(1, stream_ids::HASH_SEED).next_u64());
    assert_eq!(
        (options.order_capacity, options.slot_capacity, options.account_capacity),
        (4_096, 4_096, 4_096)
    );
}

#[test]
fn every_market_passes_both_band_rules_and_has_its_class() {
    for market in (1..=67).map(MarketId::new) {
        let params = market_params(market);
        let fee = params.maker_fee_ppm.max(params.taker_fee_ppm);
        assert!(band_rule_1_holds(params.max_leverage, params.price_band_ppm, fee), "{params:?}");
        assert!(
            band_rule_2_holds(params.min_price, params.max_leverage, params.price_band_ppm, fee),
            "{params:?}"
        );
        let fair = 100_000 + 1_000 * i64::from(market.get());
        assert_eq!((params.min_price, params.max_price), (px(fair / 2), px(2 * fair)));
    }
    assert_eq!(market_params(MarketId::new(3)).max_leverage, 50); // class 0
    assert_eq!(market_params(MarketId::new(1)).max_leverage, 20); // class 1
    assert_eq!(market_params(MarketId::new(2)).max_leverage, 10); // class 2
    assert_eq!(market_params(MarketId::new(1)).min_price, px(50_500)); // the smallest
}

#[test]
fn accounts_follow_the_cohorts_of_14_3() {
    let config = MarketFlowConfig::m3();
    // Account 9 is market maker 0 of market 3, as in the worked example of 5.6.
    assert_eq!(config.maker(MarketId::new(3), 0), acct(9));
    assert_eq!(config.cohort_of(acct(9)), Some(Cohort::Maker { market: MarketId::new(3), index: 0 }));
    assert_eq!(config.cohort_of(acct(268)), Some(Cohort::Maker { market: MarketId::new(67), index: 3 }));
    assert_eq!(config.cohort_of(acct(269)), None);
    assert_eq!(config.cohort_of(acct(1_001)), Some(Cohort::Taker));
    assert_eq!(config.cohort_of(acct(3_000)), Some(Cohort::Taker));
    assert_eq!(config.cohort_of(acct(3_001)), None);
    assert_eq!(
        config.cohort_of(acct(5_001)),
        Some(Cohort::HighLeverage { market: MarketId::new(1), side: Side::Buy })
    );
    assert_eq!(
        config.cohort_of(acct(5_003)),
        Some(Cohort::HighLeverage { market: MarketId::new(1), side: Side::Buy })
    );
    assert_eq!(
        config.cohort_of(acct(5_004)),
        Some(Cohort::HighLeverage { market: MarketId::new(1), side: Side::Sell })
    );
    assert_eq!(
        config.cohort_of(acct(5_402)),
        Some(Cohort::HighLeverage { market: MarketId::new(67), side: Side::Sell })
    );
    assert_eq!(config.cohort_of(acct(7_200)), Some(Cohort::Thin));
    assert_eq!(config.cohort_of(acct(7_201)), None);
    assert_eq!(config.cohort_of(FUND), None);
    let accounts = config.client_accounts();
    assert_eq!(accounts.len(), 2_870); // 2,870 keys (14.3)
    assert!(accounts.windows(2).all(|pair| pair[0] < pair[1]));
    // The smoke flow: one long and one short high-leverage account per market.
    let smoke = MarketFlowConfig::smoke();
    assert_eq!(smoke.client_accounts().len(), 24 + 40 + 12 + 10);
    assert_eq!(
        smoke.cohort_of(acct(5_001)),
        Some(Cohort::HighLeverage { market: MarketId::new(1), side: Side::Buy })
    );
    assert_eq!(
        smoke.cohort_of(acct(5_002)),
        Some(Cohort::HighLeverage { market: MarketId::new(1), side: Side::Sell })
    );
    assert_eq!(
        smoke.cohort_of(acct(5_012)),
        Some(Cohort::HighLeverage { market: MarketId::new(6), side: Side::Sell })
    );
}

#[test]
fn setup_phases_hold_what_14_4_lists_in_its_order() {
    let config = MarketFlowConfig::m3();
    let plan = generate(&config, 0);
    assert_eq!(plan.setup_a.len(), 201 + 1 + 2_870 + 670);
    assert_eq!(plan.setup_b1.len(), 1_608 + 1_000);
    assert_eq!(plan.setup_b2.len(), 1_206);
    assert!(plan.timed.is_empty());

    // A: per market its parameters, tier and first mark; the fund; deposits; leverage.
    assert!(plan.setup_a.iter().all(|item| !item.is_client()));
    assert_eq!(*plan.setup_a[0].command(), Command::SetMarketParams(market_params(MarketId::new(1))));
    assert_eq!(
        *plan.setup_a[2].command(),
        Command::SetMark(SetMark { price: px(101_000), market: MarketId::new(1) })
    );
    let fund = Command::Deposit(Deposit { amount: dollars(1_000), account: FUND });
    assert_eq!(*plan.setup_a[201].command(), fund);
    assert_eq!(
        *plan.setup_a[202].command(),
        Command::Deposit(Deposit { amount: dollars(1_000_000), account: acct(1) })
    );
    let last_deposit = Command::Deposit(Deposit { amount: dollars(100_000), account: acct(7_200) });
    assert_eq!(*plan.setup_a[202 + 2_869].command(), last_deposit);
    let maker = Command::SetLeverage(SetLeverage { account: acct(9), market: MarketId::new(3), leverage: 5 });
    assert_eq!(*plan.setup_a[3_072 + 8].command(), maker);
    let high =
        Command::SetLeverage(SetLeverage { account: acct(5_402), market: MarketId::new(67), leverage: 20 }); // class 1
    assert_eq!(*plan.setup_a.last().expect("items").command(), high);

    // B1: the quotes (post-only, on their own side of F0), then the thin layer (GTC).
    for (i, item) in plan.setup_b1.iter().enumerate() {
        let place = place(item).expect("B1 is places");
        let quote = i < 1_608;
        assert_eq!((place.tif, place.post_only), (TimeInForce::Gtc, quote));
        let fair = start_fair_value(place.market);
        assert!(if place.side == Side::Buy { place.price < fair } else { place.price > fair }, "{place:?}");
    }
    // Market 3's maker 0 quotes 2, 4 and 6 ticks from 103,000.
    let bids: Vec<Price> =
        plan.setup_b1[48..51].iter().map(|item| place(item).expect("a place").price).collect();
    assert_eq!(bids, [px(102_998), px(102_996), px(102_994)]);
    // B2: three IOCs per high-leverage account, in id order, at F0 ± 10.
    for (i, item) in plan.setup_b2.iter().enumerate() {
        let Item::Client(client) = item else { panic!("B2 is client items") };
        let (account, market, side) = config.high_leverage((i / 3) as u32);
        let place = place(item).expect("a place");
        assert_eq!(
            (client.account, place.market, place.side, place.tif),
            (account, market, side, TimeInForce::Ioc)
        );
        let fair = start_fair_value(market);
        assert_eq!(place.price, if side == Side::Buy { fair + px(10) } else { fair - px(10) });
    }
}

#[test]
fn the_same_seed_gives_the_same_plan_and_a_longer_plan_starts_with_a_shorter_one() {
    let config = MarketFlowConfig::m3();
    let a = generate(&config, 20_000);
    let b = generate(&config, 20_000);
    assert_eq!(
        (a.setup_a.clone(), a.setup_b1.clone(), a.setup_b2.clone(), a.timed.clone()),
        (b.setup_a, b.setup_b1, b.setup_b2, b.timed)
    );
    let other = generate(&MarketFlowConfig { seed: 2, ..config }, 20_000);
    assert_ne!(a.setup_b1, other.setup_b1);
    assert_ne!(a.timed, other.timed);

    let longer = generate(&config, 30_000);
    assert_eq!(longer.timed[..a.timed.len()], a.timed[..]);
    assert_eq!(a.timed.iter().filter(|item| item.is_client()).count(), 20_000);
    assert!(a.timed.last().expect("items").is_client(), "the plan ends at its last client item");
    let clients: Vec<&ClientItem> = a.client_items().collect();
    assert_eq!(clients.len(), a.setup_client_items() + 20_000);
    assert_eq!(a.setup_client_items(), 2_608 + 1_206);
}

#[test]
fn the_first_items_and_the_counts_after_1m_items_are_pinned() {
    // Taken from this generator when it was written. The mix test below checks these
    // counts against the model of 14.7; this one pins the exact stream, so that any change
    // in a draw's order shows. If the generator changes on purpose, bump FLOW_VERSION.
    let (_, mut flow) = MarketFlow::start(MarketFlowConfig::m3());
    let first: Vec<Item> = flow.by_ref().take(3).collect();
    let cancel = |account, nonce, sequence, market| {
        Item::Client(ClientItem {
            account,
            nonce,
            command: Command::CancelOrder(CancelOrder {
                order_id: order_id(account, OrderSeq::new(sequence)),
                market,
            }),
        })
    };
    assert_eq!(first[0], cancel(acct(1), 7, 1, MarketId::new(1)));
    let replacement = PlaceOrder {
        order_id: order_id(acct(1), OrderSeq::new(7)),
        price: px(100_999),
        qty: lots(508_366),
        market: MarketId::new(1),
        side: Side::Buy,
        tif: TimeInForce::Gtc,
        post_only: true,
    };
    assert_eq!(
        first[1],
        Item::Client(ClientItem { account: acct(1), nonce: 8, command: Command::PlaceOrder(replacement) })
    );
    let modify = ModifyOrder {
        order_id: order_id(acct(1), OrderSeq::new(4)),
        new_price: px(101_003),
        new_size: lots(448_096),
        market: MarketId::new(1),
    };
    assert_eq!(
        first[2],
        Item::Client(ClientItem { account: acct(1), nonce: 9, command: Command::ModifyOrder(modify) })
    );

    let mut mix = Mix::default();
    first.iter().for_each(|item| mix.add(item));
    let mut last = first[2];
    for item in flow.by_ref().take(1_000_000 - 3) {
        mix.add(&item);
        last = item;
    }
    let pinned =
        Mix { gtc: 323_429, ioc: 40_186, cancels: 323_429, modifies: 306_311, marks: 6_636, withdrawals: 9 };
    assert_eq!(mix, pinned);
    let modify = ModifyOrder {
        order_id: order_id(acct(34), OrderSeq::new(1_212)),
        new_price: px(109_039),
        new_size: lots(326_572),
        market: MarketId::new(9),
    };
    assert_eq!(
        last,
        Item::Client(ClientItem { account: acct(34), nonce: 3_584, command: Command::ModifyOrder(modify) })
    );
    assert_eq!((flow.flow_ns(), flow.jumps().len()), (9_947_014_925, 3));
}

#[test]
fn the_mix_after_20_s_of_flow_time_is_the_one_of_14_7() {
    let (_, mut flow) = MarketFlow::start(MarketFlowConfig::m3());
    let mut mix = Mix::default();
    loop {
        let item = flow.next().expect("the flow never ends");
        if flow.flow_ns() >= 20_000_000_000 {
            break;
        }
        mix.add(&item);
    }
    let clients = mix.clients();
    // 14.7's shares of client commands, within 1.5 points (18.1).
    let near = |part: u64, expected: f64| (percent(part, clients) - expected).abs() < 1.5;
    assert!(near(mix.gtc, 32.5), "{:.2}% resting places", percent(mix.gtc, clients));
    assert!(near(mix.cancels, 32.5), "{:.2}% cancels", percent(mix.cancels, clients));
    assert!(near(mix.modifies, 30.9), "{:.2}% modifies", percent(mix.modifies, clients));
    assert!(near(mix.ioc, 4.05), "{:.2}% IOCs", percent(mix.ioc, clients));
    // Every market maker place replaces a cancelled quote, and so does every thin place.
    assert_eq!(mix.gtc, mix.cancels);
    // About 99,860 client commands, 670 marks and one withdrawal per second of flow time.
    let per_second = clients / 20;
    assert!((97_000..103_000).contains(&per_second), "{per_second} client commands a second");
    // The first mark of each market comes after 100 ms: 67 × (10 × 20 − 1) marks, plus jumps.
    assert!((13_333..13_360).contains(&mix.marks), "{} marks", mix.marks);
    assert_eq!(mix.withdrawals, 19); // at 1 s, 2 s, …, 19 s
    // About 0.3 jumps a second (7 in the model's 20 s).
    assert!((2..=15).contains(&flow.jumps().len()), "{} jumps", flow.jumps().len());
}

/// Checks the invariants of the module docs over a whole plan: nonces and order sequences
/// per account, cancels and modifies of the account's own earlier orders in the right
/// market, prices inside the market's range, and every order inside the band of the
/// generator's latest mark (18.1).
fn check_plan_rules(plan: &FlowPlan) {
    let config = &plan.config;
    let mut last_nonce: HashMap<AccountId, u64> = HashMap::new();
    let mut last_sequence: HashMap<AccountId, u32> = HashMap::new();
    let mut placed: HashMap<OrderId, MarketId> = HashMap::new();
    let mut marks: HashMap<MarketId, Price> = HashMap::new();
    let inside_band = |marks: &HashMap<MarketId, Price>, market: MarketId, price: Price| {
        let edges = band_edges(marks[&market], class_of(market).band_ppm);
        (edges.lower..=edges.upper).contains(&price)
    };
    for item in FlowPhase::ALL.into_iter().flat_map(|phase| plan.phase(phase)) {
        let Item::Client(client) = item else {
            if let Command::SetMark(mark) = item.command() {
                let params = market_params(mark.market);
                assert!((params.min_price..=params.max_price).contains(&mark.price), "{mark:?}");
                marks.insert(mark.market, mark.price);
            }
            continue;
        };
        let account = client.account;
        assert!(config.cohort_of(account).is_some(), "{client:?}");
        let nonce = last_nonce.entry(account).or_default();
        assert_eq!(client.nonce, *nonce + 1, "nonces are 1, 2, 3, … per account: {client:?}");
        *nonce = client.nonce;
        match client.command {
            Command::PlaceOrder(place) => {
                assert_eq!(account_of(place.order_id), account, "{client:?}");
                let sequence = last_sequence.entry(account).or_default();
                assert_eq!(
                    sequence_of(place.order_id).get(),
                    *sequence + 1,
                    "order sequences 1, 2, 3, …: {client:?}"
                );
                *sequence += 1;
                let params = market_params(place.market);
                assert!((params.min_price..=params.max_price).contains(&place.price), "{place:?}");
                assert!(inside_band(&marks, place.market, place.price), "{place:?} outside the band");
                assert!((lots(1)..=config.maker_max_size).contains(&place.qty), "{place:?}");
                placed.insert(place.order_id, place.market);
            }
            Command::CancelOrder(cancel) => {
                assert_eq!(account_of(cancel.order_id), account, "{client:?}");
                assert_eq!(
                    placed.get(&cancel.order_id),
                    Some(&cancel.market),
                    "a cancel of an unknown order: {client:?}"
                );
            }
            Command::ModifyOrder(modify) => {
                assert_eq!(account_of(modify.order_id), account, "{client:?}");
                assert_eq!(
                    placed.get(&modify.order_id),
                    Some(&modify.market),
                    "a modify of an unknown order: {client:?}"
                );
                assert!(inside_band(&marks, modify.market, modify.new_price), "{modify:?} outside the band");
            }
            other => panic!("a client never sends {other:?}"),
        }
    }
}

#[test]
fn nonces_sequences_ownership_ranges_and_bands_hold_over_the_m3_plan() {
    check_plan_rules(&generate(&MarketFlowConfig::m3(), 300_000));
}

#[test]
fn nonces_sequences_ownership_ranges_and_bands_hold_over_the_smoke_plan() {
    // Its jumps come often (1 step in 20), so this covers many pulls and requotes.
    check_plan_rules(&generate(&MarketFlowConfig::smoke(), 100_000));
}

#[test]
fn every_jump_is_recorded_at_its_mark() {
    let plan = generate(&MarketFlowConfig::smoke(), 30_000);
    assert!(plan.jumps.len() > 10, "{} jumps", plan.jumps.len());
    for jump in &plan.jumps {
        assert_eq!(
            *plan.timed[jump.item].command(),
            Command::SetMark(SetMark { price: jump.to, market: jump.market })
        );
        // In ticks: 2% to 6% of the fair value before the jump.
        let (size, from) = ((jump.to - jump.from).ticks().abs(), jump.from.ticks());
        assert!(
            size * 1_000_000 >= from * 20_000 - 1_000_000 && size * 1_000_000 <= from * 60_000,
            "{jump:?}"
        );
        // Every quote of the market is pulled, then placed again.
        let quotes = 4 * 2 * 3;
        let pulled = &plan.timed[jump.item + 1..jump.item + 1 + quotes];
        assert!(
            pulled
                .iter()
                .all(|item| matches!(item.command(), Command::CancelOrder(c) if c.market == jump.market))
        );
        let placed = &plan.timed[jump.item + 1 + quotes..jump.item + 1 + 2 * quotes];
        assert!(
            placed.iter().all(|item| place(item).is_some_and(|p| p.post_only && p.market == jump.market))
        );
    }
    assert!(
        plan.jumps.windows(2).all(|pair| pair[0].item < pair[1].item && pair[0].flow_ns <= pair[1].flow_ns)
    );
}

#[test]
fn every_setup_phase_is_accepted_by_the_engine_and_the_timed_flow_keeps_rejects_rare() {
    let config = MarketFlowConfig::m3();
    let plan = generate(&config, 50_000);
    let mut engine: Engine<Book, Fast> = Engine::new(config.engine_options());
    // 14.4: zero engine rejects in A and B1; B2's IOCs may leave remainders, never rejects.
    for (phase, items) in [("A", &plan.setup_a), ("B1", &plan.setup_b1), ("B2", &plan.setup_b2)] {
        let applied = apply(&mut engine, items);
        assert!(
            applied.rejects.is_empty(),
            "phase {phase}: {:?}",
            &applied.rejects[..applied.rejects.len().min(5)]
        );
    }
    // Every high-leverage account holds its position.
    let snapshot = engine.snapshot();
    let positions =
        snapshot.markets.iter().flat_map(|market| &market.slots).filter(|slot| slot.pos != Qty::ZERO).count();
    assert!(positions >= 402, "{positions} open positions");
    let applied = apply(&mut engine, &plan.timed);
    let clients = plan.timed.iter().filter(|item| item.is_client()).count() as u64;
    let share = percent(applied.rejects.len() as u64, clients);
    assert!(share < 5.0, "{share:.2}% of client commands rejected (INFO.md 7 flags 5%)");
    engine.assert_invariants();
}

#[test]
fn the_smoke_flow_is_accepted_and_liquidates_early() {
    let config = MarketFlowConfig::smoke();
    let plan = generate(&config, 30_000);
    let mut engine: Engine<Book, Fast> = Engine::new(config.engine_options());
    for items in [&plan.setup_a, &plan.setup_b1, &plan.setup_b2] {
        assert!(apply(&mut engine, items).rejects.is_empty());
    }
    // The signed smoke run sends 3,000 client messages (18.4): its seed gives it a jump that
    // liquidates, and the fund's $1 makes that a shortfall, so its replay covers both.
    let end = plan.timed.iter().scan(0, |clients, item| {
        *clients += usize::from(item.is_client());
        Some(*clients)
    });
    let first_3000 = end.take_while(|&clients| clients <= 3_000).count();
    let applied = apply(&mut engine, &plan.timed[..first_3000]);
    assert!(applied.liquidations >= 1 && applied.shortfalls >= 1, "{applied:?}");
    // The pre-verified smoke run sends 30,000 (18.4).
    let rest = apply(&mut engine, &plan.timed[first_3000..]);
    assert!(rest.liquidations > 10, "{} liquidations", rest.liquidations);
    let share = percent((applied.rejects.len() + rest.rejects.len()) as u64, 30_000);
    assert!(share < 5.0, "{share:.2}% of client commands rejected");
    engine.assert_invariants();
}

#[test]
fn check_refuses_a_config_the_generator_cannot_run() {
    assert_eq!(MarketFlowConfig::m3().check(), Ok(()));
    assert_eq!(MarketFlowConfig::smoke().check(), Ok(()));
    let m3 = MarketFlowConfig::m3();
    for bad in [
        MarketFlowConfig { markets: 0, ..m3 },
        MarketFlowConfig { markets: 251, ..m3 }, // 1,004 market makers
        MarketFlowConfig { takers: 0, ..m3 },
        MarketFlowConfig { takers: 4_001, ..m3 },
        MarketFlowConfig { high_leverage_per_market: 30, ..m3 }, // 2,010 accounts
        MarketFlowConfig { jump_one_in: 0, ..m3 },
        MarketFlowConfig { maker_min_size: lots(2_000_000), ..m3 },
        MarketFlowConfig { step_ns: 0, ..m3 },
    ] {
        assert!(bad.check().is_err(), "{bad:?}");
    }
    assert!(MarketFlowConfig { markets: 250, ..m3 }.check().is_ok()); // exactly 1,000
}

#[test]
fn the_digest_names_the_config() {
    let m3 = MarketFlowConfig::m3();
    assert_eq!(m3.digest(), MarketFlowConfig::m3().digest());
    let changed = [
        MarketFlowConfig { seed: 2, ..m3 },
        MarketFlowConfig { fund_deposit: m3.fund_deposit + Micros::new(1), ..m3 },
        MarketFlowConfig { thin_max_qty: m3.thin_max_qty + lots(1), ..m3 },
        MarketFlowConfig::smoke(),
    ];
    for other in changed {
        assert_ne!(other.digest(), m3.digest(), "{other:?}");
    }
}
