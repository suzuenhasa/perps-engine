//! The pipeline in process, end to end: sequencer → journal → core → gate, fed with
//! operator records and client records by a test sender, on a real journal directory
//! (`docs/PIPELINE.md` 2.8, 12, 13.3 and 13.5).
//!
//! **What is checked.**
//! - **Output gating** (12.1): while the first batch waits for its commit interval, nothing
//!   is released; at every moment the commands released are at most the durable
//!   watermark; and the gate's durability wait shows the commit interval (a gate that
//!   released at `t_done` would show microseconds).
//! - **Nothing is lost in the shutdown cascade** (2.8): sequenced = journaled = applied =
//!   released, and the capture is complete.
//! - **Replay equality** (13.3): the journal, recovered and replayed from disk, gives the
//!   live run's event stream slot for slot and an equal `EngineSnapshot`, also with another
//!   hash seed, and through the reference engine too; the nonce table is each account's
//!   last nonce; the gate's fund equity equals the snapshot's.
//! - **Restart** (13.5): recover, replay, resume in a new segment with the replayed engine,
//!   the next seq and a clock anchored above the last timestamp; the second life's live
//!   engine and events match a replay of both lives.
//! - Signed journals (kind-1 records with their signature and expiry), discard mode, the
//!   release log and `--stamps off`.
//!
//! The sender here waits for ring space (it is a test, not the open-loop load generator),
//! sends setup first and waits until it is released (as the harness's barriers do, 14.4),
//! then the timed flow. Every thread idles with "spin then yield", so the test shares CPUs
//! politely; outcomes don't depend on the scheduling.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engine::command::Command;
use engine::engine::FUND;
use engine::mode::Naive;
use engine::reference::ReferenceBook;
use engine::types::AccountId;

use pipeline::affinity::CpuLayout;
use pipeline::clock::RunClock;
use pipeline::codec::encode_command;
use pipeline::gate::{EventsHeader, Phases, Stage, read_events_file};
use pipeline::idle::IdleStrategy;
use pipeline::journal::files::StdFiles;
use pipeline::journal::format::JournalIdentity;
use pipeline::journal::recovery::{Recovered, recover, scan_journal};
use pipeline::journal::writer::{JournalConfig, JournalMode};
use pipeline::records::{
    AuthScheme, ClientRecord, InjectionMode, Meta, OperatorRecord, SIGNATURE_WORDS, Source, Stamps,
};
use pipeline::replay::{LiveRun, ReplayVerdict, first_difference, replay, replay_as, replay_test};
use pipeline::ring::{Producer, channel};
use pipeline::run::{Pipeline, PipelineConfig, PipelineOutput, Resume, RingCapacities};
use pipeline::sequencer::Inputs;

use common::{TestFlow, client_account, engine_options, is_client};

const IDLE: IdleStrategy = IdleStrategy::SpinThenYield { spins: 64 };
const LANES: usize = 2;
const SEED: u64 = 0xE2E;
/// The commit interval: long enough that a gate releasing before the disk would show.
const COMMIT_INTERVAL: Duration = Duration::from_millis(100);

/// A fresh directory for one test's run.
fn run_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pipeline-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

fn config(dir: &Path, mode: InjectionMode, journal: JournalMode, stamps: Stamps) -> PipelineConfig {
    PipelineConfig {
        deployment: 7,
        mode,
        auth: AuthScheme::Perp,
        engine: engine_options(SEED),
        lanes: LANES,
        capacities: RingCapacities { journal: 1_024, core: 256, event: 512 },
        journal: JournalConfig {
            segment_bytes: 256 << 10,
            commit_interval_ns: COMMIT_INTERVAL.as_nanos() as u64,
            mode: journal,
            preallocate: 2,
            ..JournalConfig::new(dir.join("journal"))
        },
        registry_digest: if mode == InjectionMode::Signed { [0x5A; 32] } else { [0; 32] },
        layout: CpuLayout::unpinned(),
        idle: IDLE,
        capture: Some(1 << 20),
        release_log: None,
        live: false,
        stamps,
        phases: Phases::not_yet(),
        ablation_verify_on_core: None,
    }
}

