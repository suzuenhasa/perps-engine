//! What a run sends: the flow plan and its messages, built before any pipeline thread
//! starts (`docs/PIPELINE.md` 14.1, 14.8, 13.5 and 5.8).
//!
//! **Contract.**
//! - [`Workload::build`] generates the plan for a run's timed client items and builds one
//!   message per client item: signed on every allowed CPU (`presign`), in the run's signing
//!   scheme, or compact for pre-verified runs. It prints how long each part took, and for
//!   signing the estimate from the probed `t_sign` first, when a probe measured one (14.8).
//! - **Reused as a prefix** (14.8). A longer plan starts with every item of a shorter one,
//!   and each run starts a fresh engine, journal and nonce state, so one workload serves
//!   every run of its flow, mode, deployment and scheme that needs as many items or fewer.
//!   The flow is told by its config's digest (`flow.rs`): the M3 flow and the
//!   Polymarket-shaped one never share a workload, nor do two settings of `--makers` or
//!   `--shock`, which change the plan; runs with and without `--bursts` do, since bursts
//!   change only the send times (D-034).
//!   [`Workloads`] keeps one workload per flow, mode, deployment and scheme, and builds a
//!   new one only when a run needs more items than the kept one has, or (below) when the
//!   kept one's messages would go stale; [`Workloads::reserve`] builds each perp workload
//!   for the largest run of a sweep before the sweep starts, so interleaved repetitions
//!   (A B C, A B C) never sign the same messages twice.
//! - **Fresh** (5.8; D-033). An EIP-712 message carries the time it was signed, `ts`, and
//!   the gateways refuse it once `ts` is 5 minutes behind their clock (`MAX_AGE_MS`). So
//!   an EIP-712 workload serves a run only if its messages will still be inside that window
//!   when the run sends its last one: their age now, plus how long the run sends (its setup
//!   phases at `SETUP_RATE`, then its warm-up, window and tail), plus
//!   [`FRESHNESS_MARGIN_MS`] for everything else (starting the pipeline, the setup
//!   barriers' waits, a late sender), must be at most 5 minutes. Otherwise
//!   [`Workloads::get`] signs the messages again, with the time then. Their `ts` is the time
//!   signing started, so signing takes part of the 5 minutes too: `get` checks the new
//!   messages the same way once they are signed, and refuses the run, with how long signing
//!   took, if they are stale already. `reserve` leaves EIP-712 workloads to `get`, just
//!   before their runs: signed ahead for a whole sweep, they would go stale. A perp message
//!   never expires, so its workload is always fresh.
//! - **The rest of the flow after a restart** ([`rest_of_plan`], 13.5). The operator ring is
//!   FIFO and the sequencer takes it in order, so the journal holds a prefix of the plan's
//!   operator items: the first `operator_done` are done. A client resends what it has no
//!   result for: every item whose nonce is above its account's last journaled nonce (the
//!   replayed nonce table, 6.3). The rest keeps every item's nonce, so the gateways accept
//!   it, and the order of the plan. (Perp scheme only: `RunConfig::check` refuses to resume
//!   an EIP-712 run.)
//!
//! **Complexity.** Generation is O(items); signing costs `messages × t_sign / threads`.

use std::sync::Arc;
use std::time::Instant;

use gateway::check::MAX_AGE_MS;
use loadgen::market_flow::{FlowPlan, Item, Jump, PlanConfig};
use loadgen::presign::{presign, presign_eip712, preverified, signing_threads, unix_now_ms};
use loadgen::sender::{Messages, SETUP_RATE};
use pipeline::records::{AuthScheme, InjectionMode};
use pipeline::replay::NonceTable;

use super::config::RunConfig;
use super::flow::Flow;
use super::units::{SECOND_NS, count};

/// What an EIP-712 run may take beyond sending its setup phases and its timed flow, for
/// its messages to count as fresh (module docs, "Fresh"): a minute.
pub const FRESHNESS_MARGIN_MS: u64 = 60_000;

