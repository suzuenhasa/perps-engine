//! Named regression tests for the session report (`docs/PIPELINE.md` 15.5, 15.7 and 15.11).
//! Each test pins a review finding of Milestone 3 that was fixed (PIPELINE.md 22), so that
//! it can't come back. The report's unit tests are in `bench/src/e2e/report.rs`.

use std::path::{Path, PathBuf};

use bench::e2e::report::session_report;
use bench::e2e::results::stage_key;
use bench::e2e::session::LOAD_SWEEP;
use bench::e2e::summary::Summary;
use pipeline::gate::Stage;

/// A fresh session directory for one test.
fn session_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bench-report-regressions-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// A finished run's summary with every client stage's p99 at `p99`; invalid if `invalid`
/// says why.
fn run_summary(name: &str, mode: &str, rate: u64, p99: u64, invalid: Option<&str>) -> Summary {
    let mut s = Summary::new();
    s.put("run.name", name);
    s.put("run.mode", mode);
    s.put("run.journal", "disk");
    s.put("run.rate", rate);
    s.put("run.lanes", 2);
    s.put("check.valid", invalid.is_none());
    s.put("check.invalid", invalid.unwrap_or("-"));
    s.put("window.achieved_rate", rate);
    s.put("window.sequenced_ppm", 1_000_000);
    s.put("window.not_served", 0);
    for stage in Stage::ALL {
        s.put_ns(format!("stage.client.{}.p99", stage_key(stage)), Some(p99));
    }
    s.put_ns("journal.fdatasync.p99", Some(400_000));
    s.put("health.limit", "not saturated (busiest: core at 3.4%)");
    s
}

fn write_run(dir: &Path, group: &str, run: &str, summary: &Summary) {
    let run_dir = dir.join(group).join(run);
    std::fs::create_dir_all(&run_dir).expect("created");
    summary.write(&run_dir.join("summary.txt")).expect("written");
}

// F3-invalid-runs-in-medians: an invalid (here throttled) run's numbers went into its
// sweep point's median and range, although invalid runs are "discarded" (15.7).
#[test]
fn an_invalid_run_is_left_out_of_its_points_median_and_range() {
    let dir = session_dir("f3");
    let point = "preverified-20k";
    write_run(&dir, LOAD_SWEEP, "preverified-20k-r1", &run_summary(point, "preverified", 20_000, 800, None));
    write_run(&dir, LOAD_SWEEP, "preverified-20k-r2", &run_summary(point, "preverified", 20_000, 900, None));
    let throttled =
        run_summary(point, "preverified", 20_000, 50_000, Some("CFS throttling: 3 periods, 90 µs"));
    write_run(&dir, LOAD_SWEEP, "preverified-20k-r3", &throttled);
    let report = session_report(&dir);
    std::fs::remove_dir_all(&dir).expect("cleaned up");
    let rows: Vec<&str> = report.lines().filter(|line| line.starts_with("| preverified-20k |")).collect();
    let used: Vec<&&str> = rows.iter().filter(|row| row.contains("50.0 µs")).collect();
    assert!(used.is_empty(), "the invalid run's 50.0 µs is in the point's median or range: {used:#?}");
    assert!(rows.iter().any(|row| row.starts_with("| preverified-20k | 2/3 |")), "{rows:#?}");
}

// F8-headline-yes-conditions: the headline said "yes" from a single timing run that was
// invalid and whose clock doesn't count toward a headline, although 15.7 asks for 3
// passing repetitions and 15.1 for a clock that counts.
#[test]
fn the_headline_needs_three_valid_timing_runs_whose_clock_counts() {
    let dir = session_dir("f8");
    // The durable limit's pre-verified 20k/s runs: fdatasync p99 400 µs, limit 2.4 ms.
    for r in 1..=3 {
        let summary = run_summary("preverified-20k", "preverified", 20_000, 800, None);
        write_run(&dir, LOAD_SWEEP, &format!("preverified-20k-r{r}"), &summary);
    }
    let timing = |invalid: Option<&str>, clock_counts: bool| {
        let mut timing = run_summary("signed-100k", "signed", 100_000, 1_000_000, invalid);
        timing.put_ns("sender.lag.p99", Some(1_000));
        timing.put("check.throttled_periods", 0);
        timing.put("check.inversions", 0);
        timing.put("check.clock_counts_for_headline", clock_counts);
        timing
    };
    write_run(&dir, "headline", "signed-100k-timing-r1", &timing(Some("the replay differs: slot 7"), false));
    let headline = |dir: &Path| {
        let report = session_report(dir);
        report.lines().find(|line| line.contains("signed orders/s end to end")).unwrap_or("none").to_string()
    };
    let died = headline(&dir);
    assert!(died.contains("not decided yet"), "one invalid run of 3: {died}");

    // Three valid runs, but one whose clock doesn't count toward a headline.
    write_run(&dir, "headline", "signed-100k-timing-r1", &timing(None, true));
    write_run(&dir, "headline", "signed-100k-timing-r2", &timing(None, true));
    write_run(&dir, "headline", "signed-100k-timing-r3", &timing(None, false));
    let slow_clock = headline(&dir);
    std::fs::remove_dir_all(&dir).expect("cleaned up");
    assert!(
        slow_clock.contains("end to end on the M3 flow (D-027): no"),
        "a clock that doesn't count: {slow_clock}"
    );
}
