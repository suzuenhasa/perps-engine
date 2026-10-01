//! The sender on real rings, with the test's thread standing in for the pipeline: it reads
//! the rings and moves the `released` counter the barriers wait on. Each test decides when
//! to read, so what it checks doesn't depend on thread timing: a sender that waited for the
//! operator ring, or passed a barrier early, would make a test hang (and fail at its
//! deadline) or see a record too soon.

use std::thread::JoinHandle;

use super::*;
use crate::market_flow::{ClientItem, MarketFlowConfig, generate};
use crate::presign::{presign, preverified};
use engine::command::{CancelOrder, Command, SetMark};
use engine::types::{AccountId, MarketId, OrderSeq, Price, order_id};
use gateway::wire::decode;
use pipeline::codec::decode_command;
use pipeline::gate::Phase;
use pipeline::records::MESSAGE_BYTES;
use pipeline::ring::{Consumer, channel};

const IDLE: IdleStrategy = IdleStrategy::SpinThenYield { spins: 16 };

/// Account number `account`'s cancel of its order `n`, with nonce `n`.
fn client(account: u32, n: u32) -> Item {
    let account = AccountId::new(account);
    let command = Command::CancelOrder(CancelOrder {
        order_id: order_id(account, OrderSeq::new(n)),
        market: MarketId::new(1),
    });
    Item::Client(ClientItem { account, nonce: u64::from(n), command })
}

/// An operator mark of `price` ticks on market 1.
fn mark(price: i64) -> Item {
    Item::Operator(Command::SetMark(SetMark { price: Price::new(price), market: MarketId::new(1) }))
}

/// A plan with these phases, for the smoke flow's accounts and seed.
fn flow_plan(setup_a: Vec<Item>, setup_b1: Vec<Item>, timed: Vec<Item>) -> FlowPlan {
    let config = MarketFlowConfig::smoke();
    FlowPlan { config, setup_a, setup_b1, setup_b2: Vec::new(), timed, jumps: Vec::new(), flow_ns: 0 }
}

/// Evenly spaced at `rate`, every timed client item sent, and all of them measured.
fn timing(rate: u64, timed_clients: usize) -> Timing {
    Timing {
        arrivals: Arrivals::Uniform,
        setup_rate: rate,
        rate,
        timed_clients,
        warmup_ns: 0,
        window_ns: u64::MAX,
    }
}

/// The sender on its thread, and the other ends of its rings.
struct Rig {
    clients: Vec<Consumer<3>>,
    operator: Consumer<1>,
    pipeline: Arc<PipelineCounters>,
    counters: Arc<SenderCounters>,
    phases: Arc<Phases>,
    stop: Arc<AtomicBool>,
    /// `None` once joined.
    sender: Option<JoinHandle<SenderStats>>,
}

impl Rig {
    fn start(plan: SenderPlan, rings: usize, capacity: usize, operator_capacity: usize, stop: bool) -> Rig {
        Rig::start_with_timeout(plan, rings, capacity, operator_capacity, stop, Duration::from_secs(10))
    }

    fn start_with_timeout(
        plan: SenderPlan,
        rings: usize,
        capacity: usize,
        operator_capacity: usize,
        stop: bool,
        timeout: Duration,
    ) -> Rig {
        let (producers, clients): (Vec<_>, Vec<_>) = (0..rings).map(|_| channel::<3>(capacity)).unzip();
        let (operator_in, operator) = channel::<1>(operator_capacity);
        let pipeline = Arc::new(PipelineCounters::new());
        let counters = Arc::new(SenderCounters::default());
        let phases = Arc::new(Phases::not_yet());
        let stop = Arc::new(AtomicBool::new(stop));
        let barrier = Barrier {
            poll: Duration::from_micros(200),
            timeout,
            ..Barrier::new(Arc::clone(&pipeline), Vec::new())
        };
        let config = SenderConfig {
            clock: RunClock::start(),
            idle: IDLE,
            cpu: None,
            barrier,
            stop: Arc::clone(&stop),
            phases: Arc::clone(&phases),
            counters: Arc::clone(&counters),
        };
        let outputs = SenderOutputs { clients: producers, operator: operator_in };
        let sender = spawn_sender(plan, outputs, config);
        Rig { clients, operator, pipeline, counters, phases, stop, sender: Some(sender) }
    }