/// A flow plan and a message for each of its client items.
#[derive(Clone, Debug)]
pub struct Workload {
    pub plan: FlowPlan<Flow>,
    pub messages: Messages,
    pub mode: InjectionMode,
    pub deployment: u32,
    /// How the messages are signed: `Perp` in a pre-verified workload, which signs nothing.
    pub auth: AuthScheme,
}

impl Workload {
    /// Generates the plan for `config`'s timed client items and builds its messages (module
    /// docs). `sign_ns` is the probed cost of one signature, for the estimate.
    pub fn build(config: &RunConfig, sign_ns: Option<u64>) -> Workload {
        let started = Instant::now();
        let plan = config.flow.generate(config.timed_clients());
        eprintln!(
            "workload: generated {} setup client items and {} timed items in {:.1} s",
            count(plan.setup_client_items() as u64),
            count(plan.timed.len() as u64),
            started.elapsed().as_secs_f64()
        );
        Workload::from_plan(plan, config, sign_ns)
    }

    /// Builds the messages of `plan`'s client items for `config`'s mode, deployment and
    /// signing scheme.
    pub fn from_plan(plan: FlowPlan<Flow>, config: &RunConfig, sign_ns: Option<u64>) -> Workload {
        let started = Instant::now();
        let (mode, deployment, auth) = (config.mode, config.deployment, config.auth);
        let messages = match mode {
            InjectionMode::Signed => {
                let threads = signing_threads();
                let messages = plan.client_items().count() as u64;
                if let Some(sign_ns) = sign_ns {
                    let estimate = messages * sign_ns / threads as u64;
                    eprintln!(
                        "workload: signing {} messages on {threads} threads, estimated {:.1} s",
                        count(messages),
                        estimate as f64 / 1e9
                    );
                }
                let arena = match auth {
                    AuthScheme::Perp => presign(&plan, deployment, threads),
                    AuthScheme::Eip712 => {
                        let ts_ms = unix_now_ms();
                        eprintln!(
                            "workload: EIP-712 messages with ts = {ts_ms} ms: fresh for 5 minutes (5.8)"
                        );
                        presign_eip712(&plan, deployment, ts_ms, threads)
                    }
                };
                Messages::Signed(Arc::new(arena))
            }
            InjectionMode::PreVerified => Messages::PreVerified(Arc::new(preverified(&plan))),
        };
        eprintln!(
            "workload: {} messages built in {:.1} s",
            count(messages.len() as u64),
            started.elapsed().as_secs_f64()
        );
        Workload { plan, messages, mode, deployment, auth }
    }

    /// Timed client items in the plan.
    pub fn timed_clients(&self) -> usize {
        self.plan.timed.iter().filter(|item| item.is_client()).count()
    }

    /// When the messages were signed, in milliseconds since the UNIX epoch, if they expire:
    /// an EIP-712 workload's `ts`. `None` for perp messages and pre-verified items.
    pub fn signed_at_ms(&self) -> Option<u64> {
        match &self.messages {
            Messages::Signed(arena) if arena.header().auth == AuthScheme::Eip712 => {
                Some(arena.header().signed_at_ms)
            }
            _ => None,
        }
    }

    /// True if this workload's messages are those of `config`'s flow, mode, deployment and
    /// scheme.
    fn is_for(&self, config: &RunConfig) -> bool {
        self.plan.config.digest() == config.flow.digest()
            && self.mode == config.mode
            && self.deployment == config.deployment
            && self.auth == config.auth
    }

    /// True if this workload can serve `config`'s run, started at wall-clock time `now_ms`
    /// (module docs, "Reused as a prefix" and "Fresh").
    pub fn serves(&self, config: &RunConfig, now_ms: u64) -> bool {
        self.is_for(config)
            && self.timed_clients() >= config.timed_clients()
            && self.fresh_for(config, now_ms)
    }

    /// True if every message will still be inside the gateways' timestamp window when
    /// `config`'s run, started at `now_ms`, sends its last one (module docs, "Fresh").
    pub fn fresh_for(&self, config: &RunConfig, now_ms: u64) -> bool {
        self.age_at_end_ms(config, now_ms).is_none_or(|age| age <= MAX_AGE_MS)
    }

