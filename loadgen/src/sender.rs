//! The open-loop sender: sends each pre-built item at its scheduled time, whether or not
//! the pipeline keeps up (`docs/PIPELINE.md` 14.10, 2.4, 2.8 and 14.4; `docs/DECISIONS.md`
//! D-024 and D-027).
//!
//! **Contract** (14.10). For each phase of the [`SenderPlan`] in order, with `t0` the run
//! clock at the phase's start, item `i` is due at `t_sched = t0 + schedule[i]`:
//! - The sender spins until `t_sched` (it never sleeps: waking takes tens of microseconds),
//!   pushing pending operator items while it waits. A late item goes out at once, with its
//!   original `t_sched`, so any lateness shows in the latencies, which are always measured
//!   from `t_sched` (15.2).
//! - **A client item** goes to its account's ring, `account mod N`: the gateway's ingress
//!   ring in a signed run (an `IngressSlot`: the 136-byte message, `t_sched`, `t_sent`), the
//!   sequencer's lane in a pre-verified run (a `ClientRecord` with source 2 and no
//!   signature). **If that ring is full, the message is dropped and counted**
//!   (`IngressFull`), as a full socket buffer would drop it. Waiting would make every later
//!   message late and hide the delay from the measurements (coordinated omission), and one
//!   slow gateway would hold up the others (2.4).
//! - **An operator item** becomes due, and is pushed into the operator ring if there is
//!   room. If there isn't, it stays pending, in order, and is pushed as soon as room
//!   appears, while client items keep going out on schedule. Operator commands are never
//!   dropped (they are the exchange's own inputs) and never hold up a client item. Its
//!   latency still counts from its own `t_sched`.
//! - After the phase's last item, the sender keeps pushing until nothing is pending.
//!
//! **Barriers between phases** (14.4). Before the next phase starts, every item of the ones
//! before must be resolved: `released + gateway rejects + ingress drops == items offered`,
//! polled every 100 µs. Different accounts use different lanes, whose order at the
//! sequencer isn't promised (9.1), so without a barrier an account's first order could be
//! sequenced before its deposit. A barrier that takes more than 10 s aborts the run (or
//! longer, by an allowance per item of the phase, for a journal that syncs every record:
//! [`Barrier::per_item`]).
//!
//! **The measured window** (15.5). When the timed phase starts, the sender sets the run's
//! [`Phases`] (the setup, then warm-up, then the window, by `t_sched`), since only it knows
//! when that is. The gate and the gateways read them to decide what is measured.
//!
//! **Stopping** (2.8). The sender stops at the end of its plan, when a barrier times out,
//! or when `stop` is set; either way returning drops its ring ends, which closes the rings
//! and starts the pipeline's shutdown cascade.
//!
//! **What it measures.** Its own lag, `t_sent − t_sched`, over the window's client items:
//! if its p99 is above 5 µs the run is "generator-limited" and doesn't count (14.10). Since
//! the sender never waits for ring space, its lag can only come from its own CPU (or the OS
//! taking it away), never from the pipeline being behind. Also: drops per ring, in total and
//! in the window (the window's are "not served", 15.4), and the largest operator backlog and
//! the longest time there was one.
//!
//! **Allocation.** Everything is allocated when the sender is built; sending allocates
//! nothing (the smoke test checks the pinned threads, 18.4).
//!
//! **Complexity.** Per item: one clock read (two if it has to wait), one ring write and one
//! Release store; per client item also one histogram record if it is in the window.
//!
//! **Not built:** `--senders L` (2.2). One sender needs about 50 ns per message, so it
//! keeps up to well past 1M messages/s; the lag histogram says when it doesn't.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gateway::{GatewayCounters, gateway_of};
use pipeline::affinity::pin_current_thread;
use pipeline::clock::RunClock;
use pipeline::codec::{COMMAND_WORDS, encode_command};
use pipeline::counters::{BusyMeter, PipelineCounters, ThreadCounters};
use pipeline::gate::Phases;
use pipeline::histogram::LatencyHistogram;
use pipeline::idle::IdleStrategy;
use pipeline::records::{
    ClientRecord, IngressSlot, MESSAGE_WORDS, Meta, OperatorRecord, SIGNATURE_WORDS, Source,
};
use pipeline::ring::Producer;

use crate::market_flow::{FlowPhase, FlowPlan, Item, PlanConfig};
use crate::presign::{Arena, CompactArena, message_account};
use crate::schedule::{Arrivals, Schedule};