/// The test's sender: client commands into lane `account mod N` with the account's next
/// nonce, operator commands into the operator ring. It waits for ring space.
struct Sender {
    lanes: Vec<Producer<3>>,
    operator: Producer<1>,
    mode: InjectionMode,
    clock: RunClock,
    /// The last nonce of each account (index = account).
    nonces: Vec<u64>,
}

impl Sender {
    fn send(&mut self, command: &Command) {
        let t_sched = self.clock.now();
        if is_client(command) {
            let account = client_account(command);
            self.nonces[account.index()] += 1;
            let nonce = self.nonces[account.index()];
            let lane = account.get() as usize % LANES;
            let record = self.client_record(command, account, lane, nonce, t_sched);
            let ring = &mut self.lanes[lane];
            while ring.free(1) == 0 {
                IDLE.idle();
            }
            ring.write(&record.to_words());
            ring.publish();
        } else {
            while self.operator.free(1) == 0 {
                IDLE.idle();
            }
            let record =
                OperatorRecord { command: encode_command(command), t_sched, t_sent: self.clock.now() };
            self.operator.write(&record.to_words());
            self.operator.publish();
        }
    }

    /// A lane record as a gateway (signed) or the pre-verified sender would write it (3.3).
    fn client_record(
        &self,
        command: &Command,
        account: AccountId,
        lane: usize,
        nonce: u64,
        t_sched: u64,
    ) -> ClientRecord {
        let t_sent = self.clock.now();
        let signed = self.mode == InjectionMode::Signed;
        ClientRecord {
            meta: Meta { source: self.mode.client_source(), lane: lane as u16, account },
            nonce,
            command: encode_command(command),
            expires_at: if signed { u64::MAX } else { 0 },
            // The pipeline never verifies: any bytes do, and the journal must keep them.
            signature: if signed { [nonce ^ 0xABCD; SIGNATURE_WORDS] } else { [0; SIGNATURE_WORDS] },
            t_sched,
            t_sent,
            t_gw_in: if signed { self.clock.now() } else { 0 },
            t_gw_out: if signed { self.clock.now() } else { 0 },
        }
    }
}

/// A started pipeline and its sender.
struct Run {
    pipeline: Pipeline,
    sender: Sender,
    clock: RunClock,
    sent: u64,
}

impl Run {
    fn start(config: PipelineConfig, clock: RunClock, resume: Option<Resume>, nonces: Vec<u64>) -> Run {
        let mode = config.mode;
        let (lanes, lane_ends): (Vec<_>, Vec<_>) = (0..LANES).map(|_| channel::<3>(256)).unzip();
        let (operator, operator_end) = channel::<1>(256);
        let inputs = Inputs { lanes: lane_ends, operator: operator_end };
        let pipeline = Pipeline::start(config, inputs, clock, resume).expect("the pipeline starts");
        let sender = Sender { lanes, operator, mode, clock, nonces };
        Run { pipeline, sender, clock, sent: 0 }
    }

    fn send_all(&mut self, commands: &[Command]) {
        for command in commands {
            self.sender.send(command);
            self.sent += 1;
        }
    }

    /// Waits until everything sent so far is released, checking at every sample that no
    /// command was released before it was durable.
    fn wait_released(&self, first_seq: u64) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let released = self.pipeline.released(); // read before the watermark
            let durable = self.pipeline.durable();
            assert!(
                first_seq - 1 + released <= durable,
                "released {released} commands, durable only to seq {durable}"
            );
            if released == self.sent {
                return;
            }
            assert!(Instant::now() < deadline, "only {released} of {} released", self.sent);
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    /// Drops the sender's ring ends (the shutdown cascade, 2.8) and joins.
    fn stop(self) -> (PipelineOutput, Vec<u64>) {
        let Run { pipeline, sender, .. } = self;
        let nonces = sender.nonces;
        drop(sender.lanes);
        drop(sender.operator);
        (pipeline.join(), nonces)
    }
}

