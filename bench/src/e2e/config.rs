//! What one run is: the mode, the flow, the offered rate, the timing, the journal and the
//! threads (`docs/PIPELINE.md` 14.9 to 14.11, 15.5, 11.5, 11.6, 2.6 and 18.4).
//!
//! **Contract.** A [`RunConfig`] says everything that makes two runs comparable; the runner
//! (`runner.rs`) turns it into threads and rings, and the summary records it. Its defaults
//! are the spec's: the M3 flow with seed 1, a 5 s warm-up and a 30 s window (15.5),
//! Poisson arrivals (14.9), the journal on disk with `T` = 1 ms and `B` = 4,096 (11.5),
//! stamps on, the default CPU layout (2.6), busy-polling, `k256` verifying signatures
//! (5.7: libsecp256k1 only when asked for, in a build that has it), and our own nonce
//! scheme signing them (5.1 to 5.3). [`RunConfig::smoke`] is 18.4's smoke run, and
//! [`RunConfig::polymarket_smoke`] the same run of the Polymarket-shaped flow.
//! [`RunConfig::check`] refuses combinations the spec rules out, such as the replay test in
//! discard mode (13.3), a verifier the build doesn't have, and a flow its generator can't
//! run.
//!
//! **The flow and its stress switches** (`flow`; D-034). The M3 flow (D-027) by default, or
//! the Polymarket-shaped flow (`flow.rs`). Of D-034's three stress switches, `--makers K`
//! and `--shock` change what the plan contains, so they are part of the Polymarket flow's
//! config (and of its digest); `--bursts` changes only when messages are sent, so it is the
//! run's arrivals, [`Arrivals::Cox`] ([`RunConfig::bursts`]), with either flow. A run's
//! name says the flow and every switch (`signed-100k-polymarket-makers3-bursts-median`), so
//! the runs of one session never share a name across flows or switches.
//!
//! **The signing scheme** (`auth`; 5.8, D-033). `perp`, the default, or `eip712`: Polymarket
//! Perps' own scheme, where each message is signed over an EIP-712 digest with a salt and a
//! millisecond timestamp, and the gateways recover the signer. Only signed runs sign, so a
//! pre-verified run always has the default, and [`RunConfig::in_mode`], which sweeps and
//! searches use to derive their runs from one template, gives it that. `check` refuses the
//! EIP-712 scheme where v1 doesn't support it: in pre-verified mode, in the verify-on-core
//! ablation (the core's verifier checks perp signatures), with `--resume` (a restarted
//! gateway would need the last 5 minutes of requests from the journal), and for a timed
//! flow over [`EIP712_MAX_TIMED_FLOW_NS`] (its messages are signed before the run, and the
//! gateways refuse them 5 minutes after that).
//!
//! **The fingerprint** ([`RunConfig::fingerprint`]). A session reuses a finished run by its
//! name (15.5), and the name says only what differs between the runs of one sweep. So every
//! setting that makes two runs comparable goes into the run's summary as `run.*` keys, and
//! a session reuses a finished run only if every one of them matches what is asked now
//! (review finding F4-resume-keyed-by-name-only; `session.rs`). A key added later reads, in
//! a summary written before it, as what every run then had ([`recorded_before`]):
//! `run.auth` came with the EIP-712 scheme, so an older summary's is `perp` if it was
//! signed, `none` if not; `run.flow`, `run.makers`, `run.bursts` and `run.shock` came with
//! the Polymarket-shaped flow, so an older summary's are `m3`, `default`, `none` and
//! `none`. (The flow's digest, `run.flow_digest`, already told the flows and the plan
//! switches apart, and `run.arrivals` the bursts; the four keys say it in words.)
//!
//! **The tail.** The timed flow is `warm-up + window + tail` long: the flow keeps going for
//! a moment after the window closes, so main samples the window's closing edge (page faults,
//! busy time) while every thread still runs; a thread's `/proc` entry disappears when it
//! ends (15.4). Commands in the tail are neither setup nor measured.
//!
//! **Complexity.** Plain data.

use engine::engine::EngineOptions;
use gateway::VerifierKind;
use loadgen::market_flow::PlanConfig;
use loadgen::schedule::{Arrivals, Bursts};
use pipeline::idle::IdleStrategy;
use pipeline::journal::format::{DEFAULT_SEGMENT_BYTES, JournalIdentity, MAX_BATCH_RECORDS};
use pipeline::journal::writer::JournalMode;
use pipeline::records::{AuthScheme, InjectionMode, JournalRecord, Stamps};

use super::flow::{Flow, bursts_name, shock_name};
use super::summary::Summary;
use super::units::{SECOND_NS, short_duration, short_rate};

