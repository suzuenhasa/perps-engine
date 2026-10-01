//! One run, end to end: the harness's wiring of every thread and ring, the phases, the
//! window's edges, and the checks after the run (`docs/PIPELINE.md` 19.2, 2.8, 14.4, 15.4,
//! 15.5, 13.3, 13.4 and 13.5).
//!
//! **Contract** ([`run`]).
//! 1. **Before any pipeline thread starts:** the workload is built (the flow plan and its
//!    messages, signed on every CPU, 14.8); the CPU layout is worked out and checked
//!    against the machine and its CPU quota (2.6); the clock is checked (15.1); the
//!    throttling counters are read (2.6); in signed mode `keys.txt` is written and loaded
//!    back, its keys parsed for the run's verifier (5.7), and its digest goes into the
//!    journal's headers (5.4, 11.2), with the run's signing scheme (5.8).
//! 2. **A restart** (`resume`, 13.5): recovery, then replay, give the engine, the nonce
//!    table, the next seq and the last `ts`; the clock is anchored above that `ts`, the
//!    gateways start with the nonce table, and the sender sends the rest of the flow
//!    (`workload::rest_of_plan`).
//! 3. **Wiring** (19.2), on the run's own main thread, which pins itself first so that the
//!    rings and the engine's memory are first touched on the pipeline's CPU package (2.6):
//!    the lanes and the operator ring, `Pipeline::start` (with the phases not yet known:
//!    the sender sets them when the timed flow starts), one gateway thread per lane in
//!    signed mode, then the sender. One `RunClock` for all of them. In the EIP-712 scheme
//!    each gateway gets its replay table here too, allocated and pre-touched on main (5.8,
//!    `gateway::salts`), with room for every message the sender will offer it
//!    ([`requests_per_gateway`]): it can accept no more, and a run under 5 minutes expires
//!    none, so the table never fills.
//! 4. **Main's loop** (`Watcher`, 2.2): every millisecond it looks where the run is by
//!    those phases, samples every hot thread's counters when the window opens and when it
//!    closes (15.4), and, for the "fsync per order" ablation, stops the sender if the
//!    backlog hasn't drained within the cap after the window (16). It never touches a ring.
//! 5. **Stopping follows the data** (2.8): when the sender is done it drops its ring ends;
//!    main joins the sender, then the gateways, then the pipeline, whose threads stop by
//!    themselves in data-flow order. Then it reads the throttling counters again, and
//!    checks the drain cap once more: a sender that finished its flow before the cap leaves
//!    its backlog in the rings, and the run still fails if that took longer than the cap.
//! 6. **After the run:** the commands never served in the window are added to the
//!    end-to-end histograms as infinitely late (15.4); with capture on, the events are
//!    written to `events.bin` and the replay test runs (13.3); in signed mode with the
//!    audit on, every journaled signature is checked again (13.4, in the scheme the
//!    journal's header names); the journal is deleted unless it is to be kept (11.6). A
//!    kept journal comes with the live engine's snapshot, `snapshot-live.txt`, so `e2e
//!    replay` can compare its own rebuild with the live run's state as well as with
//!    `events.bin`.
//!
//! What the run measured, and whether it counts, is `results.rs`'s.
//!
//! **Why the run has its own main thread.** Main pins itself to one CPU (2.6), and threads
//! inherit their creator's CPU mask: the next run's signing pool, spawned from a pinned
//! thread, would all sign on that one CPU.
//!
//! **Complexity.** The run's own duration, plus building the workload (signing dominates)
//! and the replay test (one pass over the journal per replay).

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gateway::audit::verify_journal;
use gateway::core_verifier::RegistryVerifier;
use gateway::salts::{SaltTable, random_seed};
use gateway::thread::{INGRESS_CAPACITY, LANE_CAPACITY};
use gateway::{Gateway, GatewayCounters, GatewayStats, KeyRegistry, ThreadConfig, gateway_of};
use loadgen::market_flow::PlanConfig;
use loadgen::presign::message_account;
use loadgen::sender::{
    Barrier, Messages, PhaseSends, SETUP_RATE, SenderConfig, SenderCounters, SenderOutputs, SenderPlan,
    SenderStats, Timing, spawn_sender,
};
use pipeline::affinity::{
    CpuLayout, CpuQuota, LayoutRequest, Role, Throttling, Topology, default_layout, pin_current_thread,
    spinning_threads,
};
use pipeline::clock::RunClock;
use pipeline::gate::{EventsHeader, Phase, Phases, write_events_file};
use pipeline::journal::JournalError;
use pipeline::journal::files::{StdFiles, create_dir_all_durable};
use pipeline::journal::format::DEFAULT_SEGMENT_BYTES;
use pipeline::journal::recovery::{recover, scan_journal};
use pipeline::journal::writer::{JournalConfig, segments_needed};
use pipeline::records::{AuthScheme, InjectionMode, Source};
use pipeline::replay::{LiveRun, NonceTable, replay, replay_test};
use pipeline::ring::{Consumer, Producer, channel};
use pipeline::run::{Pipeline, PipelineConfig, Resume, RingCapacities, VerifyOnCore};
use pipeline::sequencer::Inputs;

