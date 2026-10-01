//! `e2e run --watch`: the run as it goes, drawn in the terminal and recorded to
//! `watch.jsonl` for playback.
//!
//! **What it shows.** Ten times a second main reads the counters the hot threads already
//! share with it (`pipeline::counters`, the sender's items sent, each gateway's forwarded
//! messages) and draws one frame:
//! - where the run is: setup, warm-up, measured window or drain;
//! - each stage's rate over the last second, and its total;
//! - what the released events say: fills, notional, cancels, marks, liquidations;
//! - how busy each hot thread is.
//!
//! Rates and busy shares are differences between this frame and the one a second earlier.
//!
//! **Cost.** Main reads about 40 shared counters per frame and never touches a ring. A read
//! slows only the thread that writes that counter, and only at that moment. The one extra
//! piece of work on a hot thread is the gate's running totals of the released events
//! (`GateConfig::live`), and only `--watch` turns it on. The panel goes to standard error.
//! On a terminal each frame redraws the last one; anywhere else it prints one line a second.
//!
//! **The recording.** `watch.jsonl`, in the run directory, holds one JSON object a line:
//! - the header (`"type":"header"`): machine, mode, flow, rate, timing;
//! - every frame (`"type":"frame"`): the raw counters at run time `t`, in ns;
//! - from `e2e run`, the result (`"type":"result"`): the summary's headline numbers and
//!   its verdict.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use loadgen::schedule::Arrivals;
use pipeline::gate::Phases;
use pipeline::records::InjectionMode;

use super::checks::Watched;
use super::config::{RunConfig, mode_name};
use super::probes::cpu_model;
use super::summary::Summary;
use super::units::{SECOND_NS, count, latency, short_duration, short_rate};

/// The recording's file name, in the run directory.
pub const WATCH_FILE: &str = "watch.jsonl";
/// Time between frames, in ns.
pub const FRAME_NS: u64 = 100_000_000;
/// Frames that rates and busy shares are taken over: one second.
const RATE_FRAMES: usize = 10;
/// Width of a busy bar, in characters.
const BAR: usize = 24;

/// Where the run is, as the panel names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Before the timed flow: accounts funded and books seeded.
    Setup,
    WarmUp,
    Window,
    /// After the window: the tail, then the pipeline draining.
    Drain,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::Setup => "setup",
            Stage::WarmUp => "warm-up",
            Stage::Window => "window",
            Stage::Drain => "drain",
        }
    }

    /// The stage at run time `now`, and how far through its span it is, 0 to 1 (setup has
    /// no known length, so 0; drain comes after the window, so 1).
    fn at(phases: &Phases, now: u64) -> (Stage, f64) {
        let Some((timed_start, window)) = phases.timed() else { return (Stage::Setup, 0.0) };
        let through = |start: u64, end: u64| {
            (now.saturating_sub(start) as f64 / end.saturating_sub(start).max(1) as f64).min(1.0)
        };
        if now < timed_start {
            (Stage::Setup, 0.0)
        } else if now < window.start {
            (Stage::WarmUp, through(timed_start, window.start))
        } else if now < window.end {
            (Stage::Window, through(window.start, window.end))
        } else {
            (Stage::Drain, 1.0)
        }
    }
}

/// The counters at one moment, as recorded.
#[derive(Clone, Debug, PartialEq)]
struct Frame {
    /// Run-clock time, in ns.
    t: u64,
    stage: Stage,
    through: f64,
    sent: u64,
    verified: u64,
    rejected: u64,
    released: u64,
    flushes: u64,
    sync_ns: u64,
    busy_sender: u64,
    busy_gateways: Vec<u64>,
    busy_sequencer: u64,
    busy_journal: u64,
    busy_core: u64,
    busy_gate: u64,
    events: u64,
    fills: u64,
    notional: u64,
    cancels: u64,
    modifies: u64,
    marks: u64,
    liquidations: u64,
}

