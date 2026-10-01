//! The values that pipeline threads share outside the rings, all single-writer
//! (`docs/PIPELINE.md` 3.2, 11.7 and 15.4): the counters main reads while a run is going,
//! the durable watermark, and what each hot thread publishes about its own health.
//!
//! **Contract.** Each shared value is one `AtomicU64` alone on its own 128 bytes
//! ([`CachePadded`]), written by exactly one thread. So a write never has to wait for
//! another writer, and a reader never slows down any thread but the writer, and only when
//! it reads.
//!
//! - [`SharedCounter`]: written and read Relaxed. Relaxed is enough because the counts only
//!   grow and main uses them only for progress, barriers and sampling at the window's
//!   edges: a barrier waits until the counts add up to a known total, which a stale read
//!   can only delay, never fake (every stale value is at or below the true one).
//! - [`Watermark`]: the journal writer stores it with Release after `fdatasync` returns,
//!   the gate loads it with Acquire. Release/Acquire means: once the gate sees a new value,
//!   the flush that it stands for really has happened (11.7).
//!
//! **Thread health** (15.4). Each spinning thread publishes its busy time (the time spent
//! in loop passes that found work, measured by a [`BusyMeter`]) and its OS thread id, so
//! main can read its minor page faults from `/proc` at the window's edges. A fault there
//! means a page was touched for the first time inside the window: a ring or buffer that
//! wasn't pre-touched, or an allocation. Or it means the kernel was moving one of the
//! thread's pages just then: memory compaction migrates pages that are already mapped,
//! and a thread that touches one mid-move waits in a minor fault. So main also reads the
//! machine's count of page migrations ([`page_migrations`]) at both edges: faults in a
//! window without migrations are the code's. Busy time is CPU work: the journal writer
//! leaves out the time it spends blocked in `fdatasync`, which it publishes on its own,
//! with its count of flushes, so main can tell a busy writer from a slow disk and count
//! flushes over the window.
//!
//! **The hot-thread hook** ([`set_hot_thread_hook`]). Every hot thread (the sender, the
//! gateways, the sequencer, the journal writer, the core and the gate) calls
//! [`ThreadCounters::publish_tid`] when it starts. A test binary may register one function
//! that runs there, on the thread itself: the smoke test's counting allocator uses it to
//! flag the threads whose allocations it counts (18.4). Nothing else sets it.
//!
//! **Complexity.** Every operation is one or two plain loads or stores; reading the page
//! faults, or the page migrations, reads one small file.

use std::io;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ring::CachePadded;

/// A count that one thread writes and other threads read. See the module docs.
#[derive(Debug, Default)]
pub struct SharedCounter(CachePadded<AtomicU64>);

impl SharedCounter {
    pub const fn new() -> Self {
        SharedCounter(CachePadded::new(AtomicU64::new(0)))
    }

    /// Stores the writer's new total (Relaxed). Only the owning thread may call this.
    pub fn store(&self, value: u64) {
        self.0.store(value, Ordering::Relaxed);
    }

    /// Adds `n` (Relaxed). Only the owning thread may call this, so a plain load and store
    /// is enough: no other thread writes in between, and it avoids the locked instruction
    /// of `fetch_add`.
    pub fn add(&self, n: u64) {
        self.store(self.load() + n);
    }