/// Client commands a second in setup phases B1 and B2 (14.4, 14.9).
pub const SETUP_RATE: u64 = 20_000;
/// A sender lag p99 above this marks the run "generator-limited" (14.10).
pub const GENERATOR_LIMIT_NS: u64 = 5_000;

// ---------------------------------------------------------------------------------------
// The plan the sender follows.

/// The messages of every client item, in plan order: signed, or compact for a
/// pre-verified run. Which one also decides where client items go (module docs).
#[derive(Clone, Debug)]
pub enum Messages {
    Signed(Arc<Arena>),
    PreVerified(Arc<CompactArena>),
}

impl Messages {
    pub fn len(&self) -> usize {
        match self {
            Messages::Signed(arena) => arena.len(),
            Messages::PreVerified(arena) => arena.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The digest of the flow config the messages were built from.
    pub fn flow_digest(&self) -> [u8; 32] {
        match self {
            Messages::Signed(arena) => arena.header().flow_digest,
            Messages::PreVerified(arena) => arena.flow_digest(),
        }
    }
}

/// An operator command, and its position among its phase's items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperatorSend {
    pub position: usize,
    /// CMD40 words.
    pub command: [u64; COMMAND_WORDS],
}

/// One phase, as the sender sends it: which items are operator commands (the rest are
/// client items, in arena order), and when each item is due.
#[derive(Clone, Debug)]
pub struct PhaseSends {
    pub phase: FlowPhase,
    /// The phase's operator commands, in order.
    pub operators: Vec<OperatorSend>,
    /// The arena index of the phase's first client item.
    pub first_client: usize,
    /// Each item's send time, in ns from the phase's start (14.9).
    pub schedule: Vec<u64>,
}

impl PhaseSends {
    fn new(phase: FlowPhase, items: &[Item], first_client: usize, schedule: Vec<u64>) -> PhaseSends {
        let operators = items
            .iter()
            .enumerate()
            .filter_map(|(position, item)| match item {
                Item::Operator(command) => Some(OperatorSend { position, command: encode_command(command) }),
                Item::Client(_) => None,
            })
            .collect();
        PhaseSends { phase, operators, first_client, schedule }
    }

    /// Client items in the phase.
    pub fn clients(&self) -> usize {
        self.schedule.len() - self.operators.len()
    }
}

/// How a run's items are timed (14.9, 15.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub arrivals: Arrivals,
    /// Client commands a second in setup phases B1 and B2: [`SETUP_RATE`].
    pub setup_rate: u64,
    /// The offered rate of the timed flow: client commands a second.
    pub rate: u64,
    /// Client items of the timed flow to send: the plan's first ones (a run may use a
    /// prefix of a longer plan, 14.8).
    pub timed_clients: usize,
    /// The warm-up, then the measured window, from the timed flow's start (15.5).
    pub warmup_ns: u64,
    pub window_ns: u64,
}

/// Everything the sender sends, and when (module docs).
#[derive(Clone, Debug)]
pub struct SenderPlan {
    pub messages: Messages,
    /// The phases in order: setup A, B1, B2, then the timed flow.
    pub phases: Vec<PhaseSends>,
    /// The measured window, in ns from the timed flow's start.
    pub window: Range<u64>,
}

impl SenderPlan {
    /// The sender's plan for `plan`: its setup phases and the first `timing.timed_clients`
    /// client items of its timed flow, with `messages` built from the same flow (this plan,
    /// or a longer one). The send times come from one `SCHEDULE` stream of the plan's seed,
    /// phase after phase. Panics if the messages come from another flow, or are too few.
    pub fn new<C: PlanConfig>(plan: &FlowPlan<C>, messages: Messages, timing: &Timing) -> SenderPlan {
        let timed = timed_prefix(&plan.timed, timing.timed_clients);
        // Message `k` is the plan's client item `k` only if both come from the same flow.
        assert!(messages.flow_digest() == plan.config.digest(), "the messages were built from another flow");
        let needed = plan.setup_client_items() + timing.timed_clients;
        assert!(
            messages.len() >= needed,
            "the run needs {needed} messages, the arena has {}",
            messages.len()
        );
        let mut schedule = Schedule::new(timing.arrivals, plan.config.seed());
        let mut first_client = 0;
        let mut phases = Vec::new();
        for phase in FlowPhase::ALL {
            let (items, rate) = match phase {
                FlowPhase::Timed => (timed, timing.rate),
                setup => (plan.phase(setup), timing.setup_rate),
            };
            phases.push(PhaseSends::new(phase, items, first_client, schedule.send_times(items, rate)));
            first_client += items.iter().filter(|item| item.is_client()).count();
        }
        let window = timing.warmup_ns..timing.warmup_ns.saturating_add(timing.window_ns);
        SenderPlan { messages, phases, window }
    }
}

