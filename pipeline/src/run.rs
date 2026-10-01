//! `Pipeline`: starts the sequencer, the journal writer, the core and the gate, on a new
//! journal or resumed after a crash, and joins them when the shutdown cascade is done
//! (`docs/PIPELINE.md` 2.8, 13.5 and 19.2).
//!
//! **Contract.**
//! - [`Pipeline::start`] installs the abort-on-panic hook (2.8), checks the settings,
//!   prepares the journal's first segment for this life (and preallocates the rest, 11.6),
//!   creates the three inner rings, and spawns the four threads, each pinned to its CPU in
//!   the layout (2.6) before it touches its data. The caller owns the inputs: the lanes and
//!   the operator ring (the gateways and the sender live in other crates and are wired in
//!   by the harness, 19.2).
//! - **Fresh or resumed** (13.5). A new journal starts at seq 1 in segment 0, in a
//!   directory that doesn't already hold a journal: no segment there may have a valid
//!   header. What a crash in a life's first flush can leave there (the header's sector
//!   lost, part of the body on disk: no record of it was released) is a torn tail, and
//!   recovery's steps 2 and 3 check it, copy it aside and zero it, as a restart would, so
//!   the new life starts with zeros after its end (11.8). A restart takes a [`Resume`] from
//!   recovery and replay: the replayed engine, the next seq, the last `ts` (the clock must
//!   be anchored above it, [`RunClock::resume`]), and where the journal ended (the writer
//!   starts in the next segment, 11.8 step 4). The durable watermark starts at the last
//!   recovered seq: those records are on disk.
//! - **Stopping follows the data** (2.8). There is no stop flag. When the caller drops the
//!   lanes' and the operator ring's producers, the sequencer drains them and closes the
//!   journal and core rings; the writer flushes its last batch at once and stops; the core
//!   drains its ring and closes the event ring; the gate releases the last events once the
//!   final flush covers them, and stops. [`Pipeline::join`] waits for all of it.
//! - While it runs, [`Pipeline::released`] and [`Pipeline::counters`] read the
//!   single-writer counters (3.2, 15.4), and [`Pipeline::phases`] lets main set the measured
//!   window.
//!
//! **The "verify on core" ablation** (section 16): `ablation_verify_on_core` makes the core
//! ring's records 3 lines long, in both arms, and in the core arm hands the core a
//! verifier. Insecure by design; only `e2e ablate` sets it, and only with the perp signing
//! scheme (the core's verifier rebuilds that scheme's signed bytes).
//!
//! **The signing scheme** (`auth`; `docs/DECISIONS.md` D-033) changes nothing here: the
//! gateways check the signatures, and a kind-1 record holds the same words either way. It
//! goes into the journal's identity (header byte 120), so that the audit checks the records
//! the right way and a restart can't switch schemes. `start` refuses the EIP-712 scheme in
//! pre-verified mode (nothing is signed there) and with the ablation.

use std::fs::File;
use std::sync::Arc;
use std::thread::JoinHandle;

use engine::book::Book;
use engine::engine::{Engine, EngineOptions};
use engine::mode::Fast;

use crate::affinity::{CpuLayout, Role, pin_current_thread};
use crate::clock::RunClock;
use crate::core_thread::{CoreConfig, CoreStats, CoreVerifier, EventRingSink, run_core};
use crate::counters::{CounterSnapshot, PipelineCounters, Watermark};
use crate::gate::{Gate, GateConfig, GateStats, Phases, run_gate};
use crate::idle::IdleStrategy;
use crate::journal::files::{DiscardFiles, JournalFiles, StdFiles};
use crate::journal::format::{JournalIdentity, SegmentHeader};
use crate::journal::recovery::{check_tail, make_durable, scan_journal};
use crate::journal::writer::{
    JournalConfig, JournalMode, JournalStats, JournalWriter, preallocate, run_writer,
};
use crate::journal::{JournalError, JournalPosition};
use crate::panic::install_abort_on_panic;
use crate::records::{AuthScheme, InjectionMode, Stamps};
use crate::ring::{Producer, channel};
use crate::sequencer::{Inputs, Sequencer, SequencerStats, run_sequencer};

