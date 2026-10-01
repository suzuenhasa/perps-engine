# Pipeline specification (Milestone 3)

Status: **built** (commit 38df759, 2026-09-30) and **revised after the review of the
build** the same day (section 22, "Review of the build"); the spec was drafted 2026-09-29 and
revised after three reviews (durability and determinism: F1 to F15; security: S1 to S12;
measurement: M3-*). Section 22 logs every finding and what changed. Decisions:
`docs/DECISIONS.md` D-021 to D-034. The owner answered section 21's questions on
2026-09-30, and the same day approved Bitcoin Core's libsecp256k1 as an opt-in second
verifier, built after its review (5.7, D-032), an opt-in second signing scheme that
matches Polymarket Perps' (EIP-712 over keccak-256 of MessagePack, a salt and a
millisecond timestamp, the signer recovered), built the same day (5.8, D-033), and a
second synthetic flow shaped like Polymarket Perps' recorded traffic, with three stress
switches, also built that day (14.12, D-034). Headline numbers are pending the session on
PERPSBOX (`docs/RUNBOOK-PERPSBOX.md`).

This is the exact specification of Milestone 3: everything that turns signed client
messages into durable, released engine results; the load generator that produces those
messages; the harness that measures them; and the replay test that proves the journal
alone rebuilds the run. It is written so that an engineer can implement it without
guessing. Byte layouts are exact and little-endian unless a field says otherwise.

**Fixed by the owner on 2026-09-29 (not reopened here):**
1. Signatures are secp256k1 ECDSA through RustCrypto's `k256` (pure Rust). Bitcoin Core's
   C library (the rust-bitcoin `secp256k1` crate) is used only if M3's measurements show
   that 100k signed orders/s can't fit on the rented box, and only after its own review.
   The crate named `libsecp256k1` is banned (an abandoned lookalike; `deny.toml`).
   *Changed by the owner on 2026-09-30:* after that review, the C library is built as an
   opt-in second verifier (the `c-secp256k1` feature), to measure it side by side with
   `k256` on the rented box before the headline decides; `k256` stays the default (5.7,
   D-032).
2. Durability: clients see results (acks, fills, cancels, rejects) only after the journal
   batch holding the command is on disk (output gating). The core may run ahead of the
   disk; its events are held until the durable watermark passes them.
3. No new crates for queues, latency histograms or thread pinning. We write them with
   std, plus `libc` (already locked at 0.2.189, reviewed in M0) for pinning. The one
   `unsafe` block, the affinity system call, is one small documented function.

**The library, in one paragraph** (the owner asked). We use RustCrypto's `k256`, pure Rust,
now locked at 0.14.0 (with `ecdsa` 0.17.0, `signature` 3.0.0 and `sha2` 0.11.0) after the
supply-chain review in `docs/SUPPLY-CHAIN.md`. The crate named `libsecp256k1` is the
abandoned lookalike: we never used it, and `deny.toml` bans it. The fallback is a
different crate with a confusingly similar name, `secp256k1` from rust-bitcoin, which wraps
Bitcoin Core's C library; we would use it only if M3's measurements show that 100k signed
orders/s can't fit on the rented box, and only after its own review. Since 2026-09-30 it
is reviewed and built as an opt-in alternative, off by default (5.7).

Also fixed earlier: headline end-to-end numbers run natively on PERPSBOX (INFO.md 5a);
local runs are for development. The engine (M1, M2) is done and M3 changes nothing it
does: every new line lives in `pipeline/`, `gateway/`, `loadgen/` and `bench/`, except one
additive method the owner authorised on 2026-09-30, `Engine::prefault`, which writes the
engine's reserved memory once before a run so that its pages aren't first touched inside a
measured window, and changes nothing the engine holds or emits (15.4; section 22). With the
EIP-712 scheme (D-033) the engine's `id_hash` module became public, so that the gateways'
salt tables reuse its seeded hash (5.8); that changes what other crates may call, not
anything the engine does (recorded in D-033 for the owner to confirm).

**Sources:** INFO.md sections 3, 4 ("Commands", "Events", "Determinism rules"), 5, 5a, 6
(M3 row), 7, 8 and 10; D-001, D-004, D-005, D-008, D-011, D-016, D-017, D-019, D-020;
`docs/RISK.md` 3.1, 11, 14 and 15; `docs/BENCHMARKS.md` M1 and M2; the engine API in
`engine/src/engine.rs`, `command.rs` and `event.rs`.

**How the numbers were checked.** Every worked number was computed with throwaway Python
(standard library only, in the scratchpad, not in the repo):
- two independently written ECDSA models of secp256k1 with RFC 6979 nonces. Both reproduce
  the published RFC 6979 test vector (private key 1, message "Satoshi Nakamoto":
  `r = 934b1ea1…`, `s = 2442ce9d…`) and agree on the example of 5.6;
- CRC32C, byte at a time and slicing by 8 (equal on 2,000 random inputs), which gives the
  standard check value `0xE3069283` for "123456789", then the CRCs of the records of 11.3;
- the histogram index formula, checked over every bucket up to 2^40 (contiguous, widest
  bucket exactly 1/128 of its lower bound);
- a model of the synthetic flow generator (section 14) for its command mix and rate; a
  model of the high-leverage positions for the liquidations and the fund's result per jump
  (14.3, 14.7); two queueing models of the gateways under that flow's bursts (section 17);
  and a simulation of a single queue (M/D/1) for what "p99 under 50 µs" means (15.7);
- a model of journal recovery (11.8) through 20,000 random runs of "write, crash, recover,
  restart, crash again", with power loss keeping a random subset of the unsynced 512-byte
  sectors. No released record was lost and no record from an earlier life came back. The
  draft's recovery rule, in the same model, accepted stale records (review finding F1).

That is evidence for the spec's arithmetic and rules, not for the Rust code, which gets its
own known-answer, property and crash tests (section 18).

## Contents

1. Scope
2. Threads and data flow
3. Rings
4. Binary encodings of commands and events
5. The signed client message
6. Nonces and replay protection
7. The gateway
8. The operator queue
9. The sequencer
10. The core thread
11. The journal
12. Output gating
13. Replay and the determinism test
14. The load generator
15. Measurement
16. Ablations
17. What 100k signed orders/s needs
18. Tests
19. Code layout and APIs
20. Choices made in this spec
21. Questions for the owner
22. Review log

---

## 1. Scope

In Milestone 3:
- **Gateway threads:** decode, domain and ownership checks, the nonce check, secp256k1
  verification, back-pressure rejects. In the opt-in EIP-712 scheme (5.8), a timestamp
  window and a table of recent requests take the nonce's place, and the gateway recovers
  the signer instead of verifying.
- **The sequencer:** merges the gateways' rings and the operator queue, stamps a sequence
  number and a timestamp, and sends each command to both the journal and the core.
- **The journal writer:** group commit to preallocated files, a CRC per record, and a
  durable watermark.
- **The core thread:** pinned, busy-polling, runs `Engine<Book, Fast>` and writes its
  events to the event ring.
- **The gate:** the event consumer. It releases events only once they are durable, and it
  records every latency measurement.
- **The load generator:** the M3 synthetic flow on 67 markets, pre-signing, an open-loop
  sender, and two injection modes (signed, pre-verified). Since D-034, also a second flow,
  Polymarket Perps' 88 real markets traded with the shape of their recorded traffic, and
  three stress switches: one busy account, bursty arrivals, correlated shocks (14.12).
- **The harness:** runs, sweeps, max-rate searches, ablations and reports.
- **Replay, crash recovery and restart:** the journal alone rebuilds the engine and the
  gateways' nonce state, also after a crash (11.8, 13.5).

Not in M3 (INFO.md 12.6): a network gateway (v1 gateways are in-process threads),
per-account rate limits, signed user withdrawals and leverage changes, private and
market-data feeds, a hot standby, key rotation while running, and batch signatures (named
only as a lever, section 17).

**v1 is not safe for untrusted clients, on purpose.** Every client is the in-process load
generator. Section 7.4 and D-031 list what a network gateway must add first (for example,
a flood of forged messages costs a verification each).

---

## 2. Threads and data flow

### 2.1 The picture

```
 SIGNED MODE
 sender ──ingress[g]──> gateway g ──lane[g]──┐
 (loadgen)  g = 0..N-1    (N threads)         │
    │                                         ├──> sequencer ──journal ring──> journal writer ──> disk
    └───────────── operator ring ────────────┘        │                             │
                                                       │ core ring                   │ durable watermark
 PRE-VERIFIED MODE                                     v                             v (one atomic u64)
 sender ──lane[g]──> (straight into the sequencer)   core thread ──event ring──> gate (event consumer)
                                                     Engine<Book, Fast>          releases events whose
                                                                                 command is durable;
                                                                                 histograms, counters,
                                                                                 capture for replay
```

Every arrow is a bounded single-producer, single-consumer ring (section 3), except the
durable watermark, which is one atomic counter written by the journal writer and read by
the gate.

### 2.2 Threads

| Thread | Count | What it does | When idle | Pinned |
|---|---|---|---|---|
| sender | 1 (option `--senders L`) | sends each pre-built message at its scheduled time | spins until the next send time | yes |
| gateway `g` | `N` (10 on PERPSBOX, 2 locally) | checks and verifies the messages of accounts with `account mod N == g` | spins | yes |
| sequencer | 1 | merges its inputs, stamps, writes to the journal ring and the core ring | spins | yes; its SMT sibling is left idle |
| journal writer | 1 | encodes records, checksums them, writes and `fdatasync`s batches, publishes the watermark | spins; blocks inside `fdatasync` | yes |
| core | 1 | applies each command to `Engine<Book, Fast>`, writes events | spins | yes; its SMT sibling is left idle |
| gate | 1 | releases durable events; histograms, counters, capture | spins | yes |
| main | 1 | starts the threads, polls progress every 1 ms, samples counters at the window's edges, joins the threads, prints the report | sleeps | housekeeping CPU |
| pre-signing pool | `min(allowed CPUs, floor(CPU quota))` | signs every message before the run starts | — | no (it finishes before the other threads start) |

With `L > 1` senders, sender `j` owns the lanes `g` with `g mod L == j`, and sender 0 also
owns the operator ring. One sender is the default: it needs about 50 ns per message, so it
keeps up to well past 1M messages/s; the sender-lag histogram (15.4) says when it doesn't.

Every spinning thread also publishes its **busy time** (the time spent in loop passes that
found work: one extra clock read per non-empty batch) as a counter main can read, so every
run reports each thread's busy share and says what was the limit (15.4). Busy time is CPU
work: the journal writer leaves out the time it is blocked inside `fdatasync`, which it
publishes on its own, with its count of flushes.

### 2.3 Rings

| Ring | Producer → consumer | Slot | Capacity | Memory | When the ring is full |
|---|---|---|---|---|---|
| `ingress[g]` | sender → gateway `g` | 3 lines (19 words) | 1,024 | 192 KiB each | the sender drops the message and counts `IngressFull` |
| `lane[g]` | gateway `g` → sequencer (signed mode); sender → sequencer (pre-verified mode) | 3 lines (20 words) | 1,024 | 192 KiB each | gateway: rejects `Busy`, checked before verifying; places and modifies are refused while 64 or fewer slots are free, cancels only when none is (7.1). Pre-verified sender: drops, `IngressFull` |
| `operator` | sender 0 → sequencer | 1 line (8 words) | 4,096 | 256 KiB | the item stays pending in the sender, which retries on its next pass; operator commands are never dropped and never hold up client sends (14.10) |
| `core` | sequencer → core | 2 lines (12 words) | 16,384 | 2 MiB | the sequencer stops taking input |
| `journal` | sequencer → journal writer | 3 lines (10 or 19 words) | 65,536 | 12 MiB | the sequencer stops taking input |
| `event` | core → gate | 1 line (8 words) | 131,072 | 8 MiB | the core waits inside its event sink |

A line is 64 bytes: eight `u64` words. Capacities are powers of two. Record layouts are in
3.3.

### 2.4 Back-pressure: reject at the edge, wait inside

**At the edge, reject.** The sender is the open-loop clock. If it waited for space, every
later message would be sent late and the delay would vanish from the measurements
(coordinated omission), and one slow gateway would hold up the others. So a full `ingress`
ring drops the message, which is what a full socket buffer does. A gateway whose `lane` is
full rejects with `Busy`, and checks this before spending about 50 µs on verification, so
a gateway whose lane is full sheds load cheaply. Either way the client gets an explicit
answer and nothing is buffered without a limit (INFO.md 5).

What "cheaply" does **not** cover: a well-formed message for a registered account, with a
fresh nonce, a future expiry and a low `s`, passes every cheap check and costs one full
verification even if it is forged, and a forgery doesn't use up its nonce (6.1 rule 6), so
the same bytes can be sent again. v1 is safe from that only because its one client is the
in-process load generator; 7.4 lists what a network gateway needs first.

**Inside, wait.** Once the sequencer gives a command a sequence number, that command must
reach both the journal and the core, or the journal would have a gap. So the sequencer
checks for space in both rings *before* it takes a record, and when there is none it
takes nothing. The pressure then moves back into the lanes, where the gateways reject. The
core can't drop events either: they belong to a command it has already applied. It waits
inside the event sink until the gate frees space, which happens as the journal flushes.

**Operator commands** (marks, deposits, market setup) are the exchange's own inputs and must
not be lost, but the sender must not wait for them either, or every client send behind a
full operator ring would go out late and the run would stop being open-loop. So an
operator item that doesn't fit stays pending in the sender (in order), and the sender
pushes pending items whenever space appears, while client sends stay on schedule (14.10).
The rate is tiny (about 670 a second), so the ring fills only when the sequencer has
stopped taking input altogether.

**The chain when the disk is slow** (INFO.md 5, "If the disk is slow"): the journal ring
and the event ring fill → the core waits in its sink and the sequencer stops taking input
→ the lanes fill → the gateways reject `Busy`. The core never waits on a per-order fsync,
but it does stop once the backlog of not-yet-durable work is full. The stall histograms of
15.4 show when that happens.

### 2.5 Sizing the rings that hold not-yet-durable work

INFO.md 5 asks for the journal buffer to hold `(commit interval + p99.9 fsync) × peak
command rate`, and the event ring that times the events per command.

- `T` = 1 ms (the commit interval, 11.5). `F99.9` = 10 ms, an **assumption** until the
  fsync probe (15.10) measures the real disk. `R_peak` = 2M commands/s, above the core's
  expected maximum (M2 measured the engine alone at about 3.7M/s on the local machine).
- Journal ring: `(1 ms + 10 ms) × 2M/s` = 22,000 records. Capacity 65,536, three times that.
  (The journal writer's own batch buffer holds up to `B` = 4,096 more, 11.5.)
- Event ring: 22,000 commands × 5 slots (about 4 events and one trailer per command in the
  M3 flow; the run reports the real figure) = 110,000 slots. Capacity 131,072.

**Commands with many events.** An IOC sweeping many levels, or a `SetMark` that
liquidates many positions, can emit more events than there are free slots, even more than
the ring holds. The rule: the core's sink publishes what it has written, then waits for
space. The gate releases a command's events only together with its trailer (12.1), so it
holds those first events while the ring still has room: the core is not waiting then, and
will write the rest and the trailer. Only when the ring is full of that one command's
events (it has more than the ring's 131,072 slots) does the gate release them as they come,
since the core can't go on until some are freed. Either way the command becomes durable:
the sequencer published it to the journal ring before the core ring, the journal writer
waits on nothing but the ring and the disk, and so the record reaches the writer and the
disk eventually (9.3). Eventually is enough here; nothing needs the writer to see it first.
So a command of any size gets through: no event is ever dropped or truncated, and there is
no deadlock.

### 2.6 Pinning

Each thread pins itself to one logical CPU at start (19.1, `affinity.rs`). The layout is
computed from the machine's topology, read from `/sys/devices/system/cpu/cpu*/topology/`
(`core_id`, `physical_package_id`) and `/proc/self/status` (`Cpus_allowed_list`), all with
`std::fs`. The run prints the resulting table, and `--cpus role=list` overrides it.

**PERPSBOX** (Xeon E5-2680 v4, 14 cores / 28 threads as probed on 2026-09-28; re-probe on
every rental). Physical core `P0` to `P13`, each with hyperthreads A and B:

| Physical core | Signed mode: A | Signed mode: B | Pre-verified mode: A | Pre-verified mode: B |
|---|---|---|---|---|
| P0 | sender | main thread and the OS (housekeeping) | sender | main and the OS |
| P1 | core | idle | core | idle |
| P2 | sequencer | idle | sequencer | idle |
| P3 | gate | journal writer | gate | idle |
| P4 | gateway 0 | idle (or a gateway with `--gateway-smt`) | journal writer | idle |
| P5 to P13 | gateways 1 to 9 | idle (or gateways with `--gateway-smt`) | idle | idle |

- The core and the sequencer are the serial part of the pipeline, so each gets a whole
  physical core: a busy thread on the SMT sibling would compete for the same execution
  units.
- Gateways get one physical core each by default. Elliptic-curve arithmetic keeps the
  multiply units busy, so an SMT sibling adds little; `--gateway-smt` measures how little
  (15.10, verify scaling).
- **Signed mode:** the gate and the journal writer share P3. At the signed rates (at most
  a few hundred thousand a second, limited by the gateways) the gate needs about 5% of a
  core per 100k/s and the writer about 3% plus its waits in `fdatasync` (section 17).
- **Pre-verified mode** drives the core toward millions of commands a second, and the
  core-path search runs with the journal in discard mode, where the writer never blocks in
  `fdatasync` and spins the whole time. The gateways are unused, so the gate and the writer
  each get a physical core. Otherwise their shared core, not the engine, could be the
  limit that search finds (section 17's budget puts the two together at about 0.8 µs of
  work per command, against about 0.5 µs for the core).
- vast.ai boxes are containers with a CPU quota (about 26.9 CPUs when probed). Every
  spinning thread uses a full CPU of that quota, and going over it makes the kernel throttle
  the whole container for the rest of a 100 ms period. The harness reads the quota
  (`/sys/fs/cgroup/cpu.max` on cgroup v2, `cpu.cfs_quota_us / cpu.cfs_period_us` on v1; `max`
  or `-1` means no quota) and refuses a layout whose spinning threads exceed
  `floor(quota) − 2`: one CPU for main and one for kernel work done on the container's
  behalf. The default signed layout spins 15 threads, the pre-verified one 5. With
  `--gateway-smt` the gateways fill the idle siblings only up to that limit: 19 gateways
  at 26.9 CPUs (24 spinning threads).
- **Throttling is detected, not just avoided.** The harness reads `cpu.stat`
  (`nr_throttled`, `throttled_usec`) before and after every run; a run with any throttling
  is discarded and reported as such (15.7).
- All pipeline threads stay on one CPU package (`physical_package_id`). Main pins itself
  there before it allocates the rings and the engine's memory, so their pages are first
  touched on that package.
- **Quiet CPUs for the serial threads.** The probe (15.10) measures, per physical core,
  the largest gap between consecutive clock reads over 10 s and how many gaps exceed 10 µs
  (other tenants, interrupts). The core and the sequencer go to the two quietest physical
  cores; the table above shows positions, not fixed CPU numbers.
- There is no core isolation in a container: the OS can still run other work on the "idle"
  siblings. This is recorded with every number.

**Local machine** (Ryzen 7 2700X under WSL2; the guest sees 8 logical CPUs as four sibling
pairs (0,1) (2,3) (4,5) (6,7); `./dev` gives the build container CPUs 0 to 6 and keeps CPU 7
for the recorders):

| CPU | Signed mode | Pre-verified mode |
|---|---|---|
| 0 | main + journal writer | main + journal writer |
| 1 | gate | idle (the writer's sibling) |
| 2 | core | core |
| 3 | sequencer | idle (the core's sibling) |
| 4 | gateway 0 | sequencer |
| 5 | gateway 1 | sender |
| 6 | sender | gate |
| 7 | recorders (untouched) | recorders (untouched) |

In signed mode locally the sequencer shares the core's sibling pair and the gate shares
the writer's; WSL2's virtual CPUs float over the host's threads anyway, so local numbers
are for development only.

### 2.7 Life of one signed order (illustrative numbers, not measurements)

Times are nanoseconds since the run started (15.1). `N` = 8 gateways, as in the examples
of 6.5 and 11.3.

| Step | Time | What happens |
|---|---|---|
| scheduled | 1,000,000 | the send schedule says account 9's place goes now (`t_sched`) |
| sent | 1,000,150 | the sender writes it into `ingress[1]` (`t_sent`; account 9 mod 8 = 1) |
| gateway in | 1,000,400 | gateway 1 takes it (`t_gw_in`) |
| gateway out | 1,051,300 | the cheap checks pass (including the expiry, against the clock read at `t_gw_in`), the signature verifies (about 51 µs), nonce used up, written to `lane[1]` (`t_gw_out`) |
| sequenced | 1,051,500 | sequence number 5,000 and a timestamp; written and published to the journal ring, then to the core ring (`t_seq`) |
| core done | 1,051,900 | the engine applied it: `Ack`, then the top-up (`BalanceChanged`, `PositionChanged`); the order rests (`t_done`; core path 0.4 µs) |
| durable | 1,751,600 | the batch holding 5,000 had opened 0.5 ms earlier, so it closed at 1,551,500 (`T` = 1 ms after its oldest record); `write` and `fdatasync` took 0.2 ms; the watermark moves to the batch's last seq |
| released | 1,751,700 | the gate sees the watermark at 5,000 or more and releases the events (`t_release`) |

The signed-order-to-durable-ack latency is `t_release − t_sched` = 751.7 µs. The core
spent 0.4 µs of it.

### 2.8 Stopping, and what a panic does

**Stopping follows the data.** Each ring has a `closed` flag (3.1). A producer closes its
ring when it is done: after its last publish, dropping the producer end closes the ring.
A consumer stops only when its ring is closed **and** empty. So the pipeline stops in
data-flow order, by itself:

1. The sender finishes its plan (or main tells it to stop early) and drops its ends: the
   `ingress` rings and the operator ring close.
2. Each gateway drains its `ingress` ring, then drops its lane: the lane closes.
3. The sequencer drains the operator ring and every lane, then drops its two ends: the
   journal ring and the core ring close.
4. The journal writer drains the journal ring, flushes its last batch at once (whatever
   `T` says), publishes the watermark, and stops. The core drains the core ring, then drops
   its end of the event ring: the event ring closes.
5. The gate drains the event ring. The last events are released as soon as the writer's
   final flush moves the watermark past them. Then the gate stops, and main joins
   everyone.

A single "stop" flag seen by every thread would be wrong: a thread could see it while the
thread upstream is still producing (a gateway in the middle of a 50 µs verification, say),
exit, and leave that record unsequenced or unjournaled.

**Any panic stops the whole process at once.** Rust's default on a panic is to end only the
panicking thread. Here that would be dangerous: if the journal writer died, the watermark
would freeze and the run would hang; if the core died in the middle of a command, the rest
of the pipeline would carry on without it. So `Pipeline::start` (and `replay`, 13.1)
install a panic hook that prints the panic, the thread's name and the `seq` it was working
on (the core and replay keep it in a thread-local; replay also keeps the command), and
then calls `std::process::abort()`. Recovery (11.8) takes over at the next start. (A
`panic = "abort"` release profile would do the same for release builds only; the hook also
covers tests and debug builds.)

**What a panic can leave released.** The other threads run on while the hook prints, so the
gate may release more of what is complete and durable, but nothing of the command that
panicked: the gate releases a command's events only with its trailer (12.1), which that
command never writes. The one exception is a command that had already emitted more events
than the event ring holds (131,072): its first events were released as they came (2.5), so
a client can have seen part of it (12.4).

The engine panics on purpose in a few places, even in release builds, for example when the
book disagrees with the engine's own checks, or a checked `i64` sum overflows (RISK.md 2.4).
A command that makes the engine panic is a **poison pill**: it is already journaled, so
every replay panics at the same `seq`, and the exchange can't restart until the engine is
fixed. Replay's message names that `seq` and the decoded command. The cure is a code fix,
never an edit to the journal (13.1). If the poison pill had emitted more events than the
event ring holds, some of them may have been released (above): the fix must then reproduce
exactly those first events for that `seq`, or clients' view and the journal's disagree.

---

## 3. Rings

### 3.1 The SPSC ring

Our own ring, in safe Rust, in `pipeline/src/ring.rs` (owner decision 3; D-024).

```rust
/// One cache line of eight atomic words. The alignment makes every slot start on a
/// cache line (D-005's promise), so two slots never share a line.
#[repr(align(64))]
pub struct Line(pub [AtomicU64; 8]);

/// A counter alone on its cache lines. 128 bytes, not 64, because Intel's adjacent-line
/// prefetcher fetches lines in pairs, so two counters 64 bytes apart would still
/// interfere.
#[repr(align(128))]
pub struct CachePadded<T>(pub T);

struct Shared<const LINES: usize> {
    slots: Box<[[Line; LINES]]>,    // capacity slots, allocated and pre-touched at creation
    head: CachePadded<AtomicU64>,   // records the consumer has released (written by the consumer)
    tail: CachePadded<AtomicU64>,   // records the producer has published (written by the producer)
    closed: CachePadded<AtomicBool>, // set once by the producer after its last publish
}
```

- `head` and `tail` count records since the ring was created; they never wrap (2^64
  records at a billion a second would take 584 years). The slot of record `i` is
  `i & (capacity − 1)`.
- The ring holds `tail − head` records and has `capacity − (tail − head)` free slots.
- A record is written into its slot one word at a time with relaxed atomic stores and read
  back one word at a time with relaxed atomic loads. On x86 these are plain `mov`
  instructions. Because every word is an atomic, a reader can never see a half-written
  word, and no `unsafe` is needed. The price is that records must be encoded field by field
  into words, which the journal needs anyway (D-005).
- Each side keeps a private copy of the other side's counter (`cached_head` in the
  producer, `cached_tail` in the consumer) and reloads it only when its copy can't satisfy
  what the caller wants (the `wanted` argument below). So in the common case neither side
  touches the other's cache line, and a stale copy never shrinks a batch.
- `Shared` is behind an `Arc` owned by the two handles. The `Arc`'s reference count is
  touched only at creation and drop, never per record. That is the one kind of shared
  state INFO.md 5 allows on the hot path: a lock-free ring.
- **Pre-touched.** `channel` writes every word of every slot once (a nonzero value, then
  zero), so every page of the ring is mapped before the run. Otherwise a zero-initialised
  allocation can come straight from the OS as untouched pages, and the first lap of the
  ring would take page faults inside the measured window, at low rates for many seconds
  (15.4 checks that the hot threads take none).

```rust
/// Panics unless `capacity` is a power of two (the slot index is `i & (capacity − 1)`).
pub fn channel<const LINES: usize>(capacity: usize) -> (Producer<LINES>, Consumer<LINES>);

impl<const LINES: usize> Producer<LINES> {
    /// Free slots. Reloads `head` (Acquire) only if the cached view shows fewer than
    /// `wanted` free.
    pub fn free(&mut self, wanted: usize) -> usize;
    /// Writes one record into the next free slot, not yet visible. Panics if there is no
    /// free slot (callers check `free()` first) or if `words.len() > 8 * LINES`.
    pub fn write(&mut self, words: &[u64]);
    /// Makes every record written so far visible to the consumer: one Release store.
    pub fn publish(&mut self);
}
/// Dropping the producer publishes anything written, then sets `closed` (Release).
impl<const LINES: usize> Drop for Producer<LINES> { /* ... */ }

impl<const LINES: usize> Consumer<LINES> {
    /// Published records not yet read. Reloads `tail` (Acquire) only if the cached view
    /// shows fewer than `wanted`.
    pub fn available(&mut self, wanted: usize) -> usize;
    /// Word `w` of the next unread record, without moving on.
    pub fn peek(&self, w: usize) -> u64;
    /// Copies the next unread record into `out` and moves the read cursor on.
    pub fn read(&mut self, out: &mut [u64]);
    /// Frees every slot read so far: one Release store.
    pub fn release(&mut self);
    /// True once the producer has closed the ring and every record has been read: loads
    /// `closed` (Acquire) first, then reloads `tail` (Acquire) and checks nothing is left.
    pub fn is_finished(&mut self) -> bool;
}
```

Both handles are `Send` and not `Clone`, so each end belongs to exactly one thread at a
time. Handing an end to another thread is a move.

### 3.2 Memory ordering, in plain words

- **Producer:** write the words (Relaxed), then store the new `tail` (Release).
- **Consumer:** load `tail` (Acquire), then read the words (Relaxed).
  The Release/Acquire pair means: everything the producer wrote before publishing is
  visible to the consumer once it sees the new `tail`.
- **Consumer, done:** store the new `head` (Release) after copying the words out.
- **Producer, before reusing a slot:** load `head` (Acquire). So the producer never
  overwrites a slot the consumer is still reading. (Without this pair, the consumer's
  Relaxed word loads could read the next lap's value.)
- **Closing:** the producer stores `tail` (Release) for its last records, then stores
  `closed = true` (Release). The consumer loads `closed` (Acquire) **first**, then reloads
  `tail` (Acquire). If it sees `closed`, it is guaranteed to see the final `tail`, so
  "closed and empty" really means nothing more will come. Checking in the other order could
  miss a last record published between the two loads.

That is the whole argument for the rings: no locks, no compare-and-swap. The concurrency
test (18.3) checks the ring's logic on 10 million records (indexing, wrap-around, full and
empty, closing), but not these orderings: it runs on x86, which never reorders a store
with a store or a load with a load, and where Relaxed and Release/Acquire compile to the
same instructions, so a Release weakened to Relaxed would still pass. The orderings rest on
this argument, checked by review; only a weakly ordered machine (ARM) or a model checker
(loom or Miri, new dependencies) would test them.

**The other shared variables**, all single-writer:
- the durable watermark (11.7): stored with Release after `fdatasync` returns, loaded with
  Acquire by the gate;
- counters that main reads while the run is going: commands released (gate), gateway
  rejects (each gateway), each thread's busy time, the core's event-ring stall time, the
  sequencer's full-ring passes (core ring and journal ring apart), and the journal writer's
  time in `fdatasync` and its flushes. Each is an `AtomicU64` on its
  own 128-byte line, written by one thread and read by main, both Relaxed. Relaxed is
  enough: the counts only grow, main uses them only for progress, barriers and sampling
  at the window's edges, and a barrier waits until they add up to a known total, which a
  stale read can only delay, never fake (each stale value is at or below the true one).

### 3.3 Record layouts

Word numbers are within the record. `t_*` values are nanoseconds since the run started
(15.1); 0 means "not applicable" (every real stamp is later than the run's start). The
**meta word** packs: bits 0-7 `source` (1 signed client, 2 pre-verified client, 3
operator), bits 8-15 zero, bits 16-31 `lane`, bits 32-63 `account`.

**IngressSlot** (sender → gateway), 19 words:

| Word | Content |
|---|---|
| 0-16 | the 136-byte client message (section 5); word `k` = bytes `8k..8k+8` read as a little-endian `u64` |
| 17 | `t_sched` |
| 18 | `t_sent` |

**ClientRecord** (`lane[g]`: gateway → sequencer, or pre-verified sender → sequencer), 20
words:

| Word | Content |
|---|---|
| 0 | meta: source 1 or 2, lane `g`, account |
| 1 | nonce |
| 2-6 | the command, as the five words of its 40-byte encoding (4.2) |
| 7 | `expires_at` (0 in pre-verified mode) |
| 8-15 | the signature: message bytes 72..136 as eight words (all zero in pre-verified mode) |
| 16 | `t_sched` |
| 17 | `t_sent` |
| 18 | `t_gw_in` (0 in pre-verified mode) |
| 19 | `t_gw_out` (0 in pre-verified mode) |

**OperatorRecord** (sender 0 → sequencer), 8 words:

| Word | Content |
|---|---|
| 0 | meta: source 3, lane 0, account 0 |
| 1-5 | the command (4.2) |
| 6 | `t_sched` |
| 7 | `t_sent` |

**CoreRecord** (sequencer → core), 12 words:

| Word | Content |
|---|---|
| 0 | `seq` |
| 1 | meta (as received) |
| 2-6 | the command (4.2) |
| 7 | `t_sched` |
| 8 | `t_sent` |
| 9 | `t_gw_in` |
| 10 | `t_gw_out` |
| 11 | `t_seq` |

With `--stamps off` (15.1, a one-off measurement of what the stamps cost), the sequencer
writes only words 0 to 6 and the core reads only those, so the record touches one cache
line instead of two.

In the "verify on core" ablation (section 16), both arms use a 3-line core record: the 12
words above, then the nonce, `expires_at` and the 8 signature words (22 words), so the
record's size is not a hidden difference between the arms.

**JournalRecord** (sequencer → journal writer): exactly the on-disk record of 11.3, as 10
words (80 bytes) or 19 words (152 bytes), with the CRC field left 0 for the writer to fill.

**EventSlot** (core → gate), 8 words:

| Word | Content |
|---|---|
| 0 | `seq` of the command that produced it |
| 1-7 | an engine event in its 56-byte encoding (4.3), or a trailer (10.3) |

---

## 4. Binary encodings of commands and events

### 4.1 Conventions

- One encoding for commands (40 bytes, "CMD40") and one for events (56 bytes, "EVT56"),
  used everywhere: in client messages, in every ring, in the journal and in event captures.
  They live in `pipeline/src/codec.rs`, not in the engine, which stays untouched.
- The encodings are field by field, never the `repr(C)` memory of the structs, whose
  padding bytes are uninitialized (D-005).
- Integers are little-endian. `i64`/`u64` fields take 8 bytes. A `u32` or `i32` in an
  8-byte field takes its low 4 bytes and the other 4 are zero; where two 4-byte values share
  8 bytes, the table says so.
- Tags start at 1, so a zeroed buffer is never a valid record.
- Enum codes are the declaration order in the engine (all `#[repr(u8)]` with implicit
  discriminants): `Side` Buy 0, Sell 1; `TimeInForce` Gtc 0, Ioc 1; `CancelReason`
  UserRequested 0, IocRemainder 1, SelfTrade 2, Liquidation 3, SizeBelowFilled 4,
  PriceBand 5; `RejectReason` InvalidPrice 0, InvalidQty 1, Duplicate 2, UnknownOrder 3,
  PostOnlyWouldCross 4, UnknownMarket 5, NoMark 6, PriceBand 7, MarginCall 8,
  InsufficientMargin 9, InsufficientBalance 10, MarketNotEmpty 11, InvalidLeverage 12,
  ReservedAccount 13, SizeLimit 14, InvalidAmount 15, InvalidParams 16,
  WithdrawalReserve 17, NoRiskTiers 18. The codec converts with explicit `match`es, and a
  unit test pins every code, so reordering an engine enum breaks a test instead of the
  journal.

### 4.2 Command encoding (CMD40)

Bytes 0-7 are the head: `[0]` tag, `[1]` field a, `[2]` field b, `[3]` field c, `[4..6]`
market (`u16`), `[6..8]` field x (`u16`). Bytes 8-39 are four 8-byte fields f1 to f4. A
dash means the bytes must be zero.

| Tag | Command | a | b | c | market | x | f1 (8..16) | f2 (16..24) | f3 (24..32) | f4 (32..40) |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | `PlaceOrder` | side | tif | post_only (0/1) | market | – | order_id | price | qty | – |
| 2 | `CancelOrder` | – | – | – | market | – | order_id | – | – | – |
| 3 | `ModifyOrder` | – | – | – | market | – | order_id | new_price | new_size | – |
| 4 | `Deposit` | – | – | – | – | – | amount | account (`u32`) | – | – |
| 5 | `Withdraw` | – | – | – | – | – | amount | account (`u32`) | – | – |
| 6 | `SetLeverage` | – | – | – | market | leverage | account (`u32`) | – | – | – |
| 7 | `SetMark` | – | – | – | market | – | price | – | – | – |
| 8 | `SetMarketParams` | – | – | – | market | max_leverage | min_price | max_price | maker_fee_ppm (`i32`, bytes 24..28), taker_fee_ppm (`i32`, bytes 28..32) | price_band_ppm (`u32`, bytes 32..36; 36..40 zero) |
| 9 | `SetRiskTier` | index | count | – | market | max_leverage | lower_bound | – | – | – |

As five words: word 0 = `tag | a << 8 | b << 16 | c << 24 | market << 32 | x << 48`, and
words 1 to 4 are f1 to f4.

### 4.3 Event encoding (EVT56)

The same head layout, then six 8-byte fields f1 to f6 (bytes 8 to 55).

| Tag | Event | a | b | market | x | f1 | f2 | f3 | f4 | f5 | f6 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | `Ack` | – | – | – | – | order_id | – | – | – | – | – |
| 2 | `Reject` | reason | – | – | – | order_id | account (`u32`) | – | – | – | – |
| 3 | `Fill` | taker_side | – | market | – | maker_order | taker_order | price | qty | maker_fee | taker_fee |
| 4 | `Cancelled` | reason | side | market | – | order_id | remaining | – | – | – | – |
| 5 | `Modified` | – | – | market | – | order_id | price | qty | – | – | – |
| 6 | `PositionChanged` | – | – | market | – | position | cost_basis | locked | account (`u32`) | – | – |
| 7 | `BalanceChanged` | – | – | – | – | free | account (`u32`) | – | – | – | – |
| 8 | `MarkPrice` | – | – | market | – | price | – | – | – | – | – |
| 9 | `Liquidation` | – | – | market | – | position | account (`u32`) | – | – | – | – |
| 10 | `InsuranceAbsorb` | – | – | market | – | position | cost_basis | collateral | – | – | – |
| 11 | `InsuranceShortfall` | – | – | – | – | uncovered | – | – | – | – | – |
| 12 | `LeverageSet` | – | – | market | leverage | account (`u32`) | – | – | – | – | – |
| 13 | `MarketParamsSet` | – | – | market | max_leverage | min_price | max_price | fees as in CMD40 f3 | band as in CMD40 f4 | – | – |
| 14 | `RiskTierSet` | index | count | market | max_leverage | lower_bound | – | – | – | – | – |

Byte 3 of the head is always zero for events. Tag 255 is reserved for the pipeline's
trailer (10.3), which is never an engine event.

### 4.4 Decoding rules and canonical form

- `decode_command(&[u64; 5]) -> Result<Command, DecodeError>` and
  `decode_event(&[u64; 7]) -> Result<Event, DecodeError>`. Decoding checks the tag, that
  every byte marked "–" is zero, and that every enum code and `post_only` is in range.
  It does **not** check business rules (a quantity of 0 decodes fine; the engine rejects
  it): the codec only guarantees that the bytes name exactly one command.
- **Canonical:** `decode(encode(x)) == x` for every command and event, and for every
  40-byte (or 56-byte) input that decodes, `encode(decode(b)) == b`. So there is exactly one
  byte string per command, which is what lets the journal store a decoded command and
  still rebuild the exact bytes a client signed (13.4). Property-tested (18.2).

---

## 5. The signed client message

### 5.1 Layout

Every client message is exactly 136 bytes (17 words): a 72-byte signed part and a 64-byte
signature.

| Offset | Size | Field | Rule (gateway reject reason, section 7) |
|---|---|---|---|
| 0 | 4 | magic, ASCII `PERP` (`50 45 52 50`) | else `WrongDomain` |
| 4 | 2 | protocol version, `u16` = 1 | else `WrongDomain` |
| 6 | 2 | reserved | must be 0, else `Malformed` |
| 8 | 4 | deployment id, `u32` | must equal the gateway's configured id, else `WrongDomain` |
| 12 | 4 | account, `u32` (the signer) | see 5.5 and section 7 |
| 16 | 8 | nonce, `u64` | above the account's last nonce, else `StaleNonce`; at most 2^32 above it, else `NonceJump` (section 6) |
| 24 | 8 | `expires_at`, `u64`: nanoseconds since the UNIX epoch | the gateway's clock must not be past it, else `Expired` |
| 32 | 40 | the command, CMD40 (4.2) | must decode, else `Malformed`; tag 1, 2 or 3 only, else `OperatorOnly` |
| 72 | 32 | `r`, 256-bit big-endian | `1 <= r < n`, else `BadSignature` |
| 104 | 32 | `s`, 256-bit big-endian | `1 <= s <= floor(n / 2)`, else `HighS` or `BadSignature` |

`n` is the order of the secp256k1 group:
`n = FFFFFFFF FFFFFFFF FFFFFFFF FFFFFFFE BAAEDCE6 AF48A03B BFD25E8C D0364141`, and
`floor(n / 2) = 7FFFFFFF FFFFFFFF FFFFFFFF FFFFFFFF 5D576E73 57A4501D DFE92F46 681B20A0`.

`r` and `s` are big-endian because that is how SEC1 and every secp256k1 library write
them. Every integer of our own is little-endian.

**The domain separator** is bytes 0 to 11: protocol name, version and deployment id. The
message type is byte 32, the command's tag. All of them are inside the signed bytes, so a
signature made for one deployment, protocol version or message type can't be moved to
another (6.5 works through the attacks).

**A deployment id names one journal history.** A new, emptied or restored journal gets a
new deployment id (and so a new `keys.txt` and new signed bytes for every client), never
the old one. Nonces live only in the journal (6.3): an empty journal under an old id would
reset every account's nonce to 0 and make every message ever signed for it valid again.
Benchmark deployment ids are never used for anything else; the benchmark reuses its
pre-signed messages across fresh journals (14.8) only because its keys are test keys.

**The expiry.** `expires_at` bounds how long a signed message stays usable. Without it, a
message the gateway never forwarded (dropped, `Busy`) stays valid until the account uses a
higher nonce, and anyone who kept its bytes (a relay, a log) could get it executed later,
at a moment of their choosing, for example a day later when the market has moved but the
old price is still inside the band. The gateway checks it against its own clock, read when
it takes the message (`t_gw_in`): a message is never accepted once that clock is past its
expiry, and forwarding follows within one verification (about 50 µs, longer if the
gateway's thread is preempted). So a message taken just before its expiry can be forwarded
just after it; a bound of days, which is what attack 9 needs, is unaffected. It does not
promise when the order executes either: a message forwarded just before its expiry is
sequenced microseconds later. The engine never sees it; it is journaled only so that the
signed bytes can be rebuilt (11.3). The benchmark's pre-signed messages use
`expires_at = u64::MAX`, so the comparison still runs on every message and is measured.

Two client rules go with it (they belong in any client library):
- never sign two different messages with the same nonce;
- to kill every outstanding message with a nonce up to `k`, send any accepted message with
  a nonce above `k`. The cheapest is a cancel of an order id you never used: the engine
  rejects it with `UnknownOrder`, but it is journaled and its nonce is used up.

### 5.2 What is signed, the hash, the signature

- **Signed bytes:** bytes 0 to 71, exactly as sent.
- **Digest:** `SHA-256(bytes 0..72)`. SHA-256 is what `k256`'s ECDSA uses by default
  (`Signer::sign` and `Verifier::verify` hash the message with SHA-256 internally), so the
  gateway calls `verifying_key.verify(&msg[0..72], &signature)` and never handles the
  digest itself. (The optional libsecp256k1 verifier takes the digest, so for it the
  gateway computes this same SHA-256 itself, 5.7.) SHA-256 pads a message to whole 64-byte blocks with at least 9 extra
  bytes, so 72 bytes take two blocks, the same as 64 would (up to 119 bytes fit in two):
  two compression rounds, well under a microsecond.
- **Signature:** ECDSA over secp256k1, 64 bytes `r || s`, as `Signature::from_slice` and
  `to_bytes` read and write it. `k256` signs deterministically (RFC 6979 nonces), so the
  same key and message always give the same signature, which makes pre-signed runs
  reproducible byte for byte.
- **Why not keccak and EIP-712?** Ethereum-style systems, including Polymarket's CLOB,
  sign orders as EIP-712 typed data hashed with keccak-256, so that wallets can display and
  sign them. v1 has no wallets in the loop. Keccak would add the `sha3` crate and a typed-
  data encoder for no benefit today. Switching later changes only the digest function and
  the domain bytes, not the pipeline. *Since 2026-09-30:* an opt-in second scheme does
  this the way Polymarket Perps does, with our own keccak-256 and MessagePack writer (no
  crate; 5.8, D-033). The pipeline didn't change; the replay rule did (a salt and a
  timestamp instead of the nonce), and the gateway recovers the signer.

**The `k256` API this spec uses**, checked on 2026-09-29 against the locked sources
(`k256` 0.14.0, `ecdsa` 0.17.0, `signature` 3.0.0, read from the cargo cache inside the
container):
- `k256::ecdsa::{Signature, SigningKey, VerifyingKey}` and
  `k256::ecdsa::signature::{Signer, Verifier}`;
- `Signature::from_slice(&[u8])` (checks `0 < r, s < n`) and `Signature::to_bytes()`;
- `VerifyingKey::from_sec1_bytes(&[u8])` to load a registered key, and
  `verifying_key().to_sec1_point(true)` for its 33-byte compressed form (0.13 called this
  `to_encoded_point`);
- `SigningKey::from_bytes(&FieldBytes)` for a derived private key (14.8);
- `k256::sha2::Sha256` (re-exported by `k256`) for the key derivation and the registry
  digest, so no crate is added for SHA-256.

For secp256k1, `k256` sets `EcdsaCurve::NORMALIZE_S = true`: signing always returns the
low-S form, and verification refuses a high-S signature. Our own check (5.3) runs first
anyway. The features are the supply-chain session's (`default-features = false`, `ecdsa`
only; `docs/SUPPLY-CHAIN.md`).

### 5.3 Low-S and malleability

For any valid signature `(r, s)`, `(r, n − s)` is also mathematically valid for the same
message and key. So without a rule, anyone can turn one valid signed message into a second,
different-looking one ("malleability"). v1 accepts only the low form, `s <= floor(n / 2)`
(as Bitcoin's BIP 62/146 rule does):

- The gateway checks it itself, by comparing the 32 bytes of `s` with the constant
  `floor(n / 2)` as big-endian byte arrays (a lexicographic comparison of equal-length
  big-endian numbers is a numeric comparison), and rejects with its own reason, `HighS`.
  `k256`'s verifier also refuses high-S signatures, but the rule should not depend on a
  library detail; the unit test checks that our check fires first.
- `k256` produces low-S signatures when signing. The loadgen checks every signature it
  makes against the same constant (a debug assertion).
- **What low-S guarantees, and what it doesn't.** A third party can't turn a valid message
  into a second valid encoding: their only transform, `s → n − s`, is rejected. The key
  holder can always make more: signing the same bytes with a different ECDSA nonce `k`
  (any signer that doesn't use RFC 6979) gives a different valid signature. So the
  signature bytes are **not** a message id. The id of a signed message is
  `(deployment, account, nonce)`, which the nonce rule makes unique in the journal.

### 5.4 Keys and the registry

- Each account has one secp256k1 key pair. The public key is registered; the private key
  stays with the client (in v1, the load generator).
- **Keys are made for this protocol only**, never reused from a wallet or another system.
  The digest is plain SHA-256 of our bytes, so any signer that will sign an arbitrary
  32-byte hash with the same key (a legacy wallet's raw-hash signing, a key service that
  signs digests) could be tricked into signing a valid order it can't display. Polymarket
  users already hold Ethereum secp256k1 keys, so this matters the day wallets appear; then
  the answer is EIP-712 (D-021, "What would change it"), which wallets display and never
  confuse with a raw hash. The opt-in scheme of 5.8 signs EIP-712 digests with the same
  registered keys and the same `keys.txt`.
- **The registry is gateway state.** It is loaded once when the gateways start, from a
  text file, `keys.txt`:

  ```
  # perps key registry v1
  deployment 1
  1 <66 hex digits>
  2 <66 hex digits>
  ...
  9 03872ba80a104a5c56609998c3da5e267e380deffe94d6e3d024969cb2e0efb729
  ...
  ```

  One line per account: the account id in decimal, a space, and the public key as a
  33-byte compressed SEC1 point in 66 hex digits. Lines are sorted by account with no
  duplicates. Loading rejects the file if the deployment differs from the gateway's, if a
  key doesn't parse as a point on the curve (`VerifyingKey::from_sec1_bytes`, or
  libsecp256k1's parser with the optional verifier, 5.7), or if the file lists account
  4,294,967,295 (the insurance fund, `FUND`, which can't trade). It also
  rejects any other spelling of the same keys: the file must be byte for byte what the
  writer produces for them (`\n` line endings, a newline after the last line, lower-case
  hex), so one set of keys has one file and one digest, and an auditor who rebuilds the file
  from the keys the clients confirm gets the digest the journal carries (13.4).
- Parsing a compressed key costs a square root in the field, so it is done once at load,
  not per message: each gateway keeps a `HashMap<AccountId, AccountState>` holding the
  parsed key (`k256`'s `VerifyingKey`, or libsecp256k1's with the optional verifier, 5.7)
  and the account's last nonce, for the accounts with
  `account mod N == g` only. std's `HashMap` with its default, randomly keyed hasher is
  fine here: only registered accounts are ever inserted, so an attacker can't choose keys
  to collide, and the lookup (about 20 ns) is nothing next to a 50 µs verification. Keep
  it that way: every account on gateway `g` has `id ≡ g (mod N)`, so with an identity or
  Fx-style hasher and a power-of-two `N`, all of them would share their low bits.
- **The engine never sees keys**, and keys are not journaled: replay doesn't verify
  signatures to rebuild state (13.1). Each journal segment's header records the SHA-256 of
  the `keys.txt` its writer loaded (11.2), so an auditor can check which registry file
  applied to which records.
- In v1 the file is written by the load generator from its seed (14.8). **Keys change only
  at a restart**: to replace a leaked key, stop, edit `keys.txt`, restart. The journal
  continues (the new registry's digest goes into the new segment's header), but every
  account is offline meanwhile. Adding or revoking keys while running is "Later" (a control
  ring from main to each gateway, each change journaled as an operator record; D-031).

### 5.5 Ownership

`PlaceOrder`, `CancelOrder` and `ModifyOrder` name the owning account inside the order id
(`account_of(order_id)`, the high 32 bits; D-005). The gateway checks, in order:
1. `header.account == account_of(command.order_id)`, else `NotOwner`;
2. the signature verifies with the registered key of `header.account`, else
   `BadSignature`.

Together: only the holder of account A's key can place, cancel or modify an order whose
id says A. The engine does not check ownership (RISK.md 6.2): it trusts the gateway.
The explicit `account` field is redundant with the order id, but it lets the gateway route
and look up the key from a fixed offset for every message type, and it gives a clear
reject reason.

### 5.6 Worked example

Account 9, deployment 1, nonce 1, placing a post-only GTC bid on market 3: order
`order_id(9, 1)` = `0x0000_0009_0000_0001` = 38,654,705,665, price 102,998 ticks, 500,000
lots, valid until `expires_at` = 1,790,000,030,000,000,000 ns (30 s after the journal
timestamp of 11.3's example). (Account 9 is market maker 0 of market 3 in the M3 flow,
quoting 2 ticks under the starting fair value of 103,000; section 14. The flow itself
signs with `expires_at = u64::MAX`; this example shows a real expiry.)

Signed bytes (hex, 16 per row):

```
  0: 50 45 52 50 01 00 00 00 01 00 00 00 09 00 00 00    "PERP", version 1, reserved, deployment 1, account 9
 16: 01 00 00 00 00 00 00 00 00 ac 16 20 8b 5b d7 18    nonce 1 | expires_at
 32: 01 00 00 01 03 00 00 00 01 00 00 00 09 00 00 00    tag 1, side Buy, tif GTC, post_only 1, market 3, x 0 | order_id
 48: 56 92 01 00 00 00 00 00 20 a1 07 00 00 00 00 00    price 102,998 | qty 500,000
 64: 00 00 00 00 00 00 00 00                            f4 = 0
```

- SHA-256 of these 72 bytes:
  `1b3977db461c6b73508894a9d1075fbb2ff90a9c6013d18a1718138390a8f19d`.
- Account 9's private key in the M3 flow with seed 1 (14.8):
  `af3ac022da885faa582f5ec5f871ed9d0868a43a7bf5ddc0268209ca743cea37`; its registered
  public key:
  `03872ba80a104a5c56609998c3da5e267e380deffe94d6e3d024969cb2e0efb729`.
- The RFC 6979 signature:
  `r = 962936e1f02f1c3022212df5bb4c09034bd81fb49f5aca9e04d826e8f06c1dd5`,
  `s = 753521c3a0eb5f3a8ae85e6ecfad672bf6543d718e169e076d2c1fd8cb607010`
  (low-S: it starts with `75`, below `7f`. The raw ECDSA result was the high form; the
  signer normalised it, as `k256` does).
- Its high-S twin, `n − s` =
  `8acade3c5f14a0c57517a191305298d2c45a9f752132023452a63eb404d5d131`, also satisfies the
  ECDSA equation and must be rejected with `HighS`.
- Flipping any bit of the signed bytes makes the signature invalid (the models confirm it
  for the deployment byte at offset 8 and an `expires_at` byte at offset 24). The gateway
  may report an earlier check first: for the deployment byte, `WrongDomain`, since the
  deployment then differs from its own.

These values become known-answer tests (18.1). They were computed with the two Python
models described at the top; if `k256` disagrees, one of them is wrong and must be found,
not the constant replaced.

### 5.7 An optional second verifier: Bitcoin Core's libsecp256k1

On 2026-09-30 the owner approved Bitcoin Core's libsecp256k1, through rust-bitcoin's
`secp256k1` crate (0.33.1, with `secp256k1-sys` 0.14.1, which vendors libsecp256k1
v0.8.0), as an **opt-in** second verifier, to measure it side by side with `k256` on
PERPSBOX (D-032; its review: `docs/SUPPLY-CHAIN.md`). `k256` stays the default and the
only verifier of the default build. (`libsecp256k1` here always means Bitcoin Core's
library; the crates.io crate of that name is the banned lookalike.)

- **Opt-in at build time.** Only a build with the gateway's `c-secp256k1` feature (the
  bench crate's feature of that name turns it on) compiles the C. A binary without it
  refuses `--verifier libsecp256k1` at once, and `KeyRegistry::load` refuses it too, before
  reading the file: "the libsecp256k1 verifier is not in this build: build with
  `--features c-secp256k1`".
- **Chosen at start, per run.** `e2e ... --verifier k256|libsecp256k1` (default `k256`).
  The registry is loaded for that verifier: each key is parsed once, by that library, and
  every verification in the run uses it: the gateways, the core in the "verify on core"
  ablation (16), and the audit (13.4). Every summary records `run.verifier` (`none` in
  pre-verified runs, which verify nothing), so a session never mixes the two, and the
  reports print it. The probe measures `t_v` and the verify-scaling curve for every
  verifier the binary has (15.10).
- **Nothing else changes:** the message, the signed bytes, the cheap checks and their
  order (7.1), our own low-S check (check 11, before either library is called), the
  nonces, the journal. Both verify the same equation: libsecp256k1 takes a 32-byte digest,
  so the gateway computes SHA-256 of bytes 0..72 with `k256::sha2` and passes it, which is
  the hash `k256` computes inside. Both refuse `r` or `s` equal to 0 or not below `n`, and a
  high `s` on their own, so the reject reasons don't depend on the verifier.
- **The cross-check** (`gateway/src/verifier.rs`, in a build with the feature): for 240
  messages under 40 deterministic keys, both accept what `k256` signs, and both refuse the
  same cases: one flipped bit in the signed bytes, in `r`, in `s`; another account's key;
  `r` or `s` equal to 0, `n`, `n + 1` or `2^256 − 1`; the high-S twin, at the verifier
  itself (below our low-S check); and the fixed vector of 5.6 (its bytes, key, `r` and
  `s`; its twin; its deployment and expiry bytes flipped). Both parse the same keys and
  refuse the same bad ones. In that build every other gateway test, the signed pipeline
  test and a signed smoke run (18.4) run with libsecp256k1 verifying.
- **No context object per gateway.** libsecp256k1's C calls take a "context". In
  `secp256k1` 0.33 the verification call takes none (`ecdsa::verify` supplies its own; the
  deprecated `Secp256k1::verify_ecdsa` ignores the context it is called on), so a gateway
  holds nothing for it. The workspace turns on the crate's `std` feature (2026-09-30), so
  the crate keeps one context per thread, allocated on the thread's first verification;
  every thread that verifies during a run calls `verifier::prepare_this_thread` first (the
  gateways at start, the core in the verify-on-core arm), so that allocation happens
  before the timed flow. Without `std` the crate rebuilt a context from a global copy
  behind a spinlock on every call: 2.65 µs of each verification, which
  `libsecp256k1/per_call_context` in `pipeline_parts` now measures at about 4 ns
  (section 22).

### 5.8 A second scheme matching Polymarket Perps (EIP-712)

On 2026-09-30 the owner approved an opt-in second signing scheme (D-033): the one
Polymarket Perps uses for every order, cancel and modify. It exists to measure whether our
pipeline reaches its signed rate under Polymarket's own design. The scheme of 5.1 to 5.7
and section 6, called `perp`, stays the default. Three things change: what a client signs,
how the gateway knows who signed, and what stops a replay. The rings, the lanes, the
sequencer, the journal's record sizes and the engine stay as they are. Code:
`gateway/src/eip712.rs`, `keccak.rs`, `msgpack.rs` and `salts.rs`, and the version-2 parts
of `wire.rs`, `check.rs`, `verifier.rs` and `audit.rs`.

**Chosen at start, per run.** `e2e ... --auth perp|eip712` (default `perp`), with either
verifier (`--verifier k256|libsecp256k1`, 5.7).
- A gateway checks one scheme, chosen when it is made (`Gateway::new` or
  `Gateway::new_eip712`).
- Every summary records `run.auth` (`none` in pre-verified runs, which sign nothing). A
  summary written before the scheme existed reads as `perp` if its run was signed.
- EIP-712 runs are named with `-eip712` (`signed-100k-eip712`), and `run.auth` is part of
  the fingerprint (15.5), so a session never reuses a run of the other scheme. A session
  may hold both schemes' runs; its report gives each scheme its own headline.
- The journal's header records the scheme (byte 120, 11.2).
- A sweep, a search or an ablation applies the scheme to its signed runs only. Its
  pre-verified runs sign nothing, so an EIP-712 session can start from a copy of another
  session's pre-verified 20k/s runs (the durable limit, 15.7), as the libsecp256k1 pass
  does.

**What the client signs, step 1: the compact form.** Each command becomes a positional
array, Polymarket's "compact operation":

| Command | Compact form |
|---|---|
| `PlaceOrder` | `["createOrders", [[iid, buy, p, qty, tif, po, c]]]` |
| `CancelOrder` | `["cancelOrdersCOID", [c]]` |
| `ModifyOrder` | `["modifyOrdersCOID", [[c, p, qty]]]` |

| Field | Value | Type |
|---|---|---|
| `iid` | the market | integer |
| `buy` | true for a buy | boolean |
| `p` | the price: ticks as a decimal with `PRICE_DECIMALS` = 2 places | string |
| `qty` | the size: lots as a decimal with `QTY_DECIMALS` = 4 places; for a modify, the new total size (D-008) | string |
| `tif` | `"gtc"` or `"ioc"` | string |
| `po` | the post-only flag, always present | boolean |
| `c` | the order id as 32 lowercase hex digits | string |

- **Absent fields are dropped.** Polymarket's SDKs and server build every field in its
  place, then drop the absent ones wherever they are, rather than write them as nil.
  Their order has two more fields: `ro` (reduce-only, written only when true) and `tr` (a
  trigger). v1 has neither, so both are dropped, and a place has 7 fields. As of
  2026-09-30 the SDKs (py-sdk `to_raw_order`, ts-sdk `toRawPerpsOrder`) add two more slots
  after `tr` when a builder session is set: one reserved, always absent, and a
  builder-attribution pair `[address, feeRate]`, so such an order signs as `[iid, buy, p,
  qty, tif, po, c, [address, feeRate]]`. v1 has no builders, so both are absent and
  dropped like `ro` and `tr`; a gateway that rebuilds the form from the CMD40 could not
  verify a signature over the builder form.
- **`c`.** Polymarket's client order id has 128 bits and ours 64 (D-005), so the first 16
  digits are always 0: `order_id(9, 1)` is `"00000000000000000000000900000001"`.
- **Decimals** are written in minimal form: no trailing zeros, and no point for a whole
  number. So 50 ticks are `"0.5"`, 102,998 ticks `"1029.98"`, 100,000 lots `"10"` and 1
  lot `"0.0001"`. The two decimal counts add up to 6, so one tick times one lot is still
  one micro-dollar (D-004). Polymarket hashes a client's strings exactly as sent: its SDK
  test signs `"100.50"`, trailing zero included. We rebuild the form from the 40-byte
  command, which has one spelling, so a real SDK's signature verifies here only if its
  client wrote that spelling (D-033, "Options considered").
- **The market of a cancel or a modify is not signed.** Polymarket's forms name the order
  only. Our engine routes by `market`, so the CMD40 still carries it, and a copy of a
  signed cancel or modify with another market passes every gateway check, the signer's
  included. So the market is part of the request the gateway accepts once:
  `(account, salt, ts, market)` (the owner's choice, 2026-09-30; D-033, "Trade-offs"),
  and such a copy is another request. The gateway accepts it; the engine rejects it and
  changes nothing: `UnknownMarket` for a market that doesn't exist, `UnknownOrder` for one
  that does. A wrong market can't reach another order, because an order id's sequence is
  unique per account across all markets (the engine's `Duplicate` check, 6.4), so the
  order rests in its own market only. The genuine message is still accepted, whether it
  comes before the copy or after it, and a copy with the same market is the same request
  (`ReusedRequest`). So whoever holds the signed bytes of a cancel or a modify can make the
  engine reject up to one junk copy per market id (the other 65,535 values of a `u16`,
  while the `ts` is in the window), but can never make the genuine message
  `ReusedRequest`, and never reach another order (attack 7 below). Each copy is an
  accepted message all the same: it costs a recovery and takes a slot in the salt table, a
  lane slot and a journal record, so a flood of copies could fill a table sized for the
  honest requests only (`SaltTableFull`); a network gateway's per-session limits would
  bound that (7.4), and v1's one client is in process, so nobody else holds its signed
  bytes. The audit keys its check the same way, so a journal with a copy next to the
  genuine record passes; it still can't see a market changed in the journal after the
  fact (13.4). Before that choice the key was `(account, salt, ts)`, and one such copy,
  sent before the genuine message, used the request up, so the genuine one got
  `ReusedRequest` (section 22, R2 and the entry after it). A place's market is signed: a
  copy of a place with another market is `WrongSigner`, and takes no slot.
- **Operator commands** (tags 4 to 9) have no form. Clients never send them
  (`OperatorOnly`).

**Step 2: MessagePack, then keccak-256.** `data = keccak256(MessagePack(compact form))`.
The bytes must be exactly the ones Polymarket's SDKs write (`@msgpack/msgpack` in
TypeScript, `msgpack.packb` in Python), which always take the smallest form of each value.
Our own writer (`gateway/src/msgpack.rs`, no crate, into a stack buffer) writes these forms,
from the MessagePack specification:

| Value | Form | Bytes |
|---|---|---|
| array of `n` < 16 values | fixarray | `0x90 + n` |
| array of `n` < 65,536 values | array 16 | `0xdc`, `n` as a `u16` |
| string of `n` < 32 bytes | fixstr | `0xa0 + n`, then the bytes |
| string of `n` < 256 bytes | str 8 | `0xd9`, `n` as a `u8`, then the bytes |
| string of `n` < 65,536 bytes | str 16 | `0xda`, `n` as a `u16`, then the bytes |
| integer `v` < 128 | positive fixint | `v` |
| integer `v` < 256, < 65,536, < 2^32, or larger | uint 8, 16, 32 or 64 | `0xcc`, `0xcd`, `0xce` or `0xcf`, then `v` |
| `false`, `true` | | `0xc2`, `0xc3` |

Lengths and integers after the type byte are big-endian. Our three forms need only
fixarrays, fixstr and str 8 (`c` has 32 characters, so it starts `d9 20`), integers up to
uint 16 (a market is a `u16`) and booleans. Nothing else is ever signed: no maps, nil,
negative integers or floats. The longest form is 103 bytes (`MAX_OP_BYTES`: the largest
market id, and a price and a size of `i64::MIN`, 21 characters each). The place of 5.6's
worked example takes 68 bytes, a cancel 53.

**keccak-256** (`gateway/src/keccak.rs`, our own, the owner's choice in D-033). This is
Ethereum's hash: the Keccak sponge over the Keccak-f[1600] permutation (24 rounds over 25
lanes of 64 bits), with a rate of 136 bytes and the padding `0x01 … 0x80`. It is not
NIST's SHA3-256, which differs only in the first padding byte (`0x06`): keccak-256 of
nothing is `c5d24601…`, and SHA3-256 of nothing is `a7ffc6f8…`. A hash takes one
permutation per whole 136-byte block, plus one for the last block. Every hash of this
scheme fits in one block, so a message costs four permutations (`data`, the struct hash,
the digest, and the address below). The domain costs three more, once per gateway.

**Step 3: the EIP-712 digest.** EIP-712 hashes typed data: the type's hash, then each field
as a 32-byte word (a `uint64` or `uint256` big-endian, a `string` as its keccak-256).

| Part | Value |
|---|---|
| domain type | `EIP712Domain(string name,string version,uint256 chainId)`; its keccak-256, `DOMAIN_TYPEHASH`, is `c2f87871…935b6e` |
| domain | name `"Polymarket"`, version `"1"`, chain id = our deployment id. No `verifyingContract` and no salt |
| domain separator | `keccak256(DOMAIN_TYPEHASH \|\| keccak256("Polymarket") \|\| keccak256("1") \|\| chainId)`: 128 bytes, computed once per gateway (`Domain::new`) |
| operation type | `Op(bytes32 data,uint64 salt,uint64 ts)`; its keccak-256, `OP_TYPEHASH`, is `c13e0021…ea2ff5` |
| struct hash | `keccak256(OP_TYPEHASH \|\| data \|\| salt \|\| ts)`: 128 bytes |
| digest | `keccak256(0x19 0x01 \|\| domain separator \|\| struct hash)`: 66 bytes |

`0x19` is a byte that no RLP-encoded Ethereum transaction starts with, and `0x01` is
EIP-712's version. Our deployment id is the chain id, so a signature is good for one
deployment only; Polymarket's production chain id is 137. A test recomputes both type
hashes from their strings.

**Signing.** The client signs the digest itself (it is not hashed again) with secp256k1,
deterministically (RFC 6979) and low-S. It sends `r || s` and the recovery id, 0 or 1: the
parity of the `y` of the point `R` whose `x` is `r`. Ethereum writes it as `v` = 27 + id;
our message carries the id itself. The load generator signs with
`verifier::sign_recoverable` (`k256`'s `sign_prehash_recoverable`). It refuses ids 2 and 3,
which mean that `R`'s `x` was at least `n` (probability about 2^-128) and which Ethereum's
`v` can't express.

**The message: version 2 of the same 136 bytes.** It is still 17 words, so the sender, the
arena, the rings and the lane records are unchanged. Our own integers are little-endian;
`r` and `s` are big-endian, as in 5.1.

| Offset | Size | Field | Rule (gateway reject reason; the checks are in the table below) |
|---|---|---|---|
| 0 | 4 | magic, ASCII `PERP` | else `WrongDomain` |
| 4 | 2 | protocol version, `u16` = 2 | else `WrongDomain`: each scheme's gateway takes only its own version |
| 6 | 1 | the recovery id: 0 or 1 | else `Malformed` (Ethereum's 27 and 28 too) |
| 7 | 1 | reserved | must be 0, else `Malformed` |
| 8 | 4 | deployment id, `u32`: the chain id | must equal the gateway's, else `WrongDomain` |
| 12 | 4 | account, `u32` | routes the message; its registered address must be the signer's |
| 16 | 8 | salt, `u64`, any number the client likes | the request `(account, salt, ts, market)`, the market being the CMD40's, must not have been accepted before, else `ReusedRequest` |
| 24 | 8 | `ts`, `u64`: milliseconds since the UNIX epoch | at most 5 minutes behind the gateway's clock, else `StaleTimestamp`; at most 60 s ahead, else `FutureTimestamp` |
| 32 | 40 | the command, CMD40 (4.2) | must decode, else `Malformed`; tag 1, 2 or 3 only, else `OperatorOnly` |
| 72 | 32 | `r` | a key must come out of the recovery, else `BadSignature` |
| 104 | 32 | `s` | `s <= floor(n / 2)`, else `HighS` |

What is signed is not these bytes but the digest of the command, the salt and `ts` in the
deployment's domain (`DecodedEip712::digest`). The account is not in the digest: the
signer is known by its address. So if the salt, `ts`, the command, the deployment or the
recovery id is changed after signing, the recovery finds another key and the message fails
with `WrongSigner`. A changed account fails earlier, with `NotOwner` (the order id names the
other account), or else at the address comparison.

**Recovery and the address check** (`wire::check_signer`, `VerifierKind::recover`).
ECDSA's `r` is the `x` of a point `R` the signer made, and the recovery id says which of the
two points with that `x` it was. The signer's public key is then `r⁻¹ (s·R − z·G)`, where
`z` is the digest. `k256` computes it with `VerifyingKey::recover_from_prehash`, and
libsecp256k1 with `RecoverableSignature::recover_ecdsa`. That is libsecp256k1's recovery
module, which the `c-secp256k1` feature compiles (`docs/SUPPLY-CHAIN.md`). A key's
Ethereum address is the last 20 bytes of the keccak-256 of its uncompressed `x || y`
(`eip712::address_of`).
- **The registry is the same.** It is the same `keys.txt` (5.4), with the same digest.
  Each EIP-712 gateway derives its accounts' addresses from their registered keys once,
  when it is made.
- **Three outcomes, three reasons.** Our own low-S check runs first (`HighS`). Then the
  recovery: `r` or `s` equal to 0 or not below `n`, or an `r` that is not the `x` of any
  point, gives no key (`BadSignature`). Any other signature gives *some* key, so the
  address decides: `WrongSigner` if it isn't the account's.
- **Our own low-S check matters more here.** Neither library refuses a high `s` when it
  recovers (both do when they verify), and the high-S twin `(r, n − s)` with the other id
  recovers the signer's own key. Without the check, anyone could make a second valid
  encoding of every message. A test shows `k256` doing it.
- **Compare, don't look up.** Polymarket's server looks the recovered address up to find
  the account. Ours compares it with the claimed account's address (D-033): the account
  still routes the message to its gateway (`account mod N`), and the cost is the same.
- **Cost.** A recovery is a verification's double multiplication, plus a square root (to
  find `R` from `r`), plus the hashing. Locally (release, WSL2, Ryzen 7 2700X,
  `pipeline_parts`) the whole signer check takes 90.7 µs with `k256`, against 78.5 µs to
  verify a perp message in the same run, and 46.7 µs with libsecp256k1, against 39.3 µs.
  Of that, the digest takes 1.3 µs and the address's keccak-256 0.41 µs: hashing is about
  1.7 µs of every check. (The first version of `keccak_f` took 3.0 µs a block, about 12 µs
  a check, only because of how its loops were written; section 22.)

**Replay protection: a time window and a table, instead of a nonce.**
- **The window.** `ts` may be at most 5 minutes (`MAX_AGE_MS` = 300,000) behind the
  gateway's clock and at most 60 s (`MAX_AHEAD_MS` = 60,000) ahead of it, both edges
  included. Polymarket refuses a signature "older than 5 minutes", and its sequencer one
  "over five minutes old or one minute ahead". The clock is the one read when the gateway
  takes the message (`start_unix_ns + t_gw_in`, 7.3), in whole milliseconds, rounded down.
- **A request is accepted once.** A request is `(account, salt, ts, market)`: the same
  signed request `(account, salt, ts)` (Polymarket's error for a reuse is
  `signature_already_used`), plus the command's market, so that a copy of a cancel or a
  modify on another market, whose market is not signed, is another request and never uses
  up the genuine one (step 1). The key is not the salt alone: the official SDKs draw 32-bit
  salts at random, and a busy client's salts collide within 5 minutes (the birthday bound).
  Two accounts, one account with the same salt at another `ts`, or the same salt and `ts`
  on another market, make two requests.
- **Used up exactly when accepted,** after the signer was checked, as a nonce is (6.1,
  rules 3, 4 and 6). So a message rejected for any reason, `Busy` included, may be sent
  again until its `ts` leaves the window, and a forgery can neither burn a client's request
  nor take a slot in the table. A copy of a cancel or a modify on another market is no
  forgery: it passes the signer check and takes a slot, as another request, and leaves the
  genuine one's alone (step 1, above; attack 7).
- **The salt table** (`gateway/src/salts.rs`). Each gateway has one, holding the requests
  it accepted. It uses open addressing with linear probing, over a power-of-two number of
  24-byte slots (`salt` and `ts`, 8 bytes each; `account`, 4; `market`, 2; a used flag, 1:
  23 bytes, padded to 24. The market took 2 of the 3 bytes that were padding, so a slot is
  24 bytes, as it was without it): at least twice as many slots as the requests it has room
  for, and never more than half of them used. So a lookup walks a few slots (about 2.5 on
  average when half full) and always ends. The first slot is a seeded hash of all four
  fields of the request: the engine's splitmix64 (`engine::id_hash`, D-011), with a
  seed per gateway drawn from the operating system through std's `RandomState`. So a client
  can't choose salts that pile up on one slot. `find` only looks, and answers
  `ReusedRequest`, `SaltTableFull`, or where the request would go; `insert` runs only after
  check 13 passed. The table is allocated, and every slot written once, on main before the
  gateway threads start (15.4). Nothing allocates after that.
- **Expiry, lazily.** A request whose `ts` has left the window can never pass check 7
  again, so its slot may be reused: an insert takes the first expired slot on its walk, if
  there is one, before the unused slot at the walk's end. An expired slot never ends a
  lookup, and nothing is ever deleted.
- **Full.** An insert that would take an unused slot when half the slots are used is
  refused with `SaltTableFull`, rather than grow the table, which would allocate. A table
  with room for every request a gateway accepts in a run never refuses one, whether or not
  anything expires. The harness gives each gateway's table room for exactly the messages
  the sender will offer it (`runner::requests_per_gateway`), and caps a run's timed flow at
  3 minutes, so a table never fills. The 100k/s headline over 10
  gateways gives each about 650,000 requests: 2^21 slots, 48 MiB, and 480 MiB for all ten.
  A long-lived deployment would need to delete expired requests (backward-shift deletion
  keeps linear probing correct). v1 doesn't.
- **Cost and memory.** A lookup and an insert take 26 ns, a replay's lookup 25 ns
  (`pipeline_parts`, locally). A table needs 48 to 96 bytes per request it has room for,
  against 8 bytes per account for a nonce.

**The check order.** It is the perp scheme's order (7.1), with checks 7 to 9 replaced and a
13th added:

| # | Check | Reject | Cost |
|---|---|---|---|
| 1 | deployment id, magic `PERP`, version 2 | `WrongDomain` | compare |
| 2 | byte 7 zero and recovery id 0 or 1; the CMD40 decodes (4.4) | `Malformed` | decode |
| 3 | command tag is 1, 2 or 3 | `OperatorOnly` | compare |
| 4 | `account mod N == g` | `WrongGateway` | compare |
| 5 | `account_of(order_id) == account` | `NotOwner` | compare |
| 6 | the account is in this gateway's registry | `UnknownAccount` | hash lookup |
| 7 | `ts` at most 5 minutes behind the clock, and at most 60 s ahead | `StaleTimestamp`, `FutureTimestamp` | compare |
| 8 | the request `(account, salt, ts, market)` was not accepted before | `ReusedRequest` | table lookup |
| 9 | the table has room for it | `SaltTableFull` | the same lookup |
| 10 | `lane[g]` has room: more than 64 free slots for a place or a modify, at least 1 for a cancel | `Busy` | ring check |
| 11 | `s <= floor(n / 2)` | `HighS` | 32-byte compare |
| 12 | a key is recovered from the digest, `r`, `s` and the recovery id | `BadSignature` | MessagePack, three keccak-256, one recovery |
| 13 | that key's address is the account's | `WrongSigner` | one keccak-256, a 20-byte compare |

Then the request goes into the table, and the record into `lane[g]` as in 7.1, with the
salt in its nonce word and `ts` in its expiry word. The order has the perp scheme's
reasons: everything up to check 11 costs nanoseconds, so a replay, a stale message, a full
table or a full lane never costs a recovery, while a well-formed forgery for a registered
account costs one every time (7.4). Check 13 exists because a recovery finds some key from
almost any signature, and only the address says whose. The new reasons are counted like
the others (`gateway.rejects.<reason>` in a run's summary). In the harness's own runs, a
single `StaleTimestamp`, `FutureTimestamp`, `ReusedRequest` or `SaltTableFull` makes the
run invalid: it means the harness sent stale or repeated messages, or sized a table wrong,
so the run measured something else (`RunResult::harness_rejects`).

**The attacks of 6.5, in this scheme.**

| Attack (6.5) | In the EIP-712 scheme |
|---|---|
| 1. Replay of an accepted message | `ReusedRequest`, before any recovery, while its `ts` is in the window; `StaleTimestamp` after that |
| 2. Replay of a message the engine rejected | the same: the request was used up when the message was forwarded |
| 3. Reordered messages | **Not prevented:** requests have no order. A place delivered after its own cancel is accepted: the cancel gets `UnknownOrder`, and the place rests. Polymarket's scheme is the same. In v1 it can't happen, because one account's messages go through one ring, in order |
| 4. Cancelling someone else's order | `NotOwner`; with the account field changed too, `WrongSigner` |
| 5. Cross-deployment | `WrongDomain`; with the deployment field changed too, `WrongSigner` (the domain is the deployment) |
| 6. Changing the message type | the form's name and fields are signed: `WrongSigner` |
| 7. Malleability | of the signature: `ReusedRequest` if the original was accepted, otherwise `HighS`. Of the message: every field is signed but a cancel's or a modify's market. A copy with another market is another request, since the market is in the key: the gateway accepts it, and the engine answers `UnknownOrder` or `UnknownMarket` and changes nothing. The original is still accepted, before the copy or after it, and an exact copy of either is `ReusedRequest`. So one observed cancel or modify can become up to one engine reject per other market id, each costing a recovery and a table slot, but it can't be suppressed and can't reach another order. A place's copy with another market is `WrongSigner`. Not in v1, whose one client is in process (7.4) |
| 8. Nonce burning | nothing to burn: a forgery fails check 12 or 13, and never takes a slot. A copy of a cancel or a modify with another market takes a slot of its own, as another request, and uses nothing of the genuine one's (see 7) |
| 9. Delayed execution of an abandoned message | usable until its `ts` is 5 minutes old (6 minutes if it was signed 60 s ahead), then `StaleTimestamp`. The client can't choose a shorter bound: Polymarket's optional `exp` field is not signed, and our message doesn't carry it |
| 10. A flood of forgeries | as in the perp scheme: each forgery costs one recovery, fails `WrongSigner`, uses no request, and can be sent again (7.4) |

**The journal and the audit.**
- A kind-1 record keeps its 152 bytes (11.3): the salt in the nonce word, `ts` in the
  expiry word, and `r || s` as sent. The recovery id is not journaled.
- Byte 120 of every segment header is the scheme, 1 for EIP-712 (11.2). It is part of the
  journal's identity: recovery refuses a restart with the other scheme (`auth: journal
  eip712, configuration perp`). A binary from before the scheme refuses such a journal (for
  it, byte 120 is a reserved byte that isn't zero), rather than audit it the wrong way.
- The signature audit (13.4) branches on segment 0's scheme. For each kind-1 record it
  rebuilds the digest from the header's deployment and the record's salt, `ts` and CMD40,
  and verifies `r || s` over it with the account's registered key
  (`PublicKey::verifies_digest`: `k256`'s `verify_prehash`, or libsecp256k1's `verify`
  with the digest as the message), low-S first. It needs no recovery id: a signature by that key over that digest is exactly
  one whose recovery, with the right id, gives that key. It can't see a market changed in
  the journal on a cancel or a modify, which the digest doesn't cover (step 1).
- In place of the nonce order, the audit checks that no request
  `(account, salt, ts, market)`, the gateway's key, appears twice in the whole journal, and
  that each `ts` lies in the window around the record's own time, the sequencer's `ts`
  (13.4). So a copy of a cancel or a modify on another market, which the gateway accepted
  as another request, passes next to the genuine record, and an exact repeat fails.
- Replay (13.1) is unchanged: the engine never sees the scheme.

**What v1 refuses, or doesn't have, in this scheme.**

| What | Where | Why |
|---|---|---|
| `--auth eip712` in pre-verified mode | `RunConfig::check`, `Pipeline::start` | nothing is signed |
| the "verify on core" ablation (16) | `RunConfig::check`, `Pipeline::start`; `Gateway::without_signature_checks` panics | the core's verifier rebuilds the perp scheme's 72 bytes |
| `--resume` after a crash (13.5) | `RunConfig::check` | a restarted gateway would need the last 5 minutes of requests, and the salt table isn't rebuilt from the journal |
| a timed flow over 3 minutes (`EIP712_MAX_TIMED_FLOW_NS`) | `RunConfig::check` | the messages are signed before the run and go stale 5 minutes after their `ts` (14.8) |
| a restart in the other scheme | recovery (11.8): `IdentityMismatch` | the scheme is part of the journal's identity |
| JSON or HTTP; several orders or cancels in one request; TP/SL groups; Polymarket's other operations (`cancelOrders` by exchange id, `cancelAll`, `updateLeverage`, ...) | not built | one message carries one command, in one of the three forms. The tests write the other forms with the generic writer, only to check the golden vectors |

Polymarket also refuses a client order id of all zeros. v1 doesn't check it: `order_id`
0 is written as 32 zeros. It is harmless here, since ownership comes from the order id's
high 32 bits (5.5).

**The load generator** (14.8). The salt is the item's nonce, which is unique per account, so
the plan and its pinned tests stay as they are (D-033). `ts` is one time for the whole
arena: the wall clock when signing starts. The harness signs an arena again before a run
it wouldn't last through: when the arena's age now, plus how long the run sends, plus a
60 s margin, comes to more than 5 minutes (`bench/src/e2e/workload.rs`). Signing takes
part of those 5 minutes too, so it checks a newly signed arena the same way once signing
is done, and refuses the run if it already fails.

**Worked example: Polymarket's golden vector 1.** It comes from Polymarket's py-sdk,
`tests/unit/test_perps_signing_golden.py` ("Golden signing vectors generated from the
TypeScript SDK Perps implementation", fetched 2026-09-30). The tests of `eip712.rs` check
its values byte for byte. The private key is `0123456789abcdef` four times over (32 bytes),
and the chain id is 137.

- The operation: `["createOrders", [[1, True, "0.5", "10", "gtc", False, None, None,
  None]]]`, salt 12,345, `ts` 1,751,500,000,000. Compacted, the three `None`s are dropped:
  a GTC buy of 10 at 0.5 on instrument 1, not post-only, in 6 fields (it has no `c`).
- Its MessagePack, 30 bytes:

```
92                                        array of 2
ac 63 72 65 61 74 65 4f 72 64 65 72 73    "createOrders" (fixstr, 12 bytes)
91                                        array of 1: the orders
96                                        array of 6: the order
01                                        iid 1
c3                                        buy: true
a3 30 2e 35                               p "0.5"
a2 31 30                                  qty "10"
a3 67 74 63                               tif "gtc"
c2                                        po: false
```

- `data`, the keccak-256 of those bytes (the file gives it):
  `8004f264b573f0d5edd3377ef127f251a2b11e0b9463c5fb5f1be3b42c94336a`.
- The domain separator for chain id 137:
  `38c098bf3fef934754c5164631cf31d78b2185103816a46ccdf281ecec4cc920`; the struct hash:
  `cb4631275769d1fdf92d7fcc813ca8559a8e027019e690e5b319cc0d4826bc48`; the digest:
  `7faac675878a329e050b14101173de773a49edf1a9803511b684f7cc28afd8e6`. The file doesn't
  print these three; they were computed with a throwaway Python model, and the next line
  proves them.
- The file's signature, `r || s || v`:
  `r = dd933bdada3c14c01dbe48fc02d470f423f0e0ef0897052602c964e20f82c709`,
  `s = 7c1ea1033e1e939a0633db606781f3b7160603337ab6201755cb191e99bc2d9b` (low: it starts
  with `7c`, below `7f`), and `v` = `1c` = 28, so the recovery id is 1. RFC 6979 is
  deterministic, and signing our digest with the key gives exactly these bytes, which
  proves the digest.
- Recovering from the digest, `r`, `s` and id 1 gives the key whose address is
  `0xfcad0b19bb29d4674531d6f115237e16afce377c`, with `k256`, and with libsecp256k1 in the
  feature build. The file doesn't print the address. Circle's `cctp-go` pairs it with the
  same key (`testutil.TestAddress`, "Derived from TestPrivateKey"), and the test also
  derives it from the key.
- **The same order as one of ours:** market 1, a buy, 50 ticks, 100,000 lots, GTC, not
  post-only, `order_id(9, 1)`. Its bytes are the vector's, with the order's header `96`
  changed to `97` (7 fields) and `c` appended: `d9 20`, then
  `"00000000000000000000000900000001"`. That makes 64 bytes
  (`a_place_is_golden_vector_1_with_its_order_id`).
- **A cancel of that order** is `92 b0 "cancelOrdersCOID" 91 d9 20
  "00000000000000000000000900000001"`. With golden vector 5's id in place of ours, its
  keccak-256 is that vector's `data`, `df8f1749…c7cc95`.

The file's other vectors are an IOC without a price (a gap in the middle of the order,
dropped), a bracket order with two triggers, `cancelOrders`, `cancelOrdersCOID`,
`updateLeverage`, `autoCancel` armed (a uint 64) and cleared, `updateMargin`,
`deleteProxy`, and two owner-signed messages, `CreateProxy` and `Withdraw` (struct types of
their own; Withdraw's domain adds a `verifyingContract`, and its `ts` is in seconds). The
tests write the forms our encoder can't make with the generic writer. Every `data` and
every signature matches, and every verifier recovers the address. 18.1 lists the other
known answers.

**Sources** (fetched 2026-09-30):
- Golden vectors: github.com/Polymarket/py-sdk, `tests/unit/test_perps_signing_golden.py`;
  the fields of `CreateProxy` and `Withdraw`: the same repository,
  `src/polymarket/_internal/actions/perps/signing.py`.
- The TypeScript SDK's `data` vectors: github.com/Polymarket/ts-sdk,
  `packages/client/src/websockets/perps/actions/trading.test.ts`.
- The forms, the replay rules and the error texts: docs.polymarket.com, `perps/trading.md`
  and `perps/errors.md`, and the Perps OpenAPI specification.
- EIP-712's worked example: github.com/ethereum/EIPs, `assets/eip-712/Example.js`.
- keccak-256 known answers: go-ethereum, `crypto/crypto_test.go` and
  `core/types/hashes.go`; the Keccak team's XKCP, `tests/TestVectors`
  (`KeccakF-1600-IntermediateValues.txt`, `ShortMsgKAT_SHA3-256.txt`).
- MessagePack: github.com/msgpack/msgpack, `spec.md`.
- The golden key's address: github.com/circlefin/cctp-go, `testutil`.

---

## 6. Nonces and replay protection

This section is the perp scheme's, the default. In the opt-in EIP-712 scheme (5.8), a
timestamp window and a table of recently accepted requests take the nonce's place; 5.8
goes through the attacks of 6.5 again for it.

### 6.1 The rules

1. Every signed message carries a nonce, a `u64`, per account.
2. A gateway accepts a message only if its nonce is **strictly greater** than the account's
   last used nonce. Gaps are allowed (INFO.md: "strictly increasing"), but a jump of more
   than 2^32 is rejected (`NonceJump`): a client bug that sent `2^64 − 1` (or a timestamp
   where a counter was meant) would otherwise lock its own account out for good, since
   every later message would be stale, also after a restart. 2^32 is far more than any
   counter needs.
3. **A nonce is used up exactly when the message is forwarded to the sequencer**, that is,
   when it goes into `lane[g]`, from which the sequencer takes every record into the
   journal ring. The record becomes durable at the journal's next flush, about a
   millisecond later. So "used up" means "journaled" once that flush is on disk; a crash
   before it un-uses the nonce, because nonces are rebuilt from the durable journal (6.3).
4. A message the gateway rejects, for any reason (bad signature, `Busy`, `Expired`,
   anything), does not use up its nonce. The same bytes can be sent again, and will be
   accepted if no later nonce has been used meanwhile and it hasn't expired.
5. A message the **engine** rejects (for example `InsufficientMargin`) has already used up
   its nonce: it was journaled. RISK.md 3.1 requires this (see attack 2 below).
6. The nonce is used up only **after** the signature verifies. Otherwise anyone could send
   a forged message with a high nonce and make every genuine message of the account stale.

The gateway's state per account is one `u64`, `last_nonce`, starting at 0, so the first
valid nonce is 1 or more. The load generator numbers each account's messages 1, 2, 3, …

**What `StaleNonce` tells a client, and what it doesn't.** It means only "this nonce is not
above the last nonce forwarded for your account". It does not say that *this* message
executed: the forwarded message may have been a different one with a higher nonce (gaps
are allowed), and it may still be in flight, not yet durable. The only proof that a
command executed is its released result (section 12), or after a restart, the account's
state. 12.4 works through the cases.

### 6.2 Where it is checked: the gateway

Each account is routed to exactly one gateway, `g = account mod N`, and one account's
messages go through one ring, in order. So the nonce check needs no shared state: gateway
`g` holds `last_nonce` for its accounts only, and a gateway receiving a message for an
account it doesn't own rejects it (`WrongGateway`), so no account's nonce can ever live in
two places.

Why not the sequencer (the other option INFO.md leaves open):
- a replayed message would cost a full signature verification at a gateway before the
  sequencer rejected it; at the gateway, a stale nonce is rejected before verifying, in
  about 100 ns. (This protects against *replays*. A *forgery* with a fresh nonce still
  costs one verification wherever the nonce lives; see 7.4.)
- the sequencer is on the path of every command to the core, and it would need state for
  every account; the gateways are parallel and each holds a slice.

### 6.3 Restart: rebuilt from the journal

The nonce state is not saved separately. On restart (13.5):
1. Recovery finds the journal's valid records and makes them durable (11.8).
2. Replaying them rebuilds the engine (13.1). The same pass reads every client record's
   `account` and `nonce` (journal kinds 1 and 2) and keeps the highest nonce per account.
3. Each gateway starts with that `last_nonce` for its accounts (`Gateway::new` takes the
   table, 19.2).

This is exact because of rule 3: the recovered journal holds exactly the messages whose
nonces stay used up. A message that never reached the journal file (still in a ring when
the process died, or written but lost in a power failure) is not in the recovered journal:
its results were never released (section 12) and its nonce is not used up, so the client
may resend it unchanged, if it hasn't expired. A message that reached the file but whose
results weren't released yet is in the recovered journal, so its nonce is used up (12.4
explains what the client sees). Recovery re-writes and syncs what it keeps before anything
is derived from it (11.8), so the rebuilt nonces never rest on data that exists only in
the page cache. The gateway count `N` may differ after a restart; the state is per
account, not per gateway.

**Never restart a deployment on an empty journal.** Nonce state lives only in the journal.
Starting a fresh journal under the same deployment id and `keys.txt` (to "fix" a failed
recovery, say) would reset every nonce to 0 and make every message ever signed for that
deployment valid again. A fresh journal needs a new deployment id (5.1), or new keys.

### 6.4 Nonces and the engine's order sequence (D-008, D-016, RISK.md 3.1)

Two counters, two jobs:

| | Nonce (gateway) | Order sequence, low 32 bits of `OrderId` (engine) |
|---|---|---|
| Protects against | a message being executed twice | two live orders sharing an id |
| Counts | every signed message: place, cancel, modify | places only |
| Moves when | the message is forwarded (journaled), even if the engine then rejects it | the engine accepts a place (`next_seq = seq + 1`) |
| Checked by | the account's gateway | the engine, before any book call (`Duplicate`) |
| Rebuilt after restart from | the journal's nonces | replaying the journal into the engine |

Why both: a cancel or a modify names an existing order, so the order id can't also be its
replay guard; it needs a nonce. And the engine's `next_seq` doesn't move on a rejected
place, by design (a client may resend a rejected order with the same id), so it can't
guard against replay either. The nonce closes that gap (attack 2).

### 6.5 Attacks, worked

Setup (attacks 1 to 9): `N` = 8 gateways, so account 9 is on gateway 1 and account 7 on
gateway 7; account 9's `last_nonce` is 41.

1. **Replay of an accepted message.** An attacker captures account 9's message with nonce
   41 and sends it again. Gateway 1: `41 <= 41`, reject `StaleNonce`, before verifying.
   Nothing is sequenced; the attack costs the gateway about 100 ns.
2. **Replay of a message the engine rejected.** Account 9 signs nonce 42: place with order
   sequence 17. The engine rejects it (`InsufficientMargin`); `next_seq` stays at 17. The
   nonce was used up when the message was forwarded, so `last_nonce` is 42. Later account
   9 deposits, and an attacker resends the old message: `StaleNonce`. Without the nonce the
   engine would now accept sequence 17: an order the client had given up on.
3. **Reordered messages.** Account 9 sends a place (nonce 50), then its cancel (nonce 51).
   Suppose a network delivered the cancel first: it is accepted (51 > 49) and the engine
   rejects it `UnknownOrder`; then the place (50) arrives: `50 <= 51`, `StaleNonce`. The
   place is never executed after its own cancel, and the client sees both outcomes. (In v1
   this can't happen: the rings are FIFO and one account uses one lane. The rule makes it
   safe once a network gateway exists.)
4. **Cancelling someone else's order.** Account 7 signs a cancel of `order_id(9, 17)` with
   `account` = 7: `NotOwner`. If it writes `account` = 9, the message is routed to account
   9's gateway, which verifies against account 9's key: `BadSignature`.
5. **Cross-deployment.** A message signed for a staging deployment (id 2) is replayed to
   production (id 1): `WrongDomain`. Editing the field to 1 changes the signed bytes:
   `BadSignature`.
6. **Changing the message type.** A signed cancel can't become a place: the tag at byte
   24 is signed, and a place needs a price and quantity the cancel never had. Signed
   deposits or withdrawals don't exist in v1: tags 4 to 9 from a client are rejected
   (`OperatorOnly`), whatever the signature.
7. **Malleability.** An attacker turns a message's `(r, s)` into `(r, n − s)`:
   `StaleNonce` if the original was already accepted, otherwise `HighS`. Either way the
   second form is never executed.
8. **Nonce burning.** An attacker sends a forged message for account 9 with nonce
   `2^64 − 1`: `NonceJump` (more than 2^32 above 41), before verifying. With nonce 42
   instead, it fails verification (`BadSignature`), so `last_nonce` stays 41 (rule 6), and
   account 9's next genuine message is accepted.
9. **Delayed execution of an abandoned message.** Taker account 1,500 signs an IOC buy of
   20,000 lots at 103,010 on market 3 (mark 103,000), nonce 7, `expires_at` = now + 5 s.
   The gateway answers `Busy`; the user gives up and sends nothing more. The next day the
   mark is 102,300, and whoever kept the bytes sends them again. Nonce 7 is still above
   the last (6) and the signature is valid, and without an expiry the order would pass the
   band check (upper edge `floor(102,300 × 1.008)` = 103,118) and buy 20,000 lots the user
   had abandoned. With it: `Expired`, before verifying.
10. **A flood of forgeries.** An attacker builds one message with no curve arithmetic:
    account 19 (routed to the same gateway as market maker account 9 when `N` = 10), a
    nonce just above 19's last, a far-future expiry, a valid CMD40, any `r` and a low `s`.
    Every cheap check passes, so each copy costs gateway 9 one verification (about 50 µs)
    and fails `BadSignature`, which doesn't use up the nonce: the same bytes work again.
    About 20,000 copies a second (2.7 MB/s) fill gateway 9, and account 9's genuine cancels
    queue behind them. **v1 does not defend against this**: its only client is the
    in-process load generator. 7.4 lists what a network gateway must add.

---

## 7. The gateway

### 7.1 Check order and reject reasons

In the perp scheme, the default, per message, in this order; the first failing check
rejects. (The EIP-712 scheme's order is in 5.8: checks 7 to 9 are replaced by the timestamp
window, `StaleTimestamp` and `FutureTimestamp`, the replay table, `ReusedRequest` and
`SaltTableFull`, and a 13th check, `WrongSigner`, follows the recovery. `GatewayReject` has
all 17 reasons, and a gateway of either scheme can give only its own.)

| # | Check | Reject | Cost |
|---|---|---|---|
| 1 | magic `PERP`, version 1, deployment id | `WrongDomain` | compare |
| 2 | header reserved bytes zero; CMD40 decodes (4.4) | `Malformed` | decode |
| 3 | command tag is 1, 2 or 3 | `OperatorOnly` | compare |
| 4 | `account mod N == g` | `WrongGateway` | compare |
| 5 | `account_of(order_id) == account` | `NotOwner` | compare |
| 6 | the account is in this gateway's registry | `UnknownAccount` | hash lookup |
| 7 | `nonce > last_nonce[account]` | `StaleNonce` | compare |
| 8 | `nonce − last_nonce[account] <= 2^32` | `NonceJump` | compare |
| 9 | `start_unix_ns + t_gw_in <= expires_at` (the clock read already taken for `t_gw_in`) | `Expired` | compare |
| 10 | `lane[g]` has room: more than 64 free slots for a place or a modify, at least 1 for a cancel | `Busy` | ring check |
| 11 | `s <= floor(n / 2)` | `HighS` | 32-byte compare |
| 12 | `r` and `s` are in range (`Signature::from_slice`) and the ECDSA signature verifies over bytes 0..72 with the account's key | `BadSignature` | about 50 µs |

Then: `last_nonce[account] = nonce`, write the ClientRecord (3.3) into `lane[g]`, publish.

**What the order buys.** Malformed, stale, wrong-gateway, expired and lane-full messages
cost nanoseconds, not a verification each. A well-formed message for a registered account
with a fresh nonce, a future expiry and a low `s` costs one full verification even if it is
forged (7.4). Check 10 comes before verifying so the gateway doesn't verify what it can't
forward, and it can't become false afterwards: the gateway is the lane's only producer, so
free space only grows until it writes.

**Cancels get headroom.** When the pipeline backs up (a slow disk, 2.4), the lanes fill
with places and IOCs already accepted. A market maker who sees the mark move must still be
able to pull its quotes, or the queued IOCs trade against quotes it tried to cancel. So the
last 64 slots of every lane are kept for cancels. Cancels only lower risk. A modify may too,
but the gateway can't tell, so only cancels get the headroom. It is gateway state only, so
determinism and replay are untouched. `Busy` is counted per command tag. The headroom is
against backlog, not against an adversary: any registered account on the same gateway can
fill those 64 slots with cancels of order ids it never used (each with a fresh nonce, and
rejected by the engine as `UnknownOrder` at no cost to it), and then another account's
quote pulls get `Busy`. That is flooding by a legitimate client, which per-account rate
limits (7.4, D-031 item 5) must bound before untrusted clients exist.

`IngressFull` (the sender's drop, 2.4) is not a gateway reject: the message never reached
a gateway. The two are counted separately.

Gateway rejects are not journaled and never reach the core: nothing happened in the
exchange. In v1 the gateway counts them per reason; a network gateway would send the
reject back to the client (Later; 7.4).

### 7.2 State

```rust
pub struct Gateway {
    index: usize,                                   // g
    count: usize,                                   // N
    deployment: u32,
    start_unix_ns: u64,                             // the run clock's anchor, for the expiry (or timestamp) check
    accounts: HashMap<AccountId, AccountState>,     // this gateway's accounts only
    verifies_signatures: bool,                      // false only in the insecure ablation (16)
    scheme: Scheme,                                 // which scheme's checks, and what it keeps
}
struct AccountState {
    key: PublicKey,                                 // parsed for the run's verifier (5.7)
    last_nonce: u64,                                // perp scheme
    address: Address,                               // EIP-712 scheme: derived from the key once (5.8)
}
enum Scheme {
    Perp,
    Eip712 { domain: Domain, verifier: VerifierKind, salts: SaltTable },  // 5.8
}
```

The thread loop (7.3) keeps the per-reason reject counts (per tag for `Busy`, for the
window and in total); they are private and returned when the thread is joined. The one
count main needs while the run is going (the total rejected, for the barriers of 14.4) is
also kept as a single-writer `AtomicU64` on its own line (3.2).

`Gateway::check(&mut self, msg: &[u8; 136], now_unix_ns: u64, lane_free: usize) ->
Result<Accepted, GatewayReject>` is a plain function with no threads or rings, so the
unit tests call it directly. The thread loop wraps it.

### 7.3 The loop

```
loop {
    if ingress.available(1) == 0 { if ingress.is_finished() { break }; idle(); continue }
    take up to 32 slots; for each:
        t_gw_in = now()
        match check(msg, start_unix_ns + t_gw_in, lane.free(65)):
            Ok(accepted) => t_gw_out = now(); lane.write(ClientRecord{..}); lane.publish()
            Err(reason)  => rejects[reason] += 1 (and in the window count if t_sched is inside it)
    ingress.release()
}
// leaving the loop drops `lane`, which closes it (2.8)
```

Publishing each record at once, not per batch, keeps a verified message from waiting for
the next 31 verifications (about 1.6 ms).

Before its loop, each gateway thread calls `verifier::prepare_this_thread` (5.7) and then
`Gateway::prepare_this_thread`. In the EIP-712 scheme the second runs one digest and one
whole recovery on a signature nobody sent (`r` = the `x` of the generator point, `s` = 1),
so that their code and stack are mapped before the timed flow (15.4); in the perp scheme it
does nothing. `check` takes the clock in nanoseconds in both schemes; the EIP-712 checks
round it down to whole milliseconds (5.8).

### 7.4 What a network gateway needs first (not in v1)

v1's gateways are fine for an in-process, trusted client. Before any client connects over
a network, these are preconditions (also in D-031):
- **Forged-message floods (attack 10).** Bind each connection to one account at login (one
  signature per session, or a TLS client certificate), and reject a message whose account
  field differs from the session's before any other check. Count `BadSignature` per
  connection and close the connection after 3. Verify round-robin across connections, not
  first-in-first-out over one queue. Never throttle by the *claimed* account before the
  signature verifies: that turns a flood into a lockout of the victim; charge the work to
  the connection.
- **Probing another account's nonce.** `StaleNonce` (check 7) comes before `BadSignature`
  (check 12), so a forger could binary-search an account's `last_nonce` (64 probes) and,
  repeated, read a market maker's message rate. Send account-specific reasons
  (`StaleNonce`, `NonceJump`, `UnknownAccount`) only on the account's own session; other
  connections get one generic reject. In the EIP-712 scheme (5.8) `ReusedRequest` (check
  8) comes before the recovery in the same way, and tells a prober whether a request was
  accepted, so it goes on that list.
- **What a reply means.** Every reply carries the gateway's start id. A client treats
  `StaleNonce` as final only if it came from a gateway started after the one it first sent
  the message to; otherwise it waits for the result (12.4).
- **Rate and size limits per account** (INFO.md 12.6), and a per-slot open-order cap in the
  engine (D-031): one funded account can otherwise rest millions of 1-lot orders and make
  one command cost O(its resting orders).

---

## 8. The operator queue

- **Commands:** `SetMarketParams` and `SetRiskTier` (market setup), `SetMark` (from the
  synthetic fair-value path), `Deposit` (the fund's capital and every account's deposit),
  `SetLeverage` (setup), and `Withdraw` (one a second in the M3 flow, to exercise the path).
- **Producer in a benchmark run:** the load generator's sender 0 (14.4, 14.10). It sends
  setup phase A, then, during the timed flow, the `SetMark`s and withdrawals in their
  place in the flow. In production this queue would be fed by the operator's own process
  (Later).
- **Trust:** operator commands are not signed. They are trusted because only the thread
  that owns the ring's producer end can write it, and that end is created inside the
  process. They are journaled like everything else (kind 3) and counted separately from
  signed orders/s (INFO.md 3).
- **Priority:** strict. The sequencer drains the operator ring first on every pass (up to
  64 records), then the client lanes. Marks drive liquidations, and a `SetMark` should not
  wait behind a flood of client orders. It can't starve the clients: the flow sends about
  670 operator commands a second, and even a full ring of 4,096 delays client records once,
  by about 0.4 ms.
- **When full,** the item stays pending in the sender, which keeps sending client items on
  schedule and pushes pending operator items, in order, as space appears (2.4, 14.10).
- **Acting on a result outside the exchange.** An operator command with an effect outside
  the exchange (in production, paying out a `Withdraw`) is acted on only after its result
  is released (its `BalanceChanged` passed the gate), never when the command was sent: until
  then a crash can undo it.

---

## 9. The sequencer

### 9.1 The merge rule

```
next_seq = last recovered seq + 1        // 1 for a new journal (13.5)
start = 0
loop {
    took  = take(operator, up to 64)
    for i in 0..N { took += take(lane[(start + i) mod N], up to 32) }
    start = (start + 1) mod N
    if took == 0 { if operator and every lane is_finished() { break }; idle() }
}
// leaving the loop drops the journal and core producers, which closes both rings (2.8)

take(input, limit):
    k = min(input.available(limit), limit, core.free(limit), journal.free(limit))   // space first (2.4)
    if k == 0 and input has records: count one "full-ring pass" (15.4)
    repeat k times:
        read the record
        t_seq = now()
        seq = next_seq; next_seq += 1
        journal.write(JournalRecord{ seq, ts = run_start_unix_ns + t_seq, kind = source, lane, account, nonce, command, expires_at, signature })
        journal.publish()                                   // journal first (2.5)
        core.write(CoreRecord{ seq, meta, command, t_sched, t_sent, t_gw_in, t_gw_out, t_seq })
        core.publish()
    input.release()
    return k
```

Each record is published as soon as it is written, not at the end of the batch. On x86 a
Release store is a plain `mov`, and the consumers reload `tail` only when their cached
copy runs out, so this costs little. It keeps up to 31 records' worth of the sequencer's
own writing (about 5 µs) out of the next command's "core path" (15.4).

**Deterministic given the journal, not given the inputs.** Which lane's record goes first
depends on thread timing, so two runs of the same flow are sequenced differently. That is
fine: the journal records the order the sequencer chose, the engine is a pure function of
that order, and replay follows the journal. So the merge rule only needs to be fair and
bounded, not reproducible. Round-robin from a rotating start, at most 32 per lane per pass,
bounds how long any lane waits.

**What order is promised.** One account's messages keep their send order: they use one
lane, and a lane is FIFO. Across accounts, nothing is promised, as on any exchange.

### 9.2 Sequence numbers and time

- `seq` is a `u64`, 1, 2, 3, … with no gaps, per journal. It identifies a command in the
  journal, in the core, in every event and in the gate.
- **Time.** The engine reads no clock and, in v1, needs no time at all: `Engine::apply`
  takes only a `Command`. Time exists in the pipeline for two things:
  - **the journal's timestamp** `ts`, for audit and for the "Later" `Tick` command, which
    will carry time into the engine as a journaled command field, never as a clock read;
  - **measurement stamps** (15.2).
- Both come from one reading of `Instant` (CLOCK_MONOTONIC), taken per record:
  `t_seq` = nanoseconds since the run started, and `ts = run_start_unix_ns + t_seq`, where
  `run_start_unix_ns` is read once at start and written in the header of every segment
  this process writes. So within one process `ts` is anchored to wall-clock time but never
  goes backwards, even if the system clock is stepped.
- **Across a restart**, the system clock may have been stepped back. So a restarted process
  sets `run_start_unix_ns = max(SystemTime::now(), last recovered ts + 1)`, and `ts` stays
  strictly increasing over the whole journal. Recovery checks this (11.8).

### 9.3 What the core receives

The CoreRecord of 3.3: `seq`, the source (and lane), the command, and the stamps so far.
The core decodes the command and applies it; it uses the stamps only to write its trailer
(10.3). The sequencer writes and publishes each record to the journal ring before the core
ring. That is for latency (the record can join the writer's current batch as early as
possible) and for liveness (2.5), not a
promise about what the journal writer has seen: the writer and the core never synchronise
with each other, and nothing needs them to. Output gating rests only on the watermark
(section 12).

---

## 10. The core thread

### 10.1 The loop

```
engine = resume.engine or Engine::<Book, Fast>::new(options)   // a new engine is built on the core thread, after pinning
expected = resume.next_seq or 1
loop {
    n = min(core_in.available(256), 256)
    if n == 0 { if core_in.is_finished() { break }; idle(); continue }
    t_batch = now(); busy_from(t_batch)            // busy time (2.2): one extra read per batch
    repeat n times:
        r = core_in.read()
        assert!(r.seq == expected); expected += 1  // one compare: the sequencer skipped or reordered nothing
        CURRENT_SEQ.set(r.seq)                     // thread-local, for the panic message (2.8)
        command = decode_command(r.command).expect("encoded by our own code upstream")
        sink.begin(r.seq)
        engine.apply(&command, &mut sink)
        t_done = now()
        sink.trailer(r, t_done)          // writes the trailer slot and publishes
    core_in.release(); busy_until(t_done)
}
// leaving the loop drops the event producer, which closes the event ring (2.8)
return engine                            // to the harness, for the snapshot (13.3)
```

- Only our own code writes the core ring (the gateway decoded and checked every client
  message; the operator and pre-verified paths use our encoder), so a decode failure here is
  a bug and panics (and so aborts the process, 2.8).
- One clock read per command, after `apply`, plus one per batch for the busy counter. The
  gate derives the command's service time from consecutive trailers:
  `t_done − max(t_seq, previous command's t_done)`, the time from when the core could start
  it to when it finished. When the core was idle, that includes the hop from the sequencer
  to the core (about 0.1 µs); 15.4 says so.
- With `--stamps off` (15.1) the core reads only words 0 to 6 of each record, reads no clock
  per command, and writes trailers with zero timings.

### 10.2 The event sink

`EventRingSink` implements the engine's `EventSink` trait:

```
emit(event):
    if event.free(1) == 0:
        event.publish()                  // let the gate see what is already written
        t0 = now(); while event.free(1) == 0 { spin }; stall_histogram.record(now() − t0)
    event.write([seq, encode_event(event)...])
    events_in_command += 1
    if this is the command's first event: outcome = Reject reason + 1 if it is a Reject, else 0
```

The rule "publish before waiting" matters: without it, the core would wait for space that
the gate can only free by reading slots the core hasn't published. The stall histogram
shows how often the disk held the core back (2.4). The wait always ends: the gate keeps
running until the event ring is closed (2.8), the journal writer keeps flushing, and if
either of them panicked, the whole process has already stopped (2.8).

### 10.3 The trailer

After each command the core writes one extra slot, the trailer, then publishes the event
ring. It marks the end of the command and carries its timings to the gate.

| Word | Content |
|---|---|
| 0 | `seq` |
| 1 | bytes: `[0]` 255 (trailer tag), `[1]` source, `[2]` command tag, `[3]` outcome (0 accepted, else `RejectReason` code + 1), `[4..6]` lane, `[6..8]` number of events (`u16`, saturating at 65,535) |
| 2 | `t_sched` |
| 3 | `t_sent` |
| 4 | `t_gw_in` |
| 5 | `t_gw_out` |
| 6 | `t_seq` |
| 7 | `t_done` |

The trailer is pipeline bookkeeping: it is never an engine event, and captures for replay
leave it out (13.2).

---

## 11. The journal

### 11.1 Files

- A run writes into `<run_dir>/journal/`: segment files `seg-000000.jnl`,
  `seg-000001.jnl`, … of a fixed size, `SEG_BYTES` = 1 GiB (tests use 4 KiB to cross
  segment boundaries often).
- Each segment starts with a 128-byte header (11.2), then records (11.3) back to back from
  offset 128. A record never spans two segments: if the next record doesn't fit in what
  is left, the rest of the segment stays zero and the record goes into the next segment.
- **Each process start begins a new segment.** Call the time from one start to the next
  crash or stop a *life*. A life never writes into a segment an earlier life used, so a
  segment's header describes every record in it: which clock anchor and which key
  registry its writer used (11.2). The rest of the previous life's last segment stays zero.
- Segments are written front to back and never rewritten, except by recovery (11.8), which
  zeroes what a crash left after the journal's end and re-writes what it keeps.
- **One helper creates every segment**, `create_segment(index)`, whether before the run, in
  the middle of a run that outgrew its preallocation, or after recovery. It creates the
  file, writes zeros over all of it (8 MiB writes), calls `sync_all` on it, then fsyncs the
  directory, all before any record in that segment can be released. POSIX doesn't promise
  that an fsync of a new file makes its *name* durable; the directory fsync does, so a
  crash can't lose a whole segment of released records. The same holds one level up for
  every directory a run creates on the way to the journal (`<session>/<group>/<run>/journal`):
  each is created by `create_dir_all_durable`, which fsyncs a new directory's parent once
  it exists, so no level of the path can vanish at a power loss either.
- Segments are opened with `OpenOptions::new().read(true).write(true)`, never
  `.append(true)`: on Linux, `pwrite` on a file opened with `O_APPEND` ignores the offset
  and appends.

### 11.2 Segment header (128 bytes)

| Offset | Size | Field | Across segments |
|---|---|---|---|
| 0 | 8 | magic, ASCII `PERPJNL1` | identity: the same in every segment |
| 8 | 2 | format version, `u16` = 1 | identity |
| 10 | 1 | journal mode: 1 signed, 2 pre-verified | identity |
| 11 | 1 | build profile of the writer: 1 release, 0 other | may differ (informational) |
| 12 | 4 | deployment id, `u32` | identity |
| 16 | 4 | segment index, `u32` | equals the file's index |
| 20 | 4 | `engine_semantics`, `u32` (below) | identity |
| 24 | 8 | `first_seq`: the seq of the first record in this segment | continues the sequence |
| 32 | 8 | `id_hash_seed` (D-011) | identity |
| 40 | 8 | `order_capacity` | identity |
| 48 | 8 | `account_capacity` | identity |
| 56 | 8 | `slot_capacity` | identity |
| 64 | 8 | `scratch_capacity` | identity |
| 72 | 8 | `run_start_unix_ns` of the life that wrote this segment (9.2) | may differ |
| 80 | 32 | SHA-256 of the `keys.txt` that life loaded (zero in a pre-verified journal) | may differ (a key replaced at a restart, 5.4) |
| 112 | 8 | the first 8 bytes of the writer's git commit id (zero if unknown) | may differ (informational) |
| 120 | 1 | signing scheme: 0 perp, 1 eip712 (5.8, D-033; reserved and zero before, so an older journal reads as perp) | identity |
| 121 | 3 | reserved, 0 | |
| 124 | 4 | CRC32C of bytes 0..124 | |

- **The journal's identity** is the fields marked "identity". Recovery compares every
  segment's identity with segment 0's; a header with a valid CRC but a different identity
  is an **error**, never "the end of the journal" (11.8). A restarting process takes the
  identity from segment 0 and refuses to start unless its own configuration matches it
  (deployment, mode, signing scheme, the five engine options, `engine_semantics`), printing
  both. The process's configuration never overrides the journal. To change any of these,
  start a new journal under a new deployment id (5.1).
- The other fields describe one life: its clock anchor (9.2), its key registry, and the
  binary's commit and build profile.
- **`engine_semantics`** ties the journal to the engine's behaviour, which the byte format
  alone doesn't: a later build with different fee rounding would replay the same journal
  into a different state, silently. It is a constant in `pipeline`, `ENGINE_SEMANTICS`,
  starting at 1, bumped by any engine change that can alter an event or the state for a
  command the old engine applied without panicking. A test pins it: it applies the first
  100,000 commands of the smoke flow and compares the CRC32C of the resulting event
  stream's bytes with a pinned value, so an engine change that alters results fails the
  test until someone bumps the constant and re-pins it, on purpose. Recovery and replay
  refuse a journal whose `engine_semantics` differs from the binary's unless given
  `--allow-engine-change`, and print both values. A fix for a poison pill (2.8) doesn't
  bump it: it changes only what happens where the old engine panicked.
- **Commit and profile** are set at build time (`option_env!("PERPS_GIT_COMMIT")`, as the
  recorder build already does; release or not from `cfg!(debug_assertions)`). Recovery and
  replay print them and warn when they differ from the running binary's. The profile is
  recorded, not enforced. The engine checks integer overflow in every profile (D-004's
  2026-10-01 update), and D-020's limits rule overflow out, so an overflow panic in any
  build has found an engine bug, which is what we would want to know.
- **The header holds the hash seed, a secret in production** (D-011): journal files must
  be protected like the seed itself. In benchmark runs the seed is derived from the run's
  seed and is not secret.
- A segment is created all zeros. The writer puts the header in front of the first batch
  it writes into that segment, so the header becomes durable with that batch. An all-zero
  header means "not used yet".

### 11.3 Records

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `len`, `u32`: 80 for kinds 2 and 3, 152 for kind 1 |
| 4 | 4 | `crc`, `u32`: CRC32C of bytes 0..4 followed by bytes 8..`len` (every byte but itself) |
| 8 | 8 | `seq`, `u64` |
| 16 | 8 | `ts`, `u64`: nanoseconds since the UNIX epoch, strictly increasing over the whole journal (9.2) |
| 24 | 1 | `kind`: 1 signed client command, 2 pre-verified client command, 3 operator command |
| 25 | 1 | reserved, 0 |
| 26 | 2 | `lane`, `u16` (0 for operator commands) |
| 28 | 4 | `account`, `u32`: the signer (0 for operator commands) |
| 32 | 8 | `nonce`, `u64` (0 for operator commands); in a journal of the EIP-712 scheme (header byte 120), the salt |
| 40 | 40 | the command, CMD40 (4.2) |
| 80 | 8 | kind 1 only: `expires_at`, as the client signed it; in the EIP-712 scheme, `ts` in milliseconds |
| 88 | 64 | kind 1 only: the signature `r || s`, as the client sent it |

Lengths are multiples of 8, so records stay aligned to words and map one-to-one onto the
ring's words.

**Which commands each kind may hold** (recovery checks all of it, 11.8):
- kinds 1 and 2 hold client commands: tags 1 to 3 only, with
  `account == account_of(order_id)`;
- kind 3 holds operator commands: tags 4 to 9 only;
- kind 1 appears only in a signed journal (mode 1), kind 2 only in a pre-verified journal
  (mode 2); kind 3 in both.

**Why the signature is journaled** (question Q3): a kind-1 record holds every signed field
(the deployment is in the header, magic and version are constants), so the exact 72 bytes
the client signed can be rebuilt (4.4) and the signature checked again by anyone with the
registry (13.4). The journal then proves that each order was authorised by the key the
registry names for its account, not only that it happened. That proof is relative to a
registry the auditor trusts from another source (for example, each client confirming its
own public key): the digest in the header shows only which registry file the exchange
used. The cost is 72 bytes per signed record (the expiry and the signature): 15.2 MB/s
instead of 8 MB/s at 100k signed orders/s. In the EIP-712 scheme (5.8) the same record
holds everything its digest is rebuilt from (the header's deployment, the salt, `ts` and
the CMD40), so the audit checks it the same way. Only the recovery id is left out, and the
audit doesn't need it.

**Worked example 1** (kind 1): the message of 5.6 sequenced as seq 5,000 at
`ts` = 1,790,000,000,123,456,789 on lane 1 (account 9 with `N` = 8). The CRC is
`0xE5C5CC1D`.

```
  0: 98 00 00 00 1d cc c5 e5 88 13 00 00 00 00 00 00    len 152 | crc | seq 5,000
 16: 15 cd 4e 2b 84 5b d7 18 01 00 01 00 09 00 00 00    ts | kind 1, reserved, lane 1, account 9
 32: 01 00 00 00 00 00 00 00 01 00 00 01 03 00 00 00    nonce 1 | CMD40 ...
 48: 01 00 00 00 09 00 00 00 56 92 01 00 00 00 00 00
 64: 20 a1 07 00 00 00 00 00 00 00 00 00 00 00 00 00
 80: 00 ac 16 20 8b 5b d7 18 96 29 36 e1 f0 2f 1c 30    expires_at | r ...
 96: 22 21 2d f5 bb 4c 09 03 4b d8 1f b4 9f 5a ca 9e
112: 04 d8 26 e8 f0 6c 1d d5 75 35 21 c3 a0 eb 5f 3a    ... r | s ...
128: 8a e8 5e 6e cf ad 67 2b f6 54 3d 71 8e 16 9e 07
144: 6d 2c 1f d8 cb 60 70 10
```

**Worked example 2** (kind 3): `SetMark { market: 3, price: 103,001 }` as seq 5,001,
2 µs later. The CRC is `0xFF8EEBC8`.

```
  0: 50 00 00 00 c8 eb 8e ff 89 13 00 00 00 00 00 00    len 80 | crc | seq 5,001
 16: e5 d4 4e 2b 84 5b d7 18 03 00 00 00 00 00 00 00    ts | kind 3, lane 0, account 0
 32: 00 00 00 00 00 00 00 00 07 00 00 00 03 00 00 00    nonce 0 | tag 7, market 3
 48: 59 92 01 00 00 00 00 00 00 00 00 00 00 00 00 00    price 103,001
 64: 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
```

### 11.4 CRC32C

Our own, in `pipeline/src/crc32c.rs` (no new crate): the Castagnoli polynomial, reflected
(`0x82F63B78`), initial value `0xFFFFFFFF`, final XOR `0xFFFFFFFF`. Check value:
`crc32c(b"123456789") == 0xE3069283`; `crc32c(b"") == 0`.

- **Slicing by 8.** Eight tables of 256 `u32` entries (8 KiB), built by a `const fn`. Each
  step XORs the running CRC into the next 8 bytes and looks up one table per byte, so it
  handles 8 bytes per step instead of 1; the last few bytes go one at a time through the
  first table. It is still safe Rust and about 30 lines. The byte-at-a-time version stays
  in the tests as the reference, and both must agree on random inputs.
- **Why slicing by 8 from the start.** Byte at a time is limited by the chain of
  dependent lookups: about 6 to 7 cycles a byte, so about 170 to 190 ns for an 80-byte
  record at 2.9 GHz. At 2M pre-verified records a second that alone is about a third of
  the writer's core, and the core-path search (15.7) must not be limited by the writer.
  Slicing by 8 typically runs at 1 to 1.5 cycles a byte. The `pipeline_parts` benchmark
  measures both, and the writer's CPU per record. (x86's CRC32C instruction would be
  faster still, but calling it needs `unsafe` intrinsics; not worth it.)
- Why CRC32C rather than zlib's CRC32: the same size and cost in software, and it is the
  checksum ext4, iSCSI, LevelDB and RocksDB use for this job.
- What it protects against: torn writes and any corruption that doesn't happen to keep the
  CRC valid (a random corruption passes with probability 2^-32). It is not a security
  measure; the file system's permissions are.

### 11.5 Group commit

The journal writer collects records into a batch and makes the batch durable with one
`write` and one `fdatasync`.

**The rule:** flush the batch when it holds `B` records, **or** when its oldest record was
sequenced at least `T` ago, whichever comes first. "Sequenced at" is the record's own
`t_seq` (`ts − run_start_unix_ns`), not the time the writer happened to take it, so time a
record spent waiting in the ring during the previous flush counts toward `T`.

**Defaults:** `T` = 1 ms, `B` = 4,096 records. `B` can be set lower, never higher:
recovery relies on one flush writing at most 4,096 records (11.8).
- `T` = 1 ms is INFO.md's "about once a millisecond". It caps the fsync rate at 1,000 a
  second whatever the order rate, which any disk sustains, and it adds at most 1 ms to the
  durable latency. A smaller `T` lowers latency and raises the fsync rate.
- **`T` = 0** means "flush as soon as there is anything": while one flush runs, the next
  records collect, and they form the next batch by themselves. A record then waits between
  `F` and `2F`, and the fsync rate can reach `1/F`. The `T` sweep (15.8) and the fsync
  ablation (section 16) measure it next to 1 ms; the evidence decides `T` (D-025).
- `B` = 4,096 bounds one write to at most 623 KB (4,096 × 152 bytes) and so bounds how
  long one flush takes after a stall. At the target rates it never fires first: at 1M
  commands/s, 1 ms holds 1,000 records.

```
loop {                          // buf holds the batch's encoded records; count = how many
    while count < B and journal_ring.available(1) > 0 {
        read the next record's words
        if it doesn't fit in the current segment { flush the batch so far, if any; switch_segment() }
        append its bytes to buf (to_le_bytes per word), compute its CRC, patch bytes 4..8
        remember: first record's t_seq (if the batch was empty), last seq
    }
    journal_ring.release()      // the records are in buf now; give the sequencer the space
    finished = journal_ring.is_finished()
    if count > 0 and (count >= B or now() − oldest_t_seq >= T or finished) { flush() }
    else if count == 0 { if finished { break }; idle() }
}

flush():
    t0 = now(); file.write_all_at(&buf, offset)       // pwrite, std::os::unix::fs::FileExt
    t1 = now(); file.sync_data()                      // fdatasync
    (either call returns an error: panic, which aborts the process; 11.7)
    t2 = now(); offset += buf.len()
    durable.store(last seq in batch, Release)         // 11.7
    record: flush time t2 − t0, fdatasync time t2 − t1, records, bytes
    buf.clear(); count = 0

switch_segment():
    the old segment was made durable by the flush just before;
    take the next segment (create_segment() if it doesn't exist yet);
    put its header (first_seq = next record's seq) at the front of buf; offset = 0
```

A new life starts the same way, in the segment after the one where recovery found the
end (11.8).

**The latency it adds** (the durable part of 12.3): a record that arrives `x` after its
batch opened waits `T − x` for the batch to close, plus the flush time `F`. With arrivals
spread evenly, the average wait is `T/2 + F` and the longest `T + F`. Example: `T` = 1 ms,
`F` = 0.2 ms: average 0.7 ms, longest 1.2 ms; with `F` = 0.5 ms: 1.0 and 1.5 ms. If the
disk is slower than `T` (`F > T`), the next batch's oldest record is already older than `T`
when the previous flush returns, so flushes run back to back, and a record waits between
`F` and `2F`, as with `T` = 0.

**The per-order ablation** (section 16) is the same writer with `B` = 1: every record gets
its own `write` and `fdatasync`.

### 11.6 fdatasync, preallocation, the write path

- **`fdatasync`, not `fsync`** (`File::sync_data`, not `sync_all`). `fdatasync` flushes
  the data and only the metadata needed to read it back (the file size, block
  allocation), and skips timestamps such as mtime. That is all durability needs.
- **Preallocation by writing zeros** (`create_segment`, 11.1). Before the run, every
  segment the run will need is created and filled with zeros. Writing zeros (rather than
  `fallocate`) matters: blocks that `fallocate` reserves are marked "unwritten", and the
  first write into them changes metadata, which `fdatasync` must then also flush (a
  file-system journal commit). Over already-written blocks, a record write is a pure data
  write, and the file size never changes during the run. **Except on copy-on-write file
  systems** (ZFS, btrfs): there every overwrite allocates new blocks anyway, and ZFS
  stores all-zero blocks as holes, so preallocation has no effect. The probe (15.10) names
  the file system and the report says so.
- **How many segments:** `ceil(expected records × record size × 1.25 / (SEG_BYTES − 128))
  + 1`. At 100k signed/s for 65 s: 6.5M × 152 bytes = 988 MB: 3 segments. At 2M
  pre-verified/s for 35 s: 5.6 GB: 8 segments, about 10 s of zero-filling before the run,
  not measured. If a run still runs out, the writer calls `create_segment` itself; that
  flush is then slow, which shows in the flush histogram, and the run is flagged.
- **Write path:** ordinary buffered writes (`pwrite` into the page cache) followed by
  `fdatasync`. Not `O_DIRECT`: it needs aligned buffers and sizes and bypasses the page
  cache, which replay would then have to read from disk; buffered plus `fdatasync` gives
  the same durability.
- **Where the journal lives:** in the local `dev` container, `/target/runs/<run>/journal`
  (the `target` named volume on WSL2's ext4 disk, the only writable disk there). On
  PERPSBOX, `./runs/<run>/journal` on the box's disk (overlayfs as probed; see Q2). The
  harness deletes a run's journal after the run unless `--keep-journal` is given (the
  50 GB disk fills otherwise).
- **What "durable" means here: on disk as far as `fdatasync` on this file system says.**
  Nothing in M3 cuts the power, and some setups make `fdatasync` a no-op: tmpfs and ramfs
  (memory only), overlayfs mounted with `volatile`, ZFS with `sync=disabled` (a common
  hosting setting a container can't see), and on WSL2, whether the guest's flush reaches
  the physical disk depends on the Windows host's virtual-disk caching. So the probe
  (15.10) refuses a run directory on tmpfs, ramfs or a `volatile` overlay, flags an
  `fdatasync` that is suspiciously fast, and every durable number is labelled "durable as
  reported by <file system> on <machine>; not power-loss tested" (15.11).
- **Discard mode** (`--journal discard`): the writer does everything above except the
  `write` and `fdatasync` (it encodes, checksums, applies the `T`/`B` rule and publishes
  the watermark). It is used only for the core-path search (15.7), to keep the disk out of a
  measurement that is about the core. Everything it releases was never on disk, so every
  output from that mode carries the label "journal discarded", and the harness refuses, in
  discard mode, any "→ durable ack" stage, the durable-limit searches and the replay test
  (13.3).

### 11.7 The durable watermark

- One `CachePadded<AtomicU64>`, `durable`, starting at 0 (at the last recovered seq after
  a restart). Only the journal writer writes it: after each successful `fdatasync`,
  `durable.store(last seq of the batch, Release)`.
- Meaning: every record with `seq <= durable` is on disk. That holds because the writer
  writes records in seq order (the journal ring is FIFO and the sequencer is its only
  producer), and each flush covers everything before it.
- The gate reads it with `load(Acquire)` (12.2). Release/Acquire here means: once the gate
  sees the new value, the flush really has happened.
- **On an I/O error**, `write` or `fdatasync` returning an error, the writer panics, which
  aborts the whole process (2.8): the watermark stops, nothing more is released, and
  recovery (11.8) takes over at the next start. Retrying is unsafe on Linux: when writeback
  fails, ext4 and xfs mark the pages clean and report the error once, so a retried
  `fdatasync` "succeeds" without writing anything (the PostgreSQL "fsyncgate" problem of
  2018; Rebello et al., "Can Applications Recover from fsync Failures?", USENIX ATC 2020).
  The new bytes also stay readable in the page cache although the disk doesn't have them,
  which is why recovery re-writes what it keeps before trusting it (11.8, step 3).
- **Operating rule:** an I/O error on the journal means the disk is failing. Investigate
  it (and preferably reboot) before restarting. Recovery refuses to start if its own
  re-write and sync fail.

### 11.8 Recovery

Recovery runs once at start, before anything reads the journal. It finds the journal's
end, makes sure nothing after the end can ever be mistaken for data, and makes everything
before the end durable. It either succeeds, or stops with an error and changes nothing.

**What a crash can leave.** The writer syncs each batch before it writes the next, so at
any moment at most one flush is unfinished: one batch (at most `B` = 4,096 records of at
most 152 bytes: 622,592 bytes), plus a segment header if the batch started a new segment.
After a power loss, any subset of that flush's 512-byte sectors may have reached the disk,
in any order. After a process crash (no power loss), the page cache still holds everything
the writer wrote. Everything written before the unfinished flush is on disk.

**Step 1: read the journal and find its end.**

```
expected = 1; end = (segment 0, offset 0)
for segment 0, 1, 2, … while the file exists:
    header all zeros: the journal ends at `end`
    header CRC fails: the journal ends at `end` (a torn header)
    header CRC valid, but its identity differs from segment 0's, its index is wrong,
        or first_seq != expected: ERROR
    off = 128; end = (segment, 128)
    loop:
        if off + 8 > SEG_BYTES: break                          // end of this segment
        len = u32 at off
        if len == 0: break                                     // end of this segment's data
        if len is not 80 or 152, or off + len > SEG_BYTES, or the CRC fails:
            the journal ends at `end`                          // these bytes are not a record
        // the CRC matched: this is a record, and everything about it must be right
        require, else ERROR: seq == expected; kind matches len and the journal's mode;
            the tag fits the kind and account == account_of(order_id) (11.3);
            reserved bytes 0 (and lane, account, nonce 0 for kind 3); the CMD40 decodes;
            ts > the previous record's ts
        accept the record; expected += 1; off += len; end = (segment, off)
    // on `break`, go on to the next segment: its header must continue the sequence
```

The rule in one sentence: **the CRC decides whether some bytes are a record at all; once
they are, everything about them must be right, or recovery stops with an error.** A torn
write fails its CRC. A record with a valid CRC but the wrong seq, a wrong kind or a
decreasing `ts` is a bug, tampering, or data from somewhere else, and quietly ending the
journal there would delete results clients have already seen.

**Step 2: check that what follows the end can only be a torn tail.** The unfinished flush
started at or before the end (everything before it was synced and is intact) and wrote at
most `W` = 128 + 4,096 × 152 = 622,720 bytes, all in one segment: the end's segment, or the
next one if that flush started a new segment. So recovery reads every byte after the end,
in every segment, and requires each nonzero byte to lie in the **tail region**: the `W`
bytes after the end in its segment, or the first `W` bytes of the next segment. And since
the writer syncs a segment's last batch before it writes anything into the next, the
nonzero bytes may lie in one of those two parts, never in both. A nonzero byte anywhere
else, or nonzero bytes in both parts (a later segment with a valid header, valid records
far past the end, the rest of a segment whose records were synced while the journal goes
on in the next one), means the damage is not a torn tail; for example bit rot or a device
error in the middle of synced data. Recovery then stops with an error naming the segment
and offset of the first such byte, and changes nothing: the evidence survives, and a person
decides. (Silently cutting the journal there could roll back millions of released
commands.)

The check can be this strict because of step 3: every life starts with every byte after
the end zero on disk, and writes only forward from there.

**Step 3: make the disk match the recovered journal.**
1. Copy the tail region's nonzero bytes, if there are any, to
   `<run>/journal/torn-<segment>-<offset>.bin` (fsync it and the directory), for
   inspection. A copy is never overwritten: two lives can be torn at the same place (a life
   whose first flush lost its new segment's header, then the next life, which starts in
   that segment again), so if the name is taken the copy goes to
   `torn-<segment>-<offset>-2.bin`, `-3`, and so on.
2. Write zeros over the tail region.
3. Re-write the kept part of the end's segment (its header and its records up to the
   end): `pwrite` back the bytes just read. That makes those pages dirty again, so the next
   `fdatasync` really writes them. After a failed `fdatasync`, Linux keeps the unwritten
   bytes readable in the page cache but marks them clean (11.7); without this step, a
   restart on the same boot would read them, keep them, build on them, and never write
   them. The same step makes durable the records a process crash left only in the page
   cache. Only the end's segment needs it: every earlier segment was fully synced before
   the writer moved on (11.5).
4. `fdatasync` every segment touched, then fsync the directory. If anything here fails,
   refuse to start.

After step 3, everything up to the end is on disk and every byte after it is zero on disk.
Only then may anything be derived from the journal: replay, the gateways' nonces, new
records (13.5).

**Step 4: continue.** The new life starts writing in the segment after the end's segment
(made with `create_segment` if it doesn't exist), with `seq = expected` and its own
header. The rest of the end's segment stays zero.

**A new journal** (a fresh start, no restart) goes through steps 1 to 3 too, before its
first record. Any segment with a valid header means a journal is already there, and the
start is refused. Without one, a nonzero byte can only be what a crash in a life's first
flush leaves (its header's sector lost, part of its body on disk, none of it released):
step 2 checks that, and step 3 copies it aside and zeroes it. Without this, the new life
would write its records in front of the old ones, and a later recovery would read on from
the new records into the old: the next seq, a valid CRC.

**Why this is enough** (the interview version):
- **Every released record survives.** It was synced before it was released, so it lies
  before the unfinished flush, and so before the end; step 3 syncs it once more.
- **Nothing from an earlier life comes back.** Every life starts with zeros after the end
  and writes forward, so at a crash the only nonzero bytes after the durable data are that
  life's own unfinished flush, and the next recovery zeroes them. The draft rule zeroed only
  the rest of the end's segment and the headers of later segments. After a crash that lost
  a new segment's header but kept part of its body, the restarted writer reused that
  segment at the same offsets, and after a second crash recovery read on from the new
  records into the old ones: same seqs, valid CRCs. Resent signed orders could then execute
  twice. A model of the draft rule accepted such stale records; a model of this one never
  did in 20,000 random crash-and-restart runs (top of the file).
- **A mismatch never truncates silently.** An identity mismatch, a valid-CRC record that
  doesn't fit, and damage outside the tail region are all errors.

**Considered instead: an epoch mixed into every record's CRC** (as RocksDB's recyclable log
format puts the log number in each record). Old records would then fail their CRC, with no
zeroing. But the epoch must be new for every life even when the header that carried it was
lost: an epoch taken as "the highest surviving one, plus 1" can repeat, and then the stale
records pass again. So it needs a random or separately stored epoch. Zeroing the tail
region is simpler to argue and to test, and step 2 reads the whole journal anyway.

**The assumption about sectors.** Records are not aligned to 4 KiB pages, so each batch's
`pwrite` dirties the page that holds the end of the previous batch, and writeback rewrites
that whole page, including bytes already released. The spec assumes that a disk writes each
512-byte sector completely or not at all, so rewriting a sector whose leading bytes are
unchanged can't damage those bytes. SSDs with power-loss protection guarantee it, and
ordinary disks and SSDs behave so in practice. Without it, a power cut could garble up to a
page of released records. Padding every batch to a 4 KiB boundary would remove the
assumption at a cost of about 2 KiB per batch (about 14% of the bytes at 100k signed/s,
more at low rates); not taken.

**Cost.** Step 2 reads every segment, at disk or page-cache speed (about a second per
GB); step 3 writes at most one segment. Restarts are rare, and benchmark runs don't
restart.

**What a cut tail means for clients.** Nothing cut was ever released: its client saw
nothing, and its nonce is not used up (6.3), so the client can resend it if it hasn't
expired. Records a process crash left in the page cache are kept (they are valid, in order
and consistent with everything before them) and step 3 makes them durable (12.4).

### 11.9 Throughput

- Signed at 100k/s: 152 bytes × 100k = 15.2 MB/s, 1,000 `fdatasync`s a second.
- Pre-verified at 2M/s: 80 bytes × 2M = 160 MB/s. Whether the disk sustains that is itself
  a measurement; if it can't, the pre-verified durable rows are disk-bound, and the report
  says so.

---

## 12. Output gating

### 12.1 The release rule

An event is released when the command that produced it is durable:
**release the event slot with command `seq` S once S ≤ `durable`, and S's trailer is in the
ring.** Every event is gated (acks, fills, cancels, rejects, operator echoes,
liquidations), so nothing anyone sees can be undone by a crash. The gate releases slots
strictly in ring order, which is the core's emission order, so every client sees results
in sequence order. Waiting for the trailer means a command is released whole: if the engine
panics in the middle of a command (2.8), none of it has been released. The exception is a
command with more events than the event ring holds: once the ring is full of that command's
events alone, the core can't go on until some are freed, so they are released as they come
(2.5).

In v1, "release" means the gate processes the slot: it records latencies from trailers,
updates counters and fund tracking, and appends the event to the capture if capture is on
(13.2). Private feeds (Later) would read from the gate's output instead.

### 12.2 How the gate waits, with no lock on the core's path

```
expected = resume.next_seq or 1
loop {
    w = durable.load(Acquire)
    released = 0
    while released < 1,024 and events.available(1) > 0 {
        if events.peek(0) > w { break }        // next slot's command isn't durable yet
        if its trailer isn't in the ring yet { // one look at the last slot published
            if the ring is full of this command's events at the pass's start
                or the ring is closed { release them as they come }
            else { break }
        }
        slot = events.read()
        if slot is a trailer {
            assert!(slot.seq == expected); expected += 1   // one compare: no command skipped
            t_release = now(); record the command's latencies and counts
            commands += 1; released_counter.store(commands, Relaxed)   // for main's barriers (3.2)
        }
        else { count the event; update fund tracking; capture it }
        released += 1
    }
    events.release()
    if released == 0 { if events.is_finished() { break }; idle() }
}
```

The core never reads the watermark and never waits on the gate except through a full
event ring (2.4). The gate re-reads the watermark only when it runs out of durable slots,
and it notices a new watermark within one spin (about 100 ns). Checking that trailers
arrive with consecutive seqs (and the core's matching check, 10.1) costs one compare each,
and turns a sequencer bug that sent the core something other than the journal's order into
an immediate stop, rather than a replay mismatch found later.

### 12.3 The latency cost

A command's result is released at `max(t_done, durable time of its batch)`. The core
finishes in microseconds and the batch in about a millisecond, so in practice the gating
delay is the group commit's wait: on average `T/2 + F`, at most `T + F`, where `F` is the
flush time (11.5). With `T` = 1 ms and `F` = 0.2 ms: 0.7 ms on average, 1.2 ms at most,
which is INFO.md 5's "bounded below by the fsync duration and above by roughly one commit
interval plus an fsync". It is reported as its own stage ("durability wait",
`t_release − t_done`) next to the measured fsync time, so it is never blended into the
core-path number.

### 12.4 What a client sees after a crash

Three cases, for a command sequenced before the crash:

| Case | Journal after recovery | Engine state after replay | What the client saw | If the client resends the same bytes after the restart |
|---|---|---|---|---|
| released | present | includes it | its results | `StaleNonce` |
| in the journal file but not released (synced but the gate hadn't got to it; or written, not synced, and kept by the page cache through a process crash, then made durable by recovery, 11.8) | present | includes it | nothing | `StaleNonce`; the order happened, and the client learns its outcome from its account's state (a query or feed, Later) |
| never in the file (still in a ring; or written, not synced, and lost in a power failure) | absent | doesn't include it | nothing | accepted, once, if it hasn't expired and no later nonce of the account was used |

The promise: **nothing a client saw is lost.** The remaining ambiguity (case 2) is the
usual one for any exchange that crashes between doing something and telling you.

**A poison pill** (2.8) is journaled but never finishes: every replay panics at its `seq`,
and until a code fix, nothing restarts. Clients saw none of its results, since the gate
releases a command only with its trailer, unless it had emitted more events than the event
ring holds (131,072): then its first events were released, and the fix must reproduce
exactly those events for that `seq`, whether or not it changes `engine_semantics` (11.2).

**`StaleNonce` does not resolve it**, before or after a restart. It says only that a nonce
at or above this one was forwarded: that may have been a different message (gaps are
allowed), and before a restart it may not even be durable yet. Example: a client sends
nonce 42; the gateway forwards it; the client times out and resends; the gateway answers
`StaleNonce`; the power fails before 42's batch is synced. After the restart 42 is not in
the journal and its nonce is free again. A client that had read `StaleNonce` as "executed"
would never resend, and the order would be lost. So the only proof of execution is a
released result, or after a restart the account's state; results of case-2 commands can
be resent to clients from the replay (feeds with resume-from-seq are Later). In v1 no
client sees gateway rejects at all; the network gateway's rule for replies is in 7.4.
(Every other gateway reject depends only on the message, the fixed key registry, the
clock or momentary ring space, never on state a crash can undo.)

---

## 13. Replay and the determinism test

### 13.1 Replay

`pipeline::replay` rebuilds the engine from the journal alone:
1. Run recovery (11.8): find the valid records and make them durable. Check the journal's
   `engine_semantics` against the binary's (11.2); refuse a mismatch unless told to allow
   it.
2. Build a fresh `Engine<Book, Fast>` with `EngineOptions` from segment 0's header,
   including `id_hash_seed` (D-011).
3. For each record in order: decode its CMD40 and `engine.apply` it, with an event sink
   that writes each event, with its `seq`, into the capture format (13.2). The `seq` being
   applied is kept in a thread-local, so a poison pill (2.8) stops replay with a message
   naming that `seq` and the decoded command.
4. Also collect, per account, the highest nonce of kinds 1 and 2 (6.3), and the last `seq`
   and `ts`.

It returns the engine, the nonce table, the next `seq` and the last `ts` (19.2): all a
restart needs (13.5).

Replay does not verify signatures: the engine never needs keys, and checking them is the
separate audit of 13.4 (6.5M verifications are about 5 minutes on one thread). Replay needs
no key registry, no clock and no threads.

### 13.2 Capturing the live event stream

- With `--capture` on, the gate appends every released engine event (not trailers) as a
  64-byte slot, the same 8 words as in the event ring (`seq`, then EVT56), to a `Vec<u64>`
  reserved and pre-touched before the run (6 slots per expected command; the M3 flow
  averages about 4 events per command, measured by the smoke run), so capturing allocates
  nothing and page-faults nothing during the run. If the reservation fills, capture stops
  and the run is marked "capture incomplete"; it never grows mid-run.
- Captured in memory, not written during the run, so capture I/O can't disturb the
  journal's disk. After the run it is written to `<run_dir>/events.bin`: a 64-byte header
  (magic `PERPEVT1`, `u16` version 2, `u16` flags (bit 0: the capture is incomplete),
  `u32` deployment, `u64` event count, `u64` first seq, `u64` last seq, zeros) followed by
  the slots. First and last seq name the commands whose events the file holds: a live
  run's file holds one life's (13.5), from the life's first command to its last (last =
  first − 1 if it issued none), so a reader knows where a life started even when it
  released no event. (Version 1 had the first and last *slot's* seq there, 0 in an empty
  file; it is refused.)
- Capture costs about 64 bytes × 4 events per command: about 1.7 GB for the 6.5M-command
  headline run (2.5 GB reserved). It is on for replay-check runs and off for timing runs,
  and each result says which. The headline session runs the 100k/s signed point a fourth
  time with capture on, for the replay test, and reports that run's latencies next to the
  three timing runs (so the cost of capturing is visible, not hidden).

### 13.3 The replay test (scorecard row 3)

After a run with capture on, the harness, in the same process:
1. takes the live engine back from the core thread and calls `snapshot()`;
2. replays the journal from disk into a fresh engine (13.1), capturing its events;
3. requires: the same number of records as the sequencer issued and the core applied; the
   replay's event stream identical to the live capture, slot for slot (the first
   difference is reported with both events decoded); and `EngineSnapshot`s equal (`==`;
   the snapshot is defined field by field, RISK.md 15.5);
4. replays once more with a different `id_hash_seed` and requires the same events and
   snapshot (D-011's claim that the seed changes nothing but speed, now on a whole run);
5. reports records, events, "identical: yes/no" and the replay rate (records per second).

**When the answer is "not checked".** If the capture was incomplete, or the journal was in
discard mode (11.6), the result is "not checked", never "identical: yes" on a partial
comparison. The harness refuses to run the replay test in discard mode at all.

This is one line in the report (INFO.md 8): *replaying the 6.5M-record journal of the
100k signed/s run rebuilt identical state and an identical event stream.*

What it proves: the pipeline adds nothing to the engine's input that isn't in the journal
(no clock, no thread timing, no lost, duplicated or reordered records), and the codec
round-trips every command. What it doesn't prove: that the engine is right (the M1 and M2
property tests do that), or that the journal survives a crash: it runs after a clean stop
and reads the journal back through the page cache. The crash tests of 18.2 and 18.4 cover
durability and restarts.

A standalone form, `e2e replay --run-dir <dir>`, replays a kept journal and writes
`events-replay.bin` and `snapshot.txt` (the snapshot's `Debug` output, which is in a fixed
order) for diffing by hand. It prints the journal's commit, build profile and
`engine_semantics` next to the binary's. It compares them with what the last finished life
left (`events.bin`, `snapshot-live.txt`): the events of exactly the commands the file's
header names (13.2), so a life that started after seq 1, and one that issued no command,
compare right; an incomplete capture is "not compared".

### 13.4 Signature audit

`gateway::audit::verify_journal(dir, registries)` takes every registry file the journal's
segments name (by the SHA-256 in each header; a key replaced at a restart means two
files). For every kind-1 record it rebuilds the signed bytes (magic, version 1, reserved
0, the header's deployment, then `account`, `nonce`, `expires_at` and the CMD40) and
verifies the signature with the registry of that record's segment. It also checks:
- `account == account_of(order_id)` (the ownership rule of 5.5, which a signature alone
  doesn't prove: account 7 can sign a cancel of account 9's order);
- that nonces strictly increase per account over the whole journal (a copied record, or a
  replayed one, fails this);
- that kinds and tags fit (11.3), and it counts kind-2 and kind-3 records separately.

It runs in parallel over the journal and reports failures, which must be zero. It is part
of the headline replay check.

**In the EIP-712 scheme** (5.8; segment 0's header byte 120 says which, 11.2), the
signatures and the replays are checked another way; ownership and the kinds as above.
- For every kind-1 record the audit rebuilds the digest the client signed: the domain from
  the header's deployment, then the record's salt (the nonce word), `ts` (the expiry word)
  and CMD40 (`eip712::op_data`, `eip712::digest`). It verifies `r || s` over that digest
  with the account's registered key (`wire::verify_digest`, low-S first). The recovery id
  isn't journaled and isn't needed: a signature by that key over that digest is exactly one
  whose recovery, with the right id, gives that key, and so the account's address. The
  digest covers the whole CMD40 but a cancel's or a modify's market, which the scheme
  doesn't sign (5.8), so the audit can't see that market changed.
- In place of the nonce order: no request `(account, salt, ts, market)` appears twice in
  the whole journal (the gateway's own key, `salts::Request`; the market is the CMD40's),
  and each `ts` lies in the window around the record's own time (the sequencer's `ts`, in
  milliseconds): at most 60 s after it, and at most 5 minutes plus `SEQUENCING_SLACK_MS`
  (10 s) before it. The gateway checked the window against its clock when it took the
  message, which is not journaled. That was before the sequencer stamped the record, so
  the first bound is exact; the slack in the second allows for the time a record waits in
  its lane between the two (longer behind a slow disk, 2.4). With the market in the key, a
  copy of a signed cancel or modify with another market, which the gateway accepted as
  another request (5.8, step 1), passes next to the genuine record: both signatures verify
  over the same digest, and the requests differ. An exact repeat, the same market
  included, fails.
- Its memory is one set entry per request, instead of one map entry per account.

**What it proves, exactly:** every client command in the journal was signed by the key the
registry names for its account, and no signed message was used twice (and, in the EIP-712
scheme, none was used outside its time window, up to the slack). In the EIP-712 scheme,
"signed" covers every field but a cancel's or a modify's market, and "used twice" means the
same request, market included: a signed cancel or modify may appear once per market id,
and the replay (13.1) rejects every copy but the one on the order's own market. So a
journal whose market was changed there after the fact still passes, and its replay turns
that cancel or modify into an `UnknownOrder` or `UnknownMarket` reject, so the order stays
on the book (a test pins this). It is relative to the
registry: whoever can write both the journal and `keys.txt` can forge both. So the auditor
must trust the registry from another source (for example, each client confirming its own
public key); the header's digest only shows which file the exchange used (Q3).

### 13.5 Restarting after a crash

The whole path, in order (the APIs are in 19.2):
1. **Recover** (11.8): find the end, check the identity against the configuration (11.2),
   zero the tail, re-write and sync the kept part. Refuse to start on any error.
2. **Replay** (13.1) into a fresh engine: returns the engine, the nonce table, the next
   `seq` and the last `ts`.
3. **Start the gateways** with the nonce table (6.3), and the pipeline with the replayed
   engine, `next_seq`, the durable watermark at the last recovered seq, and the clock anchor
   `max(now, last ts + 1)` (9.2). The journal writer starts a new segment (11.8, step 4).
4. **Clients** resend what they never got a result for (12.4).

Recovery and replay both finish before any gateway accepts a message, so nothing is ever
derived from bytes that are not on disk.

**Not in the EIP-712 scheme** (5.8): `RunConfig::check` refuses `--resume` with `--auth
eip712`. A restarted gateway would need every request of the last 5 minutes, and v1 doesn't
rebuild the salt table from the journal. Recovery, replay and the audit work on such a
journal as on any other, and recovery refuses to continue it in the perp scheme (11.2).

---

## 14. The load generator

### 14.1 What it produces

`loadgen::market_flow` generates, from a seed, one ordered list of items. Each item is a
client command (account, nonce, command) or an operator command. The list is the same on
every machine and at every offered rate: the rate only changes the send times (14.9). So
runs at different rates apply the same sequence of commands, faster or slower.

Two limits on "like with like". **Different rates measure different parts of the flow:**
the window (15.5) is a fixed stretch of real time, so at rate `R` it covers flow time from
`5R/100k` to `35R/100k` seconds: about 7 s of flow and 2 jumps at 20k/s, about 350 s and
104 jumps at 1M/s. And **repetitions share the seed**, so they show timing noise, not flow
noise: the same jumps land in each. So every result prints the jumps, liquidations and
`SetMark` sweeps inside its window, and the headline point is also run once with a second
seed, to show the result doesn't hang on one seed's jumps.

The generator runs an event simulation in its own **flow time** (integer nanoseconds) to
decide the order of the items. Flow time orders the list; it is not the send schedule.

The M0 and M1 flows in `loadgen/src/lib.rs` stay exactly as they are (a test pins M0).

**Two flows, one contract.** Sections 14.2 to 14.7 are the M3 flow (D-027), the default
(`--flow m3`). Since D-034 there is a second, `--flow polymarket` (14.12): Polymarket
Perps' 88 real markets, traded with the shape of their recorded traffic. Both give a
`FlowPlan` with the same phases and the same promises: the same list on every machine and
at every rate, and a longer plan starting with every item of a shorter one. Both hold
about 100,000 client items per second of flow time, so the window arithmetic above holds
for both. The signer, the sender and the harness take either plan through one small trait,
`PlanConfig` (seed, digest, client accounts, engine options). The M3 flow's code, plan,
digest and pinned tests are unchanged.

### 14.2 Markets

67 markets, ids 1 to 67. Market `m`:
- starting fair value `F0 = 100,000 + 1,000 × m` ticks;
- price range `min_price = F0 / 2` (integer division), `max_price = 2 × F0`;
- class `m mod 3`:

| Class | `max_leverage` | Band (ppm) | Taker / maker fee (ppm) | Rule 1: `20 × Lmax × (band + max fee)` ≤ 9,000,000 |
|---|---|---|---|---|
| 0 | 50 | 8,000 | 400 / 100 | 8,400,000 |
| 1 | 20 | 20,000 | 400 / 125 | 8,160,000 |
| 2 | 10 | 40,000 | 500 / 100 | 8,100,000 |

- a one-row tier table: `lower_bound` 0, `max_leverage` = the market's `Lmax`.
- Rule 2 (RISK.md 5.3) needs `min_price` of roughly `20 × Lmax` ticks, so at most about
  1,000 here; the smallest `min_price` is 50,500.
- Book memory: 8 bytes per tick of range, `1.5 × F0` ticks per market: 1.2 to 2.0 MB each,
  about 110 MB for all 67.

### 14.3 Accounts and cohorts

| Cohort | Accounts | Ids | Deposit | Leverage | Behaviour |
|---|---|---|---|---|---|
| market makers (MM) | 4 per market, 268 | MM `j` of market `m`: `(m − 1) × 4 + j + 1` | $1,000,000 | 5 in its market (`SetLeverage`) | quotes 3 levels per side, requotes on fair-value moves |
| takers | 2,000 | 1,001 to 3,000 | $1,000,000 | 1 (the default) | IOC orders in random markets |
| high-leverage (HL) | 6 per market, 402 | 5,001 to 5,402; index `i = id − 5,001`: market `i / 6 + 1`, long if `i mod 6 < 3`, else short | $10,000 | the market's `Lmax` (`SetLeverage`) | builds a position with IOCs; the cohort meant to be liquidated |
| thin layer | 200 | 7,001 to 7,200 | $100,000 | 1 | 5 long-lived GTC orders each, far from the top |
| insurance fund | 1 | 4,294,967,295 (`FUND`) | $1,000 (small on purpose; below) | — | capitalised by a journaled `Deposit` |

Amounts are in micros: $1,000,000 = 10^12. Total client deposits about 2.29 × 10^15
micros, far inside `i64`. 2,870 client accounts need 2,870 keys.

Engine options for M3 runs: `order_capacity` 4,096 per market (about 40 orders rest per
market), `slot_capacity` 4,096 per market (at most 2,000 takers + 4 + 6 + 200 slots),
`account_capacity` 4,096, `scratch_capacity` 4,096, `id_hash_seed` derived from the run
seed (14.6). With these, the engine allocates nothing after setup (D-010, RISK.md 3.5),
which the smoke test checks (18.4).

Why these deposits: the MMs' worst-case size is about 3 levels × 1M lots plus their
position, around $400k of notional, so $80k of margin at 5x; takers at 1x accumulate
small positions in many markets; HL accounts must survive their own orders but not a
jump against them. No cohort is meant to run out of collateral, except HL after
liquidations.

**Why the fund gets only $1,000.** INFO.md asks for the run's liquidation count, fund
drawdown and peak shortfall, and the replay test should cover `InsuranceShortfall`. A
model of the flow gives the fund, per jump, about −$554 on a 50x market (3 liquidations
past the bankruptcy price), +$175 on a 20x market and +$319 on a 10x market (most of those
liquidations still have equity), so about −$400 over the 65 s headline run. With $100,000
the drawdown would be tiny and the shortfall always zero; with $1,000, a couple of 50x
jumps take the fund below zero, so the run exercises and reports the shortfall path. It is
an assumed parameter (D-027), like the rest of the flow.

### 14.4 Setup phases (journaled, not measured; INFO.md 7 "setup through the journal only")

| Phase | Sent by | Contents | Count |
|---|---|---|---|
| A | sender 0, into the operator ring, as fast as it takes them | per market in id order: `SetMarketParams`, `SetRiskTier`, `SetMark(F0)`; then `Deposit(FUND)`; then every account's `Deposit` in id order; then `SetLeverage` for every MM (5, its market) and HL (`Lmax`, its market) | 201 + 1 + 2,870 + 670 = 3,742 |
| B1 | the sender, client messages at 20k/s | every MM's 24 quotes (market, MM, side bid then ask, level order), then the thin layer's 1,000 orders | 1,608 + 1,000 = 2,608 |
| B2 | the sender, at 20k/s | 3 IOCs per HL account, in id order | 1,206 |
| timed | the sender, at the offered rate | warm-up, then the measured window (15.5) | as long as the run |

Between phases, a **barrier**: the sender waits until every item of the phase is resolved,
meaning `released commands + gateway rejects + ingress drops == items sent` (the counters
of 3.2), polling every 100 µs, with a 10 s timeout that aborts the run. (A journal that
syncs every record, the "fsync per order" arm of section 16, gets `4 × F` per item of the
phase instead, if that is longer: phase A alone needs `3,742 × F`, over 10 s once `F`
passes about 2.7 ms.) Different accounts use different lanes, whose order at the sequencer
isn't promised (9.1), so without barriers an account's first order could be sequenced
before its deposit. After setup the harness requires zero engine rejects in all of setup
(A, B1 and B2; the gate tells setup from the rest by `t_sched`, so it needs stamps on): a
run with any is invalid, since it would measure another state than the flow meant. B2's
IOCs may leave remainders, which is normal and not a reject.

**A known deviation from INFO.md 7** ("trades that build positions while the mark moves"):
B2 runs with the fair value and the mark frozen at `F0`, so all wrong-side HL accounts of a
market start with nearly the same entry (`F0 ± 10`), and the first jump against them
liquidates them together. Entries spread during the timed flow, where HL accounts keep
trading (one IOC each about every 10 s) while the mark moves, and re-enter after being
liquidated. Making B2 a flow segment with moving marks would add a generator mode for an
effect limited to the warm-up. Recorded in D-027.

### 14.5 The timed flow

State per market: fair value `F` (starting at `F0`), and for each MM quote (market, MM
`j`, side, level `k`): its order id, price and total size, as the generator last sent them.
The generator never sees the engine: it doesn't know about fills or sweeps (open loop).

Recurring generator events, by flow time:

| Event | When (flow time) | Tie rank |
|---|---|---|
| `FairStep(m)` | `phase(m) + i × 15 ms`, `i = 0, 1, …`, with `phase(m) = m × 15 ms / 67` (integer ns) | 0 |
| `MarkTick(m)` | `phase(m) + i × 100 ms`, `i = 1, 2, …` | 1 |
| `Taker` | `i × 250 µs`, `i = 1, 2, …` | 2 |
| `HighLeverage` | `12.5 ms + i × 25 ms`, `i = 0, 1, …` | 3 |
| `Withdrawal` | `i × 1 s`, `i = 1, 2, …` | 4 |
| `ThinReplace(o)` | `offset(o) + i × 30 s`, `i = 0, 1, …` (offset from setup, below) | 5 |

Events are processed in order of time, then tie rank, then market (or thin order) index.
Each event emits items as follows. Every new order gets the account's next order sequence
(`order_id(account, s)`, `s` = 1, 2, 3, … per account, places only), and every client item
gets the account's next nonce (1, 2, 3, … per account, all client messages).
`clamp` keeps a price inside the market's range; `F` itself is clamped to
`[min_price + 100, max_price − 100]`.

**`FairStep(m)`**, using the market's `FAIR(m)` and `MM(m)` random streams (14.6):
1. `FAIR(m).below(15,000) == 0` means a **jump**:
   - size = `F × FAIR(m).in_range(20,000, 60,000) / 1,000,000` ticks (2% to 6%); up if
     `FAIR(m).below(2) == 0`, else down; `F = clamp(F ± size)`.
   - emit operator `SetMark(m, F)` (a jump moves the mark at once; no noise);
   - emit a `CancelOrder` for each of the market's 24 quotes (MM `j` = 0 to 3, bid then
     ask, level `k` = 0 to 2);
   - then, in the same order, a new post-only GTC `PlaceOrder` for each at its target
     price, with size `MM(m).in_range(200,000, 1,000,000)`.
   - Stop here.
2. Otherwise `δ = FAIR(m).in_range(−3, 3)` and `F = clamp(F + δ)`.
3. For each quote in the order above, with target `F − (2 + j + 2k)` for a bid and
   `F + (2 + j + 2k)` for an ask:
   - **refresh:** if `MM(m).below(64) == 0`, emit `CancelOrder`, then a new `PlaceOrder`
     at the target with a new size draw;
   - else, if `|price − target| > k` (level 0 on any move, deeper levels only when the
     move has taken them further away): **requote**. If `MM(m).below(2) == 0`, emit
     `ModifyOrder { new_price: target, new_size: the quote's total size }` (a total-size
     modify, D-008: fills the generator can't see are subtracted by the book); otherwise
     `CancelOrder` and a new `PlaceOrder` at the target with a new size draw;
   - else nothing.

**`MarkTick(m)`:** emit operator `SetMark(m, clamp(F + MARK(m).in_range(−2, 2)))`.

**`Taker`:** from the `TAKER` stream: account `1,001 + below(2,000)`, market
`1 + below(67)`, buy if `below(2) == 0`, quantity `in_range(1,000, 20,000)`; emit an IOC
`PlaceOrder` at `F + 10` (buy) or `F − 10` (sell), not post-only. It crosses the spread and
takes the top one or two quotes, partially: taker sizes are 1% to 10% of a quote.

**`HighLeverage`:** from the `HL` stream: `i = below(402)`, quantity
`in_range(15,000, 30,000)`; account `5,001 + i` sends an IOC on its fixed side at `F ± 10`
in its market. Each HL account adds about one order per 10 s.

**`Withdrawal`:** from the `OPS` stream: operator `Withdraw { account: 1,001 + below(2,000),
amount: 1,000,000,000 }` ($1,000).

**Thin layer.** At setup B1, for each account 7,001 to 7,200 and each of its 5 orders
(order index `o = (account − 7,001) × 5 + i`), from the `THIN` stream: market
`1 + below(67)`, bid if `below(2) == 0`, percentage `p = in_range(40, 80)`, quantity
`in_range(1,000, 5,000)`, and `offset(o) = below(30,000)` milliseconds. Its price is
`clamp(F − d)` for a bid or `clamp(F + d)` for an ask, with
`d = (F × band_ppm / 1,000,000) × p / 100`: 40% to 80% of the band from the fair value,
so it rests far below the top and inside the band. **`ThinReplace(o)`:** emit
`CancelOrder` for the order, then draw side, `p` and quantity again (the market stays) and
emit a new GTC `PlaceOrder` (not post-only).

**Why jumps are handled differently.** On an ordinary step of at most 3 ticks, no
requoted MM order can cross another MM's quote: on an up-move, every other MM's ask is
still at least `F_old + 2` while a new bid is at most `F_old + 1`, and a down-move is the
mirror image. So post-only requotes are not rejected, as long as the previous step's
requotes were sequenced before this step's. Steps are 15 ms of flow time apart, so only a
run far past saturation (flow time compressed, queues long) breaks that, and its
`PostOnlyWouldCross` rejects then show in the breakdown. A jump of 2% to 6% makes every
old quote stale, so the MMs pull and requote, as real market makers do. The mark
follows at once, so the new quotes are inside the band and the high-leverage positions on
the wrong side are liquidated (at 50x any jump liquidates them; at 20x jumps above 2.5%;
at 10x above 5%).

### 14.6 Randomness

Each concern has its own SplitMix64 stream (the generator already in `loadgen`, D-006),
so adding a market or changing one cohort doesn't shift the others:
`stream(seed, id) = SplitMix64::new(SplitMix64::new(seed ^ id.wrapping_mul(0x9E37_79B9_7F4A_7C15)).next_u64())`.

| Stream | Id |
|---|---|
| `FAIR(m)` | `0x1_0000 + m` |
| `MM(m)` | `0x2_0000 + m` |
| `MARK(m)` | `0x3_0000 + m` |
| `TAKER` | `0x4_0000` |
| `HL` | `0x5_0000` |
| `THIN` | `0x6_0000` |
| `OPS` | `0x7_0000` |
| `SCHEDULE` | `0x8_0000` |
| engine `id_hash_seed` = `stream(seed, 0x9_0000).next_u64()` | `0x9_0000` |
| `CLOCK` (the Polymarket-shaped flow only, 14.12) | `0xA_0000` |
| `SHOCK` (the Polymarket-shaped flow only, 14.12) | `0xB_0000` |

Draws within a stream happen in exactly the order 14.5 lists them. All flow arithmetic is
integer, as in the M0 and M1 flows. (The send schedule, 14.9, is the one place that uses a
float; it affects timing only, never content.) The Polymarket-shaped flow reuses `FAIR(m)`,
`MM(m)`, `TAKER`, `HL`, `SCHEDULE` and the hash seed's id for the same concerns, with `m`
Polymarket's instrument id, and adds `CLOCK` and `SHOCK`; it has no `MARK`, `THIN` or `OPS`
draws.

### 14.7 Mix and rate

From the model of this generator (20 s of flow time, seed 1), per second of flow time:

| Item | Per second | Share of client commands |
|---|---|---|
| MM `PlaceOrder` (post-only GTC) | 32,461 | 32.5% (with the thin layer's 33) |
| MM `CancelOrder` | 32,461 | 32.5% (with the thin layer's 33) |
| MM `ModifyOrder` | 30,828 | 30.9% |
| taker IOC | 4,000 | 4.05% (with HL's 40) |
| HL IOC | 40 | |
| thin `PlaceOrder` / `CancelOrder` | 33 / 33 | |
| **client commands** | **about 99,860** | 100% |
| `SetMark` (cadence) | 670 | operator |
| `SetMark` (jump) | about 0.3 | operator |
| `Withdraw` | 1 | operator |

- About 100.5k commands per second of flow time, so at an offered 100k client commands/s
  flow time runs at about real time, and the parameters above (15 ms steps, 100 ms marks)
  mean what they say.
- Jumps: `67 × 66.7 / 15,000` = 0.30 a second across all markets (7 in the model's 20 s).
  Expected liquidations per jump: 3.00 on a 50x market (all 3 wrong-side HL accounts),
  2.66 on a 20x market and 0.79 on a 10x market, so 2.15 on average, and about
  `2.15 × 0.30 × 65` ≈ 42 in the 65 s headline run at 100k/s. The fund's expected result
  over that run is about −$400 (14.3). Model estimates; the run measures them.
- Trades come only from IOCs (4.05% of commands), each taking part of one or two quotes:
  a cancel-heavy flow with many orders per trade, as on real venues.
- **Rejects.** Pre-signed flow can't react to the engine (INFO.md 7), but this flow is
  built to keep rejects rare: MM quotes are 10 to 1,000 times a taker's size and a level-0
  quote is replaced by a new order about every 2.3 steps (`1/64 + 63/64 × 6/7 × 1/2` =
  0.44 a step), so they almost never fill completely (D-012's
  deep flow lost 5% of its commands to that); ordinary steps never make post-only quotes
  cross. Rejects come mainly from jumps (cancels of quotes the band sweep already removed;
  post-only places that race another MM's cancel through a different gateway), and from
  liquidated HL accounts. The model can't predict the engine's rejects exactly; the first
  run measures them, and a run above 5% is flagged (INFO.md 7).

### 14.8 Keys and pre-signing

- **Keys:** account `a`'s private key is
  `d = SHA-256("perps-loadgen key v1" || seed as u64 LE || a as u32 LE || c as u8)` read as
  a big-endian number, with `c` = 0, 1, 2, … until `1 <= d < n` (a retry has probability
  about 2^-128). `SigningKey::from_bytes(&d)`. Account 9 with seed 1: `c` = 0 and the key
  of 5.6. `keys::write_registry(path, seed, deployment, accounts)` writes `keys.txt`
  (5.4). No `rand` crate is needed (INFO.md 5 suggested `rand_chacha`).
- **Pre-signing:** every client item of phases B1, B2 and the timed flow is encoded into
  its 136-byte message (with `expires_at = u64::MAX`) and signed before the pipeline
  starts, split across `min(allowed CPUs, floor(CPU quota))` threads (each signs a
  contiguous slice; signatures are deterministic, so the split doesn't change a byte).
- **Pre-signing in the EIP-712 scheme** (5.8): `presign_eip712(plan, deployment, ts_ms,
  threads)` signs the same items as version-2 messages, on the same threads. The salt is
  the item's nonce, unique per account, so the plan and its pinned tests stay as they are
  (D-033). `ts` is one time for the whole arena, `ts_ms`: the wall clock when signing
  starts (`unix_now_ms`). The gateways compare it with the run clock, which is anchored to
  the same wall clock (9.2). Deterministic too: the same plan, deployment and `ts_ms` give
  the same arena.
- **Such an arena goes stale.** The gateways refuse a `ts` more than 5 minutes behind their
  clock, so an EIP-712 arena serves only runs that end within 5 minutes of its `ts`. Before
  each run the harness checks that the arena's age now, plus how long the run sends (its
  setup phases at 20,000 a second, then its timed flow), plus a 60 s margin
  (`FRESHNESS_MARGIN_MS`, for starting the pipeline, the barriers' waits and a late
  sender), is at most 5 minutes; otherwise it signs the arena again, with the time then, and
  says so in its log (`bench/src/e2e/workload.rs`). A new arena's `ts` is when signing
  started, so the time signing takes counts too: once signing is done, the harness checks
  the new arena the same way, and if it already fails, it refuses the run with how long
  signing took, rather than run it into `StaleTimestamp` rejects (and an invalid run,
  tried again the same way). It never signs EIP-712 arenas ahead for
  a whole sweep, which the perp scheme does (below), since they would go stale. And it
  refuses an EIP-712 run whose timed flow is over 3 minutes, which no fresh arena could
  last through. On PERPSBOX, expect a new signing before most headline and search runs:
  `messages × t_sign / effective cores`, as below, plus the digest (5.8).
- **Time:** `messages × t_sign / effective cores`. `t_sign` for `k256` is measured by the
  probe (15.10); until then, 30 to 60 µs on PERPSBOX (without `precomputed-tables`, which
  the supply-chain review left off). An SMT sibling adds little to elliptic-curve work
  (2.6), so PERPSBOX's 26 signing threads on 14 physical cores count as about 14 to 18
  cores. The headline run needs 100k/s × 65 s + 3,814 setup = 6.5M messages: about 11 to
  28 s. The 1M/s sweep point (15 s) needs 15M: 25 to 64 s. Locally (7 threads on about 3.5
  physical cores; `t_sign` 25 to 45 µs), a 20k/s run of 35 s (0.7M messages): 4 to 9 s.
  The harness prints the estimate from the probed `t_sign`, and the real time.
- **Memory, signed items:** 136 bytes per message plus an 8-byte send time, 144 bytes:
  0.94 GB for 6.5M messages, 2.2 GB for 15M. The arena is built once for the largest run a
  session needs and reused as a prefix for smaller runs and repetitions: each run starts a
  fresh engine, a fresh journal and fresh nonce state, so the same messages are valid
  again. (That is safe only because these are test keys under a benchmark deployment id,
  5.1.)
- **Memory, pre-verified items:** no signature, so a compact item: meta, nonce and the
  CMD40 (7 words) plus the send time, 64 bytes. The core-path search probes rates in the
  millions: a 3.2M/s probe of 25 s needs 80M items, 5.1 GB (it would be 11 GB at 136
  bytes), and the 1M/s sweep point 35M items, 2.2 GB. The generator is run once for the
  largest probe and reused as a prefix; the harness prints the generation time, and stops
  doubling a search where the items would pass half of the machine's memory (the report
  says so). The full sweep and the searches are for PERPSBOX (62 GB); locally they run
  scaled down.
- **Memory, the EIP-712 scheme:** each gateway's salt table too (5.8): 48 to 96 bytes per
  message it may be offered, so the signed search's memory cap counts 4 × 24 bytes more
  per item. The 100k/s headline's tables take 480 MiB over 10 gateways.
- The arena can be saved to and loaded from a file (`presigned.bin`), so a sweep doesn't
  re-sign after a restart. Optional; nothing in the harness uses it yet. A perp arena's
  file is a 64-byte header, then the messages, 136 bytes each; the EIP-712 scheme didn't
  change it:

  | Bytes | Field |
  |---|---|
  | 0..8 | magic `PERPSGN1` |
  | 8..16 | seed, `u64` |
  | 16..20 | deployment, `u32`; 20..24 zero |
  | 24..32 | message count, `u64` |
  | 32..64 | the flow config's digest |

  An EIP-712 arena's file has its own magic, `PERPSGN2`, and a 72-byte header: the same
  64 bytes, then its signing time `ts_ms` (`u64`, never 0). Loading refuses a file whose
  seed, deployment, scheme (its magic) or digest differs from the run's, or that holds
  fewer messages than the run needs; it doesn't compare the signing time, which says how
  fresh the messages are, not what they are. It also refuses a header whose magic or
  reserved bytes are wrong, or an EIP-712 header whose signing time is 0.

### 14.9 The send schedule

- The **offered rate `R` is client commands per second.** Client items get send times
  from a Poisson process at rate `R`: gaps `round(−ln(1 − u) × 10^9 / R)` nanoseconds with
  `u = (SCHEDULE.next_u64() >> 11) / 2^53`, a uniform number in [0, 1). This is the one
  use of floating point in the generator; it can only change when a message is sent, never
  what it says.
- An operator item is sent immediately after the client item before it (gap 0; at the
  phase's start if no client item precedes it), so the client arrivals are exactly Poisson
  at `R` **in aggregate**.
- **Per gateway, arrivals come in bursts.** The generator emits each market maker's
  requotes back to back (14.5: MM `j`, then side, then level), and one account always goes
  to one gateway, so a gateway receives runs of one account's messages: the review's model
  of the flow found runs of 5.1 messages on average, 7.0 weighted by message, at most 12.
  That is realistic (real market makers requote in bursts, over one connection), and it
  roughly doubles to quadruples the queueing in front of the gateways compared with a
  Poisson split (section 17). It is kept, and reported: every signed run gives the
  per-gateway queue-wait p99. Interleaving the market makers' messages within a step is the
  knob if smoother arrivals are ever wanted.
- Setup phases B1 and B2 use `R` = 20,000.
- Why Poisson rather than even spacing: real arrivals are random, and random arrivals
  queue; even spacing hides that and flatters the tail. `--arrivals uniform` exists for
  debugging and is never reported.
- **Bursts** (`--bursts median|busiest`, D-034, 14.12): a Cox process instead, a Poisson
  process whose rate is `R × m(s)` in second `s` of the phase, with `m(s)` the exponential
  of two AR(1) processes fitted to Polymarket's recorded per-second message rate. Each
  phase with client items (B1 and B2 too) draws its multipliers first and divides them so
  that their integral over the phase's expected length, `n / R` seconds, is exactly that
  length: the phase offers `R` on average and ends at `n / R` give or take the Poisson
  noise, inside the window's tail (14.12). It works with either flow, and
  it needs Poisson arrivals: `--arrivals uniform --bursts ...` is refused. Floats again,
  for timing only.
- All send times are computed before the run into a `Vec<u64>` (nanoseconds from the start
  of the phase), bursty or not, so the sender's path is the same.

### 14.10 The sender

```
t0 = now()                                  // start of the phase
pending = first operator item not yet pushed (operator items keep their order)
for each item in order:
    t_sched = t0 + schedule[i]
    while now() < t_sched { push_pending_operator_items(); spin }   // never sleeps; late items go at once
    t_sent = now()
    client item, signed mode:      g = account mod N
        if ingress[g].free(1) == 0 { ingress_full[g] += 1; continue }
        ingress[g].write(message words, t_sched, t_sent); ingress[g].publish()
    client item, pre-verified mode: same into lane[g] as a ClientRecord (source 2)
    operator item: it becomes due; push_pending_operator_items()
after the last item: keep pushing until no operator item is pending

push_pending_operator_items():
    while an operator item is due and operator.free(1) > 0:
        operator.write(item, its own t_sched, t_sent = now()); operator.publish()
```

- **Open loop:** the schedule is fixed in advance; the sender never waits for a reply or
  for ring space, and a late send goes out at once, with its original `t_sched`, so any
  lateness shows in the latencies (15.2). Latency is always measured from `t_sched`.
- **Operator items never hold up client items.** An operator item that doesn't fit stays
  pending, in order, and is pushed as soon as there is space; its latency still counts
  from its own `t_sched`. The run reports the largest operator backlog and how long there
  was one. (The draft waited for space here; then a full operator ring, which happens
  whenever the sequencer stops taking input, would have delayed every later client send,
  and the overload runs that exist to show the pipeline falling behind would have looked
  like a slow generator.)
- The sender records its own lag, `t_sent − t_sched`. If its p99 exceeds 5 µs, the run is
  flagged "generator-limited" and doesn't count (use `--senders 2`). Because the sender
  never waits for ring space, its lag can only come from its own CPU (or the OS taking it
  away), never from the pipeline being behind. Full rings are the pipeline falling behind:
  those runs count, and 15.7 decides whether they pass.

### 14.11 The two injection modes

- **Signed:** messages go through the gateways. This is the end-to-end number: signed
  orders/s, INFO.md scorecard row 2.
- **Pre-verified:** the same commands, not signed, go straight into `lane[g]` as
  ClientRecords with source 2 and no signature. They are still sequenced, journaled (kind
  2) and gated exactly as signed ones. This drives the core past what the gateways can
  verify, so the core's own maximum rate and the lower layer rows (INFO.md 8) are
  measurable.

Both modes use the same generated flow and the same lanes (`account mod N`), so the
sequencer's work is the same, and the core sees the same commands.

### 14.12 The Polymarket-shaped flow and stress switches (D-034)

On 2026-09-30 the owner approved a second synthetic flow (D-034). The M3 flow does more
costly work than Polymarket's traffic (4% takers against 0.07%, frequent 2% to 6% jumps),
but looks nothing like it, and never tests three things that could break the signed
headline or its tail: one busy account, bursty arrivals and correlated shocks. This flow
keeps Polymarket Perps' shape per message, at the volume the pipeline is measured at. The
M3 flow stays the default and the conservative stress flow, and a session reports the two
side by side. Code: `loadgen/src/market_flow/profile.rs` (the profile's types),
`polymarket_profile.rs` (the generated table), `polymarket.rs` and `polymarket/market.rs`
(the generator), `loadgen/src/schedule.rs` (bursts), `bench/src/e2e/flow.rs` (the harness's
choice of flow) and `tools/calibrate/` (the calibration).

**Chosen at start, per run.** `e2e ... --flow m3|polymarket|smoke|polymarket-smoke`
(default `m3`), with three stress switches: `--makers K`, `--shock real|stress` and
`--bursts median|busiest`.
- `--makers` and `--shock` change what the plan holds, so they are fields of the flow's
  config (`PolymarketConfig`) and of its digest. The M3 flow has neither, so the harness
  refuses both with it while reading the options (`--makers needs --flow polymarket`).
- `--bursts` changes only when items are sent (14.9). It is the run's arrivals, works with
  either flow, and leaves the plan, its digest and its signed messages as they are. It
  needs Poisson arrivals: with `--arrivals uniform` it is refused.
- Every summary records `run.flow` (`m3` or `polymarket`), `run.makers` (`default` or
  `K`), `run.bursts` (`none`, `median` or `busiest`) and `run.shock` (`none`, `real` or
  `stress`), all part of the fingerprint (15.5). A summary written before these keys
  existed reads as `m3`, `default`, `none` and `none` (`config::recorded_before`), so older
  sessions stay resumable.
- Run names add the flow and each switch, in this order and before any other suffix:
  `signed-100k-polymarket-makers3-bursts-median-shock-stress`, `signed-100k-bursts-busiest`
  (the M3 flow with bursts), `signed-100k-polymarket-eip712`. Searches add the same suffix
  (`search-signed-polymarket-makers3`, `search-core-path-polymarket`), and their
  `search.txt` records `search.flow`, `search.makers`, `search.bursts` and `search.shock`.
  So one session may hold both flows' runs and every switch's, and never reuses a run of
  other settings.
- `RunConfig::check` also runs the flow's own check (`PolymarketConfig::check`): for
  example `--makers 0`, or more client accounts than the engine's 4,096, is refused.

**The calibration** (`tools/calibrate/README.md` has it in full). The sample is about 22
hours of Polymarket Perps' public websocket data from our recorder (D-007), 2026-09-29
14:00 to 2026-09-30 15:59 UTC, with a recording gap on 2026-09-29 from 17:10 to 19:00:
books and tickers at the last frame of each server second, and every trade. The
instruments are the published list of 2026-09-30. A Python tool, standard library only,
turns it into the profile in two steps, each a pure function of its inputs:
- `extract` reads `data/` (never committed) and the first calibration's fits (copied into
  `tools/calibrate/inputs/`), and writes `profile-2026-09-30.json`, which holds derived
  parameters only (about 3 minutes on 6 processes).
- `generate` renders that JSON into `loadgen/src/market_flow/polymarket_profile.rs`, a Rust
  `static` table (under a second).
- With `--check`, either step writes nothing and fails at the first line that differs from
  the committed file. Both said "identical" on 2026-09-30.

Independent scripts re-checked the first calibration on hold-out hours, and their corrected
values are the ones used:
- maker activity across markets is flat: the sigma of ln(share) is 0.49, not 0.72;
- the majors' spread is one real tick, 97% to 99% of seconds outside the US open;
- spreads are split lognormals;
- gap tails are `10 + exp(N(mu, sd))` real ticks;
- level sizes stack at fixed clip sizes;
- (and from this repo's review, section 22) gaps are fitted on the sides that show all 20
  levels, as a flow's ladders do, and sizes are kept level by level;
- persistent jumps come 0.051 times per market-hour (the median hour; the mean, 0.14, is
  set by one episode);
- taker clusters start 0.451 times a second.

**Every number the flow draws its content from is an integer**, as in every loadgen flow:
- shares are in ppm, summing to exactly 1,000,000 where they split a whole;
- fitted parameters are in thousandths;
- spreads and moves are in hundredths or thousandths of a basis point;
- money is in micro-dollars, or whole dollars or cents where named.

A continuous distribution comes as a `Table` of equally likely values, its quantiles at
`(i + 0.5) / n`, which a draw picks uniformly. Its fitted parameters sit next to it, for
the reader. The profile's only floats are the burst models' AR(1) parameters, which only
the send schedule uses.

**The profile** (`POLYMARKET`; types, units and invariants in `profile.rs`):

| Part | What the table holds |
|---|---|
| Markets | all 88, under Polymarket's instrument ids (1 to 90). Each has its symbol; `pd` and `qd` (`pd + qd = 6`, D-004); its maximum leverage (10x on 63 markets, 5x on 9, 20x on 8, 50x and 3x on 4 each); its real tier table (1 to 8 rows, 12 distinct tables, 453 rows in all); its `max_market_notional` ($50k, $500k or $1M); its start price, the recorded mark at 2026-09-30 10:00:00 UTC; and its real grid at that price (1 tick for 76 markets, 10 for 11, 100 for XRP-USD). Also its maker and taker weights, its spread model, its dust share and its own RMS 1-s move |
| Book classes | majors (BTC, ETH, SOL); alt crypto (the other crypto of 10x or more, 24 markets); long-tail crypto (5x or less, 13); tradfi equities (42); tradfi macro (SP500, NAS100, GOLD, SILVER, WTIOIL, BRENTOIL). Per class: its spread, its gaps after levels 1–4, 5–9 and 10–19, its clip menu, and each level's sizes (1 to 20) |
| Spreads | one real tick for the six tick-bound markets (BTC, ETH, SOL, SP500, NAS100, GOLD); else a split lognormal in bps, as median / log-sd below / log-sd above: alt 5.14/0.85/0.25, long-tail 5.30/0.62/0.95, equities 4.17/0.66/0.34; SILVER, WTIOIL and BRENTOIL each have their own |
| Gaps | per class and bucket: the share of gaps of 1 to 10 real ticks, and beyond that `10 + exp(N(mu, sd))` ticks (64 values); counted on the sides that show all 20 levels (57% to 100% of sides by class), as a flow's ladders always do |
| Sizes | per class and level, 1 to 20, as recorded there (level 1 thin, a median of $1k to $3.7k; the clips at levels 3 to 10): dust ($10 to $12.50 in the books, drawn from [$10.50, $11.60)), scaled by the market's own dust share; clips of $6,250 (13% of alt levels on average, from 0.3% at level 1 to 25% at level 9) or of $25k, $50k and $100k (tradfi), with ±3% jitter; otherwise that level's background, 64 of its recorded sizes' own quantiles |
| Maker activity | level changes: 36.6% add, 36.6% remove, 13.4% size up, 13.4% size down; 52% at levels 1–5, 27.7% at 6–10, 20.3% at 11–20 |
| Moves | per maximum-leverage class (50x, 20x, 10x, 3x and 5x): no move in 62.2% to 74.2% of seconds; else a 3-part normal scale mixture in units of the market's own RMS move (10x: weights 0.630/0.343/0.027, sd 0.47/1.17/3.88). Also one shared 1,024-value table of `\|z\|` |
| Jumps | persistent 1-s moves over 50 bps: one per 70,400 market-seconds (0.051 per market-hour), sizes 50 to 123 bps (16 values, from the 276 observed) |
| Takers | 695 ppm of messages; 51.0% buys. Clusters, orders at most 50 ms apart: 0.451 starts a second at Polymarket's volume, 84% single orders, mean size 1.28, up to 54; the next order in the same market as the one before 62% of the time, on the first order's side 90%; gaps p50 7 ms (16 values) |
| Taker notional | 14.6% dust; 10.7% round sizes (11 point masses, $1,000 at 6.6%); else a two-lognormal mixture truncated below $12.50 (1,024 values) |
| Bursts | AR(1) pairs `(phi, innovation sd)`: median hour, fast 0.358/0.364 and slow 0.99788/0.0129; busiest hour, 0.251/0.218 and 0.99913/0.0054 |
| Shocks | one per 347 s over the sample (900 s in the median hour). Movers: the recorded Pareto (α 1.41, above 8 markets) clamped to 14 to 88: median 14, 90th percentile 38 (recorded: 14 and 31, at most 71); 4.1% of shocks move more than 71 markets, 3.1% all 88. Each mover's move: 5.1 (median) and 6.5 (90th percentile) of its RMS of nonzero 1-s moves (`move_rms_millibps`, the detector's unit), 11.4 bps at the median. The movers' first trades come within 52 ms of each other (the median shock) |
| Constants | 20 levels a side, a mark every 200 ms per market, a $10 minimum order (applied to every size and IOC); and the SHA-256 of the JSON the table was generated from, which the flow's digest hashes |

`profile/tests.rs` checks the table's invariants and the values D-034 quotes: 88 markets,
once each; `pd + qd = 6`; tier tables that start at 0 with bounds rising and leverage
falling; start prices on their grid and inside the engine's price limits (D-020); a real
engine accepting every market and tier table; every split summing to 1,000,000; every
table in ascending order; level 1's and level 3's median sizes against the recorded books';
and that the table names the committed JSON's SHA-256. That the Rust file is the tool's
output is checked by `generate --check`, not by a cargo test.

**Markets.** Each keeps its real decimals, maximum leverage, tier table, start price and
grid (`market_params`), with three choices of this flow's own:
- **Band** `400,000 / Lmax` ppm: 8,000 at 50x, 20,000 at 20x, 40,000 at 10x, 80,000 at 5x,
  133,333 at 3x. Polymarket's own bands (`1 / Lmax`) break our band rule 1 (D-015,
  RISK.md 5.3). These pass both rules: rule 1's `20 × Lmax × (band + fee)` is 8,029,980 to
  8,400,000 against 9,000,000, and every `min_price` is at least 2,173 ticks.
- **Fees:** the M3 flow's classes, since the profile has none: 400 / 100 ppm (taker /
  maker) at 50x, 400 / 125 at 20x, 500 / 100 at 10x and below.
- **Range:** half to twice the start price, as in the M3 flow. The fair value is kept far
  enough inside it that the band's edges stay inside too.

**Accounts.**

| Cohort | Accounts | Deposit | Leverage | Behaviour |
|---|---|---|---|---|
| makers | 60, ids 1 to 60, in 20 groups of 3. The markets are dealt to the groups heaviest first (by maker weight), each to the group with the least weight so far, so each group carries 1/20 of the maker messages within 4% (`market_makers`). 60 is a multiple of the gateway counts 1 to 6, 10, 12, 15, 20 and 30, so every gateway gets as many makers. With `--makers K` (1 to 20): ids 1 to `K`, each quoting every market | $1,000,000,000 | 5 in each market it quotes, or the market's maximum if lower | 20 post-only quotes a side in each market (below) |
| takers | 1,000, ids 1,001 to 2,000 | $10,000,000 | 1 | IOC clusters |
| high-leverage | 2 per market (176), ids 5,001 to 5,176, long then short | $100,000 | the market's maximum | 3 IOCs of $2,000 in B2; then adds of $2,000 at 100 ppm of the client messages, taken out of the IOCs' 695 (every 100 ms at 100,000 a flow-second, each account about every 18 s): a market by taker weight, then one of its two accounts |
| cascade (`--shock stress` only) | 8 per market (704), ids 7,001 to 7,704, 4 long then 4 short | $1,000,000 | the market's maximum | one IOC of $5,000 in B2, and again half-way between shocks |
| insurance fund | `FUND` | $1,000 | — | small on purpose, as in the M3 flow (14.3) |

That is 1,236 client accounts (1,940 with the stress shock), under the engine's 4,096. The
engine options are the M3 flow's: 4,096 orders, slots, accounts and scratch.

**Setup phases** (14.4, with its barriers and its zero-reject rule):
- **A** (operator, 2,306 items): per market in id order, its `SetMarketParams`, one
  `SetRiskTier` per tier row, and its first `SetMark` at the start price; the fund's
  deposit; every client's deposit, in id order; each maker's `SetLeverage` in every market
  it quotes; each high-leverage (and cascade) account's in its market.
- **B1** (3,520 client items): every market's two ladders, 20 quotes a side, market by
  market. Of market `i`'s `n` makers, the bid of rank `r` is maker `(r + i) mod n`'s and the
  ask of rank `r` maker `(r + 1 + i) mod n`'s, so which makers hold the best quotes, the
  busiest, turns from market to market.
- **B2** (528 client items; 1,232 with the stress shock): each high-leverage account's 3
  IOCs, then each cascade account's one, all at the start marks. A cohort's entries in a
  market are therefore close together: 14.4's deviation, kept on purpose here, so that the
  stress shock liquidates them together.
- 4,048 setup client items in all (4,752 with the stress shock).

**The timed flow** is an integer event simulation in flow time, like 14.5's. Events are
processed in order of time, then of this list, then of their market:

| Event | When (flow time) | Emits |
|---|---|---|
| `MarkTick(m)` | every 200 ms per market; market `i` first at `200 ms + i × 200 ms / 88` | on every 5th tick, first the 1-s fair-value step; then the mark at the fair value; then the ladders following a move |
| `ShockMove(m)` | within 52 ms of a shock | the market's move, its mark, its ladders pulled and quoted again |
| `Shock` | every 10 s, with `--shock` | nothing itself: draws the movers and schedules their moves |
| `Rebuild` | half-way between stress shocks | the last shock's movers' cascade accounts re-enter, one IOC each |
| `HighLeverage` | at 100 ppm of the client messages: every 100 ms at 100,000 a flow-second, the first half a period in | one high-leverage account adds to its position |
| `TakerFollow` | a drawn gap after the order before | the next order of a taker cluster |
| `TakerTick` | every 100 µs | a new cluster's first order, with the chance below |
| `Message` | the message clock | one maker event: one to a few maker messages |

- **Scale.** The message clock sends `messages_per_flow_second × (1 − 695 ppm)` maker
  messages a second of flow (99,931 at the default 100,000): every maker message moves it
  on, whichever event emitted it (a maker event, or ladders following a move or a shock).
  So a flow-second holds 100,000 client messages (99,997 measured), in Polymarket's
  proportions, while prices move as much as they did in a recorded second. **The count is fixed, whatever the
  offered rate.** (D-034 said "as many as the offered rate": changed while building, D-034
  and section 22.) The rate changes only the send times, so runs at different rates share
  one plan and one signed arena (14.8). At an offered 100k/s flow time runs at about real
  time, so this is Polymarket's shape at about 100 times its volume (Polymarket itself sent
  750 to 1,700 messages a second), with its real price speed. At 400k/s prices move 4
  times as fast, as the M3 flow's do.
- **Makers.** A maker event picks a market by the profile's maker weights (from `CLOCK`),
  then from `MM(m)` a side, a level band (1–5, 6–10, 11–20) and a rank in it. It is one
  of two kinds:
  - A **re-price**: the quote is cancelled, and its maker places a new one between the
    quotes around it (one remove, one add), the two gaps next to it drawn together given
    their sum: each split with the chance that two independent gaps of the profile make it
    (a Gibbs step). So the ladder keeps the profile's gaps however often it is re-priced; a
    fresh gap before the moved quote left the one after it a remainder, and cut the top
    1-tick share by up to 20 points (review, section 22). The last quote goes a gap behind
    the one before, within reach; where no two gaps of the profile make the sum, the gap
    before is drawn among the shorter ones.
  - At rank 0 it is a **spread change**: both best quotes move to a new spread around the
    fair value (the bid `floor((s − 1) / 2)` real ticks below it, the ask `s` ticks above
    the bid), the second quotes staying, the spread drawn together with the two gaps it
    leaves behind the best quotes: each spread of the market's model with its chance times
    the two gaps' (a Gibbs step again, so the book keeps the spread model's law and the
    gaps). Where no spread fits ahead of the second quotes, a fresh one: any quote left at
    or ahead of a new best moves behind it too, and every cancel comes before every place.
  - A **resize**: a total-size modify at the same price (D-008), with a fresh size draw,
    so up or down about half and half.

  Which of the two: a re-price while cancels and places are below 73.2% of the maker
  messages so far, else a resize (`MakerMix::reprice_next`). A fixed chance drifted to
  76%, since a spread change can move more quotes than the two best ones; the running
  split keeps the recorded mix. About 54% of maker events are re-prices. Levels: 52%,
  27.7% and 20.3% of changes at levels 1–5, 6–10 and 11–20. A spread change counts as two
  level-1 changes, so re-prices pick band 1–5 5/6 as often, and the bands are renormalised
  (`MakerMix::of`).
- **Quotes.** Spreads, gaps and moves are counted in real ticks of the fair value
  (`10^max(0, digits − 5)` engine ticks), and every price is rounded onto its own grid,
  away from the fair value. A size is drawn for the level the quote takes, with that
  level's shares: dust (scaled by the market's own dust share), else a clip from its
  class's menu (±3% jitter), else a draw of that level's background. Lots are rounded up,
  so a quote is worth at least what was drawn, and at least $10. A quote stays within the
  ladder's **reach**, half the band from the fair value, so inside the band of the latest
  mark. A new quote's gap is drawn among its bucket's gaps that stay within reach, so a
  ladder thins out toward the reach instead of stacking one real tick apart at its edge;
  only a quote with no room left goes to the deepest free price within it.
- **Takers.** IOCs are 695 ppm of the messages: the high-leverage adds' 100 (above) and
  the takers' 595. A taker tick every 100 µs starts a cluster with the chance that makes
  takers their 595 ppm: `R × 595 × 100 / S` ppm, with `S` the mean cluster size in ppm
  (4,649 ppm at 100,000 a second: about 46 clusters and 59.5 taker orders a second). Before
  the review the adds came on top, 795 ppm of IOCs in all (section 22).
  `TAKER` draws the cluster's size, its account (one of 1,000), its first market (by taker
  weight), its side (51% buys) and each order's notional. Each next order comes a drawn gap
  later, in the same market as the one before 62% of the time (else a market by taker
  weight), and on the first order's side 90% of the time. The notional is dust (14.6%,
  from [$10.50, $11.60)), a point mass (10.7%), or a draw of the mixture's table, capped at
  the market's `max_market_notional`. Each order is an IOC at the band's edge of the latest
  mark, rounded inward onto the grid, with its lots rounded up (so at least $10). It sweeps
  the book until filled, so its fills come from the book's real depth.
- **Prices.** Each market's fair value, which is also its mark, takes one step a second
  (every 5th mark tick), drawn from `FAIR(m)`. With 1 chance in 70,400 it is a persistent
  jump, of 50 to 123 bps either way. Otherwise, with its class's no-move share, there is no
  move. Otherwise the move is `|move| = rms × sd_k × |z|` (the market's RMS, component `k`
  of its class's mixture, `|z|` from the table), with a fair coin for the direction, in
  whole real ticks and at least one. The mark follows every change at once, so every order
  is priced against the latest mark. After a move the ladders follow it whole: every quote
  moves by the move, with its maker and size (40 cancels, then 40 places), so the book
  keeps its spread and gaps, as makers move their ladders with the price; any quote the
  band's slight change leaves beyond reach is moved within it. (Moving only the best
  quotes, as first built, stretched the gaps behind them on one side each time.) After a
  jump or a shock every quote is pulled and the ladders are built again, each rank keeping
  its maker, since requoting them one by one would cross the other side.
- **Liquidations.** At its maximum leverage a fresh position is liquidated by an adverse
  move of about `0.5 / Lmax`: 1% at 50x, 2.5% at 20x. The recorded jumps are 0.5% to 1.23%,
  so a jump liquidates the wrong-side high-leverage account only on a 50x market (4 of the
  88) and only if it is large (2 of the 16 sizes). Jumps come about 4.5 times an hour of
  flow over all markets, so the default flow almost never liquidates. The recording has no
  such jump at all: the 50x markets (SP500, NAS100, BTC, ETH) had no persistent move over
  50 bps, the 20x ones 0.011 per market-hour, the 10x ones 0.13 and the 3x and 5x ones 0.31.
  The flow applies one pooled rate (the median hour's) to every market, a simplification,
  so the default flow's rare liquidations are its artefact, not Polymarket's; per-class
  rates would remove them (the owner's call, D-034). The stress shock is the liquidation
  switch.
- **Open loop.** The generator never sees the engine (14.5). A cancel or a resize can name
  a quote a taker has filled, and after a jump or a shock a cancel can name a quote that
  the mark's band sweep removed. The engine rejects those (`UnknownOrder`), as it would
  real flow. And across accounts the sequencer promises no order (9.1): a spread change, a
  ladder following a move or a requote cancels one maker's quote and places another
  maker's at or through its price, so through the pipeline the place can reach the engine
  first and be rejected `PostOnlyWouldCross` (measured by the review: 78 of a 25-s
  pre-verified stress-shock run's places at 100k/s on 4 lanes, and 3 at 50k/s with
  `--makers 3` and bursts; never in plan order). The generator then holds a quote the
  engine lacks, and its later cancel or resize is `UnknownOrder`. Only putting a market's
  crossing cancel and place under one account would prevent it, which would change the
  flow's maker structure, so it is documented, not prevented.
- **Invariants**, checked by the tests over whole plans: per account, nonces are 1, 2, 3,
  … and order sequences 1, 2, 3, …; every cancel and modify names a quote the same account
  still holds; every price and mark is on its real grid and inside its range, and every
  order inside the band of the latest mark; the generator's own book is never crossed (in
  plan order), and every book holds 20 quotes a side at every mark.
- **Arithmetic.** Integers only; a product that could pass `i64` is computed in `i128`.
- **Complexity.** A maker event costs O(log E) for the event queue (`E`, about 100 pending
  events), O(log 88) to pick its market, and O(20) per quote it touches. The whole plan is
  generated before the run; the send path never runs this code.

**The three stress switches.**
- **`--makers K`** (`makers_k`, 1 to 20): `K` maker accounts quote every market (rank
  `r`'s bid in market `i` is maker `(r + i) mod K`'s), so each sends about `1/K` of all
  traffic, all through its one gateway (`account mod N`). With `K` = 3 that one gateway,
  not the gateway count, sets the signed ceiling (17). At most 20, the quotes a side: with
  more, the ranks run out before the makers and the rest never quote (the check refuses
  it). Measured locally: with `K` = 3 the makers sent 30% to 37% of maker messages each;
  before the rotation by market, 35%, 36% and 29%, and in a signed run the busiest sent
  35.8% of all messages.
- **`--shock real|stress`** (`ShockConfig`): every 10 s of flow time, the first at 10 s,
  `N` markets move the same way. `SHOCK` draws `N` (14 plus a draw of the clamped Pareto's
  shares: 14 with 61% chance, median 14, 90th percentile 38, all 88 with 3.1%),
  which markets (a partial shuffle) and the shared direction; then per mover its time
  within 52 ms, whether it keeps the direction, and its size. Each mover gets its mark at
  once, and its ladders are pulled and quoted again.
  - `real`: the recorded sizes, 4 to 8 of each market's own RMS of nonzero 1-s moves (uniform in
    `[4, 5.09)` half the time, `[5.09, 6.53)` 40%, `[6.53, 7.97)` 10%), about 11 bps; each
    mover keeps the shared direction with 95% chance.
  - `stress`: 2% to 6% of the price, all one way, and the cascade cohort. A mark that
    moves by that much liquidates a market's wrong-side cohort together, on 50x and 20x
    markets, and on 10x ones for moves over about 5%.

  The recorded rate is one shock per 347 s. So every 10 s is a stress setting: 6 in a 60 s
  headline window at 100k/s. A run whose flow has shocks but whose window holds none (a
  low rate, or a short window) is flagged, not refused, so a search that halves to low
  rates keeps going.
- **`--bursts median|busiest`** (`Arrivals::Cox`, 14.9): the rate multiplier is
  `m(s) ∝ exp(fast(s) + slow(s))`, each `x(s) = phi × x(s − 1) + N(0, innovation_sd²)`
  started from its stationary distribution, with Box–Muller normals from `SCHEDULE`.
  - **Normalised over each phase:** the phase's `n` client items last `T = n / R` seconds
    on average, and its multipliers (the first `ceil(T) + 1` seconds, drawn with the phase)
    are divided so that their integral over `[0, T)` is exactly `T`. So the phase ends at
    `T` give or take the Poisson noise, inside the window's tail. As first built they were
    divided by the mean of all `ceil(T) + 1` whole seconds, up to 2 s more than the phase,
    and the timed flow then ran out of items up to a second before the window closed for
    44% of seeds at the headline's shape with the median preset (32% with the busiest,
    21% at the smoke run's), leaving the window's end idle and the sender's faults unknown
    (section 22). The profile's long-run normalisation, `exp(V / 2)` with `V` the two
    stationary variances' sum, is the same thing over many hours. Over one run the slow part
    barely moves (half-life 326 s in the median hour), and its level alone could put the
    run's load 20% away from `R`.
  - An arrival spends a unit exponential draw at `R × m(s)` through the seconds it crosses.
  - Per hour of multipliers: the median preset gives about 1.6 × `R` at the 90th
    percentile of seconds, 2.4 × at the 99th and 4 × at most; the busiest preset 1.3 ×,
    1.7 × and 2.3 ×. The tests check p90 and p99 within 12% of the calibration's
    simulations, and the maximum within 20%.

**What a run reports.** The `flow.*` keys (`results.rs`, `FlowContent`) are counted from
the plan and the send schedule before the run, so they are what was *offered* in the
window; what the engine did with it comes from the gate. The report (`report.rs`) gives
them per run, in "The flow's shape in the window":

| Fact | Keys | What it means |
|---|---|---|
| The flow and its switches | `run.flow`, `run.makers`, `run.bursts`, `run.shock`, `run.flow_markets` | named in the setup lines, e.g. "the Polymarket-shaped flow (D-034) + 3 market makers + bursts (median hour) + shocks (stress)"; bursty arrivals are named as such |
| The busiest account | `flow.busiest_account`, `flow.busiest_account_messages`, `_ppm` (of the window's client messages), `_per_s`, `_lane` | the account that offered the most (the lowest id on a tie) and its gateway. Next to that gateway's busy share (`health.gateway_<g>.busy_ppm`) it shows a per-account ceiling: that gateway near 100% busy while the others have room |
| Each gateway's load | `flow.lane.<g>.messages` | the per-gateway table gains "Offered" and "Busy (CPU)" columns; a pre-verified run lists what each lane was offered |
| Takers | `flow.taker_iocs`, `flow.taker_iocs_per_s`, `flow.fills_per_ioc_milli` | the takers' IOCs (accounts 1,001 to 5,000, in either flow), and the window's fills (one per match) per IOC place offered, the takers' and the high-leverage and cascade cohorts' together: how the IOCs met the book |
| Marks | `stage.set_mark_core_service.*` | a new gate histogram, over the window: the core's own time on a `SetMark`, queueing left out. A mark that liquidates or sweeps many orders makes it long. The report gives its maximum next to `SetMark`'s core path |
| The largest command | `breakdown.window.max_events_per_command`, and `core.max_events_per_command` over the run | the most events one command made: a shock's mark liquidating a cohort, or a taker sweeping many levels |
| Concentration | `flow.markets`, `flow.markets_active`, `flow.market_top` (a symbol), `flow.market_top_ppm`, `flow.market_top10_ppm`, `flow.market_median_ppm`; `flow.taker_market_top`, `flow.taker_market_top_ppm`, `flow.taker_market_top10_ppm` | how the client messages and the takers' IOCs spread over the flow's markets; the median counts markets with none as 0. Recorded: the top 10 markets hold about 23% of maker messages and 53% of taker orders |

M3 runs write the same keys, for the M3 flow. The session report gives each flow and
switch its own headline, the M3 flow's first, and one breakdown per headline (15.7); it
lists the session's flows at the top, adds a Flow column to the searches table, and adds a
caveat on the Polymarket-shaped flow: calibrated, at about 100 times the volume, with maker
concentration a switch and shocks far more often than recorded.

**The D-034 session** (`e2e sweep --kind polymarket`; `docs/RUNBOOK-PERPSBOX.md` 7b):
- the flow's three headlines at 100k/s, interleaved: as it is, with the median hour's
  bursts, and with the stress shock, each a full headline of 15.7 (3 timing runs, a capture
  run for the replay test and the audit, a second seed): 15 runs. A template's `--makers`
  stays in all three; its bursts and shock are replaced by each variant's own;
- the signed and core-path searches of the Polymarket flow, and the signed search with
  `--makers 3`, which finds the per-account ceiling.

**One durable limit per session.** It always comes from the M3 flow's pre-verified 20k/s
runs with Poisson arrivals (`session::limit_point`), whatever flow and switches a command
sends, so every flow is judged against the same disk (15.7). A session that has run only
the Polymarket flow runs those three M3 runs first.

**Memory.**
- Books cost 8 bytes per tick of range, `1.5 × F0` ticks. The 88 start prices sum to
  7,134,135 ticks, so the levels take 85.6 MB (XRP-USD alone 18.1 MB, BTC-USD 10.0 MB),
  against the M3 flow's 110 MB.
- With the rest of the engine's reservation (per market and per account, at the M3 flow's
  capacities), the engine holds 249 MB after setup A and its prefault, against 231 MB for
  the M3 flow (measured locally, each in its own process).
- A signed item costs 144 bytes, as in 14.8. `sweep --kind polymarket` keeps four signed
  arenas of about 6.5M messages, about 0.94 GB each: the plain flow and the stress-shock
  flow, each with two seeds; the bursts variant reuses the plain flow's.
- A test checks every flow's accounts against the engine's 4,096 (the largest, the stress
  shock with 1,000 makers in 50 groups of 20: 2,880), and 40 resting quotes a market
  against 4,096 orders.

**The smoke flow** (`--flow polymarket-smoke`, `Flow::polymarket_smoke`, 18.4). At 100,000
messages per flow-second, a smoke run's 3,000 messages would cover 30 ms of flow: no mark,
no move, no liquidation. So the smoke flow holds 5,000 messages per flow-second (the signed
smoke run's rate, so its flow time runs at real time), has 40 takers and a $1 fund, and
sends a stress shock every 250 ms of flow: 2 in the signed smoke run and 24 in the
pre-verified one, each moving 14 markets or more (21 on average) by 2% to 6%. Its
liquidations and
shortfalls come from the shocks. Its engine reject share is 11% to 26% (cancels of quotes
the shocks' band sweeps or the takers removed): flagged, not asserted, as for the M3 smoke
runs.

**Measured locally** (WSL2, Ryzen 7 2700X, 2026-09-30; section 22 has the rest). The
generator after the review's fixes (section 22), applied to a real engine in plan order: no
reject in setup A, B1 or B2; 0.13% of timed client commands rejected, all `UnknownOrder`
(quotes takers had filled). Over 10 s of flow: maker messages 36.60% adds, 36.60% removes,
13.16% size ups and 13.21% size downs, at levels 1–5 / 6–10 / 11–20 52.3% / 27.3% / 20.4%;
each class's shares of 1-tick and over-10-tick gaps within 4 points of the profile's in
alt crypto, long-tail crypto and tradfi equities, except up to 7 points fewer gaps over 10
ticks after levels 10 to 19 (drawn within reach), and within 7 points for the majors' and
macro's 3 and 6 markets; levels 1, 5 and 10 at 0.7 to 1.6 times the recorded books' median
distance from the mid, level 20 at 1.1 to 2.2 times (the gaps are drawn independently,
while the recorded sides are compact or sparse); each level's median depth within 25% of
the profile's (level 1: $1.0k equities to $3.7k majors, recorded $1.0k to $2.9k); depth
per quote pooled at the median within 9% of the recorded books in every class, and at the
90th percentile within 17%; spreads within 20% of the profile's 10th, 50th and 90th
percentiles (each market's spread relaxes slowly now: over 2M events of one market, within
10%); the 60 makers 1.5% to 1.9% of maker messages each, and every gateway count dividing
60 within 5% of the mean load (10 gateways: 1.03 times the mean at most); the no-move share
within 1.5 points in every leverage class. Release runs of the harness before the review's
fixes (unpinned, a shared machine, every run invalid as generator-limited, so not benchmark
numbers): pre-verified at 50k/s, 0.030% engine rejects, the top 10 markets 23.6% of
messages (recorded 23%) and 58.9% of taker IOCs (recorded 53%), 1.20 fills per IOC
(recorded: 1.35 prints per taker order); the stress shock at 100k/s, 35 liquidations,
0.040% rejects, the longest `SetMark` core service 192 µs, at most 52 events from one
command, no hot-thread fault.

**Readings taken while building, for the owner to confirm** (D-034, "Readings taken while
building", and section 22): a fixed 100,000 messages per flow-second, every maker message
counted on the clock; shocks every 10 s; the calibrated sizes' last stretch up to 8 of a
market's RMS; 95% of calibrated movers keeping the shared direction; one account per taker
cluster; the M3 flow's fees (5x and 3x markets take the 10x class's); makers at
`min(5, Lmax)`; the $1,000 fund; no operator withdrawals in this flow; `--bursts` allowed
with the M3 flow; and almost no liquidations in the default flow. From the review (section
22): the shock Pareto clamped to 14 to 88 (median 14, the recorded one; not conditioned on
14, median 22); 60 makers in 20 groups dealt by weight (D-034 says 12); the high-leverage
adds inside the IOCs' 695 ppm, in markets by taker weight; gaps fitted on the 20-level
sides and drawn within reach; sizes by level; re-prices and spread changes as Gibbs steps;
ladders that follow a move whole; one pooled jump rate for every market, although the 50x
markets had no recorded jump.

---

## 15. Measurement

### 15.1 The clock

- `std::time::Instant` (CLOCK_MONOTONIC). One `RunClock` value, copied into every thread:
  `start: Instant` and `start_unix_ns: u64`; `now()` is `start.elapsed()` in nanoseconds as
  a `u64`. All stamps are on the same clock across threads.
- On Linux with the TSC clock source, `Instant::now()` reads the TSC through the vDSO: no
  system call, about 20 ns. The local WSL2 kernel uses `hyperv_clocksource_tsc_page`
  (checked 2026-09-29), also read through the vDSO.
- **Checked, not assumed.** The probe (15.10) records the clock source
  (`/sys/devices/system/clocksource/clocksource0/current_clocksource`) and measures the
  cost of `Instant::now()` over 10 million reads; every run prints both. On a host that is
  itself a virtual machine, or whose kernel has marked the TSC unstable, the clock can fall
  back to a source read by system call, at hundreds of ns per read, which would roughly
  double the core's and the sequencer's cost per command. So a run counts toward a headline
  only if the clock source is read through the vDSO (`tsc`, `kvm-clock`,
  `hyperv_clocksource_tsc_page`) and one read costs at most 50 ns. If a rented box fails
  that, the fallback is to stamp only every k-th command (built only if a box ever needs
  it).
- A raw TSC read would be a few ns cheaper, but it needs its own calibration to
  nanoseconds and a check that the TSC is invariant and synchronized across cores; the
  vDSO already does both. Not worth it at about 20 ns.
- NTP can slew CLOCK_MONOTONIC's rate by at most 500 ppm (0.05%); irrelevant here.
- **What measurement costs the core.** More than the clock read. The five stamp words make
  each CoreRecord 12 words, two cache lines instead of one; the trailer adds one event-ring
  slot per command (about one slot in five); and the core reads the clock once per command
  (about 20 ns) and once per batch. Against the engine's 273 ns per command (M2), that is
  an estimated 10 to 20%, and it is part of every measured number, including the maximum
  rate. **It is measured once per session:** a saturation run (pre-verified, journal
  discarded, offered at twice the found maximum) with `--stamps off`, in which the
  sequencer writes only the first line of each record, the core reads no clock per command
  and writes trailers without timings, and the gate records no latencies, against the same
  run with stamps. The difference in achieved rate is the cost of the stamps (the trailer
  stays in both, so its share remains an estimate). Moving the stamps into a separate
  sequencer-to-gate ring would remove most of it; that is worth doing only if the
  comparison shows more than about 10%.

### 15.2 Stamps, and where they live

| Stamp | Taken by | When |
|---|---|---|
| `t_sched` | the schedule | computed in advance |
| `t_sent` | sender | when it writes the message into a ring |
| `t_gw_in` | gateway | when it takes the message (also the clock for the expiry check) |
| `t_gw_out` | gateway | after verifying, before writing to the lane |
| `t_seq` | sequencer | when it gives the command its seq |
| `t_done` | core | after `apply` returns |
| `t_release` | gate | when it releases the command's trailer |

Stamps travel inside the ring records (3.3) and reach the gate in the trailer. The gate
alone turns them into latencies and records them in its own histograms, so no other hot
thread records histograms. The core's measurement work is the clock reads, the stamp line
and the trailer slot (15.1). The journal writer keeps its own histograms (flush times), and
the core keeps one (event-ring stalls); both are returned when the threads are joined. A
few single-writer counters are shared with main (3.2).

**Clock inversions.** Stamps are taken on different CPUs. The kernel uses only a clock
source whose reads agree across CPUs, but if a later stamp were ever smaller, a plain `u64`
subtraction would wrap to about 1.8 × 10^19 ns in release (landing in max and p99.9), or
panic in debug. So every stage is computed with `saturating_sub`, and the gate counts, per
stage, how often the later stamp was the smaller one. A run with any inversion is flagged
and doesn't count toward a headline.

### 15.3 The histogram

Our own log-linear histogram (`pipeline/src/histogram.rs`; owner decision 3), in the
style of HdrHistogram:

- Values are nanoseconds (`u64`). With `m` = 7 sub-bucket bits:
  - values 0 to 127 have one bucket each: `index = v`;
  - otherwise, with `e = 63 − v.leading_zeros()` (the position of the top bit),
    `index = (e − 7) × 128 + (v >> (e − 7))`.
- Each power of two `[2^e, 2^(e+1))` is split into 128 equal buckets of width
  `2^(e−7)`. Buckets are contiguous and cover the whole `u64` range: 7,424 buckets of 8
  bytes = 58 KiB per histogram.
- **Relative error:** a bucket is at most 1/128 of its lower bound wide, so reporting its
  upper bound overstates a value by at most 0.79% and never understates it.
- Examples: 100 ns → bucket 100 (exact). 1,000 ns → `e` = 9, index `2 × 128 + 250 = 506`,
  bucket [1,000, 1,003]. 50,000 ns → `e` = 15, index `8 × 128 + 195 = 1,219`, bucket
  [49,920, 50,175]. 1 ms → index 1,780, bucket [999,424, 1,003,519].
- **Recording:** one `leading_zeros`, a shift, an add, an increment; plus exact `min`,
  `max`, `count` and a `u128` sum for the mean. "Not served" is recorded as `u64::MAX`
  (15.4), which lands in the top bucket and is printed as "∞".
- **Percentile p** (as a fraction `num/den`, e.g. 99/100, 999/1,000, so no floats):
  `rank = ceil(count × num / den)`; walk the buckets from 0 adding counts until the total
  reaches `rank`; report that bucket's upper bound, or the exact `max` if it is smaller.
  On 200,000 random values the model's p50, p99 and p99.9 were within 0.05%, 0.05% and
  0.24% of the exact values. An empty histogram (count 0) reports "no data", never a
  number.
- **Merging** adds counts bucket by bucket; histograms from different threads or runs
  merge exactly.
- **A consequence to know:** because the upper bound is reported, a test "p99 < 50 µs"
  fails any true p99 from 49,920 ns up (that bucket reports 50,175). It is conservative,
  by at most 0.16% at that limit.

### 15.4 What is measured, per stage

For every command released in the measured window (15.5), from its trailer:

| Stage | Formula | Mode |
|---|---|---|
| sender lag | `t_sent − t_sched` | both |
| ingress wait | `t_gw_in − t_sent`, also per gateway (the queue in front of each gateway, 14.9) | signed |
| signature verification (gateway service) | `t_gw_out − t_gw_in`, also per gateway | signed |
| sequencer wait | `t_seq − t_gw_out` (signed), `t_seq − t_sent` (pre-verified, operator) | both |
| **core path** (INFO.md 5) | `t_done − t_seq` | both; operator commands and `SetMark`s in their own histograms |
| core service (the part of the core path not spent queueing) | `t_done − max(t_seq, previous command's t_done)`; when the core was idle it includes the hop from the sequencer (about 0.1 µs) | both; `SetMark`s also in their own histogram (`stage.set_mark_core_service`, since D-034: a mark that liquidates or sweeps many orders, 14.12) |
| command → core result | `t_done − t_sched` | both |
| durability wait | `t_release − t_done` | both |
| **signed order → durable ack** | `t_release − t_sched` | signed |
| pre-verified command → durable ack | `t_release − t_sched` | pre-verified |

The stages are reported separately, and their percentiles don't add up: the p99 of a sum is
not the sum of the p99s.

**Commands that were never served count as infinitely late.** In the two end-to-end
histograms ("command → core result" and "→ durable ack"), every client command offered in
the window that was not sequenced (dropped at ingress, or rejected by a gateway) is
recorded as `u64::MAX`; the gate adds their count at the end, from the sender's and the
gateways' window counters. So percentiles are **over offered** commands, and p99 reads ∞
as soon as more than 1% was lost. Without this they would describe only the survivors: at
the signed 1M/s point, where most messages are dropped, the survivors' p99 is bounded by
the ingress depth and looks fine. At overload points the report gives both "over offered"
and "of completed", with the drop share.

Client commands the engine rejects are included as normal results: a `Reject` is a result
the client waits for.

Plus, from the journal writer: flush time (`write` + `fdatasync`), `fdatasync` alone,
records and bytes per batch (histograms over the whole run), and flushes per second over
the window (from its flush counter, sampled at the window's edges). From the core:
event-ring stall durations and their total. From the sender: ingress drops per lane, and the largest
operator backlog and how long it lasted (14.10). From the gateways: rejects per reason
(`Busy` per command tag).

**Thread health**, per run, from the counters main samples at the window's edges (3.2):
- each spinning thread's busy share (2.2): its CPU work over wall time. The journal
  writer's leaves out the time it was blocked in `fdatasync`, which is reported next to it
  as its **fdatasync share**: with group commit one `fdatasync` covers a batch of any size,
  so that share says how slow the disk is, not how much headroom the writer has;
- the core's event-ring stall total, and the sequencer's full-ring passes (9.1), the core
  ring's and the journal ring's apart, one per pass in which the ring had less room than
  the records waiting;
- minor page faults of each hot thread during the window, which must be 0 (from field 10
  of `/proc/self/task/<tid>/stat`; each hot thread publishes its OS thread id at start by
  reading the `/proc/thread-self` link). A fault means a page was touched for the first time
  inside the window: a ring or buffer that wasn't pre-touched, or an allocation. The core
  writes the engine's reserved memory (id-map buckets, the order slab, the level bitmaps,
  the liquidation heaps, the event scratch) once before its first command and after each
  accepted `SetMarketParams` (`Engine::prefault`), since the engine reserves capacity up
  front but the operating system maps each page only when it is first written. A fault can
  also be the kernel's: memory compaction migrates pages that are already mapped, and a
  thread that touches one while it moves waits in a minor fault. Hot threads spin on their
  pages, so they meet such moves often when the kernel is compacting (after a build, say,
  on this WSL2 machine tens of thousands of pages in a 350 ms window; section 22, "Three
  intermittent test failures"). So main also reads the machine's page migrations
  (`pgmigrate_success` + `pgmigrate_fail` in `/proc/vmstat`) at the window's edges: the
  run records them (`health.page_migrations`) and its fault flag names them. Faults in a
  window without migrations are the code's; the smoke test (18.4) fails only those;
- CFS throttling during the run (2.6), which must be none;
- the effective frequency of the core's CPU (`scaling_cur_freq` where present, else
  `cpu MHz` in `/proc/cpuinfo`). Pre-verified runs spin about 5 threads and signed runs
  about 15, and this Xeon runs fewer busy cores at a higher turbo frequency (up to 3.3 GHz
  against about 2.9 GHz on all cores): a 10 to 13% difference, the size of the run-to-run
  noise. The frequency is printed so rows of the layer table can be compared honestly.

**What was the limit.** From the window's counters, in this order:
1. **Durability back-pressure:** the sequencer found the journal ring full, or the core
   waited for the event ring while the gate (which frees it only as the watermark moves)
   was below 90% busy. The journal is behind: "limited by the disk (fdatasync X% of the
   window)" if the writer spent more of the window in `fdatasync` than on the CPU, else
   "limited by journal writer".
2. The core waited for the event ring while the gate was at least 90% busy: "limited by
   gate".
3. The sequencer found the core ring full: "core-limited".
4. No back-pressure: the busiest thread, if it was at least 90% busy ("core-limited" if it
   is the core, else "limited by <thread>"); if none was, "not saturated (busiest: <thread>
   at Y%)".

So a label never blames a thread that had room to spare, nor the journal writer for the
time the disk takes. The core-path search runs with the journal discarded, where the
writer never waits in `fdatasync`; its saturation run (15.7) must say "core-limited" for
its result to be reported as the core's number.

Each histogram is reported as count, p50, p99, p99.9 and max.

### 15.5 Runs: warm-up, window, repetitions

- A run: setup (14.4), then the timed flow at the offered rate `R`: **warm-up 5 s**, then
  the **measured window, 30 s** (60 s for the headline 100k signed run; 10 s at the signed
  500k and 1M points, where the gateways are far past saturation and longer runs only cost
  pre-signing), then the pipeline drains and stops.
- The window is defined by `t_sched`: a command counts if its scheduled send time is
  inside `[t0 + 5 s, t0 + 5 s + window)`. Warm-up fills caches, the rings' and journal
  files' pages, and the books' depth, and is excluded (INFO.md 8). (The rings are also
  pre-touched at creation, 3.1, so that low-rate runs, which may not go round a ring
  within 5 s, don't take first-touch faults in the window.)
- **Achieved rate:** signed (or pre-verified) client commands in the window that were
  sequenced, divided by the window length. "Signed orders/s" counts every sequenced signed
  command, accepted or rejected by the engine (INFO.md 7).
- **Repetitions:** every reported point is run 3 times, each with a fresh pipeline and
  journal. The report gives the median and the range of each percentile, over the valid
  runs only. An invalid run (15.7) is run again (as `<point>-r<k>-a2`, `-a3`; at most 3 new
  attempts per repetition and command), and it is never dropped silently: the report lists
  it with its reason, uses it nowhere else, and says per point how many runs were valid
  (BENCHMARKS.md rules).
- **Order.** On a shared rented box, slow drift (neighbours, heat) would bias whichever
  configuration runs last, so repetitions and arms are interleaved (A B C, A B C, A B C),
  never A A A, B B B.
- **Resumable.** The harness writes one `summary.txt` per run and skips runs already done,
  so a session that dies can continue where it stopped. A finished run is reused only if it
  ran with the same comparable settings (every setting that changes what is measured: the
  mode, rate, flow, timings, gateways, CPU layout options, journal settings and so on, as
  `run.*` keys in its summary); a mismatch is refused, naming the setting, rather than
  reporting another configuration's numbers. So every command of one session takes the
  same options. Since D-034 the flow and its stress switches are keys of their own
  (`run.flow`, `run.makers`, `run.bursts`, `run.shock`, next to `run.flow_digest` and
  `run.arrivals`), and part of the run's name, so one session may hold both flows' runs
  (14.12).

### 15.6 The offered-load sweep (INFO.md M3 row)

Both modes, at offered 20k, 100k, 500k and 1M client commands/s, 3 repetitions each, on
the real journal. For each point: achieved rate, drops and rejects by reason, and every
stage of 15.4. The pre-verified 20k/s point runs first in the session: its `fdatasync` p99
sets the durable limit (15.7).

**The signed 500k and 1M points are overload tests.** They are beyond what the gateways
verify, so 80 to 90% of messages are dropped. Dropped cancels leave quotes the generator
believes gone; dropped places make later cancels and modifies fail with `UnknownOrder`;
books grow, can pass `order_capacity` (so the engine allocates on the hot path), and market
makers can hit `InsufficientMargin`. So the engine does different work there. These points
are reported as achieved rate, drop share and "over offered" percentiles (15.4); their
reject share is expected above 5% and marked as expected; they are left out of the
zero-allocation claim; and their stage latencies are not compared with the passing points.

### 15.7 Maximum rate at a latency limit

One procedure for every "maximum sustained rate at which p99 stays under its limit"
(INFO.md 8). Inputs: a mode, a latency stage, a limit.

1. **A run at rate `R`** is 5 s of warm-up plus 20 s measured. It **passes** if the stage's
   p99, over offered commands (15.4), is under the limit **and** at least 99.9% of the
   offered client commands were sequenced (no more than 0.1% dropped at ingress or
   rejected at a gateway). The limit applies to the client-command histogram (operator
   commands have their own). A run with CFS throttling, a clock inversion or a
   generator-limited sender is **invalid**: it is discarded, reported, and run again (up to
   3 new attempts per search command; a search started again goes on with the next attempt,
   rather than reading the same invalid runs back).
   A run whose lanes or operator ring filled because the pipeline fell behind is valid,
   and it passes or fails on the two conditions like any other: that is the pipeline's
   limit, not the generator's.
2. **Double:** 1 run each at 100k, 200k, 400k, … until a rate fails. If 100k fails, halve
   down instead. (The fsync-per-order search starts at the probed `1/F` instead; 16.)
3. **Bisect** three times between the last pass and the first fail, 1 run each.
4. **Confirm:** run the highest passing rate and the lowest failing rate 3 times each,
   interleaved. The result is the highest rate that passed all 3 runs; if a repetition
   fails, step down to the next lower probed rate and confirm that.
5. **Report** the result, its median p99, the first failing rate, the resolution (the gap
   between the result and the first failing rate: after three bisections, 1/8 of a
   doubling step, 12.5%, and more if a confirmation stepped down), the **saturation
   throughput** (the achieved rate when offered twice the result) and what limited that
   saturation run (15.4). If the saturation run achieved 99% or more of what it was offered,
   nothing saturated there: the report says "not saturated at 2×", gives the achieved rate
   as a lower bound, and the highest rate any probe achieved.

**What "maximum rate at p99 < 50 µs" means in practice.** For a core with a steady service
time of about 0.5 µs, a queueing model (M/D/1, simulated) puts p99 at about 40 µs at 97%
utilisation and about 57 µs at 98%. So the core-path result is roughly the core's peak
throughput times 0.97, and the report gives both, since that is an obvious question.

Searches run in M3:

| Row (INFO.md 8 layer table) | Mode | Stage | Limit | Journal |
|---|---|---|---|---|
| + pre-trade risk check | pre-verified | core path | 50 µs | discard (11.6), so the disk can't hold the core back |
| + journal (group commit) | pre-verified | pre-verified command → durable ack | the durable limit | real disk |
| + gateway with signature verification | signed | signed order → durable ack | the durable limit | real disk |

These search the M3 flow. Since D-034 the same searches run on the Polymarket-shaped flow
too (`--flow polymarket`, 14.12), named with its suffix (`search-signed-polymarket`,
`search-core-path-polymarket`), and the signed one once more with `--makers 3`
(`search-signed-polymarket-makers3`): with three market makers, the busiest account's one
gateway is the likely limit, so that search finds the per-account ceiling (17). Each
`search.txt` records its flow and switches (`search.flow`, `search.makers`,
`search.bursts`, `search.shock`), and the report's searches table has a Flow column.

**Which path the "+ pre-trade risk check" row reports.** INFO.md 8's table labels that row
"pre-verified command → core result" (`t_done − t_sched`), while INFO.md 5 and the M3 row
put the 50 µs limit on the core path (sequencer in → core result). The search uses the core
path, as the M3 row says; REPORT.md's row is labelled "core path (sequencer in → core
result)", and BENCHMARKS.md also gives "pre-verified command → core result" at that rate.

**The durable limit** (Q1, answered by the owner on 2026-09-30): `2 × T` plus the median,
over the 3 valid runs of the pre-verified 20k/s sweep point (which runs first in the
session), of the `fdatasync` p99, printed next to every result that uses it. With `T` = 1 ms and an
`fdatasync` p99 of 0.5 ms, the limit is 2.5 ms. During the `T` sweep the limit stays at its
default-`T` value, so it doesn't move with what is being measured. The point is always the
M3 flow's, with Poisson arrivals, whatever flow and switches the command at hand sends
(`session::limit_point`; D-034): the limit is the disk's, so a session has one, and every
flow's headline and searches are judged against it.

**The headline, defined.** "100k signed orders/s end to end: yes" if the signed run at
offered 100k passes in all 3 repetitions, each a valid run (15.4, 15.7) whose clock counts
toward a headline (15.1): at least 99.9% of offered commands sequenced, signed order →
durable ack p99 (over offered) below the durable limit, sender lag p99 at most 5 µs, no
throttling and no clock inversions. Until 3 valid timing runs exist, the answer is "not
decided yet", never yes. The engine's reject share is reported next to it. A session gives
one headline per flow (with its switches) and per signing scheme, each judged on its own
timing runs: the M3 flow first, and within a flow the perp scheme first (5.8, 14.12). Each
headline names its flow: "100k signed orders/s end to end on the M3 flow (D-027): yes".

### 15.8 Durable latency and the fsync time

Every signed and pre-verified run reports "→ durable ack" with the journal writer's
`fdatasync` p50, p99 and p99.9 next to it, and flushes per second over the window. The
headline line is
the 100k/s signed run: *signed order → durable ack p50 / p99 / p99.9, with fdatasync p50 /
p99*.

**`T` sweep** (evidence for the commit interval, D-025): pre-verified at 100k/s, `T` in
{0, 250 µs, 500 µs, 1 ms, 2 ms}: durable ack p50/p99, the rate released (durable) per
second, flushes per second over the window, fdatasync p99.
`T` = 0 ("flush whenever the last flush is done", 11.5) is there to answer "why wait up to
a millisecond on purpose?": it has the same batching, lower latency (`F` to `2F`) and up to
`1/F` fsyncs a second. D-025's choice of `T` cites this comparison.

### 15.9 Per-run breakdown (INFO.md 7 and M3 row)

From trailers and events, over the window (and separately over setup):

- **Commands:** by type (place GTC, place IOC, cancel, modify, each operator type), by
  outcome: accepted, or rejected with each reason; the reject share of client commands,
  flagged above 5%.
- **Before the core:** ingress drops, and gateway rejects by reason.
- **Results:** fills (count, lots, notional), cancels by reason, modifies.
- **Flow content in the window:** jumps, liquidations and `SetMark` sweeps (14.1).
- **Liquidations:** `Liquidation` events, `InsuranceAbsorb` events.
- **The fund:** the gate tracks fund equity exactly, as `i128`: fund balance (from
  `BalanceChanged` for `FUND`) plus, per market, `fund_pos × mark − fund_cost` (from
  `PositionChanged` for `FUND` and `MarkPrice`), updated in O(1) per event and sampled at
  every command's trailer. Reported: starting capital, lowest equity, **largest drawdown**
  (largest fall from a previous peak), final equity, and **peak shortfall** (the largest
  `InsuranceShortfall.uncovered`). At the end, the gate's final fund equity must equal the
  snapshot's `fund_balance + fund_upnl_total`, a free consistency check.
- **Pipeline:** events per command (mean and max), largest single command in events, core
  stall time, journal bytes and flushes, events captured, operator backlog, thread health
  (15.4).
- **The flow's shape** (since D-034, for either flow; 14.12 lists each fact): the busiest
  account's share and rate, and its gateway's busy share; what each gateway or lane was
  offered; the takers' IOCs a second and the fills per IOC; the longest `SetMark` core
  service; and how concentrated the messages and the takers' IOCs were across markets.
  Counted from the plan, so what was offered in the window.

### 15.10 Machine probes (run before a session's benchmarks)

`e2e probe` prints the machine row for BENCHMARKS.md and these measurements:
- CPU model, physical cores and siblings, CPU packages, allowed CPUs, the cgroup CPU quota
  (v2 or v1; `max` means none) and throttling counters, the kernel;
- the clock source and the cost of `Instant::now()` (15.1);
- **the run directory's file system**: from `/proc/self/mountinfo`, the mount whose mount
  point is the longest prefix of the run directory's path (read with `std::fs`); its type,
  its options, and for an overlay its `upperdir`. It refuses tmpfs, ramfs and an overlay
  mounted with `volatile`, and notes copy-on-write file systems (ZFS, btrfs), where
  preallocation has no effect (11.6);
- **signature costs**, single thread: verify with every verifier the binary has (`k256`,
  and libsecp256k1 in a build with `--features c-secp256k1`, 5.7), `k256`'s sign, and
  SHA-256 of 72 bytes (also as criterion microbenchmarks, `bench/benches/pipeline_parts.rs`);
- **verify scaling:** verifications per second with 1, 2, 4, … threads on physical cores,
  then on SMT siblings too, one curve per verifier. This is the evidence for the gateway
  count (section 17);
- **the EIP-712 scheme's costs** (5.8), single thread: `<verifier>.recover_ns`, the
  gateway's whole signer check (checks 11 to 13: low-S, the digest, the recovery and the
  address, through `wire::check_signer`) with every verifier the binary has;
  `eip712.digest_ns`, the digest of a place (its MessagePack form and three keccak-256
  hashes); and `keccak.64_ns`, one keccak-256 of 64 bytes (what the address costs). Also
  as criterion microbenchmarks (`k256/recover`, `libsecp256k1/recover`, `eip712/digest`,
  `keccak/64_bytes`, ...);
- **recover scaling:** the same curve as verify scaling, counting signer checks,
  `scaling.<verifier>_recover.<i>.*`, one per verifier;
- **jitter:** 10 s on every candidate physical core at once (on one hyperthread each): the
  largest gap between consecutive clock reads, and how many gaps exceed 10 µs. The quietest
  two get the core and the sequencer (2.6);
- **fsync probe:** 30 s of journal-like writes (1 ms batches of 15.2 KB, as at 100k signed
  orders/s) into the run directory, reporting the `fdatasync` histogram. A p50 under about
  20 µs is flagged "suspiciously fast: a device with power-loss protection, or a flush that
  does nothing", and the report says which it is believed to be, and why.

### 15.11 What goes into docs/BENCHMARKS.md

The harness writes `<run_dir>/report.md`, a Markdown fragment in the file's existing style
(the container's source mount is read-only, so the fragment is pasted in by hand, as for
M1 and M2), and a plain `key = value` `summary.txt`. The M3 section contains:

1. What was measured, the machine, the environment, the commit, the CPU layout, the
   probes of 15.10 (clock source, file system), and the signing scheme and verifier of the
   signed runs (5.7, 5.8).
2. The offered-load sweep, per mode: per rate, achieved rate, drops and rejects, and p50 /
   p99 / p99.9 / max per stage ("over offered" and "of completed" at overload points).
3. The three searches of 15.7, each with the rate, its p99, the limit, the limiting thread
   and the saturation throughput; the core-path one says "journal discarded".
4. **The headline** as defined in 15.7: yes or no; if yes, its signed order → durable ack
   p50 / p99 / p99.9 with the fdatasync time; if no, section 17's analysis.
5. The `T` sweep.
6. The per-run breakdown of the headline run, including the fund's drawdown and peak
   shortfall.
7. The replay result (13.3, from the fourth, capture-on headline run) and the signature
   audit (13.4); the headline point once more with a second seed (14.1).
8. The two ablations (section 16).
9. The cost of measurement (the `--stamps off` comparison, 15.1).
10. Caveats: container, no core isolation, frequency, quota, the disk and its file system,
    "durable as reported by <file system> on <machine>; not power-loss tested", anything
    discarded and why.
11. **The Polymarket-shaped flow** (D-034, 14.12), next to the M3 flow's rows: its three
    headlines (as it is, with bursts, with the stress shock), each with its flow's shape
    (busiest account and its gateway, the longest `SetMark` core service, the most events
    of one command, takers, concentration across markets) and its capture run's replay
    and audit; its signed and core-path searches; and the signed search with 3 makers,
    the per-account ceiling. The caveat that it is calibrated from one day of public data
    at about 100 times its volume.

**The session's budget.** At the minimum (zero-filling at about 1 GB/s, no reruns): the
sweep about 16 minutes, the three searches about 25, the `T` sweep about 10, the "verify on
core" ablation about 35, the "fsync per order" ablation about 65, and the headline runs,
the replay, the audit and the probes about 15: about 3 hours of box time. The harness
prints its plan and its estimate before it starts. The D-034 pass adds about 50 minutes
with `k256` and 45 with libsecp256k1 (`docs/RUNBOOK-PERPSBOX.md` 7b).

---

## 16. Ablations (INFO.md 8, scorecard row 4)

INFO.md lists three; the naive margin loop was done in M2. M3 does the other two. They are
named by what they switch off, "verify on core" and "fsync per order", because
BENCHMARKS.md already uses "ablation A" and "ablation B" for M2's (the naive margin loop,
and the liquidation index against a full rescan).

**"Verify on core": signature verification on the core thread vs on the gateways.**
- Mode `--verify-on-core`: gateways do every check of 7.1 except 11 and 12 (`HighS`,
  `BadSignature`), use up the nonce and forward the message with its nonce, expiry and
  signature; the core thread rebuilds the 72 signed bytes, checks low-S and verifies the
  signature just before applying the command, with its own copy of the registry. Both arms
  use the 3-line ablation record (3.3), so record size is not a hidden difference.
- In this mode a bad signature aborts the run: the pre-signed flow has none, and the mode
  exists only to measure cost. It is not a secure design (the nonce is used up before
  verification), so only `e2e ablate` can turn it on; the pipeline's config field is named
  `ablation_verify_on_core` and documented as insecure.
- The perp scheme only: the core's verifier rebuilds that scheme's 72 signed bytes.
  `RunConfig::check` and `Pipeline::start` refuse the ablation with `--auth eip712`
  (5.8).
- Runs: signed mode at offered 5k, 10k, 20k, 40k and 100k/s, both ways, 3 repetitions,
  interleaved; plus the signed durable-limit search (15.7) both ways.
- Reported: achieved rate, core path p50/p99, signed → durable ack p99, maximum rate, and
  for each arm the **cores used** and **verifications per core-second**. The gateway arm
  verifies on `G` cores and the core arm on one, so the result shows what parallel
  verification and a free core path are worth, not a cheaper verification.
- Expected: with verification on the core, every command's core path includes one
  verification (40 to 80 µs est.), which by itself uses up most or all of the 50 µs
  budget; and the core arm's maximum is about `1 / (t_verify + t_core)`, some 12k to
  25k/s. So its 20k, 40k and 100k points are overload points, reported as such (15.6),
  against a gateway-limited maximum that grows with the number of gateways.

**"Fsync per order": one `fdatasync` per command vs group commit.**
- Pre-verified mode (so gateways don't limit), real disk. Three arms: per order (`B` = 1:
  every record written and `fdatasync`ed on its own), flush-when-free (`T` = 0,
  `B` = 4,096) and timed group commit (`T` = 1 ms, `B` = 4,096, the default).
- Offered 1k, 2k, 5k, 10k, 20k, 50k and 100k/s, 3 repetitions, interleaved; plus the
  pre-verified durable-limit search for each arm. The per-order search starts at the
  probed `1/F` rather than at 100k (it can't pass above that), and a run whose backlog
  hasn't drained 10 s after its window is stopped and counted as a fail: at `B` = 1 a full
  journal ring would take `65,536 × F`, about 20 s, to drain.
- Reported: the rate sequenced and the rate released (durable) per second, durable ack
  p50/p99, flushes per second over the window, fdatasync p99. The rate sequenced alone
  would mislead: the 65,536-slot journal ring takes in the per-order arm's backlog, so it
  sequences near the offered rate while only about `1/F` a second become durable.
- The per-order arm journals setup one `fdatasync` per record too, so its setup barriers
  may wait `4 × F` per item of the phase, not just 10 s (14.4).
- Expected: per order saturates at about `1 / F` commands/s (2,000/s at `F` = 0.5 ms),
  with latency growing without bound past it. Both group-commit arms batch, so they reach
  rates hundreds of times higher. `T` = 0 waits `F` to `2F` and may issue up to `1/F`
  fsyncs a second; `T` = 1 ms waits about `T/2 + F` and issues at most 1,000. D-025's
  choice of `T` cites this.

---

## 17. What 100k signed orders/s needs

A budget per stage at 100k signed orders/s on PERPSBOX. Numbers marked "est." are
estimates until the probes of 15.10 replace them.

| Stage | Cost per command | Threads | Load at 100k/s |
|---|---|---|---|
| sender | about 0.05 µs (est.) | 1 | 0.5% of a core |
| gateway: verification | `t_v` = 40 to 80 µs (est., `k256`, Broadwell at 2.4 to 3.3 GHz) | `G` | `100k × t_v / G` |
| gateway: everything else (decode, lookups, SHA-256 of 72 bytes, rings, 2 clock reads) | about 0.7 µs (est.) | | |
| sequencer | about 0.15 µs (est.) | 1 | 1.5% |
| core | about 0.5 µs (M2: 0.27 µs for the engine on the deep flow, plus decoding, event encoding and measurement, 15.1) | 1 | 5% |
| journal writer | about 0.1 to 0.3 µs of CPU per record (slicing-by-8 CRC, 11.4), plus 1,000 `fdatasync`s a second | 1 | disk-bound: 15.2 MB/s |
| gate | about 0.1 µs per slot, about 5 slots per command | 1 | 5% |

**Gateway threads needed, before bursts:** `G = ceil(R × (t_v + t_other) / u)`, with `R`
the signed rate, `t_other` about 0.7 µs, and `u` = 0.7 the highest utilisation we plan for
(above it, queueing in front of the gateways grows fast). At 100k/s:

| `t_v` | Cores of verification | `G` |
|---|---|---|
| 40 µs | 4.1 | 6 |
| 50 µs | 5.1 | 8 |
| 60 µs | 6.1 | 9 |
| 80 µs | 8.1 | 12 |
| 100 µs | 10.1 | 15 |

PERPSBOX's layout leaves 10 physical cores for gateways (2.6), so by this formula 100k/s
fits on physical cores alone if `t_v <= 0.7 × 10 / 100k − 0.7 µs` = 69 µs. Beyond that,
the SMT siblings (`--gateway-smt`, up to 19 gateways within the quota) add whatever the
verify-scaling probe shows.

**In the EIP-712 scheme** (5.8) the cost per message is the whole signer check, `t_recover`
(the digest, the recovery and the address; the probe's `<verifier>.recover_ns`), in place
of `t_v`, and the report uses it and the recover-scaling curve. Locally (release, WSL2,
Ryzen 7 2700X) it is 90.7 µs with `k256` and 46.7 µs with libsecp256k1, against 78.5 and
39.3 µs to verify a perp message in the same run. By the formula, 100k/s then needs 14
gateways with `k256` and 7 with libsecp256k1. So on PERPSBOX's 10 gateway cores, 100k/s in
this scheme looks within reach only with libsecp256k1, unless the box is much faster per
core than the local machine; the runbook's EIP-712 passes measure it
(`docs/RUNBOOK-PERPSBOX.md`, section 7). About 1.7 µs of each check is keccak-256 (5.8).

**Bursts make it harder than the formula says.** The formula treats each gateway's
arrivals as random. They aren't: a market maker's requotes arrive back to back at its one
gateway (14.9), and a burst of 12 messages takes `12 × t_v` to verify on one core whatever
`G` is: 0.6 ms at `t_v` = 50 µs. Two models of the flow at 100k/s with `t_v` = 50 µs and
`G` = 10 (utilisation 0.5) put the mean time a message spends waiting for and inside a
gateway at 184 to 299 µs, against 80 µs if the same messages were spread at random, and
its p99 at 0.46 to 1.0 ms, against 0.23 ms. The durable limit (about 2.5 ms, Q1) has to
hold that and the durability wait (up to `T + F`, about 1.5 ms) together. So 100k/s can
fail where the formula says it fits, and **the signed durable-limit search decides, not
the formula**. Every signed run reports the per-gateway queue-wait p99 (15.4).

**One busy account sets its own ceiling** (D-034's `--makers K`, 14.12). An account's
messages are all verified on its one gateway (`account mod G`), so an account that sends a
share `s` of all messages fits only while `R × s × (t_v + t_other)` stays under one core,
however large `G` is: `R <= u / (s × (t_v + t_other))`. With three market makers
(`s` about 0.36, the busiest measured locally) and `t_v` = 50 µs, that is about 55k/s at
full load and 38k/s at `u` = 0.7; with the local `k256` cost (78.9 µs) about 35k/s and
24k/s. The signed search with `--makers 3` measures it, and each run reports the busiest
account's rate and its gateway's busy share. v1 routes by account, so this is the ceiling
for one market maker that sends a third of all traffic, whatever the gateway count.

**What the report says if it doesn't fit.** The measured `t_v` (in the EIP-712 scheme,
`t_recover`); the verify-scaling (or recover-scaling) curve; the highest signed rate that
passed the durable-limit search, with `G` gateways;
the `G` that 100k/s would need by the formula; the per-gateway queue-wait p99, which shows
whether the gateways' count or the bursts were the limit; and the levers left (INFO.md 5a):
1. **Bitcoin Core's libsecp256k1** through the rust-bitcoin `secp256k1` crate: reviewed
   and built as an opt-in verifier (5.7, D-032), and measured against `k256` on this
   machine by the runbook's second pass. The pipeline doesn't change, only the gateway's
   verify call; the report uses the `t_v` and curve of the verifier its headline ran
   with.
2. **One signature per batch:** a client signs a batch of `b` orders once, cutting
   verification per order to `t_v / b`. It fits exactly the bursts above (a market maker's
   requotes for one move, signed once), but it needs a new message type (Later, not in v1).
3. A box with more cores.

The report does not claim 100k/s on the strength of the pre-verified numbers: the
headline is the signed row.

---

## 18. Tests

All run inside the container (`./dev cargo test --workspace --locked --offline`). Tests
that sign or verify need `k256` compiled with optimisations, or they take minutes in a
debug build: see C33.

### 18.1 Unit tests

**Codec** (`pipeline/src/codec.rs`):
- every command and event variant round-trips, at zero, typical and extreme values
  (`i64::MIN`/`MAX` in every 8-byte field, `u16::MAX` markets);
- every enum code is pinned to the numbers in 4.1;
- decoding rejects each reserved byte set to 1 in turn, tag 0, tags above 9 (commands) or
  14 (events), `side`/`tif`/`post_only` of 2, reason codes out of range;
- the worked example's CMD40 bytes (5.6).

**Signed messages and the gateway** (`gateway/`):
- encode, then decode, for place, cancel and modify;
- the known-answer test of 5.6: key from seed 1 and account 9, public key, SHA-256,
  `r` and `s`;
- `Gateway::check` accepts the valid message, and rejects each of: wrong magic, version 2,
  wrong deployment (`WrongDomain`); nonzero reserved bytes (`Malformed`); tags 4 to 9
  (`OperatorOnly`); wrong gateway; `account ≠ account_of(order_id)` (`NotOwner`); unknown
  account and `FUND` (`UnknownAccount`); nonce equal to and below the last
  (`StaleNonce`); a nonce `2^32 + 1` above the last (`NonceJump`; exactly `2^32` above is
  accepted); `expires_at` one nanosecond before the clock (`Expired`; equal is accepted);
  a place or a modify with 64 free lane slots and a cancel with none (`Busy`), and a
  cancel with one free slot (accepted); the high-S twin (`HighS`, before the verifier is
  called); `r` = 0, `s` = 0, `r` = `n` (`BadSignature`); a signature by another account's
  key (`BadSignature`); a message signed for deployment 2 with its field edited to 1
  (`BadSignature`); an `expires_at` byte flipped after signing (`BadSignature`);
- nonce bookkeeping: `Busy` and `Expired` leave the nonce unused, so the same bytes are
  accepted once space frees (or, for `Busy`, while not expired); a forged message with a
  nonce just above the last leaves the nonce unused; an accepted nonce 5 makes nonces 5
  and 3 stale and 7 fine (gaps allowed);
- the registry loader: a good file, a duplicate line, an unsorted file, a bad point, the
  `FUND` account, a different deployment, a verifier the build doesn't have;
- **the two verifiers** (5.7): in a build with `--features c-secp256k1`, the cross-check
  of 5.7, and every test above, with libsecp256k1 verifying.

**The EIP-712 scheme** (5.8; `gateway/`, and its parts in `pipeline/`). Every value below
that comes from outside was copied from its source (5.8, "Sources"), never computed by us:
- **keccak-256** (`keccak.rs`): of `""` and `"abc"` (go-ethereum); the Keccak team's two
  worked permutations (XKCP: the all-zero state, then its output permuted again); the same
  sponge with SHA-3's padding byte against NIST's SHA3-256 answers at 0, 1, 135 and 136
  bytes (the edge of a block, which the keccak answers above don't reach); and a two-block
  input, the EIP-712 Mail example's domain separator (160 bytes);
- **MessagePack** (`msgpack.rs`): every size boundary of the forms in 5.8; the bytes of
  golden vectors 1, 3, 4, 6 and 7, written by the writer and hashed to the file's `data`;
  a string of 65,536 bytes and a write past the buffer's end both panic;
- **Polymarket's golden vectors** (`eip712.rs`): all 12 (the ten operations, and the
  owner-signed `CreateProxy` and `Withdraw`): each `data`, each signature byte for byte
  (RFC 6979 from the file's key), and the golden address recovered by every verifier the
  build has; the key's address derived from the key; the TypeScript SDK's two `data`
  vectors (`"100.50"`, and a long negative margin);
- **EIP-712's Mail example** in full: the type hash, the encoded data, the struct hash, the
  domain separator, the final hash, the address of the key `keccak256("cow")`, and its
  `v` = 28 signature byte for byte, recovered by every verifier;
- **our forms**: both type hashes recomputed from their strings; the domain separator and
  the digest spelled out field by field for four chain ids, and each input (chain, data,
  salt, `ts`) changing the digest; minimal decimals from 0 to `i64::MIN` and `i64::MAX`;
  the order id as 32 hex digits; the place of 5.8's worked example (golden vector 1 with
  `c`); each field of a place in its place; a cancel whose `data` is golden vector 5's with
  that vector's id; a modify; the market left out of cancels and modifies; the longest
  form exactly `MAX_OP_BYTES`; no form for operator commands;
- **recovery** (`verifier.rs`): the signer's address comes out; the other id, or another
  digest, gives another address; ids 2, 3, 27 and 28 are refused; the high-S twin with the
  other id, which `k256` alone recovers to the signer's own key, is refused; `r` or `s` of
  0; `verifies_digest` accepts the signer's key only. In the feature build, over 240 keyed
  cases, both libraries recover the same signer, give the same answer (never the signer)
  for a flipped bit in the digest, `r` or `s` or for the other id, and refuse the same bad
  ids, the high-S twin, and `r` or `s` of 0, `n`, `n + 1` and `2^256 − 1`;
- **the version-2 message** (`wire.rs`): the layout of 5.8, round trips for a place, a
  cancel and a modify, the refusals of `decode_eip712` (another version, byte 7, ids above
  1, a bad CMD40, operator commands), and recovery to the signer and nobody else;
- **the check order** (`check.rs`): acceptance uses up the request, and the same salt with
  another `ts` or account is another request; each scheme refuses the other's messages as
  `WrongDomain`; `Malformed` for byte 7 and for ids 2, 3, 27 and 28; `OperatorOnly`;
  routing, ownership and registration before the timestamp; the window's edges in
  milliseconds (5 minutes back and 60 s ahead are accepted, one more is refused), and the
  clock rounded down (999,999 ns later is the same millisecond); a replay, its broken twin
  and its high-S twin all `ReusedRequest`, also with a full lane; a forgery (`WrongSigner`,
  or `BadSignature` for `r` = 0) takes no slot, so the genuine message is still accepted;
  every signed field edited after signing, and a message signed for deployment 2 with its
  field edited, give `WrongSigner`; a cancel or a modify copied to another market is
  another request: accepted, before or after the genuine message, which is still accepted,
  up to one copy per market id (market 65,535 included), while an exact copy of either is
  `ReusedRequest`; a place copied to another market is `WrongSigner` and takes no slot,
  before or after the genuine place; `Busy`
  before the recovery, with the cancel headroom; the high-S twin is `HighS` before any
  recovery; a full table refuses new requests before `Busy` and the recovery, but still
  answers `ReusedRequest`; the verify-on-core ablation panics for this scheme; the
  audit's rebuilt digest verifies; the warm-up runs;
- **the salt table** (`salts.rs`): the capacity (a power of two, at least twice the
  requests); a request is new until it is inserted, and another account, salt, `ts` or
  market is another request; copies of a request on other markets each take a slot and are
  each found; `Request::of` takes a place's, a cancel's or a modify's market, and gives
  none for an operator command; changing any one of the four fields moves where a walk
  starts; colliding requests are all found, also across the table's end; an expired slot
  is reused by an insert but never ends a lookup; a half-used table refuses a new request
  unless its walk passes an expired slot; the seed moves where requests start, and `Debug`
  doesn't print it;
- **the journal** (`pipeline/`): header byte 120 round-trips, a header from before it
  reads as perp, codes other than 0 and 1 are refused, and the three bytes after it are
  still reserved; `differences` names the scheme; recovery refuses a restart in the other
  scheme; `Pipeline::start` refuses the scheme in pre-verified mode and with the ablation;
- **the audit** (`audit.rs`): a journal of genuine EIP-712 messages across several segments
  passes, and the same records under a perp header fail; the salt, `ts`, command or key
  changed after signing, no signature, or the high-S twin, each fail; a repeated request
  fails (the same salt and `ts` for another account doesn't); a cancel or a modify copied
  to other markets passes next to the genuine record, and an exact repeat of either fails;
  a market changed in the journal on a cancel or a modify is not seen; a `ts` past either
  edge of
  the window fails, and the edges pass;
- **the gateway loop** (`thread.rs`): EIP-712 messages become lane records with the salt
  and `ts` in their nonce and expiry words, and the rejects are counted by reason;
- **with real rings, pipeline and journal** (`gateway/tests/eip712_pipeline.rs`): valid
  messages are recovered, forwarded, journaled, applied and released; a replayed request, a
  `ts` too old and one too far ahead, another account's key, the high-S twin and a perp
  message are each rejected with their reason and never journaled; the journal says its
  scheme, recovery takes it for an EIP-712 configuration and refuses it for a perp one; the
  records hold the salt and `ts`, and the audit passes on them;
- **no allocation** (`gateway/tests/eip712_no_alloc.rs`): a counting allocator sees no
  allocation and no free over 1,200 checks after the gateway thread's warm-up, 100 each of
  acceptance and 11 reject reasons, and none for a full table's refusal either, with
  whichever verifier the build recovers with.

**Loadgen and harness, the EIP-712 scheme:** every pre-signed EIP-712 message decodes to its
item, with its nonce as the salt and the arena's time, and recovers its signer with a low
`s`; an arena is the same on any thread count and bound to its time; `presigned.bin`
version 2 keeps the scheme and the time and refuses what doesn't fit; the configuration's
refusals (pre-verified, the ablation, `--resume`, a timed flow over 3 minutes), name,
fingerprint and identity; a summary from before the scheme reads as perp; the command
line's `--auth`; workloads kept apart per scheme, signed just before their runs, and signed
again when stale; each gateway's table sized for the messages it is offered; sweeps that
sign their signed points only; the reports; and the probes.

**CRC32C:** `"123456789"` → `0xE3069283`; empty → 0; the two journal record examples of
11.3 (`0xE5C5CC1D`, `0xFF8EEBC8`); slicing by 8 equals byte at a time on random inputs of
every length from 0 to 300 bytes.

**Journal format and recovery rules** (on the simulated disk below, 4 KiB segments, and
1 MiB segments where the tail region `W` must be smaller than a segment:
`pipeline/tests/journal_regressions.rs`):
- write records across several segments and read them back;
- **torn tails:** for each byte offset inside the last batch, zero everything from there
  and recover: exactly the records before the damaged one; also a flipped byte in the last
  record, a torn record at a segment boundary, a lost header of a new segment whose body
  reached the disk;
- **errors, not ends** (11.8): a valid record with a wrong `seq`; a kind-3 record holding a
  `PlaceOrder`; a kind-2 record in a signed journal; a kind-1 record whose account doesn't
  own its order id; a `ts` below the previous one; a later segment whose header has a valid
  CRC but another deployment, other engine options, or a `first_seq` that doesn't continue;
  a flipped byte in a synced record in the middle of the journal (nonzero data outside the
  tail region); a flipped byte in synced data in the last `W` bytes of a segment while the
  journal goes on in the next (nonzero data in both parts of the tail region); with 1 MiB
  segments, a nonzero byte `W` past the end, and one at offset `W` of the next segment
  (while `W − 1` in either place is a torn tail). Each must stop recovery with an error that
  names the place, and leave every byte of the journal unchanged;
- a second life torn at the same place keeps the first life's torn copy;
- a new journal over a first flush that lost its header's sector zeroes the stale body
  (and refuses a directory with a valid header);
- after recovery, the new life starts in the next segment with the right `seq`, and every
  byte after the end is zero;
- a restart whose configuration differs from segment 0's identity is refused; one with a
  different `keys.txt` is accepted and writes the new digest into its new segment;
- the flush rule, as a pure function `should_flush(batch_len, oldest_t_seq, now, T, B,
  finished)`, at each boundary, including `T` = 0.

**Ring:** a capacity-1 and a capacity-2 ring through full, empty and wrap-around;
`free(wanted)`/`available(wanted)` at every fill level (a stale cached count is reloaded
when it is below `wanted`); `peek` doesn't consume; `channel` panics on a capacity that is
not a power of two; after the producer is dropped, `is_finished` is false until the last
record is read, then true.

**Histogram:** index and bounds at every power of two and one below and above; every
bucket contiguous up to 2^40; merge equals recording both; percentiles on a small known
set, and within 0.79% of the exact values on 100,000 random ones; an empty histogram
reports "no data"; `u64::MAX` reports "∞".

**Engine semantics** (`pipeline/tests/engine_semantics.rs`): the first 100,000 commands of
the smoke flow, applied to a fresh engine; the CRC32C of the event stream's EVT56 bytes
equals the pinned value that goes with `ENGINE_SEMANTICS` (11.2).

**Loadgen:**
- the M3 flow is identical for the same seed and different for another; its first items
  and its counts after 1M items are pinned (as the M0 test pins M0);
- the mix after 20 s of flow time is within 1.5 points of 14.7's shares;
- per account, nonces are 1, 2, 3, … and order sequences increase; every cancel and modify
  names an order the same account placed earlier; every price is inside its market's range;
  every quote and thin order is inside the band of the generator's own latest mark;
- key derivation matches 5.6; every pre-signed message verifies (a sample of 1,000) and
  has a low `s`;
- the Poisson schedule has mean gap `1/R` within 1% over 1M gaps, and operator items get
  gap 0;
- the sender, driven against a full operator ring, keeps client items on schedule and
  pushes the pending operator items in order once space appears.

**Loadgen, the Polymarket-shaped flow** (14.12; `market_flow/profile/tests.rs`,
`market_flow/polymarket/tests.rs`, `schedule.rs`):
- **the profile:** 88 markets, once each, in id order; `pd + qd = 6`; tier tables of 1 to 8
  rows that start at 0 with the market's maximum leverage, bounds rising and leverage
  falling, 12 distinct; start prices on their real grid (76 at 1 tick, 11 at 10, XRP-USD at
  100) and inside the engine's price limits; a real engine accepts every market and tier
  table; the classes hold the markets D-034 lists; weights and every split sum to
  1,000,000; every table ascends; the values D-034 quotes; draws follow the tables and the
  shares;
- **markets and setup:** every market's parameters, both band rules, the fair value's
  bounds keeping the band inside the range, 85.6 MB of levels; the real grid (5
  significant figures, rounding across a power of ten); setup A, B1 and B2's contents,
  counts and order, and each cohort's accounts;
- **determinism:** the same config gives the same plan, another seed another; a longer plan
  starts with a shorter one; the digest changes with every field and switch and never
  equals the M3 flow's; the digest, the first 3 items, the counts after 1M items, the last
  item and the flow time are pinned (bump `FLOW_VERSION` on purpose);
- **invariants over whole plans**, for the default flow, `makers_k` 1 and 3, both shock
  presets and frequent jumps: nonces, order sequences, ownership, range, grid, band, 20
  uncrossed quotes a side at every mark; and the generator's own ladders between events;
- **the engine:** every setup phase accepted with no reject; the timed flow's rejects under
  0.5%, all `UnknownOrder`; the stress shock liquidates a cascade cohort together (at least
  4 in one mark, all from marks); a large jump liquidates the wrong-side high-leverage
  account;
- **the realised shape** over 10 s of flow: the maker mix (within 0.5 points) and level
  bands (within 1 point); spreads within 12% of the tables' 10th, 50th and 90th
  percentiles; depth per quote against the recorded books (median within 15%, 90th
  percentile within 30%) and dust shares; the takers' share (within 12%) and notional
  sampler; marks every 200 ms and moves once a second; the no-move share and the moves'
  RMS per leverage class;
- **jumps and shocks:** each marked at once and followed by its market's 40 cancels and 40
  new quotes; jump sizes and timing; shocks' mover counts (14 to 88), sizes, direction and
  timing per preset;
- **makers and checks:** `makers_k` 3 gives three accounts about a third each, 1 gives one;
  `check` refuses what the generator can't run (absurd sizes refused, not overflowed);
- **signing:** both schemes sign a Polymarket plan, every message decodes and verifies, and
  a `SenderPlan` with Cox arrivals takes it;
- **bursts** (`schedule.rs`): per simulated hour, the multipliers average exactly 1, with
  p90 and p99 within 12% of the calibration's values and the maximum within 20%, for both
  presets; arrivals follow the multipliers within 1% and keep the offered rate; operator
  items stay right after the client item before them; Poisson arrivals are unchanged.

**Harness, the Polymarket-shaped flow** (`bench/src/e2e/`): a `Flow` is its generator's
config (the M3 flow's digest and accounts unchanged), its plan is the generator's, the
switches are the Polymarket flow's only, and every flow fits the engine's capacities; run
names and fingerprints carry the flow and every switch, a flow the generator can't run is
refused, and a summary from before the flows reads as the M3 flow with no switch; a
workload serves only its own flow, bursts share it, and a restart's rest of a Polymarket
plan is its timed flow; the flow's content counts (by account, lane and market) and market
shares; the durable limit's point is the M3 flow's plain one whatever the template; the
D-034 headline points; search names and `search.txt` keys; each flow's own headline and
breakdown, the M3 flow first, and the run report's flow's shape; the command line's
options and refusals.

### 18.2 Property and crash tests

**Codec:** any 40 (56) random bytes either fail to decode or re-encode to the same bytes;
any random command (event) round-trips. (proptest, as configured in D-009.)

**Gateway nonce state:** random sequences of messages for 3 accounts with random nonces,
valid or forged signatures (pre-signed pool), random expiries and random lane-free counts;
the set of forwarded messages and the final `last_nonce`s equal a 10-line model's.

**The EIP-712 scheme's requests** (5.8): 1,500 random messages for 3 accounts, with few
salts (so requests repeat), two markets, timestamps on both edges of the window, random
lane-free counts and a quarter forged: each answer equals a model's (the window, then the
request `(account, salt, ts, market)`, the lane and the signer), and the table holds
exactly the requests the model accepted. And the salt table alone: 20,000 random requests
on two markets over a clock that moves on for about 400 s, so requests expire; a request
is reused exactly when it was inserted before, the table is refused as full only when half
its slots are used, and it never passes half.

**Crash, recover, restart, crash again, on a simulated disk.** The journal writer and
recovery do their file operations through a small trait (`write_at`, `read_at`,
`sync_data`, `create_segment`, `sync_dir`). The real implementation calls `std::fs`. The
test implementation, `SimDisk`, keeps the durable bytes and the not-yet-synced 512-byte
sectors separately, and can:
- **lose power:** keep a random subset of the unsynced sectors, drop the rest;
- **fail a sync the way Linux does:** return an error, and mark the unsynced sectors
  clean without making them durable, while reads still return them (11.7).

The test runs random sequences: batches written and synced, a crash (power loss, process
crash, or a failed sync followed by a restart on the same "boot"), recovery, a restart
that writes more, another crash, and so on. After every recovery it checks:
1. every record that was released (synced before the crash) is in the recovered journal,
   unchanged;
2. no record from an earlier life appears after the point where that life's journal was
   cut;
3. after a failed-sync restart and one more power loss, the records recovery kept are
   still there (step 3 of 11.8 made them durable).

This is the property the draft could not satisfy (review finding F1), and the one the
Python model of 11.8 checked over 20,000 runs. The random runs use 4 KiB segments, plus a
few with 1 MiB segments and batches of up to 4,096 records, larger than the tail region, as
production's are.

### 18.3 Concurrency tests

- **Ring stress:** a producer thread writes 10 million records whose every word is a
  function of the record's index, with random batch sizes, while the consumer reads with
  random batch sizes and checks every word; run for 1-, 2- and 3-line slots, in debug and
  release, and once with the consumer much slower than the producer, so the full-ring paths
  run constantly. At the end the producer is dropped, and the consumer must read every
  record before `is_finished` says true.
- **Watermark:** a writer thread publishes an increasing watermark while a reader checks it
  never decreases and never passes the writer's last store.
- **What they can't show:** both run on x86, where Relaxed and Release/Acquire compile to
  the same instructions and the hardware never reorders stores with stores or loads with
  loads. So they check the logic, not the memory orderings, which rest on the argument of
  3.2 (checked by review).

### 18.4 End-to-end tests (`bench/tests/`, seconds, locally)

**Smoke test** (`smoke.rs`). A scaled-down flow, `MarketFlowConfig::smoke()`: 6 markets
(two per class), 4 MMs each, 40 takers, 2 HL accounts per market (one long, one short), 10
thin accounts, jump probability 1/500 so jumps happen in a short run, and a fund deposit
of $1 so the first loss past a bankruptcy price makes a shortfall. (The config holds these
counts as fields; 14.3's id formulas use them, e.g. an HL account is long if its index
within its market is below half the market's HL count.) Idle strategy "spin then yield"
(19.1) so it shares CPUs politely with other tests; pinning off.
1. **Signed:** 2 gateways, setup plus 3,000 client messages at 5k/s.
2. **Pre-verified:** 2 lanes, setup plus 30,000 commands at 20k/s.
3. In a build with `--features c-secp256k1`: the signed run again, with libsecp256k1
   verifying in the gateways and the audit (5.7).
4. The signed run in the EIP-712 scheme (5.8), `signed-5k-eip712`: the gateways recover
   each signer with `k256` and keep a salt table, and the audit checks the rebuilt
   digests; in a build with the feature, once more with libsecp256k1 recovering and
   verifying. Each must also show that no gateway refused one of its messages as stale,
   early, reused or for want of room (`RunResult::harness_rejects`).
5. The Polymarket smoke flow (14.12, `RunConfig::polymarket_smoke`): the 88 real markets,
   5,000 messages per second of flow time and a stress shock every 250 ms, so its
   liquidations and shortfalls come from shocks. Signed
   (`signed-5k-polymarket-shock-stress`); pre-verified with three makers quoting every
   market and bursty arrivals
   (`preverified-20k-polymarket-makers3-bursts-median-shock-stress`, whose busiest account
   must be one of the three makers); signed in the EIP-712 scheme; and in a build with the
   feature, signed with libsecp256k1. So the default build runs 6 smoke runs and the
   feature build 9.

Each run checks:
- every stage handled commands, and every histogram is non-empty;
- the counts reconcile: offered = sent + ingress drops; sent = forwarded + gateway rejects
  (signed); forwarded + operator = sequenced = journaled = applied by the core = trailers
  released (the shutdown of 2.8 loses nothing);
- at least one `Liquidation` and one `InsuranceShortfall` (so the replay covers both; if a
  flow change breaks this, retune the smoke config; the Polymarket smoke flow gets them
  from its stress shocks);
- zero allocations on the pinned threads after setup: a counting global allocator in the
  test binary (the pattern of `engine/tests/no_alloc.rs`) counts allocations made on
  threads flagged "hot" (a thread-local), and must count none during the timed flow; and
  zero minor faults on those threads in the window (15.4), the core's included, unless
  the kernel migrated pages in that window: then the faults may be the kernel's, and are
  printed with the count instead of failing (a first touch would show in every window,
  and most windows see no migration);
- the replay test of 13.3 (events and snapshot identical, also with another hash seed);
- **and** a replay through `Engine<ReferenceBook, Naive>` gives the same events: the M2
  reference now checked on the M3 flow's markets, jumps and liquidations, and on the 88
  real markets' tier tables, shocks and cascades in the Polymarket smoke runs;
- the signature audit (13.4) finds no failure, and checked every forwarded message;
- the flow's shape adds up (14.12): each lane's offered messages (`flow.lane.<g>.messages`)
  sum to the window's client messages (`flow.window_clients`), which equal what the sender
  offered in the window, and `run.flow` names the flow;
- `report.md` renders, and names the verifier, the signing scheme and the flow's shape.

**Kill test** (`crash.rs`). The test starts the `e2e` binary as a child process on the
smoke flow with `--release-log`: the gate also writes every released event slot (64
bytes) to its standard output, which the test reads. At a random moment the test kills
the child with SIGKILL (a process crash: the page cache survives). Then it runs recovery
and replay on the run directory and checks that **the released events are a prefix of the
replayed event stream**, slot for slot. It then restarts the pipeline from the recovered
journal (13.5), sends the rest of the flow, kills it again (at most halfway through the
events the journal still lacks, so the kill falls inside the flow), and checks again; a
final clean stop runs the replay test, and `e2e replay` must agree. Last, one more restart
after that clean stop finds nothing left to send: that life issues no command, and its
checks and `e2e replay` must still pass (its empty `events.bin` names the seq it started
at, 13.2). This covers the restart path end to end: recovery, replay, the nonce table,
`next_seq`, the clock anchor and the new segment.

### 18.5 The replay test at scale

Part of the headline session on PERPSBOX (13.3): the fourth, capture-on 100k/s signed run,
replayed and compared, plus the signature audit.

---

## 19. Code layout and APIs

### 19.1 Crates and modules

```
engine/                 unchanged
pipeline/src/
  lib.rs                module docs: the whole picture of section 2
  ring.rs               Line, CachePadded, channel, Producer, Consumer, closing (3.1, 3.2)
  codec.rs              CMD40 and EVT56 (section 4)
  records.rs            ClientRecord, OperatorRecord, CoreRecord, JournalRecord, trailer (3.3, 10.3)
  clock.rs              RunClock (15.1)
  idle.rs               IdleStrategy: Spin, or SpinThenYield { spins } for tests
  histogram.rs          LatencyHistogram (15.3)
  crc32c.rs             slicing by 8, and the byte-at-a-time reference for tests (11.4)
  counters.rs           the single-writer counters main reads (3.2), thread health (15.4)
  panic.rs              the abort-on-panic hook and the thread-local current seq (2.8)
  sequencer.rs          (section 9)
  core_thread.rs        the core loop and EventRingSink (section 10)
  gate.rs               release, stats, fund tracking, capture (12, 13.2, 15.9)
  journal/format.rs     segment header, identity, ENGINE_SEMANTICS, record encode/decode (11.2, 11.3)
  journal/files.rs      the file-operations trait, the std::fs implementation, create_segment (11.1, 18.2)
  journal/writer.rs     group commit, preallocation, discard mode (11.5 to 11.7)
  journal/recovery.rs   (11.8)
  replay.rs             (13.1, 13.3)
  affinity.rs           topology, layout, pin_current_thread: the one unsafe function (2.6)
  run.rs                Pipeline: start (fresh or resumed), released, counters, join (2.8, 13.5)
gateway/src/
  lib.rs                module docs
  wire.rs               the 136-byte message: layout constants, encode_signed_part, decode (5.1);
                        version 2: encode_eip712, sign_eip712, decode_eip712, check_signer (5.8)
  verifier.rs           VerifierKind, PublicKey: k256, or libsecp256k1 with the c-secp256k1 feature (5.7);
                        recovery and addresses, sign_recoverable (5.8)
  registry.rs           keys.txt load (for one verifier) and write, partition by gateway (5.4)
  check.rs              Gateway, GatewayReject, the check order of each scheme (7.1, 5.8)
  salts.rs              SaltTable: the EIP-712 scheme's table of recent requests (5.8)
  eip712.rs             the compact forms, Domain, the digest, address_of (5.8)
  keccak.rs             keccak-256, our own (5.8)
  msgpack.rs            a MessagePack writer for the forms eip712.rs signs (5.8)
  thread.rs             the gateway loop (7.3)
  audit.rs              signature audit of a journal, in its scheme (13.4)
loadgen/src/
  lib.rs                M0 and M1 flows, unchanged (its header's "no floats" sentence is
                        updated to allow floats for send times only, 14.9)
  market_flow.rs        the M3 flow: config, generator, setup phases (14.1 to 14.7); FlowPlan<C>, PlanConfig
  market_flow/profile.rs            the calibrated profile's types, Table, pick (14.12)
  market_flow/polymarket_profile.rs POLYMARKET, generated by tools/calibrate: do not edit (14.12)
  market_flow/polymarket.rs         the Polymarket-shaped flow: config, switches, generator, setup (14.12)
  market_flow/polymarket/market.rs  one market: fair value, ladders, the real grid, IOCs (14.12)
  keys.rs               key derivation (14.8)
  presign.rs            parallel signing (either scheme, either flow), the signed arena, the compact pre-verified arena, presigned.bin (14.8)
  schedule.rs           Poisson, uniform and bursty (Cox) send times (14.9, 14.12)
  sender.rs             the open-loop sender (14.10)
bench/
  src/e2e/              harness: config, flow (which flow, its switches), runner, sweep, search, probes, report
  src/bin/e2e.rs        CLI: probe | run | sweep | search | ablate | replay | recover
  benches/pipeline_parts.rs   k256 verify/sign/recover/sign_recoverable, libsecp256k1 verify/recover (feature), SHA-256,
                              EIP-712 encode_op and digest, keccak-256, the salt table, codec, CRC32C (both ways),
                              ring hop, histogram record
  tests/smoke.rs        (18.4)
  tests/crash.rs        the kill test (18.4)
pipeline/tests/         ring_stress.rs, crash_recovery.rs, engine_semantics.rs (18.1 to 18.3)
tools/calibrate/        the profile's calibration, Python standard library, run on the host (14.12; its README)
```

**Dependencies** (all in place since commit "M3 spec", with `Cargo.lock` updated and no new
crate: `libc` 0.2.189 was already locked). `k256 = "=0.14.0"` (features: `ecdsa` only) is
in `gateway`, `loadgen` and `bench` (`docs/SUPPLY-CHAIN.md`, Milestone 3). Also:
- `pipeline`: `engine`, and `libc = "=0.2.189"` for `sched_setaffinity`;
- `gateway`: `pipeline` (it already has `engine` and `k256`);
- `loadgen`: `pipeline` and `gateway` (it already has `engine` and `k256`);
- `bench`: all of the above;
- the workspace: `[profile.dev.package."*"] opt-level = 3` (C33);
- added 2026-09-30, **optional**: `secp256k1 = "=0.33.1"` (features: `std` only; it
  brings `secp256k1-sys` 0.14.1 and its C) in `gateway`, only with its feature
  `c-secp256k1`, off by default; `bench` has a feature of the same name that turns the
  gateway's on (5.7; `docs/SUPPLY-CHAIN.md`). Since the EIP-712 scheme (5.8), the
  gateway's feature also turns on the crate's `recovery` feature: one more module of the
  same C, no new crate, and `Cargo.lock` unchanged.
No other new crate: no queue, histogram, pinning, CRC, hash, random-number or allocator
crate. SHA-256 comes from `k256::sha2` (5.2), for libsecp256k1's digest too; keccak-256 and
MessagePack are our own code (5.8).

**The one `unsafe` function outside test binaries** (`pipeline/src/affinity.rs`). The test
binaries have their own: `engine/tests/no_alloc.rs` already implements a counting
`GlobalAlloc` under a file-level `#![allow(unsafe_code)]`, and the smoke test (18.4) reuses
that pattern.

```rust
/// Pins the calling thread to one logical CPU.
///
/// The only unsafe code outside test binaries. SAFETY: `set` is a plain bitmask we zero and
/// fill ourselves (`CPU_ZERO`/`CPU_SET` from libc); pid 0 means "this thread"; the size
/// passed is the size of the set. The call reads the set and changes only the calling
/// thread's scheduling.
#[allow(unsafe_code)]
pub fn pin_current_thread(cpu: usize) -> std::io::Result<()>
```

Everything else about CPUs and the machine (topology, allowed CPUs, quota, throttling,
clock source, mounts, page faults, thread ids, frequency) is read from files with
`std::fs`.

### 19.2 APIs between the crates

```rust
// pipeline
pub struct PipelineConfig {
    pub deployment: u32,
    pub auth: AuthScheme,                       // Perp or Eip712: part of the journal's identity (5.8, 11.2)
    pub engine: EngineOptions,
    pub lanes: usize,                           // N
    pub capacities: RingCapacities,             // 2.3 defaults
    pub journal: JournalConfig,                 // dir, SEG_BYTES, T, B (<= 4,096), mode: Disk | Discard
    pub registry_digest: [u8; 32],              // zero in pre-verified mode
    pub layout: CpuLayout,                      // 2.6
    pub idle: IdleStrategy,
    pub capture: Option<usize>,                 // reserved event slots
    pub release_log: Option<File>,              // kill test only (18.4)
    pub stamps: Stamps,                         // On, or Off for the one-off cost measurement (15.1)
    pub ablation_verify_on_core: Option<CoreVerifier>,  // insecure; only `e2e ablate` sets it (16)
}
pub struct Inputs { pub lanes: Vec<Consumer<3>>, pub operator: Consumer<1> }

/// Everything a restart needs, from recovery and replay (13.5).
pub struct Resume {
    pub engine: Engine<Book, Fast>,
    pub next_seq: u64,
    pub last_ts: u64,
    pub end: JournalPosition,                   // the writer starts in the next segment
}
impl Pipeline {
    /// Installs the abort-on-panic hook (2.8). `resume` is None for a new journal.
    pub fn start(config: PipelineConfig, inputs: Inputs, clock: RunClock, resume: Option<Resume>) -> Pipeline;
    pub fn released(&self) -> u64;              // commands released so far (barriers)
    pub fn counters(&self) -> CounterSnapshot;  // thread health, sampled at the window's edges (15.4)
    /// Waits for the shutdown cascade of 2.8 (it starts when the sender drops its ends).
    pub fn join(self) -> PipelineOutput;
}
pub struct PipelineOutput {
    pub engine: Engine<Book, Fast>,
    pub stats: GateStats, pub journal: JournalStats, pub core: CoreStats,
    pub capture: Option<Vec<u64>>,
}

/// 11.8: checks the journal against `expect` (the configured identity), zeroes the tail,
/// re-writes and syncs what it keeps. Changes nothing if it returns an error.
pub fn recover(dir: &Path, expect: &JournalIdentity, allow_engine_change: bool) -> Result<Recovered, JournalError>;
pub struct Recovered { pub identity: JournalIdentity, pub end: JournalPosition, pub records: u64,
                       pub next_seq: u64, pub last_ts: u64, pub registry_digests: Vec<[u8; 32]> }
/// 13.1: replays a recovered journal.
pub fn replay(dir: &Path, recovered: &Recovered, seed_override: Option<u64>, capture: bool) -> Result<ReplayOutput, JournalError>;
pub struct ReplayOutput { pub engine: Engine<Book, Fast>, pub nonces: NonceTable,
                          pub next_seq: u64, pub last_ts: u64, pub capture: Option<Vec<u64>> }

// gateway
pub fn encode_signed_part(deployment: u32, account: AccountId, nonce: u64, expires_at: u64, command: &Command) -> [u8; 72];
pub fn decode(msg: &[u8; 136]) -> Result<Decoded, GatewayReject>;
impl Gateway {
    /// `nonces`: empty for a new journal, the replayed table after a restart (6.3).
    pub fn new(index: usize, count: usize, deployment: u32, registry: &KeyRegistry,
               nonces: &NonceTable, start_unix_ns: u64) -> Gateway;
    /// Added with the EIP-712 scheme (5.8): `salts` is a new table, made on main.
    pub fn new_eip712(index: usize, count: usize, deployment: u32, registry: &KeyRegistry,
                      salts: SaltTable, start_unix_ns: u64) -> Gateway;
    pub fn check(&mut self, msg: &[u8; 136], now_unix_ns: u64, lane_free: usize) -> Result<Accepted, GatewayReject>;
}
pub fn sign_eip712(key: &SigningKey, domain: &Domain, account: AccountId, salt: u64, ts_ms: u64, command: &Command) -> [u8; 136];
pub fn check_signer(verifier: VerifierKind, domain: &Domain, decoded: &DecodedEip712,
                    signature: &[u8; 64], address: &Address) -> Result<(), GatewayReject>;   // checks 11 to 13
impl SaltTable { pub fn new(requests: usize, seed: u64) -> SaltTable; }                        // allocated and pre-touched
/// The thread stops when `ingress` is closed and drained, then closes `lane` (2.8).
pub fn spawn(gateway: Gateway, ingress: Consumer<3>, lane: Producer<3>, clock: RunClock,
             idle: IdleStrategy, cpu: Option<usize>) -> JoinHandle<GatewayStats>;
pub mod audit { pub fn verify_journal(dir: &Path, registries: &[KeyRegistry]) -> AuditReport; }
/// Added with the optional verifier (5.7): every key parsed once, for `verifier`.
impl KeyRegistry { pub fn load(path: &Path, deployment: u32, verifier: VerifierKind) -> Result<KeyRegistry, RegistryError>; }

// loadgen
pub fn generate(config: &MarketFlowConfig, timed_client_items: usize) -> FlowPlan;  // setup A, B1, B2, timed
/// Added with D-034 (14.12): the Polymarket-shaped flow, into the same kind of plan.
pub fn polymarket::generate(config: &PolymarketConfig, timed_client_items: usize) -> FlowPlan<PolymarketConfig>;
pub struct FlowPlan<C = MarketFlowConfig> { pub config: C, /* setup_a, setup_b1, setup_b2, timed, jumps, flow_ns */ }
pub trait PlanConfig { fn seed(&self) -> u64; fn digest(&self) -> [u8; 32];            // either flow's config
                       fn client_accounts(&self) -> Vec<AccountId>; fn engine_options(&self) -> EngineOptions; }
pub fn signing_key(seed: u64, account: AccountId) -> SigningKey;
pub fn write_registry(path: &Path, seed: u64, deployment: u32, accounts: &[AccountId]) -> io::Result<[u8; 32]>;
pub fn presign<C: PlanConfig>(plan: &FlowPlan<C>, deployment: u32, threads: usize) -> Arena;                 // 144 B per item
pub fn presign_eip712<C: PlanConfig>(plan: &FlowPlan<C>, deployment: u32, ts_ms: u64, threads: usize) -> Arena; // 5.8: salt = nonce
pub fn preverified<C: PlanConfig>(plan: &FlowPlan<C>) -> CompactArena;                                         // 64 B per item
pub fn poisson_schedule(items: &[Item], rate: u64, seed: u64) -> Vec<u64>;
impl Schedule { pub fn new(arrivals: Arrivals /* Poisson | Uniform | Cox(Bursts) */, seed: u64) -> Schedule; }
/// Stops at the end of its plan, or early when `stop` is set (a barrier timeout); either
/// way it drops its ring ends, which starts the shutdown cascade (2.8).
pub fn spawn_sender(plan: SenderPlan, outputs: SenderOutputs, clock: RunClock,
                    barrier: Barrier, stop: Arc<AtomicBool>, cpu: Option<usize>) -> JoinHandle<SenderStats>;
```

The harness (`bench/src/e2e`) owns the wiring: it creates the rings (after pinning main,
2.6), gives each end to its thread, starts everything, runs the phases, waits for the
shutdown, replays if asked, and renders the report. Since D-034 it holds one plan type,
`FlowPlan<Flow>`, whichever flow a run sends (`bench::e2e::flow::Flow`:
`M3(MarketFlowConfig)` or `Polymarket(PolymarketConfig)`, itself a `PlanConfig`), so the
signer, the sender, the restart code and both verifiers are unchanged. For a restart it runs `recover`, then
`replay`, then starts the gateways with the nonce table and the pipeline with `Resume`
(13.5).

---

## 20. Choices made in this spec

Each is a place where INFO.md or the owner's decisions left room. The product-level ones
are repeated in section 21. C1 to C39 are the draft's (some revised after review); C40 to
C58 were added by the review (section 22); C59 to C61 came with the EIP-712 scheme (5.8);
C62 to C67 with the Polymarket-shaped flow (14.12).

- **C1. Nonce checked and used up at the gateway** (6.2), not the sequencer: replays cost
  nanoseconds there, and the state is sharded with the accounts.
- **C2. A nonce is used up exactly when its message is forwarded to the sequencer**,
  after the signature verifies (6.1); it stays used up once the record is durable, and is
  rebuilt from the durable journal on restart (6.3). Gateway rejects don't use it; engine
  rejects do.
- **C3. Strictly increasing with gaps allowed** (INFO.md's wording), not "exactly the next
  one", but no jump above 2^32 (C41). Cost: a message rejected at the gateway can't be
  resent once a later nonce has been used; the client signs it again with a new nonce.
- **C4. An explicit account field** in the message, checked against the order id
  (`NotOwner`), though the id already names the owner (5.5).
- **C5. Domain fields in the message and signed** (magic, version, deployment; type =
  command tag), compared by the gateway before verifying (5.1).
- **C6. SHA-256** (k256's default), not keccak/EIP-712 (5.2), in the default scheme; the
  opt-in scheme of 5.8 uses keccak-256 and EIP-712 (C59).
- **C7. Low-S enforced by our own byte comparison**, with its own reject reason, before
  the verifier (`k256`, or libsecp256k1, 5.7) is called (5.3). The signature bytes are not a message id;
  `(deployment, account, nonce)` is.
- **C8. Compressed public keys in a text registry**, loaded at start, parsed once,
  partitioned by gateway; its SHA-256 in each segment's header; keys change only at a
  restart (5.4).
- **C9. The journal stores each signed command's nonce, expiry and signature** (11.3; Q3).
- **C10. One command encoding and one event encoding** (CMD40, EVT56), canonical, used in
  messages, rings, the journal and captures; in `pipeline`, so the engine is untouched
  (section 4). Tags start at 1; enum codes are the engine's declaration order, pinned by a
  test.
- **C11. Our own ring**: slots of 64-byte-aligned lines of `AtomicU64`, `u64` counters,
  Release/Acquire, cached counters reloaded when below what the caller wants, 128-byte
  padding, a `closed` flag, pre-touched at creation (section 3).
- **C12. Back-pressure: reject at the edges, wait inside** (2.4): ingress drops, gateway
  `Busy` checked before verifying; the sequencer and the core wait. Operator items wait in
  the sender without holding up client items (C53).
- **C13. Ring capacities** as in 2.3, from INFO.md 5's formula with an assumed p99.9
  fsync of 10 ms and a 2M/s peak.
- **C14. Large commands stream through the event ring**; the core publishes before
  waiting; nothing is dropped (2.5, 10.2).
- **C15. A trailer slot per command** carries its stamps and outcome to the gate (10.3).
- **C16. Operator commands have strict priority** in the sequencer; lanes are
  round-robin, at most 32 per visit (9.1).
- **C17. Time:** `Instant` for everything, stamped per record; the journal's `ts` is a
  wall-clock anchor plus monotonic time, re-anchored above the last `ts` after a restart;
  the engine gets no time in v1 (9.2).
- **C18. Journal files:** 1 GiB segments, preallocated by writing zeros, created by one
  helper that fsyncs the directory; each process start begins a new segment; 128-byte
  header with the journal's identity; 80- or 152-byte records with CRC32C over every byte
  but the CRC (11.1 to 11.4).
- **C19. Group commit:** flush at `B` = 4,096 records (never more) or when the oldest record
  was sequenced `T` = 1 ms ago; `fdatasync`; buffered `pwrite`; one writer thread; an I/O
  error aborts the process (11.5 to 11.7). `T` is confirmed against `T` = 0 by the sweep.
- **C20. Recovery keeps the longest valid prefix, strictly** (C45).
- **C21. Output gating by command seq against one watermark**, one gate thread, every event
  gated (section 12).
- **C22. Journal "discard" mode** only for the core-path search, labelled, and refused for
  durable stages and the replay test (11.6, 15.7).
- **C23. Pinning layout** (2.6): whole physical cores for the core and the sequencer, on the
  quietest cores; one physical core per gateway by default; the gate and the writer share
  one in signed mode and get one each in pre-verified mode; at most `floor(quota) − 2`
  spinning threads; one CPU package.
- **C24. No allocator crate** (INFO.md 5 suggested mimalloc or jemalloc): nothing allocates
  on the hot path, which the smoke test checks, so the allocator can't matter there.
- **C25. The M3 synthetic flow** (section 14): its markets, cohorts and parameters are
  assumptions, recorded in D-027. Phase B2 builds positions at a frozen mark, a stated
  deviation from INFO.md 7 (14.4).
- **C26. Flow content is independent of the rate**; send times are Poisson at the offered
  client rate in aggregate (bursty per gateway, kept as realistic); operator items ride
  with the client item before them (14.1, 14.9).
- **C27. One sender thread by default**, open loop; it never waits (C53).
- **C28. Pre-sign once, reuse as a prefix** across rates and repetitions (14.8); a compact
  64-byte item for pre-verified runs.
- **C29. The histogram:** log-linear, 7 sub-bucket bits, 7,424 buckets, upper-bound
  reporting, at most 0.79% high (15.3).
- **C30. Latency from the scheduled send time**; stamps carried in records; histograms
  owned by the gate (15.2).
- **C31. Runs:** 5 s warm-up, 30 s window by scheduled time (60 s headline, 10 s at the
  signed overload points), 3 repetitions, interleaved, median and range reported (15.5).
- **C32. Max-rate search:** doubling and 3 bisections at 1 run per probe, then 3 runs at the
  boundary; a rate passes if all 3 runs are under the limit (over offered) with at most
  0.1% not served; invalid runs (throttled, clock inversion, generator-limited) are rerun,
  not failed (15.7).
- **C33. Dependencies compiled optimised in debug builds**
  (`[profile.dev.package."*"] opt-level = 3` in the workspace `Cargo.toml`, added at
  implementation time): tests that sign or verify then run in seconds, while our own crates
  keep debug assertions and no optimisation. The profile changes no dependency.
- **C34. The replay check runs in the same process** as the live run, with the capture in
  memory; also replayed with another hash seed, and (in the smoke test) through the naive
  reference engine (13.3, 18.4). "Not checked" when the capture is incomplete or the journal
  was discarded.
- **C35. Setup in phases with barriers**, journaled and not measured (14.4).
- **C36. `WrongGateway`, `OperatorOnly`, `NonceJump` and `Expired`** are explicit reject
  reasons (7.1).
- **C37. Withdrawals in the flow** (one a second), to exercise the operator path (14.5).
- **C38. The offered rate counts client commands only**; operator commands are counted
  separately (14.9, INFO.md 3).
- **C39. Gateway rejects are counted, not journaled**; in the end-to-end percentiles they
  count as not served (C50) (7.1, 15.4).
- **C40. Every signed message carries `expires_at`** (5.1): the message grows to 136 bytes,
  the signed part to 72 (still two SHA-256 blocks); checked at the gateway against its
  clock; journaled in kind-1 records; the benchmark signs with `u64::MAX` (Q4).
- **C41. `NonceJump`:** a nonce more than 2^32 above the last is rejected, so a client bug
  can't lock its own account out (6.1).
- **C42. Cancels get headroom:** places and modifies are refused while 64 or fewer lane
  slots are free, cancels only when none is (7.1).
- **C43. Shutdown by closing rings in data-flow order**, a `closed` flag per ring checked
  with Acquire before a fresh look at `tail`; no global stop flag (2.8, 3.2).
- **C44. Any panic aborts the process**, through a hook installed by `Pipeline::start`
  that prints the thread and the `seq` (2.8). A poison pill is fixed in code, never in the
  journal.
- **C45. Recovery rules** (11.8): the CRC decides what is a record; a record with a valid
  CRC that doesn't fit is an error, as is an identity mismatch; only nonzero bytes within
  one flush of the end count as a torn tail, anything else is an error and changes nothing;
  the tail is copied aside, then zeroed; the kept part of the last segment is re-written
  and synced before anything is derived from it; the next life starts a new segment.
- **C46. The journal's identity** (deployment, mode, engine options, hash seed,
  `engine_semantics`) is fixed at segment 0 and checked on every restart; commit and build
  profile are recorded, not enforced (11.2).
- **C47. "Durable" means as reported by `fdatasync` on the file system in use**; the probe
  refuses tmpfs, ramfs and a `volatile` overlay, and flags a suspiciously fast flush; every
  durable number says so (11.6, 15.10).
- **C48. Sector writes are assumed atomic**, rather than padding every batch to 4 KiB
  (11.8).
- **C49. CRC32C by slicing by 8** from the start (11.4).
- **C50. Commands never served count as infinitely late** in the end-to-end percentiles,
  so percentiles are over offered commands (15.4).
- **C51. The headline's pass rule** and the durable limit's inputs are defined (15.7).
- **C52. The insurance fund starts with $1,000** in the M3 flow, so the shortfall path runs
  and is replayed (14.3).
- **C53. The sender never waits**: operator items that don't fit stay pending, in order
  (14.10).
- **C54. The sequencer publishes each record at once** (9.1).
- **C55. Every run checks its own conditions**: CFS throttling, clock source and cost,
  clock inversions, minor faults on hot threads, each thread's busy share, the core CPU's
  frequency; and it says which thread was the limit (15.4).
- **C56. A deployment id names one journal history**: a fresh, emptied or restored journal
  needs a new id (5.1, 6.3).
- **C57. The "+ pre-trade risk check" row uses the core path** (sequencer in → core
  result), as INFO.md 5 and the M3 row say, although INFO.md 8's table labels it
  "pre-verified command → core result"; both are reported (15.7).
- **C58. v1's security boundary is written down** (7.4, D-031): what a network gateway
  must add before untrusted clients connect.
- **C59. An opt-in second signing scheme, Polymarket Perps'** (5.8, D-033): the compact
  form of the command, MessagePack, keccak-256 and EIP-712, the signer recovered and its
  address compared with the account's; a `ts` window and a table of requests instead of
  the nonce. Chosen per run; the journal's header records it.
- **C60. The EIP-712 gateway rebuilds what is signed from the CMD40** (5.8), in one
  canonical spelling, rather than hashing the client's own strings.
- **C61. The EIP-712 replay key is `(account, salt, ts, market)`**, kept in a
  preallocated table per gateway, filled only after the signer is checked, and sized for
  the whole run (5.8). The market was added by the owner on 2026-09-30 (it was
  `(account, salt, ts)`), so that a copy of a cancel or a modify on another market, whose
  market is not signed, can't use up the genuine request.
- **C62. A second flow, calibrated from Polymarket Perps' recorded public data** (14.12,
  D-034), beside the M3 flow, which stays the default; a session reports both, each with
  its own headline.
- **C63. The calibrated profile is a committed Rust table of integers**, generated by a
  standard-library Python tool from the recordings (which stay out of git), and checkable
  byte for byte (`generate --check`); no float decides what a plan contains (14.12).
- **C64. A fixed 100,000 client messages per second of flow time**, whatever the offered
  rate, in the Polymarket-shaped flow as in the M3 flow: plans don't depend on the rate and
  runs share signed arenas; flow time is real time only at 100k/s (14.12; D-034 had said
  "as many as the offered rate").
- **C65. Bursts live in the send schedule**, a Cox process normalised over each phase,
  never in the plan (14.9, 14.12).
- **C66. The real markets get our own band and range**: `400,000 / Lmax` ppm and half to
  twice the start price, since Polymarket's bands break band rule 1; the M3 flow's fees
  (14.12).
- **C67. One durable limit per session**, from the M3 flow's pre-verified 20k/s runs,
  for every flow and switch (15.7).

## 21. Questions for the owner

**Answered by the owner on 2026-09-30: Q1 `2 × T` plus the disk's p99 flush time; Q2
probe first, and rent local NVMe for the durable rows if the flush p99 is over 5 ms; Q3
yes, journal each signed command's signature and expiry; Q4 yes, every signed message
carries an expiry.** The spec already follows these answers; the questions are kept below
for the record.

**Q1. What latency limit defines the "durable" rows of the layer table?**
INFO.md 8 leaves it to M3 ("e.g. 2× the commit interval").
- *Recommended:* `2 × T` plus the disk's own p99 `fdatasync` at low load: the median, over
  the 3 runs of the pre-verified 20k/s point that opens every session, printed next to each
  result (15.7). With `T` = 1 ms and a p99 fsync of 0.5 ms, the limit is 2.5 ms. It measures
  the queueing the pipeline adds, not the disk's quality, and stays meaningful on any disk.
  One consequence to know in advance: in the signed row, market makers' bursts queue at
  their gateway (a model puts that p99 at 0.5 to 1 ms at 100k/s, section 17), on top of the
  durability wait. So the signed row may fail this limit where the pre-verified row passes;
  the report would then say the gateways were the limit. The limit should not be loosened
  for the signed row: gateway queueing is what that row measures.
- *Alternative 1:* a fixed 2 ms. Simple, but on a slow disk (PERPSBOX's overlay file
  system, Q2) it may be unreachable at any rate, and the row would say nothing about the
  pipeline.
- *Alternative 2:* a fixed 5 ms. Easier to meet everywhere, but loose on a fast disk.

**Q2. PERPSBOX's disk is an overlay file system inside a vast.ai container. Should the
durable rows be measured on it, whatever its fsync looks like?**
- *Recommended:* run the probe (15.10) first. It refuses tmpfs, ramfs and a `volatile`
  overlay outright. If the `fdatasync` p99 is under 5 ms, use the box as is; if not, rent a
  box with a local NVMe volume for the durable rows (the in-memory rows don't depend on the
  disk). If the p50 is suspiciously fast (under about 20 µs), the report says whether that
  is believed to be a power-loss-protected device or a flush that does nothing. Either way
  the report names the disk and the file system, and labels every durable number "not
  power-loss tested".
- *Why it matters:* every durable number, including the headline "signed order → durable
  ack", includes the fsync time, and a network-backed or overlay disk can have fsyncs of
  several milliseconds with long tails (the headline would then mostly measure the disk),
  or can acknowledge a flush without writing (the headline would then overstate
  durability).

**Q3. Should the journal keep each signed command's signature (and expiry)?**
- *Recommended: yes* (11.3). Anyone with the key registry can then check again that every
  order was signed by the key registered for its account, and that no signed message was
  used twice (13.4): an exchange that posts state roots can also show the signed inputs
  behind them. One honest limit: the proof is only as good as the registry, since whoever
  can write both the journal and `keys.txt` can forge both; the auditor must get the public
  keys from the clients, not from the exchange. The cost is 72 bytes more per signed
  record: 15.2 MB/s instead of 8 MB/s at 100k signed orders/s, well within any disk, and
  more words in two rings.
- *Alternative:* journal only the decoded command and the nonce. The journal still
  replays exactly and still rebuilds the nonces, but it proves only what the exchange
  says happened, not that clients asked for it.

**Q4. Should every signed message carry an expiry time?** (New after the security review.)
- *Recommended: yes, as now written into the spec* (5.1, C40). A message the gateway never
  forwarded (dropped, `Busy`) otherwise stays valid until its account uses a higher nonce,
  so anyone who kept its bytes can get it executed later, at a moment of their choosing
  (attack 9 in 6.5). The cost: 8 bytes (the message becomes 136 bytes, still two SHA-256
  blocks, so hashing costs nothing extra), one compare at the gateway, and 8 bytes in each
  signed journal record. The message format is being fixed now, before any client
  exists; adding a field later changes every client.
- *Alternative 1:* no expiry. Clients kill outstanding messages themselves by using a
  higher nonce (5.1's second client rule). Simpler, but it relies on every client doing it.
- *Alternative 2:* no new field, but nonces become the client's clock in milliseconds and
  the gateway accepts only nonces within a window around its own clock (Hyperliquid's
  approach). That also bounds a message's life, but pre-signed benchmark messages would
  then carry timestamps tied to the run's clock, and `NonceJump` (6.1) would have to go.

---

## 22. Review log

Three reviews of the first draft: durability and determinism (F1 to F15), security (S1 to
S12) and measurement (M3-*). Every finding was checked before the spec changed: F1 in a
model of recovery (the draft rule accepted stale records; the revised rule never did in
20,000 runs), the new examples in two ECDSA models and the CRC model, the gateway bursts in
two queueing models, M/D/1 by simulation, and the rest against the cited text, the engine's
source and the locked `k256` sources.

| ID | Verdict | What changed |
|---|---|---|
| F1 | Accepted | Reproduced in a model. Fixed in 11.8: everything after the end must be zero outside one flush's tail region; that region is zeroed; each life starts a new segment (11.1). Crash-restart-crash test (18.2). The epoch-in-CRC option was not taken: an epoch derived from the surviving headers can repeat when its header was lost, so it would need a random or separately stored epoch. |
| F2 | Accepted | Recovery re-writes the kept part of the last segment and syncs it before anything is derived (11.8, step 3); the operating rule (11.7); the simulated disk models Linux's failed sync (18.2). |
| F3 | Accepted | The identity fields are listed and checked against the configuration on restart (11.2); a valid-CRC mismatch is an error; nonzero data outside the tail region stops recovery with nothing changed; the torn tail is copied aside (11.8). |
| F4 | Accepted | An abort-on-panic hook installed by `Pipeline::start` (2.8), rather than `panic = "abort"`, because the hook also covers tests and debug builds; the poison-pill procedure; 11.7 and D-025 corrected. |
| F5 | Accepted | A `closed` flag per ring, set when the producer is dropped, checked with Acquire before a fresh look at `tail`; shutdown by cascade in data-flow order (2.8, 3.1, 3.2); no global stop flag. |
| F6 | Accepted | `StaleNonce` redefined, with the example (6.1, 12.4, D-022); the rule for a network gateway's replies (7.4). |
| F7 | Partly accepted | `engine_semantics` in the header, pinned by a golden-stream test, refused on mismatch unless overridden (11.2); commit and build profile recorded and printed, not enforced (a poison-pill fix changes the commit). Not done: `overflow-checks = true` in release. D-020's limits rule overflow out, and it would change the build the M2 numbers were measured on. |
| F8 | Accepted | Mount probe, refusals of tmpfs/ramfs/`volatile` overlay, the fast-flush flag, the "not power-loss tested" label, the copy-on-write note (11.6, 15.10, 15.11). |
| F9 | Accepted | File operations behind a trait with a simulated disk and crash sequences (18.2); a SIGKILL test with a release log (18.4); restart APIs `recover`, `replay`, `Resume`, `Gateway::new(.., nonces)` (19.2, 13.5). |
| F10 | Accepted | One `create_segment` helper, with a directory fsync, used everywhere; segments never opened with `O_APPEND` (11.1). |
| F11 | Partly accepted | The sector-atomicity assumption is stated (11.8, C48). Padding every batch to 4 KiB was not taken: about 14% more bytes at 100k signed/s, more at low rates, for a guarantee the devices already give. |
| F12 | Accepted | `closed` flags; the single-writer counters, Relaxed, each on its own line, and why Relaxed is enough (3.2); 9.3 reworded; `channel` asserts a power of two; cached counters reloaded when below `wanted` (3.1). |
| F13 | Accepted | The clock anchor after a restart (9.2); tag-kind and `ts` checks in recovery, as errors (11.8); seq-continuity asserts in the core and the gate (10.1, 12.2). |
| F14 | Accepted | "Not checked" when the capture is incomplete or the journal discarded (13.3); discard mode refuses durable stages and the replay test, and labels its output (11.6). |
| F15 | Accepted | A fresh journal needs a new deployment id (5.1, 6.3); external effects only after release (section 8). |
| S1 | Accepted | The cost claims corrected (2.4, 6.2, 7.1); attack 10 (6.5); network-gateway preconditions (7.4, D-031). The optional measured forgery mix was not added: v1 has no untrusted client, and a forgery costs exactly one verification, which the probe measures. |
| S2 | Accepted | `expires_at` in the signed part (5.1, C40, Q4); the message is 136 bytes; 5.6 and 11.3 recomputed; the two client rules (5.1); attack 9 (6.5). |
| S3 | Partly accepted | The gap is recorded (7.4, D-031), and every run checks hot-thread page faults, which a book outgrowing its capacity would show (15.4). Not added: a gateway minimum order size; it raises an attacker's cost but bounds nothing, and v1's only client is the generator. The fix is a per-slot open-order cap in the engine (Later). |
| S4 | Accepted | Recovery checks tags against kinds and kinds against the journal's mode (11.3, 11.8); the audit checks ownership and per-account nonce order and counts kinds 2 and 3 (13.4); the trust assumption is stated (11.3, 13.4, Q3). |
| S5 | Accepted | 5.3 reworded: the message id is `(deployment, account, nonce)`, never the signature bytes. |
| S6 | Partly accepted | The same fix as F6; start ids in a network gateway's replies (7.4). Deferred: carrying the nonce to the gate. v1 has no client feed and the trailer has no free word; a feed would add it. |
| S7 | Accepted | Lane headroom for cancels (7.1, C42); `Busy` counted per command tag. |
| S8 | Accepted | A deployment id names one journal history (5.1). |
| S9 | Partly accepted | `NonceJump` (6.1, 7.1). Not added: an order-sequence jump check; it needs per-account state rebuilt from the journal for a fault only the key holder can cause, and is listed for the network gateway (D-031). |
| S10 | Accepted | Account-specific reasons only on the account's own session (7.4). |
| S11 | Accepted | Keys are made for this protocol only (5.4, D-021). |
| S12 | Accepted | Keys change at a restart, the journal continues with the new registry's digest in the new segment, and the downtime is stated (5.4, D-021); a live key-control ring is Later (D-031). Also from this review: the `k256` 0.14.0 API names checked against the locked sources (5.2), and the verify-on-core mode reachable only from `e2e ablate` (16). |
| M3-PERF-01 | Accepted | Pre-verified layout gives the gate and the writer a physical core each (2.6); busy share per thread and "limited by" labels (15.4); CRC32C by slicing by 8 (11.4). |
| M3-LOAD-01 | Accepted | The sender never waits; operator backlog reported (14.10); valid-but-failing versus invalid runs defined (15.7). |
| M3-MEAS-01 | Accepted | Commands not served count as ∞ in the end-to-end percentiles; "over offered" and "of completed" (15.4). |
| M3-MEAS-02 | Partly accepted | A vDSO clock source and at most 50 ns per read required for headline runs; `saturating_sub` and per-stage inversion counts (15.1, 15.2). Not added: a pairwise skew ping-pong probe. The kernel uses only a clock source it found consistent across CPUs, and the inversion counts check every run's actual stamps. |
| M3-MEAS-03 | Accepted | The overhead restated as about 10 to 20% (15.1, 15.2) and measured once per session with `--stamps off`. A separate stamp ring is deferred until that comparison shows more than about 10%. |
| M3-MEAS-04 | Accepted, other fix | The sequencer publishes each record at once (9.1), which takes its batch delay out of the core path without a per-batch stamp in the trailer (which has no free word); core service documented as including the hop; stages don't add up (15.4). |
| M3-MEAS-05 | Accepted | The headline and the durable limit's inputs defined (15.7); Q1 updated. |
| M3-PERF-02 | Accepted | "Poisson in aggregate, bursty per gateway" (14.9); `u` = 0.7 and a section on bursts (17): a second model of the flow gives a mean of 299 µs and a p99 of 1.0 ms at `t_v` 50 µs and 10 gateways, worse than the review's 184 µs and 0.46 ms, so both are quoted as a range; the per-gateway queue-wait p99 is reported. |
| M3-ABL-01 | Accepted | `T` = 0 in the sweep and as a third arm of "fsync per order"; the per-order search starts at `1/F` with a drain cap (15.8, 16). |
| M3-ENV-01 | Accepted | Throttling read from `cpu.stat` and runs discarded; `floor(quota) − 2`; `max` and cgroup v1 handled; one CPU package; main pinned before allocating; a jitter probe (2.6, 15.10). |
| M3-FLOW-01 | Accepted | The fund starts with $1,000 (14.3), expected numbers in 14.7; the smoke flow's fund gets $1 and the smoke test asserts a shortfall (18.4). |
| M3-LOAD-02 | Accepted | Compact 64-byte pre-verified items, a memory cap on the doubling, full searches on PERPSBOX only (14.8). |
| M3-ABL-02 | Accepted | One 3-line record for both arms (3.3); cores used and verifications per core-second; overload points named (16). |
| M3-LOAD-03 | Accepted | Pre-signing times corrected, thread count `min(CPUs, floor(quota))`, the estimate replaced by the probed `t_sign` (14.8). |
| M3-MEAS-06 | Accepted | The budget, about 3 hours with the added arms (15.11); a resumable harness, one run per probe until the boundary, interleaved repetitions (15.5, 15.7). |
| M3-FLOW-02 | Accepted | The signed 500k and 1M points are stated to be overload tests and reported as such (15.6). |
| M3-FLOW-03 | Accepted (stated) | The deviation is stated in 14.4 and D-027; B2 is unchanged, because a flow segment with moving marks would add a generator mode for an effect limited to the warm-up. |
| M3-MEAS-07 | Accepted | 14.1 says so; each result prints its window's jumps, liquidations and sweeps; a second-seed headline run. |
| M3-ENV-02 | Partly accepted | The core CPU's frequency is recorded with every run (15.4). Not done: keeping gateways spinning idle in pre-verified runs; it would change the quota and the noise to hide a difference the recorded frequency already shows. |
| M3-MEAS-08 | Accepted | Rings pre-touched at creation (3.1); minor faults on hot threads checked in every run (15.4). |
| M3-MEAS-09 | Accepted | The consequences in 15.3 and 15.7; saturation throughput and the search's resolution reported; "no data" for an empty histogram. M/D/1 re-simulated: p99 about 40 µs at 97% and 57 µs at 98% utilisation. |
| M3-DOC-01 | Accepted | Ablations renamed (16); "the only unsafe code outside test binaries" (19.1); the capture-on fourth headline run (13.2); the loadgen header note (19.1); which stage REPORT.md's row uses (15.7, C57). |
| [build] | Gap, reading taken | 2.6 gives two tables, not a rule. `affinity.rs` uses the PERPSBOX table when there are at least 4 (signed) or 5 (pre-verified) whole two-sibling cores, else the local table (P0: main + journal writer, and the gate in signed mode; P1: core, and the sequencer in signed mode; the rest take the remaining CPUs in order); gateways that don't fit a roomy machine are an error, never a quiet fallback; `--gateway-smt` fills the gateway cores' A siblings, then their B siblings. |
| [build] | Deviation | `pipeline/tests/ring_stress.rs` runs 10M records per slot size in release builds but 1M in debug builds, where 10M takes about 15 s locally (18.3); `./dev cargo test -p pipeline --release --test ring_stress` runs the spec's count. |
| [build] | Gap, reading taken | `Instant` doesn't promise a nonzero first reading, but 3.3 uses 0 for "not applicable": `RunClock::now` returns at least 1. |
| [build] | Gap, reading taken | 15.3's rank `ceil(count × num / den)` is 0 for p0; the histogram uses at least rank 1, so p0 reports the smallest value's bucket, not bucket 0. |
| [build] | Gap, reading taken | No cgroup CPU files at all (`cpu.max`, `cpu.cfs_quota_us`) is read as "no quota" (2.6). |
| [build] | Spec typo, not fixed | 6.5 attack 6 says "the tag at byte 24 is signed"; since the expiry was added the command starts at offset 32, so the tag is byte 32 (5.1). |
| [build] | Gap, reading taken | 18.1 pins `ENGINE_SEMANTICS` on the smoke flow, which lives in `loadgen`, and `loadgen` depends on `pipeline`, so a `pipeline` test can't use it: `pipeline/tests/engine_semantics.rs` pins the first 100,000 commands of its own fixed flow (`pipeline/tests/common`), which reaches fills, liquidations, shortfalls and band sweeps. |
| [build] | Gap, reading taken | 11.8 step 4 starts the next life "in the segment after the end's segment"; for an empty journal (no valid header: the end is segment 0, offset 0) it starts in segment 0 itself, or the journal would begin with an empty segment 0 (`JournalPosition::next_life_segment`). |
| [build] | Addition | `create_segment` (11.1) writes the zeros under a temporary name and renames the file once it is synced, so a crash inside it can't leave a short segment; the header doesn't record `SEG_BYTES`, so recovery takes it from the segment files, which must all have one size. |
| [build] | Gap, reading taken | 11.2's "first 8 bytes of the writer's git commit id": `PERPS_GIT_COMMIT` is the short hash text (as the recorder's build sets it), so the header holds its first 8 ASCII bytes, zero-padded. A header with a valid CRC but an unknown magic, version, mode or profile byte, or nonzero reserved bytes, is an error, like an identity mismatch (11.8). |
| [build] | Gap, reading taken | 19.2's API, completed: `Pipeline::start` returns a `Result` (refused settings, a new journal in a directory that holds one, a clock anchored at or below the last `ts`, or a first segment that can't be created, all before any thread starts); `PipelineConfig` also has `mode` (the journal's mode, and the kind every lane record must carry, which the sequencer checks) and `phases` (the setup and the measured window by `t_sched`, 15.5 and 15.9, which main can set while the pipeline runs); `PipelineOutput` also returns the sequencer's counts. |
| [build] | Gap, not done | 15.9 counts places as GTC and IOC separately, but the trailer (10.3) carries only the command tag and has no free byte: the gate counts places by tag and outcome; a split needs a trailer bit, or the flow plan's counts. |
| [build] | Gap, reading taken | 15.9's fund figures: "starting capital" is the fund's equity just before the window's first command; the lowest equity and the largest drawdown are over the trailers of the window's commands, and the peak shortfall over their events. Clock inversions (15.2) are counted over every command, not only the window's. |
| [build] | Gap, reading taken | 18.4's release log holds every released engine-event slot (not the trailers), so it compares slot for slot with the capture and the replayed stream. |
| [build] | Deferred, then built | The "verify on core" ablation (section 16), built with the harness: `records::SignedFields` (nonce, expiry, signature: 10 words) follows the core record on a 3-line core ring (`CoreRecord::ABLATION_WORDS`, 22 words) in both arms; `Sequencer` and `run_core` are generic over the core ring's lines (2 by default); `PipelineConfig::ablation_verify_on_core: Option<VerifyOnCore>` (`Gateways`, or `Core(Box<dyn CoreVerifier>)`, refused with stamps off). The core arm's gateways are built with `Gateway::without_signature_checks` (checks 11 and 12 skipped, every cheap check kept) and the core verifies with `gateway::core_verifier::RegistryVerifier` before `apply`; a bad signature there panics, since the benchmark's flow is honest. |
| [build] | Gap, fixed | 9.2 promises only that `ts` "never goes backwards", but 11.3 and 11.8 need it strictly increasing, and `Instant` can return the same value twice (WSL2's Hyper-V clock source counts in 100 ns steps): two records sequenced back to back would get equal `ts`, and recovery would refuse a journal whose records were released. The sequencer takes `t_seq = max(now, previous t_seq + 1)`. |
| [build] | Gap, reading taken | 19.2's `gateway::spawn(gateway, ingress, lane, clock, idle, cpu)` gives the thread no way to count the window's rejects (7.3) or to share its total rejected with main's barriers (7.2, 14.4). `spawn` takes a `ThreadConfig { clock, idle, cpu, phases, counters }`: the phases are the pipeline's own (`Pipeline::shared_phases` and `Phases::phase_of`, both added to `pipeline`), and `GatewayCounters` holds the total rejected, the busy time and the thread id. 7.2's per-reason counts (window and total, `Busy` per tag) are the thread's `GatewayStats`, since `check` doesn't see `t_sched`. 19.2 passes the clock anchor twice (`Gateway::new`'s `start_unix_ns` and the clock): the loop asserts they are equal. |
| [build] | Gap, reading taken | 19.2's `decode(msg)` doesn't know the gateway's deployment, so it checks magic and version, the header's reserved bytes, the CMD40 and the tag; `Gateway::check` compares the deployment first, which keeps 7.1's order, since all three parts of check 1 give `WrongDomain`. |
| [build] | Gap, reading taken; the one-digest claim was false until F1 below | 5.4 shows the registry file but states only some of its rules. The loader requires, in order, exactly `# perps key registry v1`, then `deployment <id>`, then only `<account> <66 hex digits>` lines (one space; decimal with no sign or leading zero; hex digits only), so a registry has one spelling and so one digest; a blank line or another comment is refused, naming the line. |
| [build] | Gap, reading taken | 13.4 doesn't say what happens when a segment's registry digest matches none of the files given: each of that segment's kind-1 records counts as a failure (it can't be checked), as does every record of a registry whose `deployment` line isn't the journal's. The audit also checks the nonce order of kind-2 records (6.3 counts both client kinds), and "in parallel" means one thread reads while batches of 16,384 signatures are verified on every allowed CPU, so memory stays at one batch. |
| [build] | Addition | `gateway::spawn` installs the abort-on-panic hook too (2.8 names only `Pipeline::start` and `replay`): a gateway thread that panicked would otherwise drop its lane's producer, which closes the lane, and the run would carry on without that gateway's accounts. |
| [build] | Evidence | The generator (`loadgen::market_flow`) reproduces 14.7's model with seed 1: over 20 s of flow time, 99,881 client commands a second, of which 32.56% resting places, 32.56% cancels, 30.83% modifies and 4.04% IOCs, 667 marks a second, and 7 jumps; setup A, B1 and B2 hold 3,742, 2,608 and 1,206 items, all accepted by `Engine<Book, Fast>`; applied in plan order (no reordering across lanes), the engine rejects 0.026% of the first 500,000 timed client commands (93 `UnknownOrder` and 37 `PostOnlyWouldCross`, with 2 jumps in that stretch), and the smoke flow about 2.5%. Where 14.4 and 14.5 leave a draw's stream or order open, the reading taken: B1's quote sizes come from `MM(m)` (as a jump's do), the thin layer's setup draws are market, side, band share, quantity, offset (14.5's order), and B2 draws each IOC's quantity from `HL`. |
| [build] | Deviation | 18.4's smoke flow jumps 1 step in 500. `MarketFlowConfig::smoke()` sends about 13,000 client items per second of flow time, so the signed smoke run's 3,000 messages cover about 0.23 s of it: 91 steps at 6 markets, and at 1 in 500 a jump (hence a liquidation) about one run in six. The smoke config uses 1 in 20: a jump in 99% of seeds; with its seed 1 the first jump comes at client item 318 and liquidates at once, with a shortfall (a unit test checks it); the pre-verified run's 30,000 items see about 45 jumps. 14.5's `phase(m) = m × 15 ms / 67` uses the config's market count, as 18.4 asks of 14.3's formulas. |
| [build] | Gap, reading taken | 19.2's loadgen API, completed. `generate(config, n)` stops at the timed flow's `n`-th client item, so a shorter run's plan, and its arena, is a prefix of a longer one's (14.8); `SenderPlan::new(plan, messages, timing)` builds the sender's phases and schedules, and refuses messages built from another flow (by the config digest) or too few of them. `presign(plan, deployment, threads)` takes the keys' seed from the plan (14.8: one seed for the flow and the keys). `spawn_sender(plan, outputs, SenderConfig { clock, idle, cpu, barrier, stop, phases, counters })`: like `gateway::spawn`, the sender needs the pipeline's `Phases`, which it sets when the timed flow starts, since only it knows `t0` (15.5), and single-writer counters for main (3.2, 15.4). `Barrier` holds the pipeline's counters (`Pipeline::shared_counters`, added to `pipeline`) and each gateway's `GatewayCounters`, and ends the run with `SenderEnd::BarrierTimeout` after 10 s. The sender doesn't install the abort-on-panic hook: a panicking sender only closes its rings early, and the pipeline stops cleanly. A run schedules phases B1, B2 and the timed flow from one `Schedule`, continuing the `SCHEDULE` stream (14.6 names one stream); `poisson_schedule(items, rate, seed)` is kept. |
| [build] | Gap, reading taken | 14.10's sender, where the pseudo-code leaves room: pending operator items are also pushed after every item, not only while waiting and when an operator item falls due (2.4: "whenever space appears"), so a sender that is running late still pushes them; a stop during a wait ends the run at once. The lag histogram covers every client item scheduled inside the window, sent or dropped (a dropped item's `t_sent` is when it was tried). "How long it lasted" is the longest stretch with any operator item pending. |
| [build] | Gap, reading taken | 14.8's compact item is "meta, nonce and the CMD40 (7 words)", but the meta word's lane depends on the run's lane count, and the arena is reused across runs: `CompactItem` holds the account in the meta's place (still 56 bytes), and the sender builds the meta with the lane `account mod N`. `presigned.bin` (14.8 names its header's contents only): a 64-byte header (magic `PERPSGN1`, seed, deployment, message count, the flow config's digest: SHA-256 of a version constant and every field), then 136 bytes a message; loading refuses another seed, deployment or digest, and a file with fewer messages than the run needs. |
| [build] | Deferred | `--senders L` (2.2) is not built: one sender, whose lag p99 flags a generator-limited run (14.10). Several senders would need a barrier that waits for every sender to finish its phase before comparing the counts. |
| [build] | Addition | `pipeline::counters::set_hot_thread_hook(fn())`: every hot thread calls the hook once, as it publishes its tid, before its loop; the smoke test uses it to mark hot threads for its counting allocator (18.4: no allocation on a hot thread). No hook is set outside that test. |
| [build] | Gap, fixed | The smoke test found the journal writer allocating at a segment switch (the path and `File::open`). `JournalFiles::prepare(segment)` (a default no-op) opens each preallocated segment as it is created, and `StdFiles::exists` answers from its open files first, so a switch allocates nothing. |
| [build] | Deviation, since removed (F2-core-first-touch-faults below) | 18.4's zero minor faults hold for every hot thread except the core in the smoke run: the engine first touches its reserved capacity lazily (about 250 faults in the signed smoke's 350 ms window, about a dozen pre-verified), which a real run's 5 s warm-up covers. The smoke test prints the core's count instead of asserting it. Every run reports faults per thread (`health.*`) and flags any in the window (15.4). |
| [build] | Gap, reading taken | The timed flow runs a 200 ms tail past the window (`RunConfig::tail_ns`, not measured), so the closing sample of every thread's counters (15.4) is taken while the threads still run, and the window's last commands are released under load, not in the drain. |
| [build] | Gap, reading taken | Section 16's drain cap: main sets the sender's stop flag 10 s after the window closes; the pipeline still drains what was sent, and the run is a fail (`check.drain_cap_exceeded`), not invalid. (Fixed at integration: a sender that finished its flow before the cap left its backlog in the rings unflagged, e.g. a per-order run at 5k/s drained for 27 s after its window and passed the check; main now checks the cap again once every thread has stopped.) |
| [build] | Gap, reading taken | With stamps off (15.1) no trailer carries `t_sched`, so the gate can't tell the window's commands: 15.7's window reconciliation is skipped, and the throughput of both stamps arms is the gate's released commands per second of wall time between the window's two samples (`window.released_per_second`), which the cost-of-measurement table compares. |
| [build] | Gap, reading taken; since fixed (F9-flush-rate-whole-run below) | `journal.flushes_per_s` (15.7) is averaged over the run, from `Pipeline::start` to the stop: the writer's counters aren't sampled at the window's edges. |
| [build] | Addition | `--allow-generator-limited` (development only): a generator-limited run (14.10) counts as valid, with a flag and `run.allow_generator_limited = true` in its summary. On the local WSL2 machine the timer's tick of about 10 µs puts the sender's lag p99 near 10 µs at every rate, so without it no local run is valid. Never for reported numbers. |
| [build] | Addition | The CLI's structure (19.1 lists the commands only): `e2e sweep --kind load, commit-interval, stamps or headline`; `e2e report` rebuilds a session's `report.md` from its summaries; every run directory holds an atomically written `summary.txt` and a `report.md`. A session is resumable by run name (15.5), so one session directory holds one machine's runs at one set of timings: a changed template doesn't rerun a name that has finished. |
| [build] | Gap, reading taken | 13.5's restart, for the kill test (`e2e run --resume`): the rest of the flow is the plan's operator items after the first `operator_done` (the count of operator records in the journal; the operator ring is FIFO), plus every client item whose nonce is above its account's journaled nonce (a client resends what it has no result for); jumps are re-indexed (`workload::rest_of_plan`). |
| [build] | Gap, reading taken | 15.5's 60 s headline window is applied as twice the template's window (60 s at the default 30 s) to all five headline runs (timing ×3, capture-on, second seed). 15.7's durable limit runs the pre-verified 20k/s load point with the session's timings and the default journal (`T` = 1 ms, whatever is being swept). |
| [build] | Gap, reading taken; since replaced (F1-limit-label-fdatasync below) | 15.4's limit label, when the core isn't the limit: if the core stalled on output gating, the busier of the gate and the journal writer; else, if the sequencer found a ring full, the busiest of the core, the writer and the gate; else the busiest hot thread. |
| [build] | Gap, partly fixed | The GTC and IOC split of 15.9: the report takes the IOC places offered in the window from the flow plan (`window.ioc_places_offered`), next to the gate's place counts by outcome; the outcome by time in force still needs a trailer bit. |
| [build] | Gap, reading taken | `e2e replay` and `e2e recover` take the journal's identity from segment 0's header, print its commit and `ENGINE_SEMANTICS` next to the binary's, and replay under the binary's semantics. Bench runs use deployment `0x00BE_0003`, which the spec leaves open. |
| [build] | Gap, fixed | 2.2's busy time, in the journal writer: a pass that only flushed (the batch's `T` came due with no new record) ended a busy stretch begun at the last pass that took records, so the wait for `T` counted as busy, and that earlier pass twice. The flush-only pass now starts its own stretch (`run_writer`, with a unit test that fails without the fix). |
| [build] | Evidence, since fixed (F1-limit-label-fdatasync below) | 15.4's limit label with the journal on disk: the writer's busy share includes its time blocked in `fdatasync` (a pass that found work), so on a disk whose flush takes about as long as `T` (locally: `fdatasync` p50 0.8 ms at `T` = 1 ms, writer 89% busy at 20k/s) the writer is the busiest thread at every rate, and a signed run whose gateways are 95% busy is labelled "limited by journal writer". The label decides only the core-path search, which runs in discard mode where the writer never blocks, so it is left as specified; read the gateways' busy shares next to it in durable runs. |
| [build] | Evidence, since fixed (F2-core-first-touch-faults below) | The smoke-run deviation above (the core's first-touch faults, "which a real run's 5 s warm-up covers") doesn't hold in real runs locally: after a 5 s warm-up, the core took 1,960 to 6,815 minor faults in 15 s windows (6,808 at pre-verified 20k/s, twice; 1,960 at 100k/s; 6,436 to 6,815 signed at 15k to 25k/s) and 81 to 3,343 in 5 s windows; every other hot thread took none. They are the engine's memory reserved up front and touched on first use (`EngineOptions` of the M3 flow: 4,096 orders and 4,096 slots per market, 67 markets, much of it hash-map buckets that new ids land in). Every run is flagged (15.4: "must be 0"). A fix needs the engine to touch its reserved memory when it is built, a change to `engine/` this build did not make: the owner's call. |
| [build] | Gap, fixed | 14.8: the arena "is built once for the largest run a session needs and reused as a prefix for smaller runs and repetitions", but the harness kept only the last run's workload, so an interleaved sweep (15.5: A B C, A B C) rebuilt at every change of mode or size and re-signed its signed points at every repetition. `Workloads` now keeps one workload per flow, mode and deployment, and `run_interleaved` builds each, before the sweep, for the largest of the runs still to go. |
| [build] | Addition; the first seq since taken from the file's header (T1 below) | 13.3's standalone `e2e replay` writes `events-replay.bin` and `snapshot.txt` "for diffing by hand", but the live run kept no snapshot to diff against. A run that keeps its journal also writes the live engine's snapshot, `snapshot-live.txt`; `e2e replay` compares its rebuild with that and with the run's `events.bin` (from the file's first seq, since a restarted life's file holds only that life's events), prints `replay.events_vs_live` and `replay.state_vs_live`, and exits with status 2 if either differs. The kill test (18.4) runs it over its three lives. |
| [build] | Deviation | 18.2's codec property tests are "proptest, as configured in D-009", but `pipeline` (like `gateway`, for its nonce-model test) has no proptest dev-dependency and the manifests are frozen: both are fixed-seed xorshift loops over the same properties (codec: 100,000 random 40- and 56-byte inputs, and 20,000 random values of every variant; gateway: 1,500 messages over 3 accounts). No shrinking; a failure names its input. |
| [build] | Gap, open; `docs/RUNBOOK-PERPSBOX.md` sets it | 15.11's commit (and the header's commit field, 11.2): `PERPS_GIT_COMMIT` is set only by `./dev record build`; the `dev` container gets no such variable and hides `.git`, so every local run records commit `unknown` (an empty header field). A PERPSBOX build must set it: `PERPS_GIT_COMMIT=$(git rev-parse --short HEAD) cargo build --release --locked`. |
| [build] | Addition | `CRASH_RECOVERY_SCALE=k` multiplies `pipeline/tests/crash_recovery.rs`'s run counts (18.2), with new seeds past the default ones, for longer soaks. At `k` = 1,000 in `--release` (300,000 random runs of 6 lives, 300,000 F1 and 100,000 F2 variants) every check held, in 116 s. |

### Review of the build (2026-09-30)

Three reviewers (durability, security, measurement) read the build at commit 38df759 and
reported 22 findings, all confirmed. Each was fixed, or its claim corrected where the
behaviour was right; where a test can show the bug, a regression test fails without the
fix (named below; in the `*_regressions.rs` files, a comment on each test names the finding
it guards). Their other
notes found the output gating, the rings' orderings, the watermark, the sequencer, replay,
the gateway's checks and the audit correct.

| ID | Verdict | What changed |
|---|---|---|
| F-TAIL-UNION | Confirmed, fixed; spec clarified | Recovery took nonzero bytes in both parts of the tail region at once (the last `W` bytes after the end in its segment and the first `W` of the next) as one torn tail, and zeroed both: bit rot in synced data near a segment's end silently cut every record after it (2,000 released records in the reviewer's test). The writer syncs a segment's last batch before it writes the next segment, so one unfinished flush lies in one part only: 11.8 step 2 now says so, and `check_tail` refuses nonzero bytes in both, naming both places, with nothing changed. `journal_regressions.rs` (1 MiB segments); the unit test that encoded the gap now expects the error. |
| F-PARTIAL-RELEASE | Confirmed, fixed | The gate released a durable command's events before its trailer, so when the event ring filled in the middle of a command and the engine then panicked, part of that command had been released, against 2.8. The gate now releases a command only once its trailer is in the ring (one look at the last published slot, 12.1, 12.2). The exception, stated in 2.5, 2.8, 12.1 and 12.4 and in the poison-pill procedure: a command with more events than the ring holds (131,072) streams, since the core can't finish it otherwise; a fix for such a poison pill must reproduce the events already released. `gate_regressions.rs`, and 3 unit tests in `gate.rs`. |
| F-FRESH-START | Confirmed, fixed | A fresh start looked only at segment 0's header, so over a first flush whose header sector was lost it wrote in front of stale records that a later recovery would read on into. A new journal now runs recovery's steps 1 to 3 first (11.8, "A new journal"): a journal that scans (segment 0's header valid) refuses the start, a torn first flush is copied aside and zeroed, and any other nonzero byte is an error. `journal_regressions.rs`, and a unit test in `run.rs`. |
| F-DIR-FSYNC | Confirmed, fixed | Only the journal directory's parent was fsynced. `create_dir_all_durable` fsyncs the parent of every directory it creates, and the runner and the session create every level above the journal with it (11.1). Not testable without a power cut; unit tests check the directories. |
| F-TEST-W-BOUND | Confirmed, fixed | Every recovery test used 4 KiB segments, below `W` (622,720 bytes), so the bound itself was never exercised. `journal_regressions.rs` tests with 1 MiB segments that a nonzero byte `W` past the end, or at offset `W` of the next segment, is an error, and one at `W − 1` a torn tail; `crash_recovery.rs` adds random crash runs at 1 MiB (18.2). Mutation check: using the segment's end for the region's end fails 2 of the new tests. |
| F-TEST-X86-ORDERING | Confirmed, docs corrected | The stress tests run on x86, where Relaxed and Release/Acquire compile alike, so they check the rings' logic, not their orderings. Said in 3.2, 18.3 and `ring_stress.rs`; the orderings rest on 3.2's argument, which the reviewer checked by hand. |
| F-TORN-COPY-OVERWRITE | Confirmed, fixed | Two lives torn at the same place wrote the same `torn-<segment>-<offset>.bin`, and the second overwrote the first. Side files are created with `create_new`; a taken name gets `-2`, `-3`, ... (11.8 step 3.1). `journal_regressions.rs`, and a unit test. |
| F1 (registry) | Confirmed, fixed | The loader took CRLF line endings, a missing final newline and upper-case hex, so one set of keys had several files and digests, and an auditor who rebuilt the file from the clients' keys could miss the journal's digest. The file must now be byte for byte what `registry_text` writes (5.4). `registry_regressions.rs`, and a unit test. |
| F2 (expiry) | Confirmed, doc corrected | "Never forwarded after its expiry" overclaimed: the expiry is checked against the clock read when the gateway takes the message, and forwarding follows within one verification. 5.1 now says that; attack 9's bound of days is unaffected. |
| F1-limit-label-fdatasync | Confirmed, fixed | The journal writer's busy share included its time blocked in `fdatasync`, so every disk run said "limited by journal writer" (at 1k/s too). The writer's busy time is now CPU work only, its `fdatasync` time and flush count are published apart (2.2), the sequencer counts full passes per ring (core, journal), a late sender publishes its busy time, and 15.4's label rule is new: back-pressure first ("limited by the disk" or "journal writer", gate, core), else the busiest thread only if it is at least 90% busy, else "not saturated (busiest: X at Y%)". Locally, pre-verified 20k/s: before, "limited by journal writer" (87% busy); after, writer 1.4% CPU and 82% in `fdatasync`, "not saturated (busiest: core at 3.0%)"; signed 20k/s: "limited by gateway 1" (gateways 93% to 97% busy). |
| F2-core-first-touch-faults | Confirmed, fixed (engine: additive) | The engine's reserved memory (hash-map buckets above all) was first written inside the window. `Engine::prefault` and `Engine::prefault_market` (new, `engine/src/prefault.rs`) write it once; the core calls them before its first command and after each accepted `SetMarketParams` (15.4). They change nothing the engine holds or emits: an empty map is filled to its capacity and cleared, which leaves a fresh table; a map with entries (a replayed engine) is rebuilt, which is safe since the engine never iterates its maps (D-011). `engine/tests/no_alloc.rs` checks the same events and snapshot and no allocation after it; the smoke test now asserts zero faults on every hot thread, the core's included. Numbers in the table below. |
| F3-invalid-runs-in-medians | Confirmed, fixed | Invalid runs went into medians and ranges, the durable limit and `1/F`. Reports use valid runs only and say how many were valid; a sweep reruns an invalid repetition as `-a2`, `-a3` (15.5). `report_regressions.rs`. |
| F4-resume-keyed-by-name-only | Confirmed, fixed | A finished run was reused by name alone. Each summary holds its comparable settings (`run.*` keys); a mismatch is refused, naming the setting (15.5). |
| F5-fsync-ablation-achieved-rate | Confirmed, fixed | Disk rows showed the rate sequenced, which the journal ring's backlog keeps near the offered rate. The `T` sweep, the fsync ablation and disk sweep rows show the rate made durable next to it (15.8, 16). |
| F6-search-outcome-after-step-down | Confirmed, fixed | After a step-down the resolution is `first fail − result`; a saturation run that kept up (99% or more) is reported as "not saturated at 2×" with the highest rate achieved; the limit comes from the saturation run; the unused variable is gone (15.7 step 5). |
| F7-search-stuck-after-invalid | Confirmed, fixed | A resumed search read its own invalid attempts back and failed at once. `Session::run_valid` numbers new attempts after the existing ones, up to 3 per call (15.7). |
| F8-headline-yes-conditions | Confirmed, fixed | "Yes" came from any timing runs that existed. It now needs 3 valid timing runs whose clock counts toward a headline; before that the answer is "not decided yet" (15.7). `report_regressions.rs`. |
| F9-flush-rate-whole-run | Confirmed, fixed | Flushes per second were over the whole run. The writer's flush counter is sampled at the window's edges (15.4, 15.8); locally 732/s in the window against 750/s over the run. The `fdatasync` histograms stay whole-run, as 15.4 says. |
| F10-per-order-setup-barrier | Confirmed, fixed | At `B` = 1 phase A's 3,742 records need `3,742 × F`, over the barrier's 10 s once `F` passes about 2.7 ms. The per-order arm's barriers allow `4 × F` per item (14.4, 16). |
| F11-memory-cap-ignores-cgroup | Confirmed, fixed | The doubling cap used the host's memory; it now uses the cgroup's limit if lower, which the probe prints (14.8). |
| F12-setup-rejects-unchecked | Confirmed, fixed | A run with any engine reject in setup (A, B1 or B2) is invalid (14.4). |
| F13-dead-sender-counters | Confirmed, fixed | The sender's shared `offered` and `dropped` counters were written per item and read by nothing; removed. |
| Reviewer note (security) | Doc added | Cancel headroom (7.1) is against backlog, not an adversary: a registered account can fill it with cancels of ids it never used; per-account rate limits (D-031) must bound that first. |
| Reviewer note (measurement) | Not changed | The drain cap stops only the sender, so an overloaded per-order run still waits out its backlog, up to `65,536 × F` (about 1 minute at `F` = 0.8 ms). The verdict is right; it costs box time, which 15.11's 65 minutes may not cover on a slow disk. |
| [build] | Evidence, open | Pre-verified runs at 100k/s locally: when a lane ring (1,024 slots) fills during a stall of about 20 ms, the sender drops, as specified (2.4), and the flow, built in advance, diverges for good: a dropped cancel leaves a quote that later post-only quotes cross, and their cancels and modifies then find nothing. 3,615 window drops gave a 28.7% reject share (46,595 warm-up drops, 41.0%); runs without drops, 0.02% at 100k/s and 0.06% at 20k/s. Applied in plan order with one lane's next 1,000 client items dropped, the engine rejects 20% to 27% of client commands for the next 10 s of flow (100 drops: about 4%). 15.4 flags such a run; the runbook says not to trust it. |

**Before and after, locally** (WSL2, `./dev`, release, seed 1, 5 s warm-up and 10 s
window, `--allow-generator-limited`, so no run is headline-valid; "after" is the fixed
build, 1 to 3 runs):

| Run | Core faults in the window | Durable ack p50 / p99 | Engine rejects | Limit label |
|---|---|---|---|---|
| pre-verified 20k/s, before | 5,443 | 1.64 / 22.0 ms | 0.06% | limited by journal writer |
| pre-verified 20k/s, after | 0 (every hot thread 0) | 1.52 / 30.4 ms | 0.06% | not saturated (busiest: core at 3.0%) |
| pre-verified 100k/s, before | 2,324 | 1.65 / 54.8 ms | 41.0% (46,595 warm-up drops) | limited by journal writer |
| pre-verified 100k/s, after | 0; 10; 14 | 1.61 / 21.9; 1.47 / 12.3; 1.78 / 50.9 ms | 0.02%; 0.02%; 28.7% (3,615 drops) | not saturated (busiest: core at 10% to 11%) |
| signed 20k/s, before | 5,465 | 4.59 / 39.6 ms | 0.06% | limited by gateway 0 |
| signed 20k/s, after | 0; 0; 0 | 3.46 / 35.9; 8.13 / 76.0; 10.0 / 86.5 ms | 0.06%; 0.11%; 0.06% | limited by gateway 1 |

The first-touch faults were deterministic (the same count in runs of the same seed); the
few left at 100k/s are not (0 and 10 in two identical runs), and every other hot thread
took 0 to 2 in the same runs, so they point at the kernel (reclaim or page migration under
WSL2), not at the engine's memory. The durable-ack spread is the local disk (`fdatasync`
p99 3 to 6 ms, p99.9 up to 22 ms) and, in signed runs, the gateways' queues at 93% to 97%
busy (queue wait p50 0.9 to 6.4 ms), not the changes: the durability wait itself had a p50
of 1.4 to 2.2 ms in the signed runs after, 1.4 ms before.

### The optional libsecp256k1 verifier (2026-09-30)

Built after the owner's approval (D-032) and the review of the build script and of the
vendored C against upstream (`docs/SUPPLY-CHAIN.md`); 5.7 is the spec.

| ID | Verdict | What changed |
|---|---|---|
| [verifier] | Addition | `gateway::verifier` (`VerifierKind`, `PublicKey`); the registry loaded for one verifier, which the gateways, the core's ablation verifier and the audit then use; `e2e --verifier`; `run.verifier` in every summary and fingerprint, `search.verifier` in `search.txt`, the verifier in the reports; the probe per verifier (`<verifier>.verify_ns`, `scaling.<verifier>.<i>.*`: the scaling keys moved from `scaling.<i>.*`); `libsecp256k1/verify` and `libsecp256k1/per_call_context` in `pipeline_parts`; the cross-check tests and a libsecp256k1 signed smoke run (18.1, 18.4); the runbook's second pass. |
| [verifier] | Deviation | The plan was a per-thread verifier holding a verify-only `Secp256k1` context made once at start. `secp256k1` 0.33 has no call that verifies with a caller's context: `ecdsa::verify` takes none, and the deprecated `Secp256k1::verify_ecdsa` calls it and ignores its own. So nothing is held per thread; each gateway holds only its parsed keys (5.7, "No context object per gateway"). |
| [verifier] | Evidence, then done | Without the crate's `std` feature (the manifests enable `alloc` only), `ecdsa::verify` takes a spinlock on one global context, copies it into a stack buffer, then builds a fresh context over that copy anyway (the crate doesn't record the copy as made), for every call: 2.54 µs of the 41.1 µs locally (6%), measured alone as `libsecp256k1/per_call_context`. With `std`, the crate keeps a thread-local context made once per thread; the lockfile would not change, and `secp256k1-sys`'s build script doesn't read the feature. Left to the owner: the numbers so far include the 2.5 µs. The lock doesn't limit scaling locally: 4 threads verify 95,803 a second, 4 × 23,940 on one. | **Done 2026-09-30 by the lead:** `std` turned on (the lockfile gained only a recorded link to the already-locked `rand` 0.9.5, which is not compiled: `cargo tree` shows `secp256k1` depending on `secp256k1-sys` alone); every verifying thread calls `verifier::prepare_this_thread` before the timed flow (the smoke test's hot-thread allocation and fault checks pass 3 of 3 with libsecp256k1). `per_call_context` 2.65 µs -> 3.9 ns; libsecp256k1 verify 43.9 -> 39.2 µs (k256 80.1 µs, so 2.04x). |
| [verifier] | Gap, reading taken | Pre-verified runs record `run.verifier = none`, since nothing verifies, so a session with `--verifier libsecp256k1` reads a pre-verified run back whatever its verifier: the runbook's second pass starts from a copy of the first pass's pre-verified 20k/s runs, and both passes are judged against the same durable limit (15.7). |
| [verifier] | Gap, reading taken; since fixed (the independent check) | The registry parses each key with the chosen library only. `k256` alone also parses SEC1's "compact" form (tag 5 and `x`, 33 bytes too), which libsecp256k1 refuses; the one-spelling check (5.4) refused such a line anyway, but as "not in the one form". `PublicKey::from_compressed` now refuses any tag but 2 or 3 before either library parses, so both refuse exactly the same 33-byte keys (a tag other than 2 or 3, `x` not below `p`, no point with that `x`), tested for both, tag 5 included; a registry's digest is the file's, so it is the same for both verifiers. |
| [verifier] | Evidence, fixed (the independent check) | `gateway::thread`'s test `the_gateway_streams_between_a_sender_and_a_reader_and_closes_its_lane` failed in about 1 run in 13 of the feature build (3 of 38), never in 37 of the default build: with libsecp256k1 the gateway outran a reader the OS had paused, 64 records went unread in its 128-slot lane, and places were refused `Busy` (53 in one failure). Test only: its lane is now 512 slots, room for all 300 records plus the 64 free slots a place needs; 0 failures in 60 runs after. |
| [verifier] | Evidence | Locally (WSL2, Ryzen 7 2700X, release): one verification through the gateway's check, 78.9 µs with `k256` and 41.1 µs with libsecp256k1, 1.9 times faster (`pipeline_parts`); the quick probe's curves, `k256` against libsecp256k1: 12,723 against 23,940 a second on one core, 45,400 against 95,803 on 4, 73,580 against 138,546 on 7 threads with SMT. The smoke test's libsecp256k1 signed run passes every check of 18.4: no allocation on the hot threads during the timed flow, no minor fault in the window, the replay identical, and the audit, verifying with libsecp256k1, clean. Not headline numbers: PERPSBOX's second pass measures them. |

### The EIP-712 scheme (2026-09-30)

Built after the owner's decision (D-033), in three steps: the primitives first, against
official known answers (keccak-256, MessagePack, the digest, recovery); then the gateway,
the pipeline, the journal and the audit; then the load generator and the harness. 5.8 is
the spec.

| ID | Verdict | What changed |
|---|---|---|
| [eip712] | Addition | `gateway::keccak`, `gateway::msgpack`, `gateway::eip712` and `gateway::salts`; version 2 of the message (`wire.rs`); recovery, addresses and `sign_recoverable` (`verifier.rs`); `Gateway::new_eip712` and its 13 checks, with five new reject reasons (`GatewayReject::COUNT` from 12 to 17); `Gateway::prepare_this_thread`, which the gateway thread calls before its loop; `pipeline::records::AuthScheme`, header byte 120 and the journal's identity (11.2); the audit's EIP-712 branch (13.4); `presign_eip712`; `--auth`, `run.auth`, `search.auth` and the `-eip712` run names; the probe keys and benchmarks of 15.10; the scheme in the reports and section 17's analysis; the tests of 18.1, 18.2 and 18.4. `engine::id_hash` became public so the salt table reuses the seeded hash of D-011. No new crate, and `Cargo.lock` is unchanged; the `c-secp256k1` feature also turns on `secp256k1`'s `recovery` feature (`docs/SUPPLY-CHAIN.md`). |
| [eip712] | Evidence | Known answers, before any pipeline code: keccak-256 of `""` and `"abc"`; the Keccak team's two worked permutations; NIST's SHA3-256 answers at 0, 1, 135 and 136 bytes through the same sponge with SHA-3's padding; the EIP-712 Mail example in full, its `v` = 28 signature included; the TypeScript SDK's two `data` vectors; all 12 of Polymarket's golden vectors, each `data` and each signature byte for byte, and the address recovered by `k256` and, in the feature build, by libsecp256k1 (5.8, 18.1). The golden file gives each operation's `data` and signature only: the MessagePack bytes are shown by hashing to `data`, the digests by the byte-identical RFC 6979 signatures, and the key's address comes from Circle's `cctp-go` and is derived from the key in the test. |
| [eip712] | Gap, reading taken | The clock for the window: the gateway loop passes the clock in nanoseconds, as in the perp scheme, and the EIP-712 checks round it down to whole milliseconds (`now_unix_ns / 1,000,000`), so `Gateway::check` has one signature for both schemes. A `ts` exactly 5 minutes old is accepted for the whole of that millisecond. |
| [eip712] | Gap, reading taken; the owner to confirm | The audit's window (13.4) is `[record − 5 min − SEQUENCING_SLACK_MS, record + 60 s]`, with the slack 10 s, not exactly `[record − 5 min, record + 60 s]`. The gateway's check time is not journaled, and a record may wait in its lane before the sequencer stamps it, so a bound without slack could fail a message the gateway rightly took at the edge. The upper bound is exact: the gateway and the sequencer share the run clock, and the gateway reads it first. |
| [eip712] | Gap, reading taken | Sizing the salt tables: each gateway's table has room for exactly the messages the sender will offer it in the run (`runner::requests_per_gateway`), not rate × time × a margin. `SaltTable` already keeps 2 to 4 slots per request and a run's timed flow is capped at 3 minutes, so nothing expires and no table fills. The signed search's memory cap counts 4 × 24 bytes per item for it. The 100k/s headline over 10 gateways takes 48 MiB per gateway, 480 MiB in all. |
| [eip712] | Addition | Messages go stale 5 minutes after their `ts`, and the load generator signs them before each run, so: `RunConfig::check` refuses an EIP-712 run whose timed flow is over 3 minutes (the headline's `--window` must stay at 85 s or less, since the headline doubles it); workloads are kept per scheme, and an EIP-712 one is signed again when its age, plus the run's sending time, plus a 60 s margin, would pass 5 minutes (14.8); `Workloads::reserve` doesn't sign EIP-712 workloads ahead of a sweep; `run_interleaved` checks every point before signing anything; and a run in which a gateway refused a harness message as `StaleTimestamp`, `FutureTimestamp`, `ReusedRequest` or `SaltTableFull` is invalid. |
| [eip712] | Addition | `RunConfig::in_mode` gives a sweep's, a search's or an ablation's pre-verified runs the perp scheme (they sign nothing), so one template serves both modes; the durable limit comes from pre-verified runs as before. A summary written before `run.auth` existed reads as `perp` if signed and `none` if not (`config::recorded_before`), so sessions already on disk stay resumable, and an EIP-712 session may start from a copy of the first pass's pre-verified 20k/s runs. |
| [eip712] | Deviation, since removed (R8 below) | `presigned.bin` moved to version 2 (`PERPSGN2`, a 72-byte header, 14.8): the four reserved bytes of version 1 could not hold a `u64` signing time. Version-1 files were refused. The harness never saves or loads this file. |
| [eip712] | Evidence, since fixed (R4 below) | keccak-256 is slow: 3.0 µs per block in a release build (`keccak/64_bytes`; the probe's `keccak.64_ns` said 2,974 ns), so the digest costs 9.1 µs and hashing about 12 µs of every signer check: about 12% of it with `k256` (101.9 µs) and 21% with libsecp256k1 (57.0 µs). D-033 expected all four blocks under a microsecond. The permutation (`keccak_f`) is written to be read, with loops and `% 5` indexing, and was never profiled. A tuned permutation would lower the EIP-712 gateway cost by about that share; it is the owner's call, since the owner must be able to read it, and best made before the PERPSBOX passes. |
| [eip712] | Evidence, superseded (R4 below) | Before the keccak fix, locally (WSL2, Ryzen 7 2700X, release, criterion): `k256` verify 80.1 µs, the whole EIP-712 signer check 101.9 µs, recoverable signing 74.5 µs; libsecp256k1 verify 39.7 µs, signer check 57.0 µs; `encode_op` 64 ns; the digest 9.09 µs; keccak-256 of 64 bytes 3.01 µs, of 136 bytes (two blocks) 5.98 µs; a salt-table lookup and insert 26 ns, a replay's lookup 25 ns. The quick probe, `k256`: `recover_ns` 100,863; signer checks a second on 1, 2, 4 and 7 threads (the last with SMT): 9,890, 19,570, 37,306 and 56,240, against 12,603, 24,953, 41,750 and 71,756 verifications. A release `e2e run --smoke --auth eip712`: the audit checked 3,230 EIP-712 records with no failure, the replay was identical, no hot thread took a minor fault, and the gateways refused nothing (the run is invalid only as generator-limited, as perp smoke runs are on this unpinned machine). Not headline numbers: the runbook's EIP-712 passes measure them on PERPSBOX. |
| [eip712] | Evidence, since explained and fixed (T1 to T3 in "Three intermittent test failures" below; none was the scheme's) | In the feature build's full test runs, two tests of unchanged perp code failed now and then. The smoke test's zero-minor-fault assertion failed on the load generator's sender (1 to 4 faults) or the core (2), in about 1 run in 3 of back-to-back bench runs, and once in about 20 EIP-712 smoke runs right after a build; it passed alone every time, and no gateway thread ever faulted. And `bench/tests/crash.rs` failed once: when life 2 finished the flow before its random kill point, life 3 released nothing, and `e2e replay`'s `events_vs_live` compared an empty `events.bin` from seq 1 with the whole replay and said "different"; it passed 5 of 5 times alone (it failed the same way once more in the documentation step's first feature-build run). Neither was run on the code before the scheme, so it is not shown that they predate it. In the documentation step's second feature-build run, `pipeline/tests/gate_regressions.rs` (unchanged code) failed once too: its check that the core's sink waited for ring space found no wait. It passed 8 of 8 alone; the test seems to assume the core fills the ring before the gate thread it has just started frees the first command's slots, which a loaded machine can reverse. |
| [eip712] | Gap, open | The salt table expires requests lazily (5.8): in a run longer than 5 minutes, expired slots are reused only where an insert's walk passes one, so a table sized for fewer requests than the run's can refuse `SaltTableFull` while most of its entries are expired. v1's runs are capped at 3 minutes; a long-lived deployment would need backward-shift deletion. |
| [eip712] | Gap, open | Polymarket refuses a client order id of all zeros; v1 doesn't check it (`order_id` 0 is written as 32 zeros). Harmless here, since ownership comes from the order id's high 32 bits. |

### The EIP-712 scheme's review (2026-09-30)

Four reviews of the built scheme (its format against Polymarket's, security, performance
and determinism, readability) reported 13 findings; R5 and R7 are the same one. Each was
checked against the code before anything changed. Afterwards the whole workspace passed
its tests in the default build and in the `c-secp256k1` build (no intermittent failure in
either run), clippy passed with `-D warnings` in both, and `Cargo.lock` is unchanged.

| ID | Verdict | What changed |
|---|---|---|
| R1 | Confirmed, docs | Upstream drift after the format was researched: py-sdk's "add session-oriented builder codes" (#313, 2026-09-30) and ts-sdk's `toRawPerpsOrder` append a reserved slot and a builder pair `[address, feeRate]` to an order (and to TP/SL children) when a builder session is set. Without a builder nothing changes, so our encoder, the golden vectors and the TypeScript `data` vectors stay valid. Stated in `eip712.rs`, 5.8 and D-033, step 1. |
| R2 | Confirmed, documented; since closed at the gateway by the owner's choice of replay key (2026-09-30, the next entry) | A cancel's or a modify's market is not signed, so a copy with another market passes every gateway check and uses up the request; the engine answers it `UnknownOrder` or `UnknownMarket`, and the genuine message, sent after it, gets `ReusedRequest`. At the time, keying the request by the market as well was set aside because it accepts one signature once per market id, and the full fix needs the engine to find those orders by their id alone. The owner then chose the market in the key (the next entry), and the test that pinned the gap now shows the genuine message accepted. |
| R3 | Confirmed, documented; the uniqueness check now uses the replay key (the next entry), and a market changed in the journal after the fact is still not seen | The audit can't see a changed market on a journaled EIP-712 cancel or modify, and the replay of such a journal leaves that order on the book. The guarantee is restated in `audit.rs`, 13.4, 5.8 and D-033; `a_changed_market_on_an_eip712_cancel_or_modify_is_not_seen` pins it. |
| R4 | Confirmed, fixed | `keccak_f` walked the 25 lanes with one index and `% 5`, `/ 5`, so the compiler kept the divisions, bounds checks and table loads. Loops over `x` and `y`, the same steps, let it unroll them: one block 3.01 → 0.41 µs, 7.3 times faster; the digest 9.09 → 1.32 µs. The Keccak team's worked permutations, NIST's answers and every golden vector still pass. New local numbers (WSL2, Ryzen 7 2700X, release, criterion, one session): `k256` verify 78.5 µs, signer check 90.7 µs (was 101.9), recoverable signing 71.2 µs; libsecp256k1 verify 39.3 µs, signer check 46.7 µs (was 57.0); `encode_op` 68 ns; keccak-256 of 136 bytes 0.82 µs; a salt-table lookup and insert 25 ns, a replay's lookup 24 ns. The quick probe, `k256`: `recover_ns` 90,468, `eip712.digest_ns` 1,407, `keccak.64_ns` 459; signer checks a second on 1, 2, 4 and 7 threads 10,913, 20,350, 36,823 and 61,510, against 12,686, 25,253, 47,116 and 70,540 verifications. By section 17's formula 100k/s needs 14 gateways with `k256` (was 15) and 7 with libsecp256k1 (was 9). Updated in 5.8, 17, D-033 and the runbook. |
| R5, R7 | Confirmed, fixed | A newly signed EIP-712 arena was used without the freshness check. Its `ts` is when signing started, so a long signing could leave a run's last messages stale, and each retry would sign again and fail the same way. `Workloads::get` now checks the new arena too, once signed, and refuses the run with how long signing took (`RunError::Refused`, which stops a sweep or a search instead of retrying). Test: `a_run_that_newly_signed_eip712_messages_cant_last_through_is_refused`. Said in `workload.rs`, `config.rs`, `e2e.rs`, 5.8, 14.8, D-033 and the runbook. |
| R6 | Confirmed, fixed | The session report's headline pooled every `-timing-r` run, so a session holding both schemes' headline runs mixed them under whichever label sorted first. It now writes one headline per scheme, the perp scheme first (test: `a_session_with_both_schemes_headline_runs_gets_one_headline_per_scheme`), and `e2e.rs` no longer says a session never mixes the two. |
| R8 | Confirmed, fixed | `presigned.bin` had moved to version 2 for every arena, perp included. A perp arena's file is version 1 again, byte for byte (a test checks its header against that layout); only an EIP-712 arena's file has the magic `PERPSGN2`, with 8 more header bytes for its signing time (14.8). |
| R9 | Confirmed, fixed | `verifier::prepare_this_thread` had gained a libsecp256k1 recovery that perp gateways and the core in the verify-on-core ablation never need, while EIP-712 gateways ran a second one in `Gateway::prepare_this_thread`. The first is gone (recovery uses the thread's context that the verification allocates), and one constant, `verifier::GENERATOR`, serves both warm-ups. |
| R10 | Confirmed, fixed | `keccak.rs` said each hash of the scheme takes "one or two" permutations; each takes one (at most 135 bytes), and only the tests' 160-byte domains take two. |
| R11 | Confirmed, fixed | `SaltTable::bytes_for` was used only by its own test: removed. |
| R12 | Confirmed, docs | `engine::id_hash` became public for the salt table: now recorded in D-033's trade-offs, for the owner to confirm. `salts.rs` says how the one-key hasher chains the request's three fields. |
| R13 | Confirmed, fixed | `RHO` and `ROUND_CONSTANTS` now state their rules (FIPS 202, Algorithms 2 and 5), and `the_tables_follow_their_rules` derives both tables from them, as XKCP's reference implementation does. |

### The EIP-712 replay key gains the market (2026-09-30)

The owner's decision on R2 and R3: put the market in the replay key, rather than change
the engine to find a cancel's or a modify's order by its id alone. Updated: 5.8 (step 1,
the message layout, the replay bullets, the salt table, the check table, attacks 7 and 8,
the journal bullets), 13.4, 18.1, 18.2, C61 and D-033; `salts.rs`, `check.rs`,
`audit.rs`, `lib.rs` and one sentence of `eip712.rs`.

| ID | Verdict | What changed |
|---|---|---|
| [replay-key] | Decision, built | `salts::Request` gained `market`, and `Request::of` takes it from a place, a cancel or a modify (`None` for an operator command). The salt table compares all four fields, and its seeded hash covers them (the market as one more `u64` write). The gateway builds its request with `Request::of` (`check.rs`), and the audit's uniqueness set holds the same `Request` (`audit.rs`), its failure naming the market. Every EIP-712 command is keyed this way, places too. The slot stays 24 bytes: `salt` and `ts`, 8 bytes each, `account` 4, `market` 2 and the used flag 1 make 23, padded to 24 for the `u64`s' alignment, so the market took 2 of the 3 bytes of padding. `SLOT_BYTES` is unchanged, and with it the tables' memory and the signed search's memory estimate. |
| [replay-key] | Consequence, documented | A copy of a signed cancel or modify with another market is now another request: the gateway accepts it (its signature is valid), the engine rejects it and changes nothing (`UnknownMarket` for a market that doesn't exist, `UnknownOrder` for one that does, since an order id's sequence is unique per account across all markets), and the genuine message is still accepted, whether it comes before or after the copy. A copy with the same market is `ReusedRequest`. So whoever holds the signed bytes can make the engine reject up to one junk copy per market id (the other 65,535 values of a `u16`, while the `ts` is in the window), and can never make the genuine message `ReusedRequest` or reach another order. Each copy is an accepted message: it costs a recovery, a table slot, a lane slot and a journal record, so a flood of copies could fill a table sized for the honest requests only (`SaltTableFull`). Only a network gateway's per-session limits would bound that (7.4); v1's one client is in process. Places: their market is signed, so a copy with another market is `WrongSigner` and takes no slot; the one change is that such a copy of a place already accepted was `ReusedRequest`, before the recovery, and is now `WrongSigner`, after one, like any forgery. The audit: a journal with copies next to the genuine record passes, an exact repeat still fails, and a market changed in the journal after the fact is still not seen (R3). |
| [replay-key] | Tests | `a_cancels_or_a_modifys_market_is_carried_but_not_signed_so_a_copy_uses_up_the_request`, which showed the genuine message refused, became `a_cancel_or_a_modify_copied_to_another_market_is_another_request_and_the_genuine_one_is_accepted`: the copy first or the genuine message first, both accepted, an exact copy of either `ReusedRequest`, and copies on markets 0, 1, 4 and 65,535 all accepted before the genuine one. New: `a_places_market_is_signed_so_a_copy_on_another_market_is_the_wrong_signer_and_takes_no_slot`; in `salts.rs`, `copies_of_a_request_on_other_markets_are_other_requests_and_each_is_accepted_once`, `the_request_of_a_command_takes_its_market_and_operator_commands_have_none` and `every_field_of_the_request_moves_where_its_walk_starts`; in `audit.rs`, `an_eip712_cancel_or_modify_copied_to_other_markets_passes_next_to_the_genuine_one`. Both property tests (the gateway's model and the salt table's) now key by the market, over two markets. The table's full and expiry tests are unchanged and pass. `gateway`: 130 unit tests (136 with the `c-secp256k1` feature), all passing in both builds, and clippy with `-D warnings` clean in both. |
| [replay-key] | Gap, open | `bench/benches/pipeline_parts.rs` (the `salts` benchmark) builds `Request { account, salt, ts_ms }` and needs `market` too before it compiles (`cargo clippy --all-targets` or `cargo bench` on `bench`); `bench/` was being changed by another step, so it was left to that step. The salt table's timings (25 ns a lookup and insert, 24 ns a replay's lookup, R4) were measured with three fields; the market adds one splitmix64 step to the hash, not re-measured. |

### Three intermittent test failures (2026-09-30)

The three tests of unchanged code that failed now and then in the EIP-712 scheme's full
test runs (the `[eip712]` row "Evidence" above) were each traced to its cause, reproduced
on purpose, and fixed at that cause. None was the scheme's: two were the tests' own
assumptions, one the kernel's. No retry and no sleep. One assertion changed, because it
was wrong: the smoke test took every minor fault in the window for a first touch, and now
fails only faults in a window where the kernel migrated no page (T3).

| ID | Verdict | What changed |
|---|---|---|
| T1-EMPTY-LIFE | Confirmed, fixed: `e2e replay`'s comparison was wrong; the kill test now covers the case every run | The kill test killed life 2 after a random number of released slots, `between(100, 3_000)`, whatever life 1 had left. When life 1's kill came late (its range reached 8,000 of about 11,280 slots, and a loaded machine notices a kill point late), life 2 sent the rest of the flow and stopped by itself, and life 3 had nothing to send: it issued no command, released nothing, and wrote an empty `events.bin` whose header said first seq 0. `e2e replay`'s `events_vs_live` took the first seq from the file's first slot, 1 when there was none, and compared the empty file with the whole replay: "different: the live run released 0 events, the replay emitted 11284". The run was right; the comparison wasn't. **Fix:** `events.bin` version 2 (13.2): its header names the life's commands (first seq, and last seq = first − 1 if it issued none) and says whether the capture is incomplete (a flags word where version 1 had a reserved one); version 1, whose first and last seq were the first and last slot's, is refused. `e2e replay` compares exactly the replayed events of the commands the header names, so an empty life, a life that started after seq 1 and a life that later lives carried on from all compare right, and an incomplete capture is "not compared", as in 13.3 (`pipeline::gate::EventsHeader`; `write_events_file(path, &header, slots)`; `read_events_file` returns the header). **The kill test:** life 1 is killed within its first 6,000 slots, life 2 at most halfway through the slots the journal still lacks (`FLOW_SLOTS` = 11,200: the signed smoke flow's 3,389 commands released 11,269 to 11,286 slots in six runs), so both kills fall inside the flow with room to spare; a fourth life, restarted after life 3's clean stop, has nothing to send, and must pass life 3's checks and `e2e replay`: the failing case, now in every run. With the old comparison put back, the kill test fails at life 4 every time, with the reported message. Tests: `the_live_events_are_compared_with_the_replayed_events_of_the_same_commands` (`e2e.rs`: an empty life, lives in the middle and at the end, a missing and a changed event, an incomplete capture); `an_events_file_round_trips` (`gate.rs`: the header, the flags, version 1 refused). |
| T1-STALE-SNAPSHOT | Gap, open | `e2e replay`'s *state* comparison is still with `snapshot-live.txt` as the last finished life left it. On a run directory whose last life was killed after an earlier one finished, the journal goes on past that snapshot, so the state reads "different" (the events now compare right). The kill test never does this (its last two lives finish); by hand, replay such a directory knowing the files are the last finished life's. Telling the two apart needs the snapshot to name its last seq too. |
| T2-GATE-HANDSHAKE | Confirmed, test fixed; production unchanged | `a_command_whose_events_filled_the_ring_is_not_released_before_its_trailer` filled its 8-slot event ring, then started the gate thread and let the core go on with seq 2, assuming the core would find the ring full before the new thread freed seq 1's slots. On a loaded machine the gate can run first: the core then finds room, never waits and never publishes seq 2's first events, and "the sink did wait for space" fails, though nothing partial was released. With a 20 ms pause after starting the gate, the old test fails every time. Now the core's part runs on a thread of its own (it has to wait), and the gate starts only once all 8 slots are published, which only the sink's wait for space does while nothing reads the ring: the ring has filled mid-command and the sink is waiting, whatever the scheduling. With a 20 ms pause at each of the handshake's three steps, the new test passes. The gate and the sink are unchanged: that the gate releases nothing partial is the property; the ordering was only the test's. |
| T3-MIGRATION-FAULTS | Confirmed: the kernel's faults, not a first touch; the check now tells them apart | **Reproduced:** 1 window in 75 (the core, 1 fault) in 15 rounds of "rebuild `bench`, then `crash.rs`, then `smoke.rs`"; with a background loop that writes and deletes 3 GB files and allocates 5 GB (which makes the kernel compact memory), 10 windows in 125, on every kind of hot thread (sender, both gateways, sequencer, journal writer, core, gate; 1 to 7 faults each). **Evidence:** a temporary probe (removed) read, at both edges of each window, `/proc/vmstat`, every thread's minor and major faults, and the present and swapped bits of every page of every mapping (`/proc/self/pagemap`). Every faulting window had page migrations in it (`pgmigrate_success` 13,746 in the one after a build, 4,193 to 255,298 under the churn; in 350 ms); the 170 windows without migration had no fault; and in no faulting window did any page of any mapping go from absent to present, so nothing was touched for the first time (only the probe's own buffers, on main). Pages mid-move showed at the edges as migration entries (pagemap's swapped bit): one page of the test binary's code and one of a 40 MB heap region, in the window after the build. Reclaim alone faulted no hot thread (up to 116,613 pages reclaimed in a window). **Why:** compaction replaces a page's mappings with migration entries while it copies the page, and a thread that touches it meanwhile waits in a minor fault. Hot threads touch their pages constantly, so they meet the moves; the sender and the core, with the largest working sets (the message arena, the engine's memory), met them first. A build or the kill test's children free gigabytes, and the kernel compacts after. Pre-touching can't help (the pages are touched), and pinning pages needs `unsafe` and still wouldn't stop it (`mlock`ed pages are migrated too here: `vm.compact_unevictable_allowed` = 1). **Fix:** `pipeline::counters::page_migrations` (`pgmigrate_success` + `pgmigrate_fail`); main reads it at both edges (`Sample::page_migrations`, `WindowHealth::page_migrations`, `kernel_migrated_pages`); every run records `health.page_migrations`, its fault flag reads "N minor page faults on hot threads in the window, while the kernel migrated M pages", and the report's health paragraph gives the count. The smoke test fails a fault only in a window without migrations and prints the others with the count (15.4, 18.4). A regression that first-touches memory in the window faults in every window, and most windows see no migration (74 of 75 after a build, 96 of 125 even under the forced churn), so it still fails. Trade-off, for the owner: on a machine compacting all the time, such a regression could pass a smoke run while being printed. |
| T1 to T3 | Evidence | After the fixes, in the `c-secp256k1` build: 20 rounds of `crash.rs` then `smoke.rs` back to back, 20 of 20 passed (no fault to excuse; life 4 issued 0 commands every time); 10 rounds of "rebuild `bench`, `crash.rs`, `smoke.rs`", 10 of 10 (one window, after the first rebuild, had 2 faults while the kernel migrated 81,770 pages: printed, not failed); 15 rounds under the forced-compaction churn, 15 of 15 (4 windows with 1 to 26 faults in all, each while the kernel migrated 14,943 to 124,703 pages). The gate test, 300 times alone and 300 times beside 14 spinning processes: no failure, where the old test failed 104 times in 300 beside them (none alone). The whole workspace passed its tests in both builds (606 and 614), clippy with `-D warnings` passed in both, and `Cargo.lock` is unchanged. |

### The Polymarket-shaped flow (2026-09-30)

Built after the owner's decision (D-034), in three steps: the calibrated profile and its
tool, then the generator, then the harness; 14.12 is the spec. Where the build departs
from D-034's text, D-034's "Corrected on building" says so too.

| ID | Verdict | What changed |
|---|---|---|
| [d034] | Addition | **The calibration:** `tools/calibrate/` (`calibrate.py`, `scan.py`, `derive.py`, `rust.py`; the first calibration's `flow.json` and `book.json` copied verbatim into `inputs/`, their SHA-256 in the profile's `about`; `profile-2026-09-30.json`) and `loadgen::market_flow::profile` (hand-written types) with `polymarket_profile.rs` (generated, `POLYMARKET`). Re-derived from `data/`: the instrument table and start prices, the maker and taker weights, the spread fits, the gaps, the clip shares and dust, the jump rate and sizes, the taker clusters. Where it overlaps the first calibration it matches exactly: all 88 start prices, every market's inner level changes (66,281,072 in all), taker events (46,093) and prints (62,006). Taken from `inputs/` (fits too costly to redo): the price-move mixtures, the taker-notional mixture and point masses, the burst AR(1) pairs, the shock numbers, and the maker add, remove and size mix. `extract --check` and `generate --check` both say "identical" (extract: 3 min 6 s on 6 processes). |
| [d034] | Addition | **The generator:** `market_flow::polymarket` and `polymarket/market.rs`; `FlowPlan<C = MarketFlowConfig>` and the trait `PlanConfig`, so that `presign`, `presign_eip712`, `preverified` and `SenderPlan::new` take either flow's plan; `Arrivals::Cox(Bursts)` in `schedule.rs`. The M3 flow's code, plan, digest and pinned tests (`market_flow/tests.rs`) are byte for byte unchanged, and Poisson and uniform arrivals draw exactly as before. |
| [d034] | Addition | **The harness:** `bench::e2e::flow::Flow` (the M3 flow or the Polymarket one, one plan type for the whole harness); `--flow m3\|polymarket\|smoke\|polymarket-smoke`, `--makers K`, `--bursts median\|busiest`, `--shock real\|stress`; `run.flow`, `run.makers`, `run.bursts` and `run.shock` in every summary and fingerprint, read as `m3`, `default`, `none` and `none` in older summaries; run and search names with the flow's suffix, and `search.flow`, `search.makers`, `search.bursts`, `search.shock`; `FlowContent`'s new counts and the `flow.*` keys, `MarketShares`; the flag for a flow with shocks whose window held none; `session::limit_point`; `sweep::polymarket_points` and `e2e sweep --kind polymarket`; in the reports, the flow's name, "the flow's shape in the window", the per-gateway table's "Offered" and "Busy (CPU)" columns, one headline and one breakdown per flow and scheme (the M3 flow first), the searches table's Flow column, the session's list of flows and a caveat; `GateStats::set_mark_core_service` in `pipeline/src/gate.rs`, with its test; the four Polymarket smoke runs (18.4); `docs/RUNBOOK-PERPSBOX.md` 7b. No new crate, `Cargo.lock` unchanged, no `unsafe`. |
| [d034] | Deviation from D-034, the owner to confirm | **Scale:** D-034 says a flow-second holds as many messages as the run's offered rate. The generator holds a fixed `messages_per_flow_second`, 100,000 (100,234 client messages measured), whatever the rate, as the M3 flow does: plans stay independent of the rate (14.1) and runs at different rates share one signed arena (14.8). So flow time is real time only at an offered 100k/s; at 400k/s prices move 4 times as fast. |
| [d034] | Deviation from D-034 | **Book background sizes:** D-034 names a lognormal background. The table holds the recorded levels' own quantiles (64 values; log bins of 1/200 decade), since the sizes are bimodal (small orders and large ones) and a lognormal with their mean and sd of ln(USD) puts its largest values 3 to 10 times too high (tradfi equities' largest of 64: $5.9M from the lognormal, $0.86M recorded). The mean and sd are kept in the table for the reader. |
| [d034] | Deviation from D-034 | **Bursts' normalisation:** D-034 says "normalised so the mean stays `R`", which the profile states as division by the long-run mean `exp(V / 2)`. Over one run the slow component barely moves (half-life 326 s in the median hour), so its level alone could put a run's load 20% away from `R`: the schedule divides each phase's multipliers by their own mean instead (14.9, 14.12). `profile.rs`'s `BurstModel` doc still names `exp(V / 2)`, the long-run statement; `schedule.rs` says what the send schedule does. |
| [d034] | Deviation from D-034 | **The move mixture's units:** D-034 quotes the 10x class's mixture as "sd 0.47/1.17/3.88 bps". The fit (`flow.json`, `normal_scale_mixture3`) is standardised by each market's RMS move, so those are in units of the market's own `move_rms_millibps`, which the profile carries per market (its recorded 1-s standard deviation over `sqrt(1 − no-move share)`). |
| [d034] | Gap, reading taken | **Markets:** Polymarket's bands (`1 / Lmax`) break band rule 1 (D-015), so the flow uses `400,000 / Lmax` ppm (both rules pass: rule 1's product 8,029,980 to 8,400,000, `min_price` at least 2,173), half to twice the start price, and the M3 flow's fee classes (5x and 3x markets take the 10x class's). Maker leverage `min(5, Lmax)`. The ladder's reach is half the band; a quote whose drawn gap would pass it goes to the deepest free price within it. |
| [d034] | Gap, reading taken | **Maker events:** re-price or resize is chosen by the running split of maker messages (a re-price while cancels and places are under 73.2%), since a fixed chance drifted to 76% (a spread change can move more than the two best quotes); about 54% of events are re-prices. A re-price at rank 0 is a spread change: both best quotes move to one fresh spread draw, so the book's spread is always one draw of the profile's model. |
| [d034] | Gap, reading taken; the owner to confirm | **Shocks:** every 10 s of flow time by default (the recorded rate is one per 347 s: this is a stress setting); the movers' times within 52 ms (the recorded median); the Pareto read as conditioned on at least 14 markets, everything beyond 88 counted as 88 (median 22 markets, 7.1% of shocks move all 88); the calibrated size uniform between the recorded quantiles, `[4, 5.09)` 50%, `[5.09, 6.53)` 40%, and a last stretch as wide as the one before, `[6.53, 7.97)` 10% (this flow's assumption); each calibrated mover keeping the shared direction with 95% chance. The stress preset: 2% to 6%, all one way; 8 cascade accounts per market ($1M each, IOCs of $5,000 at the maximum leverage), re-entering half-way between shocks. |
| [d034] | Gap, reading taken; the owner to confirm | **The rest of the flow's own choices:** one account per taker cluster; the high-leverage cohort at 2 per market, IOCs of $2,000, an add every 100 ms; the fund at $1,000 (so cascades past bankruptcy show as shortfall); no operator withdrawals in this flow; `--bursts` allowed with the M3 flow too (it changes only the schedule; refusing it would be one line); the durable limit always from the M3 flow's pre-verified 20k/s runs, so a Polymarket-only session runs those three first. From the profile: tradfi macro's class spread is one real tick (SP500, NAS100 and GOLD are tick-bound; SILVER, WTIOIL and BRENTOIL have their own); the cluster sizes are the recorded ones (1 to 54, some tail sizes at 0 ppm), not smoothed; the takers' share is 695 ppm from the clean window (`flow.json` says 697). |
| [d034] | Gap, open (the owner's call) | **Liquidations in the default flow almost never happen.** At the maximum leverage a fresh position is liquidated by about `0.5 / Lmax` against it (1% at 50x, 2.5% at 20x); the recorded jumps are 0.5% to 1.23%, so only a 50x market's jump of about 0.95% or more liquidates (2 of the 16 sizes, and 4 of the 88 markets are 50x). Verified only with `jump_one_in` = 1 (6 liquidations in 20 s of flow). As on Polymarket; the stress shock is the liquidation switch. Liquidations in the default flow would need a decision, e.g. entries at the band's edge or aged positions. |
| [d034] | Evidence | **The generator, locally** (default seed, applied to a real engine in plan order): no reject in setup A, B1 or B2; 0.08% of timed client commands rejected, all `UnknownOrder` (quotes takers had filled). Over 10 s of flow: maker messages 36.60% / 36.60% / 13.28% / 13.25% (adds, removes, size ups, size downs; recorded 36.6 / 36.6 / 13.4 / 13.4), levels 52.6% / 27.2% / 20.1% (52 / 27.7 / 20.3); spreads within 12% of the profile tables' 10th, 50th and 90th percentiles; depth per quote at the median within 1% to 2% of the recorded books in every class, at the 90th percentile within 5% except tradfi equities; dust shares as recorded; the no-move share within 1.5 points in every leverage class; 100,234 client messages per flow-second; the stress shock every 2 s over 10 s: 199 liquidations, all caused by marks, up to 5 in one command; `makers_k` = 3: 35%, 36% and 29% of maker messages; bursts: per-hour p90 and p99 of the multipliers within 12% of the calibration's, the maximum within 20%. Memory: the engine holds 249 MB after setup A and its prefault, against 231 MB for the M3 flow. |
| [d034] | Evidence | **The harness, locally** (release, WSL2, unpinned, a shared machine: every run invalid as generator-limited, so behaviour, not performance): `e2e run --smoke --flow polymarket-smoke` (signed): 30 liquidations, 2 shortfall reports, replay identical, audit clean. Pre-verified at 50k/s: 0.030% engine rejects; the top 10 markets 23.6% of messages (recorded 23%), the takers' top 10 58.9% (recorded 53%); 1.20 fills per IOC (recorded: 1.35 prints per taker order). Stress shock at 100k/s: 35 liquidations, 0.040% rejects, the longest `SetMark` core service 192 µs, at most 52 events from one command, no hot-thread fault. Signed at 15k/s with 3 makers and bursts: the busiest maker sent 35.8% of messages. `e2e sweep --kind polymarket` at 20k/s with 1 s windows: all 9 points ran and the report gave one headline and breakdown per variant. A bursty 100k/s run stalled while another process on the machine used about 280% CPU. Every Polymarket smoke and local run: 0 hot-thread allocations and 0 faults, about 2 events per command (the capture keeps 6), at most 52 from one command. |
| [d034] | Evidence, open | **Departures from the recorded shape:** tradfi equities' depth per quote at the 90th percentile is $201k against $159k recorded (+26%); a 10-s sample's taker share was 660 ppm against 695 (the start rate is exact; the gap is sampling noise); resizes that draw the size a quote already had are still sent (0.27% of maker messages); the band cuts Polymarket's far tail, so thin crypto and equity books stack a few deep quotes at the reach. The Polymarket smoke flow's engine reject share is 11% to 26% (cancels of quotes the shocks' band sweeps or the takers removed), flagged and not asserted, as for the M3 smoke runs; the full flow's, locally, 0.03% to 0.04%. |
| [d034] | Gap, open | A plan ends at a client-item count, so its last event can be cut in the middle (as in the M3 flow); the tests check the books at marks, not at a plan's end. |
| [d034] | Tests | `loadgen`: 91 unit tests (66 before the generator: 13 new for the profile; then 22 for the flow and 3 for bursts). `bench`: 22 new (23 in the feature build). The whole workspace in the documentation step: 30 binaries, 666 tests in the default build and 675 with `c-secp256k1`, all passing; the feature build's first run failed one unchanged `pipeline` test, `journal::writer::tests::a_pass_that_only_flushes_counts_only_itself_as_busy` (23,978,800 ns busy against its 10 ms bound), and passed in full when run again: not investigated. Clippy with `-D warnings` and `fmt` clean in both builds; `Cargo.lock` unchanged. |

### Review of the Polymarket-shaped flow (2026-09-30)

Twenty findings from reviewers of the uncommitted D-034 build, each checked against the
code before acting; 14.12, D-034 ("Corrected after review") and the code's docs say what
changed. Three pairs were one finding each: [makers-k-idle] and [digest-profile] were
reported twice, and [shock-breadth] once as fidelity and once as documentation. The M3 flow, its plan, digest and
pinned tests are untouched; `Cargo.lock` is unchanged and no crate was added. The
calibration was re-extracted (`extract`, 2 min 40 s on 6 processes: only `book_shapes` and
`shocks` changed) and regenerated (`generate --check` identical after `./dev fmt`).
`FLOW_VERSION` is 2.

| ID | Verdict | What changed |
|---|---|---|
| [depth-by-level] (1) | Confirmed, fixed | Every level drew from its class's pooled sizes, so level 1 held a median of $4k to $50k against $1k to $3k recorded. The scan (`scan.py`) now keeps sizes per class and level; `derive.book_shape` gives each level 1 to 20 its dust share, clip shares and a 64-value background of the sizes recorded there (`LevelSizes`); `draw_size` picks by rank. `Clip`, `Background` and `dust_by_level_ppm` are gone; `clips_usd` and `levels` replace them. Tests: realised level 1, 3, 5, 10 and 20 medians against the profile's (within 25%, 40% for the majors' and macro's few markets), the profile's level 1 and 3 against book.md section 4. Fills per IOC in a pre-verified 50k/s run: 1.36 (recorded 1.35 prints per taker order), 1.20 before. |
| [ladder-reach] (2) | Confirmed, fixed as far as independent gaps allow | Gaps are now fitted on the sides that show all 20 levels (the thin sides' stale far orders fed the tails), and every new gap is drawn among its bucket's gaps that fit before the reach (`next_price`, `draw_gap_within`), so a ladder no longer stacks one real tick apart at the reach; pulls after a move re-place behind the last quote the same way. Level 20's median distance went from the reach (3 to 10 times the recorded) to 1.1 to 2.2 times the recorded; levels 1, 5 and 10 at 0.7 to 1.6 times. Left, stated: the recorded 20-level sides are either compact or sparse (half of the equities' have no gap over 10 ticks after level 10, against 15% if independent), which independent gaps can't reproduce, so level 20 still sits further out at the median. Test: `levels_sit_about_as_far_from_the_mid_as_recorded`. |
| [gaps-drift] (3) | Confirmed, fixed | A re-price, a spread change and a move each left one gap a remainder, and the ladder's gaps drifted from the profile's (the top 1-tick share fell from about 60% to 40–53%). Now a re-price draws the moved quote's two gaps together given their sum, and a spread change the spread with the two gaps behind the best quotes (Gibbs steps, `draw_split`, `spread_change`: they keep the profile's law exactly), with a fresh draw only where no split fits; and after a move of the fair value the ladders follow it whole (`follow_move`: 40 cancels, then 40 places, each quote with its maker and size). Every maker message now counts on the message clock, so a flow-second still holds 100,000 client messages (99,997 measured) and the maker mix and levels stay as recorded (36.60 / 36.60 / 13.16 / 13.21%; 52.3 / 27.3 / 20.4%). Measured shares of 1-tick and over-10-tick gaps: within 4 points of the profile's in the three large classes (a few points fewer over 10 ticks after level 10, drawn within reach), within 7 points for the majors' and macro's 3 and 6 markets. A consequence, stated: the spread and gaps are exact in law but each market's relax slowly (only a ladder's last gap is ever drawn outright), so a 10-s sample holds few independent states per market; the spread test's tolerance is 20% (over 2M events of one market its quantiles come within 10%). Test: `gaps_between_levels_keep_the_profiles_shares`. |
| [iocs-share] (4) | Confirmed, fixed | The high-leverage adds came on top of the takers' 695 ppm (795 in all), in markets picked uniformly. `high_leverage_every_ns` became `high_leverage_ppm` (100), taken out of the IOCs' 695: takers 595 ppm; each add picks a market by taker weight, then one of its two accounts; the period is `10^15 / (R × ppm)` ns, so the smoke flow's 5,000 messages a flow-second get one add every 2 s, not 10 a second. The check refuses 695 ppm or more. The test counts both. |
| [shock-breadth] (5, 14) | Confirmed, fixed as a reading for the owner | Conditioning the Pareto on at least 14 gave a median of 22, a 90th percentile of 69 and 7.1% at 88, against 14, 31 and a maximum of 71 recorded. D-034's "at least 14, at most all 88" is now read as a clamp of the recorded Pareto (above 8): median 14, 90th percentile 38, 4.1% beyond 71, 3.1% all 88 (`derive.shocks`, its docstring giving both readings' numbers; 14.12; D-034). The stress unit scenario's liquidations fell from 199 to 84; the smoke flow's shocks move 21 markets on average. |
| [post-only-reorder] (6) | Confirmed, documented | True through the pipeline: the sequencer orders only within an account (9.1), and a spread change, a ladder following a move or a requote cancels one maker's quote and places another's at or through its price. Preventing it would mean putting a market's crossing cancel and place under one account, which changes the flow's maker structure, so `market.rs`'s invariant, `polymarket.rs`'s "Open loop", 14.12 and D-034 now say so, with the review's measurement. The plan-order engine test is unchanged. |
| [makers-k-idle] (7, 12) | Confirmed, fixed | Ranks run out at 20 a side, so makers from 22 on never quoted. `check` now allows `makers_k` of 1 to 20 and `makers_per_market` of 1 to 20, with no more groups than markets; the bid of rank `r` in market `i` goes to maker `(r + i) mod n` (the ask `(r + 1 + i) mod n`), so the busiest quotes turn over the makers (with `--makers 3` each sends 30% to 37%). The capacity test's largest config is 1,000 makers in 50 groups of 20; `e2e.rs`, `flow.rs` and 14.12 say 1 to 20. |
| [cox-length] (8) | Confirmed, fixed | The multipliers were normalised over `ceil(n / R) + 1` whole seconds, so the timed flow ran out of items before the window closed for 44% of seeds at the headline's shape with the median preset (32% busiest, 21% at the smoke run's shape; checked again here over 2,000 seeds). `CoxRate::new` now divides them so that their integral over `[0, n / R)` is exactly `n / R`. Tests: the integral at the headline's shape for 20 seeds of each preset, and the smoke shape's last send within 100 ms of 1.5 s for seeds 1 to 8. Release runs at 2k/s with the reviewer's seeds 2, 3, 5 and 8: every flow ended after the window, the sender's faults known. |
| [makers-per-gateway] (9) | Confirmed, fixed | 12 makers put two on gateways 1 and 2 of 10. The default is now 60 makers in 20 groups of 3 (still 3 per market), 60 being a multiple of 1 to 6, 10, 12, 15, 20 and 30 gateways, and the markets are dealt to the groups heaviest first by maker weight, so the groups carry equal shares within 4% (`market_makers`). Measured: each maker 1.5% to 1.9% of maker messages; at 10 gateways, lanes within 5% of the mean in the plan and 9.5% to 10.3% each in a release run (17.4% and 15.8% on two lanes before). D-034's "12 accounts" is corrected there, for the owner. Test: `the_default_makers_spread_evenly_over_the_gateways`. |
| [migration-edge] (10) | Confirmed, fixed | `Watched::sample` takes an `Edge`: at the window's open it reads `/proc/vmstat` before the threads' faults, at the close after them, so a migration whose fault counts inside the window counts inside it too. Not done: the optional guard after the close for a migration batch straddling it (the kernel counts a batch once it is done; a window of microseconds), stated in `checks.rs`. |
| [digest-profile] (11, 19) | Confirmed, fixed | `generate` writes the profile JSON's SHA-256 into the table (`Profile::sha256`), `PolymarketConfig::digest` hashes it next to the name, and a cargo test checks it against the committed JSON (`include_bytes!`), so a regenerated table changes the digest without a manual `FLOW_VERSION` bump; the README's manual-bump rule is gone. |
| [burst-doc] (13) | Confirmed, fixed | `BurstModel`'s doc now says the schedule normalises over each phase and `exp(V / 2)` is not used. |
| [move-units] (15) | Confirmed, fixed | `Shocks`, 14.12's profile table, the `real` preset's text, D-034 and the profile test's comment now give shock and mixture sizes in units of the market's RMS of nonzero 1-s moves (`move_rms_millibps`, the detector's unit), not its 1-s standard deviation or bps. |
| [jump-rule] (16) | Confirmed, fixed | `Jumps`, `derive.py` and the README: a 1-s move over 50 bps whose move from the second before to 5 s after is still over 25 bps (half the threshold). |
| [jump-50x] (17) | Confirmed, documented | The 50x markets had no persistent jump over 50 bps (the 20x 0.011 per market-hour), yet the pooled rate applies to them and is the default flow's only liquidation path. `polymarket.rs`, 14.12 and D-034 now say the pooled rate is a simplification, the rare liquidations its artefact, and per-class rates (0 at 50x) the owner's call; the flow is unchanged. |
| [min-notional] (18) | Confirmed, fixed | `Profile::min_notional` is now applied: every quote size and every IOC is raised to $10 before its lots are rounded up (every table already starts above it, so the plan is unchanged by this alone). "For the reader and the report" is now "for the reader" in `profile.rs` and `derive.py`; the per-class background's mean and sd are no longer carried. |
| [flow-cite] (20) | Confirmed, fixed | `flow.rs` cites 14.1 to 14.9 for the M3 flow and 14.12 for the Polymarket flow. |
| [pm-review] | Evidence | Plan-order, default seed, 10 s of flow: 0.13% of timed client commands rejected, all `UnknownOrder` (0.08% before: thinner top levels, so B2's IOCs and the takers fill more quotes). Release runs (unpinned, shared machine, generator-limited, so not benchmark numbers): pre-verified 50k/s over 20 s, 0.049% rejects, the top 10 markets 23.0% of messages (recorded 23%), the takers' top 10 55.7% (53%), 1.36 fills per IOC, no fault; the stress shock at 100k/s over 25 s, 34 liquidations, 0.070% rejects, `SetMark` core service at most 166 µs, at most 57 events from one command, no fault. |
| [pm-review] | Tests | `loadgen`: 97 unit tests (91 before): the profile's `the_table_names_the_sha_256_of_the_committed_json`; the flow's `depth_by_level_is_the_profiles`, `gaps_between_levels_keep_the_profiles_shares`, `levels_sit_about_as_far_from_the_mid_as_recorded` and `the_default_makers_spread_evenly_over_the_gateways`; the schedule's `a_cox_phase_offers_exactly_its_length_and_ends_on_time`. Changed to the new behaviour: the profile's book-shape and shock tests, the flow's pinned plan and digest (new values), setup, takers, `makers_k`, `check` and spread tests, `bench`'s `makers_k` refusals and the capacity test (1,000 makers in groups of 20). The whole workspace: 672 tests in the default build and 681 with `c-secp256k1`, all passing; clippy with `-D warnings` and `fmt` clean in both; `Cargo.lock` unchanged; the M3 flow's files, plan, digest and pinned tests unchanged. |
