//! The maximum sustained rate at which a stage's p99 stays under a limit
//! (`docs/PIPELINE.md` 15.7; INFO.md 8).
//!
//! **The procedure** ([`search`]), for a mode, a stage and a limit:
//! 1. **A run at rate `R`** is 5 s of warm-up and 20 s measured. It passes if the stage's
//!    p99 over offered commands is under the limit and at least 99.9% of the offered client
//!    commands were sequenced (`checks::judge`). An invalid run (throttled, a clock
//!    inversion, a generator-limited sender) is run again, up to 3 new attempts per call
//!    (`Session::run_valid`); a resumed search goes on after the attempts already made.
//! 2. **Double:** 1 run each at `R`, `2R`, `4R`, ... until one fails; if the first fails,
//!    halve instead until one passes. Doubling stops at the memory cap, where the run's
//!    items would pass half the memory this process may still use: the machine's, or the
//!    container's cgroup limit if lower, less what the process already holds (14.8).
//! 3. **Bisect** three times between the last pass and the first fail, 1 run each.
//! 4. **Confirm:** the highest passing rate and the lowest failing rate, 3 runs each,
//!    interleaved. The result is the highest rate that passed all 3; if a repetition fails,
//!    the next lower probed passing rate is confirmed instead.
//! 5. **Report** the result, its median p99, the first failing rate, the resolution (the gap
//!    between the result and the first failing rate: 1/8 of the last doubling step after
//!    three bisections, more if a confirmation stepped down), the **saturation
//!    throughput** (the achieved rate when offered twice the result) and what limited that
//!    run (15.4). If the saturation run kept up with what it was offered (99% or more), the
//!    pipeline wasn't saturated there, and the report says so and gives the highest rate
//!    any probe achieved instead (review finding F6-search-outcome-after-step-down).
//!
//! The algorithm takes the probe as a closure, so its tests run it against a model.
//! [`run_search`] runs it on the pipeline, through the session (every probe is a resumable
//! run, and a search that dies continues where it stopped), and writes the result to
//! `<session>/search-<name>/search.txt`.
//!
//! **The searches of M3** ([`SearchSpec::core_path`], [`SearchSpec::durable`]): the core path
//! under 50 µs, pre-verified, with the journal discarded (the disk can't hold the core
//! back); and pre-verified or signed command → durable ack under the durable limit (Q1), on
//! the real journal. The durable stages are refused in discard mode (11.6). A search of the
//! Polymarket-shaped flow, or with a stress switch (D-034), adds the same suffix to its name
//! as its runs do (`RunConfig::flow_suffix`): `signed-polymarket-makers3` runs in
//! `search-signed-polymarket-makers3`, so each flow and switch has its own search, its own
//! probes and its own `search.txt` in a session, which names the flow (`search.flow`).
//!
//! **Complexity.** About 12 runs (4 doublings, 3 bisections, 6 confirmations), plus one at
//! twice the result.

use pipeline::records::{AuthScheme, InjectionMode};

use super::checks::{Judgement, judge};
use super::config::RunConfig;
use super::flow::{bursts_name, shock_name};
use super::runner::RunError;
use super::session::{Session, median};
use super::summary::Summary;
use super::units::{SECOND_NS, latency, short_rate};

/// Bisections after the doubling (15.7, step 3).
pub const BISECTIONS: usize = 3;
/// Runs of each rate in the confirmation (15.7, step 4).
pub const CONFIRMATIONS: usize = 3;
/// The core path's limit (INFO.md 5): 50 µs.
pub const CORE_PATH_LIMIT_NS: u64 = 50_000;

/// One probe's answer.
#[derive(Clone, Debug)]
pub struct Probed {
    pub passed: bool,
    pub summary: Summary,
}

/// Where the search may go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// The first rate: 100k/s, or the probed `1/F` for the fsync-per-order arm (16).
    pub start: u64,
    /// Doubling stops above this: the memory cap (14.8).
    pub max_rate: u64,
    /// Halving gives up below this.
    pub min_rate: u64,
}