    /// How old the messages will be, at most, when `config`'s run, started at `now_ms`,
    /// sends its last one: their age now, how long the run sends, and the margin (module
    /// docs, "Fresh"). `None` if they never expire; `u64::MAX` if they were signed after
    /// `now_ms`, which means the wall clock was stepped back, so their age can't be trusted.
    pub fn age_at_end_ms(&self, config: &RunConfig, now_ms: u64) -> Option<u64> {
        let signed_at = self.signed_at_ms()?;
        let Some(age_now) = now_ms.checked_sub(signed_at) else { return Some(u64::MAX) };
        Some(age_now.saturating_add(self.sending_ms(config)).saturating_add(FRESHNESS_MARGIN_MS))
    }

    /// How long `config`'s run sends, in milliseconds (module docs, "Fresh"): every item of
    /// its setup phases at `SETUP_RATE` (14.4), then its timed flow.
    fn sending_ms(&self, config: &RunConfig) -> u64 {
        let plan = &self.plan;
        let setup_items = (plan.setup_a.len() + plan.setup_b1.len() + plan.setup_b2.len()) as u64;
        let setup_ns = setup_items * SECOND_NS / SETUP_RATE;
        (setup_ns + config.timed_flow_ns()).div_ceil(1_000_000)
    }
}

/// The workloads built so far, at most one per flow, mode, deployment and scheme, kept for
/// the next runs (module docs).
#[derive(Debug, Default)]
pub struct Workloads {
    kept: Vec<Workload>,
    /// The probed cost of one signature, for the estimates.
    pub sign_ns: Option<u64>,
}

impl Workloads {
    /// A workload that serves `config`'s run, starting now, building one only if none kept
    /// does. A new one replaces the kept one of its flow, mode, deployment and scheme, which
    /// is shorter, or stale: the new one serves every run the old one did. An error if the
    /// new one is stale already: EIP-712 messages that took so long to sign that the run
    /// can't send them all within 5 minutes of their `ts` (module docs, "Fresh").
    pub fn get(&mut self, config: &RunConfig) -> Result<&Workload, String> {
        self.get_at(config, unix_now_ms())
    }

    /// [`Workloads::get`] for a run that starts at wall-clock time `now_ms`.
    fn get_at(&mut self, config: &RunConfig, now_ms: u64) -> Result<&Workload, String> {
        if let Some(index) = self.kept.iter().position(|w| w.serves(config, now_ms)) {
            return Ok(&self.kept[index]);
        }
        let long_enough = |w: &&Workload| w.is_for(config) && w.timed_clients() >= config.timed_clients();
        if let Some(age) = self.kept.iter().find(long_enough).and_then(|w| w.age_at_end_ms(config, now_ms)) {
            let why = match age {
                u64::MAX => "were signed after now (the wall clock was stepped back)".to_string(),
                age => format!("would be {} s old when this run ends: over 5 minutes", age / 1_000),
            };
            eprintln!("workload: the kept EIP-712 messages {why}: signing them again");
        }
        // Free the shorter, or stale, one before building its replacement.
        self.kept.retain(|w| !w.is_for(config));
        self.kept.push(Workload::build(config, self.sign_ns));
        let new = self.kept.last().expect("just pushed");
        // New EIP-712 messages carry the time signing started, so signing itself took part
        // of their 5 minutes: check that what is left still covers the run, from the clock
        // once signing is done.
        let signed_ms = unix_now_ms();
        match new.age_at_end_ms(config, signed_ms) {
            Some(u64::MAX) => {
                Err("the wall clock was stepped back while the EIP-712 messages were signed (5.8, 14.8)"
                    .into())
            }
            Some(age) if age > MAX_AGE_MS => {
                let signing_s = signed_ms.saturating_sub(new.signed_at_ms().unwrap_or(signed_ms)) / 1_000;
                Err(format!(
                    "the EIP-712 messages took {signing_s} s to sign, so they would be {} s old when this run \
                     ends: over the gateways' 5 minutes (5.8, 14.8). Shorten the run, or sign on more threads",
                    age / 1_000
                ))
            }
            _ => Ok(new),
        }
    }