    /// Waits for the sender to end.
    fn join(&mut self) -> SenderStats {
        self.sender.take().expect("joined once").join().expect("the sender ends")
    }

    /// The next record of client ring `g`, waiting for it up to 10 s.
    fn read_client(&mut self, g: usize) -> [u64; ClientRecord::WORDS] {
        let mut words = [0; ClientRecord::WORDS];
        read(&mut self.clients[g], &mut words);
        words
    }

    fn read_operator(&mut self) -> OperatorRecord {
        let mut words = [0; OperatorRecord::WORDS];
        read(&mut self.operator, &mut words);
        OperatorRecord::from_words(&words)
    }

    /// The pipeline releases `n` more commands.
    fn release(&self, n: u64) {
        self.pipeline.released.add(n);
    }
}

/// Reads the next record of `ring` into `out`, waiting for it up to 10 s.
fn read<const LINES: usize>(ring: &mut Consumer<LINES>, out: &mut [u64]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while ring.available(1) == 0 {
        assert!(Instant::now() < deadline, "no record arrived within 10 s");
        IDLE.idle();
    }
    ring.read(out);
    ring.release();
}

#[test]
fn a_full_operator_ring_holds_back_no_client_item_and_its_items_follow_in_order() {
    // 18.1: against a full operator ring, client items stay on schedule, and the pending
    // operator items go out in order once there is room.
    let timed = vec![
        client(1, 1),
        client(1, 2),
        mark(1),
        client(1, 3),
        mark(2),
        mark(3),
        client(1, 4),
        client(1, 5),
        client(1, 6),
        client(1, 7),
        client(1, 8),
        client(1, 9),
        mark(4),
        mark(5),
        client(1, 10),
    ];
    let plan = flow_plan(Vec::new(), Vec::new(), timed);
    let messages = Messages::PreVerified(Arc::new(preverified(&plan)));
    // The operator ring holds one item: the first mark fills it, and nobody reads it yet.
    let mut rig = Rig::start(SenderPlan::new(&plan, messages, &timing(100_000, 10)), 1, 16, 1, false);

    // Every client item arrives while four marks are still pending: the sender never waited.
    let clients: Vec<&ClientItem> = plan.client_items().collect();
    let mut t_sched = Vec::new();
    for item in clients {
        let record = ClientRecord::from_words(&rig.read_client(0));
        assert_eq!(
            record.meta,
            Meta { source: Source::PreVerifiedClient, lane: 0, account: AccountId::new(1) }
        );
        assert_eq!(record.nonce, item.nonce);
        assert_eq!(decode_command(&record.command), Ok(item.command));
        assert_eq!((record.expires_at, record.signature, record.t_gw_in, record.t_gw_out), (0, [0; 8], 0, 0));
        assert!(record.t_sent >= record.t_sched);
        assert!(t_sched.last().is_none_or(|&last| record.t_sched > last));
        t_sched.push(record.t_sched);
    }
    // Now the marks, in order, each scheduled with the client item before it.
    for (price, after_client) in [(1, 2), (2, 3), (3, 3), (4, 9), (5, 9)] {
        let record = rig.read_operator();
        let price = Price::new(price);
        assert_eq!(
            decode_command(&record.command),
            Ok(Command::SetMark(SetMark { price, market: MarketId::new(1) }))
        );
        assert_eq!(record.t_sched, t_sched[after_client - 1], "mark {price}");
        assert!(record.t_sent >= record.t_sched);
    }
    let stats = rig.join();
    assert_eq!(stats.end, SenderEnd::Finished);
    assert_eq!((stats.client_sent, stats.operator_sent, stats.offered), (10, 5, 15));
    assert_eq!(stats.max_operator_backlog, 4); // marks 2 to 5, behind mark 1
    assert!(stats.longest_operator_backlog_ns > 0);
    assert_eq!((stats.window_offered, stats.window_dropped, stats.lag.count()), (10, 0, 10));
    assert!(rig.operator.is_finished() && rig.clients[0].is_finished(), "the sender closed its rings");
}

