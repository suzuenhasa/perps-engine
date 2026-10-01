//! The gate: the event ring's consumer. It releases a command's events only once the
//! command is durable, and it does all the measuring (`docs/PIPELINE.md` sections 12, 13.2
//! and 15.4 to 15.9).
//!
//! **The release rule** (12.1). An event slot of command `S` is released once `S <=
//! durable`, the journal writer's watermark. Every event is gated (acks, fills, cancels,
//! rejects, operator echoes, liquidations), so nothing anyone sees can be undone by a
//! crash. Slots are released strictly in ring order, which is the core's emission order,
//! so results come out in seq order. The gate re-reads the watermark only when it runs out
//! of durable slots, and the core never reads it: there is no lock on the core's path
//! (12.2). Each trailer's seq must be the next one: one compare that turns a skipped or
//! reordered command into an immediate stop.
//!
//! **A command is released whole** (12.1, 2.8). The core publishes a command's events
//! together with its trailer, unless the event ring fills in the middle of the command:
//! then it publishes what it has and waits for space (2.5). The gate releases a command's
//! events only once its trailer is in the ring too, so if the engine panics later in the
//! same command (which aborts the process) none of that command's events has been
//! released. The one exception is a command with more events than the ring holds: once
//! the ring is full of that command's events alone, the core can't go on until some are
//! released, so they stream through as 2.5 describes. (A ring that the core closed without
//! a trailer, which only a test does, is released as it is.) Checking costs one look at
//! the last published slot each time the gate's view of the ring moves past a command
//! whose trailer it hasn't seen yet (review finding F-PARTIAL-RELEASE).
//!
//! **What "release" means in v1** (12.1): for an event, count it, update the fund's equity,
//! append it to the capture (13.2) and to the release log (the kill test's, 18.4); for a
//! trailer, turn its stamps into the latencies of 15.4, count the command in the per-run
//! breakdown (15.9), sample the fund's equity, and publish the number of commands released
//! for main's barriers. Private feeds (Later) would read from here.
//!
//! **Latencies** (15.4). From each trailer: sender lag, ingress wait, verification,
//! sequencer wait, core path, core service, command → core result, durability wait and
//! command → durable ack. A stage is recorded only if both of its stamps apply (0 means
//! "not applicable", 3.3), and only for commands whose scheduled time `t_sched` is in the
//! measured window ([`Phases`]). Client and operator commands have separate histograms,
//! `SetMark` its own core path and core service, and each gateway its own ingress wait and
//! verification.
//! Stamps come from different CPUs, so every stage is a `saturating_sub`, and the gate
//! counts per stage how often the later stamp was the smaller one (15.2). Commands that were
//! never served are added at the end as infinitely late ([`GateStats::add_not_served`]).
//!
//! **The fund** (15.9). The gate tracks the insurance fund's equity exactly, in `i128`: its
//! balance (from `BalanceChanged` for `FUND`) plus, per market, `fund_pos × mark −
//! fund_cost` (from `PositionChanged` for `FUND` and `MarkPrice`), in O(1) per event, and
//! samples it at every trailer: starting capital, lowest equity, largest drawdown, final
//! equity. The final equity must equal the engine snapshot's `fund_balance +
//! fund_upnl_total`.
//!
//! **Capture** (13.2). Every released engine event (not trailers) as its 8-word slot, into
//! memory reserved and touched before the run. If the reservation fills, capture stops and
//! is marked incomplete; it never grows mid-run. [`write_events_file`] writes it out after
//! the run as `events.bin`, with a header naming the commands it covers ([`EventsHeader`]).
//!
//! **Complexity.** Per slot: one ring read and one decode; per trailer, about a dozen
//! histogram records. No allocation after start, except the fund's per-market table when a
//! market is first seen (in setup).

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::engine::FUND;
use engine::event::Event;
use engine::types::{MarketId, Micros, Price, Qty};

use crate::clock::RunClock;
use crate::codec::{EVENT_WORDS, cancel_reason_code, command_tags, decode_event, reject_reason_code};
use crate::counters::{BusyMeter, LiveCounts, PipelineCounters, Watermark};
use crate::histogram::LatencyHistogram;
use crate::idle::IdleStrategy;
use crate::records::{
    EVENT_SLOT_WORDS, Source, Stamps, TRAILER_TAG, Trailer, is_trailer, outcome_reject_reason,
};
use crate::ring::{CachePadded, Consumer};

/// Slots the gate releases per pass, at most (12.2).
pub const GATE_BATCH: usize = 1_024;
/// Command tags index arrays by tag: 1 to 9, and 0 unused.
const TAGS: usize = 10;
/// Reject reasons (codes 0 to 18, 4.1).
pub const REJECT_REASONS: usize = 19;
/// Cancel reasons (codes 0 to 5, 4.1).
pub const CANCEL_REASONS: usize = 6;

// ---------------------------------------------------------------------------------------
// Phases.

/// Which part of a run a command belongs to, by its scheduled send time `t_sched` (15.5,
/// 15.9): setup (before the timed flow), the measured window, or neither (warm-up and
/// drain). Main sets it before the timed flow starts, and the gate reads it once per pass.
#[derive(Debug)]
pub struct Phases(CachePadded<[AtomicU64; 3]>);

/// Where one command belongs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Setup,
    Window,
    Other,
}

/// [`Phases`] as read at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PhaseBounds {
    timed_start: u64,
    window: (u64, u64),
}

impl PhaseBounds {
    fn classify(self, t_sched: u64) -> Phase {
        if t_sched < self.timed_start {
            Phase::Setup
        } else if (self.window.0..self.window.1).contains(&t_sched) {
            Phase::Window
        } else {
            Phase::Other
        }
    }
}

impl Phases {
    /// Commands scheduled before `timed_start` are setup; those in `window` are measured.
    pub fn new(timed_start: u64, window: Range<u64>) -> Phases {
        let words = [AtomicU64::new(timed_start), AtomicU64::new(window.start), AtomicU64::new(window.end)];
        Phases(CachePadded::new(words))
    }

    /// No setup, and every command measured: for tests and runs without phases.
    pub fn everything() -> Phases {
        Phases::new(0, 0..u64::MAX)
    }

    /// Nothing measured yet, everything setup: for a run whose timed flow's start isn't
    /// known when the pipeline starts ([`Phases::set`] later).
    pub fn not_yet() -> Phases {
        Phases::new(u64::MAX, u64::MAX..u64::MAX)
    }

    /// Main sets the phases, before the sender sends the first timed command. The gate sees
    /// the change through the rings: the command's trailer reaches it after the store.
    pub fn set(&self, timed_start: u64, window: Range<u64>) {
        self.0[0].store(timed_start, Ordering::Release);
        self.0[1].store(window.start, Ordering::Release);
        self.0[2].store(window.end, Ordering::Release);
    }