    /// The latest value this thread has seen (Relaxed): at or below the true one.
    pub fn load(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// The durable watermark (11.7): every command with `seq <= durable` is on disk. Written
/// only by the journal writer, after each successful `fdatasync`; read by the gate.
#[derive(Debug)]
pub struct Watermark(CachePadded<AtomicU64>);

impl Watermark {
    /// Starts at 0 for a new journal, or at the last recovered seq after a restart.
    pub const fn new(start: u64) -> Self {
        Watermark(CachePadded::new(AtomicU64::new(start)))
    }

    /// Moves the watermark to `seq` (Release). Only the journal writer may call this, with
    /// the last seq of a batch whose `fdatasync` returned. It never moves back.
    pub fn publish(&self, seq: u64) {
        debug_assert!(seq >= self.0.load(Ordering::Relaxed), "the watermark never moves back");
        self.0.store(seq, Ordering::Release);
    }

    /// The watermark (Acquire): what it covers is on disk.
    pub fn load(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// A thread's busy time (2.2): the time spent in loop passes that found work, at the cost
/// of one extra clock read per non-empty batch. The thread keeps the meter and publishes its
/// total to a [`SharedCounter`] at the end of every busy stretch.
#[derive(Debug, Default)]
pub struct BusyMeter {
    total_ns: u64,
    since: u64,
}

impl BusyMeter {
    pub fn new() -> Self {
        BusyMeter::default()
    }

    /// A busy stretch starts at run time `now` (the batch's first clock read).
    pub fn begin(&mut self, now: u64) {
        self.since = now;
    }

    /// The stretch ends at run time `now`: adds it to the total and publishes the total.
    pub fn end(&mut self, now: u64, published: &SharedCounter) {
        self.end_except(now, 0, published);
    }

    /// The stretch ends at run time `now`, but `blocked_ns` of it was spent waiting in the
    /// kernel (the journal writer's `fdatasync`), which is not CPU work: adds the rest.
    pub fn end_except(&mut self, now: u64, blocked_ns: u64, published: &SharedCounter) {
        self.total_ns += now.saturating_sub(self.since).saturating_sub(blocked_ns);
        published.store(self.total_ns);
    }

    /// Busy nanoseconds so far.
    pub fn total_ns(&self) -> u64 {
        self.total_ns
    }
}

/// What one spinning thread publishes about itself (15.4).
#[derive(Debug, Default)]
pub struct ThreadCounters {
    /// Nanoseconds spent in passes that found work ([`BusyMeter`]).
    pub busy_ns: SharedCounter,
    /// The thread's OS thread id, or 0 until the thread has published it.
    pub tid: SharedCounter,
}

impl ThreadCounters {
    pub const fn new() -> Self {
        ThreadCounters { busy_ns: SharedCounter::new(), tid: SharedCounter::new() }
    }

    /// Called by the thread itself when it starts: publishes its OS thread id, then runs
    /// the hot-thread hook, if a test binary set one (module docs).
    pub fn publish_tid(&self) -> io::Result<()> {
        let tid = current_tid();
        if let Some(hook) = HOT_THREAD_HOOK.get() {
            hook();
        }
        self.tid.store(u64::from(tid?));
        Ok(())
    }

    /// The thread's busy time and, if it has published its id, its minor page faults so
    /// far. Called by main.
    pub fn sample(&self) -> ThreadSample {
        let tid = self.tid.load();
        let minor_faults = if tid == 0 { None } else { minor_faults(tid as u32).ok() };
        ThreadSample { busy_ns: self.busy_ns.load(), minor_faults }
    }
}

/// One thread's health at one moment. Two samples, at the window's edges, give its busy
/// share and the page faults it took inside the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadSample {
    pub busy_ns: u64,
    /// `None` if the thread hasn't published its id, or its `/proc` entry is gone (the
    /// thread ended).
    pub minor_faults: Option<u64>,
}

/// The pipeline's own shared counters (3.2): the four threads inside the pipeline, the
/// commands released, the core's event-ring stall time, the sequencer's full-ring passes
/// (per ring), and the journal writer's time in `fdatasync` and its flushes. (The gateways'
/// rejects live with those threads, in their own crate, as [`SharedCounter`]s too.)
#[derive(Debug, Default)]
pub struct PipelineCounters {
    pub sequencer: ThreadCounters,
    pub journal: ThreadCounters,
    pub core: ThreadCounters,
    pub gate: ThreadCounters,
    /// Commands released by the gate: main's barriers wait on it (14.4).
    pub released: SharedCounter,
    /// Nanoseconds the core spent waiting for space in the event ring (10.2).
    pub core_stall_ns: SharedCounter,
    /// Sequencer passes in which the core ring had less room than the records waiting (9.1):
    /// the core was behind.
    pub sequencer_core_full_passes: SharedCounter,
    /// The same for the journal ring: the journal writer, or the disk, was behind.
    pub sequencer_journal_full_passes: SharedCounter,
    /// Nanoseconds the journal writer spent blocked in `fdatasync` (11.6), which its busy
    /// time leaves out (15.4).
    pub journal_sync_ns: SharedCounter,
    /// The journal writer's flushes so far (15.8).
    pub journal_flushes: SharedCounter,
    /// What the released events said so far, for `e2e run --watch`; written by the gate
    /// only when the run asks for it ([`LiveCounts`]).
    pub live: LiveCounts,
}

/// The released engine events counted as the run goes, over the whole run, for the live
/// panel of `e2e run --watch`. The gate keeps these totals only when the run asks for them
/// (`GateConfig::live`): it adds each command's counts as it releases the command, and
/// stores the totals at the end of every pass that released something. Display only:
/// nothing waits on them.
#[derive(Debug, Default)]
pub struct LiveCounts {
    pub events: SharedCounter,
    pub fills: SharedCounter,
    /// Sum of `price × qty` over fills, in micro-dollars.
    pub fill_notional: SharedCounter,
    pub cancels: SharedCounter,
    pub modifies: SharedCounter,
    pub marks: SharedCounter,
    pub liquidations: SharedCounter,
}

impl LiveCounts {
    pub const fn new() -> Self {
        LiveCounts {
            events: SharedCounter::new(),
            fills: SharedCounter::new(),
            fill_notional: SharedCounter::new(),
            cancels: SharedCounter::new(),
            modifies: SharedCounter::new(),
            marks: SharedCounter::new(),
            liquidations: SharedCounter::new(),
        }
    }
}

impl PipelineCounters {
    pub const fn new() -> Self {
        PipelineCounters {
            sequencer: ThreadCounters::new(),
            journal: ThreadCounters::new(),
            core: ThreadCounters::new(),
            gate: ThreadCounters::new(),
            released: SharedCounter::new(),
            core_stall_ns: SharedCounter::new(),
            sequencer_core_full_passes: SharedCounter::new(),
            sequencer_journal_full_passes: SharedCounter::new(),
            journal_sync_ns: SharedCounter::new(),
            journal_flushes: SharedCounter::new(),
            live: LiveCounts::new(),
        }
    }

    /// Everything at one moment, for main (15.4).
    pub fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            sequencer: self.sequencer.sample(),
            journal: self.journal.sample(),
            core: self.core.sample(),
            gate: self.gate.sample(),
            released: self.released.load(),
            core_stall_ns: self.core_stall_ns.load(),
            sequencer_core_full_passes: self.sequencer_core_full_passes.load(),
            sequencer_journal_full_passes: self.sequencer_journal_full_passes.load(),
            journal_sync_ns: self.journal_sync_ns.load(),
            journal_flushes: self.journal_flushes.load(),
        }
    }
}

/// [`PipelineCounters`] at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CounterSnapshot {
    pub sequencer: ThreadSample,
    pub journal: ThreadSample,
    pub core: ThreadSample,
    pub gate: ThreadSample,
    pub released: u64,
    pub core_stall_ns: u64,
    pub sequencer_core_full_passes: u64,
    pub sequencer_journal_full_passes: u64,
    pub journal_sync_ns: u64,
    pub journal_flushes: u64,
}

