//! What every run checks about itself (`docs/PIPELINE.md` 15.4, 15.7 and 2.6): the hot
//! threads' health over the measured window, which thread was the limit, and whether a run
//! passes, fails or is invalid.
//!
//! **Thread health** (15.4). Main samples every hot thread's single-writer counters at the
//! window's two edges ([`Watched::sample`]). Two samples give each thread's **busy share**
//! (its CPU work over wall time; the journal writer's leaves out the time it was blocked in
//! `fdatasync`, which is its own **fdatasync share**), its **minor page faults** inside the
//! window, which must be 0 (a fault means a page was touched for the first time inside the
//! window: a ring or buffer that wasn't pre-touched, or an allocation), and the journal's
//! **flushes per second** over the window. Main also reads the machine's count of **page
//! migrations** at both edges: memory compaction moves pages that are already mapped, and a
//! thread that touches one mid-move takes a minor fault that no code of ours caused
//! (`pipeline::counters`). So a fault in a window without migrations is the code's; in a
//! window with them it may be the kernel's, and the run's flag says how many there were. The
//! migrations are read outside the threads' faults at both edges ([`Edge`]): before them at
//! the open, after them at the close, so a migration whose fault counts inside the window
//! counts inside it too. (The kernel adds a batch of migrations to its count once the whole
//! batch is done, so a batch straddling the close can still be missed; a window of
//! microseconds.)
//!
//! **What limited the run** ([`Limit`], 15.4), in this order:
//! 1. **Back-pressure from durability.** The sequencer found the journal ring full, or the
//!    core waited for the event ring while the gate (which frees it only as the watermark
//!    moves) was not saturated: then the journal is behind. If the writer spent more of
//!    the window in `fdatasync` than on the CPU, the label is "limited by the disk
//!    (fdatasync)", else "limited by journal writer".
//! 2. The core waited for the event ring while the gate was saturated: "limited by gate".
//! 3. The sequencer found the core ring full: "core-limited".
//! 4. No back-pressure: the busiest thread, if its busy share is at least 90%
//!    ("core-limited" if it is the core); if no thread is that busy, "not saturated
//!    (busiest: X at Y%)". So a label never blames a thread that had room to spare, nor
//!    the journal writer for the time the disk takes (review finding
//!    F1-limit-label-fdatasync).
//!
//! **Pass, fail or invalid** ([`judge`], 15.7). A run with CFS throttling, a clock inversion,
//! a generator-limited sender, or counts that don't add up is **invalid**: it says nothing
//! about the pipeline, and a search runs it again. A valid run **passes** a latency limit if
//! the stage's p99 over offered commands is under the limit and at least 99.9% of the
//! offered client commands were sequenced; otherwise it **fails**. A run whose backlog
//! didn't drain within the ablation's cap fails too (section 16).
//!
//! **Complexity.** Sampling reads one small `/proc` file per thread.

use std::sync::Arc;

use gateway::GatewayCounters;
use loadgen::sender::SenderCounters;
use pipeline::counters::{PipelineCounters, ThreadSample, page_migrations};

use super::summary::Summary;
use super::units::{latency, percent_ppm, ppm};

/// Every hot thread's counters, as main reads them.
#[derive(Clone, Debug)]
pub struct Watched {
    pub pipeline: Arc<PipelineCounters>,
    pub gateways: Vec<Arc<GatewayCounters>>,
    pub sender: Arc<SenderCounters>,
}

/// The hot threads' counters at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Run-clock time of the sample.
    pub t: u64,
    /// Each thread's name and counters: the sender, the gateways, then the pipeline's four.
    pub threads: Vec<(String, ThreadSample)>,
    pub core_stall_ns: u64,
    pub core_full_passes: u64,
    pub journal_full_passes: u64,
    /// The journal writer's time in `fdatasync` so far, and its flushes.
    pub journal_sync_ns: u64,
    pub journal_flushes: u64,
    /// Commands released by the gate so far.
    pub released: u64,
    /// Pages the kernel has migrated on the whole machine, read before the threads' counters
    /// at the window's open and after them at its close ([`Edge`]); `None` if unreadable.
    pub page_migrations: Option<u64>,
}