impl Frame {
    fn read(watched: &Watched, phases: &Phases, t: u64) -> Frame {
        let (stage, through) = Stage::at(phases, t);
        let p = &watched.pipeline;
        let live = &p.live;
        Frame {
            t,
            stage,
            through,
            sent: watched.sender.client_sent.load(),
            verified: watched.gateways.iter().map(|g| g.forwarded.load()).sum(),
            rejected: watched.gateways.iter().map(|g| g.rejected.load()).sum(),
            released: p.released.load(),
            flushes: p.journal_flushes.load(),
            sync_ns: p.journal_sync_ns.load(),
            busy_sender: watched.sender.thread.busy_ns.load(),
            busy_gateways: watched.gateways.iter().map(|g| g.thread.busy_ns.load()).collect(),
            busy_sequencer: p.sequencer.busy_ns.load(),
            busy_journal: p.journal.busy_ns.load(),
            busy_core: p.core.busy_ns.load(),
            busy_gate: p.gate.busy_ns.load(),
            events: live.events.load(),
            fills: live.fills.load(),
            notional: live.fill_notional.load(),
            cancels: live.cancels.load(),
            modifies: live.modifies.load(),
            marks: live.marks.load(),
            liquidations: live.liquidations.load(),
        }
    }

    fn json(&self) -> String {
        let gateways: Vec<String> = self.busy_gateways.iter().map(u64::to_string).collect();
        format!(
            concat!(
                r#"{{"type":"frame","t":{},"stage":"{}","through":{:.4},"sent":{},"verified":{},"#,
                r#""rejected":{},"released":{},"flushes":{},"sync_ns":{},"busy":{{"sender":{},"#,
                r#""gateways":[{}],"sequencer":{},"journal":{},"core":{},"gate":{}}},"events":{},"#,
                r#""fills":{},"notional":{},"cancels":{},"modifies":{},"marks":{},"liquidations":{}}}"#
            ),
            self.t,
            self.stage.name(),
            self.through,
            self.sent,
            self.verified,
            self.rejected,
            self.released,
            self.flushes,
            self.sync_ns,
            self.busy_sender,
            gateways.join(","),
            self.busy_sequencer,
            self.busy_journal,
            self.busy_core,
            self.busy_gate,
            self.events,
            self.fills,
            self.notional,
            self.cancels,
            self.modifies,
            self.marks,
            self.liquidations,
        )
    }
}

/// What the panel says about the run, once.
#[derive(Clone, Debug)]
struct Header {
    cpu: String,
    signed: bool,
    mode: &'static str,
    verifier: &'static str,
    auth: &'static str,
    flow: String,
    rate: u64,
    lanes: usize,
    on_disk: bool,
    /// How the sender spaces the timed flow's messages.
    arrivals: &'static str,
    commit_interval_ns: u64,
    warmup_ns: u64,
    window_ns: u64,
}

impl Header {
    fn of(config: &RunConfig) -> Header {
        Header {
            cpu: cpu_model().unwrap_or_else(|| "unknown CPU".into()),
            signed: config.mode == InjectionMode::Signed,
            mode: mode_name(config.mode),
            verifier: config.verifier_name(),
            auth: config.auth_name(),
            flow: config.flow.name().to_string(),
            rate: config.rate,
            lanes: config.lanes,
            on_disk: !config.discarded(),
            arrivals: match config.arrivals {
                Arrivals::Poisson => "Poisson arrivals",
                Arrivals::Uniform => "evenly spaced",
                Arrivals::Cox(_) => "bursty Poisson arrivals",
            },
            commit_interval_ns: config.journal.commit_interval_ns,
            warmup_ns: config.warmup_ns,
            window_ns: config.window_ns,
        }
    }

    fn json(&self) -> String {
        format!(
            concat!(
                r#"{{"type":"header","cpu":"{}","mode":"{}","verifier":"{}","auth":"{}","flow":"{}","#,
                r#""rate":{},"lanes":{},"journal":"{}","commit_interval_ns":{},"warmup_ns":{},"#,
                r#""window_ns":{},"frame_ns":{}}}"#
            ),
            json_text(&self.cpu),
            self.mode,
            self.verifier,
            self.auth,
            json_text(&self.flow),
            self.rate,
            self.lanes,
            if self.on_disk { "disk" } else { "discard" },
            self.commit_interval_ns,
            self.warmup_ns,
            self.window_ns,
            FRAME_NS,
        )
    }
}

/// The live panel: main calls [`Panel::tick`] every time it looks at the run, and
/// [`Panel::finish`] when the sender is done.
#[derive(Debug)]
pub struct Panel {
    header: Header,
    frames: Vec<Frame>,
    next_frame: u64,
    terminal: bool,
}

impl Panel {
    pub fn new(config: &RunConfig) -> Panel {
        Panel {
            header: Header::of(config),
            frames: Vec::new(),
            next_frame: 0,
            terminal: io::stderr().is_terminal(),
        }
    }

