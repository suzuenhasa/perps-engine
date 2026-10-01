# Benchmarks log

Every measured number, appended as each milestone runs. The one-page summary for readers
is `REPORT.md` (written in Milestone 4). This file is the evidence behind it and behind
the performance claims in `docs/DECISIONS.md`.

**Rules (INFO.md sections 8 and 10).**
- Every number states its layer, the machine, the environment, and the commit it was
  measured at.
- End-to-end numbers come from an open-loop load generator.
- Report the maximum rate at a latency limit, not just peak throughput.
- No cherry-picked runs: if a run is discarded, say why.

## Machines

| Name | CPU | Threads available | RAM | Environment | Notes |
|---|---|---|---|---|---|
| local | AMD Ryzen 7 2700X (8C/16T, Zen+) | 8 (WSL2 limit) | 23 GB | Docker 29.8 on WSL2 (kernel 6.18), `perps-dev:1.98.1` image | Frequency scaling on, no core isolation. `./dev` pins the recorder containers to CPU 7 and builds and benchmarks to CPUs 0-6 |
| PERPSBOX | Xeon E5-2680 v4 (14C/28T, Broadwell) | ~26.9 (cgroup quota) | 62 GB | vast.ai container, native build | Rented for headline runs (M3, M4); re-probe on each rental |

## M0: book-layer baseline (2026-09-29, commit `454caae`)

**What was measured.** `./dev cargo bench -p bench --locked --offline`
(`bench/benches/book.rs`): the time to apply one fixed batch of 10,000 commands from
`loadgen::SyntheticFlow` (default `FlowConfig`: seed 1, one market, 100 accounts, prices
uniform in mid ± 20 ticks, qty 1-10, 40% cancels) to a fresh book, through the
`OrderBook` trait into a counting sink. Book layer only: no signatures, sequencer, journal
or margin. Criterion defaults (3 s warm-up, 100 samples); release profile.

**Machine.** local (above), in the `dev` container on CPUs 0-6. The recorders were running
on CPU 7, and the history puller was re-fetching mark history there during the run.

| Book | Time per 10,000 commands (95% CI) | Throughput |
|---|---|---|
| `ReferenceBook` (linear scan, the spec) | 3.79-3.93 ms (estimate 3.85 ms) | 2.60 M commands/s |
| `Book` (M0 stub: delegates to the reference) | 3.71-3.84 ms (estimate 3.76 ms) | 2.66 M commands/s |

The two rows run the same code, so their 2% difference is noise between runs; treat both
as one baseline of about 2.6 M commands/s.

**What the batch contains** (printed by the bench): 5,986 acks, 4,128 fills, 992 cancels
(user and self-trade), 3,064 rejects. Every place in this flow is valid, so all the
rejects are cancels of orders that had already filled. The generator is open-loop and
doesn't see fills, as real flow wouldn't.

