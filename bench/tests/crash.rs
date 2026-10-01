//! The kill test of `docs/PIPELINE.md` 18.4: a process crash at a random moment, recovery,
//! replay, a restart, another crash, a clean stop, and a restart with nothing left to send;
//! across all of them, nothing a client saw is lost and nothing is applied twice.
//!
//! **How.** The test starts the `e2e` binary as a child process on the signed smoke run
//! with `--release-log`: the gate also writes every released event slot (64 bytes) to its
//! standard output, which the test reads. At a random moment (a random number of released
//! slots) the test kills the child with SIGKILL: a process crash, so the page cache
//! survives. Then:
//! 1. it runs recovery and replay on the run directory itself, and checks that **the
//!    released events are a prefix of the replayed event stream**, slot for slot: every
//!    event a client saw is in the journal, in the order it was seen;
//! 2. it restarts the child with `--resume` (13.5: recovery, replay, the nonce table, the
//!    next seq, the clock anchored above the last `ts`, a new segment), which sends the rest
//!    of the flow; kills it again at a random moment inside that rest, and checks the second
//!    life's released events the same way, against the replayed stream from that life's
//!    first seq;
//! 3. it restarts once more and lets the run finish: the child's own replay test (13.3)
//!    must say "identical" for that life, its signature audit (13.4) must pass over the
//!    whole journal, and every event it released must be exactly the replayed stream from
//!    its first seq. The audit checks that nonces strictly increase per account over the
//!    whole journal, so no message was applied twice across the restarts. The standalone
//!    `e2e replay` then rebuilds all three lives from the journal and must find the last
//!    life's `events.bin` and the live engine's final state identical.
//! 4. Last, it restarts after that clean stop. Nothing is left to send (only a client's
//!    last message, had a full ingress ring dropped it), so the fourth life issues no
//!    command and releases nothing; it must pass the same checks as the third, and `e2e
//!    replay` must still say identical. Its `events.bin` is empty and its header names the
//!    seq the life started at (13.2): `e2e replay` once took an empty file to start at seq 1
//!    and called it different from the whole replay.
//!
//! **Where the kills fall.** The whole flow releases about 11,280 slots ([`FLOW_SLOTS`]).
//! Life 1 is killed within its first 6,000, and life 2 at most halfway through the slots
//! the journal is still missing, so both kills fall inside the flow with room to spare: a
//! kill point the test notices late, on a loaded machine, still finds the child running.
//! Were a child to finish first all the same, the test says so and goes on: its released
//! events are checked the same way, and the next life has nothing to send, as in step 4.
//!
//! The kill points come from a seed printed at the start, so a failure can be repeated.
//! The children's progress goes to `<run directory>-life-<n>.log` next to the run
//! directory; a failed check names the file.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bench::e2e::config::RunConfig;
use bench::e2e::summary::Summary;
use pipeline::journal::recovery::recover;
use pipeline::records::{EVENT_SLOT_WORDS, InjectionMode};
use pipeline::replay::{first_difference, replay};

const E2E: &str = env!("CARGO_BIN_EXE_e2e");
/// Bytes in one released event slot.
const SLOT_BYTES: usize = EVENT_SLOT_WORDS * 8;
/// Event slots the signed smoke flow releases in all, rounded down: its 3,389 commands
/// released 11,269 to 11,286 in six runs (the order in which the two gateways' messages
/// are sequenced changes a few events).
const FLOW_SLOTS: usize = 11_200;

/// A tiny deterministic generator (xorshift64), for the kill points.
struct XorShift(u64);

impl XorShift {
    fn between(&mut self, low: usize, high: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        low + (self.0 % (high - low + 1) as u64) as usize
    }
}

/// A running child and the thread that reads its release log.
struct Life {
    child: Child,
    reader: JoinHandle<Vec<u8>>,
    /// Bytes of release log read so far.
    read: Arc<AtomicUsize>,
    log: PathBuf,
}