    /// Builds, for each flow, mode, deployment and scheme among `configs`, the workload of
    /// the largest of their runs, unless one kept already serves it (module docs: built once
    /// for the largest run a sweep needs, 14.8). EIP-712 runs are left to `get` (module docs,
    /// "Fresh").
    pub fn reserve<'a>(&mut self, configs: impl IntoIterator<Item = &'a RunConfig>) {
        let mut largest: Vec<&RunConfig> = Vec::new();
        for config in configs.into_iter().filter(|c| c.auth != AuthScheme::Eip712) {
            let same = |c: &&RunConfig| {
                c.flow.digest() == config.flow.digest()
                    && c.mode == config.mode
                    && c.deployment == config.deployment
                    && c.auth == config.auth
            };
            match largest.iter_mut().find(|c| same(c)) {
                Some(kept) if kept.timed_clients() < config.timed_clients() => *kept = config,
                Some(_) => {}
                None => largest.push(config),
            }
        }
        for config in largest {
            self.get(config).expect("a perp workload never goes stale");
        }
    }
}

/// The part of `plan` a restarted life still has to send (module docs): the operator items
/// after the first `operator_done`, and each client item whose nonce is above its account's
/// in `nonces`. Jumps whose mark was already sent are dropped, and the others re-indexed.
pub fn rest_of_plan(plan: &FlowPlan<Flow>, operator_done: u64, nonces: &NonceTable) -> FlowPlan<Flow> {
    let mut operators_seen = 0;
    let mut keep = |item: &Item| match item {
        Item::Operator(_) => {
            operators_seen += 1;
            operators_seen > operator_done
        }
        Item::Client(client) => client.nonce > nonces.get(client.account),
    };
    let mut rest = FlowPlan { timed: Vec::new(), jumps: Vec::new(), ..plan.clone() };
    rest.setup_a = plan.setup_a.iter().copied().filter(&mut keep).collect();
    rest.setup_b1 = plan.setup_b1.iter().copied().filter(&mut keep).collect();
    rest.setup_b2 = plan.setup_b2.iter().copied().filter(&mut keep).collect();
    // The timed items, with where each one lands in the rest, for the jumps.
    let mut new_index = vec![None; plan.timed.len()];
    for (i, item) in plan.timed.iter().enumerate() {
        if keep(item) {
            new_index[i] = Some(rest.timed.len());
            rest.timed.push(*item);
        }
    }
    rest.jumps = plan
        .jumps
        .iter()
        .filter_map(|jump| new_index.get(jump.item).copied().flatten().map(|item| (jump, item)))
        .map(|(jump, item)| Jump { item, ..*jump })
        .collect();
    rest
}

#[cfg(test)]
mod tests {
    use super::*;
    use loadgen::schedule::{Arrivals, Bursts};

    fn smoke_plan() -> FlowPlan<Flow> {
        Flow::smoke().generate(2_000)
    }

    #[test]
    fn a_workload_serves_only_its_own_flow_and_bursts_share_it() {
        let pre_verified = |config: RunConfig| RunConfig { fixed_timed_clients: Some(300), ..config };
        let m3 = pre_verified(RunConfig::smoke(InjectionMode::PreVerified));
        let polymarket = pre_verified(RunConfig::polymarket_smoke(InjectionMode::PreVerified));
        let bursts = RunConfig { arrivals: Arrivals::Cox(Bursts::Median), ..polymarket.clone() };
        let makers =
            RunConfig { flow: polymarket.flow.with_makers(3).expect("a switch"), ..polymarket.clone() };
        let mut workloads = Workloads::default();
        workloads.reserve([&m3, &polymarket, &bursts, &makers]);
        let digests: Vec<[u8; 32]> = workloads.kept.iter().map(|w| w.plan.config.digest()).collect();
        assert_eq!(
            digests,
            [m3.flow.digest(), polymarket.flow.digest(), makers.flow.digest()],
            "one per plan: the bursty run shares the plain one's"
        );
        let workload = workloads.get(&bursts).expect("fresh");
        assert_eq!(workload.plan.config, polymarket.flow);
        assert_eq!(workload.plan.setup_b1.len(), 88 * 40, "20 quotes a side in each of the 88 markets");
        assert_eq!(workloads.kept.len(), 3, "nothing new was built");
    }