    /// Where a command scheduled at `t_sched` belongs, as the phases are now. The gateways
    /// use it to count the rejects of the measured window (7.3), which the gate adds as not
    /// served (15.4).
    pub fn phase_of(&self, t_sched: u64) -> Phase {
        self.load().classify(t_sched)
    }

    /// When the timed flow started and the measured window, once they are set; for main's
    /// display (`e2e run --watch`).
    pub fn timed(&self) -> Option<(u64, Range<u64>)> {
        let bounds = self.load();
        (bounds.timed_start != u64::MAX).then_some((bounds.timed_start, bounds.window.0..bounds.window.1))
    }

    fn load(&self) -> PhaseBounds {
        PhaseBounds {
            timed_start: self.0[0].load(Ordering::Acquire),
            window: (self.0[1].load(Ordering::Acquire), self.0[2].load(Ordering::Acquire)),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Stages.

/// The stages of 15.4, in report order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// `t_sent − t_sched`
    SenderLag,
    /// `t_gw_in − t_sent` (signed only)
    IngressWait,
    /// `t_gw_out − t_gw_in`: the gateway's service, mostly the signature (signed only)
    Verification,
    /// `t_seq −` (`t_gw_out` if signed, else `t_sent`)
    SequencerWait,
    /// `t_done − t_seq`: INFO.md 5's core path
    CorePath,
    /// `t_done − max(t_seq, previous command's t_done)`
    CoreService,
    /// `t_done − t_sched`: command → core result
    ToCoreResult,
    /// `t_release − t_done`
    DurabilityWait,
    /// `t_release − t_sched`: signed order (or pre-verified command) → durable ack
    ToDurableAck,
}

/// Stages, and so histograms per class of command.
pub const STAGES: usize = 9;

impl Stage {
    pub const ALL: [Stage; STAGES] = [
        Stage::SenderLag,
        Stage::IngressWait,
        Stage::Verification,
        Stage::SequencerWait,
        Stage::CorePath,
        Stage::CoreService,
        Stage::ToCoreResult,
        Stage::DurabilityWait,
        Stage::ToDurableAck,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Stage::SenderLag => "sender lag",
            Stage::IngressWait => "ingress wait",
            Stage::Verification => "signature verification",
            Stage::SequencerWait => "sequencer wait",
            Stage::CorePath => "core path",
            Stage::CoreService => "core service",
            Stage::ToCoreResult => "command -> core result",
            Stage::DurabilityWait => "durability wait",
            Stage::ToDurableAck => "command -> durable ack",
        }
    }

    /// The stage's two stamps, earlier then later, if both apply.
    fn stamps(self, t: &Timeline) -> Option<(u64, u64)> {
        let (earlier, later) = match self {
            Stage::SenderLag => (t.sched, t.sent),
            Stage::IngressWait => (t.sent, t.gw_in),
            Stage::Verification => (t.gw_in, t.gw_out),
            Stage::SequencerWait => (if t.gw_out != 0 { t.gw_out } else { t.sent }, t.seq),
            Stage::CorePath => (t.seq, t.done),
            Stage::CoreService => (t.seq.max(t.previous_done), t.done),
            Stage::ToCoreResult => (t.sched, t.done),
            Stage::DurabilityWait => (t.done, t.release),
            Stage::ToDurableAck => (t.sched, t.release),
        };
        (earlier != 0 && later != 0).then_some((earlier, later))
    }
}

/// One command's stamps, from its trailer and the gate's release time.
#[derive(Clone, Copy, Debug)]
struct Timeline {
    sched: u64,
    sent: u64,
    gw_in: u64,
    gw_out: u64,
    seq: u64,
    done: u64,
    release: u64,
    /// The previous command's `t_done`: when the core could start this one.
    previous_done: u64,
}

/// One histogram per stage.
#[derive(Clone, Debug)]
pub struct StageHistograms([LatencyHistogram; STAGES]);

impl StageHistograms {
    pub fn new() -> StageHistograms {
        StageHistograms(std::array::from_fn(|_| LatencyHistogram::new()))
    }

    pub fn get(&self, stage: Stage) -> &LatencyHistogram {
        &self.0[stage as usize]
    }

    fn get_mut(&mut self, stage: Stage) -> &mut LatencyHistogram {
        &mut self.0[stage as usize]
    }
}

impl Default for StageHistograms {
    fn default() -> Self {
        StageHistograms::new()
    }
}

/// The two stages measured per gateway (15.4): the queue in front of it, and its service.
#[derive(Clone, Debug, Default)]
pub struct GatewayHistograms {
    pub ingress_wait: LatencyHistogram,
    pub verification: LatencyHistogram,
}

// ---------------------------------------------------------------------------------------
// The per-run breakdown (15.9).

/// Counts of released engine events, for one command or many.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventCounts {
    pub events: u64,
    pub fills: u64,
    pub fill_lots: i128,
    /// Sum of `price × qty` over fills, in micro-dollars.
    pub fill_notional: i128,
    /// By cancel reason code (4.1).
    pub cancels: [u64; CANCEL_REASONS],
    pub modifies: u64,
    pub marks: u64,
    pub liquidations: u64,
    pub insurance_absorbs: u64,
    pub shortfall_reports: u64,
    /// The largest `InsuranceShortfall.uncovered` (the peak shortfall).
    pub peak_shortfall: Micros,
}

impl EventCounts {
    fn count(&mut self, event: &Event) {
        self.events += 1;
        match event {
            Event::Fill(fill) => {
                self.fills += 1;
                self.fill_lots += i128::from(fill.qty);
                self.fill_notional += i128::from(fill.price) * i128::from(fill.qty);
            }
            Event::Cancelled(cancelled) => {
                self.cancels[usize::from(cancel_reason_code(cancelled.reason))] += 1
            }
            Event::Modified(_) => self.modifies += 1,
            Event::MarkPrice(_) => self.marks += 1,
            Event::Liquidation(_) => self.liquidations += 1,
            Event::InsuranceAbsorb(_) => self.insurance_absorbs += 1,
            Event::InsuranceShortfall(shortfall) => {
                self.shortfall_reports += 1;
                self.peak_shortfall = self.peak_shortfall.max(shortfall.uncovered);
            }
            _ => {}
        }
    }

    fn add(&mut self, other: &EventCounts) {
        self.events += other.events;
        self.fills += other.fills;
        self.fill_lots += other.fill_lots;
        self.fill_notional += other.fill_notional;
        for (mine, theirs) in self.cancels.iter_mut().zip(other.cancels) {
            *mine += theirs;
        }
        self.modifies += other.modifies;
        self.marks += other.marks;
        self.liquidations += other.liquidations;
        self.insurance_absorbs += other.insurance_absorbs;
        self.shortfall_reports += other.shortfall_reports;
        self.peak_shortfall = self.peak_shortfall.max(other.peak_shortfall);
    }
}