/// The timed items up to and including the `clients`-th client item.
fn timed_prefix(items: &[Item], clients: usize) -> &[Item] {
    if clients == 0 {
        return &[];
    }
    let mut seen = 0;
    for (i, item) in items.iter().enumerate() {
        seen += usize::from(item.is_client());
        if seen == clients {
            return &items[..=i];
        }
    }
    panic!("the plan has {seen} timed client items, the run needs {clients}")
}

// ---------------------------------------------------------------------------------------
// What the sender is given.

/// The sender's ring ends. Dropping them closes the rings (2.8).
#[derive(Debug)]
pub struct SenderOutputs {
    /// One per gateway (`ingress[g]`) in a signed run, one per lane (`lane[g]`) in a
    /// pre-verified run. Account `a` goes to ring `a mod N`.
    pub clients: Vec<Producer<3>>,
    pub operator: Producer<1>,
}

/// What a barrier waits on (module docs): `released + gateway rejects + ingress drops ==
/// items offered`.
#[derive(Debug)]
pub struct Barrier {
    /// The pipeline's counters (`Pipeline::shared_counters`): commands released.
    pub pipeline: Arc<PipelineCounters>,
    /// Each gateway's counters (signed runs; none in a pre-verified run): rejects.
    pub gateways: Vec<Arc<GatewayCounters>>,
    /// How often to look: 100 µs.
    pub poll: Duration,
    /// How long to wait before aborting the run: 10 s...
    pub timeout: Duration,
    /// ...or this much per item of the phase, if that is longer: 0 by default. The "fsync
    /// per order" ablation journals setup with one `fdatasync` per record (section 16), so
    /// phase A's 3,742 items alone take `3,742 × F`: over 10 s once `F` passes about 2.7 ms.
    pub per_item: Duration,
}

impl Barrier {
    /// A barrier with 14.4's poll interval and timeout.
    pub fn new(pipeline: Arc<PipelineCounters>, gateways: Vec<Arc<GatewayCounters>>) -> Barrier {
        Barrier {
            pipeline,
            gateways,
            poll: Duration::from_micros(100),
            timeout: Duration::from_secs(10),
            per_item: Duration::ZERO,
        }
    }

    /// How long the barrier after a phase of `items` items may take (the fields' docs).
    fn timeout_for(&self, items: usize) -> Duration {
        let allowance = self.per_item.saturating_mul(u32::try_from(items).unwrap_or(u32::MAX));
        self.timeout.max(allowance)
    }

    /// Items resolved outside the sender: released by the gate, or rejected by a gateway.
    /// Each count is read Relaxed, so it may be stale, but only low (3.2): a barrier can be
    /// delayed by it, never passed early.
    fn resolved_downstream(&self) -> u64 {
        let rejected: u64 = self.gateways.iter().map(|gateway| gateway.rejected.load()).sum();
        self.pipeline.released.load() + rejected
    }
}

/// What the sender shares with main while the run goes (3.2): its busy time and its OS
/// thread id (15.4). Its counts are its own ([`SenderStats`]): the barriers are the sender's,
/// and main reads the counts when the sender is joined.
#[derive(Debug, Default)]
pub struct SenderCounters {
    pub thread: ThreadCounters,
}

/// Everything the sender thread needs besides its plan and its rings.
#[derive(Debug)]
pub struct SenderConfig {
    /// The run clock, shared with the pipeline and the gateways.
    pub clock: RunClock,
    pub idle: IdleStrategy,
    /// The CPU to pin to (2.6); `None`: not pinned.
    pub cpu: Option<usize>,
    pub barrier: Barrier,
    /// Set by main to stop the sender early.
    pub stop: Arc<AtomicBool>,
    /// The run's phases (`Pipeline::shared_phases`), which the sender sets when the timed
    /// flow starts.
    pub phases: Arc<Phases>,
    pub counters: Arc<SenderCounters>,
}

