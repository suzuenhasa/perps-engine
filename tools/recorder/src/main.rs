//! # recorder
//!
//! Records Polymarket Perps' public WebSocket market data, unattended, for the "Later"
//! fidelity work (INFO.md section 12.1). Funding needs whole hourly windows, so the
//! recorder runs from Milestone 0 onward and reconnects on its own.
//!
//! **What it records.** A *tickers* connection subscribes to `tickers::all` (mark, index,
//! last, mid, open interest and funding for every instrument). Each ticker frame names an
//! instrument. The first time one appears, it is handed to an *instruments* connection,
//! which subscribes to that instrument's `book::<id>` (20-level snapshots) and
//! `trades::<id>`. Polymarket accepts at most 100 subscriptions per connection, so
//! instruments are spread over several connections, 45 per connection
//! ([`INSTRUMENTS_PER_CONNECTION`]). New listings are picked up automatically.
//!
//! **How much it keeps.** Book and ticker frames arrive about every 100 ms per
//! instrument. They are sampled to one frame per second of server time (`sampler.rs`);
//! trades and subscription replies are all kept. See `docs/DECISIONS.md` D-007.
//!
//! **File format.** One JSON object per line, in hourly files `<out>/YYYY-MM-DD/HH.jsonl`
//! (UTC), gzipped when the hour is over (`files.rs`):
//! - `{"recv_ms":…,"frame":<frame exactly as received>}`. Frames are never re-encoded, so
//!   fields we don't know about survive. A frame that isn't single-line JSON is stored
//!   escaped instead, as `{"recv_ms":…,"raw":"…"}`.
//! - `{"recv_ms":…,"marker":…,"link":…,"detail":…}` make gaps and subscriptions explicit.
//!   `file_opened` (link `file`) starts every file; `connected` and `disconnected` bracket
//!   each connection, and a process restart shows as a `connected` with no `disconnected`
//!   before it. `subscribe` records each request (`id=N ch1,ch2,…`; ids restart at 1 on
//!   every connection) and `subscribe_refused` an error reply to one, so a gap in a
//!   channel can be traced to its request. A refused channel is not retried until the
//!   connection reconnects.
//!
//! `recv_ms` is this machine's clock, which can be off from Polymarket's by a fraction of a
//! second (about 0.4 s on 2026-09-29). Align data on each frame's own `ts`, not `recv_ms`.
//!
//! **Not on the hot path.** A separate process that never touches the engine; it uses
//! tokio and TLS, which the engine never does.
//!
//! Usage: `recorder --out <dir>` (see `./dev record build` and `./dev record start`).

mod files;
mod frames;
mod sampler;

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use files::{HourlyFiles, unix_ms};
use frames::{embeddable, instrument_ids, is_error_reply, json_escape};
use sampler::{Offer, Sampler};

const URL: &str = "wss://ws.perpetuals.polymarket.com/v1/ws";
/// Give up on a connection attempt (TCP, TLS and WebSocket handshake) after this long.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Reconnect if no data frame arrives for this long (tickers normally flow every 100 ms).
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How often an idle connection wakes up to flush files and pick up new instruments.
const POLL: Duration = Duration::from_secs(1);
/// Reconnect backoff doubles from 1 s up to this cap, and resets after a healthy session.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const HEALTHY_SESSION: Duration = Duration::from_secs(300);
/// Channels per subscribe request, to keep each request small.
const CHANNELS_PER_REQUEST: usize = 20;
const STATS_EVERY: Duration = Duration::from_secs(60);
/// Polymarket refuses subscriptions past 100 per connection ("subscription limit
/// reached", seen 2026-09-29). Two per instrument (book, trades) leaves room to spare.
const INSTRUMENTS_PER_CONNECTION: u32 = 45;
// Two channels per instrument; the first connection can also get id 0, so 46 instruments.
const _: () = assert!(2 * (INSTRUMENTS_PER_CONNECTION + 1) <= 100);
/// The commit the binary was built from (set by `./dev record build`).
const BUILD: &str = match option_env!("PERPS_GIT_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type SharedSink = Arc<Mutex<Sink>>;

fn main() {
    let out_dir = parse_out_dir();
    // Any panic ends the process, so Docker restarts it. Without this, tokio would catch a
    // panic in an instruments connection's task and that connection would stay dead
    // while the rest of the process kept running.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        std::process::exit(101);
    }));
    // rustls is compiled with only the `ring` backend (docs/DECISIONS.md D-006).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("no other TLS crypto provider should be installed");

    let header = format!(
        "recorder build {BUILD}; book::* and tickers::* sampled to the last frame of each second \
         of server time, everything else kept"
    );
    let sink = Arc::new(Mutex::new(Sink::new(out_dir, header)));
    lock(&sink).files.compress_finished_hours();

    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("failed to start tokio");
    let tickers = Link::tickers(&sink);
    runtime.block_on(run_link(tickers, sink));
}

fn parse_out_dir() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    match args.as_slice() {
        [_, flag, dir] if flag == "--out" => PathBuf::from(dir),
        _ => {
            eprintln!("usage: recorder --out <dir>");
            std::process::exit(2);
        }
    }
}