/// Commands and their results over one phase of a run (15.9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breakdown {
    /// Commands by CMD40 tag (index 1 to 9).
    pub commands: [u64; TAGS],
    /// Accepted commands by tag.
    pub accepted: [u64; TAGS],
    /// Rejected commands by tag and reject reason code.
    pub rejected: [[u64; REJECT_REASONS]; TAGS],
    /// The events of these commands.
    pub events: EventCounts,
    /// The most events one of these commands emitted.
    pub max_events_per_command: u64,
}

impl Default for Breakdown {
    fn default() -> Self {
        Breakdown {
            commands: [0; TAGS],
            accepted: [0; TAGS],
            rejected: [[0; REJECT_REASONS]; TAGS],
            events: EventCounts::default(),
            max_events_per_command: 0,
        }
    }
}

impl Breakdown {
    fn count(&mut self, trailer: &Trailer, events: &EventCounts) {
        let tag = usize::from(trailer.command_tag);
        self.commands[tag] += 1;
        match outcome_reject_reason(trailer.outcome) {
            None => self.accepted[tag] += 1,
            Some(reason) => self.rejected[tag][usize::from(reject_reason_code(reason))] += 1,
        }
        self.events.add(events);
        self.max_events_per_command = self.max_events_per_command.max(events.events);
    }

    /// Client commands (places, cancels, modifies).
    pub fn client_commands(&self) -> u64 {
        CLIENT_TAGS.iter().map(|&tag| self.commands[usize::from(tag)]).sum()
    }

    /// Client commands the engine rejected: their share is flagged above 5% (15.9).
    pub fn client_rejects(&self) -> u64 {
        CLIENT_TAGS.iter().map(|&tag| self.rejected[usize::from(tag)].iter().sum::<u64>()).sum()
    }
}

const CLIENT_TAGS: [u8; 3] =
    [command_tags::PLACE_ORDER, command_tags::CANCEL_ORDER, command_tags::MODIFY_ORDER];

// ---------------------------------------------------------------------------------------
// The fund (15.9).

/// The fund's position and cost in one market, and the market's mark.
#[derive(Clone, Copy, Debug, Default)]
struct FundMarket {
    position: Qty,
    cost: Micros,
    mark: Price,
    /// `position × mark − cost`.
    upnl: i128,
}

/// The insurance fund's equity, exactly, from the event stream (module docs).
#[derive(Clone, Debug, Default)]
struct FundTracker {
    balance: i128,
    /// Indexed by market id; grows when a market is first seen.
    markets: Vec<FundMarket>,
    upnl_total: i128,
    /// Equity at the previous trailer.
    last_sample: i128,
    window: FundStats,
}

/// What the gate saw of the fund's equity, in micro-dollars (15.9).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FundStats {
    /// Equity when the measured window opened (just before its first command).
    pub start: Option<i128>,
    /// The highest and lowest equity at any trailer in the window.
    pub peak: Option<i128>,
    pub lowest: Option<i128>,
    /// The largest fall from an earlier peak, in the window.
    pub max_drawdown: i128,
    /// Equity after the last event released.
    pub final_equity: i128,
}

impl FundTracker {
    fn equity(&self) -> i128 {
        self.balance + self.upnl_total
    }

    fn market(&mut self, market: MarketId) -> &mut FundMarket {
        let index = market.index();
        if self.markets.len() <= index {
            self.markets.resize(index + 1, FundMarket::default());
        }
        &mut self.markets[index]
    }

    /// O(1): only the fund's own balance and positions, and marks, change its equity.
    fn on_event(&mut self, event: &Event) {
        match event {
            Event::BalanceChanged(balance) if balance.account == FUND => {
                self.balance = i128::from(balance.free)
            }
            Event::PositionChanged(p) if p.account == FUND => {
                self.revalue(p.market, |market| (market.position, market.cost) = (p.position, p.cost_basis));
            }
            Event::MarkPrice(mark) => self.revalue(mark.market, |market| market.mark = mark.price),
            _ => {}
        }
    }

    /// Applies `change` to one market and moves the running total by its new PnL.
    fn revalue(&mut self, market: MarketId, change: impl FnOnce(&mut FundMarket)) {
        let entry = self.market(market);
        let old = entry.upnl;
        change(entry);
        entry.upnl = i128::from(entry.position) * i128::from(entry.mark) - i128::from(entry.cost);
        let new = entry.upnl;
        self.upnl_total += new - old;
    }

    /// At a command's trailer: the equity now, counted in the window's figures if the
    /// command is in the window.
    fn sample(&mut self, in_window: bool) {
        let equity = self.equity();
        if in_window {
            let window = &mut self.window;
            let start = *window.start.get_or_insert(self.last_sample);
            let peak = window.peak.map_or(start.max(equity), |peak| peak.max(equity));
            window.peak = Some(peak);
            window.lowest = Some(window.lowest.map_or(equity, |lowest| lowest.min(equity)));
            window.max_drawdown = window.max_drawdown.max(peak - equity);
        }
        self.last_sample = equity;
    }

    fn stats(&self) -> FundStats {
        FundStats { final_equity: self.equity(), ..self.window }
    }
}

// ---------------------------------------------------------------------------------------
// The gate.

/// The gate's settings.
#[derive(Debug)]
pub struct GateConfig {
    /// Lanes (gateways), for the per-gateway histograms.
    pub lanes: usize,
    /// The seq of the first trailer: 1, or the recovered journal's next seq (13.5).
    pub next_seq: u64,
    pub stamps: Stamps,
    /// Event slots to reserve for the capture (13.2); `None`: no capture.
    pub capture: Option<usize>,
    /// Where to write every released event slot, for the kill test (18.4).
    pub release_log: Option<File>,
    /// Keep running totals of the released events for `e2e run --watch`
    /// ([`PipelineCounters::live`]).
    pub live: bool,
    pub clock: RunClock,
}

/// Everything the gate measured (15.4, 15.9). Returned when its thread is joined.
#[derive(Clone, Debug)]
pub struct GateStats {
    /// Client commands in the window.
    pub client: StageHistograms,
    /// Operator commands in the window.
    pub operator: StageHistograms,
    /// `SetMark`'s core path, in the window.
    pub set_mark_core_path: LatencyHistogram,
    /// `SetMark`'s core service, in the window: the core's own time on a mark, queueing
    /// left out, which a mark that liquidates or sweeps many orders makes long (D-034's
    /// shocks).
    pub set_mark_core_service: LatencyHistogram,
    /// Per gateway: signed client commands in the window.
    pub per_gateway: Vec<GatewayHistograms>,
    /// Per stage (in [`Stage::ALL`] order), over every command: how often the later stamp
    /// was the smaller one. A run with any is flagged (15.2).
    pub inversions: [u64; STAGES],
    pub setup: Breakdown,
    pub window: Breakdown,
    pub fund: FundStats,
    /// Commands (trailers) and engine events released, over the whole run.
    pub commands: u64,
    pub events: u64,
    pub max_events_per_command: u64,
    /// Event slots captured, and whether the reservation ran out (13.2).
    pub captured: u64,
    pub capture_incomplete: bool,
}

