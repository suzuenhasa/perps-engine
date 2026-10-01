//! `e2e`: the end-to-end harness's command line (`docs/PIPELINE.md` 15, 16, 13.3, 13.5 and
//! 18.4; the harness itself is `bench::e2e`).
//!
//! ```text
//! e2e probe   [--dir SESSION] [--quick]                  machine probes (15.10)
//! e2e run     [--dir SESSION | --run-dir DIR] [OPTIONS]   one run; --resume restarts on its journal
//! e2e sweep   --kind load|commit-interval|stamps|headline|polymarket [--rates LIST] [--rate R] [OPTIONS]
//! e2e search  --name core-path|journal|signed [--limit DURATION] [--start RATE] [OPTIONS]
//! e2e ablate  verify-on-core|fsync-per-order [--rates LIST] [--search] [OPTIONS]
//! e2e replay  --run-dir DIR [--allow-engine-change]       replay a kept journal, compare with the run (13.3)
//! e2e recover --run-dir DIR [--allow-engine-change]       recovery only (11.8)
//! e2e report  [--dir SESSION]                             rebuild SESSION/report.md
//! ```
//!
//! Run options: `--mode signed|preverified`, `--rate 100k`, `--smoke` (18.4's smoke run),
//! `--flow m3|polymarket|smoke|polymarket-smoke`, `--makers K`, `--bursts median|busiest`,
//! `--shock real|stress`, `--seed N`, `--warmup 5s`, `--window 30s`, `--tail 200ms`,
//! `--timed-clients N`, `--gateways N`, `--journal disk|discard`, `--commit-interval 1ms`,
//! `--max-batch 4096`, `--segment-bytes 1G`, `--stamps on|off`, `--arrivals
//! poisson|uniform`, `--unpinned`, `--gateway-smt`, `--cpus role=list` (repeatable),
//! `--idle spin|yield`, `--deployment N`, `--verifier k256|libsecp256k1`, `--auth
//! perp|eip712`, `--capture`, `--audit`, `--keep-journal`, `--release-log`,
//! `--allow-engine-change`, and for development only `--allow-generator-limited` (a late
//! sender is flagged instead of making the run invalid; the report says so).
//!
//! **The flow** (DECISIONS.md D-027, D-034; `bench/src/e2e/flow.rs`). `m3`, the M3 flow of
//! section 14, by default; `polymarket`, the Polymarket-shaped flow: Polymarket Perps' 88
//! real markets, traded with the shape of their recorded traffic at about 100 times its
//! volume. `smoke` and `polymarket-smoke` are their smoke flows, for tests and quick local
//! runs (`--smoke --flow polymarket-smoke`). Three stress switches (D-034):
//! - `--makers K`: `K` market-making accounts, 1 to 20 (a market has 20 quotes a side),
//!   quote every market, so each sends about `1/K` of all traffic through its one gateway
//!   (`account mod N`): with `K` = 3 one account's gateway, not the gateway count, sets the
//!   signed ceiling. The Polymarket flow only; without it, 60 makers spread evenly over the
//!   gateways.
//! - `--shock real|stress`: every 10 s of flow time, 14 to 88 markets (14 at the median)
//!   move the same way within about 50 ms: by their recorded sizes (`real`), or by 2% to 6% with a cohort of
//!   high-leverage accounts that the move liquidates together (`stress`). The Polymarket
//!   flow only.
//! - `--bursts median|busiest`: the send schedule becomes bursty, its rate swinging second
//!   by second as Polymarket's did in its median or busiest recorded hour, around the same
//!   mean. Only when messages are sent changes, never what they say, so it works with
//!   either flow and reuses the same signed messages; it needs Poisson arrivals (not
//!   `--arrivals uniform`).
//!
//! Each flow and switch names its runs (`signed-100k-polymarket-makers3`) and its searches
//! (`search-signed-polymarket-makers3`), and is recorded in every summary (`run.flow`,
//! `run.makers`, `run.bursts`, `run.shock`), so one session may hold both flows side by side
//! and never reuses a run of other settings. The durable limit is always measured on the
//! M3 flow's pre-verified 20k/s point, so every flow is judged against the same one. The
//! report gives each flow and switch its own headline (15.7) and, per run, the facts D-034
//! asks for: the busiest account's share and rate against its gateway's load, each
//! gateway's load, the longest `SetMark` core service, the most events of one command, the
//! takers' IOCs a second and the fills per IOC, and how concentrated the traffic was across
//! markets.
//!
//! **The D-034 session** (`docs/RUNBOOK-PERPSBOX.md` 7b: on PERPSBOX, after the M3
//! session, in the same session directory and with the same options, so the report shows
//! both flows side by side; times at `--gateways 10`, without reruns):
//!
//! ```sh
//! e2e sweep --kind polymarket $O                        # ~22 min: 3 headlines, interleaved
//! e2e search --name signed --flow polymarket $O         # ~12 min
//! e2e search --name core-path --flow polymarket $O      # ~10 min, journal discarded
//! e2e search --name signed --flow polymarket --makers 3 $O   # ~8 min: the per-account ceiling
//! ```
//!
//! The sweep runs the Polymarket flow's three headlines at 100k/s (as it is, with bursts of
//! the median hour, and with the stress shock), each 3 timing runs of 65 s, a capture run
//! (the replay test and the audit, over the shocks' liquidations too) and a second seed:
//! 15 runs of about 67 s, plus signing 4 flows of 6.5M messages (bursts reuse the plain
//! flow's) at 15 to 40 s each, and 3 replay tests and audits. A search is about 14 runs of
//! 27 s plus the signing or generation of its largest probe.
//!
//! **The verifier** (PIPELINE.md 5.7): `k256` by default. `--verifier libsecp256k1` needs a
//! binary built with `--features c-secp256k1`; any other binary refuses it at once. It is
//! recorded in every run's summary (`run.verifier`), so a session never mixes the two. The
//! probe measures every verifier the binary has.
//!
//! **The signing scheme** (PIPELINE.md 5.8; DECISIONS.md D-033): `perp`, our own nonce
//! scheme, by default. `--auth eip712` signs every client message the way Polymarket Perps
//! does (EIP-712 over keccak-256 of the MessagePack-encoded command, a salt and a
//! millisecond timestamp), and the gateways recover the signer and keep a replay table. It
//! is recorded in every run's summary (`run.auth`, `none` in pre-verified runs) and names
//! its runs (`signed-100k-eip712`), so the two schemes' runs never share a name: a session
//! may hold both, and the report gives each scheme its own headline. It applies to signed
//! runs only: `e2e run --mode preverified --auth eip712` is refused, while a sweep, a
//! search or an ablation signs its signed runs in it and its pre-verified runs sign nothing
//! (the durable limit's included, so an EIP-712 session may start from a copy of another
//! session's pre-verified 20k/s runs, as the libsecp256k1 pass does). Refused with
//! `--resume`, in the verify-on-core ablation, and for a timed flow over 3 minutes: the
//! messages are signed before each run, and the gateways refuse them 5 minutes later, so
//! the harness signs them again before a run they would not last through, and refuses the
//! run if even the new ones, once signed, would not (signing took too long). The probe
//! measures what the scheme costs: `<verifier>.recover_ns`, `eip712.digest_ns`,
//! `keccak.64_ns` and a recovery scaling curve per verifier.
//!
//! **Where things go.** A session directory (`--dir`, by default `/target/runs/session` in
//! the container, where `/target` is the only writable disk, else `runs/session`) holds
//! every run and the session's `report.md`, rebuilt after every command. Commands are
//! resumable: a run whose `summary.txt` exists is not run again (15.5).
//!
//! **Output.** Progress goes to standard error; the report to standard output, except with
//! `--release-log`, where standard output carries the release log (every released event
//! slot, 64 bytes each; the kill test reads it, 18.4) and the report goes to standard
//! error too. The exit status is 0 when the command finished (a run that finished but is
//! invalid says so in its report), 2 when it couldn't.
//!
//! **The "verify on core" ablation is insecure**, so only `e2e ablate` can turn it on
//! (section 16).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use bench::e2e::ablate::{FSYNC_RATES, VERIFY_RATES, run_fsync_per_order, run_verify_on_core};
use bench::e2e::config::{Cpus, RunConfig, parse_mode};
use bench::e2e::flow::{Flow, parse_bursts, parse_shock};
use bench::e2e::probes::{FsVerdict, ProbeDurations, file_system_of, fs_verdict, probe};
use bench::e2e::report::run_report;
use bench::e2e::runner::{EVENTS_FILE, LIVE_SNAPSHOT_FILE, Quiet, RunError, run};
use bench::e2e::search::{SearchSpec, run_search};
use bench::e2e::session::Session;
use bench::e2e::summary::Summary;
use bench::e2e::sweep::{
    LOAD_RATES, commit_interval_points, headline_points, load_points, polymarket_points, run_interleaved,
    stamps_points,
};
use bench::e2e::units::{SECOND_NS, parse_bytes, parse_duration, parse_rate, parse_rates};
use bench::e2e::workload::Workloads;
use gateway::VerifierKind;
use loadgen::market_flow::PlanConfig;
use loadgen::schedule::Arrivals;
use pipeline::gate::{EventsHeader, read_events_file, write_events_file};
use pipeline::idle::IdleStrategy;
use pipeline::journal::format::{ENGINE_SEMANTICS, build_commit};
use pipeline::journal::recovery::{Recovered, recover, scan_journal};
use pipeline::journal::writer::JournalMode;
use pipeline::journal::{JournalError, files::StdFiles, format::DEFAULT_SEGMENT_BYTES};
use pipeline::records::{AuthScheme, EVENT_SLOT_WORDS, InjectionMode, Stamps};
use pipeline::replay::{first_difference, replay};