impl Life {
    /// Starts life `n` of the run in `dir`; every life after the first resumes.
    fn start(dir: &Path, n: usize) -> Life {
        let log =
            dir.with_file_name(format!("{}-life-{n}.log", dir.file_name().expect("named").to_string_lossy()));
        let mut command = Command::new(E2E);
        command.args(["run", "--smoke", "--mode", "signed", "--release-log", "--run-dir"]).arg(dir);
        if n > 1 {
            command.arg("--resume");
        }
        let stderr = std::fs::File::create(&log).expect("a log file");
        let mut child = command.stdout(Stdio::piped()).stderr(stderr).spawn().expect("the e2e binary starts");
        let mut stdout = child.stdout.take().expect("piped");
        let read = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&read);
        let reader = std::thread::spawn(move || {
            let (mut all, mut chunk) = (Vec::new(), [0u8; 1 << 16]);
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => return all,
                    Ok(n) => {
                        all.extend_from_slice(&chunk[..n]);
                        counter.store(all.len(), Ordering::Relaxed);
                    }
                }
            }
        });
        Life { child, reader, read, log }
    }

    /// Kills the child with SIGKILL once it has released `slots` event slots (or lets it
    /// end if it finishes first), and returns the released slots as words.
    fn kill_after(mut self, slots: usize) -> Vec<u64> {
        let deadline = Instant::now() + Duration::from_secs(120);
        while self.read.load(Ordering::Relaxed) < slots * SLOT_BYTES {
            if self.child.try_wait().expect("waited").is_some() {
                eprintln!("the child finished before {slots} slots were released");
                break;
            }
            assert!(Instant::now() < deadline, "no progress; see {}", self.log.display());
            std::thread::sleep(Duration::from_micros(200));
        }
        let _ = self.child.kill(); // SIGKILL: a process crash
        self.child.wait().expect("reaped");
        self.slots()
    }

    /// Waits for the child to finish by itself, and returns the released slots.
    fn finish(mut self) -> Vec<u64> {
        let status = self.child.wait().expect("reaped");
        assert!(status.success(), "the last life failed ({status}); see {}", self.log.display());
        self.slots()
    }

    /// Every whole slot read (a write cut by the kill may leave part of one).
    fn slots(self) -> Vec<u64> {
        let bytes = self.reader.join().expect("the reader ends");
        let whole = bytes.len() / SLOT_BYTES * SLOT_BYTES;
        bytes[..whole].as_chunks::<8>().0.iter().map(|chunk| u64::from_le_bytes(*chunk)).collect()
    }
}

/// Where the journal ends after a life.
struct JournalEnd {
    /// The seq the next life starts at.
    next_seq: u64,
    /// Event slots the journal's commands emit on replay, every life's so far.
    slots: usize,
}

/// Recovers and replays the journal in `dir` (as the next life will), and checks that the
/// slots `released` by the life that started at `first_seq` are a prefix of the replayed
/// stream from that seq.
fn check_prefix(dir: &Path, released: &[u64], first_seq: u64, life: usize) -> JournalEnd {
    let journal = dir.join("journal");
    let identity = RunConfig::smoke(InjectionMode::Signed).identity();
    let recovered = recover(&journal, &identity, false).expect("recovery succeeds after a kill");
    let replayed = replay(&journal, &recovered, None, true).expect("replayed");
    let events = replayed.capture.expect("captured");
    let slots = events.as_chunks::<EVENT_SLOT_WORDS>().0;
    let from = slots.iter().position(|slot| slot[0] >= first_seq).unwrap_or(slots.len());
    let this_life = &events[from * EVENT_SLOT_WORDS..];
    let released_slots = released.len() / EVENT_SLOT_WORDS;
    eprintln!(
        "life {life}: {released_slots} slots released, {} replayed from seq {first_seq}; the journal holds {} records",
        this_life.len() / EVENT_SLOT_WORDS,
        recovered.records
    );
    assert!(released.len() <= this_life.len(), "life {life}: more released than the journal holds");
    assert_eq!(
        first_difference(released, &this_life[..released.len()]),
        None,
        "life {life}: the released events are not a prefix of the replayed stream"
    );
    JournalEnd { next_seq: recovered.next_seq, slots: slots.len() }
}