/// The function every hot thread runs when it starts, if one was set (module docs).
static HOT_THREAD_HOOK: OnceLock<fn()> = OnceLock::new();

/// Registers `hook`, to run on every hot thread when it starts (module docs). For test
/// binaries; it can be set once per process, and a second call is ignored.
pub fn set_hot_thread_hook(hook: fn()) {
    let _ = HOT_THREAD_HOOK.set(hook);
}

/// The calling thread's OS thread id, read from the `/proc/thread-self` link, which points
/// at `<pid>/task/<tid>`.
pub fn current_tid() -> io::Result<u32> {
    let link = std::fs::read_link("/proc/thread-self")?;
    let tid = link.file_name().and_then(|name| name.to_str()).and_then(|name| name.parse().ok());
    tid.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("unexpected /proc/thread-self link {link:?}"))
    })
}

/// Minor page faults of thread `tid` of this process so far: field 10 of
/// `/proc/self/task/<tid>/stat`.
pub fn minor_faults(tid: u32) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat"))?;
    parse_minor_faults(&stat)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("unexpected stat line {stat:?}")))
}

/// Field 10 (`minflt`) of a `/proc/.../stat` line. Field 2 is the thread's name in
/// parentheses, which may itself contain spaces and parentheses, so the fields are counted
/// from after the *last* `)`: field 3 is the first one there, so field 10 is the eighth.
fn parse_minor_faults(stat: &str) -> Option<u64> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(7)?.parse().ok()
}

/// Pages the kernel has migrated, or tried to, on the whole machine since it booted:
/// `pgmigrate_success` plus `pgmigrate_fail` in `/proc/vmstat` (module docs). An error if
/// the kernel doesn't count them (built without page migration).
pub fn page_migrations() -> io::Result<u64> {
    let vmstat = std::fs::read_to_string("/proc/vmstat")?;
    parse_page_migrations(&vmstat).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "no pgmigrate_success line in /proc/vmstat")
    })
}

