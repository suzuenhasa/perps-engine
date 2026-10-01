//! What one run measured, whether it counts, and its `summary.txt` (`docs/PIPELINE.md`
//! 15.4, 15.5, 15.7, 15.9 and 18.4).
//!
//! **Contract.**
//! - [`RunResult`] holds everything the threads returned and main saw: the sender's,
//!   gateways' and pipeline's statistics, the window's samples, the engine's snapshot, and
//!   the replay test's and the audit's answers.
//! - [`RunResult::verdict`] applies the rules of 15.7 and 15.4. A run is **invalid**, and
//!   says nothing about the pipeline, if the kernel throttled it, a clock stamp went
//!   backwards, the sender was the limit (generator-limited), the sender didn't finish its
//!   plan (a barrier timed out), the engine rejected a setup command (14.4 requires none:
//!   the flow's state would not be the one intended), in the EIP-712 scheme a gateway
//!   refused one of the harness's own messages for its timestamp, as a replay or for want
//!   of room ([`RunResult::harness_rejects`]; 5.8), or the counts don't add up, or its
//!   own consistency checks failed (the fund's equity, the replay test, the audit): those
//!   last ones are bugs. Other
//!   findings are **flags**, printed with the run: page faults on hot threads in the window
//!   (with the pages the kernel migrated meanwhile, if any: such faults may be the
//!   kernel's, `checks.rs`), an engine reject share above 5%, a clock that doesn't count
//!   toward a headline, a file system where durability means nothing, a journal that ran
//!   out of preallocated segments, an incomplete capture, a discarded journal, a flow with
//!   shocks (D-034) whose window held none.
//! - **The counts must add up** (18.4): offered = sent + dropped; sent = forwarded + gateway
//!   rejects (signed); forwarded + operator = sequenced = journaled = applied = released;
//!   and in the window, offered = sequenced + never served.
//! - [`RunResult::summary`] writes all of it as `key = value` lines (`summary.rs`), from
//!   which every report is built.
//! - **The flow's shape in the window** (D-034; [`FlowContent`], the `flow.*` keys), counted
//!   from the plan before the run: the busiest account's share and rate, and its gateway
//!   (whose busy share `health.*` gives: with `--makers K` one account's gateway, not the
//!   gateway count, can set the signed ceiling); the messages each lane was offered; the
//!   takers' IOCs a second, and the fills per IOC; and how concentrated the messages and the
//!   takers' IOCs were across markets ([`MarketShares`]). With the gate's longest `SetMark`
//!   core service (`stage.set_mark_core_service`) and the most events of one command
//!   (`breakdown.window.max_events_per_command`), they are what D-034 asks each run to
//!   report.
//!
//! **Complexity.** Linear in the number of histograms and counters (a few hundred keys).

use std::path::PathBuf;

use engine::command::{Command, PlaceOrder};
use engine::engine::EngineSnapshot;
use engine::types::{AccountId, MarketId, TimeInForce};
use gateway::audit::AuditReport;
use gateway::thread::RejectCounts;
use gateway::{GatewayReject, GatewayStats, KeyRegistry, gateway_of};
use loadgen::market_flow::{FlowPhase, FlowPlan, HIGH_LEVERAGE_BASE, Item, PlanConfig, TAKER_BASE};
use loadgen::sender::{SenderEnd, SenderPlan, SenderStats};
use pipeline::affinity::CpuLayout;
use pipeline::codec::{cancel_reason_from_code, reject_reason_from_code};
use pipeline::gate::{Breakdown, CANCEL_REASONS, REJECT_REASONS, STAGES, Stage, StageHistograms};
use pipeline::histogram::LatencyHistogram;
use pipeline::journal::format::build_commit;
use pipeline::records::{InjectionMode, Stamps};
use pipeline::replay::ReplayVerdict;
use pipeline::run::PipelineOutput;

use super::checks::WindowHealth;
use super::config::{RunConfig, VerifyArm};
use super::probes::{ClockCheck, FsVerdict, Mount, fs_verdict};
use super::runner::Monitor;
use super::session::median;
use super::summary::Summary;
use super::units::{SECOND_NS, latency, percent_ppm, ppm};

/// The two end-to-end histograms of client commands before the commands never served were
/// added: "of completed", next to "over offered" at overload points (15.4).
#[derive(Clone, Debug)]
pub struct Completed {
    pub to_core_result: LatencyHistogram,
    pub to_durable_ack: LatencyHistogram,
}

impl Completed {
    /// Copies the two histograms; the caller adds the never-served commands afterwards.
    pub fn before_not_served(output: &PipelineOutput) -> Completed {
        Completed {
            to_core_result: output.stats.client.get(Stage::ToCoreResult).clone(),
            to_durable_ack: output.stats.client.get(Stage::ToDurableAck).clone(),
        }
    }
}

/// What the plan offered inside the window (14.1, 15.9; D-034): its jumps, its IOC places,
/// and who sent what where. Counted from the plan and the send schedule before the run,
/// since a trailer can't tell a GTC place from an IOC, nor say which account sent a
/// command or in which market. What the engine did with them (fills, rejects) comes from
/// the gate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlowContent {
    /// Jumps whose mark is in the window; in the Polymarket-shaped flow, shock moves too.
    pub jumps: u64,
    /// IOC places: the takers', the high-leverage accounts' and the cascade cohort's.
    pub ioc_places: u64,
    /// The takers' IOCs: accounts `TAKER_BASE` to `HIGH_LEVERAGE_BASE − 1`, in both flows.
    pub taker_iocs: u64,
    /// Client items.
    pub clients: u64,
    /// Client items per lane, `account mod N`: the gateway that verifies them in a signed
    /// run, the lane they go into in a pre-verified one (14.10).
    pub per_lane: Vec<u64>,
    /// The account that offered the most client items, and how many (the lowest id on a
    /// tie): with `--makers K`, one of the `K` makers, whose one gateway may be the limit.
    pub busiest_account: Option<(AccountId, u64)>,
    /// Per market with any, in id order: its client items and taker IOCs.
    pub per_market: Vec<MarketCount>,
}