    /// Takes and draws a frame if one is due at run time `now`.
    pub fn tick(&mut self, watched: &Watched, phases: &Phases, now: u64) {
        if now < self.next_frame {
            return;
        }
        self.next_frame = now + FRAME_NS;
        self.frames.push(Frame::read(watched, phases, now));
        if self.terminal {
            let mut text = String::new();
            if self.frames.len() == 1 {
                text.push_str("\x1b[2J"); // clear the screen once
            }
            text.push_str("\x1b[H"); // then draw over the last frame
            // No newline after the last line: a panel as tall as the terminal mustn't scroll.
            text.push_str(&self.render().lines().collect::<Vec<_>>().join("\x1b[K\n"));
            text.push_str("\x1b[K\x1b[J");
            let _ = io::stderr().write_all(text.as_bytes());
        } else if self.frames.len() % RATE_FRAMES == 1 {
            eprintln!("watch: {}", self.line());
        }
    }

    /// Takes the last frame, draws it, and writes the recording into `dir`.
    pub fn finish(mut self, watched: &Watched, phases: &Phases, now: u64, dir: &Path) -> io::Result<()> {
        self.next_frame = 0;
        self.tick(watched, phases, now);
        if self.terminal {
            eprintln!(); // below the panel
        }
        let mut file = io::BufWriter::new(File::create(dir.join(WATCH_FILE))?);
        writeln!(file, "{}", self.header.json())?;
        for frame in &self.frames {
            writeln!(file, "{}", frame.json())?;
        }
        file.flush()
    }

    /// The latest frame and the one a second before it (or the first).
    fn span(&self) -> Option<(&Frame, &Frame)> {
        let last = self.frames.last()?;
        let first = &self.frames[self.frames.len().saturating_sub(RATE_FRAMES + 1)];
        Some((first, last))
    }

    /// One line, for a standard error that isn't a terminal.
    fn line(&self) -> String {
        let Some((a, b)) = self.span() else { return String::new() };
        let per_s = |f: fn(&Frame) -> u64| rate(f(a), f(b), a.t, b.t);
        format!(
            "{} {:.0}%: sent {}/s, durable {}/s, fills {}/s, flushes {}/s",
            b.stage.name(),
            b.through * 100.0,
            count(per_s(|f| f.sent)),
            count(per_s(|f| f.released)),
            count(per_s(|f| f.fills)),
            count(per_s(|f| f.flushes)),
        )
    }