impl GateStats {
    fn new(lanes: usize) -> GateStats {
        GateStats {
            client: StageHistograms::new(),
            operator: StageHistograms::new(),
            set_mark_core_path: LatencyHistogram::new(),
            set_mark_core_service: LatencyHistogram::new(),
            per_gateway: (0..lanes).map(|_| GatewayHistograms::default()).collect(),
            inversions: [0; STAGES],
            setup: Breakdown::default(),
            window: Breakdown::default(),
            fund: FundStats::default(),
            commands: 0,
            events: 0,
            max_events_per_command: 0,
            captured: 0,
            capture_incomplete: false,
        }
    }

    /// Records `count` client commands offered in the window but never served (dropped at
    /// ingress or rejected by a gateway) as infinitely late in the two end-to-end
    /// histograms, so that their percentiles are over offered commands (15.4). The
    /// harness calls this after the run, with the sender's and the gateways' window counts.
    pub fn add_not_served(&mut self, count: u64) {
        for _ in 0..count {
            self.client.get_mut(Stage::ToCoreResult).record(u64::MAX);
            self.client.get_mut(Stage::ToDurableAck).record(u64::MAX);
        }
    }
}

/// The capture's memory (13.2).
#[derive(Debug)]
struct Capture {
    slots: Vec<u64>,
    incomplete: bool,
}

impl Capture {
    /// Reserves room for `slots` event slots, and touches every page of it now.
    fn reserve(slots: usize) -> Capture {
        let words = slots * EVENT_SLOT_WORDS;
        // `vec!` may hand out untouched zero pages; one store per 4 KiB page, of a value
        // the compiler can't see, maps every page now, so none is left for a first-touch
        // fault during the run (as `LatencyHistogram::new` does).
        let mut memory = vec![0; words];
        for word in memory.iter_mut().step_by(512) {
            *word = std::hint::black_box(1);
        }
        memory.clear();
        Capture { slots: memory, incomplete: false }
    }

    fn push(&mut self, slot: &[u64; EVENT_SLOT_WORDS]) {
        if self.incomplete || self.slots.len() + EVENT_SLOT_WORDS > self.slots.capacity() {
            self.incomplete = true; // it never grows mid-run, and never resumes
            return;
        }
        self.slots.extend_from_slice(slot);
    }
}

/// The gate as a plain value: [`Gate::pass`] is one pass of the loop of 12.2, so tests
/// drive it with a watermark they control; [`run_gate`] is the thread.
#[derive(Debug)]
pub struct Gate {
    events: Consumer<1>,
    /// The seq of the next trailer.
    expected: u64,
    stamped: bool,
    clock: RunClock,
    stats: GateStats,
    /// The events of the command whose trailer hasn't come yet.
    pending: EventCounts,
    /// Every command up to this seq may have its events released: its trailer is in the
    /// ring, or it has more events than the ring holds (module docs, "A command is
    /// released whole").
    releasable_through: u64,
    fund: FundTracker,
    previous_done: u64,
    capture: Option<Capture>,
    release_log: Option<BufWriter<File>>,
    /// The released events so far, if the run watches them (`GateConfig::live`).
    live: Option<EventCounts>,
    busy: BusyMeter,
}

impl Gate {
    pub fn new(events: Consumer<1>, config: GateConfig) -> Gate {
        Gate {
            events,
            expected: config.next_seq,
            stamped: config.stamps == Stamps::On,
            clock: config.clock,
            stats: GateStats::new(config.lanes),
            pending: EventCounts::default(),
            releasable_through: config.next_seq - 1,
            fund: FundTracker::default(),
            previous_done: 0,
            capture: config.capture.map(Capture::reserve),
            release_log: config.release_log.map(BufWriter::new),
            live: config.live.then(EventCounts::default),
            busy: BusyMeter::new(),
        }
    }

    /// One pass (12.2): releases, in ring order, up to 1,024 slots whose command is
    /// durable and whole (module docs), and stops at the first that isn't. Returns how
    /// many it released.
    pub fn pass(&mut self, durable: &Watermark, phases: &Phases, counters: &PipelineCounters) -> usize {
        let watermark = durable.load();
        let bounds = phases.load();
        let mut released = 0;
        while released < GATE_BATCH && self.events.available(1) > 0 {
            let seq = self.events.peek(0);
            if seq > watermark {
                break; // the next slot's command isn't durable yet
            }
            if seq > self.releasable_through && !self.is_whole(seq, released == 0) {
                break; // the rest of the command, and its trailer, aren't in the ring yet
            }
            if released == 0 {
                self.busy.begin(self.clock.now());
            }
            let mut slot = [0; EVENT_SLOT_WORDS];
            self.events.read(&mut slot);
            if is_trailer(&slot) {
                self.release_command(&Trailer::from_words(&slot), bounds);
                counters.released.store(self.stats.commands);
            } else {
                self.release_event(&slot);
            }
            released += 1;
        }
        self.events.release();
        if released > 0 {
            if let Some(live) = &self.live {
                publish_live(live, &counters.live);
            }
            self.busy.end(self.clock.now(), &counters.gate.busy_ns);
        }
        released
    }

    /// True if command `seq`, whose first unreleased slot is next, may be released
    /// (module docs, "A command is released whole"): its trailer has been published, or
    /// the ring holds nothing but its events and is full (`at_pass_start`: no slot has been
    /// read and not yet freed in this pass), or the core has closed the ring. Moves
    /// `releasable_through` on.
    fn is_whole(&mut self, seq: u64, at_pass_start: bool) -> bool {
        let seen = self.events.available(1);
        self.releasable_through = self.releasable_through.max(self.whole_before(seen));
        if self.releasable_through >= seq {
            return true;
        }
        // Not in the gate's view of the ring: look at the ring as it is now. `closed` is
        // read before `tail`, so a closed ring's last record is seen (3.2).
        let closed = self.events.is_closed();
        let capacity = self.events.capacity();
        let now = self.events.available(capacity);
        self.releasable_through = self.releasable_through.max(self.whole_before(now));
        if self.releasable_through >= seq {
            return true;
        }
        if closed || (at_pass_start && now == capacity) {
            self.releasable_through = seq; // streamed: it is bigger than the ring
            return true;
        }
        false
    }