/// One market's client items and taker IOCs in the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarketCount {
    pub market: MarketId,
    pub clients: u64,
    pub taker_iocs: u64,
}

impl FlowContent {
    /// What `plan` offers inside the window of `sender_plan`, over `lanes` lanes.
    pub fn of<C>(plan: &FlowPlan<C>, sender_plan: &SenderPlan, lanes: usize) -> FlowContent {
        let lanes = lanes.max(1);
        let mut content = FlowContent { per_lane: vec![0; lanes], ..FlowContent::default() };
        let Some(timed) = sender_plan.phases.iter().find(|p| p.phase == FlowPhase::Timed) else {
            return content;
        };
        let in_window =
            |item: usize| timed.schedule.get(item).is_some_and(|t| sender_plan.window.contains(t));
        content.jumps = plan.jumps.iter().filter(|jump| in_window(jump.item)).count() as u64;
        // Counts indexed by account id and by market id, grown as ids appear.
        let (mut per_account, mut per_market) = (Vec::<u64>::new(), Vec::<MarketCount>::new());
        for (i, item) in plan.timed[..timed.schedule.len()].iter().enumerate() {
            let Item::Client(client) = item else { continue };
            if !in_window(i) {
                continue;
            }
            content.clients += 1;
            content.per_lane[gateway_of(client.account, lanes)] += 1;
            let account = client.account as usize;
            if per_account.len() <= account {
                per_account.resize(account + 1, 0);
            }
            per_account[account] += 1;
            let market = client_market(&client.command);
            if per_market.len() <= usize::from(market) {
                per_market.resize(usize::from(market) + 1, MarketCount::default());
            }
            let counts = &mut per_market[usize::from(market)];
            counts.market = market;
            counts.clients += 1;
            if is_ioc(item) {
                content.ioc_places += 1;
                if (TAKER_BASE..HIGH_LEVERAGE_BASE).contains(&client.account) {
                    content.taker_iocs += 1;
                    counts.taker_iocs += 1;
                }
            }
        }
        // The first of the largest counts: the lowest id on a tie.
        let mut busiest: Option<(AccountId, u64)> = None;
        for (account, &n) in per_account.iter().enumerate() {
            if n > busiest.map_or(0, |(_, most)| most) {
                busiest = Some((account as AccountId, n));
            }
        }
        content.busiest_account = busiest;
        content.per_market = per_market.into_iter().filter(|counts| counts.clients > 0).collect();
        content
    }
}

/// True for a client's IOC place.
fn is_ioc(item: &Item) -> bool {
    let Item::Client(client) = item else { return false };
    matches!(client.command, Command::PlaceOrder(PlaceOrder { tif: TimeInForce::Ioc, .. }))
}

/// The market of a client command: a place, a cancel or a modify. (A client sends nothing
/// else; 0, no market, if it ever did.)
fn client_market(command: &Command) -> MarketId {
    match command {
        Command::PlaceOrder(place) => place.market,
        Command::CancelOrder(cancel) => cancel.market,
        Command::ModifyOrder(modify) => modify.market,
        _ => 0,
    }
}

/// How one count spreads over a flow's markets (D-034, "Activity across markets"; the
/// recorded maker messages have their top 10 markets at about 23%, the takers' at about
/// 53%): the busiest market and its share, the top 10's share, and the median market's
/// share, over every market of the flow, those with none counting as 0. Shares in ppm of
/// the total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarketShares {
    pub top: Option<MarketId>,
    pub top_ppm: u64,
    pub top10_ppm: u64,
    pub median_ppm: u64,
    /// Markets with any.
    pub active: usize,
}

impl MarketShares {
    /// From each market's count (`(market, n)`, markets with none may be left out), over a
    /// flow of `markets` markets.
    pub fn of(counts: impl Iterator<Item = (MarketId, u64)>, markets: usize) -> MarketShares {
        let mut counts: Vec<(MarketId, u64)> = counts.filter(|&(_, n)| n > 0).collect();
        // Largest first; the lowest id first on a tie.
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let total: u64 = counts.iter().map(|&(_, n)| n).sum();
        let mut all: Vec<u64> = counts.iter().map(|&(_, n)| n).collect();
        all.resize(markets.max(all.len()), 0);
        MarketShares {
            top: counts.first().map(|&(market, _)| market),
            top_ppm: ppm(counts.first().map_or(0, |&(_, n)| n), total),
            top10_ppm: ppm(counts.iter().take(10).map(|&(_, n)| n).sum(), total),
            median_ppm: ppm(median(&mut all), total),
            active: counts.len(),
        }
    }
}

/// The kernel's throttling of the container during the run (2.6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Throttled {
    pub periods: u64,
    pub us: u64,
}

