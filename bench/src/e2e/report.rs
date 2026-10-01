//! Markdown for `docs/BENCHMARKS.md`, built only from summaries (`docs/PIPELINE.md` 15.11).
//!
//! **Contract.**
//! - [`run_report`]: one run, as a fragment: what ran and where (its signing scheme and
//!   verifier included, 5.8 and 5.7), the verdict and its flags,
//!   the rates and losses, every stage's count, p50, p99, p99.9 and max (15.4), the journal,
//!   the per-run breakdown of 15.9 (commands by type and outcome, drops and gateway rejects,
//!   results, flow content, liquidations, the fund's drawdown and peak shortfall), thread
//!   health and the limiting thread, and the replay test and the audit when they ran.
//! - [`session_report`]: a session directory, as the M3 section of BENCHMARKS.md, in 15.11's
//!   order: the machine and its probes; the offered-load sweep per mode; the three
//!   searches; the headline; the `T` sweep; the headline run's breakdown; the replay test,
//!   the audit and the second seed; the two ablations; the cost of measurement; caveats,
//!   with every invalid run and its reason. A section with no data yet says so.
//!
//! **The flow** (D-034). Every run and search names its flow, the M3 flow (D-027) or the
//! Polymarket-shaped one, with its stress switches (makers, bursts, shock), and a run's
//! report adds the flow's shape in the window: the busiest account's share and rate against
//! its gateway's load, each gateway's offered messages and load, the longest `SetMark` core
//! service, the most events of one command, the takers' IOCs a second and fills per IOC,
//! and how concentrated the traffic was across markets. The session gives each flow (with
//! its switches) and each scheme its own headline, the M3 flow's first, all against the one
//! durable limit, which the M3 flow's pre-verified 20k/s runs set (`session::limit_point`).
//! A summary written before the flows existed was the M3 flow's.
//!
//! **The signing scheme** (5.8). Every signed run, search and headline names its scheme:
//! `perp`, or `eip712` (Polymarket Perps' format), whose section 17 counts gateways from the
//! probe's signer check (`<verifier>.recover_ns`) instead of its verification. A summary
//! written before the scheme existed was a perp run (`config::recorded_before`).
//!
//! Repeated runs are shown as the median, with the range in brackets (15.5), **over the
//! valid runs only**: an invalid run is listed with its reason at the end and used nowhere
//! else, and each point says how many of its runs were valid (review finding F3). Latencies
//! are over offered commands wherever commands were lost ("∞" once more than 1% was not
//! served, 15.4); at overload points "of completed" is shown next to it. Next to the
//! achieved (sequenced) rate, tables of durable runs give the rate released, which on a
//! disk is the rate made durable (review finding F5).
//!
//! The container's source mount is read-only, so the fragment is pasted into
//! BENCHMARKS.md by hand, as for M1 and M2.
//!
//! **Complexity.** Linear in the summaries.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use gateway::{GatewayReject, VerifierKind};
use pipeline::gate::Stage;

use super::config::recorded_before;
use super::results::{stage_key, tag_key};
use super::session::{LOAD_SWEEP, REPETITIONS, median, read_group};
use super::summary::Summary;
use super::units::{count, dollars, latency, percent_ppm, ppm, short_duration, short_rate, signed_count};

/// The stages of 15.4 in report order, with their report names.
fn stages(summary: &Summary) -> Vec<(&'static str, &'static str)> {
    let signed = summary.get("run.mode") == Some("signed");
    let discarded = summary.get("run.journal") == Some("discard");
    let ack = if signed { "signed order → durable ack" } else { "pre-verified command → durable ack" };
    Stage::ALL
        .iter()
        .map(|&stage| (stage_key(stage), stage))
        .filter(|(_, stage)| signed || !matches!(stage, Stage::IngressWait | Stage::Verification))
        .filter(|(_, stage)| !discarded || !matches!(stage, Stage::DurabilityWait | Stage::ToDurableAck))
        .map(|(key, stage)| {
            let name = match stage {
                Stage::SenderLag => "sender lag",
                Stage::IngressWait => "ingress wait",
                Stage::Verification => "signature verification",
                Stage::SequencerWait => "sequencer wait",
                Stage::CorePath => "**core path**",
                Stage::CoreService => "core service",
                Stage::ToCoreResult => "command → core result",
                Stage::DurabilityWait => "durability wait",
                Stage::ToDurableAck => ack,
            };
            (key, name)
        })
        .collect()
}

/// A latency cell from a summary key.
fn ns_cell(s: &Summary, key: &str) -> String {
    latency(s.ns(key).flatten())
}

/// One histogram as a table row: name, count, p50, p99, p99.9, max.
fn histogram_row(out: &mut String, s: &Summary, name: &str, prefix: &str) {
    let _ = writeln!(
        out,
        "| {name} | {} | {} | {} | {} | {} |",
        count(s.u64(&format!("{prefix}.count")).unwrap_or(0)),
        ns_cell(s, &format!("{prefix}.p50")),
        ns_cell(s, &format!("{prefix}.p99")),
        ns_cell(s, &format!("{prefix}.p999")),
        ns_cell(s, &format!("{prefix}.max")),
    );
}

const HISTOGRAM_HEADER: &str = "| Stage | Count | p50 | p99 | p99.9 | Max |\n|---|---|---|---|---|---|\n";

/// `-` for an absent value.
fn get<'a>(s: &'a Summary, key: &str) -> &'a str {
    s.get(key).unwrap_or("-")
}

/// A run setting (`run.*`), or, in a summary written before its key existed, what every run
/// then had (`config::recorded_before`); `-` if neither.
fn setting<'a>(s: &'a Summary, key: &str) -> &'a str {
    s.get(key).or_else(|| recorded_before(key, s)).unwrap_or("-")
}

/// A run's signing scheme (module docs, "The signing scheme"): `perp`, `eip712`, or `none`
/// for a pre-verified run.
fn auth(s: &Summary) -> &str {
    setting(s, "run.auth")
}

/// The flow a run (`prefix` `run`) or a search (`search`) sent, with its stress switches,
/// as the report names it (module docs, "The flow"): `the M3 flow (D-027)`, `the
/// Polymarket-shaped flow (D-034) + 3 market makers + bursts (median hour) + shocks
/// (stress)`. A summary from before the keys existed was the M3 flow's, with no switch.
fn flow_label(s: &Summary, prefix: &str) -> String {
    let value = |what: &str| -> String {
        let key = format!("{prefix}.{what}");
        s.get(&key).or_else(|| recorded_before(&format!("run.{what}"), s)).unwrap_or("-").to_string()
    };
    let mut label = match value("flow").as_str() {
        "polymarket" => "the Polymarket-shaped flow (D-034)".to_string(),
        _ => "the M3 flow (D-027)".to_string(),
    };
    let makers = value("makers");
    if makers != "default" && makers != "-" {
        label.push_str(&format!(" + {makers} market makers"));
    }
    let bursts = value("bursts");
    if bursts != "none" && bursts != "-" {
        label.push_str(&format!(" + bursts ({bursts} hour)"));
    }
    let shock = value("shock");
    if shock != "none" && shock != "-" {
        label.push_str(&format!(" + shocks ({shock})"));
    }
    label
}

/// A run's arrivals, as the report names them: `Poisson`, `Uniform`, or with `--bursts`
/// Poisson with the rate of the median (busiest) recorded hour's seconds (D-034).
fn arrivals_label(s: &Summary) -> String {
    match setting(s, "run.bursts") {
        "none" | "-" => format!("{} arrivals", get(s, "run.arrivals")),
        bursts => format!("bursty Poisson arrivals, the {bursts} recorded hour's"),
    }
}

/// A scheme as the report names it.
fn auth_label(auth: &str) -> String {
    match auth {
        "eip712" => "eip712 (Polymarket Perps format)".to_string(),
        other => other.to_string(),
    }
}

/// One run, as a fragment (module docs).
pub fn run_report(s: &Summary) -> String {
    let mut out = String::new();
    let name = get(s, "run.name");
    let _ = writeln!(out, "### Run `{name}`\n");
    write_setup(&mut out, s);
    write_verdict(&mut out, s);
    write_rates(&mut out, s);
    write_stages(&mut out, s);
    write_journal(&mut out, s);
    write_breakdown(&mut out, s);
    write_flow_shape(&mut out, s);
    write_health(&mut out, s);
    write_replay_and_audit(&mut out, s);
    out
}

fn write_setup(out: &mut String, s: &Summary) {
    let signed = get(s, "run.mode") == "signed";
    let journal = if get(s, "run.journal") == "discard" {
        "**journal discarded** (11.6)".to_string()
    } else {
        format!(
            "journal on disk, T = {}, B = {}",
            short_duration(s.u64("run.commit_interval_ns").unwrap_or(0)),
            get(s, "run.max_batch")
        )
    };
    let _ = writeln!(
        out,
        "- {} mode, offered {}/s ({}), warm-up {}, window {}; {} {}; {}; stamps {}; verify on core: {}; auth: {}; verifier: {}.",
        get(s, "run.mode"),
        short_rate(s.u64("run.rate").unwrap_or(0)),
        arrivals_label(s),
        short_duration(s.u64("run.warmup_ns").unwrap_or(0)),
        short_duration(s.u64("run.window_ns").unwrap_or(0)),
        get(s, "run.lanes"),
        if signed { "gateways" } else { "lanes" },
        journal,
        get(s, "run.stamps"),
        get(s, "run.verify_on_core"),
        auth_label(auth(s)),
        get(s, "run.verifier"),
    );
    let _ = writeln!(
        out,
        "- Flow: {}, seed {}, {} markets; first seq {}. Commit `{}`, {} build. Clock {} at {:.1} ns a read; core CPU at {} MHz. File system: {}.",
        flow_label(s, "run"),
        get(s, "run.flow_seed"),
        get(s, "run.flow_markets"),
        get(s, "run.first_seq"),
        get(s, "machine.commit"),
        get(s, "machine.profile"),
        get(s, "machine.clock_source"),
        s.u64("machine.clock_read_ps").unwrap_or(0) as f64 / 1_000.0,
        get(s, "machine.core_mhz"),
        get(s, "machine.fs"),
    );
    let _ = writeln!(out, "- CPU layout: {}.\n", get(s, "machine.layout"));
}