fn identity(mode: InjectionMode) -> JournalIdentity {
    JournalIdentity::new(7, mode, engine_options(SEED))
}

/// Sends the setup, and checks gating while the first batch waits for its commit interval;
/// then opens the measured window and sends the timed flow.
fn run_flow(run: &mut Run, flow: &mut TestFlow, timed: usize) {
    let setup = flow.setup();
    let started = Instant::now();
    run.send_all(&setup);
    // Before the first flush, the core has applied the setup but nothing is released.
    while run.pipeline.durable() == 0 {
        assert_eq!(run.pipeline.released(), 0, "released before anything was durable");
        std::thread::sleep(Duration::from_micros(200));
    }
    assert!(started.elapsed() >= COMMIT_INTERVAL / 2, "the first flush waited for its interval");
    run.wait_released(1);
    let t0 = run.clock.now();
    run.pipeline.phases().set(t0, t0..u64::MAX);
    let commands: Vec<Command> = (0..timed).map(|_| flow.next_command()).collect();
    run.send_all(&commands);
}

/// The replay test of 13.3 on a finished run, required to say "identical".
fn assert_replays_identically(dir: &Path, recovered: &Recovered, output: &PipelineOutput, first_seq: u64) {
    let snapshot = output.engine.snapshot();
    let live = LiveRun {
        snapshot: &snapshot,
        capture: output.capture.as_deref(),
        capture_incomplete: output.stats.capture_incomplete,
        journal_discarded: false,
        first_seq,
        sequenced: output.sequencer.records,
        applied: output.core.commands,
    };
    match replay_test(&dir.join("journal"), recovered, &live, SEED ^ 0xFFFF).expect("replayed") {
        ReplayVerdict::Identical(report) => {
            assert_eq!(report.records, recovered.records);
            assert_eq!(report.events, output.stats.captured);
        }
        other => panic!("the replay test said {other:?}"),
    }
}

#[test]
fn a_pre_verified_run_is_gated_by_the_journal_and_replays_identically() {
    let dir = run_dir("preverified");
    let clock = RunClock::start();
    let config = config(&dir, InjectionMode::PreVerified, JournalMode::Disk, Stamps::On);
    let mut run = Run::start(config, clock, None, vec![0; common::ACCOUNTS as usize + 1]);
    let mut flow = TestFlow::new(SEED);
    run_flow(&mut run, &mut flow, 6_000);
    run.wait_released(1);
    let sent = run.sent;
    let (output, nonces) = run.stop();

    // Nothing lost in the shutdown cascade.
    assert_eq!(output.sequencer.records, sent);
    assert_eq!(output.journal.records, sent);
    assert_eq!(output.core.commands, sent);
    assert_eq!(output.stats.commands, sent);
    assert_eq!(output.stats.events, output.core.events);
    assert!(!output.stats.capture_incomplete);

    // The gate waited for the disk: the setup's commands waited about one commit interval.
    let wait = output.stats.client.get(Stage::DurabilityWait);
    assert!(wait.count() > 0, "client commands in the window");
    let setup = &output.stats.setup;
    assert_eq!(
        setup.commands.iter().sum::<u64>(),
        TestFlow::new(SEED).setup().len() as u64,
        "outside the window"
    );
    assert!(output.stats.window.client_commands() > 4_000);
    assert!(output.stats.client.get(Stage::CorePath).count() > 0);
    assert_eq!(output.stats.client.get(Stage::Verification).count(), 0, "no gateways in pre-verified mode");
    assert_eq!(output.stats.inversions, [0; 9]);
    assert!(output.journal.flushes >= 2);

    // The journal alone rebuilds the run.
    let recovered =
        recover(&dir.join("journal"), &identity(InjectionMode::PreVerified), false).expect("recovered");
    assert_eq!(recovered.records, sent);
    assert_replays_identically(&dir, &recovered, &output, 1);

    // The nonce table is each account's last nonce, and the fund's equity adds up.
    let replayed = replay(&dir.join("journal"), &recovered, None, false).expect("replayed");
    for (account, &last) in nonces.iter().enumerate().skip(1) {
        assert_eq!(replayed.nonces.get(AccountId::new(account as u32)), last, "account {account}");
    }
    let snapshot = output.engine.snapshot();
    assert_eq!(output.stats.fund.final_equity, i128::from(snapshot.fund_balance) + snapshot.fund_upnl_total);
    assert!(snapshot.accounts.iter().all(|a| a.account != FUND));

    // The reference engine replays the same journal into the same events.
    let reference =
        replay_as::<ReferenceBook, Naive>(&dir.join("journal"), &recovered, None, true).expect("replayed");
    let live = output.capture.as_deref().expect("capture was on");
    assert_eq!(first_difference(live, reference.capture.as_deref().expect("captured")), None);
    std::fs::remove_dir_all(&dir).expect("cleaned up");
}

