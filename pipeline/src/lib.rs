//! # pipeline
//!
//! Everything that moves commands to the core thread and events away from it, in
//! Milestone 3: the rings between threads, the sequencer, the group-commit journal, the
//! pinned core thread that runs the engine, the gate that releases only durable results,
//! and replay. The specification is `docs/PIPELINE.md`; decisions D-021 to D-031.
//!
//! **The picture** (PIPELINE.md 2.1):
//!
//! ```text
//!  SIGNED MODE
//!  sender ──ingress[g]──> gateway g ──lane[g]──┐
//!  (loadgen)  g = 0..N-1    (N threads)         │
//!     │                                         ├──> sequencer ──journal ring──> journal writer ──> disk
//!     └───────────── operator ring ────────────┘        │                             │
//!                                                        │ core ring                   │ durable watermark
//!  PRE-VERIFIED MODE                                     v                             v (one atomic u64)
//!  sender ──lane[g]──> (straight into the sequencer)   core thread ──event ring──> gate (event consumer)
//!                                                      Engine<Book, Fast>          releases events whose
//!                                                                                  command is durable;
//!                                                                                  histograms, counters,
//!                                                                                  capture for replay
//! ```
//!
//! Every arrow is a bounded single-producer, single-consumer ring ([`ring`]), except the
//! durable watermark, one atomic counter written by the journal writer and read by the gate
//! ([`counters::Watermark`]).
//!
//! **The threads** (2.2). One sender (in `loadgen`) sends each pre-built message at its
//! scheduled time. `N` gateways (in `gateway`) check and verify the messages of the
//! accounts with `account mod N == g`. The sequencer merges the lanes and the operator ring,
//! gives each command a sequence number and a timestamp, and writes it to the journal ring
//! and then the core ring. The journal writer appends records to disk in batches, one
//! `fdatasync` per batch, and publishes the durable watermark. The core applies each command
//! to `Engine<Book, Fast>` and writes its events, then a trailer, to the event ring. The
//! gate releases a command's events once its seq is at or below the watermark, and records
//! every latency. Each of these threads is pinned to a CPU of its own where the machine
//! has enough ([`affinity`]) and busy-polls ([`idle`]); main starts them, samples the
//! counters ([`counters`]) and joins them.
//!
//! **Back-pressure: reject at the edge, wait inside** (2.4). A full ingress ring drops the
//! message and a gateway whose lane is full rejects it (`Busy`) before verifying: the
//! sender is the open-loop clock and must never wait. Once the sequencer has given a
//! command a seq, it must reach both the journal and the core, so the sequencer checks for
//! room in both rings before it takes a record, and the core waits inside its event sink
//! for the gate to free space. A slow disk therefore fills the journal and event rings,
//! then the lanes, and the gateways reject.
//!
//! **Stopping, and panics** (2.8). Each ring closes when its producer is dropped, and a
//! consumer stops only when its ring is closed and empty, so the pipeline stops by itself
//! in data-flow order, losing nothing. Any panic aborts the whole process ([`mod@panic`]),
//! and recovery takes over at the next start. The gate releases a command's events only
//! with its trailer, so a command that panics has none of its results released, unless it
//! had emitted more events than the event ring holds ([`gate`]).
//!
//! **Modules.**
//! - [`ring`]: the SPSC ring of atomic words, its memory ordering and its closing flag.
//! - [`codec`]: CMD40 and EVT56, the one encoding of commands and events (section 4).
//! - [`records`]: the ring records, word by word, and the trailer (3.3, 10.3).
//! - [`clock`]: the run clock every stamp and journal timestamp comes from (9.2, 15.1).
//! - [`idle`]: what a thread does when it finds no work.
//! - [`histogram`]: the latency histogram (15.3).
//! - [`crc32c`]: the journal's checksum, by slicing by 8 (11.4).
//! - [`counters`]: the single-writer counters main reads, the durable watermark, and
//!   thread health (3.2, 11.7, 15.4).
//! - [`mod@panic`]: the abort-on-panic hook and the thread-local current seq (2.8).
//! - [`affinity`]: the CPU topology, the default layout, and pinning, the one `unsafe`
//!   function (2.6).
//! - [`sequencer`]: the merge of the operator ring and the lanes, seqs and timestamps, and
//!   space-first writes to the journal and core rings (section 9).
//! - [`core_thread`]: the core loop over `Engine<Book, Fast>` and its event-ring sink, which
//!   never drops an event (section 10).
//! - [`journal`]: the segment format and the journal's identity, the file operations (and a
//!   simulated disk for the crash tests), group commit and the durable watermark, and crash
//!   recovery (section 11).
//! - [`gate`]: the release rule, every latency and count of a run, the fund's equity, and
//!   the capture (sections 12, 13.2, 15.4 to 15.9).
//! - [`replay`]: the journal alone rebuilds the engine and the nonce table, and the replay
//!   test (13.1, 13.3).
//! - [`run`]: `Pipeline`: start on a new journal or resumed after a crash, the counters,
//!   and join (2.8, 13.5).
//!
//! **Restarting after a crash** (13.5): `journal::recovery::recover` (find the end, check
//! the identity, zero the torn tail, re-write and sync what is kept), then
//! `replay::replay` (the engine, the nonce table, the next seq, the last `ts`), then the
//! gateways with the nonce table, and `Pipeline::start` with a `run::Resume` and a clock
//! anchored above the last `ts`. Nothing is derived from bytes that are not on disk.

pub mod affinity;
pub mod clock;
pub mod codec;
pub mod core_thread;
pub mod counters;
pub mod crc32c;
pub mod gate;
pub mod histogram;
pub mod idle;
pub mod journal;
pub mod panic;
pub mod records;
pub mod replay;
pub mod ring;
pub mod run;
pub mod sequencer;
