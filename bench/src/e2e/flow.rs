//! Which flow a run sends: the M3 flow (`docs/DECISIONS.md` D-027; `docs/PIPELINE.md` 14.1 to
//! 14.9) or the Polymarket-shaped one (D-034), with its stress switches (14.12).
//!
//! **Contract.** A [`Flow`] is the generator's config of one of the two flows, and gives the
//! harness everything it needs from it:
//! - its plan, [`Flow::generate`]: a `FlowPlan<Flow>` whichever generator made it, so the
//!   workload, the signer, the sender and the restart code (`workload.rs`, `runner.rs`)
//!   handle one type;
//! - its seed, digest, client accounts and engine options, as a [`PlanConfig`], each the
//!   generator's own: an M3 plan, its digest and its arenas are exactly what they were
//!   before this module existed;
//! - its name and switches, for run names, fingerprints and reports (`config.rs`).
//!
//! **The flows.**
//! - [`Flow::m3`]: the M3 flow of section 14 (D-027), the conservative stress flow; and
//!   [`Flow::smoke`], its scaled-down smoke flow (18.4).
//! - [`Flow::polymarket`]: the Polymarket-shaped flow of D-034 (`loadgen`'s
//!   `market_flow::polymarket`): the 88 real markets, with the shape of their recorded
//!   traffic, about 100,000 client messages per second of flow time.
//! - [`Flow::polymarket_smoke`]: the same flow scaled for 18.4's smoke runs (below).
//!
//! **The stress switches** (D-034). Two change what the plan contains, so they are fields of
//! the Polymarket flow's config: `--makers K` ([`Flow::with_makers`]: `K` accounts, 1 to 20,
//! quote every market) and `--shock real|stress` ([`Flow::with_shock`]: correlated multi-market
//! shocks every 10 s of flow time; `stress` adds the liquidation cascade). The M3 flow has
//! neither, so both refuse it. The third switch, `--bursts median|busiest`, changes only
//! when messages are sent, never what they say (14.1): it lives in the run's arrivals
//! (`RunConfig::arrivals`, `Arrivals::Cox`), works with either flow, and leaves the plan, its
//! digest and its signed messages as they are.
//!
//! **Scale.** Both flows hold a fixed number of client messages per second of flow time,
//! about 100,000, whatever the offered rate: the rate changes only the send times (14.1),
//! so runs at different rates apply the same commands and share one signed arena (14.8).
//! At an offered 100k/s flow time runs at about real time, so the Polymarket-shaped flow is
//! Polymarket's shape at about 100 times its volume with its real price speed; at 400k/s
//! its prices move 4 times as fast, as the M3 flow's do.
//!
//! **The Polymarket smoke flow.** At 100,000 messages per flow-second, a smoke run's 3,000
//! messages would cover 30 ms of flow time: no mark (the first comes at 200 ms), no move, no
//! liquidation. So the smoke flow holds 5,000 messages per flow-second (the signed smoke
//! run's rate, so its flow time runs at real time), sends a stress shock every 250 ms of
//! flow time (so the signed smoke run sees two and the pre-verified one 24, each moving 14
//! markets or more, 21 on average, by 2% to 6%), and, as the M3 smoke flow does, gives the
//! insurance fund $1 and has 40 takers: the first loss past a bankruptcy price is then a
//! shortfall, and the smoke test's replay covers liquidations and shortfalls (18.4).
//!
//! **Complexity.** Plain data; [`Flow::generate`] is the generator's.

use engine::engine::EngineOptions;
use engine::types::{AccountId, MarketId};
use loadgen::market_flow::polymarket::{self, PolymarketConfig, ShockConfig, ShockSize};
use loadgen::market_flow::profile::POLYMARKET;
use loadgen::market_flow::{self, DOLLAR, FlowPlan, MarketFlowConfig, PlanConfig};
use loadgen::schedule::Bursts;

/// Flow time between two shocks of the Polymarket smoke flow (module docs): 250 ms.
pub const SMOKE_SHOCK_EVERY_NS: u64 = 250_000_000;
/// Client messages per second of flow time of the Polymarket smoke flow (module docs).
pub const SMOKE_MESSAGES_PER_FLOW_SECOND: u64 = 5_000;

/// The generator's config of one of the two flows (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// The M3 flow (D-027), or its smoke flow.
    M3(MarketFlowConfig),
    /// The Polymarket-shaped flow (D-034), with its switches.
    Polymarket(PolymarketConfig),
}

impl Flow {
    /// The M3 flow of section 14.
    pub fn m3() -> Flow {
        Flow::M3(MarketFlowConfig::m3())
    }