    /// The highest seq whose trailer is among the first `n` unread slots, read from the
    /// last of them: a trailer ends its own command, and an event of command `L` means
    /// that the core has written the trailer of every command before `L`. With `n` = 0,
    /// `releasable_through` as it is.
    fn whole_before(&self, n: usize) -> u64 {
        if n == 0 {
            return self.releasable_through;
        }
        let last_seq = self.events.peek_at(n - 1, 0);
        let last_is_trailer = self.events.peek_at(n - 1, 1) as u8 == TRAILER_TAG;
        if last_is_trailer { last_seq } else { last_seq.saturating_sub(1) }
    }

    /// True once the core has closed the event ring and every slot was released.
    pub fn is_finished(&mut self) -> bool {
        self.events.is_finished()
    }

    /// Writes out what the release log holds so far.
    pub fn flush_release_log(&mut self) {
        if let Some(log) = &mut self.release_log {
            log.flush().unwrap_or_else(|e| panic!("writing the release log: {e}"));
        }
    }

    /// The statistics and the capture, once the gate is done.
    pub fn finish(mut self) -> (GateStats, Option<Vec<u64>>) {
        self.flush_release_log();
        let mut stats = self.stats;
        stats.fund = self.fund.stats();
        if let Some(capture) = &self.capture {
            stats.captured = (capture.slots.len() / EVENT_SLOT_WORDS) as u64;
            stats.capture_incomplete = capture.incomplete;
        }
        (stats, self.capture.map(|capture| capture.slots))
    }

    /// An engine event: count it, track the fund, capture it and log it.
    fn release_event(&mut self, slot: &[u64; EVENT_SLOT_WORDS]) {
        let words: &[u64; EVENT_WORDS] = slot[1..].try_into().expect("7 words");
        let event = decode_event(words)
            .unwrap_or_else(|e| panic!("seq {}: an event that doesn't decode ({e})", slot[0]));
        self.pending.count(&event);
        self.fund.on_event(&event);
        self.stats.events += 1;
        if let Some(capture) = &mut self.capture {
            capture.push(slot);
        }
        if let Some(log) = &mut self.release_log {
            for word in slot {
                log.write_all(&word.to_le_bytes()).unwrap_or_else(|e| panic!("writing the release log: {e}"));
            }
        }
    }

    /// A trailer: the command is complete and released.
    fn release_command(&mut self, trailer: &Trailer, bounds: PhaseBounds) {
        assert_eq!(
            trailer.seq, self.expected,
            "the gate got the trailer of seq {} but expected {}",
            trailer.seq, self.expected
        );
        self.expected += 1;
        let phase = bounds.classify(trailer.t_sched);
        if self.stamped {
            self.record_latencies(trailer, phase == Phase::Window);
        }
        match phase {
            Phase::Setup => self.stats.setup.count(trailer, &self.pending),
            Phase::Window => self.stats.window.count(trailer, &self.pending),
            Phase::Other => {}
        }
        self.fund.sample(phase == Phase::Window);
        if let Some(live) = &mut self.live {
            live.add(&self.pending);
        }
        self.stats.commands += 1;
        self.stats.max_events_per_command = self.stats.max_events_per_command.max(u64::from(trailer.events));
        self.pending = EventCounts::default();
    }

    /// The stages of one command (module docs, "Latencies").
    fn record_latencies(&mut self, trailer: &Trailer, in_window: bool) {
        let timeline = Timeline {
            sched: trailer.t_sched,
            sent: trailer.t_sent,
            gw_in: trailer.t_gw_in,
            gw_out: trailer.t_gw_out,
            seq: trailer.t_seq,
            done: trailer.t_done,
            release: self.clock.now(),
            previous_done: self.previous_done,
        };
        self.previous_done = trailer.t_done;
        let operator = trailer.source == Source::Operator;
        for stage in Stage::ALL {
            let Some((earlier, later)) = stage.stamps(&timeline) else { continue };
            if later < earlier {
                self.stats.inversions[stage as usize] += 1;
            }
            if !in_window {
                continue;
            }
            let latency = later.saturating_sub(earlier);
            let class = if operator { &mut self.stats.operator } else { &mut self.stats.client };
            class.get_mut(stage).record(latency);
            if trailer.source == Source::SignedClient {
                let gateway = &mut self.stats.per_gateway[usize::from(trailer.lane)];
                match stage {
                    Stage::IngressWait => gateway.ingress_wait.record(latency),
                    Stage::Verification => gateway.verification.record(latency),
                    _ => {}
                }
            }
            if trailer.command_tag == command_tags::SET_MARK {
                match stage {
                    Stage::CorePath => self.stats.set_mark_core_path.record(latency),
                    Stage::CoreService => self.stats.set_mark_core_service.record(latency),
                    _ => {}
                }
            }
        }
    }
}

/// The gate's thread loop (12.2): passes until the event ring is closed and every slot is
/// released. Returns the statistics and the capture.
pub fn run_gate(
    mut gate: Gate,
    durable: &Watermark,
    phases: &Phases,
    counters: &PipelineCounters,
    idle: IdleStrategy,
) -> (GateStats, Option<Vec<u64>>) {
    loop {
        if gate.pass(durable, phases, counters) == 0 {
            gate.flush_release_log();
            if gate.is_finished() {
                break;
            }
            idle.idle();
        }
    }
    gate.finish()
}

/// Stores the running totals for `e2e run --watch` (`GateConfig::live`). Notional above
/// `u64::MAX` micro-dollars (about $18 trillion) reads as `u64::MAX`.
fn publish_live(totals: &EventCounts, live: &LiveCounts) {
    live.events.store(totals.events);
    live.fills.store(totals.fills);
    live.fill_notional.store(u64::try_from(totals.fill_notional).unwrap_or(u64::MAX));
    live.cancels.store(totals.cancels.iter().sum());
    live.modifies.store(totals.modifies);
    live.marks.store(totals.marks);
    live.liquidations.store(totals.liquidations);
}

// ---------------------------------------------------------------------------------------
// events.bin (13.2).

/// The first 8 bytes of an events file.
pub const EVENTS_MAGIC: [u8; 8] = *b"PERPEVT1";
/// The events file's version: 2 since the header names the commands the file covers.
/// Version 1 had the first and last *slot's* seq there, 0 in an empty file, so a reader
/// couldn't tell where a life that released no event started.
pub const EVENTS_VERSION: u16 = 2;
/// Bytes in an events file's header.
pub const EVENTS_HEADER_BYTES: usize = 64;

/// What an events file says about its slots (13.2): the commands they are the events of.
/// A live run's file holds one life's events (13.5), a replay's the whole journal's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventsHeader {
    pub deployment: u32,
    /// The seq of the first command covered: the life's first (1 on a new journal).
    pub first_seq: u64,
    /// The seq of the last command covered; `first_seq - 1` if the life issued none.
    pub last_seq: u64,
    /// The capture ran out of room (13.2): the slots are only the first of those events.
    pub incomplete: bool,
}