// ---------------------------------------------------------------------------------------
// What the sender returns.

/// Why the sender stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SenderEnd {
    /// It sent its whole plan.
    Finished,
    /// `stop` was set during `phase`.
    Stopped { phase: FlowPhase },
    /// The barrier after `phase` timed out: `resolved` of `offered` items were resolved.
    BarrierTimeout { phase: FlowPhase, resolved: u64, offered: u64 },
}

/// What the sender counted (module docs).
#[derive(Clone, Debug)]
pub struct SenderStats {
    pub end: SenderEnd,
    /// Items offered: client items sent or dropped, and operator items pushed.
    pub offered: u64,
    pub client_sent: u64,
    pub operator_sent: u64,
    /// Client items dropped because their ring was full (`IngressFull`), per ring.
    pub dropped: Vec<u64>,
    /// Client items scheduled inside the window, and how many of them were dropped.
    pub window_offered: u64,
    pub window_dropped: u64,
    /// `t_sent − t_sched` of every client item scheduled inside the window (14.10).
    pub lag: LatencyHistogram,
    /// The most operator items pending at once, and the longest stretch with any pending.
    pub max_operator_backlog: u64,
    pub longest_operator_backlog_ns: u64,
    /// Run-clock time the timed flow started: the start of the window's reckoning.
    pub timed_start: Option<u64>,
}

impl SenderStats {
    fn new(rings: usize) -> SenderStats {
        SenderStats {
            end: SenderEnd::Finished,
            offered: 0,
            client_sent: 0,
            operator_sent: 0,
            dropped: vec![0; rings],
            window_offered: 0,
            window_dropped: 0,
            lag: LatencyHistogram::new(),
            max_operator_backlog: 0,
            longest_operator_backlog_ns: 0,
            timed_start: None,
        }
    }

    /// Every client item dropped.
    pub fn dropped_total(&self) -> u64 {
        self.dropped.iter().sum()
    }

    /// True if the sender's lag p99 was above 5 µs: the run doesn't count (14.10).
    pub fn generator_limited(&self) -> bool {
        self.lag.percentile(99, 100).is_some_and(|p99| p99 > GENERATOR_LIMIT_NS)
    }
}

// ---------------------------------------------------------------------------------------
// The thread.

/// Starts the sender thread (19.2): it pins itself, publishes its thread id and runs
/// [`run_sender`]. It doesn't install the abort-on-panic hook: a sender that panicked would
/// only close its rings early, and the pipeline would stop cleanly.
pub fn spawn_sender(
    plan: SenderPlan,
    outputs: SenderOutputs,
    config: SenderConfig,
) -> JoinHandle<SenderStats> {
    std::thread::Builder::new()
        .name("sender".to_string())
        .spawn(move || {
            if let Some(cpu) = config.cpu {
                pin_current_thread(cpu).unwrap_or_else(|e| panic!("pinning the sender to CPU {cpu}: {e}"));
            }
            let _ = config.counters.thread.publish_tid(); // if it fails, only the fault count is lost
            run_sender(&plan, outputs, &config)
        })
        .unwrap_or_else(|e| panic!("spawning the sender thread: {e}"))
}

/// The sender's loop on the calling thread (module docs). Returning drops `outputs`, which
/// closes every ring (2.8).
pub fn run_sender(plan: &SenderPlan, outputs: SenderOutputs, config: &SenderConfig) -> SenderStats {
    let mut sender = Sender {
        plan,
        config,
        stats: SenderStats::new(outputs.clients.len()),
        outputs,
        busy: BusyMeter::new(),
        window: 0..0,
        backlog_since: None,
    };
    sender.stats.end = sender.run();
    sender.stats
}

/// The operator commands of the phase being sent: which are due, and which of those have
/// been pushed. The pending ones are `due − pushed`, always the next ones in order, so no
/// queue is needed.
#[derive(Debug)]
struct OperatorCursor<'a> {
    phase: &'a PhaseSends,
    t0: u64,
    /// Operator commands due so far (their position has been reached).
    due: usize,
    /// Operator commands pushed into the ring so far.
    pushed: usize,
}

impl OperatorCursor<'_> {
    fn pending(&self) -> usize {
        self.due - self.pushed
    }

    /// True if item `position` of the phase is the next operator command.
    fn is_next(&self, position: usize) -> bool {
        self.phase.operators.get(self.due).is_some_and(|send| send.position == position)
    }
}