/// The capacities of the three rings inside the pipeline (2.3). The lanes and the operator
/// ring belong to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingCapacities {
    /// Sequencer → journal writer: 65,536 records.
    pub journal: usize,
    /// Sequencer → core: 16,384 records.
    pub core: usize,
    /// Core → gate: 131,072 slots.
    pub event: usize,
}

impl Default for RingCapacities {
    fn default() -> Self {
        RingCapacities { journal: 65_536, core: 16_384, event: 131_072 }
    }
}

/// Everything the pipeline is started with (PIPELINE.md 19.2).
#[derive(Debug)]
pub struct PipelineConfig {
    pub deployment: u32,
    /// Signed or pre-verified: the journal's mode, and the kind of every lane record.
    pub mode: InjectionMode,
    /// How the gateways check signed messages (module docs): part of the journal's
    /// identity. `Perp` in pre-verified mode.
    pub auth: AuthScheme,
    pub engine: EngineOptions,
    /// Lanes (`N`): must match the inputs.
    pub lanes: usize,
    pub capacities: RingCapacities,
    pub journal: JournalConfig,
    /// SHA-256 of the key registry the gateways loaded; zero in pre-verified mode (11.2).
    pub registry_digest: [u8; 32],
    pub layout: CpuLayout,
    pub idle: IdleStrategy,
    /// Event slots to reserve for the capture (13.2); `None`: no capture.
    pub capture: Option<usize>,
    /// The kill test's release log (18.4).
    pub release_log: Option<File>,
    /// The gate keeps running totals of the released events, for `e2e run --watch`
    /// (`PipelineCounters::live`).
    pub live: bool,
    pub stamps: Stamps,
    /// Which commands are setup and which are measured (15.5); main can change it while
    /// the pipeline runs ([`Pipeline::phases`]).
    pub phases: Phases,
    /// The "verify on core" ablation (section 16), or `None` for the pipeline as specified.
    /// **Insecure**: only `e2e ablate` sets it.
    pub ablation_verify_on_core: Option<VerifyOnCore>,
}

/// The two arms of the "verify on core" ablation (section 16). Both give the core ring
/// 3-line records that carry each command's nonce, expiry and signature (3.3), so the
/// record's size is not a hidden difference between them.
#[derive(Debug)]
pub enum VerifyOnCore {
    /// The control arm: the gateways verify, as in every other run.
    Gateways,
    /// **Insecure**: the gateways skip checks 11 and 12 (and have already used up the
    /// nonce), and the core verifies each signed command just before applying it, with its
    /// own copy of the key registry. A bad signature aborts the run.
    Core(Box<dyn CoreVerifier>),
}

impl PipelineConfig {
    /// The journal identity this configuration starts or expects (11.2).
    pub fn identity(&self) -> JournalIdentity {
        JournalIdentity::new(self.deployment, self.mode, self.engine).with_auth(self.auth)
    }
}

/// The signing scheme's refusals of [`Pipeline::start`] (module docs): the EIP-712 scheme
/// only in signed mode, and not with the "verify on core" ablation.
fn check_auth(mode: InjectionMode, auth: AuthScheme, ablation: bool) -> Result<(), JournalError> {
    if auth == AuthScheme::Eip712 && mode != InjectionMode::Signed {
        return Err(JournalError::Refused("the eip712 signing scheme needs signed mode".into()));
    }
    if auth == AuthScheme::Eip712 && ablation {
        return Err(JournalError::Refused(
            "the verify-on-core ablation checks perp signatures only, not the eip712 scheme".into(),
        ));
    }
    Ok(())
}

/// Everything a restart needs, from recovery and replay (13.5).
#[derive(Debug)]
pub struct Resume {
    pub engine: Engine<Book, Fast>,
    pub next_seq: u64,
    pub last_ts: u64,
    /// Where the recovered journal ended: this life starts in the next segment.
    pub end: JournalPosition,
}

/// What the pipeline hands back when it is joined.
#[derive(Debug)]
pub struct PipelineOutput {
    /// The live engine, for the snapshot (13.3).
    pub engine: Engine<Book, Fast>,
    pub stats: GateStats,
    pub journal: JournalStats,
    pub core: CoreStats,
    pub sequencer: SequencerStats,
    /// The gate's capture, if it was on.
    pub capture: Option<Vec<u64>>,
}