use super::checks::{Edge, Sample, Watched, WindowHealth};
use super::config::{Cpus, RunConfig, VerifyArm};
use super::probes::{ClockCheck, Mount, cpu_mhz, file_system_of};
use super::results::{Completed, FlowContent, RunResult, Throttled};
use super::units::short_rate;
use super::workload::{Workload, Workloads, rest_of_plan};

/// Operator ring slots (2.3).
pub const OPERATOR_CAPACITY: usize = 4_096;
/// The live run's released events, in a run directory (13.2).
pub const EVENTS_FILE: &str = "events.bin";
/// The live engine's snapshot (its `Debug` text), next to a kept journal (module docs).
pub const LIVE_SNAPSHOT_FILE: &str = "snapshot-live.txt";
/// Event slots reserved for the capture, per expected command (13.2).
pub const CAPTURE_SLOTS_PER_COMMAND: usize = 6;
/// How often main looks at the run (2.2).
const POLL: Duration = Duration::from_millis(1);

/// What a test can hook into while a run goes: the smoke test counts allocations on the
/// hot threads during the timed flow (18.4).
pub trait RunObserver: Send {
    /// Main noticed that the timed flow started (within a millisecond).
    fn timed_flow_started(&mut self) {}
}

/// No hooks.
#[derive(Debug, Default)]
pub struct Quiet;

impl RunObserver for Quiet {}