/// Writes captured event slots to `path` (13.2): a 64-byte header (magic `PERPEVT1`, `u16`
/// version 2, `u16` flags (bit 0: incomplete), `u32` deployment, `u64` event count, `u64`
/// first seq, `u64` last seq, zeros), then the slots, 64 bytes each, little-endian.
pub fn write_events_file(path: &Path, header: &EventsHeader, slots: &[u64]) -> io::Result<()> {
    let count = (slots.len() / EVENT_SLOT_WORDS) as u64;
    let mut bytes = [0u8; EVENTS_HEADER_BYTES];
    bytes[0..8].copy_from_slice(&EVENTS_MAGIC);
    bytes[8..10].copy_from_slice(&EVENTS_VERSION.to_le_bytes());
    bytes[10..12].copy_from_slice(&u16::from(header.incomplete).to_le_bytes());
    bytes[12..16].copy_from_slice(&header.deployment.to_le_bytes());
    bytes[16..24].copy_from_slice(&count.to_le_bytes());
    bytes[24..32].copy_from_slice(&header.first_seq.to_le_bytes());
    bytes[32..40].copy_from_slice(&header.last_seq.to_le_bytes());
    let mut out = BufWriter::new(File::create(path)?);
    out.write_all(&bytes)?;
    for word in slots {
        out.write_all(&word.to_le_bytes())?;
    }
    out.flush()
}