/// The running pipeline. See the module docs.
#[derive(Debug)]
pub struct Pipeline {
    counters: Arc<PipelineCounters>,
    durable: Arc<Watermark>,
    phases: Arc<Phases>,
    sequencer: JoinHandle<SequencerStats>,
    journal: JoinHandle<JournalStats>,
    core: CoreHandle,
    gate: JoinHandle<(GateStats, Option<Vec<u64>>)>,
}

impl Pipeline {
    /// Starts the pipeline (module docs). `resume` is `None` for a new journal. Installs
    /// the abort-on-panic hook (2.8). Returns an error, before any thread starts, if the
    /// settings are refused or the journal's first segment can't be prepared.
    pub fn start(
        mut config: PipelineConfig,
        inputs: Inputs,
        clock: RunClock,
        resume: Option<Resume>,
    ) -> Result<Pipeline, JournalError> {
        install_abort_on_panic();
        config.journal.check()?;
        let ablation = config.ablation_verify_on_core.take();
        if ablation.is_some() && config.stamps == Stamps::Off {
            return Err(JournalError::Refused("the verify-on-core ablation needs stamps on".into()));
        }
        check_auth(config.mode, config.auth, ablation.is_some())?;
        if inputs.lanes.len() != config.lanes {
            let lanes = inputs.lanes.len();
            return Err(JournalError::Refused(format!(
                "{lanes} lanes given, but the configuration says {}",
                config.lanes
            )));
        }
        let (engine, next_seq, first_segment) = match resume {
            None => (None, 1, 0),
            Some(resume) => {
                if clock.start_unix_ns() <= resume.last_ts {
                    return Err(JournalError::Refused(format!(
                        "the clock is anchored at {}, not above the journal's last ts {} (use RunClock::resume, 9.2)",
                        clock.start_unix_ns(),
                        resume.last_ts
                    )));
                }
                (Some(resume.engine), resume.next_seq, resume.end.next_life_segment())
            }
        };
        let header =
            SegmentHeader::for_life(config.identity(), clock.start_unix_ns(), config.registry_digest);
        let journal = &config.journal;
        let durable = Arc::new(Watermark::new(next_seq - 1));
        let counters = Arc::new(PipelineCounters::new());
        let phases = Arc::new(config.phases);
        let (journal_in, journal_out) = channel::<3>(config.capacities.journal);
        let (event_in, event_out) = channel::<1>(config.capacities.event);
        let layout = &config.layout;

        let journal_thread = match journal.mode {
            JournalMode::Disk => {
                let files = prepare_life(journal, first_segment, next_seq == 1)?;
                let writer = JournalWriter::with_config(files, header, first_segment, journal);
                spawn_writer(writer, journal_out, &durable, &counters, clock, config.idle, layout)
            }
            JournalMode::Discard => {
                let files = DiscardFiles::new(journal.segment_bytes);
                let writer = JournalWriter::with_config(files, header, first_segment, journal);
                spawn_writer(writer, journal_out, &durable, &counters, clock, config.idle, layout)
            }
        };

        let middle = Middle {
            inputs,
            journal_in,
            event_in,
            core_capacity: config.capacities.core,
            mode: config.mode,
            stamps: config.stamps,
            options: config.engine,
            engine,
            next_seq,
            clock,
            idle: config.idle,
            layout,
            counters: &counters,
        };
        // The core ring's slots: 2 lines, or 3 in the ablation (3.3).
        let (sequencer_thread, core_thread) = match ablation {
            None => middle.spawn::<2>(None),
            Some(VerifyOnCore::Gateways) => middle.spawn::<3>(None),
            Some(VerifyOnCore::Core(verifier)) => middle.spawn::<3>(Some(verifier)),
        };

        let gate = Gate::new(
            event_out,
            GateConfig {
                lanes: config.lanes,
                next_seq,
                stamps: config.stamps,
                capture: config.capture,
                release_log: config.release_log,
                live: config.live,
                clock,
            },
        );
        let gate_thread = {
            let (counters, durable, phases, idle) =
                (Arc::clone(&counters), Arc::clone(&durable), Arc::clone(&phases), config.idle);
            spawn_pinned("gate", layout.cpu(Role::Gate), move || {
                let _ = counters.gate.publish_tid();
                run_gate(gate, &durable, &phases, &counters, idle)
            })
        };

        Ok(Pipeline {
            counters,
            durable,
            phases,
            sequencer: sequencer_thread,
            journal: journal_thread,
            core: core_thread,
            gate: gate_thread,
        })
    }