fn write_verdict(out: &mut String, s: &Summary) {
    if s.flag("check.valid") == Some(true) {
        let _ = writeln!(out, "**Verdict: valid.** Flags: {}.\n", get(s, "check.flags"));
    } else {
        let _ = writeln!(
            out,
            "**Verdict: INVALID** ({}); it doesn't count. Flags: {}.\n",
            get(s, "check.invalid"),
            get(s, "check.flags")
        );
    }
}

fn write_rates(out: &mut String, s: &Summary) {
    if get(s, "run.stamps") == "off" {
        let _ = writeln!(
            out,
            "**Stamps off** (15.1): the trailers carry no `t_sched`, so the gate can't tell which commands were in the window, and no latency is recorded. Released per second of wall time in the window: **{}/s**.\n",
            count(s.u64("window.released_per_second").unwrap_or(0)),
        );
        return;
    }
    let offered = s.u64("window.offered").unwrap_or(0);
    let _ = writeln!(
        out,
        "**Rates in the window.** Offered {} client commands; sequenced {} ({}), achieved **{}/s**; released (durable, client and operator, by wall time) {}/s; dropped at ingress {}; rejected by gateways {}; engine rejects {} of sequenced client commands. Jumps in the window: {}; IOC places offered: {}.\n",
        count(offered),
        count(s.u64("window.sequenced").unwrap_or(0)),
        percent_ppm(s.u64("window.sequenced_ppm").unwrap_or(0)),
        count(s.u64("window.achieved_rate").unwrap_or(0)),
        count(s.u64("window.released_per_second").unwrap_or(0)),
        count(s.u64("window.dropped").unwrap_or(0)),
        count(s.u64("window.gateway_rejects").unwrap_or(0)),
        percent_ppm(s.u64("check.reject_share_ppm").unwrap_or(0)),
        get(s, "window.jumps"),
        count(s.u64("window.ioc_places_offered").unwrap_or(0)),
    );
}

fn write_stages(out: &mut String, s: &Summary) {
    let lost = s.u64("window.not_served").unwrap_or(0) > 0;
    let _ = writeln!(
        out,
        "**Latency, client commands scheduled in the window** (end to end over offered commands{}):\n",
        if lost { "; never-served commands count as ∞" } else { "" }
    );
    out.push_str(HISTOGRAM_HEADER);
    for (key, name) in stages(s) {
        histogram_row(out, s, name, &format!("stage.client.{key}"));
    }
    if lost {
        for (key, name) in [
            ("to_core_result", "command → core result, of completed"),
            ("to_durable_ack", "→ durable ack, of completed"),
        ] {
            histogram_row(out, s, name, &format!("stage.client.{key}.completed"));
        }
    }
    histogram_row(out, s, "sender lag (the sender's own, 14.10)", "sender.lag");
    let _ = writeln!(out, "\n**Operator commands in the window:**\n");
    out.push_str(HISTOGRAM_HEADER);
    for (key, name) in
        stages(s).into_iter().filter(|(key, _)| !matches!(*key, "ingress_wait" | "verification"))
    {
        histogram_row(out, s, name, &format!("stage.operator.{key}"));
    }
    histogram_row(out, s, "`SetMark` core path", "stage.set_mark_core_path");
    histogram_row(out, s, "`SetMark` core service", "stage.set_mark_core_service");
    if get(s, "run.mode") == "signed" {
        // What each gateway was offered and how busy it was: an account verifies on one
        // gateway, so a busy account's gateway can be the limit (D-034, `--makers K`).
        let _ = writeln!(
            out,
            "\n**Per gateway** (what it was offered in the window, how busy it was, the queue in front of it, and its service; 14.9, 17):\n"
        );
        out.push_str("| Gateway | Offered | Busy (CPU) | Ingress wait p99 | Ingress wait max | Verification p50 | Verification p99 |\n|---|---|---|---|---|---|---|\n");
        let lanes = s.u64("run.lanes").unwrap_or(0);
        for g in 0..lanes {
            let key = |what: &str| format!("stage.gateway.{g}.{what}");
            let _ = writeln!(
                out,
                "| {g} | {} | {} | {} | {} | {} | {} |",
                s.u64(&format!("flow.lane.{g}.messages")).map_or("-".to_string(), count),
                s.u64(&format!("health.gateway_{g}.busy_ppm")).map_or("-".to_string(), percent_ppm),
                ns_cell(s, &key("ingress_wait.p99")),
                ns_cell(s, &key("ingress_wait.max")),
                ns_cell(s, &key("verification.p50")),
                ns_cell(s, &key("verification.p99")),
            );
        }
    }
    out.push('\n');
}

/// The flow's shape in the window (D-034; `results.rs`, the `flow.*` keys): the busiest
/// account against its gateway's load, what each lane was offered (pre-verified; a signed
/// run's per-gateway table has it), the takers and their fills, the marks' longest core
/// service and the largest command, and how concentrated the traffic was across markets. A
/// summary written before these keys shows nothing here.
fn write_flow_shape(out: &mut String, s: &Summary) {
    let Some(clients) = s.u64("flow.window_clients") else { return };
    let signed = get(s, "run.mode") == "signed";
    let lane_name = if signed { "gateway" } else { "lane" };
    out.push_str("**The flow's shape in the window** (D-034; counted from the plan):\n\n");
    if let Some(account) = s.get("flow.busiest_account") {
        let lane = get(s, "flow.busiest_account_lane");
        let load = s
            .u64(&format!("health.gateway_{lane}.busy_ppm"))
            .map_or(String::new(), |busy| format!(", which was {} busy", percent_ppm(busy)));
        let _ = writeln!(
            out,
            "- **Busiest account:** {account} offered {} of the {} client messages ({}), {}/s, all through {lane_name} {lane}{load}.",
            count(s.u64("flow.busiest_account_messages").unwrap_or(0)),
            count(clients),
            percent_ppm(s.u64("flow.busiest_account_ppm").unwrap_or(0)),
            count(s.u64("flow.busiest_account_per_s").unwrap_or(0)),
        );
    }
    if !signed {
        let lanes: Vec<String> = (0..s.u64("run.lanes").unwrap_or(0))
            .map(|g| format!("{g}: {}", count(s.u64(&format!("flow.lane.{g}.messages")).unwrap_or(0))))
            .collect();
        let _ = writeln!(out, "- **Offered per lane:** {}.", lanes.join("; "));
    }
    let _ = writeln!(
        out,
        "- **Takers:** {} taker IOCs ({}/s); {} fills from all {} IOCs (the takers' and the high-leverage cohorts'): {:.2} fills per IOC.",
        count(s.u64("flow.taker_iocs").unwrap_or(0)),
        count(s.u64("flow.taker_iocs_per_s").unwrap_or(0)),
        count(s.u64("breakdown.window.fills").unwrap_or(0)),
        count(s.u64("window.ioc_places_offered").unwrap_or(0)),
        s.u64("flow.fills_per_ioc_milli").unwrap_or(0) as f64 / 1_000.0,
    );
    let _ = writeln!(
        out,
        "- **Marks and the largest command:** the longest `SetMark` core service {} (core path {}); the most events of one command {} ({} over the whole run).",
        ns_cell(s, "stage.set_mark_core_service.max"),
        ns_cell(s, "stage.set_mark_core_path.max"),
        get(s, "breakdown.window.max_events_per_command"),
        get(s, "core.max_events_per_command"),
    );
    let share = |key: &str| percent_ppm(s.u64(key).unwrap_or(0));
    let _ = writeln!(
        out,
        "- **Across markets:** {} of the {} markets had client messages; the busiest, {}, had {} of them, the top 10 {}, the median market {}. Taker IOCs: {} had {}, the top 10 {}.\n",
        get(s, "flow.markets_active"),
        get(s, "flow.markets"),
        get(s, "flow.market_top"),
        share("flow.market_top_ppm"),
        share("flow.market_top10_ppm"),
        share("flow.market_median_ppm"),
        get(s, "flow.taker_market_top"),
        share("flow.taker_market_top_ppm"),
        share("flow.taker_market_top10_ppm"),
    );
}

