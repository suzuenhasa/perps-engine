//! The end-to-end harness of Milestone 3 (`docs/PIPELINE.md` sections 15 to 17, 18.4 and
//! 19): it wires the load generator, the gateways and the pipeline together, runs them at
//! an offered rate, measures every stage, checks every run's own conditions, and writes
//! the tables that go into `docs/BENCHMARKS.md`.
//!
//! **The layers, bottom up.**
//! - [`config`]: what one run is (mode, flow, rate, timing, journal, threads), with the
//!   spec's defaults and the smoke runs of 18.4.
//! - [`flow`]: which flow a run sends, the M3 flow (D-027) or the Polymarket-shaped one
//!   (D-034) with its stress switches, as one type the rest of the harness handles.
//! - [`workload`]: the flow plan and its pre-signed (or compact) messages, built once and
//!   reused as a prefix (14.8); the rest of the flow after a restart (13.5).
//! - [`runner`]: one run: the wiring of 19.2, main's loop that samples the window's edges,
//!   the shutdown in data-flow order, the replay test and the audit.
//! - [`results`]: what a run measured, whether it counts (15.7: invalid runs, flags), and
//!   its `summary.txt`, from which every report is built ([`summary`]).
//! - [`checks`]: thread health, the limiting thread (15.4), and the pass rule of 15.7.
//! - [`session`]: a directory of resumable runs (15.5), and the durable limit (Q1).
//! - [`sweep`], [`search`], [`ablate`]: the offered-load sweep and the `T` sweep, the
//!   maximum rate at a latency limit, and the two ablations of section 16.
//! - [`probes`]: the machine probes of 15.10, and the facts every run records.
//! - [`report`]: Markdown for BENCHMARKS.md.
//! - [`units`]: rates, durations and latencies as people write and read them.
//! - [`watch`]: `e2e run --watch`, the live panel and its recording.
//!
//! The command line is `src/bin/e2e.rs`: `probe | run | sweep | search | ablate | replay |
//! recover`.
//!
//! **Honest numbers** (INFO.md 10): every run states its layer, machine, environment and
//! commit; latency is measured from the scheduled send time and over offered commands; a
//! run the kernel throttled, whose clock went backwards or whose sender was the limit is
//! reported as invalid and never used; nothing is dropped from a report.

pub mod ablate;
pub mod checks;
pub mod config;
pub mod flow;
pub mod probes;
pub mod report;
pub mod results;
pub mod runner;
pub mod search;
pub mod session;
pub mod summary;
pub mod sweep;
pub mod units;
pub mod watch;
pub mod workload;