    /// Commands released so far: main's barriers wait on it (14.4).
    pub fn released(&self) -> u64 {
        self.counters.released.load()
    }

    /// The durable watermark: every command with a seq at or below it is on disk.
    pub fn durable(&self) -> u64 {
        self.durable.load()
    }

    /// Thread health, sampled at the window's edges (15.4).
    pub fn counters(&self) -> CounterSnapshot {
        self.counters.snapshot()
    }

    /// The same counters, shared: the sender's barriers read `released` from its own
    /// thread (14.4; `loadgen::sender::Barrier`).
    pub fn shared_counters(&self) -> Arc<PipelineCounters> {
        Arc::clone(&self.counters)
    }

    /// The measured window, which main may set while the pipeline runs.
    pub fn phases(&self) -> &Phases {
        &self.phases
    }

    /// The same phases, shared: each gateway thread takes a handle, to count the rejects
    /// of messages scheduled inside the window (7.3, 15.4).
    pub fn shared_phases(&self) -> Arc<Phases> {
        Arc::clone(&self.phases)
    }

    /// Waits for the shutdown cascade of 2.8, which starts when the caller drops the
    /// lanes' and the operator ring's producers, and returns what the threads measured.
    pub fn join(self) -> PipelineOutput {
        // A thread that panicked has already aborted the process (2.8), so a join error
        // can't happen here.
        let sequencer = self.sequencer.join().expect("the sequencer thread ends cleanly");
        let journal = self.journal.join().expect("the journal writer thread ends cleanly");
        let (engine, core) = self.core.join().expect("the core thread ends cleanly");
        let (stats, capture) = self.gate.join().expect("the gate thread ends cleanly");
        PipelineOutput { engine, stats, journal, core, sequencer, capture }
    }
}

/// Everything the sequencer and the core start with. They are started together because
/// the ring between them has 2-line slots, or 3-line ones in the "verify on core" ablation
/// ([`Middle::spawn`]'s `LINES`).
struct Middle<'a> {
    inputs: Inputs,
    journal_in: Producer<3>,
    event_in: Producer<1>,
    core_capacity: usize,
    mode: InjectionMode,
    stamps: Stamps,
    /// For a new engine; `engine` is the replayed one after a restart.
    options: EngineOptions,
    engine: Option<Engine<Book, Fast>>,
    next_seq: u64,
    clock: RunClock,
    idle: IdleStrategy,
    layout: &'a CpuLayout,
    counters: &'a Arc<PipelineCounters>,
}

type CoreHandle = JoinHandle<(Engine<Book, Fast>, CoreStats)>;

impl Middle<'_> {
    /// Creates the core ring with `LINES`-line slots and spawns the sequencer and the core,
    /// each pinned to its CPU. `verifier` is the ablation's core arm (section 16).
    fn spawn<const LINES: usize>(
        self,
        verifier: Option<Box<dyn CoreVerifier>>,
    ) -> (JoinHandle<SequencerStats>, CoreHandle) {
        let (core_in, core_out) = channel::<LINES>(self.core_capacity);
        let sequencer = Sequencer::new(
            self.inputs,
            self.journal_in,
            core_in,
            self.mode,
            self.stamps,
            self.clock,
            self.next_seq,
        );
        let sequencer_thread = {
            let (counters, idle) = (Arc::clone(self.counters), self.idle);
            spawn_pinned("sequencer", self.layout.cpu(Role::Sequencer), move || {
                let _ = counters.sequencer.publish_tid(); // if it fails, only the fault count is lost
                run_sequencer(sequencer, idle, &counters)
            })
        };
        let core_thread = {
            let (counters, options, engine, event_in) =
                (Arc::clone(self.counters), self.options, self.engine, self.event_in);
            let core_config = CoreConfig {
                next_seq: self.next_seq,
                stamps: self.stamps,
                clock: self.clock,
                idle: self.idle,
            };
            spawn_pinned("core", self.layout.cpu(Role::Core), move || {
                let _ = counters.core.publish_tid();
                // A new engine is built here, after pinning, so its memory is first touched
                // on the core's CPU package (10.1).
                let engine = engine.unwrap_or_else(|| Engine::new(options));
                let sink = EventRingSink::new(
                    event_in,
                    core_config.clock,
                    core_config.idle,
                    &counters.core_stall_ns,
                );
                run_core(engine, core_out, sink, core_config, verifier.as_deref(), &counters.core)
            })
        };
        (sequencer_thread, core_thread)
    }
}