fn write_journal(out: &mut String, s: &Summary) {
    let _ = writeln!(
        out,
        "**Journal.** {} flushes ({}/s in the window), {} records, {} bytes; records per batch p50 {} / p99 {} / max {}; flush p99 {}; **fdatasync p50 {} / p99 {} / p99.9 {}** (histograms over the whole run); segments the writer had to create: {}.{}\n",
        count(s.u64("journal.flushes").unwrap_or(0)),
        count(s.u64("journal.flushes_per_s").unwrap_or(0)),
        count(s.u64("journal.records").unwrap_or(0)),
        count(s.u64("journal.bytes").unwrap_or(0)),
        s.ns("journal.batch_records.p50").flatten().unwrap_or(0),
        s.ns("journal.batch_records.p99").flatten().unwrap_or(0),
        s.ns("journal.batch_records.max").flatten().unwrap_or(0),
        ns_cell(s, "journal.flush.p99"),
        ns_cell(s, "journal.fdatasync.p50"),
        ns_cell(s, "journal.fdatasync.p99"),
        ns_cell(s, "journal.fdatasync.p999"),
        get(s, "journal.segments_created"),
        if get(s, "run.journal") == "disk" {
            format!(" Durable as reported by {}; not power-loss tested.", get(s, "machine.fs"))
        } else {
            " Journal discarded: nothing released was on disk.".to_string()
        },
    );
}

fn write_breakdown(out: &mut String, s: &Summary) {
    let b = "breakdown.window";
    let _ = writeln!(out, "**Commands in the window, by type and outcome** (15.9):\n");
    out.push_str("| Command | Sequenced | Accepted | Rejected, by reason |\n|---|---|---|---|\n");
    for tag in 1..=9 {
        let name = tag_key(tag);
        let Some(commands) = s.u64(&format!("{b}.commands.{name}")) else { continue };
        let prefix = format!("{b}.rejected.{name}.");
        let reasons: Vec<String> =
            s.with_prefix(&prefix).map(|(k, v)| format!("{} {}", v, &k[prefix.len()..])).collect();
        let _ = writeln!(
            out,
            "| {name} | {} | {} | {} |",
            count(commands),
            count(s.u64(&format!("{b}.accepted.{name}")).unwrap_or(0)),
            if reasons.is_empty() { "-".to_string() } else { reasons.join(", ") }
        );
    }
    let client_commands = s.u64(&format!("{b}.client_commands")).unwrap_or(0);
    let client_rejects = s.u64(&format!("{b}.client_rejects")).unwrap_or(0);
    let share = ppm(client_rejects, client_commands);
    let flag = if share > 50_000 { " **(above 5%, flagged)**" } else { "" };
    let _ = writeln!(out, "\nEngine reject share of client commands: {}{flag}.\n", percent_ppm(share));

    let gateway: Vec<String> = GatewayReject::ALL
        .iter()
        .filter_map(|r| s.u64(&format!("gateway.window_rejects.{r}")).map(|n| format!("{n} {r}")))
        .collect();
    let busy: Vec<String> = ["place", "cancel", "modify"]
        .iter()
        .filter_map(|tag| s.u64(&format!("gateway.busy.{tag}")).map(|n| format!("{n} {tag}")))
        .collect();
    let _ = writeln!(
        out,
        "- **Before the core:** dropped at ingress {} (whole run: {}); gateway rejects in the window: {}; `Busy` by command over the run: {}.",
        count(s.u64("window.dropped").unwrap_or(0)),
        count(s.u64("counts.dropped").unwrap_or(0)),
        if gateway.is_empty() { "none".to_string() } else { gateway.join(", ") },
        if busy.is_empty() { "none".to_string() } else { busy.join(", ") },
    );
    let prefix = format!("{b}.cancels.");
    let cancels: Vec<String> =
        s.with_prefix(&prefix).map(|(k, v)| format!("{v} {}", &k[prefix.len()..])).collect();
    let _ = writeln!(
        out,
        "- **Results:** {} fills, {} lots, notional {}; cancels by reason: {}; {} modifies.",
        count(s.u64(&format!("{b}.fills")).unwrap_or(0)),
        signed_count(s.i128(&format!("{b}.fill_lots")).unwrap_or(0)),
        dollars(s.i128(&format!("{b}.fill_notional")).unwrap_or(0)),
        if cancels.is_empty() { "none".to_string() } else { cancels.join(", ") },
        count(s.u64(&format!("{b}.modifies")).unwrap_or(0)),
    );
    let _ = writeln!(
        out,
        "- **Flow content:** {} jumps; {} marks; {} orders swept by the price band; **{} liquidations**, {} insurance absorbs, {} shortfall reports.",
        get(s, "window.jumps"),
        count(s.u64(&format!("{b}.marks")).unwrap_or(0)),
        s.u64(&format!("{b}.cancels.PriceBand")).unwrap_or(0),
        get(s, &format!("{b}.liquidations")),
        get(s, &format!("{b}.insurance_absorbs")),
        get(s, &format!("{b}.shortfall_reports")),
    );
    let money = |key: &str| s.i128(key).map_or("-".to_string(), dollars);
    let _ = writeln!(
        out,
        "- **The fund:** starting capital {}, lowest equity {}, **largest drawdown {}**, final equity {}, **peak shortfall {}**; the gate's final equity {} the snapshot's.",
        money("fund.start"),
        money("fund.lowest"),
        money("fund.max_drawdown"),
        money("fund.final"),
        dollars(i128::from(s.u64("fund.peak_shortfall").unwrap_or(0))),
        if s.flag("fund.consistent") == Some(true) { "equals" } else { "**differs from**" },
    );
    let _ = writeln!(
        out,
        "- **Pipeline:** {:.2} events per command (max {}); core stalled {} on the event ring; operator backlog at most {} items for {}; {} events captured.\n",
        s.u64("core.events_per_command_milli").unwrap_or(0) as f64 / 1_000.0,
        get(s, "core.max_events_per_command"),
        latency(s.u64("core.stall_total_ns")),
        get(s, "sender.max_operator_backlog"),
        latency(s.u64("sender.longest_operator_backlog_ns")),
        count(s.u64("counts.captured").unwrap_or(0)),
    );
}

fn write_health(out: &mut String, s: &Summary) {
    let _ = writeln!(out, "**Thread health in the window** (15.4): {}.\n", get(s, "health.limit"));
    out.push_str("| Thread | Busy (CPU) | Minor faults |\n|---|---|---|\n");
    for (key, busy) in s.with_prefix("health.").filter(|(k, _)| k.ends_with(".busy_ppm")) {
        let thread = &key["health.".len()..key.len() - ".busy_ppm".len()];
        let faults = get(s, &format!("health.{thread}.minor_faults"));
        let mut busy = percent_ppm(busy.parse().unwrap_or(0));
        if let Some(sync) = s.u64(&format!("health.{thread}.fdatasync_ppm")) {
            busy.push_str(&format!(", and {} blocked in fdatasync", percent_ppm(sync)));
        }
        let _ = writeln!(out, "| {} | {busy} | {faults} |", thread.replace('_', " "));
    }
    let _ = writeln!(
        out,
        "\nCore event-ring stalls {}; the sequencer found the core ring full in {} passes and the journal ring in {}; throttled periods {}; clock inversions {}; pages the kernel migrated in the window {} (a thread that touches one mid-move takes a minor fault).\n",
        latency(s.u64("health.core_stall_ns")),
        get(s, "health.core_full_passes"),
        get(s, "health.journal_full_passes"),
        get(s, "check.throttled_periods"),
        get(s, "check.inversions"),
        get(s, "health.page_migrations"),
    );
    if let Some(verifications) = s.u64("ablation.verifications") {
        let _ = writeln!(
            out,
            "**Verification** (section 16): {} verifications on {} cores, {} per busy core-second.\n",
            count(verifications),
            get(s, "ablation.cores_used"),
            count(s.u64("ablation.verifications_per_core_second").unwrap_or(0)),
        );
    }
}

fn write_replay_and_audit(out: &mut String, s: &Summary) {
    match get(s, "replay.verdict") {
        "identical" => {
            let _ = writeln!(
                out,
                "**Replay test** (13.3): replaying the {}-record journal rebuilt identical state and an identical event stream ({} events), also with another hash seed; {} records/s.\n",
                count(s.u64("replay.records").unwrap_or(0)),
                count(s.u64("replay.events").unwrap_or(0)),
                count(s.u64("replay.records_per_s").unwrap_or(0)),
            );
        }
        "off" | "-" => {}
        verdict => {
            let _ = writeln!(out, "**Replay test** (13.3): {verdict}: {}.\n", get(s, "replay.detail"));
        }
    }
    if let Some(passed) = s.flag("audit.passed") {
        let _ = writeln!(
            out,
            "**Signature audit** (13.4): {} signed records verified, {} operator; {} failures{}.\n",
            count(s.u64("audit.signed").unwrap_or(0)),
            count(s.u64("audit.operator").unwrap_or(0)),
            get(s, "audit.failures"),
            if passed { String::new() } else { format!(" (first: {})", get(s, "audit.first_failure")) },
        );
    }
}

// ---------------------------------------------------------------------------------------
// The session.

/// True if a run's summary says it is valid (15.7).
fn is_valid(s: &Summary) -> bool {
    s.flag("check.valid") == Some(true)
}

/// Runs of one point: the summaries of `<point>-r1`, `-r2`, ... and their re-runs
/// (`-r1-a2`, ...). Only the valid ones give numbers (module docs).
struct Reps<'a> {
    runs: Vec<&'a Summary>,
}

impl<'a> Reps<'a> {
    fn valid(&self) -> impl Iterator<Item = &'a Summary> + '_ {
        self.runs.iter().copied().filter(|s| is_valid(s))
    }