    #[test]
    fn the_rest_of_a_polymarket_plan_after_its_setup_is_its_timed_flow() {
        let plan = Flow::polymarket_smoke().generate(500);
        let mut nonces = NonceTable::new();
        for item in plan.setup_b1.iter().chain(&plan.setup_b2) {
            if let Item::Client(client) = item {
                nonces.note(client.account, client.nonce);
            }
        }
        let rest = rest_of_plan(&plan, plan.setup_a.len() as u64, &nonces);
        assert!(rest.setup_a.is_empty() && rest.setup_b1.is_empty() && rest.setup_b2.is_empty());
        assert_eq!((rest.timed, rest.jumps), (plan.timed, plan.jumps));
    }

    #[test]
    fn workloads_are_kept_per_mode_and_built_once_for_the_largest_run() {
        let smoke =
            |mode, clients| RunConfig { fixed_timed_clients: Some(clients), ..RunConfig::smoke(mode) };
        let (pre_verified, signed) = (InjectionMode::PreVerified, InjectionMode::Signed);
        let configs =
            [smoke(pre_verified, 500), smoke(signed, 100), smoke(pre_verified, 2_000), smoke(signed, 300)];
        let mut workloads = Workloads::default();
        workloads.reserve(&configs);
        let sizes = |w: &Workloads| -> Vec<(InjectionMode, usize)> {
            w.kept.iter().map(|workload| (workload.mode, workload.timed_clients())).collect()
        };
        assert_eq!(
            sizes(&workloads),
            [(pre_verified, 2_000), (signed, 300)],
            "one each, for the largest run"
        );
        // Interleaved runs, in any order, are all served by those two: nothing is rebuilt.
        for config in configs.iter().rev().chain(&configs) {
            workloads.get(config).expect("fresh");
        }
        assert_eq!(sizes(&workloads), [(pre_verified, 2_000), (signed, 300)]);
        // A longer run replaces its mode's workload, and leaves the other mode's alone.
        let longer = smoke(pre_verified, 3_000);
        workloads.get(&longer).expect("fresh");
        assert_eq!(sizes(&workloads), [(signed, 300), (pre_verified, 3_000)]);
    }

    /// The arena of a signed workload, to tell a kept one from a new one.
    fn arena(workload: &Workload) -> Arc<loadgen::presign::Arena> {
        match &workload.messages {
            Messages::Signed(arena) => Arc::clone(arena),
            Messages::PreVerified(_) => panic!("a signed workload"),
        }
    }

    #[test]
    fn eip712_workloads_are_kept_apart_built_just_before_their_runs_and_signed_again_when_stale() {
        let perp = RunConfig { fixed_timed_clients: Some(100), ..RunConfig::smoke(InjectionMode::Signed) };
        let eip712 = RunConfig { auth: AuthScheme::Eip712, ..perp.clone() };
        let mut workloads = Workloads::default();
        workloads.reserve([&perp, &eip712]);
        assert_eq!(workloads.kept.len(), 1, "reserve signs the perp workload only");
        assert_eq!(workloads.kept[0].auth, AuthScheme::Perp);

        let first = workloads.get(&eip712).expect("fresh");
        assert_eq!(first.auth, AuthScheme::Eip712);
        let signed_at = first.signed_at_ms().expect("EIP-712 messages carry their time");
        let first = arena(first);
        assert_eq!(workloads.kept.len(), 2, "one per scheme");
        assert!(
            Arc::ptr_eq(&arena(workloads.get_at(&eip712, signed_at).expect("fresh")), &first),
            "fresh: kept"
        );

        // Fresh while the age at the run's end, margin included, is at most 5 minutes.
        let kept = workloads.kept.iter().find(|w| w.auth == AuthScheme::Eip712).expect("kept");
        let sending = kept.sending_ms(&eip712);
        assert!(sending >= 600, "at least the smoke run's warm-up, window and tail: {sending} ms");
        let last_fresh = signed_at + MAX_AGE_MS - FRESHNESS_MARGIN_MS - sending;
        assert!(kept.fresh_for(&eip712, last_fresh));
        assert!(!kept.fresh_for(&eip712, last_fresh + 1));
        assert!(!kept.fresh_for(&eip712, signed_at - 1), "signed in the future: the clock stepped back");
        let perp_workload = &workloads.kept[0];
        assert_eq!(perp_workload.age_at_end_ms(&perp, u64::MAX), None, "a perp message never expires");
        assert!(perp_workload.fresh_for(&perp, u64::MAX));

        // Stale: signed again, and it replaces the old one.
        let again = arena(workloads.get_at(&eip712, last_fresh + 1).expect("signed again"));
        assert!(!Arc::ptr_eq(&again, &first));
        assert!(again.header().signed_at_ms >= signed_at);
        assert_eq!(workloads.kept.len(), 2);
    }