/// Opens the journal directory for this life, whose first segment is `first_segment`, and
/// makes sure that segment exists (and the preallocated ones after it). A new journal
/// (`fresh`) refuses a directory that already holds one, and zeroes a torn first flush
/// ([`clear_for_a_new_journal`]); every existing segment must have the configured size.
fn prepare_life(config: &JournalConfig, first_segment: u32, fresh: bool) -> Result<StdFiles, JournalError> {
    let dir = &config.dir;
    let io = |what: &str| {
        let what = format!("{what} in {}", dir.display());
        move |e| JournalError::io(what, e)
    };
    let mut files = StdFiles::open(dir, config.segment_bytes).map_err(io("opening the journal"))?;
    let existing = StdFiles::open_existing(dir, config.segment_bytes).map_err(io("reading the journal"))?;
    if existing.segment_bytes() != config.segment_bytes {
        return Err(JournalError::Refused(format!(
            "the journal's segments are {} bytes, but the configuration says {}",
            existing.segment_bytes(),
            config.segment_bytes
        )));
    }
    if fresh {
        clear_for_a_new_journal(&mut files)?;
    }
    preallocate(&mut files, first_segment, config.preallocate.max(1)).map_err(io("creating segments"))?;
    Ok(files)
}

/// A new journal's check of its directory (module docs, "Fresh or resumed"). Any segment
/// with a valid header means a journal is there: refused. Without one, whatever else is
/// nonzero must be what a crash in a life's first flush leaves (its header's sector lost,
/// some of its body on disk; nothing of it was released): recovery's step 2 checks exactly
/// that, and step 3 copies it aside to `torn-*.bin` and zeroes it, so the new life can't
/// write its records in front of an old life's and have them read on into later (review
/// finding F-FRESH-START). Anything else nonzero is an error. Returns the torn copies.
fn clear_for_a_new_journal(files: &mut StdFiles) -> Result<Vec<String>, JournalError> {
    let scan = scan_journal(files, |_, _, _| {})?;
    if !scan.headers.is_empty() {
        return Err(JournalError::Refused(format!(
            "{} already holds a journal: recover and resume it (13.5), or start a new journal in a new \
             directory under a new deployment id (5.1)",
            files.dir().display()
        )));
    }
    let tail = check_tail(files, scan.end)?;
    make_durable(files, scan.end, &tail)
}

fn spawn_writer<F: JournalFiles + Send + 'static>(
    writer: JournalWriter<F>,
    ring: crate::ring::Consumer<3>,
    durable: &Arc<Watermark>,
    counters: &Arc<PipelineCounters>,
    clock: RunClock,
    idle: IdleStrategy,
    layout: &CpuLayout,
) -> JoinHandle<JournalStats> {
    let (durable, counters) = (Arc::clone(durable), Arc::clone(counters));
    spawn_pinned("journal", layout.cpu(Role::Journal), move || {
        let _ = counters.journal.publish_tid();
        run_writer(writer, ring, &durable, clock, idle, &counters)
    })
}

/// Spawns a named thread that pins itself to `cpu` (if any) before running `body`.
fn spawn_pinned<T: Send + 'static>(
    name: &str,
    cpu: Option<usize>,
    body: impl FnOnce() -> T + Send + 'static,
) -> JoinHandle<T> {
    let thread_name = name.to_string();
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            if let Some(cpu) = cpu {
                pin_current_thread(cpu)
                    .unwrap_or_else(|e| panic!("pinning the {thread_name} thread to CPU {cpu}: {e}"));
            }
            body()
        })
        .unwrap_or_else(|e| panic!("spawning the {name} thread: {e}"))
}