/// The benchmark's deployment id (5.1): used for nothing else, with test keys only, so the
/// pre-signed messages may be reused across fresh journals (14.8).
pub const BENCH_DEPLOYMENT: u32 = 0x00BE_0003;
/// The flow sent after the window closes (module docs, "The tail").
pub const DEFAULT_TAIL_NS: u64 = SECOND_NS / 5;
/// Gateways by default: the local machine's 2 (2.6). PERPSBOX runs pass `--gateways 10`.
pub const DEFAULT_GATEWAYS: usize = 2;
/// The longest timed flow ([`RunConfig::timed_flow_ns`]) of an EIP-712 run (module docs,
/// "The signing scheme"): 3 minutes, which leaves 2 of the gateways' 5 for signing, the
/// start, the setup phases and a margin (`workload.rs`, "Fresh", which refuses a run whose
/// signing took more than its share).
pub const EIP712_MAX_TIMED_FLOW_NS: u64 = 180 * SECOND_NS;

/// The journal's settings for a run (11.5, 11.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalSettings {
    pub mode: JournalMode,
    /// `T`, in nanoseconds.
    pub commit_interval_ns: u64,
    /// `B`.
    pub max_batch: usize,
    pub segment_bytes: u64,
}

impl Default for JournalSettings {
    /// On disk, `T` = 1 ms, `B` = 4,096, 1 GiB segments.
    fn default() -> Self {
        JournalSettings {
            mode: JournalMode::Disk,
            commit_interval_ns: 1_000_000,
            max_batch: MAX_BATCH_RECORDS,
            segment_bytes: DEFAULT_SEGMENT_BYTES,
        }
    }
}

/// Where the threads run (2.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cpus {
    /// Nothing pinned: tests, which share the machine.
    Unpinned,
    /// The default layout for this machine, with `--gateway-smt` and `--cpus role=list`
    /// overrides.
    Pinned { gateway_smt: bool, overrides: Vec<String> },
}

/// The arms of the "verify on core" ablation (section 16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyArm {
    /// The gateways verify, with the ablation's 3-line core records.
    Gateways,
    /// The core verifies (insecure).
    Core,
}

impl VerifyArm {
    pub fn name(self) -> &'static str {
        match self {
            VerifyArm::Gateways => "gateways",
            VerifyArm::Core => "core",
        }
    }
}

/// Everything about one run (module docs).
#[derive(Clone, Debug)]
pub struct RunConfig {
    /// Signed (through the gateways) or pre-verified (straight into the lanes), 14.11.
    pub mode: InjectionMode,
    /// The M3 flow or the Polymarket-shaped one, with its plan switches (module docs, "The
    /// flow and its stress switches").
    pub flow: Flow,
    pub deployment: u32,
    /// The offered rate of the timed flow: client commands a second (14.9).
    pub rate: u64,
    /// Poisson by default; `Cox` with `--bursts` (module docs).
    pub arrivals: Arrivals,
    pub warmup_ns: u64,
    pub window_ns: u64,
    pub tail_ns: u64,
    /// Client items of the timed flow; `None`: enough for warm-up, window and tail at
    /// `rate` ([`RunConfig::timed_clients`]).
    pub fixed_timed_clients: Option<usize>,
    /// Gateways in a signed run, lanes in a pre-verified one (`N`).
    pub lanes: usize,
    pub journal: JournalSettings,
    pub stamps: Stamps,
    /// Capture the released events and run the replay test after the run (13.2, 13.3).
    pub capture: bool,
    /// Run the signature audit after a signed run (13.4).
    pub audit: bool,
    /// The "verify on core" ablation's arm, or `None` (section 16).
    pub verify_on_core: Option<VerifyArm>,
    /// What verifies the signatures in a signed run: the gateways, the core in the
    /// ablation's core arm, and the audit (5.7).
    pub verifier: VerifierKind,
    /// How a signed run's messages are signed and checked (module docs, "The signing
    /// scheme"); `Perp` in a pre-verified run, which signs nothing.
    pub auth: AuthScheme,
    pub cpus: Cpus,
    pub idle: IdleStrategy,
    /// A run whose backlog hasn't drained this long after its window closes is stopped and
    /// fails (the "fsync per order" ablation, 16).
    pub drain_cap_ns: Option<u64>,
    /// Keep the journal after the run (11.6: deleted by default, the disk fills otherwise).
    pub keep_journal: bool,
    /// Write every released event slot to standard output (the kill test, 18.4).
    pub release_log: bool,
    /// Restart on the journal already in the run directory (13.5) and send the rest of the
    /// flow.
    pub resume: bool,
    /// Accept a journal written with other engine semantics (11.2).
    pub allow_engine_change: bool,
    /// **Development only**: a generator-limited run is flagged instead of invalid, so a
    /// search can be tried on a machine whose CPUs are taken away too often (a VM's timer
    /// ticks). The summary and the report say so.
    pub allow_generator_limited: bool,
    /// How long a setup barrier may wait per item of its phase, on top of the fixed 10 s
    /// (14.4): the "fsync per order" arm (`B` = 1) journals setup one `fdatasync` per
    /// record, so its barriers need about `F` per item (section 16; `ablate::fsync_arm`).
    pub barrier_per_item_ns: Option<u64>,
}