/// Checks a life that ran to the end, started at `first_seq`, from its `summary.txt`: its
/// own replay test and audit passed, its counts add up, every command it issued is in the
/// journal, and it released exactly the replayed events of those commands. Returns the seq
/// the next life starts at.
fn check_finished_life(dir: &Path, released: &[u64], first_seq: u64, life: usize) -> u64 {
    let summary = Summary::read(&dir.join("summary.txt")).expect("the life's summary");
    assert_eq!(summary.u64("run.first_seq"), Some(first_seq), "life {life} started where the journal ended");
    assert_eq!(
        summary.get("replay.verdict"),
        Some("identical"),
        "life {life}: {}",
        summary.get("replay.detail").unwrap_or("-")
    );
    assert_eq!(
        summary.flag("audit.passed"),
        Some(true),
        "life {life}: {}",
        summary.get("audit.first_failure").unwrap_or("-")
    );
    assert!(
        !summary.get("check.invalid").unwrap_or("").contains("don't add up"),
        "life {life}: {}",
        summary.get("check.invalid").unwrap_or("-")
    );
    let end = check_prefix(dir, released, first_seq, life).next_seq;
    assert_eq!(
        end,
        first_seq + summary.u64("counts.sequenced").expect("counted"),
        "every command of life {life} is there"
    );
    let released_slots = (released.len() / EVENT_SLOT_WORDS) as u64;
    assert_eq!(
        summary.u64("counts.events"),
        Some(released_slots),
        "life {life}: a clean stop releases everything"
    );
    end
}

/// The standalone `e2e replay` (13.3) rebuilds every life so far from the journal, and must
/// find the last life's `events.bin` and the live engine's final state identical.
fn check_standalone_replay(dir: &Path, after_life: usize) {
    let replayed =
        Command::new(E2E).args(["replay", "--run-dir"]).arg(dir).output().expect("e2e replay runs");
    let text = String::from_utf8_lossy(&replayed.stdout);
    let context = format!("after life {after_life}: {text}{}", String::from_utf8_lossy(&replayed.stderr));
    assert!(replayed.status.success(), "{context}");
    assert!(text.contains("replay.events_vs_live = identical"), "{context}");
    assert!(text.contains("replay.state_vs_live = identical"), "{context}");
}

#[test]
fn a_killed_run_restarts_from_its_journal_and_loses_nothing_it_released() {
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).expect("after 1970").as_nanos() as u64 | 1;
    eprintln!("kill test seed {seed}");
    let mut random = XorShift(seed);
    let dir = std::env::temp_dir().join(format!("bench-kill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // Life 1: killed somewhere in setup or the timed flow, within its first 6,000 slots of
    // about 11,280.
    let released = Life::start(&dir, 1).kill_after(random.between(300, 6_000));
    let after_1 = check_prefix(&dir, &released, 1, 1);

    // Life 2: restarted on the journal, killed again, at most halfway through the slots the
    // journal is still missing, so that the kill falls inside the rest of the flow (module
    // docs).
    let missing = FLOW_SLOTS.saturating_sub(after_1.slots);
    let released = Life::start(&dir, 2).kill_after(random.between(100, (missing / 2).max(100)));
    let after_2 = check_prefix(&dir, &released, after_1.next_seq, 2);
    assert!(after_2.next_seq >= after_1.next_seq);

    // Life 3: runs to the end.
    let released = Life::start(&dir, 3).finish();
    let end = check_finished_life(&dir, &released, after_2.next_seq, 3);
    check_standalone_replay(&dir, 3);

    // Life 4: a restart after the clean stop, with nothing left to send (module docs).
    let released = Life::start(&dir, 4).finish();
    let after_4 = check_finished_life(&dir, &released, end, 4);
    eprintln!("life 4 issued {} commands", after_4 - end);
    check_standalone_replay(&dir, 4);

    std::fs::remove_dir_all(&dir).expect("cleaned up");
    for n in 1..=4 {
        let log = dir.with_file_name(format!("bench-kill-{}-life-{n}.log", std::process::id()));
        let _ = std::fs::remove_file(log);
    }
}