#[cfg(test)]
mod tests {
    //! `Pipeline::start` installs the abort-on-panic hook, so starting a pipeline is tested
    //! in `pipeline/tests/pipeline_e2e.rs`; these test what comes before.
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pipeline-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn config(dir: &std::path::Path) -> JournalConfig {
        JournalConfig { segment_bytes: 4_096, preallocate: 3, ..JournalConfig::new(dir.to_path_buf()) }
    }

    #[test]
    fn a_new_life_gets_its_first_segment_and_the_preallocated_ones() {
        let dir = temp_dir("prepare");
        let mut files = prepare_life(&config(&dir), 0, true).expect("prepared");
        assert_eq!(files.segments().expect("listed"), [0, 1, 2]);
        let mut files = prepare_life(&config(&dir), 2, false).expect("a resumed life in segment 2");
        assert_eq!(files.segments().expect("listed"), [0, 1, 2, 3, 4]);
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn a_new_journal_is_refused_where_one_already_is_and_so_is_another_segment_size() {
        let dir = temp_dir("refuse");
        let mut files = prepare_life(&config(&dir), 0, true).expect("prepared");
        let identity = JournalIdentity::new(1, InjectionMode::PreVerified, EngineOptions::default());
        let header = SegmentHeader { first_seq: 1, ..SegmentHeader::for_life(identity, 1, [0; 32]) };
        files.write_at(0, 0, &header.encode()).expect("written");
        let error = prepare_life(&config(&dir), 0, true).expect_err("a journal is there");
        assert!(error.to_string().contains("already holds a journal"), "{error}");
        let bigger = JournalConfig { segment_bytes: 8_192, ..config(&dir) };
        let error = prepare_life(&bigger, 1, false).expect_err("another size");
        assert!(error.to_string().contains("segments are 4096 bytes"), "{error}");
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn a_new_journal_zeroes_a_torn_first_flush_and_refuses_anything_else() {
        let dir = temp_dir("fresh");
        let mut files = prepare_life(&config(&dir), 0, true).expect("prepared");
        // A first flush whose header's sector was lost: only the body reached the disk.
        files.write_at(0, 600, &[7; 100]).expect("written");
        files.sync_data(0).expect("synced");
        let mut files = prepare_life(&config(&dir), 0, true).expect("a torn first flush is not a journal");
        let mut segment = [9; 4_096];
        files.read_at(0, 0, &mut segment).expect("read");
        assert!(segment.iter().all(|&b| b == 0), "the stale body is zeroed");
        assert!(dir.join("torn-000000-0.bin").is_file(), "and copied aside first");
        // Nonzero bytes in segments 0 and 1 at once can't be one torn flush.
        files.write_at(0, 600, &[7; 100]).expect("written");
        files.write_at(1, 600, &[7; 100]).expect("written");
        let error = prepare_life(&config(&dir), 0, true).expect_err("not a torn tail");
        assert!(matches!(error, JournalError::Corrupt { .. }), "{error}");
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn the_eip712_scheme_is_refused_in_pre_verified_mode_and_with_the_ablation() {
        use AuthScheme::{Eip712, Perp};
        use InjectionMode::{PreVerified, Signed};
        assert!(check_auth(Signed, Eip712, false).is_ok());
        let error = check_auth(PreVerified, Eip712, false).expect_err("nothing is signed");
        assert!(error.to_string().contains("needs signed mode"), "{error}");
        let error = check_auth(Signed, Eip712, true).expect_err("the core verifies perp signatures");
        assert!(error.to_string().contains("verify-on-core"), "{error}");
        for (mode, ablation) in [(Signed, false), (Signed, true), (PreVerified, false)] {
            assert!(check_auth(mode, Perp, ablation).is_ok(), "the perp scheme as before");
        }
    }

    #[test]
    fn the_default_capacities_are_those_of_2_3() {
        let capacities = RingCapacities::default();
        assert_eq!((capacities.journal, capacities.core, capacities.event), (65_536, 16_384, 131_072));
    }
}