impl RunConfig {
    /// A run of the M3 flow at `rate`, with the defaults of the module docs.
    pub fn new(mode: InjectionMode, rate: u64) -> RunConfig {
        RunConfig {
            mode,
            flow: Flow::m3(),
            deployment: BENCH_DEPLOYMENT,
            rate,
            arrivals: Arrivals::Poisson,
            warmup_ns: 5 * SECOND_NS,
            window_ns: 30 * SECOND_NS,
            tail_ns: DEFAULT_TAIL_NS,
            fixed_timed_clients: None,
            lanes: DEFAULT_GATEWAYS,
            journal: JournalSettings::default(),
            stamps: Stamps::On,
            capture: false,
            audit: false,
            verify_on_core: None,
            verifier: VerifierKind::K256,
            auth: AuthScheme::Perp,
            cpus: Cpus::Pinned { gateway_smt: false, overrides: Vec::new() },
            idle: IdleStrategy::Spin,
            drain_cap_ns: None,
            keep_journal: false,
            release_log: false,
            resume: false,
            allow_engine_change: false,
            allow_generator_limited: false,
            barrier_per_item_ns: None,
        }
    }

    /// The smoke run of 18.4: the smoke flow; signed, 2 gateways and 3,000 client messages
    /// at 5k/s, or pre-verified, 2 lanes and 30,000 commands at 20k/s. Threads spin, then
    /// yield, and nothing is pinned, so the test shares the machine politely. Capture (and
    /// the replay test) and the audit are on, the journal is kept for the test's own
    /// checks, and segments are 1 MiB, so none takes long to create. The window covers the
    /// middle of the flow: a warm-up before it, and a tail after it.
    pub fn smoke(mode: InjectionMode) -> RunConfig {
        let (rate, clients, warmup, window, tail) = match mode {
            InjectionMode::Signed => (5_000, 3_000, 150, 350, 100),
            InjectionMode::PreVerified => (20_000, 30_000, 500, 800, 200),
        };
        let ms = 1_000_000;
        RunConfig {
            flow: Flow::smoke(),
            warmup_ns: warmup * ms,
            window_ns: window * ms,
            tail_ns: tail * ms,
            fixed_timed_clients: Some(clients),
            journal: JournalSettings { segment_bytes: 1 << 20, ..JournalSettings::default() },
            capture: true,
            audit: mode == InjectionMode::Signed,
            cpus: Cpus::Unpinned,
            idle: IdleStrategy::SpinThenYield { spins: 64 },
            keep_journal: true,
            ..RunConfig::new(mode, rate)
        }
    }

    /// [`RunConfig::smoke`] with the Polymarket smoke flow (`flow.rs`): the same rates,
    /// counts and timing, on the 88 real markets, with a stress shock every 250 ms of flow
    /// time.
    pub fn polymarket_smoke(mode: InjectionMode) -> RunConfig {
        RunConfig { flow: Flow::polymarket_smoke(), ..RunConfig::smoke(mode) }
    }

    /// `--bursts`'s preset, if the arrivals are bursty (module docs).
    pub fn bursts(&self) -> Option<Bursts> {
        match self.arrivals {
            Arrivals::Cox(bursts) => Some(bursts),
            Arrivals::Poisson | Arrivals::Uniform => None,
        }
    }

    /// What run names add for the flow and its switches (module docs): nothing for the M3
    /// flow without bursts; else `-polymarket`, `-makers<K>`, `-bursts-<preset>` and
    /// `-shock-<size>`, in that order. Searches add it to their names too (`search.rs`).
    pub fn flow_suffix(&self) -> String {
        let mut suffix = String::new();
        if let Flow::Polymarket(_) = self.flow {
            suffix.push_str("-polymarket");
        }
        if let Some(makers) = self.flow.makers() {
            suffix.push_str(&format!("-makers{makers}"));
        }
        if let Some(bursts) = self.bursts() {
            suffix.push_str(&format!("-bursts-{}", bursts_name(bursts)));
        }
        if let Some(shock) = self.flow.shock() {
            suffix.push_str(&format!("-shock-{}", shock_name(shock)));
        }
        suffix
    }