/// Options that take no value.
const FLAGS: [&str; 13] = [
    "quick",
    "resume",
    "release-log",
    "keep-journal",
    "capture",
    "audit",
    "unpinned",
    "gateway-smt",
    "allow-engine-change",
    "allow-generator-limited",
    "search",
    "smoke",
    "help",
];

/// The command line: words, and `--name value` or `--flag` options.
#[derive(Debug, Default)]
struct Args {
    words: Vec<String>,
    options: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(mut raw: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut args = Args::default();
        while let Some(arg) = raw.next() {
            let Some(name) = arg.strip_prefix("--") else {
                args.words.push(arg);
                continue;
            };
            if FLAGS.contains(&name) {
                args.options.push((name.to_string(), None));
            } else {
                let value = raw.next().ok_or_else(|| format!("--{name} needs a value"))?;
                args.options.push((name.to_string(), Some(value)));
            }
        }
        Ok(args)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.options.iter().rev().find(|(n, _)| n == name).and_then(|(_, v)| v.as_deref())
    }

    fn values(&self, name: &str) -> Vec<&str> {
        self.options.iter().filter(|(n, _)| n == name).filter_map(|(_, v)| v.as_deref()).collect()
    }

    fn flag(&self, name: &str) -> bool {
        self.options.iter().any(|(n, _)| n == name)
    }

