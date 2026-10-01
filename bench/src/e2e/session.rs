//! A benchmark session: one directory that holds every run, search and probe of a session
//! on one machine, and the report built from them (`docs/PIPELINE.md` 15.5, 15.7 and
//! 15.11).
//!
//! **Layout.**
//!
//! ```text
//! <session>/probe/summary.txt           the machine probes (e2e probe)
//! <session>/<group>/<run>/summary.txt   one run: key = value (summary.rs)
//! <session>/<group>/<run>/report.md     the same run, rendered
//! <session>/<group>/search.txt          a search's result (search.rs)
//! <session>/report.md                   everything above, rendered (report.rs)
//! ```
//!
//! Groups are `runs` (single runs), `sweep-load`, `sweep-commit-interval`, `sweep-stamps`,
//! `headline`, `search-<name>`, `ablate-verify-on-core` and `ablate-fsync-per-order`.
//!
//! **Contract.**
//! - [`Session::run`] is **resumable** (15.5): a run whose `summary.txt` exists is not run
//!   again, its summary is read back; a run directory without one (a run that died) is
//!   deleted and the run starts over. So a session that dies continues where it stopped,
//!   and the report is always built from the summaries alone. A finished run is reused
//!   only if it ran with the same comparable settings (the config's fingerprint, every
//!   `run.*` key of `RunConfig::fingerprint`): otherwise the run is refused, naming the
//!   setting that differs, rather than reporting another configuration's numbers (review
//!   finding F4-resume-keyed-by-name-only).
//! - [`Session::run_valid`] runs until a run is valid (15.7: an invalid run is "discarded,
//!   reported, and run again"): `name`, then `name-a2`, `name-a3`, ..., at most 3 new runs
//!   per call. Attempts that finished before are read back, and a resumed session numbers
//!   its new attempts after them (review finding F7). Sweeps, searches and the durable
//!   limit all run through it, and reports use valid runs only (review finding F3).
//! - [`Session::open`] reads the probe's results, if the session has them: the jitter
//!   ranking of physical cores (the core and the sequencer take the quietest two, 2.6), and
//!   `t_sign` for the signing estimate (14.8).
//! - [`Session::durable_limit`] is Q1's answer: `2 × T` (the default `T`, 1 ms, whatever is
//!   being swept) plus the median, over the 3 valid runs of the pre-verified 20k/s sweep
//!   point, of the `fdatasync` p99 (15.7). It runs that point first if the session lacks it.
//!   The point is always the M3 flow's, with Poisson arrivals ([`limit_point`]), so a session
//!   that sends both flows (D-034) has one limit, the disk's, for all of them.
//!
//! **Complexity.** Reading a group reads one small file per run.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use loadgen::schedule::Arrivals;
use pipeline::journal::files::create_dir_all_durable;
use pipeline::records::InjectionMode;

use super::config::{RunConfig, recorded_before};
use super::report;
use super::runner::{self, Quiet, RunError};
use super::summary::Summary;
use super::units::latency;
use super::workload::Workloads;

/// The group of the offered-load sweep, whose pre-verified 20k/s point sets the durable
/// limit.
pub const LOAD_SWEEP: &str = "sweep-load";
/// The sweep point that sets the durable limit, and how often it runs (15.7).
pub const LIMIT_RATE: u64 = 20_000;
pub const REPETITIONS: usize = 3;
/// New attempts at a run that keeps coming out invalid, per call of [`Session::run_valid`].
pub const ATTEMPTS: usize = 3;

/// An open session (module docs).
#[derive(Debug)]
pub struct Session {
    pub dir: PathBuf,
    pub workloads: Workloads,
    /// The jitter probe's ranking of physical cores, quietest first.
    pub quietest_first: Vec<usize>,
    /// Runs this process ran, and runs it found already done.
    pub ran: usize,
    pub skipped: usize,
}

/// What [`Session::run_valid`] ended with.
#[derive(Clone, Debug)]
pub struct Attempted {
    /// The valid run's summary, or the last attempt's if none was valid.
    pub summary: Summary,
    pub valid: bool,
    /// Every attempt's run name, in order: the last is `summary`'s.
    pub names: Vec<String>,
}

/// The durable limit and what it was made of (15.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurableLimit {
    pub limit_ns: u64,
    /// The median `fdatasync` p99 of the pre-verified 20k/s runs.
    pub fdatasync_p99_ns: u64,
    pub commit_interval_ns: u64,
}