    /// Client items of the timed flow: the fixed count, or `rate × (warm-up + window +
    /// tail)` rounded up.
    pub fn timed_clients(&self) -> usize {
        self.fixed_timed_clients.unwrap_or_else(|| {
            let ns = self.warmup_ns + self.window_ns + self.tail_ns;
            (u128::from(self.rate) * u128::from(ns)).div_ceil(u128::from(SECOND_NS)) as usize
        })
    }

    /// How long the timed flow lasts: its warm-up, window and tail, or longer if a fixed
    /// count of client items takes longer at `rate`.
    pub fn timed_flow_ns(&self) -> u64 {
        let by_count = u128::from(SECOND_NS) * self.timed_clients() as u128 / u128::from(self.rate.max(1));
        (self.warmup_ns + self.window_ns + self.tail_ns).max(u64::try_from(by_count).unwrap_or(u64::MAX))
    }

    /// The engine options of every run of this flow (14.3).
    pub fn engine_options(&self) -> EngineOptions {
        self.flow.engine_options()
    }

    /// The journal identity this run starts, or expects on a restart (11.2), with its
    /// signing scheme (header byte 120).
    pub fn identity(&self) -> JournalIdentity {
        JournalIdentity::new(self.deployment, self.mode, self.engine_options()).with_auth(self.auth)
    }

    /// This run's settings in `mode`: a sweep, a search or an ablation derives each of its
    /// runs from one template this way (module docs, "The signing scheme"). A pre-verified
    /// run signs nothing, so it takes the default scheme; a signed one keeps the template's.
    pub fn in_mode(&self, mode: InjectionMode) -> RunConfig {
        let auth = match mode {
            InjectionMode::Signed => self.auth,
            InjectionMode::PreVerified => AuthScheme::Perp,
        };
        RunConfig { mode, auth, ..self.clone() }
    }

    /// Bytes per journal record: 152 for signed commands, 80 otherwise (11.3).
    pub fn record_bytes(&self) -> u64 {
        match self.mode {
            InjectionMode::Signed => u64::from(JournalRecord::SIGNED_BYTES),
            InjectionMode::PreVerified => JournalRecord::UNSIGNED_WORDS as u64 * 8,
        }
    }

    /// True if the journal is discarded (11.6): nothing it releases was on disk.
    pub fn discarded(&self) -> bool {
        self.journal.mode == JournalMode::Discard
    }