#[test]
fn a_barrier_waits_10_s_or_its_allowance_per_item_if_that_is_longer() {
    // Review finding F10: at one fdatasync per record, phase A's 3,742 items need 3,742 × F.
    let mut barrier = Barrier::new(Arc::new(PipelineCounters::new()), Vec::new());
    assert_eq!(barrier.timeout_for(3_742), Duration::from_secs(10));
    barrier.per_item = Duration::from_millis(12); // 4 × an F of 3 ms
    assert_eq!(barrier.timeout_for(3_742), Duration::from_millis(44_904));
    assert_eq!(barrier.timeout_for(10), Duration::from_secs(10), "never below 10 s");
}

#[test]
fn a_sender_that_is_always_late_still_publishes_its_busy_time() {
    // Review finding F1-limit-label-fdatasync (c): busy time was published only when the
    // sender waited, so a sender that was always late showed none in the window. Here every
    // item is due at once, and the operator ring holds one of the two marks, so the sender
    // stays in its phase, pushing the second, until the test reads the first. By then it has
    // spent well over a millisecond on 50,000 late items (sent or dropped), and that must
    // show; the empty setup phases before them account for well under a microsecond.
    let mut timed = vec![mark(1), mark(2)];
    timed.extend((1..=50_000).map(|n| client(1, n)));
    let plan = flow_plan(Vec::new(), Vec::new(), timed);
    let messages = Messages::PreVerified(Arc::new(preverified(&plan)));
    let at_once = timing(1_000_000_000_000, 50_000);
    let mut rig = Rig::start(SenderPlan::new(&plan, messages, &at_once), 1, 4_096, 1, false);
    let deadline = Instant::now() + Duration::from_secs(10);
    while rig.counters.thread.busy_ns.load() < 1_000_000 {
        assert!(Instant::now() < deadline, "no busy time published while the sender was late");
        std::thread::yield_now();
    }
    rig.read_operator();
    rig.read_operator();
    assert_eq!(rig.join().end, SenderEnd::Finished);
}