/// Why a run didn't happen, or didn't finish its checks.
#[derive(Debug)]
pub enum RunError {
    /// The configuration, the machine or the directory doesn't allow the run.
    Refused(String),
    Journal(JournalError),
    Io(io::Error),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Refused(why) => write!(f, "refused: {why}"),
            RunError::Journal(error) => write!(f, "journal: {error}"),
            RunError::Io(error) => write!(f, "I/O: {error}"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<JournalError> for RunError {
    fn from(error: JournalError) -> Self {
        RunError::Journal(error)
    }
}

impl From<io::Error> for RunError {
    fn from(error: io::Error) -> Self {
        RunError::Io(error)
    }
}

/// Runs `config` in directory `dir` (module docs). `workloads` keeps the workload for the
/// next run; `quietest_first` is the jitter probe's ranking of physical cores, if a probe
/// ran (2.6).
pub fn run(
    config: &RunConfig,
    workloads: &mut Workloads,
    dir: &Path,
    quietest_first: &[usize],
    observer: &mut dyn RunObserver,
) -> Result<RunResult, RunError> {
    config.check().map_err(RunError::Refused)?;
    // Every directory above the journal made durable too (11.1; review finding F-DIR-FSYNC).
    create_dir_all_durable(dir)?;
    let journal_dir = dir.join("journal");
    let restart = if config.resume { Some(Restart::recover(config, &journal_dir)?) } else { None };
    // A restarted life sends only the rest of the flow, with its own messages.
    let rest = restart.as_ref().map(|restart| {
        let plan = config.flow.generate(config.timed_clients());
        let rest = rest_of_plan(&plan, restart.operator_done, &restart.nonces);
        Workload::from_plan(rest, config, workloads.sign_ns)
    });
    let (workload, timed_clients) = match &rest {
        Some(rest) => (rest, rest.timed_clients()),
        None => (workloads.get(config).map_err(RunError::Refused)?, config.timed_clients()),
    };
    let setup = Setup {
        config,
        workload,
        timed_clients,
        layout: layout(config, quietest_first)?,
        dir,
        journal_dir: &journal_dir,
        clock_check: ClockCheck::measure(1_000_000),
        mount: file_system_of(dir).ok(),
        throttling_before: Throttling::read()?,
    };
    // The run's own main thread (module docs).
    let mut result = std::thread::scope(|scope| {
        let main = scope.spawn(move || setup.run(restart, observer));
        main.join().expect("a panic aborts the process before this")
    })?;
    finish(&mut result, &journal_dir)?;
    Ok(result)
}

/// What a restart takes from the journal (13.5).
struct Restart {
    resume: Resume,
    nonces: NonceTable,
    /// Operator records in the journal: the plan's first ones (`workload::rest_of_plan`).
    operator_done: u64,
}

impl Restart {
    /// Recovery, replay, and a count of the journal's operator records (module docs).
    fn recover(config: &RunConfig, journal_dir: &Path) -> Result<Restart, RunError> {
        let recovered = recover(journal_dir, &config.identity(), config.allow_engine_change)?;
        for warning in recovered.build_warnings() {
            eprintln!("recovery: warning: {warning}");
        }
        for copy in &recovered.torn_copies {
            eprintln!("recovery: the torn tail was copied to {copy}");
        }
        eprintln!(
            "recovery: {} records, next seq {}, the journal ends at {}",
            recovered.records, recovered.next_seq, recovered.end
        );
        let replayed = replay(journal_dir, &recovered, None, false)?;
        let mut files = StdFiles::open_existing(journal_dir, DEFAULT_SEGMENT_BYTES)?;
        let mut operator_done = 0;
        scan_journal(&mut files, |_, record, _| {
            operator_done += u64::from(record.meta.source == Source::Operator)
        })?;
        let resume = Resume {
            engine: replayed.engine,
            next_seq: replayed.next_seq,
            last_ts: replayed.last_ts,
            end: recovered.end,
        };
        Ok(Restart { resume, nonces: replayed.nonces, operator_done })
    }
}

/// The default layout for this machine with the run's overrides, checked against the
/// machine and its CPU quota (2.6); or no pinning at all.
fn layout(config: &RunConfig, quietest_first: &[usize]) -> Result<CpuLayout, RunError> {
    let Cpus::Pinned { gateway_smt, overrides } = &config.cpus else { return Ok(CpuLayout::unpinned()) };
    let topology = Topology::read()?;
    let request = LayoutRequest {
        mode: config.mode,
        gateways: config.lanes,
        gateway_smt: *gateway_smt,
        quietest_first: quietest_first.to_vec(),
    };
    // A machine without a default layout needs `--cpus` for every role.
    let mut layout = match default_layout(&topology, &request) {
        Ok(layout) => layout,
        Err(_) if !overrides.is_empty() => CpuLayout::unpinned(),
        Err(why) => return Err(RunError::Refused(why)),
    };
    for arg in overrides {
        layout.apply_override(arg).map_err(RunError::Refused)?;
    }
    layout.check(&topology).map_err(RunError::Refused)?;
    CpuQuota::read()?.check(spinning_threads(config.mode, config.lanes, 1)).map_err(RunError::Refused)?;
    Ok(layout)
}

/// Everything the run's main thread starts from.
struct Setup<'a> {
    config: &'a RunConfig,
    workload: &'a Workload,
    timed_clients: usize,
    layout: CpuLayout,
    dir: &'a Path,
    journal_dir: &'a Path,
    clock_check: ClockCheck,
    mount: Option<Mount>,
    throttling_before: Option<Throttling>,
}

/// The rings the harness creates (19.2): the producers go to the sender, the consumers to
/// the gateways (signed) or the sequencer.
struct Rings {
    sender_clients: Vec<Producer<3>>,
    operator: Producer<1>,
    operator_end: Consumer<1>,
    /// Signed mode: each gateway's ingress consumer and lane producer.
    gateway_ends: Vec<(Consumer<3>, Producer<3>)>,
    lane_ends: Vec<Consumer<3>>,
}

impl Rings {
    fn new(mode: InjectionMode, lanes: usize) -> Rings {
        let (operator, operator_end) = channel::<1>(OPERATOR_CAPACITY);
        let (lane_producers, lane_ends): (Vec<_>, Vec<_>) =
            (0..lanes).map(|_| channel::<3>(LANE_CAPACITY)).unzip();
        match mode {
            InjectionMode::Signed => {
                let (ingress, ingress_ends): (Vec<_>, Vec<_>) =
                    (0..lanes).map(|_| channel::<3>(INGRESS_CAPACITY)).unzip();
                let gateway_ends = ingress_ends.into_iter().zip(lane_producers).collect();
                Rings { sender_clients: ingress, operator, operator_end, gateway_ends, lane_ends }
            }
            InjectionMode::PreVerified => {
                let gateway_ends = Vec::new();
                Rings { sender_clients: lane_producers, operator, operator_end, gateway_ends, lane_ends }
            }
        }
    }
}

impl Setup<'_> {
    /// Steps 3 to 5 of the module docs, on the run's main thread.
    fn run(self, restart: Option<Restart>, observer: &mut dyn RunObserver) -> Result<RunResult, RunError> {
        let config = self.config;
        if let Some(cpu) = self.layout.cpu(Role::Main) {
            pin_current_thread(cpu)?;
        }
        let registry = self.registry()?;
        let first_seq = restart.as_ref().map_or(1, |r| r.resume.next_seq);
        let (clock, resume, nonces) = match restart {
            Some(r) => (RunClock::resume(r.resume.last_ts), Some(r.resume), r.nonces),
            None => (RunClock::start(), None, NonceTable::new()),
        };
        let sender_plan =
            SenderPlan::new(&self.workload.plan, self.workload.messages.clone(), &self.timing());
        let content = FlowContent::of(&self.workload.plan, &sender_plan, config.lanes);
        let commands: usize = sender_plan.phases.iter().map(|phase| phase.schedule.len()).sum();
        // The EIP-712 gateways' replay tables are sized from the plan (module docs, step 3).
        let requests = match config.auth {
            AuthScheme::Perp => Vec::new(),
            AuthScheme::Eip712 => requests_per_gateway(&sender_plan, config.lanes),
        };

        let rings = Rings::new(config.mode, config.lanes);
        let pipeline_config = self.pipeline_config(commands, registry.as_ref())?;
        let inputs = Inputs { lanes: rings.lane_ends, operator: rings.operator_end };
        let pipeline = Pipeline::start(pipeline_config, inputs, clock, resume)?;
        let started = Instant::now(); // after preallocation, which `start` does first
        let (gateways, gateway_counters) =
            self.spawn_gateways(rings.gateway_ends, registry.as_ref(), &nonces, &requests, clock, &pipeline);
        let watched = Watched {
            pipeline: pipeline.shared_counters(),
            gateways: gateway_counters.clone(),
            sender: Arc::new(SenderCounters::default()),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let sender = spawn_sender(
            sender_plan,
            SenderOutputs { clients: rings.sender_clients, operator: rings.operator },
            SenderConfig {
                clock,
                idle: config.idle,
                cpu: self.layout.cpu(Role::Sender),
                barrier: Barrier {
                    per_item: Duration::from_nanos(config.barrier_per_item_ns.unwrap_or(0)),
                    ..Barrier::new(pipeline.shared_counters(), gateway_counters)
                },
                stop: Arc::clone(&stop),
                phases: pipeline.shared_phases(),
                counters: Arc::clone(&watched.sender),
            },
        );
        eprintln!(
            "run {}: started: setup, then the timed flow at {}/s",
            config.name(),
            short_rate(config.rate)
        );
        let watcher =
            Watcher { phases: pipeline.shared_phases(), clock, watched, config, stop, layout: &self.layout };
        let mut monitor = watcher.watch(&sender, observer);

        // Stopping follows the data (2.8): the sender, then the gateways, then the pipeline.
        let sender = sender.join().expect("a panic aborts the process before this");
        let gateways: Vec<GatewayStats> =
            gateways.into_iter().map(|g| g.join().expect("a panic aborts the process before this")).collect();
        let mut output = pipeline.join();
        let duration_ns = started.elapsed().as_nanos() as u64;
        let throttled = throttled(self.throttling_before, Throttling::read()?);
        monitor.check_drained(config.drain_cap_ns);
        eprintln!("run {}: every thread stopped after {:.1} s", config.name(), duration_ns as f64 / 1e9);

        let completed = Completed::before_not_served(&output);
        let not_served =
            sender.window_dropped + gateways.iter().map(|g| g.window_rejects.total()).sum::<u64>();
        output.stats.add_not_served(not_served);
        let snapshot = output.engine.snapshot();
        Ok(RunResult {
            config: config.clone(),
            dir: self.dir.to_path_buf(),
            first_seq,
            layout: self.layout,
            clock: self.clock_check,
            mount: self.mount,
            throttled,
            sender,
            gateways,
            output,
            completed,
            monitor,
            content,
            duration_ns,
            snapshot,
            registry,
            replay: None,
            audit: None,
        })
    }

    /// The send schedule's settings (14.9, 15.5).
    fn timing(&self) -> Timing {
        let config = self.config;
        Timing {
            arrivals: config.arrivals,
            setup_rate: SETUP_RATE,
            rate: config.rate,
            timed_clients: self.timed_clients,
            warmup_ns: config.warmup_ns,
            window_ns: config.window_ns,
        }
    }

    /// Signed mode: writes `keys.txt` and loads it back for the run's verifier, as a real
    /// start would (5.4, 5.7).
    fn registry(&self) -> Result<Option<KeyRegistry>, RunError> {
        if self.config.mode != InjectionMode::Signed {
            return Ok(None);
        }
        let flow = &self.config.flow;
        let path = self.dir.join("keys.txt");
        loadgen::keys::write_registry(&path, flow.seed(), self.config.deployment, &flow.client_accounts())?;
        let registry = KeyRegistry::load(&path, self.config.deployment, self.config.verifier)
            .map_err(|e| RunError::Refused(format!("{}: {e}", path.display())))?;
        Ok(Some(registry))
    }

    /// The pipeline's configuration (19.2) for a life that sequences about `commands`.
    fn pipeline_config(
        &self,
        commands: usize,
        registry: Option<&KeyRegistry>,
    ) -> Result<PipelineConfig, RunError> {
        let config = self.config;
        let settings = config.journal;
        let journal = JournalConfig {
            dir: self.journal_dir.to_path_buf(),
            segment_bytes: settings.segment_bytes,
            commit_interval_ns: settings.commit_interval_ns,
            max_batch: settings.max_batch,
            mode: settings.mode,
            preallocate: segments_needed(commands as u64, config.record_bytes(), settings.segment_bytes),
        };
        let ablation = match (config.verify_on_core, registry) {
            (None, _) => None,
            (Some(VerifyArm::Gateways), _) => Some(VerifyOnCore::Gateways),
            (Some(VerifyArm::Core), Some(registry)) => {
                Some(VerifyOnCore::Core(Box::new(RegistryVerifier::new(registry))))
            }
            (Some(VerifyArm::Core), None) => {
                return Err(RunError::Refused("verify on core needs keys".into()));
            }
        };
        Ok(PipelineConfig {
            deployment: config.deployment,
            mode: config.mode,
            auth: config.auth,
            engine: config.engine_options(),
            lanes: config.lanes,
            capacities: RingCapacities::default(),
            journal,
            registry_digest: registry.map_or([0; 32], KeyRegistry::digest),
            layout: self.layout.clone(),
            idle: config.idle,
            capture: config.capture.then_some(commands * CAPTURE_SLOTS_PER_COMMAND),
            release_log: if config.release_log { Some(stdout_file()?) } else { None },
            stamps: config.stamps,
            phases: Phases::not_yet(),
            ablation_verify_on_core: ablation,
        })
    }

    /// Signed mode: one gateway thread per lane (19.2), in the run's signing scheme: with
    /// the nonce table (6.3), or with a new replay table with room for `requests[g]`
    /// requests (5.8; module docs, step 3).
    fn spawn_gateways(
        &self,
        ends: Vec<(Consumer<3>, Producer<3>)>,
        registry: Option<&KeyRegistry>,
        nonces: &NonceTable,
        requests: &[usize],
        clock: RunClock,
        pipeline: &Pipeline,
    ) -> (Vec<JoinHandle<GatewayStats>>, Vec<Arc<GatewayCounters>>) {
        let config = self.config;
        let (mut handles, mut counters) = (Vec::new(), Vec::new());
        let Some(registry) = registry else { return (handles, counters) };
        let (lanes, deployment, anchor) = (config.lanes, config.deployment, clock.start_unix_ns());
        for (g, (ingress, lane)) in ends.into_iter().enumerate() {
            let mut gateway = match config.auth {
                AuthScheme::Perp => Gateway::new(g, lanes, deployment, registry, nonces, anchor),
                AuthScheme::Eip712 => {
                    // Allocated and pre-touched here, on main, before the gateway thread starts.
                    let salts = SaltTable::new(requests[g], random_seed());
                    Gateway::new_eip712(g, lanes, deployment, registry, salts, anchor)
                }
            };
            if config.verify_on_core == Some(VerifyArm::Core) {
                gateway = gateway.without_signature_checks(); // insecure: the core verifies (16)
            }
            let shared = Arc::new(GatewayCounters::default());
            let thread = ThreadConfig {
                clock,
                idle: config.idle,
                cpu: self.layout.cpu(Role::Gateway(g)),
                phases: pipeline.shared_phases(),
                counters: Arc::clone(&shared),
            };
            handles.push(gateway::spawn(gateway, ingress, lane, thread));
            counters.push(shared);
        }
        (handles, counters)
    }
}

/// The signed messages the sender will offer each of `lanes` gateways in this life (module
/// docs, step 3): every client item of its plan, routed by account (`account mod N`), as
/// the sender routes it. Zeros for a pre-verified run, which has no gateways.
pub fn requests_per_gateway(plan: &SenderPlan, lanes: usize) -> Vec<usize> {
    let mut requests = vec![0; lanes];
    if let Messages::Signed(arena) = &plan.messages {
        // The phases' client items are the arena's first ones, in order.
        let clients: usize = plan.phases.iter().map(PhaseSends::clients).sum();
        for index in 0..clients {
            requests[gateway_of(message_account(arena.message(index)), lanes)] += 1;
        }
    }
    requests
}

/// Standard output as a `File`, for the release log (18.4): a duplicate of descriptor 1.
fn stdout_file() -> io::Result<File> {
    Ok(File::from(io::stdout().as_fd().try_clone_to_owned()?))
}

/// How much the kernel throttled the container between two readings (2.6).
fn throttled(before: Option<Throttling>, after: Option<Throttling>) -> Throttled {
    match (before, after) {
        (Some(before), Some(after)) => Throttled {
            periods: after.nr_throttled.saturating_sub(before.nr_throttled),
            us: after.throttled_us.saturating_sub(before.throttled_us),
        },
        _ => Throttled::default(),
    }
}

/// What main saw while the run went (module docs, step 4).
#[derive(Clone, Debug, Default)]
pub struct Monitor {
    /// The samples at the window's two edges, if it opened.
    pub open: Option<Sample>,
    pub close: Option<Sample>,
    /// The window was still open when the sender finished: the flow didn't cover it.
    pub closed_early: bool,
    /// The backlog didn't drain within the cap after the window (16).
    pub drain_cap_exceeded: bool,
    /// When main saw the window close.
    pub window_closed_at: Option<Instant>,
    /// The core's CPU frequency at the window's close, in MHz (15.4).
    pub core_mhz: Option<u64>,
}

impl Monitor {
    /// The hot threads over the window, if it opened.
    pub fn health(&self) -> Option<WindowHealth> {
        Some(WindowHealth::between(self.open.as_ref()?, self.close.as_ref()?))
    }