    /// The first valid run, for what a table shows once per point (the limit label).
    fn first_valid(&self) -> Option<&'a Summary> {
        self.valid().next()
    }

    /// "valid/total".
    fn valid_of_total(&self) -> String {
        format!("{}/{}", self.valid().count(), self.runs.len())
    }

    /// "median [low–high]" of a latency key over the valid runs; the median alone for one
    /// run.
    fn ns(&self, key: &str) -> String {
        if self.first_valid().is_none() {
            return "no valid run".to_string();
        }
        let mut values: Vec<u64> = self.valid().filter_map(|s| s.ns(key).flatten()).collect();
        if values.is_empty() {
            return "no data".to_string();
        }
        let m = median(&mut values);
        let (low, high) = (values[0], values[values.len() - 1]);
        if values.len() == 1 || low == high {
            latency(Some(m))
        } else {
            format!("{} [{}–{}]", latency(Some(m)), latency(Some(low)), latency(Some(high)))
        }
    }

    /// The same for a count.
    fn count(&self, key: &str) -> String {
        if self.first_valid().is_none() {
            return "no valid run".to_string();
        }
        let mut values: Vec<u64> = self.valid().filter_map(|s| s.u64(key)).collect();
        if values.is_empty() {
            return "-".to_string();
        }
        let m = median(&mut values);
        let (low, high) = (values[0], values[values.len() - 1]);
        if values.len() == 1 || low == high {
            count(m)
        } else {
            format!("{} [{}–{}]", count(m), count(low), count(high))
        }
    }

    /// A share in ppm, as "median%", over the valid runs.
    fn percent(&self, key: &str) -> String {
        let mut values: Vec<u64> = self.valid().filter_map(|s| s.u64(key)).collect();
        if values.is_empty() { "-".to_string() } else { percent_ppm(median(&mut values)) }
    }
}

/// A group's runs by point: `<point>-r<k>` → point.
fn by_point(runs: &[(String, Summary)]) -> BTreeMap<String, Vec<&Summary>> {
    let mut points: BTreeMap<String, Vec<&Summary>> = BTreeMap::new();
    for (name, summary) in runs {
        let point = name.rsplit_once("-r").map_or(name.as_str(), |(point, _)| point);
        points.entry(point.to_string()).or_default().push(summary);
    }
    points
}

/// The points of a group, in rate order, for `mode` (or every mode for `None`); at one rate,
/// by commit interval (the `T` sweep reads 0, 250 µs, 500 µs, 1 ms, 2 ms), then by name.
fn points_in_rate_order<'a>(
    points: &'a BTreeMap<String, Vec<&'a Summary>>,
    mode: Option<&str>,
) -> Vec<(&'a String, Reps<'a>)> {
    let mut list: Vec<(&String, Reps)> = points
        .iter()
        .filter(|(_, runs)| mode.is_none_or(|m| runs[0].get("run.mode") == Some(m)))
        .map(|(name, runs)| (name, Reps { runs: runs.clone() }))
        .collect();
    list.sort_by_key(|(name, reps)| {
        let first = reps.runs[0];
        (
            first.u64("run.rate").unwrap_or(0),
            first.u64("run.commit_interval_ns").unwrap_or(0),
            (*name).clone(),
        )
    });
    list
}

/// The sweep's rate table and one latency table per stage (15.11 item 2).
fn write_sweep(out: &mut String, runs: &[(String, Summary)]) {
    let points = by_point(runs);
    for mode in ["preverified", "signed"] {
        let list = points_in_rate_order(&points, Some(mode));
        if list.is_empty() {
            continue;
        }
        let _ = writeln!(out, "#### {mode}\n");
        out.push_str("| Point | Valid runs | Achieved/s | Released (durable)/s | Sequenced | Dropped | Gateway rejects | Engine rejects | Limit |\n|---|---|---|---|---|---|---|---|---|\n");
        for (name, reps) in &list {
            let _ = writeln!(
                out,
                "| {name} | {} | {} | {} | {} | {} | {} | {} | {} |",
                reps.valid_of_total(),
                reps.count("window.achieved_rate"),
                reps.count("window.released_per_second"),
                reps.percent("window.sequenced_ppm"),
                reps.count("window.dropped"),
                reps.count("window.gateway_rejects"),
                reps.percent("check.reject_share_ppm"),
                reps.first_valid().map_or("-", |s| get(s, "health.limit")),
            );
        }
        for (key, stage) in stages(list[0].1.runs[0]) {
            let _ = writeln!(out, "\n{stage} (median [range] of the runs):\n");
            out.push_str("| Point | p50 | p99 | p99.9 | Max |\n|---|---|---|---|---|\n");
            for (name, reps) in &list {
                let prefix = format!("stage.client.{key}");
                let _ = writeln!(
                    out,
                    "| {name} | {} | {} | {} | {} |",
                    reps.ns(&format!("{prefix}.p50")),
                    reps.ns(&format!("{prefix}.p99")),
                    reps.ns(&format!("{prefix}.p999")),
                    reps.ns(&format!("{prefix}.max")),
                );
            }
        }
        let overloaded: Vec<_> = list
            .iter()
            .filter(|(_, reps)| reps.valid().any(|s| s.u64("window.not_served") > Some(0)))
            .collect();
        if !overloaded.is_empty() {
            let _ = writeln!(
                out,
                "\nWhere commands were lost, → durable ack of completed commands, with the drop share:\n"
            );
            out.push_str("| Point | Not served | p50 | p99 | p99.9 |\n|---|---|---|---|---|\n");
            for (name, reps) in overloaded {
                let first = reps.first_valid().expect("a valid run lost commands");
                let offered = first.u64("window.offered").unwrap_or(0);
                let lost = first.u64("window.not_served").unwrap_or(0);
                let _ = writeln!(
                    out,
                    "| {name} | {} | {} | {} | {} |",
                    percent_ppm(ppm(lost, offered)),
                    reps.ns("stage.client.to_durable_ack.completed.p50"),
                    reps.ns("stage.client.to_durable_ack.completed.p99"),
                    reps.ns("stage.client.to_durable_ack.completed.p999"),
                );
            }
        }
        out.push('\n');
    }
}

/// The durable limit from the valid pre-verified 20k/s sweep runs (15.7), if there are any.
fn durable_limit(dir: &Path) -> Option<(u64, u64)> {
    let runs = read_group(&dir.join(LOAD_SWEEP));
    let mut p99s: Vec<u64> = runs
        .iter()
        .filter(|(name, s)| name.starts_with("preverified-20k-r") && is_valid(s))
        .filter_map(|(_, s)| s.ns("journal.fdatasync.p99").flatten())
        .collect();
    if p99s.is_empty() {
        return None;
    }
    let p99 = median(&mut p99s);
    Some((2_000_000 + p99, p99))
}

/// Every search of the session (15.11 item 3).
fn write_searches(out: &mut String, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut searches: Vec<Summary> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("search-"))
        .filter_map(|e| Summary::read(&e.path().join("search.txt")).ok())
        .collect();
    searches.sort_by(|a, b| a.get("search.name").cmp(&b.get("search.name")));
    if searches.is_empty() {
        out.push_str("No search has run in this session yet.\n\n");
        return;
    }
    out.push_str("| Search | Flow | Mode | Stage | Limit | Journal | Result | Its median p99 | First fail | Resolution | Limited by | Saturation |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for s in &searches {
        let rate = |key: &str| s.u64(key).map_or("-".to_string(), |r| format!("{}/s", count(r)));
        // A saturation run that kept up with what it was offered is only a lower bound.
        let saturation = match s.flag("search.saturation_reached") {
            Some(false) => format!(
                "≥ {} (not saturated at 2×; the highest any probe achieved: {})",
                rate("search.saturation"),
                rate("search.highest_achieved")
            ),
            _ => rate("search.saturation"),
        };
        // A signed search names its scheme and its verifier (5.8, 5.7); a pre-verified one
        // signs and verifies nothing. A search from before the scheme existed was perp.
        let mode = match s.get("search.verifier") {
            Some(verifier) if verifier != "none" => {
                let auth = s.get("search.auth").unwrap_or("perp");
                format!("{}, {auth}, {verifier}", get(s, "search.mode"))
            }
            _ => get(s, "search.mode").to_string(),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | **{}** | {} | {} | {} | {} | {} |",
            get(s, "search.name"),
            flow_label(s, "search"),
            mode,
            get(s, "search.stage"),
            latency(s.u64("search.limit_ns")),
            if get(s, "search.journal") == "discard" { "discarded" } else { "disk" },
            rate("search.result"),
            ns_cell(s, "search.median_p99"),
            rate("search.first_fail"),
            rate("search.resolution"),
            get(s, "search.limit"),
            saturation,
        );
    }
    for s in searches.iter().filter(|s| get(s, "search.note") != "-") {
        let _ = writeln!(out, "\n{}: {}.", get(s, "search.name"), get(s, "search.note"));
    }
    out.push('\n');
}

/// The headline of 15.7: the signed 100k/s point (or the rate the headline sweep was given)
/// passes in all 3 runs, or not.
fn write_headline(out: &mut String, dir: &Path) {
    let runs = read_group(&dir.join("headline"));
    let timing: Vec<&Summary> =
        runs.iter().filter(|(n, _)| n.contains("-timing-r")).map(|(_, s)| s).collect();
    if timing.is_empty() {
        out.push_str("The headline runs haven't run in this session yet (`e2e sweep --kind headline`).\n\n");
        return;
    }
    let Some((limit, fsync_p99)) = durable_limit(dir) else {
        out.push_str("The durable limit is unknown: the pre-verified 20k/s sweep point hasn't run.\n\n");
        return;
    };
    // One headline per flow, switches and signing scheme (15.7, 5.8; D-034): their runs
    // have different names, so a session may hold several, and each is judged on its own
    // runs, against the one durable limit. The M3 flow first, then each scheme's perp
    // first.
    let mut headlines: Vec<HeadlineKey> = timing.iter().map(|s| headline_key(s)).collect();
    headlines.sort();
    headlines.dedup();
    for key in headlines {
        let runs: Vec<&Summary> = timing.iter().copied().filter(|s| headline_key(s) == key).collect();
        write_one_headline(out, dir, &runs, limit, fsync_p99);
    }
}

