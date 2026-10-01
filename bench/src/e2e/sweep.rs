//! Sweeps: sets of runs repeated 3 times, interleaved (`docs/PIPELINE.md` 15.5, 15.6, 15.8,
//! 15.1 and 15.7).
//!
//! **Contract.** A sweep is a list of [`Point`]s (one run configuration each) run
//! [`REPETITIONS`] times **interleaved**: A B C, A B C, A B C, never A A A, B B B, so slow
//! drift on a shared box (neighbours, heat) doesn't bias whichever point runs last (15.5).
//! Every run goes through the session, so a sweep is resumable, and every point is checked
//! (`RunConfig::check`) before anything is signed. Runs are named `<point>-r<repetition>`.
//! An invalid repetition is run again, as `<point>-r<k>-a2`, `-a3` (15.7;
//! `Session::run_valid`), so each point gets its valid runs; nothing is dropped: the report
//! lists every invalid run with its reason and uses only the valid ones (review finding
//! F3).
//!
//! The sweeps of M3:
//! - [`load_points`] (15.6): both modes at offered 20k, 100k, 500k and 1M client commands a
//!   second, on the real journal. The pre-verified 20k/s point runs first: its `fdatasync`
//!   p99 sets the durable limit (15.7). The signed 500k and 1M points are overload tests and
//!   use a 10 s window (15.5).
//! - [`commit_interval_points`] (15.8): pre-verified at 100k/s, `T` in {0, 250 µs, 500 µs,
//!   1 ms, 2 ms}: the evidence for the commit interval (D-025).
//! - [`stamps_points`] (15.1): the cost of measurement: a saturation run (pre-verified,
//!   journal discarded, offered at twice the found maximum) with and without stamps.
//! - [`headline_points`] (15.5, 15.7, 13.2, 14.1): the signed 100k/s point with twice the
//!   sweep's window (60 s at the spec's 30 s), three timing runs, then a fourth with capture
//!   on for the replay test and the audit, and one more with a second seed. Of whichever
//!   flow the template sends, with its switches: each flow and switch has its own run names
//!   (`config.rs`), so a session may hold several headlines, and the report gives each its
//!   own (15.7).
//! - [`polymarket_points`] (D-034): the Polymarket-shaped flow's three headlines, interleaved:
//!   the flow as it is, with bursts (the median hour's), and with the stress shock. Each is
//!   a full headline (its timing runs, a capture run whose replay test covers the shocks'
//!   liquidations, a second seed): 15 runs of about 67 s.
//!
//! Each point takes its mode from the sweep and the rest from one template
//! (`RunConfig::in_mode`): its signed points sign in the template's scheme, its
//! pre-verified points sign nothing (5.8).
//!
//! **Complexity.** The sum of the runs' durations; [`plan`] prints an estimate first.

use loadgen::market_flow::polymarket::ShockSize;
use loadgen::schedule::{Arrivals, Bursts};
use pipeline::journal::writer::JournalMode;
use pipeline::records::{InjectionMode, Stamps};

use super::config::{JournalSettings, RunConfig};
use super::runner::RunError;
use super::session::{REPETITIONS, Session};
use super::summary::Summary;
use super::units::{SECOND_NS, short_duration};

/// One configuration of a sweep.
#[derive(Clone, Debug)]
pub struct Point {
    pub name: String,
    pub config: RunConfig,
    /// Runs of it: 3, or 1 for the headline's capture and second-seed runs.
    pub repetitions: usize,
}

impl Point {
    /// A point named after its configuration, run 3 times.
    pub fn new(config: RunConfig) -> Point {
        Point { name: config.name(), config, repetitions: REPETITIONS }
    }
}

/// The offered rates of the load sweep (15.6).
pub const LOAD_RATES: [u64; 4] = [20_000, 100_000, 500_000, 1_000_000];
/// The commit intervals of the `T` sweep (15.8), in nanoseconds.
pub const COMMIT_INTERVALS: [u64; 5] = [0, 250_000, 500_000, 1_000_000, 2_000_000];
/// The window at the signed overload points (15.5).
const OVERLOAD_WINDOW_NS: u64 = 10 * SECOND_NS;