/// Everything one run measured (module docs).
#[derive(Debug)]
pub struct RunResult {
    pub config: RunConfig,
    pub dir: PathBuf,
    /// This life's first seq: 1, or the recovered journal's next seq after a restart.
    pub first_seq: u64,
    pub layout: CpuLayout,
    pub clock: ClockCheck,
    /// The file system holding the run directory, if it could be read.
    pub mount: Option<Mount>,
    pub throttled: Throttled,
    pub sender: SenderStats,
    pub gateways: Vec<GatewayStats>,
    /// The pipeline's output, with the never-served commands already added (15.4).
    pub output: PipelineOutput,
    pub completed: Completed,
    pub monitor: Monitor,
    pub content: FlowContent,
    /// From the pipeline's start (after preallocation) to its join, in nanoseconds.
    pub duration_ns: u64,
    pub snapshot: EngineSnapshot,
    pub registry: Option<KeyRegistry>,
    pub replay: Option<ReplayVerdict>,
    pub audit: Option<AuditReport>,
}

/// Whether a run counts, and what to say about it (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Why the run is invalid; empty if it is valid.
    pub invalid: Vec<String>,
    pub flags: Vec<String>,
}

/// An engine reject share above this is flagged (INFO.md 7, 15.9), in ppm.
pub const REJECT_SHARE_FLAG_PPM: u64 = 50_000;

impl RunResult {
    /// The rules of 15.7 and 15.4 (module docs).
    pub fn verdict(&self) -> Verdict {
        let mut v = Verdict::default();
        if self.throttled.periods > 0 {
            v.invalid.push(format!(
                "CFS throttling: {} periods, {} µs",
                self.throttled.periods, self.throttled.us
            ));
        }
        let inversions: u64 = self.output.stats.inversions.iter().sum();
        if inversions > 0 {
            v.invalid.push(format!("{inversions} clock inversions (15.2)"));
        }
        if self.sender.generator_limited() {
            let p99 = latency(self.sender.lag.percentile(99, 100));
            let why = format!("generator-limited: sender lag p99 {p99} is above 5 µs (14.10)");
            if self.config.allow_generator_limited {
                v.flags.push(format!("{why}; allowed for development (--allow-generator-limited)"));
            } else {
                v.invalid.push(why);
            }
        }
        if self.sender.end != SenderEnd::Finished && !self.monitor.drain_cap_exceeded {
            v.invalid.push(format!("the sender didn't finish: {:?}", self.sender.end));
        }
        if let Some(why) = self.setup_rejects() {
            v.invalid.push(why);
        }
        if let Some(why) = self.harness_rejects() {
            v.invalid.push(why);
        }
        v.invalid.extend(self.count_mismatches());
        if !self.fund_consistent() {
            v.invalid.push("the gate's fund equity differs from the engine snapshot's (15.9)".to_string());
        }
        if let Some(ReplayVerdict::Different(difference)) = &self.replay {
            v.invalid.push(format!("the replay differs: {difference}"));
        }
        if let Some(audit) = self.audit.as_ref().filter(|audit| !audit.passed()) {
            v.invalid.push(format!("the signature audit found {} failures", audit.failures));
        }
        v.flags.extend(self.flags());
        v
    }