    /// The M3 smoke flow of 18.4.
    pub fn smoke() -> Flow {
        Flow::M3(MarketFlowConfig::smoke())
    }

    /// D-034's flow, with no stress switch.
    pub fn polymarket() -> Flow {
        Flow::Polymarket(PolymarketConfig::default())
    }

    /// The Polymarket flow scaled for smoke runs (module docs).
    pub fn polymarket_smoke() -> Flow {
        let shock = ShockConfig { every_ns: SMOKE_SHOCK_EVERY_NS, ..ShockConfig::stress() };
        Flow::Polymarket(PolymarketConfig {
            messages_per_flow_second: SMOKE_MESSAGES_PER_FLOW_SECOND,
            takers: 40,
            fund_deposit: DOLLAR,
            shock: Some(shock),
            ..PolymarketConfig::default()
        })
    }

    /// The flow named on the command line: `m3`, `smoke` (the M3 smoke flow), `polymarket`
    /// or `polymarket-smoke`.
    pub fn from_name(name: &str) -> Result<Flow, String> {
        match name {
            "m3" => Ok(Flow::m3()),
            "smoke" => Ok(Flow::smoke()),
            "polymarket" => Ok(Flow::polymarket()),
            "polymarket-smoke" => Ok(Flow::polymarket_smoke()),
            _ => Err(format!("not a flow: {name:?} (m3, polymarket, smoke, polymarket-smoke)")),
        }
    }

    /// `m3` or `polymarket`: which generator (the `run.flow` key; a smoke flow is its
    /// generator's).
    pub fn name(&self) -> &'static str {
        match self {
            Flow::M3(_) => "m3",
            Flow::Polymarket(_) => "polymarket",
        }
    }

    /// The same flow with another seed: every random stream, the keys and the engine's hash
    /// seed change (14.6).
    pub fn with_seed(self, seed: u64) -> Flow {
        match self {
            Flow::M3(config) => Flow::M3(MarketFlowConfig { seed, ..config }),
            Flow::Polymarket(config) => Flow::Polymarket(PolymarketConfig { seed, ..config }),
        }
    }

    /// `--makers K` (module docs): `K` market-making accounts, 1 to 20, quote every market
    /// (the flow's check refuses others). The Polymarket flow's only.
    pub fn with_makers(self, makers: u32) -> Result<Flow, String> {
        match self {
            Flow::Polymarket(config) => {
                Ok(Flow::Polymarket(PolymarketConfig { makers_k: Some(makers), ..config }))
            }
            Flow::M3(_) => Err("--makers needs --flow polymarket (D-034)".into()),
        }
    }

    /// `--shock real|stress` (module docs): the flow's shocks, every 10 s of flow time,
    /// replacing any it had. The Polymarket flow's only.
    pub fn with_shock(self, size: ShockSize) -> Result<Flow, String> {
        let shock = match size {
            ShockSize::Calibrated => ShockConfig::real(),
            ShockSize::Stress => ShockConfig::stress(),
        };
        match self {
            Flow::Polymarket(config) => {
                Ok(Flow::Polymarket(PolymarketConfig { shock: Some(shock), ..config }))
            }
            Flow::M3(_) => Err("--shock needs --flow polymarket (D-034)".into()),
        }
    }

    /// The same flow with no shocks (the M3 flow has none anyway).
    pub fn without_shock(self) -> Flow {
        match self {
            Flow::Polymarket(config) => Flow::Polymarket(PolymarketConfig { shock: None, ..config }),
            Flow::M3(_) => self,
        }
    }

    /// The M3 flow with this flow's seed, or this flow itself if it is one: what the
    /// durable limit is measured on, whatever flow a session sends (`session.rs`).
    pub fn m3_counterpart(self) -> Flow {
        match self {
            Flow::M3(_) => self,
            Flow::Polymarket(config) => Flow::m3().with_seed(config.seed),
        }
    }

    /// The Polymarket flow with this flow's seed, or this flow itself if it is one: what
    /// the D-034 headlines run (`sweep.rs`).
    pub fn polymarket_counterpart(self) -> Flow {
        match self {
            Flow::Polymarket(_) => self,
            Flow::M3(config) => Flow::polymarket().with_seed(config.seed),
        }
    }

    /// `--makers K`'s `K`, if set.
    pub fn makers(&self) -> Option<u32> {
        match self {
            Flow::M3(_) => None,
            Flow::Polymarket(config) => config.makers_k,
        }
    }

    /// The size of the flow's shocks, if it has any.
    pub fn shock(&self) -> Option<ShockSize> {
        match self {
            Flow::M3(_) => None,
            Flow::Polymarket(config) => config.shock.map(|shock| shock.size),
        }
    }

    /// Markets in the flow: 67 in the M3 flow, 88 in the Polymarket one.
    pub fn markets(&self) -> usize {
        match self {
            Flow::M3(config) => usize::from(config.markets),
            Flow::Polymarket(config) => config.markets().len(),
        }
    }

    /// How reports name a market: its symbol in the Polymarket flow (`BTC-USD`), else
    /// `market 12`.
    pub fn market_label(&self, market: MarketId) -> String {
        match self {
            Flow::Polymarket(_) => match POLYMARKET.market(market) {
                Some(spec) => spec.symbol.to_string(),
                None => format!("market {market}"),
            },
            Flow::M3(_) => format!("market {market}"),
        }
    }

    /// Refuses a config its generator can't run (the generator's own check).
    pub fn check(&self) -> Result<(), String> {
        match self {
            Flow::M3(config) => config.check(),
            Flow::Polymarket(config) => config.check(),
        }
    }

    /// The plan: the setup phases, then the timed flow up to its `timed_client_items`-th
    /// client item, from this flow's generator, with this flow as its config.
    pub fn generate(&self, timed_client_items: usize) -> FlowPlan<Flow> {
        match self {
            Flow::M3(config) => with_config(market_flow::generate(config, timed_client_items), *self),
            Flow::Polymarket(config) => with_config(polymarket::generate(config, timed_client_items), *self),
        }
    }
}