/// What a search found (module docs, step 5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchOutcome {
    /// The highest rate that passed all its confirmations.
    pub result: Option<u64>,
    /// The lowest rate that failed, above the result.
    pub first_fail: Option<u64>,
    /// The first failing rate minus the result (module docs, step 5).
    pub resolution: Option<u64>,
    /// The median p99 of the result's confirmations.
    pub median_p99: Option<u64>,
    /// What limited the saturation run (15.4); the result's first confirmation's label if
    /// there was no saturation run.
    pub limit: Option<String>,
    /// The achieved rate when offered twice the result.
    pub saturation: Option<u64>,
    /// The saturation run achieved at least 99% of what it was offered: the pipeline wasn't
    /// saturated there, so `saturation` is only a lower bound.
    pub saturation_not_reached: bool,
    /// The highest rate any probe achieved.
    pub highest_achieved: Option<u64>,
    /// Every probe: its run name, rate, and whether it passed.
    pub trail: Vec<(String, u64, bool)>,
    /// Anything the search stopped at: the memory cap, no passing rate.
    pub note: Option<String>,
}

/// The procedure of 15.7 (module docs), with `probe(rate, name)` running one probe named
/// `name`.
pub fn search<E>(
    bounds: Bounds,
    stage: &str,
    mut probe: impl FnMut(u64, &str) -> Result<Probed, E>,
) -> Result<SearchOutcome, E> {
    let mut outcome = SearchOutcome::default();
    let mut once = |rate: u64, step: &str, outcome: &mut SearchOutcome| -> Result<Probed, E> {
        let name = format!("{}-{step}", short_rate(rate));
        let probed = probe(rate, &name)?;
        outcome.trail.push((name, rate, probed.passed));
        let achieved = probed.summary.u64("window.achieved_rate");
        outcome.highest_achieved = outcome.highest_achieved.max(achieved);
        Ok(probed)
    };

    // 2. Double, or halve.
    let (mut pass, mut fail) = (None, None);
    let mut rate = bounds.start;
    if once(rate, "probe", &mut outcome)?.passed {
        pass = Some(rate);
        while fail.is_none() {
            if rate * 2 > bounds.max_rate {
                outcome.note = Some(format!(
                    "doubling stopped at the memory cap, {}/s (14.8)",
                    short_rate(bounds.max_rate)
                ));
                break;
            }
            rate *= 2;
            if once(rate, "probe", &mut outcome)?.passed { pass = Some(rate) } else { fail = Some(rate) }
        }
    } else {
        fail = Some(rate);
        while pass.is_none() {
            if rate / 2 < bounds.min_rate {
                outcome.note = Some(format!("no rate passed, down to {}/s", short_rate(rate)));
                return Ok(outcome);
            }
            rate /= 2;
            if once(rate, "probe", &mut outcome)?.passed { pass = Some(rate) } else { fail = Some(rate) }
        }
    }
    let (mut low, mut high) = (pass.expect("set above"), fail);

    // 3. Bisect.
    if high.is_some() {
        for _ in 0..BISECTIONS {
            let hi = high.expect("set");
            let mid = low + (hi - low) / 2;
            if mid == low || mid == hi {
                break;
            }
            if once(mid, "bisect", &mut outcome)?.passed { low = mid } else { high = Some(mid) }
        }
    }

    // 4. Confirm: the highest pass (stepping down if it fails) and the lowest fail.
    let mut passing: Vec<u64> = outcome.trail.iter().filter(|t| t.2 && t.1 <= low).map(|t| t.1).collect();
    passing.sort_unstable_by(|a, b| b.cmp(a));
    passing.dedup();
    for (i, &candidate) in passing.iter().enumerate() {
        let mut runs = Vec::new();
        for k in 1..=CONFIRMATIONS {
            runs.push(once(candidate, &format!("confirm{k}"), &mut outcome)?);
            if i == 0
                && let Some(hi) = high
            {
                once(hi, &format!("confirm{k}"), &mut outcome)?;
            }
        }
        if runs.iter().all(|r| r.passed) {
            outcome.result = Some(candidate);
            let mut p99s: Vec<u64> = runs
                .iter()
                .map(|r| r.summary.ns(&format!("stage.client.{stage}.p99")).flatten().unwrap_or(u64::MAX))
                .collect();
            outcome.median_p99 = Some(median(&mut p99s));
            outcome.limit = runs[0].summary.get("health.limit").map(str::to_string);
            break;
        }
    }
    let result = outcome.result;
    outcome.first_fail =
        outcome.trail.iter().filter(|t| !t.2 && result.is_none_or(|r| t.1 > r)).map(|t| t.1).min();
    outcome.resolution = outcome.first_fail.zip(result).map(|(fail, result)| fail - result);

    // 5. Saturation: offered twice the result, if that fits in memory.
    match result {
        Some(result) if 2 * result <= bounds.max_rate => {
            let saturated = once(2 * result, "saturation", &mut outcome)?;
            let achieved = saturated.summary.u64("window.achieved_rate");
            outcome.saturation = achieved;
            outcome.saturation_not_reached = achieved.is_some_and(|a| a * 100 >= 2 * result * 99);
            if let Some(limit) = saturated.summary.get("health.limit") {
                outcome.limit = Some(limit.to_string());
            }
        }
        Some(_) => {
            outcome.note = Some("saturation not measured: twice the result is over the memory cap".into())
        }
        None => {}
    }
    Ok(outcome)
}