**Why this number flatters the reference book.** The flow keeps the book shallow: measured
on this flow during M0, on average 228 resting orders (438 at most) and 13 price levels
per side. A linear scan is cheap at that depth. Milestone 1 adds a deep-book flow (thousands
of resting orders, as in exchange-core's benchmarks), where the reference's cost per
command should grow with depth and the fast book's should not.

**Not comparable to later numbers yet.** No core isolation, frequency scaling on, and
Docker on WSL2. The headline runs (M3, M4) happen on PERPSBOX.

**Recorder load during the run** (context for the machine, not a benchmark): about 5.8 MB
of JSONL a minute, 8 GiB a day raw and about 2 GiB gzipped. The full 15:00 UTC hour was
347 MB raw and 82 MB gzipped (4.2:1). See D-007.

## M1: fast book vs reference book (2026-09-29, commit `f279dc3`)

**What was measured.** `./dev cargo bench -p bench --locked --offline`
(`bench/benches/book.rs`), book layer only: 10,000 commands per batch through the
`OrderBook` trait into a counting sink. Two flows (D-012):
- `book/synthetic_flow`: the M0 shallow flow on a fresh book, unchanged since M0.
- `book/deep_flow`: 100,000 warm-up commands (not timed) build a book of about 9,100
  resting orders; each iteration then applies the next 10,000 commands to a clone of it.

Both books get identical batches and emit identical events (checked by the property tests,
D-009). Criterion defaults, except the deep reference: 10 samples, flat sampling, 5 s.

**Machine.** local, `dev` container on CPUs 0-6, recorders on CPU 7. The REST puller was
catching up after a network outage during the run.

| Flow | Book | Time per 10,000 commands (95% CI, estimate) | Throughput | Per command |
|---|---|---|---|---|
| shallow | `ReferenceBook` | 4.21-4.24 ms (4.22 ms) | 2.37 M/s | 422 ns |
| shallow | `Book` | 430-434 µs (432 µs) | **23.2 M/s** | 43 ns |
| deep | `ReferenceBook` | 245-251 ms (248 ms) | 40 K/s | 24.8 µs |
| deep | `Book` | 608-612 µs (610 µs) | **16.4 M/s** | 61 ns |

The fast book is 9.8 times faster on the shallow flow and about 406 times faster on the
deep one. This meets the M1 definition of done, "millions of ops/s for the book alone".

**Batch contents** (printed by the bench, identical for both books):
- shallow: 5,986 acks, 4,128 fills, 992 cancels, 3,064 rejects (all unknown order:
  cancels of orders that had filled), as in M0.
- deep: 4,583 acks, 543 fills, 3,972 cancels, 924 modifies, 529 rejects (5.3%: 505
  unknown order, 24 post-only would cross). Depth before the batch: bids 4,530 orders on
  125 levels, asks 4,566 on 116; after: 4,570 on 126 and 4,537 on 112.

**Run-to-run variation.** The shallow reference measured 3.85 ms at M0, 3.59 ms and
4.23 ms in two agent runs today, and 4.22 ms here, on unchanged code. So differences
under about 15% between runs on this machine are noise, not effects.

**Caveats.**
- The deep book costs about 40% more per command than the shallow one, although none of its
  steps depend on depth; likely cache effects from a larger working set (D-010). Not
  profiled.
- A cloned `Vec` keeps its length, not its spare capacity. Each clone's slab is exactly the
  warm-up peak (9,150 slots) and the batch peaks at 9,121, so no timed iteration grows it.
  If the flow changes so a batch exceeds the warm-up peak, timed iterations would include a
  slab reallocation.
- The mid never moves, so no command sweeps many levels.
- Docker on WSL2, no core isolation, frequency scaling on. Not comparable with PERPSBOX
  numbers.

## M2: risk layer, and ablations A and B (2026-09-29, commit `a013f90`)

**What was measured.** `./dev cargo bench -p bench --locked --offline --bench book --bench
ablation_a --bench ablation_b --bench risk_cost` (7.5 minutes). Engine layer: book plus
risk, no signatures, sequencer or journal. Scenario construction and what each timed
command does are printed by the benches and described in their module docs
(`bench/benches/*.rs`, `bench/src/risk.rs`); every timed command is checked to have done
what it should (top-up, re-key, liquidation count), and no engine is ever cloned.

**Machine.** local, `dev` container on CPUs 0-6, recorders on CPU 7. The host was quiet:
load average 0.8 at the start and 1.2 at the end. (An earlier agent run during heavy
unrelated load, 5 to 15, was discarded for absolute numbers.)

### The risk check (M2 target: about 1 µs)

A margin-checked place that rests without filling (top-up, then the account's liquidation
key moves), and its cancel (release, key moves back). `Fast` = running open totals and the
liquidation index. Estimates; the 95% intervals are within about ±5% except where noted.

| Positions in the market (`n`) | Resting orders per account (`k`) | `Fast` place | `Fast` cancel | Naive place (loops over the account's orders) | Naive cancel |
|---|---|---|---|---|---|
| 100 | 1 | 372 ns | 186 ns | 399 ns | 193 ns |
| 100 | 16 | 381 ns | 186 ns | 486 ns | 247 ns |
| 100 | 256 | 345 ns | 193 ns | 1.98 µs | 979 ns |
| 100 | 4,096 | 306 ns | 169 ns | 68.1 µs | 27.1 µs |
| 10,000 | 1 | 390 ns (see note) | 179 ns | 422 ns | 197 ns |
| 10,000 | 16 | 460 ns | 205 ns | 546 ns | 241 ns |
| 10,000 | 256 | 420 ns | 214 ns | 2.05 µs | 992 ns |
| 10,000 | 4,096 | 371 ns | 178 ns | 34.9 µs | 21.8 µs |
| 1,000,000 | 1 | 391 ns | 189 ns | 412 ns | 194 ns |
| 1,000,000 | 16 | 404 ns | 184 ns | 514 ns | 240 ns |
| 1,000,000 | 256 | 389 ns | 188 ns | 1.89 µs | 951 ns |
| 1,000,000 | 4,096 | 339 ns | 185 ns | 35.3 µs | 22.1 µs |

- **The check stays well under 1 µs**: 0.3-0.46 µs for a risk-checked place, including the
  book, the top-up, the release and the re-key, with up to a million positions in the market
  and 4,096 resting orders per account.
- **Ablation A:** `Fast` is flat in `k`; the naive loop grows linearly with it, to about
  100 times slower at 4,096 orders per account. The re-key's `log n` is not visible from
  100 to 1,000,000 positions: it is lost in the noise of the rest of the command.
- Note: the full run measured the `n = 10,000, k = 1` place at 962 ns with a wide interval
  (795 ns to 1.15 µs), against about 390-460 ns for its neighbours. Re-run alone straight
  afterwards, the same cell measured 382-398 ns (390 ns), which is the value in the table.
  Both runs are recorded here; the first is treated as noise.

### Ablation B: liquidation index vs full rescan on `SetMark`

Time per `SetMark` (timing an empty stretch of code costs 26 ns, included once in each
liquidating `SetMark`), and per place and cancel in the same books.

| Positions (`n`) | Liquidations | Index (`Fast`) | Full rescan (`NaiveLiquidation`) |
|---|---|---|---|
| 1,000 | 0 | 29 ns (reads 2 index entries) | 10.2 µs (reads 1,000 slots) |
| 1,000 | 1 | 236 ns | 10.4 µs |
| 1,000 | 100 | 22.3 µs | 25.0 µs |
| 100,000 | 0 | 28 ns | 6.4 ms |
| 100,000 | 1 | 347 ns | 6.7 ms |
| 100,000 | 100 | 36.1 µs | 6.6 ms |
| 1,000,000 | 0 | 27 ns | 113 ms |
| 1,000,000 | 1 | 317 ns | 115 ms |
| 1,000,000 | 100 | 31.7 µs | 111 ms |

| Positions (`n`) | Place: index / rescan mode | Cancel: index / rescan mode |
|---|---|---|
| 1,000 | 219 / 168 ns | 159 / 119 ns |
| 100,000 | 230 / 175 ns | 173 / 120 ns |
| 1,000,000 | 213 / 164 ns | 177 / 117 ns |

- **The trade, both ways:** the index costs about 45-55 ns on every place and 40-55 ns on
  every cancel (the re-key). The rescan costs O(n) on every mark update: 113 ms at a million
  positions, whether or not anything is liquidated. At 100,000 orders a second the index's
  share is about 0.5% of a core; a rescan at a million positions every 200 ms would be more
  than half of one.
- A liquidation through the index costs about 0.2-0.36 µs each (cancelling the account's
  orders, moving the position to the fund, events). The 4-ary heap makes this about 2.4
  times the `BTreeSet`'s cost, in exchange for no allocation and cheaper orders (D-018).

### The risk layer's cost per command

The M1 deep flow (about 9,100 resting orders, 1,000 accounts) through the book alone and
through `Engine<Book, Fast>` (every place and replacing modify margin-checked with a
top-up, fees on every fill, releases and re-keys in the post-command pass, every ledger
event emitted). Mark fixed at the mid; nothing rejected by risk or liquidated (checked).

| Path | Per command |
|---|---|
| Book alone | 63 ns (62.3-63.6) |
| Engine (book + risk) | 273 ns (261-287) |

The risk layer adds about 210 ns per command, 4.3 times the book alone, or about 3.7 M
commands a second through the whole engine on one core.

### Books, unchanged since M1

`book/synthetic_flow`: reference 3.41 ms, book 436 µs per 10,000 commands. `book/deep_flow`:
reference 226 ms, book 603 µs. Within run-to-run variation of the M1 numbers.

### Caveats

- These are means per command (criterion), not percentiles; the p99 of a risk-checked
  place is measured in Milestone 3 with HdrHistogram.
- Ablation B's scenario puts the liquidated positions at the top of the index on purpose;
  the index walk reads only them, the rescan reads everything.
- Docker on WSL2, no core isolation, frequency scaling on. Headline numbers come from
  PERPSBOX.