/// The load sweep's points (module docs), from `template`'s timing and threads; `rates`
/// replaces 15.6's for scaled-down local sessions. Pre-verified first, lowest rate first.
pub fn load_points(template: &RunConfig, rates: &[u64]) -> Vec<Point> {
    let mut points = Vec::new();
    for mode in [InjectionMode::PreVerified, InjectionMode::Signed] {
        for &rate in rates {
            let mut config = RunConfig { rate, ..template.in_mode(mode) };
            if mode == InjectionMode::Signed && rate >= 500_000 {
                config.window_ns = config.window_ns.min(OVERLOAD_WINDOW_NS);
            }
            points.push(Point::new(config));
        }
    }
    points
}

/// The `T` sweep's points (module docs).
pub fn commit_interval_points(template: &RunConfig, rate: u64) -> Vec<Point> {
    COMMIT_INTERVALS
        .iter()
        .map(|&interval| {
            let mut config = RunConfig { rate, ..template.in_mode(InjectionMode::PreVerified) };
            config.journal.commit_interval_ns = interval;
            // Named with its T even at the default, so every row of the sweep says which.
            let mut name = config.name();
            if interval == JournalSettings::default().commit_interval_ns {
                name.push_str(&format!("-T{}", short_duration(interval)));
            }
            Point { name, config, repetitions: REPETITIONS }
        })
        .collect()
}

/// The cost-of-measurement pair (module docs): `rate` should be twice the core-path
/// search's result.
pub fn stamps_points(template: &RunConfig, rate: u64) -> Vec<Point> {
    [Stamps::On, Stamps::Off]
        .into_iter()
        .map(|stamps| {
            let mut config = RunConfig { rate, stamps, ..template.in_mode(InjectionMode::PreVerified) };
            config.journal.mode = JournalMode::Discard;
            Point::new(config)
        })
        .collect()
}

/// The headline's runs (module docs): `seed_2` is the second seed.
pub fn headline_points(template: &RunConfig, rate: u64, seed_2: u64) -> Vec<Point> {
    let mut timing = RunConfig { rate, ..template.in_mode(InjectionMode::Signed) };
    timing.window_ns = 2 * template.window_ns;
    let mut capture = timing.clone();
    capture.capture = true;
    capture.audit = true;
    let mut second_seed = timing.clone();
    second_seed.flow = second_seed.flow.with_seed(seed_2);
    vec![
        Point { name: format!("{}-timing", timing.name()), config: timing, repetitions: REPETITIONS },
        Point { name: format!("{}-capture", capture.name()), config: capture, repetitions: 1 },
        Point { name: format!("{}-seed{seed_2}", second_seed.name()), config: second_seed, repetitions: 1 },
    ]
}

/// The D-034 headlines (module docs): [`headline_points`] of the Polymarket-shaped flow as
/// it is, with bursts (the median hour's), and with the stress shock, in that order, so that
/// [`run_interleaved`] runs their repetitions interleaved. The flow is the template's if it
/// is the Polymarket one (with any `--makers` it has), else the Polymarket flow with the
/// template's seed; bursts or a shock the template has are replaced by the variants' own.
pub fn polymarket_points(template: &RunConfig, rate: u64, seed_2: u64) -> Vec<Point> {
    let mut plain = template.clone();
    plain.flow = template.flow.polymarket_counterpart().without_shock();
    if plain.bursts().is_some() {
        plain.arrivals = Arrivals::Poisson;
    }
    let bursts = RunConfig { arrivals: Arrivals::Cox(Bursts::Median), ..plain.clone() };
    let shock = RunConfig {
        flow: plain.flow.with_shock(ShockSize::Stress).expect("the Polymarket flow has the switch"),
        ..plain.clone()
    };
    [plain, bursts, shock].iter().flat_map(|variant| headline_points(variant, rate, seed_2)).collect()
}