    /// Findings that don't invalidate the run (module docs).
    fn flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        let health = self.monitor.health();
        match health.as_ref().map(WindowHealth::total_faults) {
            Some(Some(0)) => {}
            Some(Some(faults)) => {
                let mut flag = format!("{faults} minor page faults on hot threads in the window");
                // Faults may be the kernel's, not a first touch (15.4, `checks.rs`).
                if let Some(pages) =
                    health.as_ref().and_then(|h| h.page_migrations).filter(|&pages| pages > 0)
                {
                    flag.push_str(&format!(", while the kernel migrated {pages} pages"));
                }
                flags.push(flag)
            }
            Some(None) => {
                flags.push("page faults in the window unknown (a thread's /proc entry was gone)".into())
            }
            None => flags.push("the measured window never opened".into()),
        }
        if self.monitor.closed_early {
            flags.push("the flow ended before the window closed".into());
        }
        // Shocks come every 10 s of flow time (D-034), and flow time runs `rate / 100,000`
        // times as fast as real time, so a short window, or a low rate, can hold none.
        if self.config.flow.shock().is_some() && self.content.jumps == 0 {
            flags.push("the flow has shocks (--shock), but none fell in the window".into());
        }
        let share = self.reject_share_ppm();
        if share > REJECT_SHARE_FLAG_PPM {
            flags.push(format!(
                "engine rejects {} of client commands in the window (above 5%)",
                percent_ppm(share)
            ));
        }
        if !self.clock.counts_for_headline() {
            flags.push(format!(
                "clock {} at {} ps a read doesn't count toward a headline (15.1)",
                self.clock.source, self.clock.read_ps
            ));
        }
        if let Some(mount) = &self.mount {
            match fs_verdict(mount) {
                FsVerdict::Refused(why) if !self.config.discarded() => {
                    flags.push(format!("durable numbers mean nothing here: {why}"))
                }
                FsVerdict::CopyOnWrite(why) => flags.push(why),
                _ => {}
            }
        }
        if self.output.journal.segments_created > 0 {
            flags.push(format!(
                "the writer created {} segments itself: preallocation ran out (11.6)",
                self.output.journal.segments_created
            ));
        }
        if self.output.stats.capture_incomplete {
            flags.push("capture incomplete".into());
        }
        if self.config.discarded() {
            flags.push("journal discarded".into());
        }
        if self.monitor.drain_cap_exceeded {
            flags.push("the backlog didn't drain within the cap: a fail".into());
        }
        flags
    }

    /// 14.4: the engine must accept every setup command (phases A, B1 and B2), or the run
    /// measures another state than the flow meant (review finding F12). The trailers tell
    /// setup by `t_sched`, which they carry only with stamps on, so with stamps off there is
    /// nothing to check.
    pub fn setup_rejects(&self) -> Option<String> {
        if self.config.stamps == Stamps::Off {
            return None;
        }
        let setup = &self.output.stats.setup;
        let mut reasons = Vec::new();
        for (tag, by_reason) in setup.rejected.iter().enumerate() {
            for (code, &n) in by_reason.iter().enumerate().filter(|(_, n)| **n > 0) {
                let reason = reject_reason_from_code(code as u8).expect("a reject code the gate counted");
                reasons.push(format!("{n} {} {reason:?}", tag_key(tag)));
            }
        }
        (!reasons.is_empty()).then(|| {
            format!("the engine rejected setup commands (14.4 requires none): {}", reasons.join(", "))
        })
    }

    /// The EIP-712 scheme (5.8): the load generator signs each request once, fresh, and the
    /// runner gives each gateway's table room for every one, so a gateway refusing one for
    /// its timestamp, as a replay, or for want of room means the harness failed (the
    /// messages went stale, or a table was too small), and the run measured something else
    /// (module docs). These reasons never occur in the perp scheme.
    pub fn harness_rejects(&self) -> Option<String> {
        let reasons = [
            GatewayReject::StaleTimestamp,
            GatewayReject::FutureTimestamp,
            GatewayReject::ReusedRequest,
            GatewayReject::SaltTableFull,
        ];
        let found: Vec<String> = reasons
            .into_iter()
            .filter_map(|reason| {
                let n: u64 = self.gateways.iter().map(|g| g.rejects.get(reason)).sum();
                (n > 0).then(|| format!("{n} {reason}"))
            })
            .collect();
        (!found.is_empty()).then(|| {
            format!("the gateways refused the harness's own EIP-712 messages (5.8): {}", found.join(", "))
        })
    }

    /// Client commands the engine rejected, as a share of those sequenced in the window.
    pub fn reject_share_ppm(&self) -> u64 {
        let window = &self.output.stats.window;
        ppm(window.client_rejects(), window.client_commands())
    }

    /// True if the gate's final fund equity equals the snapshot's (15.9).
    pub fn fund_consistent(&self) -> bool {
        let snapshot = &self.snapshot;
        self.output.stats.fund.final_equity == i128::from(snapshot.fund_balance) + snapshot.fund_upnl_total
    }

    /// Every way the counts fail to add up (module docs).
    pub fn count_mismatches(&self) -> Vec<String> {
        let (sender, output) = (&self.sender, &self.output);
        let mut mismatches = Vec::new();
        let mut require = |holds: bool, what: String| {
            if !holds {
                mismatches.push(what);
            }
        };
        let dropped = sender.dropped_total();
        require(
            sender.offered == sender.client_sent + dropped + sender.operator_sent,
            format!(
                "offered {} != sent {} + dropped {dropped} + operator {}",
                sender.offered, sender.client_sent, sender.operator_sent
            ),
        );
        let forwarded: u64 = self.gateways.iter().map(|g| g.forwarded).sum();
        let rejected: u64 = self.gateways.iter().map(|g| g.rejects.total()).sum();
        let into_lanes = match self.config.mode {
            InjectionMode::Signed => {
                require(
                    sender.client_sent == forwarded + rejected,
                    format!(
                        "sent {} != forwarded {forwarded} + gateway rejects {rejected}",
                        sender.client_sent
                    ),
                );
                forwarded
            }
            InjectionMode::PreVerified => sender.client_sent,
        };
        let stages = [
            ("sequenced", output.sequencer.records),
            ("journaled", output.journal.records),
            ("applied", output.core.commands),
            ("released", output.stats.commands),
        ];
        for (what, n) in stages {
            require(
                n == into_lanes + sender.operator_sent,
                format!("{what} {n} != client {into_lanes} + operator {}", sender.operator_sent),
            );
        }
        // With stamps off the trailers carry no `t_sched`, so the gate can't tell which
        // commands were in the window (15.1): only the totals can be compared.
        if self.config.stamps == Stamps::On {
            let window_sequenced = output.stats.window.client_commands();
            let not_served = self.window_not_served();
            require(
                sender.window_offered == window_sequenced + not_served,
                format!(
                    "in the window, offered {} != sequenced {window_sequenced} + never served {not_served}",
                    sender.window_offered
                ),
            );
        }
        mismatches
    }

    /// Client commands offered in the window and never served: dropped at ingress, or
    /// rejected by a gateway (15.4).
    pub fn window_not_served(&self) -> u64 {
        self.sender.window_dropped + self.gateways.iter().map(|g| g.window_rejects.total()).sum::<u64>()
    }

    /// The run's results as `key = value` lines (module docs; the keys are listed in the
    /// functions below).
    pub fn summary(&self) -> Summary {
        let mut s = Summary::new();
        self.put_run(&mut s);
        self.put_counts(&mut s);
        self.put_flow(&mut s);
        self.put_stages(&mut s);
        self.put_threads(&mut s);
        put_breakdown(&mut s, "breakdown.setup", &self.output.stats.setup);
        put_breakdown(&mut s, "breakdown.window", &self.output.stats.window);
        self.put_fund(&mut s);
        self.put_health(&mut s);
        self.put_checks(&mut s);
        self.put_replay_and_audit(&mut s);
        s
    }

    /// `run.*` and `machine.*`: what was run, and where. The comparable settings are the
    /// config's fingerprint, which a resumed session checks (`config.rs`).
    fn put_run(&self, s: &mut Summary) {
        let c = &self.config;
        s.put("run.name", c.name());
        for (key, value) in c.fingerprint() {
            s.put(key, value);
        }
        s.put("run.flow_seed", c.flow.seed());
        s.put("run.flow_markets", c.flow.markets());
        s.put("run.first_seq", self.first_seq);
        s.put("run.duration_ns", self.duration_ns);
        s.put("machine.clock_source", &self.clock.source);
        s.put("machine.clock_read_ps", self.clock.read_ps);
        s.put("machine.core_mhz", self.monitor.core_mhz.map_or("unknown".to_string(), |mhz| mhz.to_string()));
        let pins: Vec<String> =
            self.layout.pins().iter().map(|(role, cpu)| format!("{role}={cpu}")).collect();
        s.put("machine.layout", if pins.is_empty() { "unpinned".to_string() } else { pins.join(", ") });
        if let Some(mount) = &self.mount {
            s.put("machine.fs", format!("{} on {}", mount.fs_type, mount.mount_point));
        }
        let commit = String::from_utf8_lossy(&build_commit()).trim_end_matches('\0').to_string();
        s.put("machine.commit", if commit.is_empty() { "unknown".to_string() } else { commit });
        s.put("machine.profile", if cfg!(debug_assertions) { "debug" } else { "release" });
    }

    /// `counts.*` and `window.*`: the counts of 18.4 and the window's rate (15.5).
    fn put_counts(&self, s: &mut Summary) {
        let (sender, output) = (&self.sender, &self.output);
        s.put("counts.offered", sender.offered);
        s.put("counts.client_sent", sender.client_sent);
        s.put("counts.dropped", sender.dropped_total());
        s.put("counts.operator_sent", sender.operator_sent);
        s.put("counts.forwarded", self.gateways.iter().map(|g| g.forwarded).sum::<u64>());
        s.put("counts.gateway_rejects", self.gateways.iter().map(|g| g.rejects.total()).sum::<u64>());
        s.put("counts.sequenced", output.sequencer.records);
        s.put("counts.journaled", output.journal.records);
        s.put("counts.applied", output.core.commands);
        s.put("counts.released", output.stats.commands);
        s.put("counts.events", output.stats.events);
        s.put("counts.captured", output.stats.captured);
        let sequenced = output.stats.window.client_commands();
        s.put("window.offered", sender.window_offered);
        s.put("window.dropped", sender.window_dropped);
        s.put("window.gateway_rejects", self.gateways.iter().map(|g| g.window_rejects.total()).sum::<u64>());
        s.put("window.not_served", self.window_not_served());
        s.put("window.sequenced", sequenced);
        s.put("window.sequenced_ppm", ppm(sequenced, sender.window_offered));
        s.put(
            "window.achieved_rate",
            u128::from(sequenced) * u128::from(SECOND_NS) / u128::from(self.config.window_ns),
        );
        s.put("window.jumps", self.content.jumps);
        s.put("window.ioc_places_offered", self.content.ioc_places);
    }

    /// `flow.*`: who sent what where in the window (D-034, [`FlowContent`]). The busiest
    /// account's rate against its one gateway's busy share (`health.gateway_<g>.busy_ppm`)
    /// shows a per-account ceiling (`--makers K`); taker IOCs a second and fills per IOC
    /// show how the takers met the book; the market shares, how concentrated the flow is.
    fn put_flow(&self, s: &mut Summary) {
        let content = &self.content;
        let flow = &self.config.flow;
        let per_second =
            |n: u64| u128::from(n) * u128::from(SECOND_NS) / u128::from(self.config.window_ns.max(1));
        s.put("flow.window_clients", content.clients);
        if let Some((account, messages)) = content.busiest_account {
            s.put("flow.busiest_account", account);
            s.put("flow.busiest_account_messages", messages);
            s.put("flow.busiest_account_ppm", ppm(messages, content.clients));
            s.put("flow.busiest_account_per_s", per_second(messages));
            s.put("flow.busiest_account_lane", gateway_of(account, content.per_lane.len()));
        }
        for (lane, &messages) in content.per_lane.iter().enumerate() {
            s.put(format!("flow.lane.{lane}.messages"), messages);
        }
        s.put("flow.taker_iocs", content.taker_iocs);
        s.put("flow.taker_iocs_per_s", per_second(content.taker_iocs));
        let fills = self.output.stats.window.events.fills;
        s.put("flow.fills_per_ioc_milli", (fills * 1_000).checked_div(content.ioc_places).unwrap_or(0));
        let markets = flow.markets();
        let label = |market: Option<MarketId>| market.map_or("-".to_string(), |id| flow.market_label(id));
        let all = MarketShares::of(content.per_market.iter().map(|c| (c.market, c.clients)), markets);
        s.put("flow.markets", markets);
        s.put("flow.markets_active", all.active);
        s.put("flow.market_top", label(all.top));
        s.put("flow.market_top_ppm", all.top_ppm);
        s.put("flow.market_top10_ppm", all.top10_ppm);
        s.put("flow.market_median_ppm", all.median_ppm);
        let takers = MarketShares::of(content.per_market.iter().map(|c| (c.market, c.taker_iocs)), markets);
        s.put("flow.taker_market_top", label(takers.top));
        s.put("flow.taker_market_top_ppm", takers.top_ppm);
        s.put("flow.taker_market_top10_ppm", takers.top10_ppm);
    }

    /// `stage.*`: every histogram of 15.4.
    fn put_stages(&self, s: &mut Summary) {
        let stats = &self.output.stats;
        put_stage_histograms(s, "stage.client", &stats.client);
        put_stage_histograms(s, "stage.operator", &stats.operator);
        s.put_histogram("stage.client.to_core_result.completed", &self.completed.to_core_result);
        s.put_histogram("stage.client.to_durable_ack.completed", &self.completed.to_durable_ack);
        s.put_histogram("stage.set_mark_core_path", &stats.set_mark_core_path);
        s.put_histogram("stage.set_mark_core_service", &stats.set_mark_core_service);
        for (g, gateway) in stats.per_gateway.iter().enumerate() {
            s.put_histogram(&format!("stage.gateway.{g}.ingress_wait"), &gateway.ingress_wait);
            s.put_histogram(&format!("stage.gateway.{g}.verification"), &gateway.verification);
        }
        s.put_histogram("sender.lag", &self.sender.lag);
    }

    /// `journal.*`, `core.*`, `sequencer.*`, `sender.*`, `gateway.*`, `ablation.*`.
    /// Flushes per second are over the window (15.8), from the counter main samples at its
    /// edges; the whole run's average, setup and drain included, is kept apart. The flush
    /// histograms cover the whole run.
    fn put_threads(&self, s: &mut Summary) {
        let (journal, core) = (&self.output.journal, &self.output.core);
        s.put("journal.flushes", journal.flushes);
        s.put("journal.records", journal.records);
        s.put("journal.bytes", journal.bytes);
        match self.monitor.health() {
            Some(health) => s.put("journal.flushes_per_s", health.journal_flushes_per_second),
            None => s.put("journal.flushes_per_s", "unknown"),
        }
        s.put(
            "journal.flushes_per_s_whole_run",
            u128::from(journal.flushes) * u128::from(SECOND_NS) / u128::from(self.duration_ns.max(1)),
        );
        s.put("journal.fdatasync_total_ns", journal.fdatasync_total_ns);
        s.put("journal.segments_created", journal.segments_created);
        s.put_histogram("journal.flush", &journal.flush_ns);
        s.put_histogram("journal.fdatasync", &journal.fdatasync_ns);
        s.put_histogram("journal.batch_records", &journal.batch_records);
        s.put("core.commands", core.commands);
        s.put("core.events", core.events);
        s.put("core.events_per_command_milli", core.events * 1_000 / core.commands.max(1));
        s.put("core.max_events_per_command", core.max_events_per_command);
        s.put("core.stall_total_ns", core.stall_total_ns);
        s.put_histogram("core.stall", &core.stall_ns);
        s.put("sequencer.records", self.output.sequencer.records);
        s.put("sequencer.core_full_passes", self.output.sequencer.core_full_passes);
        s.put("sequencer.journal_full_passes", self.output.sequencer.journal_full_passes);
        let sender = &self.sender;
        s.put("sender.end", format!("{:?}", sender.end));
        s.put("sender.window_offered", sender.window_offered);
        s.put("sender.window_dropped", sender.window_dropped);
        for (ring, &dropped) in sender.dropped.iter().enumerate().filter(|(_, n)| **n > 0) {
            s.put(format!("sender.dropped.ring{ring}"), dropped);
        }
        s.put("sender.max_operator_backlog", sender.max_operator_backlog);
        s.put("sender.longest_operator_backlog_ns", sender.longest_operator_backlog_ns);
        s.put("sender.generator_limited", sender.generator_limited());
        let (mut all, mut window) = (RejectCounts::default(), RejectCounts::default());
        for g in &self.gateways {
            all.add(&g.rejects);
            window.add(&g.window_rejects);
        }
        for reason in GatewayReject::ALL {
            if all.get(reason) > 0 {
                s.put(format!("gateway.rejects.{reason}"), all.get(reason));
            }
            if window.get(reason) > 0 {
                s.put(format!("gateway.window_rejects.{reason}"), window.get(reason));
            }
        }
        for (tag, name) in [(1, "place"), (2, "cancel"), (3, "modify")] {
            if all.busy(tag) > 0 {
                s.put(format!("gateway.busy.{name}"), all.busy(tag));
            }
        }
        self.put_ablation(s);
    }

    /// `ablation.*`: the "verify on core" ablation's verifications per core-second (16).
    fn put_ablation(&self, s: &mut Summary) {
        let Some(arm) = self.config.verify_on_core else { return };
        let Some(health) = self.monitor.health() else { return };
        let verifiers: Vec<&str> = match arm {
            VerifyArm::Gateways => {
                health.threads.iter().map(|t| t.name.as_str()).filter(|n| n.starts_with("gateway")).collect()
            }
            VerifyArm::Core => vec!["core"],
        };
        let busy_ppm: u64 =
            health.threads.iter().filter(|t| verifiers.contains(&t.name.as_str())).map(|t| t.busy_ppm).sum();
        // Busy core-seconds of the verifying threads in the window, in nanoseconds.
        let busy_ns = u128::from(busy_ppm) * u128::from(self.config.window_ns) / 1_000_000;
        let verifications = self.output.stats.window.client_commands();
        s.put("ablation.cores_used", verifiers.len());
        s.put("ablation.verifications", verifications);
        s.put(
            "ablation.verifications_per_core_second",
            u128::from(verifications) * u128::from(SECOND_NS) / busy_ns.max(1),
        );
    }

    /// `fund.*` (15.9).
    fn put_fund(&self, s: &mut Summary) {
        let fund = &self.output.stats.fund;
        let option = |value: Option<i128>| value.map_or("none".to_string(), |v| v.to_string());
        s.put("fund.start", option(fund.start));
        s.put("fund.peak", option(fund.peak));
        s.put("fund.lowest", option(fund.lowest));
        s.put("fund.max_drawdown", fund.max_drawdown);
        s.put("fund.final", fund.final_equity);
        s.put("fund.snapshot", i128::from(self.snapshot.fund_balance) + self.snapshot.fund_upnl_total);
        s.put("fund.peak_shortfall", self.output.stats.window.events.peak_shortfall);
        s.put("fund.consistent", self.fund_consistent());
    }

    /// `health.*` (15.4).
    fn put_health(&self, s: &mut Summary) {
        let Some(health) = self.monitor.health() else {
            s.put("health.limit", "unknown (the window never opened)");
            return;
        };
        for thread in &health.threads {
            let key = thread.name.replace(' ', "_");
            s.put(format!("health.{key}.busy_ppm"), thread.busy_ppm);
            let faults = thread.minor_faults.map_or("unknown".to_string(), |f| f.to_string());
            s.put(format!("health.{key}.minor_faults"), faults);
        }
        s.put("health.journal_writer.fdatasync_ppm", health.journal_sync_ppm);
        s.put(
            "health.page_migrations",
            health.page_migrations.map_or("unknown".to_string(), |p| p.to_string()),
        );
        s.put("health.core_stall_ns", health.core_stall_ns);
        s.put("window.released_per_second", health.released_per_second);
        s.put("health.core_full_passes", health.core_full_passes);
        s.put("health.journal_full_passes", health.journal_full_passes);
        s.put("health.limit", health.limit.label());
    }

    /// `check.*`: the verdict and what it was made from.
    fn put_checks(&self, s: &mut Summary) {
        let verdict = self.verdict();
        s.put("check.valid", verdict.invalid.is_empty());
        s.put("check.invalid", joined(&verdict.invalid));
        s.put("check.flags", joined(&verdict.flags));
        s.put("check.throttled_periods", self.throttled.periods);
        s.put("check.throttled_us", self.throttled.us);
        s.put("check.inversions", self.output.stats.inversions.iter().sum::<u64>());
        for (stage, &n) in Stage::ALL.iter().zip(&self.output.stats.inversions).filter(|(_, n)| **n > 0) {
            s.put(format!("check.inversions.{}", stage_key(*stage)), n);
        }
        s.put("check.clock_counts_for_headline", self.clock.counts_for_headline());
        s.put("check.drain_cap_exceeded", self.monitor.drain_cap_exceeded);
        let faults = self.monitor.health().and_then(|h| h.total_faults());
        s.put("check.faults_in_window", faults.map_or("unknown".to_string(), |f| f.to_string()));
        s.put("check.reject_share_ppm", self.reject_share_ppm());
    }

    /// `replay.*` and `audit.*` (13.3, 13.4).
    fn put_replay_and_audit(&self, s: &mut Summary) {
        match &self.replay {
            None => s.put("replay.verdict", "off"),
            Some(ReplayVerdict::NotChecked(why)) => {
                s.put("replay.verdict", "not checked");
                s.put("replay.detail", why);
            }
            Some(ReplayVerdict::Different(difference)) => {
                s.put("replay.verdict", "different");
                s.put("replay.detail", difference);
            }
            Some(ReplayVerdict::Identical(report)) => {
                s.put("replay.verdict", "identical");
                s.put("replay.records", report.records);
                s.put("replay.events", report.events);
                s.put("replay.records_per_s", report.records_per_second);
            }
        }
        if let Some(audit) = &self.audit {
            s.put("audit.passed", audit.passed());
            s.put("audit.signed", audit.signed);
            s.put("audit.pre_verified", audit.pre_verified);
            s.put("audit.operator", audit.operator);
            s.put("audit.failures", audit.failures);
            if let Some(first) = audit.listed.first() {
                s.put("audit.first_failure", format!("seq {}: {}", first.seq, first.what));
            }
        }
    }
}