    /// Once every thread has stopped: flags a run whose backlog took longer than `cap` after
    /// the window closed to drain (16; module docs, step 5).
    fn check_drained(&mut self, cap: Option<u64>) {
        if let (Some(cap), Some(closed)) = (cap, self.window_closed_at)
            && closed.elapsed() > Duration::from_nanos(cap)
        {
            self.drain_cap_exceeded = true;
        }
    }
}

/// Main's view of a run (module docs, step 4).
struct Watcher<'a> {
    phases: Arc<Phases>,
    clock: RunClock,
    watched: Watched,
    config: &'a RunConfig,
    /// Stops the sender early (the drain cap).
    stop: Arc<AtomicBool>,
    layout: &'a CpuLayout,
}

impl Watcher<'_> {
    /// Looks at the run every millisecond until the sender has finished.
    fn watch(self, sender: &JoinHandle<SenderStats>, observer: &mut dyn RunObserver) -> Monitor {
        let name = self.config.name();
        let mut monitor = Monitor::default();
        let mut timed_started = false;
        while !sender.is_finished() {
            std::thread::sleep(POLL);
            let now = self.clock.now();
            let phase = self.phases.phase_of(now);
            if !timed_started && phase != Phase::Setup {
                timed_started = true;
                observer.timed_flow_started();
            }
            if monitor.open.is_none() && phase == Phase::Window {
                monitor.open = Some(self.watched.sample(now, Edge::Open));
                eprintln!("run {name}: the measured window opened");
            }
            if monitor.open.is_some() && monitor.close.is_none() && phase != Phase::Window {
                monitor.close = Some(self.watched.sample(now, Edge::Close));
                monitor.core_mhz = self.layout.cpu(Role::Core).and_then(cpu_mhz);
                monitor.window_closed_at = Some(Instant::now());
                eprintln!("run {name}: the measured window closed");
            }
            if let (Some(cap), Some(closed)) = (self.config.drain_cap_ns, monitor.window_closed_at)
                && closed.elapsed() > Duration::from_nanos(cap)
                && !self.stop.swap(true, Ordering::Relaxed)
            {
                monitor.drain_cap_exceeded = true;
                eprintln!(
                    "run {name}: the backlog hadn't drained {cap} ns after the window: the sender stops"
                );
            }
        }
        if monitor.open.is_some() && monitor.close.is_none() {
            // Some threads may have ended already: their faults then read as unknown.
            monitor.close = Some(self.watched.sample(self.clock.now(), Edge::Close));
            monitor.closed_early = true;
        }
        monitor
    }
}