impl Session {
    /// Opens (or creates) the session in `dir` (module docs).
    pub fn open(dir: &Path) -> io::Result<Session> {
        create_dir_all_durable(dir)?; // the path to every journal of the session (11.1)
        let mut session = Session {
            dir: dir.to_path_buf(),
            workloads: Workloads::default(),
            quietest_first: Vec::new(),
            ran: 0,
            skipped: 0,
        };
        if let Ok(probe) = Summary::read(&session.probe_path()) {
            session.workloads.sign_ns = probe.u64("k256.sign_ns");
            let ranking = probe.get("jitter.quietest_first").unwrap_or("-");
            session.quietest_first = ranking.split(',').filter_map(|cpu| cpu.parse().ok()).collect();
        }
        Ok(session)
    }

    pub fn probe_path(&self) -> PathBuf {
        self.dir.join("probe").join("summary.txt")
    }

    /// Runs `config` as run `name` of `group`, or reads its summary back if it already ran
    /// with the same fingerprint (module docs). Writes the run's `summary.txt` and
    /// `report.md`.
    pub fn run(&mut self, group: &str, name: &str, config: &RunConfig) -> Result<Summary, RunError> {
        let dir = self.dir.join(group).join(name);
        let path = dir.join("summary.txt");
        if let Ok(summary) = Summary::read(&path) {
            check_same_settings(&summary, config, &dir)?;
            self.skipped += 1;
            eprintln!("session: {group}/{name} already ran; reading its summary");
            return Ok(summary);
        }
        if dir.exists() {
            fs::remove_dir_all(&dir)?; // a run that died: start it over
        }
        let result = runner::run(config, &mut self.workloads, &dir, &self.quietest_first, &mut Quiet)?;
        let summary = result.summary();
        fs::write(dir.join("report.md"), report::run_report(&summary))?;
        summary.write(&path)?;
        self.ran += 1;
        print_outcome(group, name, &summary);
        Ok(summary)
    }

    /// Runs `config` as `name` of `group` until a run is valid (module docs): the attempts
    /// are `name`, `name-a2`, `name-a3`, ...; finished ones are read back, and at most
    /// [`ATTEMPTS`] new ones run. Every attempt stays in the session.
    pub fn run_valid(&mut self, group: &str, name: &str, config: &RunConfig) -> Result<Attempted, RunError> {
        let (mut names, mut new_runs, mut attempt) = (Vec::new(), 0, 1);
        loop {
            let run_name = if attempt == 1 { name.to_string() } else { format!("{name}-a{attempt}") };
            new_runs += usize::from(!self.has_run(group, &run_name));
            let summary = self.run(group, &run_name, config)?;
            names.push(run_name);
            let valid = summary.flag("check.valid") == Some(true);
            if valid || new_runs == ATTEMPTS {
                return Ok(Attempted { summary, valid, names });
            }
            attempt += 1;
        }
    }

    /// True if run `name` of `group` has finished: its summary exists.
    pub fn has_run(&self, group: &str, name: &str) -> bool {
        self.dir.join(group).join(name).join("summary.txt").is_file()
    }

    /// Every run of `group` that has a summary, by name.
    pub fn summaries(&self, group: &str) -> Vec<(String, Summary)> {
        read_group(&self.dir.join(group))
    }

    /// Q1's durable limit (module docs), running the pre-verified 20k/s sweep point first if
    /// the session hasn't. `template` gives the runs' timing and threads ([`limit_point`]).
    pub fn durable_limit(&mut self, template: &RunConfig) -> Result<DurableLimit, RunError> {
        let config = limit_point(template);
        let default = config.journal;
        let mut p99s = Vec::new();
        for rep in 1..=REPETITIONS {
            let attempted = self.run_valid(LOAD_SWEEP, &format!("{}-r{rep}", config.name()), &config)?;
            if attempted.valid {
                p99s.push(attempted.summary.ns("journal.fdatasync.p99").flatten().unwrap_or(u64::MAX));
            }
        }
        if p99s.is_empty() {
            return Err(RunError::Refused(
                "the durable limit needs a valid pre-verified 20k/s run, and every attempt was invalid (see \
                 the report's list of invalid runs)"
                    .into(),
            ));
        }
        let fdatasync_p99_ns = median(&mut p99s);
        let limit = DurableLimit {
            limit_ns: (2 * default.commit_interval_ns).saturating_add(fdatasync_p99_ns),
            fdatasync_p99_ns,
            commit_interval_ns: default.commit_interval_ns,
        };
        eprintln!(
            "session: the durable limit is 2 × T ({}) + the median fdatasync p99 ({}) = {}",
            latency(Some(default.commit_interval_ns)),
            latency(Some(fdatasync_p99_ns)),
            latency(Some(limit.limit_ns))
        );
        Ok(limit)
    }