/// What tells two headlines apart (module docs, "The flow"): the M3 flow or not, the flow
/// with its switches, the perp scheme or not, the scheme. Sorts the M3 flow first, and the
/// perp scheme first within a flow.
type HeadlineKey = (bool, String, bool, String);

fn headline_key(s: &Summary) -> HeadlineKey {
    (setting(s, "run.flow") != "m3", flow_label(s, "run"), auth(s) != "perp", auth(s).to_string())
}

/// The first timing run of each headline in `headline` (the group's runs, by name): the
/// run the breakdown of 15.9 shows.
fn first_timing_runs(headline: &[(String, Summary)]) -> Vec<&Summary> {
    let mut firsts: Vec<&Summary> = Vec::new();
    for (_, s) in headline.iter().filter(|(name, _)| name.contains("-timing-r1")) {
        if firsts.iter().all(|first| headline_key(first) != headline_key(s)) {
            firsts.push(s);
        }
    }
    firsts.sort_by_key(|s| headline_key(s));
    firsts
}

/// The headline of one flow and scheme from its `timing` runs (at least one), against the
/// durable `limit`.
fn write_one_headline(out: &mut String, dir: &Path, timing: &[&Summary], limit: u64, fsync_p99: u64) {
    let rate = timing[0].u64("run.rate").unwrap_or(0);
    // Every headline names its flow (D-034); the EIP-712 scheme is named too, and the perp
    // scheme is the headline of 15.7 as written.
    let flow = flow_label(timing[0], "run");
    let scheme = if auth(timing[0]) == "eip712" { ", EIP-712 (Polymarket Perps format, 5.8)" } else { "" };
    // 15.7: the answer needs 3 valid repetitions; a session that died after one can't say
    // yes (review finding F8).
    let valid: Vec<&Summary> = timing.iter().copied().filter(|s| is_valid(s)).collect();
    if valid.len() < REPETITIONS {
        let _ = writeln!(
            out,
            "**{} signed orders/s end to end on {flow}{scheme}: not decided yet.** {} of the {REPETITIONS} timing runs are valid so far; 15.7 asks for {REPETITIONS} passing repetitions.\n",
            short_rate(rate),
            valid.len()
        );
        return;
    }
    let failures: Vec<String> = valid.iter().flat_map(|s| headline_failures(s, limit)).collect();
    let reps = Reps { runs: valid.clone() };
    let _ = writeln!(
        out,
        "**{} signed orders/s end to end on {flow}{scheme}: {}.** Durable limit {} (2 × T + the median fdatasync p99, {}, of the pre-verified 20k/s runs; Q1).\n",
        short_rate(rate),
        if failures.is_empty() { "yes" } else { "no" },
        latency(Some(limit)),
        latency(Some(fsync_p99))
    );
    if failures.is_empty() {
        let _ = writeln!(
            out,
            "Signed order → durable ack p50 / p99 / p99.9: **{} / {} / {}**, with fdatasync p50 / p99 {} / {}; engine reject share {}.\n",
            reps.ns("stage.client.to_durable_ack.p50"),
            reps.ns("stage.client.to_durable_ack.p99"),
            reps.ns("stage.client.to_durable_ack.p999"),
            reps.ns("journal.fdatasync.p50"),
            reps.ns("journal.fdatasync.p99"),
            reps.percent("check.reject_share_ppm"),
        );
    } else {
        let _ = writeln!(out, "Why not: {}.\n", failures.join("; "));
        write_section_17(out, dir, timing[0], rate);
    }
}

/// What keeps one headline run from passing (15.7), if anything.
fn headline_failures(s: &Summary, limit: u64) -> Vec<String> {
    let name = get(s, "run.name");
    let mut why = Vec::new();
    if s.u64("window.sequenced_ppm").unwrap_or(0) < 999_000 {
        why.push(format!(
            "{name}: only {} sequenced",
            percent_ppm(s.u64("window.sequenced_ppm").unwrap_or(0))
        ));
    }
    if s.ns("stage.client.to_durable_ack.p99").flatten().is_none_or(|p99| p99 >= limit) {
        why.push(format!(
            "{name}: durable ack p99 {} is not under the limit",
            ns_cell(s, "stage.client.to_durable_ack.p99")
        ));
    }
    if s.ns("sender.lag.p99").flatten().is_none_or(|p99| p99 > 5_000) {
        why.push(format!("{name}: sender lag p99 {}", ns_cell(s, "sender.lag.p99")));
    }
    if s.u64("check.throttled_periods") != Some(0) || s.u64("check.inversions") != Some(0) {
        why.push(format!("{name}: throttled or clock inversions"));
    }
    if s.flag("check.clock_counts_for_headline") != Some(true) {
        why.push(format!("{name}: its clock doesn't count toward a headline (15.1)"));
    }
    why
}

/// Section 17's analysis, when the headline's `rate` (100k/s) doesn't fit, with the probe's
/// numbers for the verifier the headline ran with (5.7): its verification, or in the
/// EIP-712 scheme its signer check (the digest, the recovery and the address; 5.8), and
/// that check's scaling curve.
fn write_section_17(out: &mut String, dir: &Path, headline: &Summary, rate: u64) {
    let verifier = headline.get("run.verifier").unwrap_or("k256");
    let (t_v_name, cost_key, curve, curve_name) = match auth(headline) {
        "eip712" => (
            "`t_recover` (the EIP-712 signer check: digest, recovery, address)",
            format!("{verifier}.recover_ns"),
            format!("{verifier}_recover"),
            "Recover scaling",
        ),
        _ => ("`t_v`", format!("{verifier}.verify_ns"), verifier.to_string(), "Verify scaling"),
    };
    let probe = Summary::read(&dir.join("probe").join("summary.txt")).ok();
    let t_v = probe.as_ref().and_then(|p| p.u64(&cost_key));
    if let Some(t_v) = t_v {
        // G = ceil(R × (t_v + t_other) / u), t_other = 0.7 µs, u = 0.7 (section 17).
        let needed = (u128::from(rate) * u128::from(t_v + 700)).div_ceil(700_000_000);
        let _ = writeln!(
            out,
            "- Measured {t_v_name} ({verifier}) {}: {}/s needs about {needed} gateways by section 17's formula.",
            latency(Some(t_v)),
            short_rate(rate)
        );
    }
    if let Some(probe) = &probe {
        let _ = writeln!(out, "- {curve_name} ({verifier}): {}.", scaling_points(probe, &curve).join("; "));
    }
    let queue: Vec<String> = (0..headline.u64("run.lanes").unwrap_or(0))
        .map(|g| ns_cell(headline, &format!("stage.gateway.{g}.ingress_wait.p99")))
        .collect();
    let _ = writeln!(out, "- Per-gateway queue-wait p99: {}.", queue.join(", "));
    let libsecp = if verifier == "libsecp256k1" {
        "Bitcoin Core's libsecp256k1 is what this headline already ran with"
    } else {
        "Bitcoin Core's libsecp256k1 (5.7: `--verifier libsecp256k1` in a build with `--features c-secp256k1`)"
    };
    let _ = writeln!(
        out,
        "- The levers left (INFO.md 5a): {libsecp}; one signature per batch; a box with more cores. The signed search above gives the highest signed rate that passed.\n"
    );
}

/// The probe's scaling points of `curve` (a verifier's name, or `<verifier>_recover`):
/// "N threads: checks/s", with " with SMT" where the threads use both siblings of their
/// cores.
fn scaling_points(probe: &Summary, curve: &str) -> Vec<String> {
    (0..)
        .map_while(|i| {
            let key = |field: &str| format!("scaling.{curve}.{i}.{field}");
            let threads = probe.u64(&key("threads"))?;
            let smt = if probe.flag(&key("smt")) == Some(true) { " with SMT" } else { "" };
            Some(format!("{threads} threads{smt}: {}/s", count(probe.u64(&key("per_second"))?)))
        })
        .collect()
}

/// Every distinct `what` of the session's signed runs, in name order: their verifiers
/// (5.7), or their signing schemes (5.8).
fn of_signed_runs(dir: &Path, what: impl Fn(&Summary) -> String) -> Vec<String> {
    of_runs(dir, |s| s.get("run.mode") == Some("signed"), what)
}

/// Every distinct `what` of the session's runs that `keep` keeps, in name order.
fn of_runs(dir: &Path, keep: impl Fn(&Summary) -> bool, what: impl Fn(&Summary) -> String) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut values: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .flat_map(|group| read_group(&group.path()))
        .filter(|(_, s)| keep(s))
        .map(|(_, s)| what(&s))
        .collect();
    values.sort();
    values.dedup();
    values
}