    /// The verifier's name in a signed run; `none` in a pre-verified one, which verifies
    /// nothing, so its runs compare equal whatever `--verifier` says.
    pub fn verifier_name(&self) -> &'static str {
        match self.mode {
            InjectionMode::Signed => self.verifier.name(),
            InjectionMode::PreVerified => "none",
        }
    }

    /// The signing scheme's name in a signed run; `none` in a pre-verified one, which signs
    /// nothing (as [`RunConfig::verifier_name`]).
    pub fn auth_name(&self) -> &'static str {
        match self.mode {
            InjectionMode::Signed => self.auth.name(),
            InjectionMode::PreVerified => "none",
        }
    }

    /// A short name for directories and tables, from what varies between runs:
    /// `signed-100k`, `signed-100k-eip712`, `preverified-20k-T250us`,
    /// `signed-40k-verify-core`, `signed-100k-polymarket-bursts-median`, ...
    pub fn name(&self) -> String {
        let mut name = format!("{}-{}", mode_name(self.mode), short_rate(self.rate));
        name.push_str(&self.flow_suffix());
        if self.auth == AuthScheme::Eip712 {
            name.push_str("-eip712");
        }
        let journal = self.journal;
        if journal.mode == JournalMode::Discard {
            name.push_str("-discard");
        }
        if journal.commit_interval_ns != JournalSettings::default().commit_interval_ns {
            name.push_str(&format!("-T{}", short_duration(journal.commit_interval_ns)));
        }
        if journal.max_batch != MAX_BATCH_RECORDS {
            name.push_str(&format!("-B{}", journal.max_batch));
        }
        if self.stamps == Stamps::Off {
            name.push_str("-stamps-off");
        }
        if let Some(arm) = self.verify_on_core {
            name.push_str(&format!("-verify-{}", arm.name()));
        }
        name
    }

    /// Every setting that makes two runs comparable, as the `run.*` keys and values the
    /// run's summary records (module docs, "The fingerprint"). Left out: what doesn't
    /// change what is measured (keeping the journal, the release log, the barrier's
    /// allowance), and a restart, which only `e2e run --resume` makes.
    pub fn fingerprint(&self) -> Vec<(&'static str, String)> {
        let journal = self.journal;
        let cpus = match &self.cpus {
            Cpus::Unpinned => "unpinned".to_string(),
            Cpus::Pinned { gateway_smt, overrides } => {
                let mut parts = vec!["pinned".to_string()];
                if *gateway_smt {
                    parts.push("gateway-smt".into());
                }
                parts.extend(overrides.iter().cloned());
                parts.join(", ")
            }
        };
        let digest = self.flow.digest();
        vec![
            ("run.mode", mode_name(self.mode).to_string()),
            ("run.rate", self.rate.to_string()),
            ("run.flow", self.flow.name().to_string()),
            ("run.makers", self.flow.makers().map_or("default".to_string(), |k| k.to_string())),
            ("run.bursts", self.bursts().map_or("none", bursts_name).to_string()),
            ("run.shock", self.flow.shock().map_or("none", shock_name).to_string()),
            ("run.flow_digest", digest[..8].iter().map(|b| format!("{b:02x}")).collect()),
            ("run.deployment", self.deployment.to_string()),
            ("run.arrivals", format!("{:?}", self.arrivals)),
            ("run.warmup_ns", self.warmup_ns.to_string()),
            ("run.window_ns", self.window_ns.to_string()),
            ("run.tail_ns", self.tail_ns.to_string()),
            ("run.timed_clients", self.timed_clients().to_string()),
            ("run.lanes", self.lanes.to_string()),
            ("run.journal", if self.discarded() { "discard" } else { "disk" }.to_string()),
            ("run.commit_interval_ns", journal.commit_interval_ns.to_string()),
            ("run.max_batch", journal.max_batch.to_string()),
            ("run.segment_bytes", journal.segment_bytes.to_string()),
            ("run.stamps", if self.stamps == Stamps::On { "on" } else { "off" }.to_string()),
            ("run.capture", self.capture.to_string()),
            ("run.audit", self.audit.to_string()),
            ("run.verify_on_core", self.verify_on_core.map_or("none", VerifyArm::name).to_string()),
            ("run.verifier", self.verifier_name().to_string()),
            ("run.auth", self.auth_name().to_string()),
            ("run.cpus", cpus),
            ("run.idle", format!("{:?}", self.idle)),
            ("run.drain_cap_ns", self.drain_cap_ns.map_or("none".to_string(), |ns| ns.to_string())),
            ("run.allow_generator_limited", self.allow_generator_limited.to_string()),
        ]
    }

    /// Refuses what the spec rules out (module docs).
    pub fn check(&self) -> Result<(), String> {
        self.flow.check()?;
        let eip712 = self.auth == AuthScheme::Eip712;
        let rules = [
            (self.lanes >= 1, "at least one gateway or lane"),
            (self.window_ns > 0, "a measured window"),
            (!(self.capture && self.discarded()), "no replay test in discard mode (13.3)"),
            (!(self.audit && self.mode != InjectionMode::Signed), "the audit only in signed mode (13.4)"),
            (
                !(self.verify_on_core.is_some() && self.mode != InjectionMode::Signed),
                "the verify-on-core ablation only in signed mode (16)",
            ),
            (
                !(self.verify_on_core.is_some() && self.stamps == Stamps::Off),
                "stamps on in the verify-on-core ablation",
            ),
            (!(self.resume && self.discarded()), "a journal on disk to resume from (13.5)"),
            (
                self.verifier.is_built(),
                "a build with the `c-secp256k1` feature for the libsecp256k1 verifier (5.7)",
            ),
            (self.journal.max_batch >= 1 && self.journal.max_batch <= MAX_BATCH_RECORDS, "B from 1 to 4,096"),
            (!(eip712 && self.mode != InjectionMode::Signed), "the EIP-712 scheme only in signed mode (5.8)"),
            (
                !(eip712 && self.verify_on_core.is_some()),
                "the perp scheme for the verify-on-core ablation (16: the core verifies perp signatures)",
            ),
            (
                !(eip712 && self.resume),
                "the perp scheme to resume (13.5: v1 doesn't rebuild the EIP-712 replay table from the journal)",
            ),
            (
                !(eip712 && self.timed_flow_ns() > EIP712_MAX_TIMED_FLOW_NS),
                "a timed flow of at most 3 minutes for the EIP-712 scheme (5.8: its messages go stale 5 \
                 minutes after they are signed)",
            ),
        ];
        match rules.iter().find(|(holds, _)| !holds) {
            Some((_, rule)) => Err(format!("run {}: needs {rule}", self.name())),
            None => Ok(()),
        }
    }
}