/// Which edge of the measured window a sample is taken at (module docs): it decides whether
/// the page migrations are read before the threads' faults or after them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Open,
    Close,
}

impl Watched {
    /// Samples every thread at run time `t`, at the window's `edge`.
    pub fn sample(&self, t: u64, edge: Edge) -> Sample {
        // At the open, first: a migration that finishes after this read counts inside the
        // window, as does any fault it causes after the faults' read below.
        let migrations_first = if edge == Edge::Open { page_migrations().ok() } else { None };
        let pipeline = self.pipeline.snapshot();
        let mut threads = vec![("sender".to_string(), self.sender.thread.sample())];
        for (g, gateway) in self.gateways.iter().enumerate() {
            threads.push((format!("gateway {g}"), gateway.thread.sample()));
        }
        threads.push(("sequencer".to_string(), pipeline.sequencer));
        threads.push(("journal writer".to_string(), pipeline.journal));
        threads.push(("core".to_string(), pipeline.core));
        threads.push(("gate".to_string(), pipeline.gate));
        Sample {
            t,
            threads,
            core_stall_ns: pipeline.core_stall_ns,
            core_full_passes: pipeline.sequencer_core_full_passes,
            journal_full_passes: pipeline.sequencer_journal_full_passes,
            journal_sync_ns: pipeline.journal_sync_ns,
            journal_flushes: pipeline.journal_flushes,
            released: pipeline.released,
            // At the close, last: it counts every migration that finished before the threads'
            // faults were read.
            page_migrations: match edge {
                Edge::Open => migrations_first,
                Edge::Close => page_migrations().ok(),
            },
        }
    }
}

/// One thread over the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadHealth {
    pub name: String,
    /// Busy time over wall time, in parts per million.
    pub busy_ppm: u64,
    /// Minor page faults inside the window; `None` if either sample couldn't read them.
    pub minor_faults: Option<u64>,
}

/// What limited a run (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Limit {
    CoreLimited,
    LimitedBy(String),
    /// The journal was behind, and its writer was mostly blocked in `fdatasync`.
    Disk {
        sync_ppm: u64,
    },
    /// No back-pressure, and no thread at least 90% busy.
    NotSaturated {
        busiest: String,
        busy_ppm: u64,
    },
}

impl Limit {
    pub fn label(&self) -> String {
        match self {
            Limit::CoreLimited => "core-limited".to_string(),
            Limit::LimitedBy(thread) => format!("limited by {thread}"),
            Limit::Disk { sync_ppm } => {
                format!("limited by the disk (fdatasync {} of the window)", percent_ppm(*sync_ppm))
            }
            Limit::NotSaturated { busiest, busy_ppm } => {
                format!("not saturated (busiest: {busiest} at {})", percent_ppm(*busy_ppm))
            }
        }
    }
}

/// A thread at least this busy is saturated (module docs), in ppm.
pub const SATURATED_PPM: u64 = 900_000;

/// The hot threads over the window (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowHealth {
    pub threads: Vec<ThreadHealth>,
    /// Nanoseconds the core waited for the event ring inside the window.
    pub core_stall_ns: u64,
    /// Sequencer passes that found the core ring (the journal ring) full inside the window.
    pub core_full_passes: u64,
    pub journal_full_passes: u64,
    /// The journal writer's time in `fdatasync` over wall time, in ppm.
    pub journal_sync_ppm: u64,
    /// The journal's flushes per second of wall time (15.8).
    pub journal_flushes_per_second: u64,
    /// Commands (client and operator) the gate released per second of wall time between
    /// the two samples: a throughput that needs no stamps (the `--stamps off` comparison,
    /// 15.1).
    pub released_per_second: u64,
    pub limit: Limit,
    /// Pages the kernel migrated on the whole machine inside the window (module docs);
    /// `None` if either sample couldn't read them.
    pub page_migrations: Option<u64>,
}