/// `plan` with `config` in place of its generator's config: the items are the same.
fn with_config<C>(plan: FlowPlan<C>, config: Flow) -> FlowPlan<Flow> {
    let FlowPlan { config: _, setup_a, setup_b1, setup_b2, timed, jumps, flow_ns } = plan;
    FlowPlan { config, setup_a, setup_b1, setup_b2, timed, jumps, flow_ns }
}

/// Each flow's generator config is a [`PlanConfig`]; so is a `Flow`, through it (module
/// docs): the M3 flow's digest, keys and engine options are unchanged.
impl PlanConfig for Flow {
    fn seed(&self) -> u64 {
        match self {
            Flow::M3(config) => config.seed,
            Flow::Polymarket(config) => config.seed,
        }
    }

    fn digest(&self) -> [u8; 32] {
        match self {
            Flow::M3(config) => config.digest(),
            Flow::Polymarket(config) => config.digest(),
        }
    }

    fn client_accounts(&self) -> Vec<AccountId> {
        match self {
            Flow::M3(config) => config.client_accounts(),
            Flow::Polymarket(config) => config.client_accounts(),
        }
    }

    fn engine_options(&self) -> EngineOptions {
        match self {
            Flow::M3(config) => config.engine_options(),
            Flow::Polymarket(config) => config.engine_options(),
        }
    }
}

/// `--bursts`'s name for a preset: `median` or `busiest` (the recorded hour whose burstiness
/// it reproduces).
pub fn bursts_name(bursts: Bursts) -> &'static str {
    match bursts {
        Bursts::Median => "median",
        Bursts::Busiest => "busiest",
    }
}

/// The preset `--bursts` names.
pub fn parse_bursts(text: &str) -> Result<Bursts, String> {
    match text {
        "median" => Ok(Bursts::Median),
        "busiest" => Ok(Bursts::Busiest),
        _ => Err(format!("not a burst preset: {text:?} (median, busiest)")),
    }
}

/// `--shock`'s name for a size: `real` (the recorded sizes) or `stress` (2% to 6%, with the
/// cascade cohort).
pub fn shock_name(size: ShockSize) -> &'static str {
    match size {
        ShockSize::Calibrated => "real",
        ShockSize::Stress => "stress",
    }
}