/// What a summary written before fingerprint key `key` existed must have recorded for it
/// (module docs, "The fingerprint"); `None` for a key every summary records.
pub fn recorded_before(key: &str, summary: &Summary) -> Option<&'static str> {
    match key {
        // Before D-033 every signed run used the nonce scheme.
        "run.auth" if summary.get("run.mode") == Some("signed") => Some(AuthScheme::Perp.name()),
        "run.auth" => Some("none"),
        // Before D-034 every run sent the M3 flow, with Poisson (or uniform) arrivals.
        "run.flow" => Some("m3"),
        "run.makers" => Some("default"),
        "run.bursts" | "run.shock" => Some("none"),
        _ => None,
    }
}

/// `signed` or `preverified`.
pub fn mode_name(mode: InjectionMode) -> &'static str {
    match mode {
        InjectionMode::Signed => "signed",
        InjectionMode::PreVerified => "preverified",
    }
}

/// The mode named by [`mode_name`].
pub fn parse_mode(text: &str) -> Result<InjectionMode, String> {
    match text {
        "signed" => Ok(InjectionMode::Signed),
        "preverified" | "pre-verified" => Ok(InjectionMode::PreVerified),
        _ => Err(format!("not a mode: {text:?} (signed, preverified)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loadgen::market_flow::MarketFlowConfig;
    use loadgen::market_flow::polymarket::ShockSize;

    #[test]
    fn the_defaults_are_the_specs() {
        let config = RunConfig::new(InjectionMode::Signed, 100_000);
        assert_eq!((config.warmup_ns, config.window_ns), (5 * SECOND_NS, 30 * SECOND_NS));
        assert_eq!((config.journal.commit_interval_ns, config.journal.max_batch), (1_000_000, 4_096));
        assert_eq!(config.timed_clients(), 3_520_000, "35.2 s at 100k/s");
        assert_eq!(config.name(), "signed-100k");
        assert_eq!(config.record_bytes(), 152);
        config.check().expect("valid");
    }

    #[test]
    fn the_smoke_runs_are_those_of_18_4() {
        let signed = RunConfig::smoke(InjectionMode::Signed);
        assert_eq!((signed.rate, signed.timed_clients(), signed.lanes), (5_000, 3_000, 2));
        let pre_verified = RunConfig::smoke(InjectionMode::PreVerified);
        assert_eq!((pre_verified.rate, pre_verified.timed_clients()), (20_000, 30_000));
        assert_eq!(pre_verified.flow.markets(), 6);
        let polymarket = RunConfig::polymarket_smoke(InjectionMode::Signed);
        assert_eq!(
            (polymarket.flow.markets(), polymarket.name()),
            (88, "signed-5k-polymarket-shock-stress".into())
        );
        for config in
            [signed, pre_verified, polymarket, RunConfig::polymarket_smoke(InjectionMode::PreVerified)]
        {
            config.check().expect("valid");
            let flow_ns = config.timed_clients() as u64 * SECOND_NS / config.rate;
            assert_eq!(config.warmup_ns + config.window_ns + config.tail_ns, flow_ns, "the flow's length");
        }
    }

    #[test]
    fn the_flow_and_every_switch_are_in_the_name_and_the_fingerprint() {
        let m3 = RunConfig::new(InjectionMode::Signed, 100_000);
        let polymarket = RunConfig { flow: Flow::polymarket(), ..m3.clone() };
        let makers = RunConfig { flow: polymarket.flow.with_makers(3).expect("a switch"), ..m3.clone() };
        let bursts = RunConfig { arrivals: Arrivals::Cox(Bursts::Median), ..polymarket.clone() };
        let shock = RunConfig {
            flow: polymarket.flow.with_shock(ShockSize::Stress).expect("a switch"),
            ..m3.clone()
        };
        let m3_bursts = RunConfig { arrivals: Arrivals::Cox(Bursts::Busiest), ..m3.clone() };
        let names: Vec<String> =
            [&m3, &polymarket, &makers, &bursts, &shock, &m3_bursts].map(|c| c.name()).into();
        assert_eq!(
            names,
            [
                "signed-100k",
                "signed-100k-polymarket",
                "signed-100k-polymarket-makers3",
                "signed-100k-polymarket-bursts-median",
                "signed-100k-polymarket-shock-stress",
                "signed-100k-bursts-busiest",
            ]
        );
        let key = |config: &RunConfig, key: &str| {
            config.fingerprint().into_iter().find(|(k, _)| *k == key).map(|(_, v)| v).expect("a key")
        };
        assert_eq!((key(&m3, "run.flow"), key(&polymarket, "run.flow")), ("m3".into(), "polymarket".into()));
        assert_eq!((key(&m3, "run.makers"), key(&makers, "run.makers")), ("default".into(), "3".into()));
        assert_eq!((key(&m3, "run.bursts"), key(&bursts, "run.bursts")), ("none".into(), "median".into()));
        assert_eq!((key(&m3, "run.shock"), key(&shock, "run.shock")), ("none".into(), "stress".into()));
        // Bursts change only when messages are sent: the same plan, another fingerprint.
        assert_eq!(key(&bursts, "run.flow_digest"), key(&polymarket, "run.flow_digest"));
        let configs = [&m3, &polymarket, &makers, &bursts, &shock, &m3_bursts];
        for (i, a) in configs.iter().enumerate() {
            a.check().expect("valid");
            for b in &configs[i + 1..] {
                assert_ne!(a.fingerprint(), b.fingerprint(), "{} and {}", a.name(), b.name());
            }
        }
        assert_eq!(m3.engine_options(), MarketFlowConfig::m3().engine_options(), "the M3 flow's, unchanged");
    }

    #[test]
    fn a_flow_its_generator_cant_run_is_refused() {
        let polymarket =
            RunConfig { flow: Flow::polymarket(), ..RunConfig::new(InjectionMode::Signed, 100_000) };
        let error =
            RunConfig { flow: polymarket.flow.with_makers(0).expect("a switch"), ..polymarket.clone() }
                .check()
                .expect_err("no maker");
        assert!(error.contains("makers_k of 1 to 20"), "{error}");
        // A market has 20 quotes a side, so a 21st maker would never quote.
        let error = RunConfig { flow: polymarket.flow.with_makers(21).expect("a switch"), ..polymarket }
            .check()
            .expect_err("more makers than the flow allows");
        assert!(error.contains("makers_k of 1 to 20"), "{error}");
    }

    #[test]
    fn a_summary_from_before_the_polymarket_flow_reads_as_the_m3_flow_with_no_switch() {
        let summary = Summary::new();
        let before: Vec<Option<&str>> = ["run.flow", "run.makers", "run.bursts", "run.shock"]
            .map(|key| recorded_before(key, &summary))
            .into();
        assert_eq!(before, [Some("m3"), Some("default"), Some("none"), Some("none")]);
        // What today's fingerprint says for such a run.
        let m3 = RunConfig::new(InjectionMode::PreVerified, 20_000).fingerprint();
        for (key, value) in ["run.flow", "run.makers", "run.bursts", "run.shock"].into_iter().zip(before) {
            assert!(m3.contains(&(key, value.expect("recorded").to_string())), "{key}");
        }
    }

    #[test]
    fn the_fingerprint_tells_runs_apart_that_the_name_does_not() {
        let base = RunConfig::new(InjectionMode::Signed, 100_000);
        let mut smt = base.clone();
        smt.cpus = Cpus::Pinned { gateway_smt: true, overrides: Vec::new() };
        let mut longer = base.clone();
        longer.window_ns = 20 * SECOND_NS;
        let mut more_gateways = base.clone();
        more_gateways.lanes = 10;
        for other in [smt, longer, more_gateways] {
            assert_eq!(other.name(), base.name(), "the same name");
            assert_ne!(other.fingerprint(), base.fingerprint(), "another fingerprint");
        }
        let mut kept = base.clone();
        kept.keep_journal = true;
        assert_eq!(kept.fingerprint(), base.fingerprint(), "keeping the journal measures the same");
    }

    #[test]
    fn the_verifier_is_part_of_a_signed_runs_fingerprint_only() {
        let signed = RunConfig::new(InjectionMode::Signed, 100_000);
        let libsecp = RunConfig { verifier: VerifierKind::LibSecp256k1, ..signed.clone() };
        assert_eq!(signed.verifier_name(), "k256");
        assert_eq!(libsecp.verifier_name(), "libsecp256k1");
        assert_ne!(libsecp.fingerprint(), signed.fingerprint());
        let pre_verified = RunConfig::new(InjectionMode::PreVerified, 20_000);
        let pre_verified_libsecp = RunConfig { verifier: VerifierKind::LibSecp256k1, ..pre_verified.clone() };
        assert_eq!(pre_verified_libsecp.verifier_name(), "none");
        assert_eq!(pre_verified_libsecp.fingerprint(), pre_verified.fingerprint(), "nothing verifies");
        let built = libsecp.check();
        if cfg!(feature = "c-secp256k1") {
            built.expect("valid");
        } else {
            assert!(built.expect_err("refused").contains("c-secp256k1"));
        }
    }

    #[test]
    fn the_eip712_scheme_names_its_runs_and_is_part_of_the_fingerprint_and_identity() {
        let perp = RunConfig::new(InjectionMode::Signed, 100_000);
        let eip712 = RunConfig { auth: AuthScheme::Eip712, ..perp.clone() };
        assert_eq!(eip712.name(), "signed-100k-eip712");
        assert_eq!((perp.auth_name(), eip712.auth_name()), ("perp", "eip712"));
        assert_ne!(eip712.fingerprint(), perp.fingerprint(), "a session never reuses the other scheme's run");
        assert!(eip712.fingerprint().contains(&("run.auth", "eip712".to_string())));
        assert_eq!((perp.identity().auth, eip712.identity().auth), (AuthScheme::Perp, AuthScheme::Eip712));
        eip712.check().expect("valid");
        // A pre-verified run derived from an EIP-712 template signs nothing: the default.
        let pre_verified = eip712.in_mode(InjectionMode::PreVerified);
        assert_eq!((pre_verified.auth, pre_verified.auth_name()), (AuthScheme::Perp, "none"));
        assert_eq!(pre_verified.name(), "preverified-100k");
        pre_verified.check().expect("valid");
        assert_eq!(eip712.in_mode(InjectionMode::Signed).auth, AuthScheme::Eip712, "a signed run keeps it");
    }

    #[test]
    fn the_eip712_scheme_is_refused_where_v1_does_not_support_it() {
        let eip712 = RunConfig { auth: AuthScheme::Eip712, ..RunConfig::new(InjectionMode::Signed, 40_000) };
        let refused = |config: RunConfig, why: &str| {
            let error = config.check().expect_err("refused");
            assert!(error.contains(why), "{error}");
        };
        refused(RunConfig { mode: InjectionMode::PreVerified, ..eip712.clone() }, "only in signed mode");
        refused(RunConfig { verify_on_core: Some(VerifyArm::Gateways), ..eip712.clone() }, "verify-on-core");
        refused(RunConfig { resume: true, ..eip712.clone() }, "the perp scheme to resume");
        refused(RunConfig { window_ns: 180 * SECOND_NS, ..eip712.clone() }, "at most 3 minutes");
        let longest = RunConfig { window_ns: 170 * SECOND_NS, ..eip712.clone() };
        longest.check().expect("5 s + 170 s + 200 ms fits in 3 minutes");
        let many = RunConfig { fixed_timed_clients: Some(40_000 * 181), ..eip712.clone() };
        assert_eq!(many.timed_flow_ns(), 181 * SECOND_NS, "181 s of items at 40k/s");
        refused(many, "at most 3 minutes");
        RunConfig { auth: AuthScheme::Eip712, ..RunConfig::smoke(InjectionMode::Signed) }
            .check()
            .expect("valid");
    }

    #[test]
    fn a_summary_from_before_the_scheme_existed_reads_as_the_perp_scheme() {
        let mut signed = Summary::new();
        signed.put("run.mode", "signed");
        assert_eq!(recorded_before("run.auth", &signed), Some("perp"));
        let mut pre_verified = Summary::new();
        pre_verified.put("run.mode", "preverified");
        assert_eq!(recorded_before("run.auth", &pre_verified), Some("none"));
        assert_eq!(recorded_before("run.rate", &signed), None, "every summary records it");
        // What the fingerprint would say for such runs.
        assert_eq!(RunConfig::new(InjectionMode::Signed, 1).auth_name(), "perp");
        assert_eq!(RunConfig::new(InjectionMode::PreVerified, 1).auth_name(), "none");
    }

    #[test]
    fn names_say_what_differs_and_checks_refuse_what_the_spec_rules_out() {
        let mut config = RunConfig::new(InjectionMode::PreVerified, 20_000);
        config.journal.commit_interval_ns = 250_000;
        assert_eq!(config.name(), "preverified-20k-T250us");
        config.journal.mode = JournalMode::Discard;
        config.capture = true;
        assert!(config.check().expect_err("refused").contains("no replay test in discard mode"));
        let mut ablation = RunConfig::new(InjectionMode::Signed, 40_000);
        ablation.verify_on_core = Some(VerifyArm::Core);
        assert_eq!(ablation.name(), "signed-40k-verify-core");
        ablation.mode = InjectionMode::PreVerified;
        assert!(ablation.check().is_err());
        assert_eq!(parse_mode("pre-verified"), Ok(InjectionMode::PreVerified));
    }
}