/// `pgmigrate_success` plus `pgmigrate_fail` from `/proc/vmstat`'s `name value` lines;
/// `None` without a `pgmigrate_success` line.
fn parse_page_migrations(vmstat: &str) -> Option<u64> {
    let value = |name: &str| {
        vmstat.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')?.trim().parse::<u64>().ok())
    };
    Some(value("pgmigrate_success")? + value("pgmigrate_fail").unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn each_shared_value_is_alone_on_128_bytes() {
        assert_eq!(size_of::<SharedCounter>(), 128);
        assert_eq!(size_of::<Watermark>(), 128);
        assert_eq!(align_of::<ThreadCounters>(), 128);
    }

    #[test]
    fn a_counter_adds_and_stores() {
        let counter = SharedCounter::new();
        counter.add(3);
        counter.add(4);
        assert_eq!(counter.load(), 7);
        counter.store(100);
        assert_eq!(counter.load(), 100);
    }

    #[test]
    fn a_busy_meter_adds_up_busy_stretches_and_publishes_the_total() {
        let published = SharedCounter::new();
        let mut meter = BusyMeter::new();
        meter.begin(1_000);
        meter.end(1_500, &published);
        assert_eq!(published.load(), 500);
        meter.begin(10_000);
        meter.end(10_250, &published);
        assert_eq!((meter.total_ns(), published.load()), (750, 750));
        meter.begin(20_000);
        meter.end_except(21_000, 900, &published);
        assert_eq!(published.load(), 850, "900 ns of the 1,000 were blocked, not busy");
    }

    #[test]
    fn the_watermark_starts_where_it_is_told() {
        let watermark = Watermark::new(41);
        assert_eq!(watermark.load(), 41);
        watermark.publish(41);
        watermark.publish(5_000);
        assert_eq!(watermark.load(), 5_000);
    }

    #[test]
    fn minor_faults_are_field_10_even_when_the_name_has_spaces_and_parentheses() {
        let line = "1234 (core (x) 1) R 1 1234 1234 0 -1 4194304 177 0 0 0 5 3 0 0 20 0 1 0 100 1000 50";
        assert_eq!(parse_minor_faults(line), Some(177));
        assert_eq!(parse_minor_faults("1234 (short) R 1"), None);
        assert_eq!(parse_minor_faults("no parenthesis"), None);
    }

    #[test]
    fn page_migrations_add_the_successes_and_the_failures() {
        let vmstat =
            "pgfault 900\npgmigrate_success 102638375\npgmigrate_fail 3686736\npgmigrate_success_x 1\n";
        assert_eq!(parse_page_migrations(vmstat), Some(102_638_375 + 3_686_736));
        assert_eq!(parse_page_migrations("pgmigrate_success 5\n"), Some(5), "no failures counted");
        assert_eq!(parse_page_migrations("pgfault 900\n"), None, "a kernel without migration");
    }

    #[test]
    fn a_thread_publishes_its_id_and_main_reads_its_faults() {
        let counters = Arc::new(ThreadCounters::new());
        assert_eq!(counters.sample().minor_faults, None, "no id published yet");
        let (sender, receiver) = std::sync::mpsc::channel();
        let (done_sender, done_receiver) = std::sync::mpsc::channel::<()>();
        let thread_counters = Arc::clone(&counters);
        let thread = std::thread::spawn(move || {
            thread_counters.publish_tid().expect("/proc/thread-self is readable");
            sender.send(current_tid().expect("a tid")).expect("main is waiting");
            // Stay alive until main has sampled, so /proc still lists this thread.
            done_receiver.recv().expect("main says when");
        });
        let tid = receiver.recv().expect("the thread sends its id");
        assert_eq!(counters.tid.load(), u64::from(tid));
        assert_ne!(tid, current_tid().expect("main's own id"), "each thread has its own id");
        assert!(counters.sample().minor_faults.is_some(), "the thread's stat file is readable");
        done_sender.send(()).expect("the thread is waiting");
        thread.join().expect("the thread ends cleanly");
    }

    #[test]
    fn a_snapshot_reads_every_counter() {
        let counters = PipelineCounters::new();
        counters.released.store(12);
        counters.core_stall_ns.store(34);
        counters.sequencer_core_full_passes.store(56);
        counters.sequencer_journal_full_passes.store(57);
        counters.journal_sync_ns.store(58);
        counters.journal_flushes.store(59);
        counters.core.busy_ns.store(78);
        let snapshot = counters.snapshot();
        assert_eq!((snapshot.released, snapshot.core_stall_ns), (12, 34));
        assert_eq!((snapshot.sequencer_core_full_passes, snapshot.sequencer_journal_full_passes), (56, 57));
        assert_eq!((snapshot.journal_sync_ns, snapshot.journal_flushes), (58, 59));
        assert_eq!(snapshot.core, ThreadSample { busy_ns: 78, minor_faults: None });
    }
}