/// Prints what a sweep will run and about how long it will take (15.11: the harness prints
/// its plan and its estimate before it starts).
pub fn plan(group: &str, points: &[Point]) {
    let mut total_ns = 0;
    eprintln!("{group}: {} points, interleaved:", points.len());
    for point in points {
        let c = &point.config;
        // Setup and draining: about 2 s a run, beside the timed flow.
        let run_ns = c.warmup_ns + c.window_ns + c.tail_ns + 2 * SECOND_NS;
        total_ns += run_ns * point.repetitions as u64;
        eprintln!(
            "  {} × {}: warm-up {}, window {}",
            point.repetitions,
            point.name,
            short_duration(c.warmup_ns),
            short_duration(c.window_ns)
        );
    }
    eprintln!(
        "{group}: about {} minutes of runs, plus building the workloads",
        total_ns.div_ceil(60 * SECOND_NS)
    );
}

/// Runs every point of `points` in `group`, interleaved (module docs). Returns each run's
/// summary, in run order.
pub fn run_interleaved(
    session: &mut Session,
    group: &str,
    points: &[Point],
) -> Result<Vec<(String, Summary)>, RunError> {
    for point in points {
        point.config.check().map_err(RunError::Refused)?; // before any signing
    }
    plan(group, points);
    let rounds = points.iter().map(|p| p.repetitions).max().unwrap_or(0);
    // Build each workload once, for the largest run still to go (14.8).
    let to_run: Vec<&RunConfig> = points
        .iter()
        .filter(|p| (1..=p.repetitions).any(|rep| !session.has_run(group, &run_name(p, rep))))
        .map(|p| &p.config)
        .collect();
    session.workloads.reserve(to_run);
    let mut summaries = Vec::new();
    for rep in 1..=rounds {
        for point in points.iter().filter(|p| p.repetitions >= rep) {
            let attempted = session.run_valid(group, &run_name(point, rep), &point.config)?;
            let name = attempted.names.last().expect("at least one attempt").clone();
            summaries.push((name, attempted.summary));
        }
    }
    Ok(summaries)
}