#[test]
fn barriers_hold_each_phase_until_the_last_is_resolved_and_drops_count_as_resolved() {
    // Accounts 2 and 4 go to ring 0, accounts 1 and 3 to ring 1.
    let setup_a = vec![mark(1), mark(2), mark(3)];
    let setup_b1 = vec![client(1, 1), client(2, 1), client(3, 1), client(4, 1)];
    let timed = vec![
        client(2, 2),
        client(4, 2),
        client(1, 2),
        client(2, 3),
        client(4, 3),
        client(3, 2),
        client(2, 4),
        client(4, 4),
        client(1, 3),
    ];
    let plan = flow_plan(setup_a, setup_b1, timed);
    let messages = Messages::PreVerified(Arc::new(preverified(&plan)));
    let mut rig = Rig::start(SenderPlan::new(&plan, messages, &timing(200_000, 9)), 2, 4, 8, false);

    // Phase A: three marks. Until they are released, nothing of B1 may be sent.
    for _ in 0..3 {
        rig.read_operator();
    }
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(
        (rig.clients[0].available(1), rig.clients[1].available(1)),
        (0, 0),
        "B1 went before A was resolved"
    );
    rig.release(3);
    // B1: two items per ring. Until they are released, nothing of the timed flow is sent.
    for g in [0, 0, 1, 1] {
        let record = ClientRecord::from_words(&rig.read_client(g));
        assert_eq!((usize::from(record.meta.lane), record.meta.account.get() as usize % 2), (g, g));
    }
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(
        (rig.clients[0].available(1), rig.clients[1].available(1)),
        (0, 0),
        "timed went before B1 was resolved"
    );
    rig.release(4);

    // The timed flow: six items for ring 0, which holds four, so two are dropped.
    let stats = rig.join();
    assert_eq!(stats.end, SenderEnd::Finished);
    assert_eq!(stats.dropped, [2, 0]);
    assert_eq!((stats.offered, stats.client_sent, stats.operator_sent), (3 + 4 + 9, 11, 3));
    assert_eq!((stats.window_offered, stats.window_dropped), (9, 2));
    let timed_start = stats.timed_start.expect("the timed flow started");
    assert_eq!(rig.phases.phase_of(timed_start), Phase::Window);
    assert_eq!(rig.phases.phase_of(timed_start - 1), Phase::Setup);
    for (g, expected) in [(0, 4), (1, 3)] {
        for _ in 0..expected {
            let record = ClientRecord::from_words(&rig.read_client(g));
            assert_eq!(usize::from(record.meta.lane), g);
            assert_eq!(record.meta.account.get() as usize % 2, g);
        }
        assert!(rig.clients[g].is_finished());
    }
}

#[test]
fn a_barrier_that_times_out_ends_the_run_and_closes_the_rings() {
    let plan = flow_plan(vec![mark(1), mark(2)], vec![client(1, 1)], Vec::new());
    let messages = Messages::PreVerified(Arc::new(preverified(&plan)));
    let sender_plan = SenderPlan::new(&plan, messages, &timing(1_000, 0));
    let mut rig = Rig::start_with_timeout(sender_plan, 1, 4, 4, false, Duration::from_millis(20));
    let stats = rig.join();
    // Nobody released the two marks.
    assert_eq!(stats.end, SenderEnd::BarrierTimeout { phase: FlowPhase::SetupA, resolved: 0, offered: 2 });
    assert_eq!(stats.client_sent, 0);
    rig.read_operator();
    rig.read_operator();
    assert!(rig.operator.is_finished() && rig.clients[0].is_finished());
}