/// A search to run (module docs, "The searches of M3").
#[derive(Clone, Debug)]
pub struct SearchSpec {
    /// The group's name: `search-<name>`.
    pub name: String,
    /// The stage's summary name: `core_path`, `to_durable_ack`.
    pub stage: &'static str,
    pub limit_ns: u64,
    /// Every probe's configuration but its rate.
    pub template: RunConfig,
    pub start: u64,
}

impl SearchSpec {
    /// The core path under 50 µs, pre-verified, the journal discarded (15.7).
    pub fn core_path(template: &RunConfig) -> SearchSpec {
        let mut template = template.in_mode(InjectionMode::PreVerified);
        template.journal.mode = pipeline::journal::writer::JournalMode::Discard;
        SearchSpec {
            name: format!("core-path{}", template.flow_suffix()),
            stage: "core_path",
            limit_ns: CORE_PATH_LIMIT_NS,
            template,
            start: 100_000,
        }
    }

    /// Command → durable ack under `limit_ns`, on the real journal (15.7). The search is
    /// named `name`, plus its flow's and switches' suffix (module docs, "The searches of
    /// M3").
    pub fn durable(name: &str, template: &RunConfig, limit_ns: u64) -> SearchSpec {
        SearchSpec {
            name: format!("{name}{}", template.flow_suffix()),
            stage: "to_durable_ack",
            limit_ns,
            template: template.clone(),
            start: 100_000,
        }
    }

    /// The memory cap and the halving floor (module docs).
    fn bounds(&self) -> Bounds {
        let seconds = (self.template.warmup_ns + self.template.window_ns + self.template.tail_ns)
            .div_ceil(SECOND_NS)
            .max(1);
        // About what each timed item costs in memory: its plan item, its send time, and its
        // message (136 bytes signed, 56 compact); in the EIP-712 scheme, also its slots in a
        // gateway's replay table, at most 4 (`gateway::salts`: at least twice as many slots
        // as requests, a power of two).
        let message = match self.template.mode {
            InjectionMode::Signed => 136,
            InjectionMode::PreVerified => 56,
        };
        let replay_table = match self.template.auth {
            AuthScheme::Perp => 0,
            AuthScheme::Eip712 => 4 * gateway::salts::SLOT_BYTES,
        };
        let per_item = (size_of::<loadgen::market_flow::Item>() + 8 + message + replay_table) as u64;
        // What the process may still use: its limit (the cgroup's, if lower than the
        // machine's) less what it holds already, such as the session's other workloads.
        let limit = super::probes::memory_bytes().unwrap_or(8 << 30);
        let memory = limit.saturating_sub(super::probes::resident_bytes().unwrap_or(0));
        let max_rate = (memory / 2 / per_item / seconds).max(self.start);
        Bounds { start: self.start, max_rate, min_rate: 100 }
    }
}

/// Runs `spec` through the session (module docs) and writes its result.
pub fn run_search(session: &mut Session, spec: &SearchSpec) -> Result<SearchOutcome, RunError> {
    let durable = spec.stage == "to_durable_ack" || spec.stage == "durability_wait";
    if durable && spec.template.discarded() {
        return Err(RunError::Refused(format!(
            "search {}: no durable stage in discard mode (11.6)",
            spec.name
        )));
    }
    let group = format!("search-{}", spec.name);
    eprintln!(
        "search {}: the highest {}/s with {} p99 under {}",
        spec.name,
        super::config::mode_name(spec.template.mode),
        spec.stage,
        latency(Some(spec.limit_ns))
    );
    let bounds = spec.bounds();
    let outcome = search(bounds, spec.stage, |rate, name| probe(session, &group, spec, rate, name))?;
    let path = session.dir.join(&group).join("search.txt");
    outcome_summary(spec, &outcome).write(&path)?;
    Ok(outcome)
}