impl WindowHealth {
    /// From the samples at the window's two edges.
    pub fn between(open: &Sample, close: &Sample) -> WindowHealth {
        let wall = close.t.saturating_sub(open.t).max(1);
        let threads: Vec<ThreadHealth> = open
            .threads
            .iter()
            .zip(&close.threads)
            .map(|((name, before), (_, after))| ThreadHealth {
                name: name.clone(),
                busy_ppm: ppm(after.busy_ns.saturating_sub(before.busy_ns), wall),
                minor_faults: after.minor_faults.zip(before.minor_faults).map(|(a, b)| a.saturating_sub(b)),
            })
            .collect();
        let core_stall_ns = close.core_stall_ns - open.core_stall_ns;
        let core_full_passes = close.core_full_passes - open.core_full_passes;
        let journal_full_passes = close.journal_full_passes - open.journal_full_passes;
        let journal_sync_ppm = ppm(close.journal_sync_ns - open.journal_sync_ns, wall);
        let per_second = |n: u64| (u128::from(n) * 1_000_000_000 / u128::from(wall)) as u64;
        let journal_flushes_per_second = per_second(close.journal_flushes - open.journal_flushes);
        let released_per_second = per_second(close.released - open.released);
        let pressure = Pressure { core_stall_ns, core_full_passes, journal_full_passes, journal_sync_ppm };
        let limit = limit(&threads, &pressure);
        let page_migrations =
            close.page_migrations.zip(open.page_migrations).map(|(a, b)| a.saturating_sub(b));
        WindowHealth {
            threads,
            core_stall_ns,
            core_full_passes,
            journal_full_passes,
            journal_sync_ppm,
            journal_flushes_per_second,
            released_per_second,
            limit,
            page_migrations,
        }
    }

    /// Minor faults of every hot thread inside the window; `None` if any couldn't be read.
    pub fn total_faults(&self) -> Option<u64> {
        self.threads.iter().map(|t| t.minor_faults).sum()
    }

    /// True if the kernel migrated pages inside the window, so that a minor fault in it may
    /// be the kernel's rather than a first touch (module docs).
    pub fn kernel_migrated_pages(&self) -> bool {
        self.page_migrations.is_some_and(|pages| pages > 0)
    }
}

/// The back-pressure a window showed (module docs).
struct Pressure {
    core_stall_ns: u64,
    core_full_passes: u64,
    journal_full_passes: u64,
    journal_sync_ppm: u64,
}

/// The rule of 15.4 (module docs).
fn limit(threads: &[ThreadHealth], pressure: &Pressure) -> Limit {
    let busy = |name: &str| threads.iter().find(|t| t.name == name).map_or(0, |t| t.busy_ppm);
    let gate_saturated = busy("gate") >= SATURATED_PPM;
    // 1. The journal is behind: its ring filled, or the gate was waiting for the watermark.
    if pressure.journal_full_passes > 0 || (pressure.core_stall_ns > 0 && !gate_saturated) {
        return if pressure.journal_sync_ppm > busy("journal writer") {
            Limit::Disk { sync_ppm: pressure.journal_sync_ppm }
        } else {
            Limit::LimitedBy("journal writer".into())
        };
    }
    // 2. and 3.
    if pressure.core_stall_ns > 0 {
        return Limit::LimitedBy("gate".into());
    }
    if pressure.core_full_passes > 0 {
        return Limit::CoreLimited;
    }
    // 4. No back-pressure.
    let Some(busiest) = threads.iter().max_by_key(|t| t.busy_ppm) else {
        return Limit::NotSaturated { busiest: "-".into(), busy_ppm: 0 };
    };
    match busiest {
        t if t.busy_ppm < SATURATED_PPM => {
            Limit::NotSaturated { busiest: t.name.clone(), busy_ppm: t.busy_ppm }
        }
        t if t.name == "core" => Limit::CoreLimited,
        t => Limit::LimitedBy(t.name.clone()),
    }
}

/// What a search makes of one run (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Judgement {
    Pass,
    Fail(String),
    Invalid(String),
}