/// A table of repeated points with chosen columns.
fn write_points_table(out: &mut String, runs: &[(String, Summary)], columns: &[(&str, &str, bool)]) {
    let points = by_point(runs);
    let list = points_in_rate_order(&points, None);
    let _ = write!(out, "| Point | Valid runs |");
    for (title, _, _) in columns {
        let _ = write!(out, " {title} |");
    }
    let _ = write!(out, "\n|---|---|");
    for _ in columns {
        out.push_str("---|");
    }
    out.push('\n');
    for (name, reps) in &list {
        let _ = write!(out, "| {name} | {} |", reps.valid_of_total());
        for (_, key, is_latency) in columns {
            let cell = if *is_latency { reps.ns(key) } else { reps.count(key) };
            let _ = write!(out, " {cell} |");
        }
        out.push('\n');
    }
    out.push('\n');
}

/// Every invalid run of the session, with its reason (15.5: none are dropped).
fn write_invalid_runs(out: &mut String, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut invalid = Vec::new();
    for group in entries.filter_map(Result::ok).filter(|e| e.path().is_dir()) {
        for (name, s) in read_group(&group.path()) {
            if s.flag("check.valid") != Some(true) {
                invalid.push(format!(
                    "- `{}/{name}`: {}",
                    group.file_name().to_string_lossy(),
                    get(&s, "check.invalid")
                ));
            }
        }
    }
    invalid.sort();
    if invalid.is_empty() {
        out.push_str("No run was invalid.\n");
    } else {
        out.push_str("Invalid runs (reported, not used):\n\n");
        out.push_str(&invalid.join("\n"));
        out.push('\n');
    }
}

/// The machine and its probes (15.11 item 1).
fn write_machine(out: &mut String, dir: &Path) {
    let Ok(p) = Summary::read(&dir.join("probe").join("summary.txt")) else {
        out.push_str("The machine hasn't been probed in this session (`e2e probe`).\n\n");
        return;
    };
    let memory = p.u64("machine.memory_bytes").unwrap_or(0) >> 30;
    let _ = writeln!(
        out,
        "| CPU | Logical CPUs (allowed) | Physical cores | Packages | RAM | Kernel | CPU quota |\n|---|---|---|---|---|---|---|\n| {} | {} ({}) | {} | {} | {memory} GB | {} | {} |\n",
        get(&p, "machine.cpu_model"),
        get(&p, "machine.logical_cpus"),
        get(&p, "machine.allowed_cpus"),
        get(&p, "machine.physical_cores"),
        get(&p, "machine.packages"),
        get(&p, "machine.kernel"),
        get(&p, "machine.quota"),
    );
    let _ = writeln!(
        out,
        "- **Clock:** {}, {:.1} ns a read ({} toward a headline, 15.1).",
        get(&p, "clock.source"),
        p.u64("clock.read_ps").unwrap_or(0) as f64 / 1_000.0,
        if p.flag("clock.counts_for_headline") == Some(true) { "counts" } else { "does NOT count" }
    );
    let _ = writeln!(
        out,
        "- **File system of the run directory** ({}): {} on {} ({}); overlay upper dir {}; device write cache {}; verdict {}.",
        get(&p, "fs.dir"),
        get(&p, "fs.type"),
        get(&p, "fs.mount_point"),
        get(&p, "fs.options"),
        get(&p, "fs.upperdir"),
        get(&p, "fs.write_cache"),
        get(&p, "fs.verdict"),
    );
    // Every verifier the probing binary had (5.7): `k256`, and libsecp256k1 in a build
    // with `--features c-secp256k1`.
    let verifiers: Vec<&str> = VerifierKind::ALL
        .into_iter()
        .map(VerifierKind::name)
        .filter(|v| p.get(&format!("{v}.verify_ns")).is_some())
        .collect();
    let verify: Vec<String> =
        verifiers.iter().map(|v| format!("{v} {}", latency(p.u64(&format!("{v}.verify_ns"))))).collect();
    let _ = writeln!(
        out,
        "- **One thread:** verify: {}; k256 sign {}; SHA-256 of 72 bytes {}.",
        verify.join(", "),
        latency(p.u64("k256.sign_ns")),
        latency(p.u64("k256.sha256_72_ns")),
    );
    // The EIP-712 scheme's costs (5.8), if the probing binary measured them.
    let recover: Vec<String> = verifiers
        .iter()
        .filter_map(|v| p.u64(&format!("{v}.recover_ns")).map(|ns| format!("{v} {}", latency(Some(ns)))))
        .collect();
    if !recover.is_empty() {
        let _ = writeln!(
            out,
            "- **One thread, EIP-712 (5.8):** signer check (digest, recovery, address): {}; of which the digest of a place {}; keccak-256 of 64 bytes {}.",
            recover.join(", "),
            latency(p.u64("eip712.digest_ns")),
            latency(p.u64("keccak.64_ns")),
        );
    }
    for verifier in &verifiers {
        let _ =
            writeln!(out, "- **Verify scaling ({verifier}):** {}.", scaling_points(&p, verifier).join("; "));
    }
    for verifier in &verifiers {
        let points = scaling_points(&p, &format!("{verifier}_recover"));
        if !points.is_empty() {
            let _ = writeln!(out, "- **Recover scaling ({verifier}, EIP-712):** {}.", points.join("; "));
        }
    }
    let _ = writeln!(
        out,
        "- **Jitter:** physical cores quietest first (by CPU): {}.",
        get(&p, "jitter.quietest_first")
    );
    match p.get("fsync.refused") {
        Some(why) => {
            let _ = writeln!(out, "- **fdatasync:** refused: {why}.");
        }
        None => {
            let _ = writeln!(
                out,
                "- **fdatasync** (15.2 KB every 1 ms): p50 {} / p99 {} / p99.9 {} / max {}; {}.",
                ns_cell(&p, "fsync.fdatasync.p50"),
                ns_cell(&p, "fsync.fdatasync.p99"),
                ns_cell(&p, "fsync.fdatasync.p999"),
                ns_cell(&p, "fsync.fdatasync.max"),
                get(&p, "fsync.verdict"),
            );
        }
    }
    out.push('\n');
}