#[test]
fn a_signed_run_journals_the_signatures_and_replays_identically() {
    let dir = run_dir("signed");
    let clock = RunClock::start();
    let mut config = config(&dir, InjectionMode::Signed, JournalMode::Disk, Stamps::On);
    let log_path = dir.join("release.log");
    config.release_log = Some(std::fs::File::create(&log_path).expect("created"));
    let mut run = Run::start(config, clock, None, vec![0; common::ACCOUNTS as usize + 1]);
    let mut flow = TestFlow::new(SEED + 1);
    run_flow(&mut run, &mut flow, 2_000);
    run.wait_released(1);
    let sent = run.sent;
    let (output, _) = run.stop();
    assert_eq!(output.stats.commands, sent);
    let gateway_samples: u64 = output.stats.per_gateway.iter().map(|g| g.verification.count()).sum();
    assert_eq!(gateway_samples, output.stats.client.get(Stage::Verification).count());
    assert!(gateway_samples > 0);

    let journal = dir.join("journal");
    let recovered = recover(&journal, &identity(InjectionMode::Signed), false).expect("recovered");
    assert_eq!(recovered.registry_digests, vec![[0x5A; 32]; recovered.headers.len()]);
    assert_replays_identically(&dir, &recovered, &output, 1);

    // Every client record is kind 1 and kept its signature and expiry.
    let mut files = StdFiles::open_existing(&journal, 0).expect("opened");
    let mut signed = 0;
    scan_journal(&mut files, |_, record, _| {
        if record.meta.source == Source::SignedClient {
            assert_eq!(record.expires_at, u64::MAX);
            assert_eq!(record.signature, [record.nonce ^ 0xABCD; SIGNATURE_WORDS]);
            signed += 1;
        }
    })
    .expect("read");
    assert_eq!(signed, output.stats.setup.client_commands() + output.stats.window.client_commands());

    // The release log holds every released event slot, in order: the capture.
    let log = std::fs::read(&log_path).expect("read");
    let words: Vec<u64> = log.as_chunks::<8>().0.iter().map(|chunk| u64::from_le_bytes(*chunk)).collect();
    assert_eq!(words, output.capture.expect("capture was on"));
    std::fs::remove_dir_all(&dir).expect("cleaned up");
}