    /// A parsed value, or `None` if absent.
    fn parsed<T>(&self, name: &str, parse: impl Fn(&str) -> Result<T, String>) -> Result<Option<T>, String> {
        self.value(name).map(|v| parse(v).map_err(|e| format!("--{name}: {e}"))).transpose()
    }
}

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => return fail(&e),
    };
    let command = args.words.first().map(String::as_str).unwrap_or("help");
    let result = match command {
        "probe" => probe_command(&args),
        "run" => run_command(&args),
        "sweep" => sweep_command(&args),
        "search" => search_command(&args),
        "ablate" => ablate_command(&args),
        "replay" => replay_command(&args),
        "recover" => recover_command(&args),
        "report" => report_command(&args),
        _ => {
            print!("{}", usage());
            return if args.flag("help") || command == "help" {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            };
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("e2e: {message}");
    ExitCode::from(2)
}

fn usage() -> String {
    let docs = include_str!("e2e.rs");
    let start = docs.find("```text\n").map_or(0, |i| i + 8);
    let end = docs[start..].find("//! ```").map_or(docs.len(), |i| start + i);
    let lines: Vec<&str> = docs[start..end].lines().map(|l| l.trim_start_matches("//! ")).collect();
    format!("usage:\n{}\n(see the module docs of bench/src/bin/e2e.rs for the options)\n", lines.join("\n"))
}

/// The session directory (module docs, "Where things go").
fn session_dir(args: &Args) -> PathBuf {
    match args.value("dir") {
        Some(dir) => PathBuf::from(dir),
        None if Path::new("/target").is_dir() => PathBuf::from("/target/runs/session"),
        None => PathBuf::from("runs/session"),
    }
}

/// A run configuration from the run options (module docs), checked.
fn run_config(args: &Args, default_mode: InjectionMode, default_rate: u64) -> Result<RunConfig, String> {
    let config = run_options(args, default_mode, default_rate)?;
    config.check()?;
    Ok(config)
}

/// The template a sweep, a search or an ablation derives its runs from, each in its own
/// mode (`RunConfig::in_mode`): the run options, in `default_mode` unless `--mode` says
/// otherwise. `--auth` is the scheme of its signed runs, and its pre-verified runs sign
/// nothing (module docs, "The signing scheme"), so the template is checked as a run in its
/// own mode and as a signed run.
fn template_config(args: &Args, default_mode: InjectionMode) -> Result<RunConfig, String> {
    let template = run_options(args, default_mode, 100_000)?;
    template.in_mode(template.mode).check()?;
    template.in_mode(InjectionMode::Signed).check()?;
    Ok(template)
}

/// A run configuration from the run options (module docs), not yet checked.
fn run_options(args: &Args, default_mode: InjectionMode, default_rate: u64) -> Result<RunConfig, String> {
    let mode = args.parsed("mode", parse_mode)?.unwrap_or(default_mode);
    let rate = args.parsed("rate", parse_rate)?.unwrap_or(default_rate);
    let mut c = if args.flag("smoke") { RunConfig::smoke(mode) } else { RunConfig::new(mode, rate) };
    if args.value("rate").is_some() {
        c.rate = rate;
    }
    // The flow first, then what changes it (module docs, "The flow").
    if let Some(flow) = args.parsed("flow", Flow::from_name)? {
        c.flow = flow;
    }
    if let Some(seed) = args.parsed("seed", |v| v.parse::<u64>().map_err(|e| e.to_string()))? {
        c.flow = c.flow.with_seed(seed);
    }
    if let Some(makers) = args.parsed("makers", |v| v.parse::<u32>().map_err(|e| e.to_string()))? {
        c.flow = c.flow.with_makers(makers)?;
    }
    if let Some(size) = args.parsed("shock", parse_shock)? {
        c.flow = c.flow.with_shock(size)?;
    }
    let durations = [("warmup", &mut c.warmup_ns), ("window", &mut c.window_ns), ("tail", &mut c.tail_ns)];
    for (name, field) in durations {
        if let Some(ns) = args.parsed(name, parse_duration)? {
            *field = ns;
        }
    }
    if let Some(n) = args.parsed("timed-clients", |v| v.parse::<usize>().map_err(|e| e.to_string()))? {
        c.fixed_timed_clients = Some(n);
    }
    if let Some(n) = args.parsed("gateways", |v| v.parse::<usize>().map_err(|e| e.to_string()))? {
        c.lanes = n;
    }
    if let Some(n) = args.parsed("deployment", |v| v.parse::<u32>().map_err(|e| e.to_string()))? {
        c.deployment = n;
    }
    if let Some(verifier) = args.parsed("verifier", parse_verifier)? {
        c.verifier = verifier;
    }
    if let Some(auth) = args.parsed("auth", AuthScheme::from_name)? {
        c.auth = auth;
    }
    match args.value("journal") {
        None | Some("disk") => {}
        Some("discard") => c.journal.mode = JournalMode::Discard,
        Some(other) => return Err(format!("--journal {other:?}: disk or discard")),
    }
    if let Some(ns) = args.parsed("commit-interval", parse_duration)? {
        c.journal.commit_interval_ns = ns;
    }
    if let Some(b) = args.parsed("max-batch", |v| v.parse::<usize>().map_err(|e| e.to_string()))? {
        c.journal.max_batch = b;
    }
    if let Some(bytes) = args.parsed("segment-bytes", parse_bytes)? {
        c.journal.segment_bytes = bytes;
    }
    match args.value("stamps") {
        None | Some("on") => {}
        Some("off") => c.stamps = Stamps::Off,
        Some(other) => return Err(format!("--stamps {other:?}: on or off")),
    }
    match args.value("arrivals") {
        None | Some("poisson") => {}
        Some("uniform") => c.arrivals = Arrivals::Uniform,
        Some(other) => return Err(format!("--arrivals {other:?}: poisson or uniform")),
    }
    // Bursts are a Poisson process whose rate swings (module docs, "The flow").
    if let Some(bursts) = args.parsed("bursts", parse_bursts)? {
        if c.arrivals == Arrivals::Uniform {
            return Err("--bursts needs Poisson arrivals, not --arrivals uniform".into());
        }
        c.arrivals = Arrivals::Cox(bursts);
    }
    match args.value("idle") {
        None => {}
        Some("spin") => c.idle = IdleStrategy::Spin,
        Some("yield") => c.idle = IdleStrategy::SpinThenYield { spins: 64 },
        Some(other) => return Err(format!("--idle {other:?}: spin or yield")),
    }
    let overrides: Vec<String> = args.values("cpus").into_iter().map(str::to_string).collect();
    if args.flag("unpinned") {
        c.cpus = Cpus::Unpinned;
    } else if args.flag("gateway-smt") || !overrides.is_empty() {
        c.cpus = Cpus::Pinned { gateway_smt: args.flag("gateway-smt"), overrides };
    }
    c.capture |= args.flag("capture");
    c.audit |= args.flag("audit");
    c.keep_journal |= args.flag("keep-journal") || args.flag("release-log");
    c.release_log = args.flag("release-log");
    c.resume = args.flag("resume");
    c.allow_engine_change = args.flag("allow-engine-change");
    c.allow_generator_limited = args.flag("allow-generator-limited");
    Ok(c)
}

/// `--verifier`: one this binary has (module docs, "The verifier").
fn parse_verifier(text: &str) -> Result<VerifierKind, String> {
    let verifier = VerifierKind::from_name(text)?;
    verifier.check_built().map_err(|e| e.to_string())?;
    Ok(verifier)
}

/// Prints `text` on standard output, or on standard error when standard output carries the
/// release log.
fn say(args: &Args, text: &str) {
    if args.flag("release-log") { eprint!("{text}") } else { print!("{text}") }
}

/// Rebuilds the session's report and says where it is.
fn rebuild_report(session: &Session) -> Result<(), String> {
    let path = session.write_report().map_err(|e| e.to_string())?;
    eprintln!("e2e: the session's report is {}", path.display());
    Ok(())
}

/// Refuses a session directory where durable numbers would mean nothing (15.10).
fn check_file_system(dir: &Path) -> Result<(), String> {
    let mount =
        file_system_of(dir).map_err(|e| format!("reading the file system of {}: {e}", dir.display()))?;
    match fs_verdict(&mount) {
        FsVerdict::Refused(why) => Err(format!("{}: {why}; choose another directory", dir.display())),
        FsVerdict::CopyOnWrite(why) => {
            eprintln!("e2e: note: {why}");
            Ok(())
        }
        FsVerdict::Ok => Ok(()),
    }
}

fn run_error(e: RunError) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------------------
// The commands.

fn probe_command(args: &Args) -> Result<(), String> {
    let session = Session::open(&session_dir(args)).map_err(|e| e.to_string())?;
    let durations = if args.flag("quick") { ProbeDurations::quick() } else { ProbeDurations::full() };
    let probe_dir = session.dir.join("probe");
    let summary = probe(&probe_dir, durations).map_err(|e| e.to_string())?;
    summary.write(&session.probe_path()).map_err(|e| e.to_string())?;
    let refused = summary.get("fsync.refused").map(str::to_string);
    rebuild_report(&session)?;
    print!("{}", summary.text());
    match refused {
        Some(why) => Err(format!("the run directory's file system is refused: {why}")),
        None => Ok(()),
    }
}

fn run_command(args: &Args) -> Result<(), String> {
    let config = run_config(args, InjectionMode::Signed, 100_000)?;
    let (session, dir) = match args.value("run-dir") {
        Some(dir) => (None, PathBuf::from(dir)),
        None => {
            let session = Session::open(&session_dir(args)).map_err(|e| e.to_string())?;
            let name = args.value("name").map_or_else(|| config.name(), str::to_string);
            let dir = session.dir.join("runs").join(name);
            (Some(session), dir)
        }
    };
    if !config.discarded() {
        check_file_system(&dir)?;
    }
    let mut workloads = Workloads::default();
    let quietest = session.as_ref().map(|s| s.quietest_first.clone()).unwrap_or_default();
    let result = run(&config, &mut workloads, &dir, &quietest, &mut Quiet).map_err(run_error)?;
    let summary = result.summary();
    summary.write(&dir.join("summary.txt")).map_err(|e| e.to_string())?;
    let report = run_report(&summary);
    std::fs::write(dir.join("report.md"), &report).map_err(|e| e.to_string())?;
    say(args, &report);
    if let Some(session) = &session {
        rebuild_report(session)?;
    }
    Ok(())
}

/// The template the session's commands build their runs from: the run options in `mode`,
/// with 15.7's search timing (5 s + 20 s) unless `--window` says otherwise.
fn search_template(args: &Args, mode: InjectionMode) -> Result<RunConfig, String> {
    let mut template = template_config(args, mode)?.in_mode(mode);
    if args.value("window").is_none() {
        template.window_ns = 20 * SECOND_NS;
    }
    Ok(template)
}

fn open_session(args: &Args) -> Result<Session, String> {
    let session = Session::open(&session_dir(args)).map_err(|e| e.to_string())?;
    check_file_system(&session.dir)?;
    Ok(session)
}

fn sweep_command(args: &Args) -> Result<(), String> {
    let mut session = open_session(args)?;
    let template = template_config(args, InjectionMode::PreVerified)?;
    let kind = args.value("kind").ok_or("--kind load|commit-interval|stamps|headline|polymarket")?;
    let points = match kind {
        "load" => {
            let rates = args.parsed("rates", parse_rates)?.unwrap_or(LOAD_RATES.to_vec());
            ("sweep-load", load_points(&template, &rates))
        }
        "commit-interval" => {
            let rate = args.parsed("rate", parse_rate)?.unwrap_or(100_000);
            ("sweep-commit-interval", commit_interval_points(&template, rate))
        }
        "stamps" => {
            let rate =
                args.parsed("rate", parse_rate)?.ok_or("--rate: twice the core-path search's result")?;
            ("sweep-stamps", stamps_points(&template, rate))
        }
        "headline" => {
            let rate = args.parsed("rate", parse_rate)?.unwrap_or(100_000);
            ("headline", headline_points(&template, rate, template.flow.seed() + 1))
        }
        // The Polymarket flow's three headlines (module docs, "The D-034 session").
        "polymarket" => {
            let rate = args.parsed("rate", parse_rate)?.unwrap_or(100_000);
            ("headline", polymarket_points(&template, rate, template.flow.seed() + 1))
        }
        other => {
            return Err(format!("--kind {other:?}: load, commit-interval, stamps, headline or polymarket"));
        }
    };
    let result = run_interleaved(&mut session, points.0, &points.1).map_err(run_error);
    rebuild_report(&session)?;
    result.map(|_| eprintln!("e2e: {} runs ran, {} were already done", session.ran, session.skipped))
}

fn search_command(args: &Args) -> Result<(), String> {
    let mut session = open_session(args)?;
    let name = args.value("name").ok_or("--name core-path|journal|signed")?;
    let sweep_template = template_config(args, InjectionMode::PreVerified)?;
    let mut spec = match name {
        "core-path" => SearchSpec::core_path(&search_template(args, InjectionMode::PreVerified)?),
        "journal" | "signed" => {
            let mode = if name == "signed" { InjectionMode::Signed } else { InjectionMode::PreVerified };
            let limit = match args.parsed("limit", parse_duration)? {
                Some(limit) => limit,
                None => session.durable_limit(&sweep_template).map_err(run_error)?.limit_ns,
            };
            SearchSpec::durable(name, &search_template(args, mode)?, limit)
        }
        other => return Err(format!("--name {other:?}: core-path, journal or signed")),
    };
    if let Some(start) = args.parsed("start", parse_rate)? {
        spec.start = start;
    }
    if name == "core-path"
        && let Some(limit) = args.parsed("limit", parse_duration)?
    {
        spec.limit_ns = limit;
    }
    let result = run_search(&mut session, &spec).map_err(run_error);
    rebuild_report(&session)?;
    let outcome = result?;
    eprintln!("e2e: search {name}: {outcome:?}");
    Ok(())
}

fn ablate_command(args: &Args) -> Result<(), String> {
    let mut session = open_session(args)?;
    let which = args.words.get(1).map(String::as_str).ok_or("ablate verify-on-core|fsync-per-order")?;
    let sweep_template = template_config(args, InjectionMode::PreVerified)?;
    let search = args.flag("search");
    let result = match which {
        "verify-on-core" => {
            let rates = args.parsed("rates", parse_rates)?.unwrap_or(VERIFY_RATES.to_vec());
            let template = template_config(args, InjectionMode::Signed)?;
            let search_template = search_template(args, InjectionMode::Signed)?;
            run_verify_on_core(&mut session, &template, &search_template, &sweep_template, &rates, search)
        }
        "fsync-per-order" => {
            let rates = args.parsed("rates", parse_rates)?.unwrap_or(FSYNC_RATES.to_vec());
            let template = template_config(args, InjectionMode::PreVerified)?;
            let search_template = search_template(args, InjectionMode::PreVerified)?;
            run_fsync_per_order(&mut session, &template, &search_template, &sweep_template, &rates, search)
        }
        other => return Err(format!("ablate {other:?}: verify-on-core or fsync-per-order")),
    };
    rebuild_report(&session)?;
    result.map_err(run_error)
}

fn report_command(args: &Args) -> Result<(), String> {
    let session = Session::open(&session_dir(args)).map_err(|e| e.to_string())?;
    rebuild_report(&session)?;
    print!("{}", bench::e2e::report::session_report(&session.dir));
    Ok(())
}

/// The journal of a run directory, and the identity its segment 0 names, with this
/// binary's `engine_semantics` (11.2: a journal written with other semantics is refused
/// unless `--allow-engine-change`).
fn kept_journal(args: &Args) -> Result<(PathBuf, pipeline::journal::format::JournalIdentity), String> {
    let run_dir = PathBuf::from(args.value("run-dir").ok_or("--run-dir DIR")?);
    let journal = run_dir.join("journal");
    let mut files = StdFiles::open_existing(&journal, DEFAULT_SEGMENT_BYTES)
        .map_err(|e| format!("{}: {e}", journal.display()))?;
    let scan = scan_journal(&mut files, |_, _, _| {}).map_err(|e: JournalError| e.to_string())?;
    let identity = scan
        .identity
        .ok_or_else(|| format!("{}: no journal (no valid segment header)", journal.display()))?;
    let header = scan.headers.first().expect("an identity comes from a header");
    let commit = String::from_utf8_lossy(&header.commit).trim_end_matches('\0').to_string();
    let this_commit = String::from_utf8_lossy(&build_commit()).trim_end_matches('\0').to_string();
    eprintln!(
        "journal: commit {commit:?} ({} build), engine_semantics {}; this binary: commit {this_commit:?} ({} build), engine_semantics {ENGINE_SEMANTICS}",
        if header.release_build { "release" } else { "debug" },
        identity.engine_semantics,
        if cfg!(debug_assertions) { "debug" } else { "release" },
    );
    let expect =
        pipeline::journal::format::JournalIdentity { engine_semantics: ENGINE_SEMANTICS, ..identity };
    Ok((journal, expect))
}

/// Recovery (11.8) of a kept journal, printed.
fn recover_kept(args: &Args) -> Result<(PathBuf, Recovered), String> {
    let (journal, expect) = kept_journal(args)?;
    let recovered =
        recover(&journal, &expect, args.flag("allow-engine-change")).map_err(|e| e.to_string())?;
    for warning in recovered.build_warnings() {
        eprintln!("recovery: warning: {warning}");
    }
    for copy in &recovered.torn_copies {
        eprintln!("recovery: the torn tail was copied to {copy}");
    }
    Ok((journal, recovered))
}

fn recover_command(args: &Args) -> Result<(), String> {
    let (_, r) = recover_kept(args)?;
    let mut s = Summary::new();
    s.put("recovered.records", r.records);
    s.put("recovered.next_seq", r.next_seq);
    s.put("recovered.last_ts", r.last_ts);
    s.put("recovered.end", r.end);
    s.put("recovered.segments", r.headers.len());
    s.put("recovered.deployment", r.identity.deployment);
    s.put("recovered.engine_semantics", r.identity.engine_semantics);
    print!("{}", s.text());
    Ok(())
}

/// `e2e replay` (13.3): replays a kept journal, writes `events-replay.bin` and
/// `snapshot.txt` next to it for diffing by hand, and compares them with what the live run
/// left there (`events.bin`, `snapshot-live.txt`): it fails if either differs.
fn replay_command(args: &Args) -> Result<(), String> {
    let (journal, recovered) = recover_kept(args)?;
    let started = std::time::Instant::now();
    let output = replay(&journal, &recovered, None, true).map_err(|e| e.to_string())?;
    let elapsed = started.elapsed().max(Duration::from_nanos(1));
    let run_dir = journal.parent().expect("the journal is inside the run directory");
    let events = output.capture.as_deref().unwrap_or(&[]);
    let header = EventsHeader {
        deployment: recovered.identity.deployment,
        first_seq: 1,
        last_seq: output.next_seq - 1,
        incomplete: false,
    };
    write_events_file(&run_dir.join("events-replay.bin"), &header, events).map_err(|e| e.to_string())?;
    let snapshot = format!("{:#?}\n", output.engine.snapshot());
    std::fs::write(run_dir.join("snapshot.txt"), &snapshot).map_err(|e| e.to_string())?;
    let events_vs_live = events_vs_live(&run_dir.join(EVENTS_FILE), events);
    let state_vs_live = state_vs_live(&run_dir.join(LIVE_SNAPSHOT_FILE), &snapshot);
    let mut s = Summary::new();
    s.put("replay.records", output.records);
    s.put("replay.events", events.len() / EVENT_SLOT_WORDS);
    s.put("replay.next_seq", output.next_seq);
    s.put("replay.accounts_with_nonces", output.nonces.len());
    s.put("replay.records_per_s", (u128::from(output.records) * 1_000_000_000 / elapsed.as_nanos()) as u64);
    s.put(
        "replay.files",
        format!(
            "{} and {}",
            run_dir.join("events-replay.bin").display(),
            run_dir.join("snapshot.txt").display()
        ),
    );
    s.put("replay.events_vs_live", &events_vs_live);
    s.put("replay.state_vs_live", &state_vs_live);
    print!("{}", s.text());
    if events_vs_live.starts_with("different") || state_vs_live.starts_with("different") {
        return Err("the replay differs from the live run".to_string());
    }
    Ok(())
}

/// The replayed events against the live run's `events.bin`, if it left one. The file holds
/// one life's events (13.5) and its header names that life's commands, `first_seq` to
/// `last_seq` (13.2), so exactly the replayed events of those commands are compared: a life
/// that started after seq 1, one that issued no command (a restart after a clean stop, with
/// nothing left to send), and one that later lives carried on from are all compared right.
/// After a clean stop the live run released every event of its commands, so the counts
/// must match too. An incomplete capture is not compared (13.3).
fn events_vs_live(path: &Path, replayed: &[u64]) -> String {
    let (header, live) = match read_events_file(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return "not compared: no live events.bin".into();
        }
        Err(e) => return format!("not compared: {e}"),
    };
    if header.incomplete {
        return "not compared: the live capture is incomplete (13.2)".into();
    }
    // The replay's slots are in seq order: those of the life's commands are one stretch.
    let slots = replayed.as_chunks::<EVENT_SLOT_WORDS>().0;
    let from = slots.partition_point(|slot| slot[0] < header.first_seq);
    let to = slots.partition_point(|slot| slot[0] <= header.last_seq);
    let this_life = &replayed[from * EVENT_SLOT_WORDS..to * EVENT_SLOT_WORDS];
    match first_difference(&live, this_life) {
        None => format!(
            "identical: {} events of the {} commands from seq {}",
            live.len() / EVENT_SLOT_WORDS,
            (header.last_seq + 1).saturating_sub(header.first_seq),
            header.first_seq
        ),
        Some(difference) => format!("different: {difference}"),
    }
}