/// `-` for nothing, else the items joined by `; `.
fn joined(items: &[String]) -> String {
    if items.is_empty() { "-".to_string() } else { items.join("; ") }
}

/// A stage's name in summary keys: `core_path`, `to_durable_ack`, ...
pub fn stage_key(stage: Stage) -> &'static str {
    match stage {
        Stage::SenderLag => "sender_lag",
        Stage::IngressWait => "ingress_wait",
        Stage::Verification => "verification",
        Stage::SequencerWait => "sequencer_wait",
        Stage::CorePath => "core_path",
        Stage::CoreService => "core_service",
        Stage::ToCoreResult => "to_core_result",
        Stage::DurabilityWait => "durability_wait",
        Stage::ToDurableAck => "to_durable_ack",
    }
}

fn put_stage_histograms(s: &mut Summary, prefix: &str, histograms: &StageHistograms) {
    debug_assert_eq!(Stage::ALL.len(), STAGES);
    for stage in Stage::ALL {
        s.put_histogram(&format!("{prefix}.{}", stage_key(stage)), histograms.get(stage));
    }
}

/// A command tag's name in summary keys.
pub fn tag_key(tag: usize) -> &'static str {
    [
        "none",
        "place",
        "cancel",
        "modify",
        "deposit",
        "withdraw",
        "set_leverage",
        "set_mark",
        "set_market_params",
        "set_risk_tier",
    ][tag]
}