/// One probe: a run at `rate`, run again while it is invalid (`Session::run_valid`), then
/// judged (module docs, step 1).
fn probe(
    session: &mut Session,
    group: &str,
    spec: &SearchSpec,
    rate: u64,
    name: &str,
) -> Result<Probed, RunError> {
    let config = RunConfig { rate, ..spec.template.clone() };
    let attempted = session.run_valid(group, name, &config)?;
    let run_name = attempted.names.last().expect("at least one attempt");
    if !attempted.valid {
        return Err(RunError::Refused(format!(
            "search {}: {name} was invalid in every attempt ({}); fix the cause and run the search again: \
             it goes on with the next attempt",
            spec.name,
            attempted.names.join(", ")
        )));
    }
    match judge(&attempted.summary, spec.stage, spec.limit_ns) {
        Judgement::Pass => Ok(Probed { passed: true, summary: attempted.summary }),
        Judgement::Fail(why) => {
            eprintln!("search {}: {run_name} fails: {why}", spec.name);
            Ok(Probed { passed: false, summary: attempted.summary })
        }
        Judgement::Invalid(why) => Err(RunError::Refused(format!("search {}: {run_name}: {why}", spec.name))),
    }
}

/// The search's `search.txt`.
fn outcome_summary(spec: &SearchSpec, outcome: &SearchOutcome) -> Summary {
    let mut s = Summary::new();
    let optional = |value: Option<u64>| value.map_or("none".to_string(), |v| v.to_string());
    s.put("search.name", &spec.name);
    s.put("search.mode", super::config::mode_name(spec.template.mode));
    // The flow and its switches, as its runs' `run.flow`, `run.makers`, ... say them.
    let template = &spec.template;
    s.put("search.flow", template.flow.name());
    s.put("search.makers", template.flow.makers().map_or("default".to_string(), |k| k.to_string()));
    s.put("search.bursts", template.bursts().map_or("none", bursts_name));
    s.put("search.shock", template.flow.shock().map_or("none", shock_name));
    s.put("search.stage", spec.stage);
    s.put("search.limit_ns", spec.limit_ns);
    s.put("search.journal", if spec.template.discarded() { "discard" } else { "disk" });
    s.put("search.verify_on_core", spec.template.verify_on_core.map_or("none", |arm| arm.name()));
    s.put("search.verifier", spec.template.verifier_name());
    s.put("search.auth", spec.template.auth_name());
    s.put("search.commit_interval_ns", spec.template.journal.commit_interval_ns);
    s.put("search.max_batch", spec.template.journal.max_batch);
    s.put("search.result", optional(outcome.result));
    s.put("search.first_fail", optional(outcome.first_fail));
    s.put("search.resolution", optional(outcome.resolution));
    s.put_ns("search.median_p99", outcome.median_p99);
    s.put("search.limit", outcome.limit.as_deref().unwrap_or("-"));
    s.put("search.saturation", optional(outcome.saturation));
    s.put("search.saturation_reached", !outcome.saturation_not_reached);
    s.put("search.highest_achieved", optional(outcome.highest_achieved));
    s.put("search.note", outcome.note.as_deref().unwrap_or("-"));
    let trail: Vec<String> = outcome
        .trail
        .iter()
        .map(|(name, _, passed)| format!("{name}:{}", if *passed { "pass" } else { "fail" }))
        .collect();
    s.put("search.trail", trail.join(" "));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model pipeline whose p99 is under the limit below `capacity`, plus flaky rates
    /// whose confirmations fail.
    fn model(capacity: u64, flaky: &[u64]) -> impl FnMut(u64, &str) -> Result<Probed, String> + '_ {
        move |rate, name| {
            let mut summary = Summary::new();
            let passed = rate < capacity && !(flaky.contains(&rate) && name.contains("confirm2"));
            summary.put_ns("stage.client.core_path.p99", Some(rate / 10));
            summary.put("window.achieved_rate", rate.min(capacity));
            summary.put("health.limit", format!("the limit of {name}"));
            Ok(Probed { passed, summary })
        }
    }

    const BOUNDS: Bounds = Bounds { start: 100_000, max_rate: 100_000_000, min_rate: 100 };

    #[test]
    fn it_doubles_bisects_three_times_and_confirms() {
        let outcome = search(BOUNDS, "core_path", model(730_000, &[])).expect("ran");
        // 100k, 200k, 400k pass; 800k fails; bisect 600k (pass), 700k (pass), 750k (fail).
        assert_eq!(outcome.result, Some(700_000));
        assert_eq!(outcome.first_fail, Some(750_000));
        assert_eq!(outcome.resolution, Some(50_000), "1/8 of the 400k step");
        assert_eq!(outcome.median_p99, Some(70_000));
        assert_eq!(outcome.saturation, Some(730_000), "offered 1.4M, achieved the capacity");
        assert!(!outcome.saturation_not_reached);
        assert_eq!(outcome.limit.as_deref(), Some("the limit of 1.4M-saturation"), "the saturated run's");
        assert_eq!(outcome.highest_achieved, Some(730_000));
        let names: Vec<&str> = outcome.trail.iter().map(|t| t.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "100k-probe",
                "200k-probe",
                "400k-probe",
                "800k-probe",
                "600k-bisect",
                "700k-bisect",
                "750k-bisect",
                "700k-confirm1",
                "750k-confirm1",
                "700k-confirm2",
                "750k-confirm2",
                "700k-confirm3",
                "750k-confirm3",
                "1.4M-saturation"
            ]
        );
    }

    #[test]
    fn a_failed_confirmation_steps_down_to_the_next_passing_rate() {
        let outcome = search(BOUNDS, "core_path", model(730_000, &[700_000])).expect("ran");
        assert_eq!(outcome.result, Some(600_000));
        assert_eq!(outcome.first_fail, Some(700_000), "700k failed a confirmation");
        assert_eq!(outcome.resolution, Some(100_000), "the real gap, not 1/8 of the step");
    }

    #[test]
    fn a_saturation_run_that_keeps_up_is_only_a_lower_bound() {
        // Latency-limited: p99 fails above 500k, but every offered rate is achieved.
        let latency_bound = |rate: u64, name: &str| -> Result<Probed, String> {
            let mut summary = Summary::new();
            summary.put("window.achieved_rate", rate);
            summary.put("health.limit", format!("the limit of {name}"));
            Ok(Probed { passed: rate < 500_000, summary })
        };
        let outcome = search(BOUNDS, "core_path", latency_bound).expect("ran");
        assert_eq!(outcome.result, Some(450_000));
        assert_eq!(outcome.saturation, Some(900_000));
        assert!(outcome.saturation_not_reached, "offered 900k, achieved all of it");
        assert_eq!(outcome.highest_achieved, Some(900_000));
    }

    #[test]
    fn if_the_first_rate_fails_it_halves_and_it_gives_up_at_the_floor() {
        let outcome = search(BOUNDS, "core_path", model(30_000, &[])).expect("ran");
        // 100k, 50k fail; 25k passes; bisect 37.5k (fail), 31.25k (fail), 28.125k (pass).
        assert_eq!(outcome.trail[..3].iter().map(|t| t.1).collect::<Vec<_>>(), [100_000, 50_000, 25_000]);
        assert_eq!(outcome.result, Some(28_125));
        let nothing = search(BOUNDS, "core_path", model(50, &[])).expect("ran");
        assert_eq!(nothing.result, None);
        assert!(nothing.note.expect("a note").contains("no rate passed"));
    }

    #[test]
    fn a_search_of_the_polymarket_flow_or_with_a_switch_has_its_own_name_and_says_its_flow() {
        use super::super::flow::Flow;
        let m3 = RunConfig::new(InjectionMode::Signed, 1);
        let polymarket = RunConfig { flow: Flow::polymarket(), ..m3.clone() };
        let makers = RunConfig { flow: polymarket.flow.with_makers(3).expect("a switch"), ..m3.clone() };
        assert_eq!(SearchSpec::core_path(&m3).name, "core-path", "the M3 flow's names are unchanged");
        assert_eq!(SearchSpec::durable("signed", &m3, 1).name, "signed");
        assert_eq!(SearchSpec::core_path(&polymarket).name, "core-path-polymarket");
        let spec = SearchSpec::durable("signed", &makers, 2_500_000);
        assert_eq!(spec.name, "signed-polymarket-makers3");
        let summary = outcome_summary(&spec, &SearchOutcome::default());
        let flow = ["search.flow", "search.makers", "search.bursts", "search.shock"].map(|k| summary.get(k));
        assert_eq!(flow, [Some("polymarket"), Some("3"), Some("none"), Some("none")]);
    }

    #[test]
    fn doubling_stops_at_the_memory_cap() {
        let bounds = Bounds { max_rate: 450_000, ..BOUNDS };
        let outcome = search(bounds, "core_path", model(10_000_000, &[])).expect("ran");
        assert_eq!(outcome.result, Some(400_000));
        assert_eq!(outcome.first_fail, None);
        assert_eq!(outcome.saturation, None);
        assert!(outcome.note.expect("a note").contains("memory cap"));
    }
}