    /// Rebuilds `<session>/report.md` from everything in the session (report.rs).
    pub fn write_report(&self) -> io::Result<PathBuf> {
        let path = self.dir.join("report.md");
        fs::write(&path, report::session_report(&self.dir))?;
        Ok(path)
    }
}

/// The load sweep's point that sets the durable limit (module docs; 15.7): `template`'s
/// timing and threads in pre-verified mode at 20k/s, with the default journal, and always
/// the M3 flow (the template's own, or the M3 flow with its seed) with no bursts, whatever
/// flow and switches the command at hand sends (D-034). The limit is the disk's, so a
/// session has one, and every flow's headline and search is judged against it; and the
/// point keeps its name, `preverified-20k`, which is where the report looks for it.
pub fn limit_point(template: &RunConfig) -> RunConfig {
    let mut config = RunConfig { rate: LIMIT_RATE, ..template.in_mode(InjectionMode::PreVerified) };
    config.journal = RunConfig::new(InjectionMode::PreVerified, LIMIT_RATE).journal;
    config.flow = template.flow.m3_counterpart();
    if config.bursts().is_some() {
        config.arrivals = Arrivals::Poisson;
    }
    config
}

/// A finished run is reused only if every comparable setting matches (module docs). A key
/// added to the fingerprint after the run reads as what every run then had
/// (`config::recorded_before`).
fn check_same_settings(summary: &Summary, config: &RunConfig, dir: &Path) -> Result<(), RunError> {
    for (key, value) in config.fingerprint() {
        let recorded = summary.get(key).or_else(|| recorded_before(key, summary)).unwrap_or("(not recorded)");
        if recorded != value {
            return Err(RunError::Refused(format!(
                "{} already ran with {key} = {recorded}, but this run has {key} = {value}: use another \
                 session directory (--dir), or delete that run directory to run it again",
                dir.display()
            )));
        }
    }
    Ok(())
}

/// Every run directory in `group_dir` that holds a summary, sorted by name.
pub fn read_group(group_dir: &Path) -> Vec<(String, Summary)> {
    let Ok(entries) = fs::read_dir(group_dir) else { return Vec::new() };
    let mut runs: Vec<(String, Summary)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let summary = Summary::read(&entry.path().join("summary.txt")).ok()?;
            Some((entry.file_name().to_string_lossy().into_owned(), summary))
        })
        .collect();
    runs.sort_by(|a, b| a.0.cmp(&b.0));
    runs
}

/// The median of `values` (the middle one; the lower middle for an even count).
pub fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values.get(values.len().saturating_sub(1) / 2).copied().unwrap_or(0)
}