    #[test]
    fn a_run_that_newly_signed_eip712_messages_cant_last_through_is_refused() {
        // A timed flow of 5 minutes: however fast the signing, the last message would be
        // over 5 minutes old, margin included. (`RunConfig::check` caps it at 3 minutes; the
        // signing time of a large run can use up the rest the same way.)
        let long = RunConfig {
            auth: AuthScheme::Eip712,
            fixed_timed_clients: Some(100),
            window_ns: MAX_AGE_MS * 1_000_000,
            ..RunConfig::smoke(InjectionMode::Signed)
        };
        let error = Workloads::default().get(&long).expect_err("stale when signed");
        assert!(
            error.contains("s to sign, so they would be ") && error.contains("over the gateways' 5 minutes")
        );
        // The same run, shorter: the messages just signed serve it.
        let short = RunConfig { window_ns: SECOND_NS, ..long };
        assert!(Workloads::default().get(&short).is_ok());
    }

    #[test]
    fn the_rest_after_nothing_is_the_whole_plan() {
        let plan = smoke_plan();
        let rest = rest_of_plan(&plan, 0, &NonceTable::new());
        assert_eq!(
            (rest.setup_a, rest.setup_b1, rest.setup_b2),
            (plan.setup_a.clone(), plan.setup_b1, plan.setup_b2)
        );
        assert_eq!((rest.timed, rest.jumps), (plan.timed, plan.jumps));
    }

    #[test]
    fn the_rest_skips_the_journaled_operator_prefix_and_each_accounts_journaled_nonces() {
        let plan = smoke_plan();
        let setup_operators = plan.setup_a.len() as u64;
        // Setup A done, plus the first 3 operator items of the timed flow; every client item
        // of the first 1,000 timed items done (their accounts' nonces noted).
        let mut nonces = NonceTable::new();
        let mut timed_operators_done = 0;
        for item in plan.setup_b1.iter().chain(&plan.setup_b2).chain(&plan.timed[..1_000]) {
            match item {
                Item::Client(client) => nonces.note(client.account, client.nonce),
                Item::Operator(_) if timed_operators_done < 3 => timed_operators_done += 1,
                Item::Operator(_) => {}
            }
        }
        let rest = rest_of_plan(&plan, setup_operators + 3, &nonces);
        assert!(rest.setup_a.is_empty() && rest.setup_b1.is_empty() && rest.setup_b2.is_empty());
        let clients_after: Vec<&Item> = plan.timed[1_000..].iter().filter(|i| i.is_client()).collect();
        let clients_rest: Vec<&Item> = rest.timed.iter().filter(|i| i.is_client()).collect();
        assert_eq!(clients_rest, clients_after, "exactly the client items not yet journaled, in order");
        let operators_total = plan.timed.iter().filter(|i| !i.is_client()).count();
        assert_eq!(rest.timed.iter().filter(|i| !i.is_client()).count(), operators_total - 3);
        for jump in &rest.jumps {
            assert!(matches!(rest.timed[jump.item], Item::Operator(_)), "re-indexed onto its mark");
        }
    }
}