/// The size `--shock` names.
pub fn parse_shock(text: &str) -> Result<ShockSize, String> {
    match text {
        "real" => Ok(ShockSize::Calibrated),
        "stress" => Ok(ShockSize::Stress),
        _ => Err(format!("not a shock size: {text:?} (real, stress)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flow_is_its_generators_config() {
        let m3 = Flow::m3();
        assert_eq!(m3.digest(), MarketFlowConfig::m3().digest(), "an M3 arena's digest is unchanged");
        assert_eq!(m3.client_accounts(), MarketFlowConfig::m3().client_accounts());
        let polymarket = Flow::polymarket();
        assert_eq!(polymarket.digest(), PolymarketConfig::default().digest());
        assert_ne!(polymarket.digest(), m3.digest());
        assert_eq!((m3.markets(), polymarket.markets()), (67, 88));
        assert_eq!((m3.name(), polymarket.name(), Flow::smoke().name()), ("m3", "polymarket", "m3"));
        for name in ["m3", "smoke", "polymarket", "polymarket-smoke"] {
            Flow::from_name(name).expect("a flow").check().expect("valid");
        }
        assert!(Flow::from_name("m4").expect_err("unknown").contains("not a flow"));
    }

    #[test]
    fn a_plan_is_the_generators_with_the_flow_as_its_config() {
        let flow = Flow::smoke();
        let plan = flow.generate(500);
        let direct = market_flow::generate(&MarketFlowConfig::smoke(), 500);
        assert_eq!(plan.config, flow);
        assert_eq!(
            (&plan.setup_a, &plan.setup_b1, &plan.setup_b2),
            (&direct.setup_a, &direct.setup_b1, &direct.setup_b2)
        );
        assert_eq!((&plan.timed, &plan.jumps, plan.flow_ns), (&direct.timed, &direct.jumps, direct.flow_ns));
        let polymarket = Flow::polymarket_smoke().generate(100);
        assert_eq!(polymarket.timed.iter().filter(|item| item.is_client()).count(), 100);
    }

    #[test]
    fn the_switches_are_the_polymarket_flows_and_the_seed_is_every_flows() {
        let flow = Flow::polymarket().with_makers(3).expect("a switch of this flow");
        let flow = flow.with_shock(ShockSize::Stress).expect("a switch of this flow");
        assert_eq!((flow.makers(), flow.shock()), (Some(3), Some(ShockSize::Stress)));
        assert_ne!(flow.digest(), Flow::polymarket().digest(), "the switches change the plan");
        assert!(Flow::m3().with_makers(3).expect_err("no makers switch").contains("--flow polymarket"));
        assert!(Flow::m3().with_shock(ShockSize::Calibrated).is_err());
        assert_eq!((Flow::m3().makers(), Flow::m3().shock()), (None, None));
        // The smoke flow's own stress shock, replaced by the recorded sizes every 10 s.
        let real = Flow::polymarket_smoke().with_shock(ShockSize::Calibrated).expect("a switch");
        assert_eq!(real.shock(), Some(ShockSize::Calibrated));
        assert_eq!(Flow::m3().with_seed(7).seed(), 7);
        assert_eq!(Flow::polymarket().with_seed(7).seed(), 7);
        assert_eq!(flow.m3_counterpart(), Flow::m3());
        assert_eq!(Flow::m3().with_seed(4).polymarket_counterpart(), Flow::polymarket().with_seed(4));
        assert_eq!(
            (parse_bursts("busiest"), parse_shock("real")),
            (Ok(Bursts::Busiest), Ok(ShockSize::Calibrated))
        );
        assert!(parse_bursts("worst").is_err() && parse_shock("huge").is_err());
    }

    #[test]
    fn the_engine_has_room_for_every_account_of_either_flow_in_every_market() {
        // The engine reserves its capacities up front and allocates nothing after setup
        // (D-010), as long as the flow stays inside them: every client account and the fund
        // in `account_capacity`, and a slot in one market for each account that may trade
        // it (any taker may trade any market) in `slot_capacity`. The largest configs of
        // each flow: the M3 flow, and the Polymarket flow with its cascade cohort and the
        // most makers its check allows, 1,000 in 50 groups of 20.
        let polymarket = Flow::polymarket().with_shock(ShockSize::Stress).expect("a switch");
        let Flow::Polymarket(config) = polymarket else { unreachable!("the Polymarket flow") };
        let most_makers =
            Flow::Polymarket(PolymarketConfig { makers: 1_000, makers_per_market: 20, ..config });
        assert!(polymarket.with_makers(21).expect("a switch").check().is_err(), "at most 20");
        for flow in [Flow::m3(), Flow::polymarket(), polymarket, most_makers] {
            flow.check().expect("valid");
            let (accounts, options) = (flow.client_accounts().len(), flow.engine_options());
            assert!(accounts < options.account_capacity, "{accounts} accounts and the fund");
            assert!(accounts <= options.slot_capacity, "{accounts} accounts in one market");
        }
        // The Polymarket flow's makers keep 20 quotes a side in each market, far inside the
        // 4,096 orders a market's book reserves.
        let plan = Flow::polymarket().generate(0);
        let quotes_per_market = plan.setup_b1.len() / Flow::polymarket().markets();
        assert_eq!(quotes_per_market, 40);
        assert!(quotes_per_market * 100 < Flow::polymarket().engine_options().order_capacity);
    }

    #[test]
    fn markets_are_named_by_symbol_in_the_polymarket_flow() {
        let first = &POLYMARKET.markets[0];
        assert_eq!(Flow::polymarket().market_label(first.id), first.symbol);
        assert_eq!(Flow::m3().market_label(12), "market 12");
    }
}