/// One line per finished run, on standard error.
fn print_outcome(group: &str, name: &str, summary: &Summary) {
    let valid = summary.flag("check.valid") == Some(true);
    let verdict = if valid {
        "valid".to_string()
    } else {
        format!("INVALID ({})", summary.get("check.invalid").unwrap_or("?"))
    };
    // Without stamps the gate can't see the window: the wall-time rate is all there is.
    let rate = match summary.get("run.stamps") {
        Some("off") => format!("released {}/s", summary.u64("window.released_per_second").unwrap_or(0)),
        _ => format!("achieved {}/s", summary.u64("window.achieved_rate").unwrap_or(0)),
    };
    eprintln!("session: {group}/{name}: {verdict}; {rate}; {}", summary.get("health.limit").unwrap_or("-"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_median_is_the_middle_value() {
        assert_eq!(median(&mut [3, 1, 2]), 2);
        assert_eq!(median(&mut [5, u64::MAX, 1]), 5);
        assert_eq!(median(&mut [4, 1]), 1);
        assert_eq!(median(&mut []), 0);
    }

    /// A session in a fresh directory, with a finished run `name` of `group` that ran
    /// `config`, valid or not.
    fn finished(dir: &Path, group: &str, name: &str, config: &RunConfig, valid: bool) {
        let mut summary = Summary::new();
        for (key, value) in config.fingerprint() {
            summary.put(key, value);
        }
        summary.put("check.valid", valid);
        fs::create_dir_all(dir.join(group).join(name)).expect("created");
        summary.write(&dir.join(group).join(name).join("summary.txt")).expect("written");
    }

    #[test]
    fn a_finished_run_is_reused_only_with_the_same_settings() {
        let dir = std::env::temp_dir().join(format!("bench-session-fingerprint-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let config = RunConfig::new(InjectionMode::Signed, 100_000);
        finished(&dir, "search-signed", "100k-probe", &config, true);
        let mut session = Session::open(&dir).expect("opened");
        session.run("search-signed", "100k-probe", &config).expect("the same settings: read back");
        assert_eq!((session.ran, session.skipped), (0, 1));
        let mut smt = config.clone();
        smt.cpus = super::super::config::Cpus::Pinned { gateway_smt: true, overrides: Vec::new() };
        let error = session.run("search-signed", "100k-probe", &smt).expect_err("another layout");
        assert!(
            error.to_string().contains("run.cpus = pinned, but this run has run.cpus = pinned, gateway-smt"),
            "{error}"
        );
        fs::remove_dir_all(&dir).expect("removed");
    }

    #[test]
    fn a_run_finished_before_the_signing_scheme_existed_is_reused_as_a_perp_run_only() {
        let dir = std::env::temp_dir().join(format!("bench-session-before-auth-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let config = RunConfig::new(InjectionMode::Signed, 100_000);
        // Its summary has every key of today's fingerprint but `run.auth`.
        let mut summary = Summary::new();
        for (key, value) in config.fingerprint().into_iter().filter(|(key, _)| *key != "run.auth") {
            summary.put(key, value);
        }
        summary.put("check.valid", true);
        fs::create_dir_all(dir.join("search-signed").join("100k-probe")).expect("created");
        summary.write(&dir.join("search-signed").join("100k-probe").join("summary.txt")).expect("written");
        let mut session = Session::open(&dir).expect("opened");
        session.run("search-signed", "100k-probe", &config).expect("a perp run: read back");
        let eip712 = RunConfig { auth: pipeline::records::AuthScheme::Eip712, ..config };
        let error = session.run("search-signed", "100k-probe", &eip712).expect_err("another scheme");
        assert!(error.to_string().contains("run.auth = perp, but this run has run.auth = eip712"), "{error}");
        fs::remove_dir_all(&dir).expect("removed");
    }

    #[test]
    fn a_resumed_run_until_valid_goes_on_after_the_attempts_already_made() {
        let dir = std::env::temp_dir().join(format!("bench-session-attempts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let config = RunConfig::new(InjectionMode::PreVerified, 20_000);
        finished(&dir, LOAD_SWEEP, "preverified-20k-r1", &config, false);
        finished(&dir, LOAD_SWEEP, "preverified-20k-r1-a2", &config, false);
        finished(&dir, LOAD_SWEEP, "preverified-20k-r1-a3", &config, true);
        let mut session = Session::open(&dir).expect("opened");
        let attempted = session.run_valid(LOAD_SWEEP, "preverified-20k-r1", &config).expect("read back");
        assert!(attempted.valid);
        assert_eq!(attempted.names, ["preverified-20k-r1", "preverified-20k-r1-a2", "preverified-20k-r1-a3"]);
        assert_eq!(session.ran, 0, "nothing ran: the third attempt was valid");
        fs::remove_dir_all(&dir).expect("removed");
    }

    #[test]
    fn the_durable_limit_is_the_m3_flows_plain_point_whatever_the_template_sends() {
        use super::super::flow::Flow;
        use loadgen::schedule::Bursts;
        let m3 = RunConfig::new(InjectionMode::Signed, 100_000);
        let polymarket = RunConfig {
            flow: Flow::polymarket().with_makers(3).expect("a switch").with_seed(5),
            arrivals: Arrivals::Cox(Bursts::Median),
            ..m3.clone()
        };
        let limit = limit_point(&polymarket);
        assert_eq!(limit.name(), "preverified-20k");
        assert_eq!((limit.flow, limit.arrivals), (Flow::m3().with_seed(5), Arrivals::Poisson));
        assert_eq!(
            limit_point(&m3.clone()).fingerprint(),
            limit_point(&RunConfig { flow: Flow::polymarket(), ..m3 }).fingerprint()
        );
    }

    #[test]
    fn a_group_is_read_from_the_summaries_its_runs_left() {
        let dir = std::env::temp_dir().join(format!("bench-session-group-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (name, rate) in [("b-r1", 2), ("a-r1", 1)] {
            let mut summary = Summary::new();
            summary.put("run.rate", rate);
            fs::create_dir_all(dir.join(name)).expect("created");
            summary.write(&dir.join(name).join("summary.txt")).expect("written");
        }
        fs::create_dir_all(dir.join("died")).expect("a run without a summary");
        let runs = read_group(&dir);
        let names: Vec<&str> = runs.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["a-r1", "b-r1"]);
        assert_eq!(runs[1].1.u64("run.rate"), Some(2));
        fs::remove_dir_all(&dir).expect("removed");
    }
}