#[test]
fn stop_ends_the_sender_before_its_next_item_or_while_it_waits() {
    let plan = flow_plan(vec![mark(1)], Vec::new(), vec![client(1, 1)]);
    let messages = Messages::PreVerified(Arc::new(preverified(&plan)));
    let mut rig = Rig::start(SenderPlan::new(&plan, messages.clone(), &timing(1, 1)), 1, 4, 4, true);
    let stats = rig.join();
    assert_eq!(stats.end, SenderEnd::Stopped { phase: FlowPhase::SetupA });
    assert_eq!(stats.offered, 0);
    assert!(rig.operator.is_finished());

    // At one client item a second, the timed item is due a second after the flow starts:
    // the sender is waiting for it when told to stop.
    let mut rig = Rig::start(SenderPlan::new(&plan, messages, &timing(1, 1)), 1, 4, 4, false);
    rig.read_operator();
    rig.release(1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while rig.phases.phase_of(u64::MAX - 1) != Phase::Window {
        assert!(Instant::now() < deadline, "the timed flow didn't start");
        IDLE.idle();
    }
    rig.stop.store(true, Ordering::Relaxed);
    let stats = rig.join();
    assert_eq!(stats.end, SenderEnd::Stopped { phase: FlowPhase::Timed });
    assert_eq!(stats.client_sent, 0);
    assert!(rig.clients[0].is_finished());
}

#[test]
fn signed_messages_go_to_their_gateway_as_ingress_slots() {
    let timed: Vec<Item> = (1..=6).map(|account| client(account, 1)).collect();
    let plan = flow_plan(Vec::new(), Vec::new(), timed);
    let arena = Arc::new(presign(&plan, 5, 2));
    let messages = Messages::Signed(Arc::clone(&arena));
    let mut rig = Rig::start(SenderPlan::new(&plan, messages, &timing(100_000, 6)), 3, 8, 4, false);
    let stats = rig.join();
    assert_eq!((stats.end, stats.client_sent), (SenderEnd::Finished, 6));
    for account in 1..=6u32 {
        let g = account as usize % 3;
        let mut words = [0; IngressSlot::WORDS];
        read(&mut rig.clients[g], &mut words);
        let slot = IngressSlot::from_words(&words);
        assert_eq!(slot.message, arena.message_bytes(account as usize - 1));
        let decoded = decode(&slot.message).expect("decodes");
        assert_eq!((decoded.deployment, decoded.account), (5, AccountId::new(account)));
        assert!(slot.t_sent >= slot.t_sched);
    }
}

#[test]
fn ingress_words_are_an_ingress_slot() {
    let message: [u8; MESSAGE_BYTES] = std::array::from_fn(|i| i as u8);
    let words = ingress_words(&pipeline::records::message_words(&message), 11, 12);
    assert_eq!(words, IngressSlot { message, t_sched: 11, t_sent: 12 }.to_words());
}

#[test]
fn the_plan_indexes_the_arena_in_plan_order_and_takes_a_prefix_of_the_timed_flow() {
    let plan = generate(&MarketFlowConfig::smoke(), 200);
    let messages = Messages::PreVerified(Arc::new(preverified(&plan)));
    let timing = Timing { warmup_ns: 5, window_ns: 10, ..timing(20_000, 50) };
    let sender_plan = SenderPlan::new(&plan, messages, &timing);
    let [a, b1, b2, timed] = &sender_plan.phases[..] else { panic!("four phases") };
    assert_eq!([a.phase, b1.phase, b2.phase, timed.phase], FlowPhase::ALL);
    assert_eq!((a.clients(), a.operators.len()), (0, plan.setup_a.len()));
    assert!(a.schedule.iter().all(|&t| t == 0), "phase A goes as fast as the ring takes it");
    assert_eq!((b1.first_client, b2.first_client), (0, plan.setup_b1.len()));
    assert_eq!(timed.first_client, plan.setup_client_items());
    assert_eq!(timed.clients(), 50);
    let prefix = &plan.timed[..timed.schedule.len()];
    assert!(prefix.last().expect("items").is_client());
    for send in &timed.operators {
        assert_eq!(Item::Operator(decode_command(&send.command).expect("decodes")), prefix[send.position]);
    }
    assert!(timed.schedule.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(sender_plan.window, 5..15);
}

#[test]
fn a_longer_plans_messages_serve_a_shorter_run() {
    let longer = generate(&MarketFlowConfig::smoke(), 100);
    let plan = generate(&MarketFlowConfig::smoke(), 10);
    let messages = Messages::PreVerified(Arc::new(preverified(&longer)));
    assert_eq!(SenderPlan::new(&plan, messages, &timing(20_000, 10)).phases[3].clients(), 10);
}

#[test]
#[should_panic(expected = "the arena has")]
fn a_plan_needs_a_message_for_every_client_item() {
    let plan = generate(&MarketFlowConfig::smoke(), 10);
    let shorter = generate(&MarketFlowConfig::smoke(), 5);
    SenderPlan::new(&plan, Messages::PreVerified(Arc::new(preverified(&shorter))), &timing(20_000, 10));
}

#[test]
#[should_panic(expected = "another flow")]
fn a_plan_refuses_messages_of_another_flow() {
    let plan = generate(&MarketFlowConfig::smoke(), 10);
    let other = generate(&MarketFlowConfig { seed: 2, ..MarketFlowConfig::smoke() }, 10);
    SenderPlan::new(&plan, Messages::PreVerified(Arc::new(preverified(&other))), &timing(20_000, 10));
}