/// The breakdown of 15.9 under `prefix`: commands and outcomes by type and reason, and
/// their results.
fn put_breakdown(s: &mut Summary, prefix: &str, b: &Breakdown) {
    for tag in 1..b.commands.len() {
        if b.commands[tag] == 0 {
            continue;
        }
        let name = tag_key(tag);
        s.put(format!("{prefix}.commands.{name}"), b.commands[tag]);
        s.put(format!("{prefix}.accepted.{name}"), b.accepted[tag]);
        for code in 0..REJECT_REASONS {
            let n = b.rejected[tag][code];
            if n > 0 {
                let reason = reject_reason_from_code(code as u8).expect("a reject code the gate counted");
                s.put(format!("{prefix}.rejected.{name}.{reason:?}"), n);
            }
        }
    }
    s.put(format!("{prefix}.client_commands"), b.client_commands());
    s.put(format!("{prefix}.client_rejects"), b.client_rejects());
    let e = &b.events;
    s.put(format!("{prefix}.events"), e.events);
    s.put(format!("{prefix}.fills"), e.fills);
    s.put(format!("{prefix}.fill_lots"), e.fill_lots);
    s.put(format!("{prefix}.fill_notional"), e.fill_notional);
    for code in 0..CANCEL_REASONS {
        if e.cancels[code] > 0 {
            let reason = cancel_reason_from_code(code as u8).expect("a cancel code the gate counted");
            s.put(format!("{prefix}.cancels.{reason:?}"), e.cancels[code]);
        }
    }
    s.put(format!("{prefix}.modifies"), e.modifies);
    s.put(format!("{prefix}.marks"), e.marks);
    s.put(format!("{prefix}.liquidations"), e.liquidations);
    s.put(format!("{prefix}.insurance_absorbs"), e.insurance_absorbs);
    s.put(format!("{prefix}.shortfall_reports"), e.shortfall_reports);
    s.put(format!("{prefix}.peak_shortfall"), e.peak_shortfall);
    s.put(format!("{prefix}.max_events_per_command"), b.max_events_per_command);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2e::workload::Workload;
    use loadgen::sender::{SETUP_RATE, Timing};

    #[test]
    fn market_shares_rank_markets_and_count_the_quiet_ones_in_the_median() {
        // 10 markets of a flow of 12: two share the top count, two have none.
        let counts = [(5, 40), (2, 40), (7, 10), (1, 5), (3, 1), (4, 1), (6, 1), (8, 1), (9, 1), (10, 0)];
        let shares = MarketShares::of(counts.into_iter(), 12);
        assert_eq!(shares.top, Some(2), "the lowest id of the two busiest");
        assert_eq!((shares.top_ppm, shares.active), (400_000, 9));
        assert_eq!(shares.top10_ppm, 1_000_000, "9 markets hold everything");
        // Ascending over 12 markets: 0, 0, 0, 1, 1, 1, 1, 1, 5, 10, 40, 40; the lower middle is 1.
        assert_eq!(shares.median_ppm, 10_000);
        assert_eq!(MarketShares::of([].into_iter(), 88), MarketShares::default(), "nothing offered");
    }

    #[test]
    fn the_flow_content_counts_the_windows_client_items_by_account_lane_and_market() {
        let config = RunConfig {
            fixed_timed_clients: Some(2_000),
            ..RunConfig::polymarket_smoke(InjectionMode::PreVerified)
        };
        let workload = Workload::build(&config, None);
        let timing = Timing {
            arrivals: config.arrivals,
            setup_rate: SETUP_RATE,
            rate: config.rate,
            timed_clients: 2_000,
            warmup_ns: 20_000_000,
            window_ns: 50_000_000,
        };
        let plan = &workload.plan;
        let sender_plan = SenderPlan::new(plan, workload.messages.clone(), &timing);
        let content = FlowContent::of(plan, &sender_plan, 3);
        // The same counts, straight from the schedule.
        let timed = sender_plan.phases.iter().find(|p| p.phase == FlowPhase::Timed).expect("a timed phase");
        let in_window: Vec<&loadgen::market_flow::ClientItem> = plan.timed[..timed.schedule.len()]
            .iter()
            .zip(&timed.schedule)
            .filter(|(_, t)| sender_plan.window.contains(t))
            .filter_map(|(item, _)| match item {
                Item::Client(client) => Some(client),
                Item::Operator(_) => None,
            })
            .collect();
        assert!(in_window.len() > 500, "{} client items in the window", in_window.len());
        assert_eq!(content.clients, in_window.len() as u64);
        assert_eq!(content.per_lane.iter().sum::<u64>(), content.clients);
        assert_eq!(content.per_market.iter().map(|m| m.clients).sum::<u64>(), content.clients);
        let (busiest, most) = content.busiest_account.expect("someone sent");
        assert_eq!(most, in_window.iter().filter(|c| c.account == busiest).count() as u64);
        for account in in_window.iter().map(|c| c.account) {
            let n = in_window.iter().filter(|c| c.account == account).count() as u64;
            assert!(n < most || (n == most && account >= busiest), "account {account} sent {n}");
        }
        let lane = gateway_of(busiest, 3);
        assert!(content.per_lane[lane] >= most, "the busiest account's messages all go to its lane");
        assert!(content.taker_iocs <= content.ioc_places);
        assert_eq!(content.per_market.iter().map(|m| m.taker_iocs).sum::<u64>(), content.taker_iocs);
        assert!(content.per_market.windows(2).all(|pair| pair[0].market < pair[1].market), "in id order");
    }
}