/// Reads an events file back: its header and its slots. Refuses another version.
pub fn read_events_file(path: &Path) -> io::Result<(EventsHeader, Vec<u64>)> {
    let bytes = std::fs::read(path)?;
    let invalid =
        |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {what}", path.display()));
    if bytes.len() < EVENTS_HEADER_BYTES || bytes[0..8] != EVENTS_MAGIC {
        return Err(invalid("not an events file"));
    }
    let u16_at = |at: usize| u16::from_le_bytes(bytes[at..at + 2].try_into().expect("2 bytes"));
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
    let version = u16_at(8);
    if version != EVENTS_VERSION {
        return Err(invalid(&format!("events file version {version}, this binary reads {EVENTS_VERSION}")));
    }
    let header = EventsHeader {
        deployment: u32::from_le_bytes(bytes[12..16].try_into().expect("4 bytes")),
        first_seq: u64_at(24),
        last_seq: u64_at(32),
        incomplete: u16_at(10) & 1 == 1,
    };
    let count = u64_at(16);
    let body = &bytes[EVENTS_HEADER_BYTES..];
    if body.len() as u64 != count * 64 {
        return Err(invalid("the event count doesn't match the file's length"));
    }
    let slots = body.as_chunks::<8>().0.iter().map(|chunk| u64::from_le_bytes(*chunk)).collect();
    Ok((header, slots))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::{Trailer, event_slot};
    use crate::ring::{Producer, channel};
    use engine::event::{
        Ack, BalanceChanged, CancelReason, Cancelled, Fill, InsuranceShortfall, MarkPrice, PositionChanged,
        Reject, RejectReason,
    };
    use engine::types::{AccountId, OrderId, Side};

    fn gate(capture: Option<usize>, lanes: usize) -> (Producer<1>, Gate) {
        let (producer, consumer) = channel::<1>(64);
        let config = GateConfig {
            lanes,
            next_seq: 1,
            stamps: Stamps::On,
            capture,
            release_log: None,
            live: false,
            clock: RunClock::start(),
        };
        (producer, Gate::new(consumer, config))
    }

    fn trailer(seq: u64, source: Source, tag: u8, outcome: u8, events: u16) -> Trailer {
        Trailer {
            seq,
            source,
            command_tag: tag,
            outcome,
            lane: 1,
            events,
            t_sched: 100,
            t_sent: 110,
            t_gw_in: if source == Source::SignedClient { 130 } else { 0 },
            t_gw_out: if source == Source::SignedClient { 50_130 } else { 0 },
            t_seq: 50_200,
            t_done: 50_700,
        }
    }

    fn ack(seq: u64) -> [u64; 8] {
        event_slot(seq, &Event::Ack(Ack { order_id: OrderId::new(seq) }))
    }

    fn publish(producer: &mut Producer<1>, slots: &[[u64; 8]]) {
        for slot in slots {
            producer.write(slot);
        }
        producer.publish();
    }

    #[test]
    fn nothing_is_released_above_the_watermark_and_everything_at_or_below_it_is() {
        let (mut producer, mut gate) = gate(Some(16), 2);
        let place = |seq| trailer(seq, Source::PreVerifiedClient, 1, 0, 1).to_words();
        publish(&mut producer, &[ack(1), place(1), ack(2), place(2), ack(3), place(3)]);
        let (durable, phases, counters) = (Watermark::new(0), Phases::everything(), PipelineCounters::new());
        assert_eq!(gate.pass(&durable, &phases, &counters), 0, "nothing is durable");
        assert_eq!(counters.released.load(), 0);
        durable.publish(1);
        assert_eq!(gate.pass(&durable, &phases, &counters), 2, "seq 1's event and trailer, and no more");
        assert_eq!(counters.released.load(), 1);
        assert_eq!(gate.pass(&durable, &phases, &counters), 0, "seq 2 is not durable yet");
        durable.publish(3);
        assert_eq!(gate.pass(&durable, &phases, &counters), 4);
        assert_eq!(counters.released.load(), 3);
        assert!(!gate.is_finished(), "the core hasn't closed the ring");
        drop(producer);
        assert!(gate.is_finished());
        let (stats, capture) = gate.finish();
        assert_eq!((stats.commands, stats.events, stats.captured), (3, 3, 3));
        let capture = capture.expect("capture was on");
        assert_eq!(capture, [ack(1), ack(2), ack(3)].concat(), "events only, no trailers");
    }

    #[test]
    fn a_command_is_released_only_once_its_trailer_is_in_the_ring() {
        let (mut producer, mut gate) = gate(Some(16), 1);
        let (durable, phases, counters) = (Watermark::new(1), Phases::everything(), PipelineCounters::new());
        // The core published two of seq 1's events and is still applying it.
        publish(&mut producer, &[ack(1), ack(1)]);
        assert_eq!(gate.pass(&durable, &phases, &counters), 0, "durable, but not whole yet");
        publish(&mut producer, &[trailer(1, Source::Operator, 7, 0, 2).to_words(), ack(2)]);
        assert_eq!(gate.pass(&durable, &phases, &counters), 3, "whole now; seq 2 isn't durable");
    }

    #[test]
    fn a_command_with_more_events_than_the_ring_holds_streams_through() {
        let (mut producer, consumer) = channel::<1>(4);
        let config = GateConfig {
            lanes: 1,
            next_seq: 1,
            stamps: Stamps::On,
            capture: None,
            release_log: None,
            live: false,
            clock: RunClock::start(),
        };
        let mut gate = Gate::new(consumer, config);
        let (durable, phases, counters) = (Watermark::new(1), Phases::everything(), PipelineCounters::new());
        publish(&mut producer, &[ack(1), ack(1), ack(1)]);
        assert_eq!(gate.pass(&durable, &phases, &counters), 0, "room left: the core goes on");
        publish(&mut producer, &[ack(1)]);
        assert_eq!(gate.pass(&durable, &phases, &counters), 4, "full of seq 1 alone: it streams");
        publish(&mut producer, &[ack(1), trailer(1, Source::Operator, 7, 0, 5).to_words()]);
        assert_eq!(gate.pass(&durable, &phases, &counters), 2);
        assert_eq!(counters.released.load(), 1);
    }

    #[test]
    fn a_ring_closed_in_the_middle_of_a_command_is_released_as_it_is() {
        let (mut producer, mut gate) = gate(None, 1);
        publish(&mut producer, &[ack(1), ack(1)]);
        drop(producer);
        let (durable, phases, counters) = (Watermark::new(1), Phases::everything(), PipelineCounters::new());
        assert_eq!(gate.pass(&durable, &phases, &counters), 2, "nothing more will come");
        assert!(gate.is_finished());
    }

    #[test]
    #[should_panic(expected = "the gate got the trailer of seq 2 but expected 1")]
    fn a_skipped_command_stops_the_gate() {
        let (mut producer, mut gate) = gate(None, 1);
        publish(&mut producer, &[trailer(2, Source::Operator, 7, 0, 0).to_words()]);
        gate.pass(&Watermark::new(5), &Phases::everything(), &PipelineCounters::new());
    }

    #[test]
    fn each_stage_is_recorded_from_its_two_stamps() {
        let (mut producer, mut gate) = gate(None, 2);
        publish(&mut producer, &[ack(1), trailer(1, Source::SignedClient, 1, 0, 1).to_words()]);
        let counters = PipelineCounters::new();
        gate.pass(&Watermark::new(1), &Phases::everything(), &counters);
        let (stats, _) = gate.finish();
        let exact = |stage| stats.client.get(stage).max();
        assert_eq!(exact(Stage::SenderLag), Some(10));
        assert_eq!(exact(Stage::IngressWait), Some(20));
        assert_eq!(exact(Stage::Verification), Some(50_000));
        assert_eq!(exact(Stage::SequencerWait), Some(70), "t_seq - t_gw_out in signed mode");
        assert_eq!(exact(Stage::CorePath), Some(500));
        assert_eq!(exact(Stage::CoreService), Some(500), "the core was idle before it");
        assert_eq!(exact(Stage::ToCoreResult), Some(50_600));
        assert_eq!(stats.client.get(Stage::DurabilityWait).count(), 1);
        assert_eq!(stats.client.get(Stage::ToDurableAck).count(), 1);
        assert_eq!(stats.per_gateway[1].verification.max(), Some(50_000));
        assert_eq!(stats.per_gateway[0].verification.count(), 0);
        assert_eq!(stats.operator.get(Stage::CorePath).count(), 0);
        assert_eq!(stats.inversions, [0; STAGES]);
    }

    #[test]
    fn stages_whose_stamps_do_not_apply_are_skipped_and_inversions_are_counted() {
        let (mut producer, mut gate) = gate(None, 1);
        let first = trailer(1, Source::PreVerifiedClient, 2, 0, 0);
        // The second starts before the first is done (it queued), and its t_sent is below
        // its t_sched: an inversion.
        let second = Trailer { seq: 2, t_sched: 200, t_sent: 150, t_seq: 50_300, t_done: 50_900, ..first };
        let mark = Trailer {
            seq: 3,
            source: Source::Operator,
            command_tag: 7,
            t_seq: 51_000,
            t_done: 51_400,
            ..first
        };
        publish(&mut producer, &[first.to_words(), second.to_words(), mark.to_words()]);
        gate.pass(&Watermark::new(3), &Phases::everything(), &PipelineCounters::new());
        let (stats, _) = gate.finish();
        assert_eq!(stats.client.get(Stage::IngressWait).count(), 0, "no gateway in pre-verified mode");
        assert_eq!(stats.client.get(Stage::SequencerWait).min(), Some(50_090), "t_seq - t_sent");
        assert_eq!(stats.client.get(Stage::CoreService).min(), Some(200), "t_done - the first's t_done");
        assert_eq!(stats.inversions[Stage::SenderLag as usize], 1);
        assert_eq!(stats.client.get(Stage::SenderLag).min(), Some(0), "saturated, not wrapped");
        assert_eq!(stats.operator.get(Stage::CorePath).count(), 1);
        assert_eq!(stats.set_mark_core_path.count(), 1);
        // The mark's core service: from its t_seq (51,000, after the second's t_done, 50,900)
        // to its t_done (51,400).
        assert_eq!(stats.set_mark_core_service.max(), Some(400));
    }

    #[test]
    fn only_commands_scheduled_in_the_window_are_measured_and_setup_is_counted_apart() {
        let (mut producer, mut gate) = gate(None, 1);
        let at = |seq, t_sched| Trailer { seq, t_sched, ..trailer(seq, Source::PreVerifiedClient, 1, 0, 0) };
        publish(
            &mut producer,
            &[at(1, 10).to_words(), at(2, 100).to_words(), at(3, 200).to_words(), at(4, 300).to_words()],
        );
        let phases = Phases::new(50, 100..300);
        gate.pass(&Watermark::new(4), &phases, &PipelineCounters::new());
        let (stats, _) = gate.finish();
        assert_eq!(stats.setup.commands[1], 1, "t_sched 10 is setup");
        assert_eq!(stats.window.commands[1], 2, "t_sched 100 and 200");
        assert_eq!(stats.client.get(Stage::CorePath).count(), 2);
        assert_eq!(stats.commands, 4);
    }

    #[test]
    fn the_breakdown_counts_outcomes_and_results() {
        let (mut producer, mut gate) = gate(None, 1);
        let fill = Event::Fill(Fill {
            maker_order: OrderId::new(1),
            taker_order: OrderId::new(2),
            price: Price::new(1_000),
            qty: Qty::new(7),
            maker_fee: Micros::ZERO,
            taker_fee: Micros::ZERO,
            market: MarketId::new(1),
            taker_side: Side::Buy,
        });
        let cancelled = Event::Cancelled(Cancelled {
            order_id: OrderId::new(2),
            remaining: Qty::new(3),
            market: MarketId::new(1),
            reason: CancelReason::IocRemainder,
            side: Side::Buy,
        });
        let reject = Event::Reject(Reject {
            order_id: OrderId::new(3),
            account: AccountId::new(1),
            reason: RejectReason::InsufficientMargin,
        });
        let outcome = crate::records::outcome(&reject);
        publish(
            &mut producer,
            &[
                event_slot(1, &Event::Ack(Ack { order_id: OrderId::new(2) })),
                event_slot(1, &fill),
                event_slot(1, &cancelled),
                trailer(1, Source::PreVerifiedClient, 1, 0, 3).to_words(),
                event_slot(2, &reject),
                trailer(2, Source::PreVerifiedClient, 1, outcome, 1).to_words(),
            ],
        );
        gate.pass(&Watermark::new(2), &Phases::everything(), &PipelineCounters::new());
        let (stats, _) = gate.finish();
        let window = &stats.window;
        assert_eq!((window.commands[1], window.accepted[1]), (2, 1));
        assert_eq!(window.rejected[1][9], 1, "InsufficientMargin is code 9");
        assert_eq!((window.client_commands(), window.client_rejects()), (2, 1));
        assert_eq!(
            (window.events.fills, window.events.fill_lots, window.events.fill_notional),
            (1, 7, 7_000)
        );
        assert_eq!(window.events.cancels[1], 1, "IocRemainder");
        assert_eq!((window.max_events_per_command, stats.max_events_per_command), (3, 3));
    }

    #[test]
    fn the_fund_equity_follows_its_balance_positions_and_marks() {
        let mut fund = FundTracker::default();
        let balance = |free| Event::BalanceChanged(BalanceChanged { free: Micros::new(free), account: FUND });
        let position = |lots, cost_basis| {
            Event::PositionChanged(PositionChanged {
                position: Qty::new(lots),
                cost_basis: Micros::new(cost_basis),
                locked: Micros::ZERO,
                account: FUND,
                market: MarketId::new(3),
            })
        };
        let mark = |ticks| Event::MarkPrice(MarkPrice { price: Price::new(ticks), market: MarketId::new(3) });
        fund.on_event(&balance(1_000));
        fund.sample(false); // setup: equity 1,000 before the window
        fund.on_event(&mark(100));
        fund.on_event(&position(10, 900)); // upnl 10 × 100 − 900 = 100
        assert_eq!(fund.equity(), 1_100);
        fund.sample(true);
        fund.on_event(&mark(50)); // upnl 500 − 900 = −400
        fund.sample(true);
        fund.on_event(&mark(80)); // upnl −100
        fund.on_event(&Event::BalanceChanged(BalanceChanged {
            free: Micros::new(5),
            account: AccountId::new(7),
        })); // not the fund
        fund.sample(true);
        let stats = fund.stats();
        assert_eq!(stats.start, Some(1_000), "the equity when the window opened");
        assert_eq!((stats.peak, stats.lowest), (Some(1_100), Some(600)));
        assert_eq!(stats.max_drawdown, 500);
        assert_eq!(stats.final_equity, 900);
    }

    #[test]
    fn a_full_capture_stops_and_is_marked_incomplete() {
        let (mut producer, mut gate) = gate(Some(2), 1);
        let slots = [ack(1), ack(1), ack(1), trailer(1, Source::Operator, 4, 0, 3).to_words()];
        publish(&mut producer, &slots);
        gate.pass(&Watermark::new(1), &Phases::everything(), &PipelineCounters::new());
        let (stats, capture) = gate.finish();
        assert!(stats.capture_incomplete);
        assert_eq!((stats.captured, capture.expect("on").len()), (2, 16));
    }

    #[test]
    fn not_served_commands_count_as_infinitely_late() {
        let mut stats = GateStats::new(0);
        stats.client.get_mut(Stage::ToDurableAck).record(1_000);
        stats.add_not_served(2);
        let histogram = stats.client.get(Stage::ToDurableAck);
        assert_eq!(histogram.count(), 3);
        assert_eq!(histogram.percentile(99, 100), Some(u64::MAX));
        assert_eq!(histogram.percentile(1, 3), Some(1_003), "the upper bound of 1,000's bucket");
    }

    #[test]
    fn an_events_file_round_trips() {
        let path = std::env::temp_dir().join(format!("pipeline-gate-events-{}.bin", std::process::id()));
        // Commands 3 to 10, of which only 4 and 9 released an event.
        let header = EventsHeader { deployment: 7, first_seq: 3, last_seq: 10, incomplete: false };
        let slots = [ack(4), ack(9)].concat();
        write_events_file(&path, &header, &slots).expect("written");
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(&bytes[..8], b"PERPEVT1");
        assert_eq!(u16::from_le_bytes(bytes[8..10].try_into().expect("2")), 2, "version");
        assert_eq!(bytes.len(), 64 + 128);
        assert_eq!(u64::from_le_bytes(bytes[24..32].try_into().expect("8")), 3, "first seq");
        assert_eq!(u64::from_le_bytes(bytes[32..40].try_into().expect("8")), 10, "last seq");
        assert_eq!(read_events_file(&path).expect("read back"), (header, slots));

        // A life that issued no command still says where it started; an incomplete
        // capture says so.
        let empty = EventsHeader { deployment: 7, first_seq: 11, last_seq: 10, incomplete: true };
        write_events_file(&path, &empty, &[]).expect("written");
        assert_eq!(std::fs::read(&path).expect("read")[10], 1, "the flags");
        assert_eq!(read_events_file(&path).expect("read back"), (empty, Vec::new()));

        // Version 1 put the slots' seqs where version 2 puts the commands': refused.
        let mut old = std::fs::read(&path).expect("read");
        old[8] = 1;
        std::fs::write(&path, old).expect("written");
        let refused = read_events_file(&path).expect_err("version 1");
        assert!(refused.to_string().contains("version 1"), "{refused}");
        std::fs::remove_file(&path).expect("removed");
    }

    #[test]
    fn the_reason_counts_match_the_codec() {
        assert!(crate::codec::reject_reason_from_code(REJECT_REASONS as u8 - 1).is_some());
        assert!(crate::codec::reject_reason_from_code(REJECT_REASONS as u8).is_none());
        assert!(crate::codec::cancel_reason_from_code(CANCEL_REASONS as u8 - 1).is_some());
        assert!(crate::codec::cancel_reason_from_code(CANCEL_REASONS as u8).is_none());
        let shortfall = Event::InsuranceShortfall(InsuranceShortfall { uncovered: Micros::new(42) });
        let mut counts = EventCounts::default();
        counts.count(&shortfall);
        assert_eq!((counts.shortfall_reports, counts.peak_shortfall), (1, Micros::new(42)));
    }
}