/// The session, as the M3 section of BENCHMARKS.md (module docs).
pub fn session_report(dir: &Path) -> String {
    let mut out = String::new();
    out.push_str("## M3: the pipeline end to end\n\n");
    out.push_str("**What was measured.** The whole pipeline (PIPELINE.md): an open-loop sender, gateways verifying secp256k1 signatures (signed mode) or none (pre-verified), the sequencer, the group-commit journal, the core (`Engine<Book, Fast>`) and output gating. Latency is measured from each command's scheduled send time; end-to-end percentiles are over offered commands (15.4).\n\n");
    let verifiers = of_signed_runs(dir, |s| get(s, "run.verifier").to_string());
    if !verifiers.is_empty() {
        let _ = writeln!(out, "**Signatures verified with:** {} (5.7).\n", verifiers.join(", "));
    }
    let schemes = of_signed_runs(dir, |s| auth_label(auth(s)));
    if !schemes.is_empty() {
        let _ = writeln!(out, "**Signing scheme:** {} (5.8).\n", schemes.join(", "));
    }
    // Every flow and switch the session's runs sent (D-034): each has its own runs,
    // searches and headline.
    let flows = of_runs(dir, |_| true, |s| flow_label(s, "run"));
    if !flows.is_empty() {
        let _ = writeln!(out, "**Flows:** {}.\n", flows.join("; "));
    }
    out.push_str("### The machine and its probes (15.10)\n\n");
    write_machine(&mut out, dir);

    out.push_str("### The offered-load sweep (15.6)\n\n");
    let load = read_group(&dir.join(LOAD_SWEEP));
    if load.is_empty() {
        out.push_str("Not run yet (`e2e sweep --kind load`).\n\n");
    } else {
        write_sweep(&mut out, &load);
    }

    out.push_str("### Maximum rate at a latency limit (15.7)\n\n");
    write_searches(&mut out, dir);

    out.push_str("### The headline (15.7)\n\n");
    write_headline(&mut out, dir);

    out.push_str("### The commit interval: the `T` sweep (15.8)\n\n");
    let t_sweep = read_group(&dir.join("sweep-commit-interval"));
    if t_sweep.is_empty() {
        out.push_str("Not run yet (`e2e sweep --kind commit-interval`).\n\n");
    } else {
        write_points_table(
            &mut out,
            &t_sweep,
            &[
                ("Released (durable)/s", "window.released_per_second", false),
                ("Durable ack p50", "stage.client.to_durable_ack.p50", true),
                ("Durable ack p99", "stage.client.to_durable_ack.p99", true),
                ("Flushes/s in the window", "journal.flushes_per_s", false),
                ("fdatasync p99", "journal.fdatasync.p99", true),
            ],
        );
    }

    let headline = read_group(&dir.join("headline"));
    out.push_str("### The headline run's breakdown (15.9)\n\n");
    let firsts = first_timing_runs(&headline);
    if firsts.is_empty() {
        out.push_str("Not run yet.\n\n");
    }
    // One per headline (D-034): each run's name and setup lines say its flow and scheme.
    for s in firsts {
        out.push_str(&run_report(s));
    }

    out.push_str("### Replay, audit and a second seed (13.3, 13.4, 14.1)\n\n");
    let others: Vec<&(String, Summary)> = headline.iter().filter(|(n, _)| !n.contains("-timing-")).collect();
    if others.is_empty() {
        out.push_str("Not run yet.\n\n");
    }
    for (run, s) in others {
        let mut part = String::new();
        write_replay_and_audit(&mut part, s);
        let _ = writeln!(
            out,
            "- `{run}` (flow seed {}): durable ack p50 / p99 / p99.9 {} / {} / {}; {} liquidations in the window.\n",
            get(s, "run.flow_seed"),
            ns_cell(s, "stage.client.to_durable_ack.p50"),
            ns_cell(s, "stage.client.to_durable_ack.p99"),
            ns_cell(s, "stage.client.to_durable_ack.p999"),
            get(s, "breakdown.window.liquidations"),
        );
        out.push_str(&part);
    }

    out.push_str("### Ablation: verify on core (16)\n\n");
    let verify = read_group(&dir.join("ablate-verify-on-core"));
    if verify.is_empty() {
        out.push_str("Not run yet (`e2e ablate verify-on-core`).\n\n");
    } else {
        write_points_table(
            &mut out,
            &verify,
            &[
                ("Achieved/s", "window.achieved_rate", false),
                ("Core path p50", "stage.client.core_path.p50", true),
                ("Core path p99", "stage.client.core_path.p99", true),
                ("Durable ack p99", "stage.client.to_durable_ack.p99", true),
                ("Cores verifying", "ablation.cores_used", false),
                ("Verifications per core-second", "ablation.verifications_per_core_second", false),
            ],
        );
    }
    out.push_str("### Ablation: fsync per order (16)\n\n");
    let fsync = read_group(&dir.join("ablate-fsync-per-order"));
    if fsync.is_empty() {
        out.push_str("Not run yet (`e2e ablate fsync-per-order`).\n\n");
    } else {
        write_points_table(
            &mut out,
            &fsync,
            &[
                ("Sequenced/s", "window.achieved_rate", false),
                ("Released (durable)/s", "window.released_per_second", false),
                ("Durable ack p50", "stage.client.to_durable_ack.p50", true),
                ("Durable ack p99", "stage.client.to_durable_ack.p99", true),
                ("Flushes/s in the window", "journal.flushes_per_s", false),
                ("fdatasync p99", "journal.fdatasync.p99", true),
            ],
        );
    }

    out.push_str("### The cost of measurement (15.1)\n\n");
    let stamps = read_group(&dir.join("sweep-stamps"));
    if stamps.is_empty() {
        out.push_str("Not run yet (`e2e sweep --kind stamps`).\n\n");
    } else {
        let released = ("Released/s in the window, by wall time", "window.released_per_second", false);
        write_points_table(&mut out, &stamps, &[released]);
        out.push_str(
            "The difference in throughput is what the stamps cost (the trailer stays in both). Without stamps the \
             gate can't tell which commands were scheduled in the window, so both arms are measured by the \
             commands released per second of wall time between the window's edges.\n\n",
        );
    }

    out.push_str("### Caveats\n\n");
    out.push_str("- A container: no core isolation; other work can still run on the idle SMT siblings. Frequency and CPU quota as recorded per run.\n");
    out.push_str(
        "- Durable as reported by the run directory's file system on this machine; not power-loss tested.\n",
    );
    out.push_str("- The M3 flow's parameters are assumptions (D-027); repetitions share the seed, so they show timing noise, not flow noise.\n");
    if flows.iter().any(|flow| flow.contains("Polymarket-shaped")) {
        out.push_str(
            "- The Polymarket-shaped flow (D-034) is calibrated from about 22 hours of Polymarket Perps' public data: its shape per message is Polymarket's, its volume about 100 times Polymarket's, and flow time runs at real time at an offered 100k/s only (faster at higher rates). The account structure is unknown, so maker concentration (`--makers`) is a switch, not a calibrated fact; the recorded shock rate is one every 347 s, the switch's one every 10 s.\n",
        );
    }
    write_invalid_runs(&mut out, dir);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_summary(name: &str, rate: u64, p99: u64, valid: bool) -> Summary {
        let mut s = Summary::new();
        s.put("run.name", name);
        s.put("run.mode", "preverified");
        s.put("run.journal", "disk");
        s.put("run.rate", rate);
        s.put("run.lanes", 2);
        s.put("check.valid", valid);
        s.put("check.invalid", if valid { "-" } else { "CFS throttling: 3 periods, 90 µs" });
        s.put("window.achieved_rate", rate);
        s.put("window.sequenced_ppm", 1_000_000);
        s.put("window.not_served", 0);
        for stage in Stage::ALL {
            s.put_ns(format!("stage.client.{}.p99", stage_key(stage)), Some(p99));
            s.put(format!("stage.client.{}.count", stage_key(stage)), 10);
        }
        s.put_ns("journal.fdatasync.p99", Some(400_000));
        s.put("breakdown.window.commands.place", 7);
        s.put("breakdown.window.accepted.place", 6);
        s.put("breakdown.window.rejected.place.UnknownOrder", 1);
        s.put("health.core.busy_ppm", 500_000);
        s.put("health.core.minor_faults", 0);
        s.put("health.limit", "core-limited");
        s
    }

    #[test]
    fn a_run_report_renders_its_tables() {
        let report = run_report(&run_summary("preverified-20k", 20_000, 812, true));
        assert!(report.contains("### Run `preverified-20k`"));
        assert!(report.contains("**Verdict: valid.**"));
        assert!(report.contains("| **core path** |"), "{report}");
        assert!(!report.contains("signature verification"), "no gateways in pre-verified mode");
        assert!(report.contains("| place | 7 | 6 | 1 UnknownOrder |"), "{report}");
        assert!(report.contains("| core | 50.0% | 0 |"), "{report}");
    }

    #[test]
    fn repetitions_show_the_median_and_the_range() {
        let runs = vec![
            ("preverified-20k-r1".to_string(), run_summary("preverified-20k", 20_000, 900, true)),
            ("preverified-20k-r2".to_string(), run_summary("preverified-20k", 20_000, 800, true)),
            ("preverified-20k-r3".to_string(), run_summary("preverified-20k", 20_000, 1_000, false)),
        ];
        let mut out = String::new();
        write_sweep(&mut out, &runs);
        assert!(out.contains("| preverified-20k | 2/3 | 20,000 |"), "{out}");
        assert!(
            out.contains("| preverified-20k | no data | 800 ns [800 ns–900 ns] | no data | no data |"),
            "the invalid run's 1.00 µs is left out: {out}"
        );
    }

    #[test]
    fn a_run_names_its_signing_scheme_and_an_older_summary_reads_as_perp() {
        let mut signed = run_summary("signed-5k-eip712", 5_000, 812, true);
        signed.put("run.mode", "signed");
        signed.put("run.verifier", "k256");
        let older = run_report(&signed);
        assert!(older.contains("; auth: perp; verifier: k256."), "no `run.auth`: a perp run: {older}");
        signed.put("run.auth", "eip712");
        let report = run_report(&signed);
        assert!(report.contains("; auth: eip712 (Polymarket Perps format); verifier: k256."), "{report}");
        let pre_verified = run_report(&run_summary("preverified-20k", 20_000, 812, true));
        assert!(pre_verified.contains("; auth: none; verifier: -."), "{pre_verified}");
    }

    #[test]
    fn an_eip712_headline_names_its_scheme_and_counts_gateways_from_the_signer_check() {
        let dir = std::env::temp_dir().join(format!("bench-report-eip712-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |group: &str, run: &str, summary: &Summary| {
            let run_dir = dir.join(group).join(run);
            std::fs::create_dir_all(&run_dir).expect("created");
            summary.write(&run_dir.join("summary.txt")).expect("written");
        };
        for r in 1..=3 {
            write(
                LOAD_SWEEP,
                &format!("preverified-20k-r{r}"),
                &run_summary("preverified-20k", 20_000, 800, true),
            );
            // Valid, but with a durable ack p99 of 50 ms: the headline fails, section 17 explains.
            let mut timing = run_summary("signed-100k-eip712", 100_000, 50_000_000, true);
            timing.put("run.mode", "signed");
            timing.put("run.auth", "eip712");
            timing.put("run.verifier", "k256");
            write("headline", &format!("signed-100k-eip712-timing-r{r}"), &timing);
        }
        let mut probe = Summary::new();
        probe.put("k256.verify_ns", 80_000);
        probe.put("k256.recover_ns", 90_000);
        probe.put("eip712.digest_ns", 900);
        probe.put("keccak.64_ns", 300);
        probe.put("scaling.k256.0.threads", 1);
        probe.put("scaling.k256.0.smt", false);
        probe.put("scaling.k256.0.per_second", 12_500);
        probe.put("scaling.k256_recover.0.threads", 1);
        probe.put("scaling.k256_recover.0.smt", false);
        probe.put("scaling.k256_recover.0.per_second", 11_000);
        std::fs::create_dir_all(dir.join("probe")).expect("created");
        probe.write(&dir.join("probe").join("summary.txt")).expect("written");

        let report = session_report(&dir);
        std::fs::remove_dir_all(&dir).expect("removed");
        assert!(report.contains("**Signing scheme:** eip712 (Polymarket Perps format) (5.8)."), "{report}");
        assert!(
            report
                .contains("end to end on the M3 flow (D-027), EIP-712 (Polymarket Perps format, 5.8): no.**"),
            "{report}"
        );
        // G = ceil(100,000/s × (90 µs + 0.7 µs) / 0.7) = ceil(12.96) = 13.
        assert!(
            report.contains(
                "- Measured `t_recover` (the EIP-712 signer check: digest, recovery, address) (k256) 90.0 µs: \
                 100k/s needs about 13 gateways"
            ),
            "{report}"
        );
        assert!(report.contains("- Recover scaling (k256): 1 threads: 11,000/s."), "{report}");
        assert!(report.contains("signer check (digest, recovery, address): k256 90.0 µs"), "{report}");
        assert!(report.contains("- **Recover scaling (k256, EIP-712):** 1 threads: 11,000/s."), "{report}");
    }

    #[test]
    fn a_session_with_both_schemes_headline_runs_gets_one_headline_per_scheme() {
        let dir = std::env::temp_dir().join(format!("bench-report-both-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |group: &str, run: &str, summary: &Summary| {
            let run_dir = dir.join(group).join(run);
            std::fs::create_dir_all(&run_dir).expect("created");
            summary.write(&run_dir.join("summary.txt")).expect("written");
        };
        let timing = |name: &str, auth: &str| {
            let mut s = run_summary(name, 100_000, 50_000_000, true);
            s.put("run.mode", "signed");
            s.put("run.auth", auth);
            s.put("run.verifier", "k256");
            s
        };
        write(LOAD_SWEEP, "preverified-20k-r1", &run_summary("preverified-20k", 20_000, 800, true));
        // Three valid EIP-712 runs, but only two perp ones: pooled, they would make five.
        for r in 1..=3 {
            let eip712 = timing("signed-100k-eip712", "eip712");
            write("headline", &format!("signed-100k-eip712-timing-r{r}"), &eip712);
        }
        for r in 1..=2 {
            write("headline", &format!("signed-100k-timing-r{r}"), &timing("signed-100k", "perp"));
        }
        let report = session_report(&dir);
        std::fs::remove_dir_all(&dir).expect("removed");
        let perp = report
            .find("**100k signed orders/s end to end on the M3 flow (D-027): not decided yet.** 2 of the 3 timing runs are valid")
            .expect("the perp headline, on its own runs");
        let eip712 = report
            .find("**100k signed orders/s end to end on the M3 flow (D-027), EIP-712 (Polymarket Perps format, 5.8): no.**")
            .expect("the EIP-712 headline, on its own runs");
        assert!(perp < eip712, "the perp scheme first: {report}");
    }

    /// A signed run of the Polymarket flow with every switch, and the flow's shape.
    fn polymarket_summary(name: &str, rate: u64, p99: u64) -> Summary {
        let mut s = run_summary(name, rate, p99, true);
        for (key, value) in [
            ("run.mode", "signed"),
            ("run.auth", "perp"),
            ("run.verifier", "k256"),
            ("run.flow", "polymarket"),
            ("run.makers", "3"),
            ("run.bursts", "median"),
            ("run.shock", "stress"),
            ("run.arrivals", "Cox(Median)"),
            ("flow.market_top", "BTC-USD"),
            ("flow.taker_market_top", "ETH-USD"),
        ] {
            s.put(key, value);
        }
        for (key, value) in [
            ("flow.window_clients", 3_000_000),
            ("flow.busiest_account", 2),
            ("flow.busiest_account_messages", 1_005_000),
            ("flow.busiest_account_ppm", 335_000),
            ("flow.busiest_account_per_s", 33_500),
            ("flow.busiest_account_lane", 0),
            ("flow.lane.0.messages", 1_900_000),
            ("flow.lane.1.messages", 1_100_000),
            ("health.gateway_0.busy_ppm", 991_000),
            ("health.gateway_1.busy_ppm", 402_000),
            ("flow.taker_iocs", 2_085),
            ("flow.taker_iocs_per_s", 69),
            ("window.ioc_places_offered", 2_400),
            ("breakdown.window.fills", 3_240),
            ("flow.fills_per_ioc_milli", 1_350),
            ("breakdown.window.max_events_per_command", 97),
            ("core.max_events_per_command", 120),
            ("flow.markets", 88),
            ("flow.markets_active", 88),
            ("flow.market_top_ppm", 41_000),
            ("flow.market_top10_ppm", 230_000),
            ("flow.market_median_ppm", 9_000),
            ("flow.taker_market_top_ppm", 85_000),
            ("flow.taker_market_top10_ppm", 530_000),
        ] {
            s.put(key, value);
        }
        s.put_ns("stage.set_mark_core_service.max", Some(48_000));
        s.put_ns("stage.set_mark_core_path.max", Some(61_000));
        s
    }

    #[test]
    fn a_run_report_names_its_flow_and_switches_and_gives_the_flows_shape() {
        let report = run_report(&polymarket_summary(
            "signed-100k-polymarket-makers3-bursts-median-shock-stress",
            100_000,
            900,
        ));
        assert!(
            report.contains("offered 100k/s (bursty Poisson arrivals, the median recorded hour's)"),
            "{report}"
        );
        assert!(
            report.contains(
                "- Flow: the Polymarket-shaped flow (D-034) + 3 market makers + bursts (median hour) + shocks (stress), seed"
            ),
            "{report}"
        );
        assert!(
            report.contains(
                "- **Busiest account:** 2 offered 1,005,000 of the 3,000,000 client messages (33.5%), 33,500/s, all through gateway 0, which was 99.1% busy."
            ),
            "{report}"
        );
        assert!(report.contains("| 0 | 1,900,000 | 99.1% |"), "the per-gateway table: {report}");
        assert!(report.contains("2,085 taker IOCs (69/s); 3,240 fills from all 2,400 IOCs"), "{report}");
        assert!(report.contains("1.35 fills per IOC"), "{report}");
        assert!(report.contains("the longest `SetMark` core service 48.0 µs (core path 61.0 µs); the most events of one command 97 (120 over the whole run)"), "{report}");
        assert!(report.contains("88 of the 88 markets had client messages; the busiest, BTC-USD, had 4.1% of them, the top 10 23.0%, the median market 0.9%. Taker IOCs: ETH-USD had 8.5%, the top 10 53.0%."), "{report}");
        // A summary from before the flows existed: the M3 flow, and no shape paragraph.
        let older = run_report(&run_summary("preverified-20k", 20_000, 812, true));
        assert!(older.contains("- Flow: the M3 flow (D-027), seed"), "{older}");
        assert!(!older.contains("The flow's shape"), "{older}");
    }

    #[test]
    fn each_flow_gets_its_own_headline_and_breakdown_the_m3_flow_first() {
        let dir = std::env::temp_dir().join(format!("bench-report-flows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |group: &str, run: &str, summary: &Summary| {
            let run_dir = dir.join(group).join(run);
            std::fs::create_dir_all(&run_dir).expect("created");
            summary.write(&run_dir.join("summary.txt")).expect("written");
        };
        write(LOAD_SWEEP, "preverified-20k-r1", &run_summary("preverified-20k", 20_000, 800, true));
        for r in 1..=3 {
            let mut m3 = run_summary("signed-100k", 100_000, 50_000_000, true);
            m3.put("run.mode", "signed");
            write("headline", &format!("signed-100k-timing-r{r}"), &m3);
            let name = "signed-100k-polymarket-makers3-bursts-median-shock-stress";
            write("headline", &format!("{name}-timing-r{r}"), &polymarket_summary(name, 100_000, 50_000_000));
        }
        // A search of the Polymarket flow, with a switch.
        std::fs::create_dir_all(dir.join("search-signed-polymarket-makers3")).expect("created");
        let mut search = Summary::new();
        for (key, value) in [
            ("search.name", "signed-polymarket-makers3"),
            ("search.flow", "polymarket"),
            ("search.makers", "3"),
            ("search.bursts", "none"),
            ("search.shock", "none"),
            ("search.mode", "signed"),
        ] {
            search.put(key, value);
        }
        search.write(&dir.join("search-signed-polymarket-makers3").join("search.txt")).expect("written");
        let report = session_report(&dir);
        std::fs::remove_dir_all(&dir).expect("removed");
        let m3 = report
            .find("**100k signed orders/s end to end on the M3 flow (D-027): no.**")
            .expect("the M3 headline");
        let polymarket = report
            .find("**100k signed orders/s end to end on the Polymarket-shaped flow (D-034) + 3 market makers + bursts (median hour) + shocks (stress): no.**")
            .expect("the Polymarket flow's headline");
        assert!(m3 < polymarket, "the M3 flow first: {report}");
        let breakdown = &report[report.find("### The headline run's breakdown").expect("the section")..];
        let (m3, polymarket) = (
            breakdown.find("### Run `signed-100k`").expect("the M3 flow's"),
            breakdown
                .find("### Run `signed-100k-polymarket-makers3-bursts-median-shock-stress`")
                .expect("the other's"),
        );
        assert!(m3 < polymarket, "{report}");
        assert!(
            report.contains("| signed-polymarket-makers3 | the Polymarket-shaped flow (D-034) + 3 market makers | signed |"),
            "{report}"
        );
        assert!(
            report.contains("**Flows:** the M3 flow (D-027); the Polymarket-shaped flow (D-034) + 3 market makers + bursts (median hour) + shocks (stress)."),
            "{report}"
        );
        assert!(report.contains("- The Polymarket-shaped flow (D-034) is calibrated"), "{report}");
    }

    #[test]
    fn a_session_report_renders_what_exists_and_lists_invalid_runs() {
        let dir = std::env::temp_dir().join(format!("bench-report-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let run = dir.join(LOAD_SWEEP).join("preverified-20k-r1");
        std::fs::create_dir_all(&run).expect("created");
        run_summary("preverified-20k", 20_000, 812, false).write(&run.join("summary.txt")).expect("written");
        let report = session_report(&dir);
        assert!(report.contains("### The offered-load sweep (15.6)"));
        assert!(report.contains("hasn't been probed"));
        assert!(report.contains("`sweep-load/preverified-20k-r1`: CFS throttling"), "{report}");
        std::fs::remove_dir_all(&dir).expect("removed");
    }
}
