//! The two ablations of M3: what signature verification on the core would cost, and what
//! group commit saves over one `fdatasync` per order (`docs/PIPELINE.md` section 16).
//!
//! **"Verify on core"** ([`verify_on_core_points`]). Signed mode, offered 5k, 10k, 20k,
//! 40k and 100k/s, both arms, 3 repetitions, interleaved; plus the signed durable-limit
//! search both ways. Both arms carry the 3-line ablation record on the core ring (3.3), so
//! the record's size is not a hidden difference:
//! - **gateways**: the gateways verify, as in every other run;
//! - **core**: the gateways skip checks 11 and 12 and use up the nonce, and the core
//!   verifies every signed command just before applying it. **Insecure**, reachable only
//!   from `e2e ablate`.
//!
//! Each run reports the cores used for verifying and the verifications per busy
//! core-second (`results.rs`, `ablation.*`), so the comparison shows what parallel
//! verification and a free core path are worth, not a cheaper verification. Expected: the
//! core arm's maximum is about `1 / (t_verify + t_core)`, some 12k to 25k/s, so its 20k,
//! 40k and 100k points are overload points.
//!
//! **"Fsync per order"** ([`fsync_points`]). Pre-verified mode (gateways don't limit), real
//! disk, three arms: **per order** (`B` = 1: every record written and synced on its own),
//! **flush when free** (`T` = 0, `B` = 4,096) and **group commit** (`T` = 1 ms, `B` = 4,096,
//! the default). Offered 1k to 100k/s, 3 repetitions, interleaved; plus the pre-verified
//! durable-limit search per arm. The per-order search starts at the probed `1/F` (it can't
//! pass above that), and a per-order run whose backlog hasn't drained 10 s after its window
//! is stopped and counted as a fail: a full journal ring would take `65,536 × F`, about 20
//! s, to drain at `B` = 1. The per-order arm also journals setup one `fdatasync` per
//! record, so its setup barriers may wait `4 × F` per item of the phase, not just 10 s
//! (review finding F10): otherwise, with `F` above about 2.7 ms, phase A's 3,742 items
//! would time out every run.
//!
//! **Complexity.** The runs' durations: about 35 minutes for the first, 65 for the second on
//! PERPSBOX (15.11).

use pipeline::records::InjectionMode;

use super::config::{RunConfig, VerifyArm};
use super::runner::RunError;
use super::search::{SearchSpec, run_search};
use super::session::Session;
use super::sweep::{Point, run_interleaved};
use super::units::{SECOND_NS, short_rate};

/// The "verify on core" rates (16).
pub const VERIFY_RATES: [u64; 5] = [5_000, 10_000, 20_000, 40_000, 100_000];
/// The "fsync per order" rates (16).
pub const FSYNC_RATES: [u64; 7] = [1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000];
/// How long after its window a per-order run may take to drain (16).
pub const DRAIN_CAP_NS: u64 = 10 * SECOND_NS;
pub const VERIFY_GROUP: &str = "ablate-verify-on-core";
pub const FSYNC_GROUP: &str = "ablate-fsync-per-order";

/// The "verify on core" arms.
pub const VERIFY_ARMS: [VerifyArm; 2] = [VerifyArm::Gateways, VerifyArm::Core];

/// The "fsync per order" arms: a name, `T` in nanoseconds, and `B`.
pub const FSYNC_ARMS: [(&str, u64, usize); 3] =
    [("per-order", 0, 1), ("flush-when-free", 0, 4_096), ("group-commit", 1_000_000, 4_096)];

/// One arm of "verify on core" at `rate`, in the template's signing scheme, which must be
/// the perp one: `RunConfig::check` refuses the ablation in the EIP-712 scheme (16).
pub fn verify_arm(template: &RunConfig, arm: VerifyArm, rate: u64) -> RunConfig {
    RunConfig { rate, verify_on_core: Some(arm), ..template.in_mode(InjectionMode::Signed) }
}

/// One arm of "fsync per order" at `rate` (module docs); `flush_ns` is the probed `F`.
pub fn fsync_arm(template: &RunConfig, arm: (&str, u64, usize), rate: u64, flush_ns: u64) -> RunConfig {
    let (_, commit_interval_ns, max_batch) = arm;
    let mut config = RunConfig { rate, ..template.in_mode(InjectionMode::PreVerified) };
    config.journal.commit_interval_ns = commit_interval_ns;
    config.journal.max_batch = max_batch;
    config.drain_cap_ns = (max_batch == 1).then_some(DRAIN_CAP_NS);
    config.barrier_per_item_ns = (max_batch == 1).then_some(4 * flush_ns);
    config
}

/// Every rate, both arms at each (module docs).
pub fn verify_on_core_points(template: &RunConfig, rates: &[u64]) -> Vec<Point> {
    let mut points = Vec::new();
    for &rate in rates {
        for arm in VERIFY_ARMS {
            let config = verify_arm(template, arm, rate);
            points.push(Point { name: format!("{}-{}", arm.name(), short_rate(rate)), ..Point::new(config) });
        }
    }
    points
}