    /// The panel, as text with ANSI colours.
    fn render(&self) -> String {
        let Some((a, b)) = self.span() else { return String::new() };
        let h = &self.header;
        let per_s = |f: fn(&Frame) -> u64| rate(f(a), f(b), a.t, b.t);
        let busy = |x: u64, y: u64| share(x, y, a.t, b.t);
        let mut out = String::new();
        let w = &mut out;

        let _ = writeln!(w, "{BOLD} PERPS ENGINE{RESET}  live run on {}", h.cpu);
        let what = if h.signed {
            format!("signed orders ({}, {} signatures)", h.auth, h.verifier)
        } else {
            "pre-verified commands".to_string()
        };
        let journal = if h.on_disk {
            format!("journal on disk, group commit every {}", short_duration(h.commit_interval_ns))
        } else {
            "journal discarded".to_string()
        };
        let _ = writeln!(w, " {}/s of {what} · {} flow · {}", short_rate(h.rate), h.flow, journal);
        let _ = writeln!(w);

        let stages = [Stage::Setup, Stage::WarmUp, Stage::Window, Stage::Drain];
        let mut steps = String::new();
        for stage in stages {
            let mark = match (stage as u8).cmp(&(b.stage as u8)) {
                std::cmp::Ordering::Less => format!("{DIM}{} ✓{RESET}", stage.name()),
                std::cmp::Ordering::Equal => format!("{CYAN}{BOLD}▶ {}{RESET}", stage.name()),
                std::cmp::Ordering::Greater => format!("{DIM}{}{RESET}", stage.name()),
            };
            steps.push_str(&mark);
            steps.push_str("   ");
        }
        let _ = writeln!(w, " {steps}");
        let progress = match b.stage {
            Stage::WarmUp => {
                format!("warm-up {:.1} s of {:.1} s", b.through * secs(h.warmup_ns), secs(h.warmup_ns))
            }
            Stage::Window => {
                format!("measured {:.1} s of {:.1} s", b.through * secs(h.window_ns), secs(h.window_ns))
            }
            Stage::Setup => "funding accounts, seeding the books".to_string(),
            Stage::Drain => "window closed: draining".to_string(),
        };
        let _ = writeln!(w, " {CYAN}{}{RESET} {progress}", bar(b.through, 40));
        let _ = writeln!(w);

        let _ = writeln!(w, "{BOLD} PIPELINE                              per second         total{RESET}");
        let row = |w: &mut String, name: &str, r: u64, total: u64, note: &str| {
            let _ = writeln!(w, "   {name:<32} {:>13} {:>13}   {DIM}{note}{RESET}", count(r), count(total));
        };
        row(w, "orders sent", per_s(|f| f.sent), b.sent, &format!("open loop, {}", h.arrivals));
        if h.signed {
            let note = format!("secp256k1, {} gateways in parallel", h.lanes);
            row(w, "signatures verified", per_s(|f| f.verified), b.verified, &note);
        }
        let note = if h.on_disk {
            "matched, on disk, acked; with the operator's marks"
        } else {
            "matched, acked (journal discarded); with the operator's marks"
        };
        row(w, "durable and acknowledged", per_s(|f| f.released), b.released, note);
        let flushes = b.flushes - a.flushes;
        let flush_avg = (b.sync_ns - a.sync_ns).checked_div(flushes);
        let note = if h.on_disk { format!("fdatasync {} each", latency(flush_avg)) } else { String::new() };
        row(w, "journal flushes", per_s(|f| f.flushes), b.flushes, &note);
        let _ = writeln!(w);

        let _ = writeln!(w, "{BOLD} WHAT THE ENGINE COMPUTED                                   {RESET}");
        let notional = per_s(|f| f.notional);
        row(w, "fills", per_s(|f| f.fills), b.fills, &format!("{} traded per second", money(notional)));
        row(w, "cancels", per_s(|f| f.cancels), b.cancels, "");
        row(w, "modifies", per_s(|f| f.modifies), b.modifies, "");
        row(w, "mark prices", per_s(|f| f.marks), b.marks, "each one re-checks margin");
        row(w, "liquidations", per_s(|f| f.liquidations), b.liquidations, "");
        row(w, "events released", per_s(|f| f.events), b.events, "");
        let _ = writeln!(w);

        let _ = writeln!(
            w,
            "{BOLD} THREADS{RESET}                           {DIM}share of one core, busy{RESET}"
        );
        let thread = |w: &mut String, name: &str, share: f64, note: &str| {
            let _ = writeln!(
                w,
                "   {name:<14} {CYAN}{}{RESET} {:>5.1}%  {DIM}{note}{RESET}",
                bar(share, BAR),
                share * 100.0
            );
        };
        thread(w, "sender", busy(a.busy_sender, b.busy_sender), "the load generator");
        if !b.busy_gateways.is_empty() {
            let shares: Vec<f64> =
                a.busy_gateways.iter().zip(&b.busy_gateways).map(|(&x, &y)| busy(x, y)).collect();
            let mean = shares.iter().sum::<f64>() / shares.len() as f64;
            let max = shares.iter().cloned().fold(0.0, f64::max);
            thread(
                w,
                &format!("gateways ×{}", shares.len()),
                mean,
                &format!("average; busiest {:.1}%", max * 100.0),
            );
        }
        thread(w, "sequencer", busy(a.busy_sequencer, b.busy_sequencer), "one order for everything");
        thread(w, "core", busy(a.busy_core, b.busy_core), "margin checks and matching, one thread");
        let disk = busy(a.sync_ns, b.sync_ns);
        thread(
            w,
            "journal",
            busy(a.busy_journal, b.busy_journal),
            &format!("plus {:.0}% waiting on the disk", disk * 100.0),
        );
        thread(w, "gate", busy(a.busy_gate, b.busy_gate), "releases only what is durable");
        let _ = writeln!(w);
        let start = if h.signed {
            "each order: sign → gateway (decode, nonce, verify) →"
        } else {
            "each command:"
        };
        let _ = writeln!(w, " {DIM}{start} sequencer → journal + core (margin, match){RESET}");
        let durable = if h.on_disk { "fdatasync, about once a millisecond → " } else { "" };
        let _ = writeln!(w, " {DIM}            → {durable}gate → acknowledged{RESET}");
        out
    }
}