#[test]
fn a_restart_resumes_from_the_recovered_journal_in_a_new_segment() {
    let dir = run_dir("restart");
    let journal = dir.join("journal");
    let mut flow = TestFlow::new(SEED + 2);

    // Life 1.
    let clock = RunClock::start();
    let mut run = Run::start(
        config(&dir, InjectionMode::PreVerified, JournalMode::Disk, Stamps::On),
        clock,
        None,
        vec![0; common::ACCOUNTS as usize + 1],
    );
    run_flow(&mut run, &mut flow, 1_500);
    run.wait_released(1);
    let (first, nonces) = run.stop();

    // Restart (13.5): recover, replay, resume.
    let recovered = recover(&journal, &identity(InjectionMode::PreVerified), false).expect("recovered");
    assert_eq!(recovered.records, first.sequencer.records);
    let replayed = replay(&journal, &recovered, None, false).expect("replayed");
    assert_eq!(replayed.engine.snapshot(), first.engine.snapshot(), "the journal rebuilt life 1");
    let (first_seq, last_ts) = (replayed.next_seq, replayed.last_ts);
    let resume = Resume {
        engine: replayed.engine,
        next_seq: replayed.next_seq,
        last_ts: replayed.last_ts,
        end: recovered.end,
    };
    let clock = RunClock::resume(replayed.last_ts);
    let mut run = Run::start(
        config(&dir, InjectionMode::PreVerified, JournalMode::Disk, Stamps::On),
        clock,
        Some(resume),
        nonces,
    );
    let commands: Vec<Command> = (0..1_500).map(|_| flow.next_command()).collect();
    run.send_all(&commands);
    run.wait_released(first_seq);
    let (second, _) = run.stop();
    assert_eq!(second.sequencer.records, 1_500);

    // The second life started a new segment, anchored above the first life's last ts, and
    // replaying both lives rebuilds the second life's engine and events.
    let recovered = recover(&journal, &identity(InjectionMode::PreVerified), false).expect("recovered");
    assert_eq!(recovered.records, first.sequencer.records + 1_500);
    let new_life = recovered
        .headers
        .iter()
        .find(|h| h.first_seq == first_seq)
        .expect("a segment starts the second life");
    assert!(new_life.run_start_unix_ns > last_ts, "the clock anchor is above the first life's last ts");
    assert_replays_identically(&dir, &recovered, &second, first_seq);
    std::fs::remove_dir_all(&dir).expect("cleaned up");
}

#[test]
fn discard_mode_releases_everything_and_the_replay_test_says_not_checked() {
    let dir = run_dir("discard");
    let clock = RunClock::start();
    let mut run = Run::start(
        config(&dir, InjectionMode::PreVerified, JournalMode::Discard, Stamps::Off),
        clock,
        None,
        vec![0; common::ACCOUNTS as usize + 1],
    );
    let mut flow = TestFlow::new(SEED + 3);
    run_flow(&mut run, &mut flow, 1_000);
    run.wait_released(1);
    let sent = run.sent;
    let (output, _) = run.stop();
    assert_eq!(output.stats.commands, sent);
    assert!(!dir.join("journal").exists(), "nothing written");
    assert!(
        Stage::ALL.iter().all(|&stage| output.stats.client.get(stage).count() == 0),
        "stamps off: no latencies"
    );
    let snapshot = output.engine.snapshot();
    let live = LiveRun {
        snapshot: &snapshot,
        capture: output.capture.as_deref(),
        capture_incomplete: false,
        journal_discarded: true,
        first_seq: 1,
        sequenced: output.sequencer.records,
        applied: output.core.commands,
    };
    let empty = Recovered {
        identity: identity(InjectionMode::PreVerified),
        end: pipeline::journal::JournalPosition { segment: 0, offset: 0 },
        records: 0,
        next_seq: 1,
        last_ts: 0,
        registry_digests: Vec::new(),
        headers: Vec::new(),
        torn_copies: Vec::new(),
    };
    let verdict = replay_test(&dir.join("journal"), &empty, &live, 1).expect("not run");
    assert!(matches!(verdict, ReplayVerdict::NotChecked(_)), "{verdict:?}");

    // The capture written out as events.bin reads back.
    let path = dir.join("events.bin");
    let capture = output.capture.expect("capture was on");
    let header = EventsHeader { deployment: 7, first_seq: 1, last_seq: sent, incomplete: false };
    pipeline::gate::write_events_file(&path, &header, &capture).expect("written");
    assert_eq!(read_events_file(&path).expect("read"), (header, capture));
    std::fs::remove_dir_all(&dir).expect("cleaned up");
}