/// Step 6 of the module docs: `events.bin`, the replay test, the audit, and the journal's
/// deletion or the live snapshot beside it.
fn finish(result: &mut RunResult, journal_dir: &Path) -> Result<(), RunError> {
    let config = &result.config;
    let name = config.name();
    if let Some(capture) = &result.output.capture {
        // The header names this life's commands, so a reader knows where they start even
        // when the life released no event (13.2).
        let header = EventsHeader {
            deployment: config.deployment,
            first_seq: result.first_seq,
            last_seq: result.first_seq + result.output.sequencer.records - 1,
            incomplete: result.output.stats.capture_incomplete,
        };
        write_events_file(&result.dir.join(EVENTS_FILE), &header, capture)?;
    }
    if config.keep_journal {
        fs::write(result.dir.join(LIVE_SNAPSHOT_FILE), format!("{:#?}\n", result.snapshot))?;
    }
    if config.capture {
        let started = Instant::now();
        // After a clean stop recovery finds nothing to cut; it gives replay the journal's end.
        let recovered = recover(journal_dir, &config.identity(), config.allow_engine_change)?;
        let live = LiveRun {
            snapshot: &result.snapshot,
            capture: result.output.capture.as_deref(),
            capture_incomplete: result.output.stats.capture_incomplete,
            journal_discarded: config.discarded(),
            first_seq: result.first_seq,
            sequenced: result.output.sequencer.records,
            applied: result.output.core.commands,
        };
        let other_seed = config.engine_options().id_hash_seed ^ 0x9E37_79B9_7F4A_7C15;
        result.replay = Some(replay_test(journal_dir, &recovered, &live, other_seed)?);
        eprintln!("run {name}: replay test done in {:.1} s", started.elapsed().as_secs_f64());
    }
    if config.audit
        && let Some(registry) = &result.registry
    {
        let started = Instant::now();
        result.audit = Some(verify_journal(journal_dir, std::slice::from_ref(registry)));
        eprintln!("run {name}: signature audit done in {:.1} s", started.elapsed().as_secs_f64());
    }
    if !config.keep_journal && journal_dir.exists() {
        fs::remove_dir_all(journal_dir)?;
    }
    Ok(())
}