/// At least this share of the offered client commands must be sequenced (15.7), in ppm.
pub const MIN_SEQUENCED_PPM: u64 = 999_000;

/// Judges a run's summary against `limit_ns` on the client histogram of `stage` (its
/// summary name, e.g. `core_path`), by the rules of 15.7 (module docs).
pub fn judge(summary: &Summary, stage: &str, limit_ns: u64) -> Judgement {
    if summary.flag("check.valid") != Some(true) {
        let why = summary.get("check.invalid").unwrap_or("no verdict in the summary");
        return Judgement::Invalid(why.to_string());
    }
    if summary.flag("check.drain_cap_exceeded") == Some(true) {
        return Judgement::Fail("the backlog didn't drain within the cap after the window".to_string());
    }
    let sequenced = summary.u64("window.sequenced_ppm").unwrap_or(0);
    if sequenced < MIN_SEQUENCED_PPM {
        return Judgement::Fail(format!("only {} of offered commands sequenced", percent_ppm(sequenced)));
    }
    match summary.ns(&format!("stage.client.{stage}.p99")) {
        Some(Some(p99)) if p99 < limit_ns => Judgement::Pass,
        Some(p99) => {
            Judgement::Fail(format!("{stage} p99 {} is not under {}", latency(p99), latency(Some(limit_ns))))
        }
        None => Judgement::Invalid(format!("the summary has no {stage} p99")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample at `t` with these threads, core stall time and full core-ring passes; no
    /// journal pressure (`with_journal` adds it).
    fn sample(t: u64, threads: &[(&str, u64, Option<u64>)], stall: u64, full: u64) -> Sample {
        Sample {
            t,
            threads: threads
                .iter()
                .map(|&(name, busy_ns, minor_faults)| {
                    (name.to_string(), ThreadSample { busy_ns, minor_faults })
                })
                .collect(),
            core_stall_ns: stall,
            core_full_passes: full,
            journal_full_passes: 0,
            journal_sync_ns: 0,
            journal_flushes: 0,
            released: t / 1_000,
            page_migrations: Some(0),
        }
    }

    /// `sample` with the journal ring's full passes, the time in fdatasync and flushes.
    fn with_journal(mut sample: Sample, full: u64, sync_ns: u64, flushes: u64) -> Sample {
        (sample.journal_full_passes, sample.journal_sync_ns, sample.journal_flushes) =
            (full, sync_ns, flushes);
        sample
    }

    #[test]
    fn health_is_the_difference_between_the_two_edges() {
        let open =
            sample(1_000, &[("sender", 100, Some(5)), ("core", 200, Some(7)), ("gate", 0, None)], 10, 2);
        let close = sample(
            1_001_000,
            &[("sender", 100_100, Some(5)), ("core", 900_200, Some(7)), ("gate", 5, Some(1))],
            10,
            2,
        );
        let health = WindowHealth::between(&open, &close);
        assert_eq!(health.threads[0].busy_ppm, 100_000, "0.1 ms busy in 1 ms");
        assert_eq!(health.threads[1].busy_ppm, 900_000);
        assert_eq!(health.threads[1].minor_faults, Some(0));
        assert_eq!(health.threads[2].minor_faults, None, "the gate's first sample had no faults");
        assert_eq!(health.total_faults(), None);
        assert_eq!(health.limit, Limit::CoreLimited, "the core was 90% busy");
        assert_eq!((health.core_stall_ns, health.core_full_passes, health.journal_full_passes), (0, 0, 0));
        assert_eq!(health.released_per_second, 1_000_000, "one command released per µs");
        assert_eq!(health.page_migrations, Some(0));
        assert!(!health.kernel_migrated_pages());
    }

    #[test]
    fn the_kernels_page_migrations_are_counted_over_the_window() {
        let threads = [("sender", 0, Some(0))];
        let open = Sample { page_migrations: Some(102_000), ..sample(0, &threads, 0, 0) };
        let close = Sample { page_migrations: Some(115_746), ..sample(1_000, &threads, 0, 0) };
        let health = WindowHealth::between(&open, &close);
        assert_eq!(health.page_migrations, Some(13_746));
        assert!(health.kernel_migrated_pages());
        let unreadable = Sample { page_migrations: None, ..close };
        let health = WindowHealth::between(&open, &unreadable);
        assert_eq!(health.page_migrations, None);
        assert!(!health.kernel_migrated_pages(), "unknown is not a migration");
    }

    const THREADS: [&str; 3] = ["core", "journal writer", "gate"];

    /// Threads named as in `THREADS` that were `busy` ns busy over the 1,000 ns window.
    fn window(busy: [u64; 3], stall: u64, core_full: u64, journal: (u64, u64, u64)) -> WindowHealth {
        let open = sample(0, &THREADS.map(|name| (name, 0, Some(0))), 0, 0);
        let close = sample(1_000, &[0, 1, 2].map(|i| (THREADS[i], busy[i], Some(0))), stall, core_full);
        WindowHealth::between(&open, &with_journal(close, journal.0, journal.1, journal.2))
    }

    #[test]
    fn the_journal_behind_is_the_disk_when_its_writer_mostly_waited_in_fdatasync() {
        // Its ring filled; the writer was 5% busy and 85% in fdatasync.
        let disk = window([100, 50, 30], 0, 0, (4, 850, 900));
        assert_eq!(disk.limit.label(), "limited by the disk (fdatasync 85.0% of the window)");
        assert_eq!(disk.journal_flushes_per_second, 900_000_000, "900 flushes in 1 µs");
        // The core waited for the event ring, and the gate had room: waiting for the watermark.
        assert_eq!(window([100, 50, 30], 7, 0, (0, 850, 1)).limit, Limit::Disk { sync_ppm: 850_000 });
        // The writer itself was the busy one (discard mode at millions a second).
        let cpu = window([100, 950, 30], 0, 0, (3, 0, 1));
        assert_eq!(cpu.limit, Limit::LimitedBy("journal writer".into()));
    }

    #[test]
    fn a_saturated_gate_a_full_core_ring_or_a_busy_thread_name_the_limit() {
        assert_eq!(window([100, 50, 950], 7, 0, (0, 0, 0)).limit, Limit::LimitedBy("gate".into()));
        assert_eq!(window([600, 50, 30], 0, 3, (0, 0, 0)).limit, Limit::CoreLimited, "the core ring filled");
        assert_eq!(window([970, 50, 30], 0, 0, (0, 0, 0)).limit, Limit::CoreLimited);
        assert_eq!(window([100, 50, 930], 0, 0, (0, 0, 0)).limit.label(), "limited by gate");
    }

    #[test]
    fn with_no_back_pressure_and_no_busy_thread_nothing_is_blamed() {
        // At 20k/s on a disk whose fdatasync takes most of T: 3.4% core, the writer 1% busy
        // and 88% in fdatasync, nothing full. Not the writer's limit, nor the core's.
        let calm = window([34, 10, 5], 0, 0, (0, 880, 1));
        assert_eq!(calm.limit.label(), "not saturated (busiest: core at 3.4%)");
        assert_eq!(calm.journal_sync_ppm, 880_000);
    }

    #[test]
    fn a_run_passes_only_if_valid_served_and_under_the_limit() {
        let mut summary = Summary::new();
        summary.put("check.valid", true);
        summary.put("window.sequenced_ppm", 999_500);
        summary.put_ns("stage.client.core_path.p99", Some(40_000));
        assert_eq!(judge(&summary, "core_path", 50_000), Judgement::Pass);
        assert!(matches!(judge(&summary, "core_path", 40_000), Judgement::Fail(_)), "under, not at");
        summary.put("window.sequenced_ppm", 998_999);
        assert!(matches!(judge(&summary, "core_path", 50_000), Judgement::Fail(_)));
        summary.put("check.valid", false);
        summary.put("check.invalid", "throttled");
        assert_eq!(judge(&summary, "core_path", 50_000), Judgement::Invalid("throttled".into()));
    }
}