/// The replayed engine's snapshot against the live one's, if the run left it.
fn state_vs_live(path: &Path, replayed: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(live) if live == replayed => "identical".to_string(),
        Ok(_) => format!("different: diff {} with snapshot.txt", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "not compared: no live snapshot".into(),
        Err(e) => format!("not compared: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::event::{Event, MarkPrice};

    fn args(text: &str) -> Args {
        Args::parse(text.split_whitespace().map(str::to_string)).expect("parses")
    }

    #[test]
    fn options_and_flags_parse() {
        let a = args("run --mode preverified --rate 20k --capture --cpus core=2 --cpus gateways=4-5");
        assert_eq!(a.words, ["run"]);
        assert_eq!(a.value("rate"), Some("20k"));
        assert!(a.flag("capture"));
        assert_eq!(a.values("cpus"), ["core=2", "gateways=4-5"]);
        assert!(Args::parse(["--rate".to_string()].into_iter()).is_err(), "a value is needed");
    }

    #[test]
    fn run_options_build_the_config() {
        let c = run_config(
            &args("run --mode preverified --rate 20k --window 2s --commit-interval 250us --unpinned"),
            InjectionMode::Signed,
            1,
        )
        .expect("valid");
        assert_eq!(
            (c.mode, c.rate, c.window_ns, c.journal.commit_interval_ns),
            (InjectionMode::PreVerified, 20_000, 2 * SECOND_NS, 250_000)
        );
        assert_eq!(c.cpus, Cpus::Unpinned);
        let smoke = run_config(&args("run --smoke --release-log"), InjectionMode::Signed, 1).expect("valid");
        assert_eq!((smoke.rate, smoke.timed_clients()), (5_000, 3_000));
        assert!(smoke.release_log && smoke.keep_journal);
        assert!(run_config(&args("run --journal discard --capture"), InjectionMode::Signed, 1).is_err());
    }

    #[test]
    fn the_verifier_is_k256_unless_asked_and_libsecp256k1_needs_the_feature() {
        let default = run_config(&args("run"), InjectionMode::Signed, 1).expect("valid");
        assert_eq!(default.verifier, VerifierKind::K256);
        let libsecp = run_config(&args("run --verifier libsecp256k1"), InjectionMode::Signed, 1);
        if cfg!(feature = "c-secp256k1") {
            assert_eq!(libsecp.expect("valid").verifier, VerifierKind::LibSecp256k1);
        } else {
            let error = libsecp.expect_err("not in this build");
            assert!(
                error.starts_with("--verifier: the libsecp256k1 verifier is not in this build"),
                "{error}"
            );
        }
        let error =
            run_config(&args("run --verifier openssl"), InjectionMode::Signed, 1).expect_err("unknown");
        assert!(error.contains("not a verifier"), "{error}");
    }

    #[test]
    fn the_scheme_is_perp_unless_asked_and_eip712_only_for_signed_runs() {
        let default = run_config(&args("run"), InjectionMode::Signed, 1).expect("valid");
        assert_eq!(default.auth, AuthScheme::Perp);
        let eip712 = run_config(&args("run --auth eip712 --smoke"), InjectionMode::Signed, 1).expect("valid");
        assert_eq!((eip712.auth, eip712.name()), (AuthScheme::Eip712, "signed-5k-eip712".to_string()));
        let error = run_config(&args("run --auth eip712 --mode preverified"), InjectionMode::Signed, 1)
            .expect_err("nothing to sign");
        assert!(error.contains("the EIP-712 scheme only in signed mode"), "{error}");
        let error =
            run_config(&args("run --auth eip712 --resume"), InjectionMode::Signed, 1).expect_err("no resume");
        assert!(error.contains("the perp scheme to resume"), "{error}");
        let error = run_config(&args("run --auth hmac"), InjectionMode::Signed, 1).expect_err("unknown");
        assert!(error.contains("not a signing scheme"), "{error}");
        // A session's template: its pre-verified runs sign nothing, its signed runs in EIP-712.
        let template =
            template_config(&args("search --name signed --auth eip712"), InjectionMode::PreVerified)
                .expect("a template for both modes");
        assert_eq!(template.in_mode(InjectionMode::PreVerified).auth, AuthScheme::Perp);
        let search = search_template(&args("search --name signed --auth eip712"), InjectionMode::Signed)
            .expect("valid");
        assert_eq!(
            (search.mode, search.auth, search.window_ns),
            (InjectionMode::Signed, AuthScheme::Eip712, 20 * SECOND_NS)
        );
        let core_path =
            search_template(&args("search --name core-path --auth eip712"), InjectionMode::PreVerified)
                .expect("valid");
        assert_eq!(core_path.auth, AuthScheme::Perp);
        let error = template_config(
            &args("sweep --kind headline --auth eip712 --window 200s"),
            InjectionMode::PreVerified,
        )
        .expect_err("its signed runs would go stale");
        assert!(error.contains("at most 3 minutes"), "{error}");
    }

    #[test]
    fn the_flow_and_its_switches_come_from_the_options() {
        use loadgen::market_flow::polymarket::ShockSize;
        use loadgen::schedule::Bursts;
        let signed = InjectionMode::Signed;
        let all = run_config(
            &args("run --shock stress --bursts busiest --makers 3 --flow polymarket --seed 9"),
            signed,
            100_000,
        )
        .expect("valid: the flow is read first, whatever the order");
        let flow =
            Flow::polymarket().with_seed(9).with_makers(3).and_then(|f| f.with_shock(ShockSize::Stress));
        assert_eq!((Ok(all.flow), all.arrivals), (flow, Arrivals::Cox(Bursts::Busiest)));
        assert_eq!(all.name(), "signed-100k-polymarket-makers3-bursts-busiest-shock-stress");
        // Bursts change only the send schedule: the M3 flow takes them too.
        let m3 = run_config(&args("run --bursts median"), signed, 100_000).expect("valid");
        assert_eq!((m3.flow, m3.name()), (Flow::m3(), "signed-100k-bursts-median".to_string()));
        let smoke = run_config(&args("run --smoke --flow polymarket-smoke"), signed, 1).expect("valid");
        assert_eq!((smoke.flow, smoke.rate), (Flow::polymarket_smoke(), 5_000));
        let search = search_template(&args("search --name signed --flow polymarket --makers 3"), signed)
            .expect("valid");
        assert_eq!(SearchSpec::durable("signed", &search, 1).name, "signed-polymarket-makers3");
    }

    #[test]
    fn switches_a_flow_doesnt_have_and_unknown_values_are_refused() {
        let refused = |text: &str, why: &str| {
            let error = run_config(&args(text), InjectionMode::Signed, 1).expect_err("refused");
            assert!(error.contains(why), "{text}: {error}");
        };
        refused("run --makers 3", "--makers needs --flow polymarket");
        refused("run --flow smoke --shock stress", "--shock needs --flow polymarket");
        refused(
            "run --flow polymarket --bursts median --arrivals uniform",
            "--bursts needs Poisson arrivals",
        );
        refused("run --flow polymarket --makers 0", "makers_k of 1 to 20");
        refused("run --flow polymarket --makers 21", "makers_k of 1 to 20");
        refused("run --flow m4", "--flow: not a flow");
        refused("run --bursts worst", "--bursts: not a burst preset");
        refused("run --flow polymarket --shock huge", "--shock: not a shock size");
        refused("run --flow polymarket --makers three", "--makers:");
    }

    #[test]
    fn the_usage_lists_every_command() {
        let usage = usage();
        for command in ["probe", "run", "sweep", "search", "ablate", "replay", "recover", "report"] {
            assert!(usage.contains(&format!("e2e {command}")), "{usage}");
        }
    }

    /// Event slots of commands `seqs`, one mark each.
    fn slots(seqs: std::ops::RangeInclusive<u64>) -> Vec<u64> {
        let mark = |seq: u64| Event::MarkPrice(MarkPrice { price: seq as i64, market: 1 });
        seqs.flat_map(|seq| pipeline::records::event_slot(seq, &mark(seq))).collect()
    }

    #[test]
    fn the_live_events_are_compared_with_the_replayed_events_of_the_same_commands() {
        let path = std::env::temp_dir().join(format!("e2e-events-vs-live-{}.bin", std::process::id()));
        let replayed = slots(1..=6); // the whole journal: commands 1 to 6
        let life =
            |first_seq, last_seq| EventsHeader { deployment: 1, first_seq, last_seq, incomplete: false };
        let compare = |header: EventsHeader, live: &[u64]| {
            write_events_file(&path, &header, live).expect("written");
            events_vs_live(&path, &replayed)
        };
        // A restart after a clean stop, with nothing left to send (the kill test's last
        // life): it released nothing. It used to be compared with the whole replay, from
        // seq 1, and called different.
        assert_eq!(compare(life(7, 6), &[]), "identical: 0 events of the 0 commands from seq 7");
        // A life that later lives carried on from, and a restarted life that ran to the end.
        assert_eq!(compare(life(3, 4), &slots(3..=4)), "identical: 2 events of the 2 commands from seq 3");
        assert_eq!(compare(life(5, 6), &slots(5..=6)), "identical: 2 events of the 2 commands from seq 5");

        // A difference is still found: an event missing, or changed.
        let missing = compare(life(3, 5), &slots(3..=4));
        assert_eq!(missing, "different: the live run released 2 events, the replay emitted 3");
        let mut changed = slots(5..=6);
        changed[EVENT_SLOT_WORDS + 2] += 1; // seq 6's price
        assert!(compare(life(5, 6), &changed).starts_with("different: event slot 1 differs"));

        // An incomplete capture is not compared (13.3).
        let incomplete = EventsHeader { incomplete: true, ..life(1, 6) };
        assert_eq!(compare(incomplete, &slots(1..=2)), "not compared: the live capture is incomplete (13.2)");
        std::fs::remove_file(&path).expect("removed");
    }
}