/// Repetition `rep` of `point`: `<point>-r<rep>` (module docs).
fn run_name(point: &Point, rep: usize) -> String {
    format!("{}-r{rep}", point.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2e::flow::Flow;
    use loadgen::market_flow::PlanConfig;
    use pipeline::records::AuthScheme;

    fn template() -> RunConfig {
        RunConfig::new(InjectionMode::PreVerified, 1)
    }

    #[test]
    fn the_load_sweep_starts_with_the_pre_verified_20k_point_and_shortens_the_overload_points() {
        let points = load_points(&template(), &LOAD_RATES);
        let names: Vec<&str> = points.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "preverified-20k",
                "preverified-100k",
                "preverified-500k",
                "preverified-1M",
                "signed-20k",
                "signed-100k",
                "signed-500k",
                "signed-1M"
            ]
        );
        assert_eq!(points[5].config.window_ns, 30 * SECOND_NS);
        assert_eq!(points[6].config.window_ns, 10 * SECOND_NS);
        assert!(points.iter().all(|p| p.repetitions == 3));
    }

    #[test]
    fn the_t_sweep_names_every_interval_and_the_stamps_pair_discards_the_journal() {
        let names: Vec<String> =
            commit_interval_points(&template(), 100_000).into_iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            [
                "preverified-100k-T0",
                "preverified-100k-T250us",
                "preverified-100k-T500us",
                "preverified-100k-T1ms",
                "preverified-100k-T2ms"
            ]
        );
        let stamps = stamps_points(&template(), 4_000_000);
        assert_eq!(stamps[0].name, "preverified-4M-discard");
        assert_eq!(stamps[1].name, "preverified-4M-discard-stamps-off");
    }

    #[test]
    fn an_eip712_template_signs_the_signed_points_and_leaves_the_pre_verified_ones_unsigned() {
        let template = RunConfig { auth: AuthScheme::Eip712, ..RunConfig::new(InjectionMode::Signed, 1) };
        let load = load_points(&template, &[20_000]);
        let names: Vec<(&str, AuthScheme)> = load.iter().map(|p| (p.name.as_str(), p.config.auth)).collect();
        assert_eq!(names, [("preverified-20k", AuthScheme::Perp), ("signed-20k-eip712", AuthScheme::Eip712)]);
        let headline = headline_points(&template, 100_000, 2);
        assert_eq!(headline[0].name, "signed-100k-eip712-timing");
        assert!(headline.iter().all(|p| p.config.auth == AuthScheme::Eip712));
        let others = [commit_interval_points(&template, 100_000), stamps_points(&template, 1_000_000)];
        assert!(others.iter().flatten().all(|p| p.config.auth == AuthScheme::Perp));
        for point in load.iter().chain(&headline).chain(others.iter().flatten()) {
            point.config.check().expect("valid");
        }
    }

    #[test]
    fn the_headline_has_three_timing_runs_a_capture_run_and_a_second_seed() {
        let points = headline_points(&template(), 100_000, 2);
        let names: Vec<(&str, usize)> = points.iter().map(|p| (p.name.as_str(), p.repetitions)).collect();
        assert_eq!(names, [("signed-100k-timing", 3), ("signed-100k-capture", 1), ("signed-100k-seed2", 1)]);
        assert_eq!(points[0].config.window_ns, 60 * SECOND_NS);
        assert!(points[1].config.capture && points[1].config.audit);
        assert_eq!(points[2].config.flow.seed(), 2);
    }

    #[test]
    fn the_polymarket_headlines_are_the_flow_as_it_is_with_bursts_and_with_the_stress_shock() {
        // From an M3 template: the Polymarket flow, with the template's seed.
        let template = RunConfig { flow: Flow::m3().with_seed(4), ..template() };
        let points = polymarket_points(&template, 100_000, 5);
        let names: Vec<(&str, usize)> = points.iter().map(|p| (p.name.as_str(), p.repetitions)).collect();
        assert_eq!(
            names,
            [
                ("signed-100k-polymarket-timing", 3),
                ("signed-100k-polymarket-capture", 1),
                ("signed-100k-polymarket-seed5", 1),
                ("signed-100k-polymarket-bursts-median-timing", 3),
                ("signed-100k-polymarket-bursts-median-capture", 1),
                ("signed-100k-polymarket-bursts-median-seed5", 1),
                ("signed-100k-polymarket-shock-stress-timing", 3),
                ("signed-100k-polymarket-shock-stress-capture", 1),
                ("signed-100k-polymarket-shock-stress-seed5", 1),
            ]
        );
        assert_eq!(points[0].config.flow, Flow::polymarket().with_seed(4));
        assert_eq!(points[3].config.flow, points[0].config.flow, "bursts leave the plan as it is");
        for point in &points {
            point.config.check().expect("valid");
        }
        // From a Polymarket template with switches: its makers stay, its bursts and shock
        // are the variants'.
        let flow = Flow::polymarket().with_makers(3).expect("a switch");
        let switched = RunConfig {
            flow: flow.with_shock(ShockSize::Calibrated).expect("a switch"),
            arrivals: Arrivals::Cox(Bursts::Busiest),
            ..template.clone()
        };
        let names: Vec<String> = polymarket_points(&switched, 100_000, 2)
            .into_iter()
            .filter(|p| p.name.ends_with("-timing"))
            .map(|p| p.name)
            .collect();
        assert_eq!(
            names,
            [
                "signed-100k-polymarket-makers3-timing",
                "signed-100k-polymarket-makers3-bursts-median-timing",
                "signed-100k-polymarket-makers3-shock-stress-timing"
            ]
        );
    }
}