/// Appends the result line to the recording in `dir` and returns the result as the panel's
/// closing lines, from the run's summary.
pub fn finish_with_result(dir: &Path, summary: &Summary) -> io::Result<String> {
    let valid = summary.flag("check.valid") == Some(true);
    // Without stamps nothing is classed as the window (15.5), so only the gate's rate is known.
    let stamped = summary.get("run.stamps") != Some("off");
    let achieved = if stamped {
        summary.u64("window.achieved_rate").unwrap_or(0)
    } else {
        summary.u64("window.released_per_second").unwrap_or(0)
    };
    let ns = |key: &str| summary.ns(&format!("stage.client.to_durable_ack.{key}")).flatten();
    let (p50, p99, p999) = (ns("p50"), ns("p99"), ns("p999"));
    let fills = summary.u64("breakdown.window.fills").unwrap_or(0);
    let liquidations = summary.u64("breakdown.window.liquidations").unwrap_or(0);
    let replay = summary.get("replay.verdict").unwrap_or("off").to_string();
    let audit =
        summary.u64("audit.signed").map(|signed| (signed, summary.u64("audit.failures").unwrap_or(0)));
    let invalid = summary.get("check.invalid").unwrap_or("").to_string();

    let json_ns = |v: Option<u64>| v.map_or("null".to_string(), |v| v.to_string());
    let line = format!(
        concat!(
            r#"{{"type":"result","valid":{},"invalid":"{}","achieved":{},"p50_ns":{},"p99_ns":{},"#,
            r#""p999_ns":{},"fills":{},"liquidations":{},"replay":"{}","audit_signed":{},"audit_failures":{}}}"#
        ),
        valid,
        json_text(&invalid),
        achieved,
        json_ns(p50),
        json_ns(p99),
        json_ns(p999),
        fills,
        liquidations,
        json_text(&replay),
        audit.map_or("null".to_string(), |(s, _)| s.to_string()),
        audit.map_or("null".to_string(), |(_, f)| f.to_string()),
    );
    let mut file = OpenOptions::new().append(true).open(dir.join(WATCH_FILE))?;
    writeln!(file, "{line}")?;

    let mut out = String::new();
    let w = &mut out;
    let _ = writeln!(w, "\n{BOLD} RESULT{RESET}  (the measured window)");
    let _ = writeln!(w, "   achieved                 {} orders/s", count(achieved));
    if stamped {
        let _ = writeln!(
            w,
            "   order → durable ack      p50 {} · p99 {} · p99.9 {}",
            latency(p50),
            latency(p99),
            latency(p999)
        );
        let _ = writeln!(w, "   fills, liquidations      {} and {}", count(fills), count(liquidations));
    } else {
        let _ = writeln!(w, "   latency                  not measured (--stamps off)");
    }
    if replay != "off" {
        let _ = writeln!(w, "   journal replayed         {replay}");
    }
    if let Some((signed, failures)) = audit {
        let _ = writeln!(w, "   signatures re-checked    {}, {} failures", count(signed), count(failures));
    }
    if valid {
        let _ = writeln!(w, "   verdict                  {GREEN}{BOLD}valid{RESET}");
    } else {
        let _ = writeln!(w, "   verdict                  {YELLOW}{BOLD}invalid{RESET}: {invalid}");
    }
    Ok(if io::stderr().is_terminal() { out } else { plain(&out) })
}

/// `text` without its colour codes.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("\x1b[") {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        rest = rest.find('m').map_or("", |end| &rest[end + 1..]);
    }
    out.push_str(rest);
    out
}

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Per second, from two counts at run times `t0` and `t1`.
fn rate(x0: u64, x1: u64, t0: u64, t1: u64) -> u64 {
    let dt = t1.saturating_sub(t0);
    if dt == 0 {
        0
    } else {
        (u128::from(x1.saturating_sub(x0)) * u128::from(SECOND_NS) / u128::from(dt)) as u64
    }
}

/// Busy time over wall time, from two busy totals at run times `t0` and `t1`.
fn share(x0: u64, x1: u64, t0: u64, t1: u64) -> f64 {
    let dt = t1.saturating_sub(t0);
    if dt == 0 { 0.0 } else { (x1.saturating_sub(x0) as f64 / dt as f64).min(1.0) }
}

fn secs(ns: u64) -> f64 {
    ns as f64 / SECOND_NS as f64
}