/// The sender's state while it runs.
struct Sender<'a> {
    plan: &'a SenderPlan,
    config: &'a SenderConfig,
    outputs: SenderOutputs,
    stats: SenderStats,
    busy: BusyMeter,
    /// The measured window on the run clock; empty until the timed flow starts.
    window: Range<u64>,
    /// When the current operator backlog began, if there is one.
    backlog_since: Option<u64>,
}

impl Sender<'_> {
    /// Every phase in order, with a barrier after each but the last.
    fn run(&mut self) -> SenderEnd {
        let plan = self.plan;
        for (i, phase) in plan.phases.iter().enumerate() {
            let t0 = self.config.clock.now();
            if phase.phase == FlowPhase::Timed {
                self.start_window(t0);
            }
            if let Err(end) = self.send_phase(phase, t0) {
                return end;
            }
            if i + 1 < plan.phases.len()
                && let Err(end) = self.barrier(phase.phase, phase.schedule.len())
            {
                return end;
            }
        }
        SenderEnd::Finished
    }

    /// The timed flow starts at `t0`: sets the run's phases (15.5).
    fn start_window(&mut self, t0: u64) {
        let window = &self.plan.window;
        self.window = t0.saturating_add(window.start)..t0.saturating_add(window.end);
        self.config.phases.set(t0, self.window.clone());
        self.stats.timed_start = Some(t0);
    }

    /// Sends one phase's items, each at its time, then pushes what is still pending.
    fn send_phase(&mut self, phase: &PhaseSends, t0: u64) -> Result<(), SenderEnd> {
        let stopped = Err(SenderEnd::Stopped { phase: phase.phase });
        let mut operators = OperatorCursor { phase, t0, due: 0, pushed: 0 };
        let mut client = phase.first_client;
        self.busy.begin(self.config.clock.now());
        for (position, &offset) in phase.schedule.iter().enumerate() {
            if self.config.stop.load(Ordering::Relaxed) {
                return stopped;
            }
            let t_sched = t0 + offset;
            let Some(t_sent) = self.wait_until(t_sched, &mut operators) else { return stopped };
            if operators.is_next(position) {
                operators.due += 1; // pushed below, or kept pending while the ring is full
            } else {
                self.send_client(client, t_sched, t_sent);
                client += 1;
            }
            self.push_pending(&mut operators);
        }
        while operators.pending() > 0 {
            if self.config.stop.load(Ordering::Relaxed) {
                return stopped;
            }
            self.config.idle.idle();
            self.push_pending(&mut operators);
        }
        self.busy.end(self.config.clock.now(), &self.config.counters.thread.busy_ns);
        Ok(())
    }

    /// Spins until `t_sched`, pushing pending operator items meanwhile, and returns the time
    /// it stopped waiting: `t_sent`, never before `t_sched`. Returns at once if `t_sched` has
    /// passed, and `None` if `stop` is set while it waits. Waiting is not busy time (2.2),
    /// and it costs no extra clock read: the reads that end and start the busy stretch are
    /// the ones the wait makes anyway. A late item publishes the busy time so far too, or a
    /// sender that is always late, and so never waits, would show no busy time in the
    /// window at all (15.4).
    fn wait_until(&mut self, t_sched: u64, operators: &mut OperatorCursor) -> Option<u64> {
        let clock = self.config.clock;
        let mut now = clock.now();
        if now >= t_sched {
            self.busy.end(now, &self.config.counters.thread.busy_ns);
            self.busy.begin(now);
            return Some(now);
        }
        self.busy.end(now, &self.config.counters.thread.busy_ns);
        while now < t_sched {
            if self.config.stop.load(Ordering::Relaxed) {
                return None;
            }
            self.push_pending(operators);
            self.config.idle.idle();
            now = clock.now();
        }
        self.busy.begin(now);
        Some(now)
    }

    /// Client item `index` of the arena, into its account's ring, or dropped if that ring is
    /// full.
    fn send_client(&mut self, index: usize, t_sched: u64, t_sent: u64) {
        let rings = self.outputs.clients.len();
        let (ring, sent) = match &self.plan.messages {
            Messages::Signed(arena) => {
                let message = arena.message(index);
                let g = gateway_of(message_account(message), rings);
                (g, try_write(&mut self.outputs.clients[g], &ingress_words(message, t_sched, t_sent)))
            }
            Messages::PreVerified(arena) => {
                let item = arena.item(index);
                let g = gateway_of(item.account, rings);
                let lane = u16::try_from(g).expect("at most 65,536 lanes");
                let record = ClientRecord {
                    meta: Meta { source: Source::PreVerifiedClient, lane, account: item.account },
                    nonce: item.nonce,
                    command: item.command,
                    expires_at: 0,
                    signature: [0; SIGNATURE_WORDS],
                    t_sched,
                    t_sent,
                    t_gw_in: 0,
                    t_gw_out: 0,
                };
                (g, try_write(&mut self.outputs.clients[g], &record.to_words()))
            }
        };
        self.count_client(ring, sent, t_sched, t_sent);
    }

    fn count_client(&mut self, ring: usize, sent: bool, t_sched: u64, t_sent: u64) {
        self.stats.offered += 1;
        if sent {
            self.stats.client_sent += 1;
        } else {
            self.stats.dropped[ring] += 1;
        }
        if self.window.contains(&t_sched) {
            self.stats.window_offered += 1;
            self.stats.window_dropped += u64::from(!sent);
            self.stats.lag.record(t_sent - t_sched); // `t_sent` is never before `t_sched`
        }
    }

    /// Pushes pending operator items, in order, while the operator ring has room.
    fn push_pending(&mut self, operators: &mut OperatorCursor) {
        if operators.pending() == 0 {
            return;
        }
        let clock = self.config.clock;
        while operators.pending() > 0 && self.outputs.operator.free(1) > 0 {
            let send = operators.phase.operators[operators.pushed];
            let t_sched = operators.t0 + operators.phase.schedule[send.position];
            let record = OperatorRecord { command: send.command, t_sched, t_sent: clock.now() };
            self.outputs.operator.write(&record.to_words());
            self.outputs.operator.publish();
            operators.pushed += 1;
            self.stats.operator_sent += 1;
            self.stats.offered += 1;
        }
        self.track_backlog(operators.pending());
    }

    /// The largest backlog, and how long the backlog lasted: its clock reads come only when
    /// one begins or ends.
    fn track_backlog(&mut self, pending: usize) {
        self.stats.max_operator_backlog = self.stats.max_operator_backlog.max(pending as u64);
        match (pending > 0, self.backlog_since) {
            (true, None) => self.backlog_since = Some(self.config.clock.now()),
            (false, Some(since)) => {
                let lasted = self.config.clock.now().saturating_sub(since);
                self.stats.longest_operator_backlog_ns = self.stats.longest_operator_backlog_ns.max(lasted);
                self.backlog_since = None;
            }
            _ => {}
        }
    }

    /// The barrier after `phase`, of `items` items (module docs, 14.4).
    fn barrier(&mut self, phase: FlowPhase, items: usize) -> Result<(), SenderEnd> {
        let barrier = &self.config.barrier;
        let timeout = barrier.timeout_for(items);
        let started = Instant::now();
        loop {
            let resolved = barrier.resolved_downstream() + self.stats.dropped_total();
            let offered = self.stats.offered;
            if resolved == offered {
                return Ok(());
            }
            if self.config.stop.load(Ordering::Relaxed) {
                return Err(SenderEnd::Stopped { phase });
            }
            if started.elapsed() > timeout {
                return Err(SenderEnd::BarrierTimeout { phase, resolved, offered });
            }
            std::thread::sleep(barrier.poll);
        }
    }
}

/// Writes `words` into `ring` and publishes them, unless the ring is full. Returns whether
/// it wrote them.
fn try_write<const LINES: usize>(ring: &mut Producer<LINES>, words: &[u64]) -> bool {
    if ring.free(1) == 0 {
        return false;
    }
    ring.write(words);
    ring.publish();
    true
}

/// The ingress ring's record (an `IngressSlot`, 3.3): the message's 17 words, `t_sched`,
/// `t_sent`. Built from the words directly, since the arena already holds them.
fn ingress_words(message: &[u64; MESSAGE_WORDS], t_sched: u64, t_sent: u64) -> [u64; IngressSlot::WORDS] {
    let mut words = [0; IngressSlot::WORDS];
    words[..MESSAGE_WORDS].copy_from_slice(message);
    words[MESSAGE_WORDS] = t_sched;
    words[MESSAGE_WORDS + 1] = t_sent;
    words
}

#[cfg(test)]
mod tests;