/// One WebSocket connection and what it should be subscribed to.
struct Link {
    name: String,
    /// Subscribed on every (re)connect.
    channels: Vec<String>,
    /// Instrument connections: instruments handed over by the tickers connection.
    assigned: Option<mpsc::UnboundedReceiver<u32>>,
    /// Tickers connection: finds instruments and hands them to instrument connections.
    discovery: Option<Discovery>,
    sampler: Sampler,
}

impl Link {
    fn tickers(sink: &SharedSink) -> Link {
        Link {
            name: "tickers".to_string(),
            channels: vec!["tickers::all".to_string()],
            assigned: None,
            discovery: Some(Discovery { known: BTreeSet::new(), links: BTreeMap::new(), sink: sink.clone() }),
            sampler: Sampler::default(),
        }
    }

    fn instruments(index: u32, assigned: mpsc::UnboundedReceiver<u32>) -> Link {
        Link {
            name: format!("instruments-{index}"),
            channels: Vec::new(),
            assigned: Some(assigned),
            discovery: None,
            sampler: Sampler::default(),
        }
    }

    /// Channels for instruments assigned since the last call. They are also added to
    /// `channels`, so a reconnect resubscribes them.
    fn take_new_channels(&mut self) -> Vec<String> {
        let mut new = Vec::new();
        if let Some(assigned) = &mut self.assigned {
            while let Ok(id) = assigned.try_recv() {
                for channel in [format!("book::{id}"), format!("trades::{id}")] {
                    self.channels.push(channel.clone());
                    new.push(channel);
                }
            }
        }
        new
    }
}

/// Which connection carries an instrument: 1-45 on the first, 46-90 on the second, ...
fn link_index(instrument: u32) -> u32 {
    instrument.saturating_sub(1) / INSTRUMENTS_PER_CONNECTION
}

/// Lives on the tickers connection: starts instrument connections as instruments appear.
struct Discovery {
    known: BTreeSet<u32>,
    links: BTreeMap<u32, mpsc::UnboundedSender<u32>>,
    sink: SharedSink,
}

impl Discovery {
    fn observe(&mut self, frame: &str) {
        for id in instrument_ids(frame) {
            if !self.known.insert(id) {
                continue;
            }
            let sink = &self.sink;
            let link = self.links.entry(link_index(id)).or_insert_with(|| {
                let (sender, receiver) = mpsc::unbounded_channel();
                tokio::spawn(run_link(Link::instruments(link_index(id), receiver), sink.clone()));
                sender
            });
            // The receiver lives as long as its connection task, which never ends (a panic
            // ends the whole process; see main).
            let _ = link.send(id);
            lock(sink).stats.instruments = self.known.len();
        }
    }
}

/// Runs one connection forever: connect, record until it fails, wait, repeat.
async fn run_link(mut link: Link, sink: SharedSink) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let reason = match session(&mut link, &sink).await {
            Ok(never) => match never {},
            Err(reason) => reason,
        };
        {
            let mut out = lock(&sink);
            for held in link.sampler.release_all() {
                out.frame(held.recv_ms, &held.frame);
            }
            out.marker(&link.name, "disconnected", &reason);
            out.files.flush();
        }
        eprintln!("recorder: {} disconnected ({reason}); reconnecting in {backoff:?}", link.name);

        if started.elapsed() >= HEALTHY_SESSION {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One connection's lifetime. Only returns when it fails, with the reason.
async fn session(link: &mut Link, sink: &SharedSink) -> Result<Infallible, String> {
    let (mut socket, _response) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(URL))
            .await
            .map_err(|_| "connect: timed out".to_string())?
            .map_err(|e| format!("connect: {e}"))?;
    lock(sink).marker(&link.name, "connected", &format!("recorder build {BUILD}"));
    eprintln!("recorder: {} connected", link.name);

    let mut next_request_id = 1;
    link.take_new_channels(); // instruments assigned while we were disconnected
    subscribe(&mut socket, &mut next_request_id, &link.channels, &link.name, sink).await?;

    let mut last_data = Instant::now();
    loop {
        match tokio::time::timeout(POLL, socket.next()).await {
            Err(_quiet_for_a_poll) => {
                if last_data.elapsed() >= STALL_TIMEOUT {
                    return Err(format!("no data for {STALL_TIMEOUT:?}"));
                }
            }
            Ok(None) => return Err("connection closed".to_string()),
            Ok(Some(Err(e))) => return Err(format!("read: {e}")),
            Ok(Some(Ok(Message::Text(text)))) => {
                last_data = Instant::now();
                let frame = text.as_str();
                record(link, sink, frame);
                if let Some(discovery) = &mut link.discovery {
                    discovery.observe(frame);
                }
            }
            Ok(Some(Ok(Message::Close(close)))) => return Err(format!("server closed: {close:?}")),
            // tungstenite answers pings itself; the API sends no binary frames.
            Ok(Some(Ok(_control))) => {}
        }
        let new = link.take_new_channels();
        subscribe(&mut socket, &mut next_request_id, &new, &link.name, sink).await?;
        lock(sink).tick();
    }
}