/// The directory of run `name` inside `parent`.
pub fn run_dir(parent: &Path, name: &str) -> PathBuf {
    parent.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_gateway_gets_room_for_every_message_the_sender_offers_it() {
        let config = RunConfig { fixed_timed_clients: Some(200), ..RunConfig::smoke(InjectionMode::Signed) };
        let workload = Workload::build(&config, None);
        // A run that sends 150 of the 200 timed client items (a prefix, 14.8).
        let timing = Timing {
            arrivals: config.arrivals,
            setup_rate: SETUP_RATE,
            rate: config.rate,
            timed_clients: 150,
            warmup_ns: config.warmup_ns,
            window_ns: config.window_ns,
        };
        let sender_plan = SenderPlan::new(&workload.plan, workload.messages.clone(), &timing);
        let sent = workload.plan.setup_client_items() + 150;
        let expected: Vec<usize> = (0..3)
            .map(|g| {
                workload
                    .plan
                    .client_items()
                    .take(sent)
                    .filter(|item| gateway_of(item.account, 3) == g)
                    .count()
            })
            .collect();
        let requests = requests_per_gateway(&sender_plan, 3);
        assert_eq!(requests, expected);
        assert_eq!(requests.iter().sum::<usize>(), sent);
        assert!(requests.iter().all(|&n| n > 0), "{requests:?}");
    }

    #[test]
    fn a_backlog_still_draining_past_the_cap_fails_even_after_the_sender_finished() {
        let closed = Instant::now().checked_sub(Duration::from_millis(50)).expect("the clock is past 50 ms");
        let mut monitor = Monitor { window_closed_at: Some(closed), ..Monitor::default() };
        monitor.check_drained(None);
        assert!(!monitor.drain_cap_exceeded, "no cap, no fail");
        monitor.check_drained(Some(60_000_000_000));
        assert!(!monitor.drain_cap_exceeded, "drained within a minute");
        monitor.check_drained(Some(10_000_000));
        assert!(monitor.drain_cap_exceeded, "50 ms after the window, the cap was 10 ms");
    }
}