/// Every rate, the three arms at each (module docs); `flush_ns` is the probed `F`.
pub fn fsync_points(template: &RunConfig, rates: &[u64], flush_ns: u64) -> Vec<Point> {
    let mut points = Vec::new();
    for &rate in rates {
        for arm in FSYNC_ARMS {
            let config = fsync_arm(template, arm, rate, flush_ns);
            points.push(Point { name: format!("{}-{}", arm.0, short_rate(rate)), ..Point::new(config) });
        }
    }
    points
}

/// Runs "verify on core" (module docs); with `search`, also the signed durable-limit
/// search both ways, whose limit comes from `sweep_template`'s pre-verified 20k/s point.
pub fn run_verify_on_core(
    session: &mut Session,
    template: &RunConfig,
    search_template: &RunConfig,
    sweep_template: &RunConfig,
    rates: &[u64],
    search: bool,
) -> Result<(), RunError> {
    run_interleaved(session, VERIFY_GROUP, &verify_on_core_points(template, rates))?;
    if search {
        let limit = session.durable_limit(sweep_template)?;
        for arm in VERIFY_ARMS {
            let config = verify_arm(search_template, arm, 0);
            run_search(
                session,
                &SearchSpec::durable(&format!("verify-{}", arm.name()), &config, limit.limit_ns),
            )?;
        }
    }
    Ok(())
}

/// Runs "fsync per order" (module docs); with `search`, also the durable-limit search per
/// arm, the per-order one from `1/F`.
pub fn run_fsync_per_order(
    session: &mut Session,
    template: &RunConfig,
    search_template: &RunConfig,
    sweep_template: &RunConfig,
    rates: &[u64],
    search: bool,
) -> Result<(), RunError> {
    let flush_rate = probed_flush_rate(session);
    let flush_ns = SECOND_NS / flush_rate;
    run_interleaved(session, FSYNC_GROUP, &fsync_points(template, rates, flush_ns))?;
    if search {
        let limit = session.durable_limit(sweep_template)?;
        for arm in FSYNC_ARMS {
            let config = fsync_arm(search_template, arm, 0, flush_ns);
            let mut spec = SearchSpec::durable(&format!("fsync-{}", arm.0), &config, limit.limit_ns);
            if arm.2 == 1 {
                spec.start = flush_rate;
            }
            run_search(session, &spec)?;
        }
    }
    Ok(())
}

/// `1/F`: `fdatasync`s a second at the probed median flush time, from the session's probe,
/// else from its valid pre-verified 20k/s sweep runs; 1,000/s if it has neither (16).
pub fn probed_flush_rate(session: &Session) -> u64 {
    let probe = super::summary::Summary::read(&session.probe_path()).ok();
    let from_probe = probe.and_then(|p| p.ns("fsync.fdatasync.p50").flatten());
    let from_sweep = || {
        let runs = session.summaries(super::session::LOAD_SWEEP);
        runs.iter()
            .filter(|(name, s)| name.starts_with("preverified-20k-r") && s.flag("check.valid") == Some(true))
            .find_map(|(_, s)| s.ns("journal.fdatasync.p50").flatten())
    };
    match from_probe.or_else(from_sweep) {
        Some(f) if f > 0 && f < u64::MAX => (SECOND_NS / f).max(1),
        _ => 1_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pipeline::journal::writer::JournalMode;

    #[test]
    fn verify_on_core_interleaves_its_two_arms_at_every_rate() {
        let template = RunConfig::new(InjectionMode::PreVerified, 1);
        let points = verify_on_core_points(&template, &VERIFY_RATES);
        let names: Vec<&str> = points.iter().map(|p| p.name.as_str()).take(4).collect();
        assert_eq!(names, ["gateways-5k", "core-5k", "gateways-10k", "core-10k"]);
        assert_eq!(points.len(), 10);
        assert!(points.iter().all(|p| p.config.mode == InjectionMode::Signed));
        assert_eq!(points[1].config.verify_on_core, Some(VerifyArm::Core));
        points.iter().for_each(|p| p.config.check().expect("valid"));
    }

    #[test]
    fn fsync_per_order_has_three_arms_and_caps_the_per_order_drain() {
        let template = RunConfig::new(InjectionMode::Signed, 1);
        let points = fsync_points(&template, &FSYNC_RATES, 3_000_000);
        assert_eq!(points.len(), 21);
        let per_order = &points[0].config;
        assert_eq!((per_order.journal.commit_interval_ns, per_order.journal.max_batch), (0, 1));
        assert_eq!(per_order.drain_cap_ns, Some(DRAIN_CAP_NS));
        assert_eq!(per_order.barrier_per_item_ns, Some(12_000_000), "4 × F for its setup barriers");
        assert_eq!(points[1].config.barrier_per_item_ns, None, "batches need no allowance");
        assert_eq!(per_order.mode, InjectionMode::PreVerified);
        assert_eq!(per_order.journal.mode, JournalMode::Disk);
        let group = &points[2].config;
        assert_eq!(
            (group.journal.commit_interval_ns, group.journal.max_batch, group.drain_cap_ns),
            (1_000_000, 4_096, None)
        );
        assert_eq!(points[4].name, "flush-when-free-2k");
    }
}