/// A bar `width` characters wide, `fraction` full, in eighths of a character.
fn bar(fraction: f64, width: usize) -> String {
    const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (fraction.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let mut out: String = "█".repeat(eighths / 8);
    if eighths / 8 < width {
        out.push(EIGHTHS[eighths % 8]);
        out.push_str(&"·".repeat(width - eighths / 8 - 1));
    }
    out
}

/// Micro-dollars as `$1.23M`, `$45.6k` or `$789`.
fn money(micros: u64) -> String {
    let dollars = micros as f64 / 1e6;
    if dollars >= 1e9 {
        format!("${:.2}B", dollars / 1e9)
    } else if dollars >= 1e6 {
        format!("${:.2}M", dollars / 1e6)
    } else if dollars >= 1e3 {
        format!("${:.1}k", dollars / 1e3)
    } else {
        format!("${dollars:.0}")
    }
}

/// Text inside a JSON string: quotes, backslashes and control characters escaped.
fn json_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(t: u64, sent: u64, busy: u64) -> Frame {
        Frame {
            t,
            stage: Stage::Window,
            through: 0.5,
            sent,
            verified: sent,
            rejected: 0,
            released: sent,
            flushes: t / 1_000_000,
            sync_ns: t / 2,
            busy_sender: busy,
            busy_gateways: vec![busy, 2 * busy],
            busy_sequencer: busy,
            busy_journal: busy,
            busy_core: busy,
            busy_gate: busy,
            events: 2 * sent,
            fills: sent / 25,
            notional: sent * 1_000_000,
            cancels: sent / 3,
            modifies: sent / 3,
            marks: t / 200_000_000,
            liquidations: 0,
        }
    }

    #[test]
    fn rates_and_shares_are_differences_over_time() {
        assert_eq!(rate(1_000, 101_000, 0, SECOND_NS), 100_000);
        assert_eq!(rate(0, 50, 0, SECOND_NS / 2), 100);
        assert_eq!(rate(5, 5, 7, 7), 0, "no time, no rate");
        assert!((share(0, SECOND_NS / 4, 0, SECOND_NS) - 0.25).abs() < 1e-9);
        assert_eq!(share(0, 3 * SECOND_NS, 0, SECOND_NS), 1.0, "capped at one core");
    }

    #[test]
    fn bars_have_their_width_whatever_the_fraction() {
        for fraction in [0.0, 0.01, 0.5, 0.999, 1.0, 2.0, -1.0] {
            assert_eq!(bar(fraction, 10).chars().count(), 10, "{fraction}");
        }
        assert_eq!(bar(1.0, 4), "████");
        assert_eq!(bar(0.0, 4), " ···");
    }

    #[test]
    fn the_panel_draws_a_second_of_rates_and_records_every_frame_as_json() {
        let config = RunConfig::new(InjectionMode::Signed, 100_000);
        let mut panel = Panel::new(&config);
        panel.terminal = false;
        for i in 0..=20u64 {
            panel.frames.push(frame(i * FRAME_NS, i * 10_000, i * FRAME_NS / 10));
        }
        let text = panel.render();
        assert!(text.contains("100,000"), "a second of 10,000 per frame: {text}");
        assert!(text.contains("gateways ×2"), "{text}");
        assert!(text.contains("10.0%"), "busy a tenth of the time: {text}");
        assert!(panel.line().starts_with("window 50%: sent 100,000/s"), "{}", panel.line());
        let json = panel.frames[3].json();
        assert!(json.starts_with(r#"{"type":"frame","t":300000000,"stage":"window""#), "{json}");
        assert!(json.contains(r#""gateways":[30000000,60000000]"#), "{json}");
        assert!(panel.header.json().contains(r#""mode":"signed","verifier":"k256""#));
    }

    #[test]
    fn plain_text_drops_the_colour_codes() {
        assert_eq!(plain(&format!("{BOLD}a{RESET} b {GREEN}{BOLD}c{RESET}")), "a b c");
        assert_eq!(plain("no codes"), "no codes");
    }

    #[test]
    fn json_text_escapes_what_json_needs() {
        assert_eq!(json_text(r#"a "b" \ c"#), r#"a \"b\" \\ c"#);
        assert_eq!(json_text("x\ny"), "x\\u000ay");
    }

    #[test]
    fn money_is_short() {
        assert_eq!(money(1_234_000_000), "$1.2k");
        assert_eq!(money(5_600_000_000_000), "$5.60M");
        assert_eq!(money(400_000), "$0");
    }
}