/// Passes a received frame through the sampler and writes whatever it releases.
fn record(link: &mut Link, sink: &SharedSink, frame: &str) {
    let recv_ms = unix_ms();
    let mut out = lock(sink);
    out.stats.received += 1;
    if is_error_reply(frame) {
        out.stats.subscribe_errors += 1;
        out.marker(&link.name, "subscribe_refused", frame);
        eprintln!("recorder: {}: subscription refused: {frame}", link.name);
    }
    match link.sampler.offer(recv_ms, frame) {
        Offer::PassThrough => out.frame(recv_ms, frame),
        Offer::Held => {}
        Offer::Released(held) => out.frame(held.recv_ms, &held.frame),
    }
}

/// Sends subscribe requests, and writes a `subscribe` marker for each so that a reply
/// (which carries only the request id) can be matched to its channels.
async fn subscribe(
    socket: &mut Socket,
    next_id: &mut u64,
    channels: &[String],
    link: &str,
    sink: &SharedSink,
) -> Result<(), String> {
    for chunk in channels.chunks(CHANNELS_PER_REQUEST) {
        let quoted: Vec<String> = chunk.iter().map(|c| format!("\"{c}\"")).collect();
        let request = format!(r#"{{"id":{},"req":"sub","chs":[{}]}}"#, next_id, quoted.join(","));
        lock(sink).marker(link, "subscribe", &format!("id={next_id} {}", chunk.join(",")));
        *next_id += 1;
        socket.send(Message::Text(request.into())).await.map_err(|e| format!("subscribe: {e}"))?;
    }
    Ok(())
}

/// The output files plus counters, shared by all connections. Locked only for short,
/// synchronous writes, never across an `.await`.
struct Sink {
    files: HourlyFiles,
    stats: Stats,
}

impl Sink {
    fn new(dir: PathBuf, header: String) -> Self {
        Sink { files: HourlyFiles::new(dir, header), stats: Stats::default() }
    }

    fn frame(&mut self, recv_ms: u64, frame: &str) {
        let line = frame_line(recv_ms, frame);
        self.stats.written += 1;
        self.stats.bytes += line.len() as u64;
        self.files.write_line(&line);
    }

    fn marker(&mut self, link: &str, marker: &str, detail: &str) {
        let line = format!(
            r#"{{"recv_ms":{},"marker":"{marker}","link":"{link}","detail":"{}"}}"#,
            unix_ms(),
            json_escape(detail)
        );
        self.files.write_line(&line);
    }

    fn tick(&mut self) {
        self.files.flush_if_due();
        self.stats.report_if_due();
    }
}

fn frame_line(recv_ms: u64, frame: &str) -> String {
    if embeddable(frame) {
        format!(r#"{{"recv_ms":{recv_ms},"frame":{frame}}}"#)
    } else {
        format!(r#"{{"recv_ms":{recv_ms},"raw":"{}"}}"#, json_escape(frame))
    }
}

fn lock(sink: &SharedSink) -> MutexGuard<'_, Sink> {
    // A panic elsewhere can't leave the files half-updated in a way that matters here.
    sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Counters printed to the container log once a minute.
#[derive(Default)]
struct Stats {
    received: u64,
    written: u64,
    bytes: u64,
    subscribe_errors: u64,
    instruments: usize,
    since: Option<Instant>,
}

impl Stats {
    fn report_if_due(&mut self) {
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= STATS_EVERY {
            eprintln!(
                "recorder: last {:.0?}: received {} frames, wrote {} ({} KiB); {} subscription errors; \
                 {} instruments assigned",
                since.elapsed(),
                self.received,
                self.written,
                self.bytes / 1024,
                self.subscribe_errors,
                self.instruments
            );
            *self = Stats { instruments: self.instruments, since: Some(Instant::now()), ..Stats::default() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruments_are_spread_so_no_connection_passes_the_subscription_limit() {
        assert_eq!(link_index(1), 0);
        assert_eq!(link_index(45), 0);
        assert_eq!(link_index(46), 1);
        assert_eq!(link_index(90), 1);
    }

    #[test]
    fn non_json_frames_are_stored_escaped() {
        assert_eq!(frame_line(5, r#"{"ch":"x"}"#), r#"{"recv_ms":5,"frame":{"ch":"x"}}"#);
        assert_eq!(frame_line(5, "rate limited"), r#"{"recv_ms":5,"raw":"rate limited"}"#);
    }

    #[test]
    fn assigned_instruments_become_book_and_trade_channels() {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut link = Link::instruments(0, receiver);
        sender.send(7).unwrap();
        assert_eq!(link.take_new_channels(), vec!["book::7", "trades::7"]);
        assert_eq!(link.take_new_channels(), Vec::<String>::new());
        assert_eq!(link.channels, vec!["book::7", "trades::7"], "kept for resubscribing");
    }
}
