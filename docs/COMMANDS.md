# Command book

Every command this repo has, what each one does, and when to use it. Written for someone
who has just cloned the repo and has never seen it.

Each command sits in its own block, ready to copy and paste. Below each block, a few short
lines say what it does, when to use it, how long it takes, what you will see and where the
results go. Words in *italics* are explained in the [glossary](#13-glossary) at the end.

Run every command from the repo folder (the one that holds `./dev`).

## Contents

1. [Quick start](#1-quick-start): from a fresh clone to a live run, in 6 commands.
2. [Setting up](#2-setting-up): Docker, `./dev`, why `cargo` on your machine is blocked.
3. [Checking it works](#3-checking-it-works): tests, smoke runs, clippy, formatting.
4. [Seeing it in action](#4-seeing-it-in-action): live runs with a panel, and how to pick `--rate`.
5. [Reading the output](#5-reading-the-output): the panel, the result, the report, the verdict.
6. [Benchmarks](#6-benchmarks): probe, sweeps, searches, ablations, the big-machine session.
7. [Correctness checks](#7-correctness-checks): replay, signature audit, crash tests.
8. [Option reference](#8-option-reference): every `e2e` option in one table.
9. [Supply-chain and code-quality checks](#9-supply-chain-and-code-quality-checks): audit, deny, vet and more.
10. [Market data tools](#10-market-data-tools): the Polymarket recorder, history puller, calibration.
11. [Where files go and cleaning up](#11-where-files-go-and-cleaning-up)
12. [Troubleshooting](#12-troubleshooting)
13. [Glossary](#13-glossary)

---

## 1. Quick start

Six commands, in this order. You need Docker with Compose v2 and about 2 GB of disk.

```sh
./dev image
```
Builds the container image everything runs in. First time only, about 10 minutes.

```sh
./dev --net cargo fetch --locked
```
Downloads the exact dependency versions listed in `Cargo.lock`. Compiles nothing.
This is one of the few commands that uses the network.

```sh
./dev cargo test --workspace --locked --offline
```
Builds everything and runs every test, with no network. All tests should pass
(678 at the time of writing).

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --smoke
```
A *smoke run*: 3,000 signed orders through every stage of the pipeline, then a replay of
the *journal* and a re-check of every signature. The orders flow for 0.6 s; the first time,
compiling in release mode comes first. Prints a report.
To run it a second time, see ["already holds a journal"](#already-holds-a-journal).

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 30s --gateways 2 --watch --capture --audit
```
The live run: 10,000 signed orders a second for 30 s, drawn as a panel that updates ten
times a second. Needs a terminal at least 120 columns wide and 30 lines tall.
About 35 s of orders, plus signing them first, plus the replay and audit afterwards.

```sh
./dev cat /target/runs/session/runs/signed-10k/report.md
```
Shows the full report of that live run. Section [5](#5-reading-the-output) explains it.

On a laptop the verdict will usually say **invalid**. That is expected; see
[section 4.9](#49-what-the-verdict-means-on-a-laptop).

---

## 2. Setting up

### What you need

- **Docker with Compose v2.** Check it with:
  ```sh
  docker compose version
  ```
- **About 2 GB of free disk** for the image and the build. Kept journals and recordings
  take more (see [section 11](#11-where-files-go-and-cleaning-up)).
- **Nothing else.** No Rust on your machine. Python 3 is used only by the optional
  calibration tool ([section 10](#10-market-data-tools)).

### Why `cargo` on your machine is blocked

Running `cargo build` or `cargo test` directly fails on purpose, with:

```
perps: refusing to run rustc on the host; build inside ./dev (INFO.md 5a)
```

This is a supply-chain safety rule. A Rust build runs code from dependencies (build
scripts and macros). Inside `./dev` that code runs with the source read-only, `.git`
hidden and no network, so it cannot phone home or change the source. On your machine it
could.

How the block works: `.cargo/config.toml` sends every compiler call through
`tools/host-guard/`, which refuses unless `PERPS_IN_CONTAINER=1` is set. The image sets
it. The guard only applies to `cargo` started inside this folder.

Your editor is covered too: `.vscode/settings.json` stops rust-analyzer from building on
your machine.

### What `./dev` is

`./dev` is a small script that runs a command inside a Docker container. Each call starts
a fresh container and removes it afterwards. Only the Docker *volumes* keep data between
calls. Files the containers write belong to you, not to root.

The mode you pick decides what the container may do:

| You type | Network | What it may write | Use it for |
|---|---|---|---|
| `./dev <command>` | none | only `/target` (build output and runs) | build, test, run, bench, clippy, and reading files in `/target` |
| `./dev fmt` | none | the source files | formatting the code |
| `./dev --net <command>` | yes | the download cache, the advisory database | `cargo fetch`, `cargo audit`, `cargo deny`, `cargo tree`, `cargo metadata`, the two supply-chain scripts |
| `./dev --write <command>` | yes | `Cargo.lock` and the download cache, or only `supply-chain/` | `cargo generate-lockfile`, `cargo update`, `cargo vet` |
| `./dev image` | yes | the Docker image | building the image |
| `./dev record ...` | yes (`record build`: none) | `./data` (`record build`: the recorder's own build volume) | the market-data recorder ([section 10](#10-market-data-tools)) |
| `./dev` or `./dev --help` | – | – | prints the script's help |

The real protection comes from each container's mounts and network settings in
`compose.yaml`. The lists in `./dev` only catch mistakes.

```sh
./dev --help
```
Prints the short help above, from the script itself.

### The first image build

```sh
./dev image
```
- **What it does:** builds `perps-dev:1.98.1` from a pinned Rust base image. It adds a few
  Debian packages (`ca-certificates curl jq gzip git`), clippy, rustfmt, and the three
  supply-chain tools (cargo-audit, cargo-deny, cargo-vet).
- **When:** once, and again only if `docker/dev.Dockerfile` changes.
- **How long:** about 10 minutes the first time.

```sh
./dev --net cargo fetch --locked
```
- **What it does:** downloads every crate in `Cargo.lock` into the `perps_cargo-home`
  volume. Compiles nothing.
- **When:** once after cloning, and after `Cargo.lock` changes. Every later build uses
  `--offline` and reads from this cache.

### CPUs used by `./dev`

`./dev` builds and runs on all CPUs but the last one (CPUs 0-6 on an 8-CPU machine). The
last CPU is kept for the market-data recorder. To choose yourself, set `DEV_CPUS`:

```sh
DEV_CPUS=0-3 ./dev cargo test --workspace --locked --offline
```
Runs the tests on CPUs 0 to 3 only. Applies only to plain `./dev` commands.

Environment variables on your machine do **not** reach the container (the one exception:
`ALLOW` and `MIN_DAYS` for the age check in [section 9](#9-supply-chain-and-code-quality-checks)).
To pass one, put `env` in front of cargo:

```sh
./dev env PROPTEST_CASES=10000 cargo test -p engine --release --locked --offline --test engine_equivalence
```

---

## 3. Checking it works

All of these run in the default container, with no network.

The flags you will see on every cargo command:
- `--locked`: use exactly the versions in `Cargo.lock`; fail rather than change it.
- `--offline`: never try the network (the container has none anyway).
- `--release`: an optimised build. Needed for real speed numbers; tests run without it.
- `-p <crate>`: only this *crate*. The crates are `engine`, `pipeline`, `gateway`,
  `loadgen`, `bench` and `recorder`.

### Tests

```sh
./dev cargo test --workspace --locked --offline
```
- **What it does:** builds and runs every test in every crate.
- **When:** after cloning, and before trusting any change.
- **What you'll see:** cargo's test output, one block per test file, each ending in
  `test result: ok`. 678 tests at the time of writing.

```sh
./dev cargo test -p engine --locked --offline
```
- **What it does:** the tests of one crate only (here the engine). Swap in any crate name.

```sh
./dev cargo test -p pipeline --locked --offline --test crash_recovery
```
- **What it does:** one test file only: `pipeline/tests/crash_recovery.rs`. The name is the
  file name without `.rs`. The test files are listed in [section 7](#crash-tests-in-the-test-suite).

```sh
./dev cargo test -p gateway --lib --locked --offline eip712
```
- **What it does:** only the tests whose name contains `eip712`, among the gateway's unit
  tests: 43 of them, the gateway's EIP-712 checks and the golden vectors taken from
  Polymarket's own SDK tests. Ends in `43 passed; 0 failed; ... 87 filtered out`.
- **When:** any time you want one test or a family of tests: put part of its name last.

```sh
./dev cargo test -p gateway -p bench --features c-secp256k1 --locked --offline
```
- **What it does:** the tests again, in the build that adds the C library *libsecp256k1*
  as a second signature verifier. Signed tests then run with both verifiers.
- **When:** before using `--verifier libsecp256k1`. It runs the gateway's and the bench's
  tests only: 244 at the time of writing.

### The smoke test (inside the test suite)

```sh
./dev cargo test --release --locked --offline -p bench --test smoke
```
- **What it does:** runs the whole pipeline on small flows, "in seconds": signed (2
  gateways, 3,000 messages at 5k/s), *pre-verified* (30,000 commands at 20k/s), EIP-712, and
  the Polymarket smoke flow. It checks that the counts add up, that at least one
  liquidation and one insurance shortfall happen, and that hot threads make no memory
  allocations and no *page faults*.

### The smoke run (the real program, small)

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --smoke
```
- **What it does:** one small run of the real benchmark program `e2e`: 3,000 signed orders
  at 5k/s through every stage. Then the *replay test* and the *signature audit*.
- **How long:** 0.6 s of orders, plus setup, signing, replay and audit. Seconds.
- **What you'll see:** the run's report on screen.
- **Where:** `/target/runs/session/runs/signed-5k/`. The journal is kept there.
- **Again?** A second smoke run into the same folder is refused, because the first one kept
  its journal. Add `--name smoke2` (any new name), or see
  ["already holds a journal"](#already-holds-a-journal).

Three more smoke runs:

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --smoke --mode preverified
```
Pre-verified: 30,000 unsigned commands at 20k/s, straight into the pipeline, no
signature checks. 1.5 s of commands. Folder `preverified-20k`.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --smoke --flow polymarket-smoke
```
The *Polymarket-shaped flow*, scaled down, with a stress price shock every 250 ms.
Folder `signed-5k-polymarket-shock-stress`.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --smoke --auth eip712
```
Orders signed the way Polymarket Perps signs them (*EIP-712*). Folder `signed-5k-eip712`.

### Lint (clippy)

The docs say clippy runs with `-D warnings` (every warning is an error) in both builds.
The exact command is not written in the repo; these two do that:

```sh
./dev cargo clippy --workspace --all-targets --locked --offline -- -D warnings
```
Checks every crate, tests and benchmarks included, for common mistakes. Should print no
warnings.

```sh
./dev cargo clippy -p gateway -p bench --all-targets --features c-secp256k1 --locked --offline -- -D warnings
```
The same for the libsecp256k1 build.

### Formatting

```sh
./dev fmt --check
```
Checks the formatting and changes nothing. Prints the differences, if any.

```sh
./dev fmt
```
Reformats the source files in place (rules in `rustfmt.toml`: lines up to 110 characters).
This is the only `./dev` mode that may write to the source.

### Longer tests (optional)

```sh
./dev cargo test -p pipeline --release --locked --offline --test ring_stress
```
The full ring stress test: 10 million records per slot size in release (1 million in a
normal build). Checks that nothing is lost, repeated, reordered or torn between threads.

```sh
./dev env PROPTEST_CASES=10000 cargo test -p engine --release --locked --offline --test engine_equivalence
```
The long engine property run: 10,000 random scenarios instead of 256. About 3 minutes.

```sh
./dev env CRASH_RECOVERY_SCALE=100 cargo test -p pipeline --release --locked --offline --test crash_recovery
```
The crash-recovery soak: 100 times the normal count (30,000 runs). About 11 s in release.

---

## 4. Seeing it in action

### 4.1 The long command, and a shortcut

Every run of the benchmark program starts with the same long prefix:

```
./dev cargo run --release --locked --offline -p bench --bin e2e -- <subcommand> [options]
```

- `-p bench --bin e2e`: the program to build and run.
- `--`: everything after it goes to `e2e`, not to cargo.
- `<subcommand>`: `run`, `probe`, `sweep`, `search`, `ablate`, `replay`, `recover` or `report`.

This book always writes the full form, so every block can be pasted as is. If you prefer,
define two aliases in your shell (they work from the repo folder):

```sh
alias e2e='./dev cargo run --release --locked --offline -p bench --bin e2e --'
alias e2e-c='./dev cargo run --release --locked --offline -p bench --features c-secp256k1 --bin e2e --'
```
Then `e2e run --smoke` is the same as the long smoke command. `e2e-c` is the libsecp256k1
build.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- help
```
Prints the list of subcommands. `--help` anywhere does the same, so `run --help` prints
the usage too instead of starting a run.

### 4.2 Check your terminal size

The live panel needs at least 120 columns and 30 lines.

```sh
echo "$(tput cols) x $(tput lines)"
```
Prints your terminal's size. If it is smaller, make the window bigger or the font smaller.

### 4.3 How to pick `--rate`

`--rate` is the **offered load**: how many orders per second the sender sends. It is not a
cap and not a target the program tries to reach. The sender works *open loop*: it sends on
schedule whether or not the pipeline keeps up. What actually got through is the
**achieved** rate.

So pick a rate the machine can carry, or pick one it can't on purpose to watch overload.

**Rule of thumb for signed orders.** Checking signatures is the expensive step, and each
*gateway* thread checks them in parallel.
- With *k256* (the default verifier): about 10k orders/s per gateway on an older 8-core
  machine. With `--gateways 2`, that is about 20k/s at most.
- With *libsecp256k1*: roughly twice that, so about 40k/s with 2 gateways.
- On a laptop the sender, the other threads and everything else on the machine share the
  CPUs, so stay well below these. The examples use half: 10k/s with k256, 20k/s with
  libsecp256k1.

**Pre-verified runs** skip the signature checks, so they go much higher (200k/s below).

**To measure your own machine,** run the quick probe and look at the verifier's scaling
curve (checks per second for 1, 2, 4… threads):

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- probe --quick
```
About 10 s. Look for the lines `scaling.k256.<i>.threads` and `scaling.k256.<i>.per_second`.

**What overload looks like:**
- In the panel, `signatures verified` and `durable and acknowledged` fall behind your rate,
  and the gateways' bar sits near 100%. Once the gateways' input queues are full,
  `orders sent` falls too: the sender drops what doesn't fit (counted as "dropped at
  ingress") and never waits.
- In the result, `achieved` is below the rate you offered, and the p99 latency is huge or
  `∞`. (A command that was never served counts as infinitely late, so p99 shows `∞` once
  more than 1% were lost.)
- In the report, "dropped at ingress" is above 0, sequenced is under 99.9%, and the
  *limited by* label names the thread that ran out, for example `limited by gateway 1`.
- On the big machine, the signed 500k and 1M/s points are overload tests on purpose: 80% to
  90% of orders are dropped.

To see overload on a laptop, offer about twice the rule-of-thumb limit:

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 40k --window 10s --gateways 2 --watch
```
k256, 2 gateways, 40k/s offered. About 15 s of orders. Folder `signed-40k`.

### 4.4 The first live run: M3 flow, k256, 10k/s

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 30s --gateways 2 --watch --capture --audit
```
- **What it does:** sends 10,000 signed orders a second on the *M3 flow* (67 synthetic
  markets). Two gateways check every signature with k256. Each order is sequenced, written
  to the journal on disk, matched by the core, and acknowledged only once it is on disk.
  Afterwards it replays the journal (`--capture`) and re-checks every signature (`--audit`).
- **How long:** 5 s *warm-up* + 30 s measured *window* + 0.2 s *tail* = about 35 s of
  orders. Before that it signs about 352,000 messages (it prints how long that took);
  after that come the replay and the audit.
- **What you'll see:** the live panel (explained in [section 5.1](#51-the-live-panel)),
  then a short RESULT block, then the line
  `e2e: the report is <dir>/report.md; the recording <dir>/watch.jsonl`.
- **Where:** `/target/runs/session/runs/signed-10k/`: `report.md`, `summary.txt`,
  `watch.jsonl` (the panel, recorded), `events.bin` (the captured events),
  `snapshot-live.txt` (the engine's final state), `keys.txt`.

Running it again replaces that folder's report. Nothing is lost elsewhere.

### 4.5 The Polymarket-shaped flow

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 30s --gateways 2 --flow polymarket --watch
```
- **What it does:** the same pipeline, but the orders follow the *Polymarket-shaped flow*:
  Polymarket Perps' 88 real markets, with the traffic shape of about 22 hours of their
  recorded public data. 60 market makers, 1,000 takers.
- **How long:** about 35 s of orders, plus signing.
- **Where:** `/target/runs/session/runs/signed-10k-polymarket/`.

### 4.6 The libsecp256k1 verifier

```sh
./dev cargo run --release --locked --offline -p bench --features c-secp256k1 --bin e2e -- run --mode signed --rate 20k --window 30s --gateways 2 --verifier libsecp256k1 --watch --name signed-20k-libsecp256k1
```
- **What it does:** the same live run, with Bitcoin Core's C library *libsecp256k1*
  checking signatures instead of k256. Offered at 20k/s, because it is roughly twice as fast.
- **The build:** `--features c-secp256k1` compiles the C library's 4 source files the first
  time. The image already has the C compiler. Without this feature, `--verifier libsecp256k1`
  is refused at once.
- **Why `--name`:** the verifier is not part of the run's folder name, so without `--name`
  this run would share a folder with a k256 run at 20k/s.
- **What you'll see:** the panel header says `libsecp256k1 signatures`.
- **Where:** `/target/runs/session/runs/signed-20k-libsecp256k1/`.

### 4.7 Pre-verified at 200k/s

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode preverified --rate 200k --window 30s --watch
```
- **What it does:** sends 200,000 unsigned commands a second straight into 2 lanes. No
  gateways and no signature checks. Everything after that is the same: sequenced, written
  to disk, matched, released only once durable. This shows the core and the journal
  without the cost of signatures.
- **How long:** about 35 s of commands. Nothing to sign, so it starts quickly.
- **What you'll see:** the panel has no `signatures verified` row, and the header says
  `pre-verified commands`.
- **Where:** `/target/runs/session/runs/preverified-200k/`.

### 4.8 The stress switches

Three switches make the flow harder. `--makers` and `--shock` work only with
`--flow polymarket`; `--bursts` works with either flow.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 100s --gateways 2 --flow polymarket --shock stress --watch
```
- **What it does:** every 10 s of flow time, 14 to 88 markets jump the same way by 2% to 6%,
  and a group of high-leverage accounts is liquidated together.
- **Why a 100 s window:** one second of flow time holds about 100,000 orders, so at 10k/s
  the flow's clock runs at a tenth of real time and the first shock comes about 100 s
  after the orders start. With a 30 s window no shock lands at all, and the report flags
  `the flow has shocks (--shock), but none fell in the window`.
- **How long:** about 2 minutes: it signs about a million messages first (15 s here), then
  105 s of orders.
- **What to watch:** the `liquidations` row jumps when the shock lands (a test run of this
  command: 15 liquidations in the window).
- **Where:** `signed-10k-polymarket-shock-stress/`.
- `--shock real` uses the recorded move sizes instead (about 11 basis points).

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 30s --gateways 2 --bursts median --watch
```
- **What it does:** the send rate swings second by second like Polymarket's median recorded
  hour, around the same average. 1% of seconds run at 2.4 times the average or more. Only
  send times change, not the orders.
- **What to watch:** `orders sent` per second moves up and down. Peaks above what the
  gateways can check show up as queueing and longer latency.
- **Where:** `signed-10k-bursts-median/`. `--bursts busiest` uses the busiest hour instead.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 30s --gateways 2 --flow polymarket --makers 3 --watch
```
- **What it does:** only 3 market-making accounts do all the quoting. Each account always
  uses the same gateway (account number mod gateway count), so a few gateways carry most
  of the traffic.
- **What to watch:** the `gateways` row's note `busiest X%`. One gateway near 100% while
  the others have room is the per-account ceiling.
- **Where:** `signed-10k-polymarket-makers3/`. `--makers` takes 1 to 20.

All three together (a 100 s window again, so a shock lands):

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 100s --gateways 2 --flow polymarket --makers 3 --bursts median --shock stress --watch
```
Folder `signed-10k-polymarket-makers3-bursts-median-shock-stress/`.

### 4.9 What the verdict means on a laptop

The harness refuses to count a run it cannot trust, and says so in the verdict. On a laptop
or VM the verdict will usually be **invalid**. The usual reason is:

```
generator-limited: sender lag p99 X is above 5 µs (14.10)
```

That means the sender (the load generator, our measuring tool) ran late on a shared CPU.
It is about the tool, not the pipeline. The run still worked: the panel, the counts, the
replay and the audit are all real. Only its latency numbers don't count as a result.

- `invalid` does not mean broken. The command still exits with status 0.
- Some reasons do mean a bug: counts that don't add up, a replay that differs, an audit
  failure. See [section 5.4](#54-verdict-flags-and-limited-by).
- Local numbers are not comparable with numbers from the big machine.
- For local development only, `--allow-generator-limited` turns a late sender into a flag
  instead of an invalid verdict. The report says so. Never use it for real results.

---

## 5. Reading the output

### Where output goes, and exit status

- Progress messages go to standard error (the screen).
- The report goes to standard output. So you can save it straight to a file on your machine:
  ```sh
  ./dev cargo run --release --locked --offline -p bench --bin e2e -- run --smoke --name smoke-saved > smoke-report.md
  ```
  Saves the smoke run's report as `smoke-report.md` in the current folder.
  Don't do this with `--watch` (see [the panel prints one line a second](#the-panel-prints-one-line-a-second)).
- Exit status 0: the command finished, even if the run was invalid.
- Exit status 2: it could not finish. The reason is printed as `e2e: <message>`.

### 5.1 The live panel

The panel is redrawn every 100 ms. Every "per second" figure and every busy share compares
the latest frame with the one 1 s earlier. "Total" counts from the start of the run,
setup included.

**Header**

| Line | Meaning |
|---|---|
| `PERPS ENGINE  live run on <CPU>` | The machine's CPU model. |
| `<rate>/s of signed orders (<auth>, <verifier> signatures)` | The offered rate, the signing scheme (`perp` or `eip712`) and the verifier (`k256` or `libsecp256k1`). Pre-verified runs say `pre-verified commands`. |
| `· <flow> flow ·` | `m3` or `polymarket` (a smoke flow shows the name of the flow it is scaled from). |
| `journal on disk, group commit every 1ms` | The journal is real. `journal discarded` means nothing reached the disk. |

**Stage strip and progress bar.** Four stages: `setup  warm-up  window  drain`. Done stages
get a ✓; the current one a ▶. Only the **window** is measured.
- setup: `funding accounts, seeding the books` (no known length, so the bar stays empty);
- warm-up: `warm-up X s of 5.0 s`;
- window: `measured X s of 30.0 s`;
- drain: `window closed: draining`.

**PIPELINE** (per second, total, note)

| Row | Meaning |
|---|---|
| `orders sent` | What the sender put into the gateways' (or lanes') input queues. Orders dropped because a queue was full are not counted here. Note: `open loop, Poisson arrivals` (or bursty, or evenly spaced). |
| `signatures verified` | Signed runs only. Messages that passed every gateway check, all gateways together. Rejects are recorded in `watch.jsonl` but not drawn. |
| `durable and acknowledged` | Commands the *gate* released: matched, on disk, acknowledged. Includes the operator's mark-price commands, so it can sit slightly above the order rate. |
| `journal flushes` | *Group-commit* flushes, with the average *fdatasync* time. With the default *T* of 1 ms, at most about 1,000 a second. |

**WHAT THE ENGINE COMPUTED** (counted from released events)

| Row | Meaning |
|---|---|
| `fills` | Trades, with dollars traded per second. |
| `cancels`, `modifies` | Cancels and modifies. |
| `mark prices` | *Mark price* updates. Each one re-checks margin. |
| `liquidations` | Accounts liquidated. |
| `events released` | Every event released: acks, fills, cancels, rejects, liquidations and so on. |

**THREADS** (share of one core, busy). A bar and a percentage per thread: the time it
spent doing work (not waiting for work) divided by wall time over the last second.

| Row | Meaning |
|---|---|
| `sender` | The load generator. |
| `gateways ×N` | The gateways' average, with `busiest X%` in the note. |
| `sequencer` | The one thread that puts every command in a single order. |
| `core` | The one thread doing margin checks and matching. |
| `journal` | The journal writer's CPU work, `plus X% waiting on the disk`. |
| `gate` | Releases only what is durable. |

The footer shows the path one order takes:
`sign → gateway (decode, nonce, verify) → sequencer → journal + core (margin, match) → fdatasync, about once a millisecond → gate → acknowledged`.

### 5.2 The RESULT block

Printed after a `--watch` run. It covers the measured window only.

| Line | Meaning |
|---|---|
| `achieved  N orders/s` | Orders scheduled inside the window that got sequenced, divided by the window length. |
| `order → durable ack  p50 · p99 · p99.9` | Time from each order's *scheduled* send time to its release by the gate. Counted over offered orders, so a lost one counts as `∞`. |
| `fills, liquidations  X and Y` | Counts inside the window. |
| `journal replayed  identical` | Only with `--capture`. `identical` is the good answer. |
| `signatures re-checked  N, M failures` | Only with `--audit`. Failures must be 0. |
| `verdict  valid` / `invalid: <reasons>` | See [5.4](#54-verdict-flags-and-limited-by). |

### 5.3 A run's `report.md`

Every run writes `report.md` in its folder. Without `--watch` it is also printed. To read
one later:

```sh
./dev cat /target/runs/session/runs/signed-10k/report.md
```

The sections, in order, and what good looks like:

| Section | What it holds | Good looks like |
|---|---|---|
| Run `<name>` | Mode, rate, arrivals, timing, gateways, journal (`T`, `B`), auth, verifier, flow, seed, commit, build, clock, file system, CPU layout | build `release`, clock `tsc`, commit not `unknown` (locally the commit is always `unknown`: `./dev` hides `.git`) |
| Verdict | `valid` or `INVALID (<reasons>)`, then flags | valid, flags `-` |
| Rates in the window | offered, sequenced (%), **achieved/s**, released/s, dropped, gateway rejects, engine reject share | sequenced ≥ 99.9%, dropped 0, rejects low (flagged above 5%) |
| Latency table | Count, p50, p99, p99.9, max for each stage: sender lag, ingress wait, signature verification, sequencer wait, core path, core service, command → core result, durability wait, order → durable ack | sender lag p99 ≤ 5 µs. The stages' p99s don't add up: the p99 of a sum is not the sum of p99s |
| Operator commands | Mark-price commands and their core cost | a long one means a mark that liquidated many orders |
| Per gateway | Offered, busy, ingress wait, verification time per gateway | even load across gateways |
| Journal | flushes, records, bytes, batch sizes, **fdatasync p50 / p99 / p99.9** | segments created by the writer: 0 |
| Commands by type and outcome | sequenced, accepted, rejected by reason; drops; fills, cancels; liquidations; the insurance fund's drawdown and shortfall | the gate's final fund equity equals the snapshot's |
| The flow's shape | busiest account and its gateway, IOCs per second, concentration across markets | – |
| Thread health | the *limited by* label; busy share and page faults per thread; throttling; clock inversions | faults 0, throttled 0, inversions 0 |
| Replay test, signature audit | the results of `--capture` and `--audit` | identical; 0 failures |

The Polymarket smoke flow (`--flow polymarket-smoke`) rejects 11% to 26% of orders on
purpose, so its reject flag is expected. The M3 smoke flow rejects about 3%, below the flag.

### 5.4 Verdict, flags and "limited by"

**INVALID reasons** (the run doesn't count):

| Reason | What it means | On a laptop |
|---|---|---|
| `generator-limited: sender lag p99 X is above 5 µs` | The sender ran late; the measuring tool was the limit | expected |
| `CFS throttling: N periods, M µs` | The container used more CPU than its quota and the kernel paused it | possible |
| `N clock inversions` | A later timestamp was smaller than an earlier one | not expected |
| `the sender didn't finish: …` | The plan did not complete, e.g. a setup step timed out after 10 s | investigate |
| `the engine rejected setup commands …` | The books or accounts are not what the flow intended | investigate |
| `the gateways refused the harness's own EIP-712 messages …` | EIP-712 messages went stale; shorten `--window` | possible with `--auth eip712` |
| Count mismatches (`offered X != sent …` and similar) | The counts must add up | a bug |
| `the gate's fund equity differs from the engine snapshot's` | A consistency check failed | a bug |
| `the replay differs: …` | Determinism failed | a bug |
| `the signature audit found N failures` | A bad signature got into the journal | a bug |

**Flags** are printed with the run but don't make it invalid. The common ones:
- `N minor page faults on hot threads in the window` (with the kernel's page migrations, if
  any; on WSL2 these are common right after a build);
- `engine rejects X% … (above 5%)`;
- `clock … doesn't count toward a headline`;
- `durable numbers mean nothing here: …` (the folder is on a RAM file system);
- `journal discarded`;
- `the flow has shocks (--shock), but none fell in the window`;
- `the backlog didn't drain within the cap: a fail`.

**"Limited by" labels** say what stopped the run from going faster. Checked in this order:

| Label | Meaning |
|---|---|
| `limited by the disk (fdatasync X% of the window)` | The journal fell behind and the writer spent its time waiting on the disk. The disk's flush speed is the limit. |
| `limited by journal writer` | The journal fell behind because of the writer's own CPU work. |
| `limited by gate` | The gate (the releasing thread) was at least 90% busy and held things up. |
| `core-limited` | The single engine thread could not keep up. |
| `limited by <thread>` | That thread was at least 90% busy, e.g. `limited by gateway 1`: signature checking on that gateway is the ceiling. |
| `not saturated (busiest: X at Y%)` | Nothing was at its limit. Normal below capacity. |

---

## 6. Benchmarks

### Why benchmarks need a dedicated big machine

The real numbers come from a dedicated machine (the runbook's PERPSBOX), not from a laptop. The
harness pins every thread to its own CPU and checks that nothing disturbed the measurement.
That needs:
- at least 14 physical cores (gateways = physical cores − 4, so 10 on a 14-core box);
- one CPU package (or NUMA balancing off);
- at least 60 GB of RAM and 50 GB of disk;
- **local NVMe**, so that `fdatasync` is real (p99 ≤ 5 ms);
- a fast, reliable clock (`tsc`), no CPU throttling, and a sender that keeps its schedule
  (lag p99 ≤ 5 µs).

A laptop shares its CPUs with everything else, and WSL2's virtual disk may absorb flushes.
The full procedure is in [`RUNBOOK-PERPSBOX.md`](RUNBOOK-PERPSBOX.md). The headline
numbers are at the top of [`README.md`](../README.md); [`BENCHMARKS.md`](BENCHMARKS.md)
holds the microbenchmark numbers (M0 to M2).

On the big machine there is no Docker: the runbook builds natively and calls the binary as
`$E` (`./target/release/e2e`). Locally, use the long `./dev` form.

### Sessions

A *session* is one folder (`--dir`, default `/target/runs/session`) that collects the probe,
every run and one combined `report.md`, rebuilt after every command.
- Sweeps, searches and ablations can be resumed: a run that already finished is not run
  again, and a run that died is started over.
- Pass the **same options to every command** of a session. A run with different settings
  under the same name is refused.
- Never pass `--window` to a search.
- Use a separate `--dir` per verifier.

### The commands

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- probe
```
- **Measures** the machine: CPU, cores, memory, CPU quota; the clock and its cost; the
  file system; signature-check cost and how it scales over 1, 2, 4… cores; per-core
  jitter (to pick the quietest cores); and 30 s of disk flushes (`fdatasync`).
- **When:** first, in every session. Later commands use its results.
- **How long:** about 2 minutes. `--quick`: about 10 s.
- **Where:** `<session>/probe/summary.txt`, and printed as `key = value` lines.
- Refuses a RAM file system (tmpfs, ramfs) with exit status 2.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run
```
- **Measures** one run (section 4). With no options: signed, 100k/s, 30 s window.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- sweep --kind load
```
- **Measures** a set of points, each run 3 times, interleaved (A B C, A B C, A B C). It
  prints its plan and a time estimate before signing anything.
- The kinds:

| `--kind` | What it measures | Runs | On PERPSBOX |
|---|---|---|---|
| `load` | Both modes at `--rates` (default `20k,100k,500k,1M`) | 24 | ~16 min |
| `headline` | The main number: signed at `--rate` (default 100k) with a 60 s window: 3 timing runs, a capture run (replay and audit), a second seed | 5 | ~15 min |
| `commit-interval` | Pre-verified 100k/s with the group-commit interval *T* at 0, 250 µs, 500 µs, 1 ms, 2 ms | 15 | ~10 min |
| `stamps` | The cost of measuring: timestamps on against off. `--rate` is required | 6 | a few min |
| `polymarket` | Three headlines on the Polymarket flow: as it is, with `--bursts median`, with `--shock stress` | 15 | ~22 min |

- **Where:** `<session>/sweep-load/`, `sweep-commit-interval/`, `sweep-stamps/`, `headline/`.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- search --name core-path
```
- **Measures** the highest offered rate at which a stage's p99 stays under a limit while at
  least 99.9% of orders are sequenced. It doubles the rate from 100k/s until a run fails,
  narrows in three times, confirms both edges 3 times, then runs once at twice the result
  to see what saturates.
- The three searches:

| `--name` | Mode | Stage | Limit |
|---|---|---|---|
| `core-path` | pre-verified, journal discarded | the core's work per command | 50 µs |
| `journal` | pre-verified, real disk | order → durable ack | the *durable limit* |
| `signed` | signed, real disk | order → durable ack | the durable limit |

- **How long:** about 14 runs of 27 s each. On PERPSBOX the three take ~25 min together.
- **Where:** `<session>/search-<name>/search.txt` (plus a flow suffix such as `-polymarket`).
- `--limit` sets another limit; `--start` another first rate.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- ablate verify-on-core --search
```
- **Measures** what parallel signature checks on the gateways buy, against checking on the
  core thread (an insecure setup that exists only for this). Signed, at 5k to 100k/s.
- **How long:** about 35 min on PERPSBOX with `--search`.
- **Where:** `<session>/ablate-verify-on-core/`.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- ablate fsync-per-order --search
```
- **Measures** what group commit saves, against one disk flush per order. Pre-verified, real
  disk, at 1k to 100k/s, three ways of flushing.
- **How long:** about 65 min on PERPSBOX with `--search`.
- **Where:** `<session>/ablate-fsync-per-order/`.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- report
```
- **Rebuilds** the session's `report.md` from the run summaries and prints it. Immediate.
- It covers the machine, the load sweep, the searches, the headline (yes / no / not decided
  yet), the `T` sweep, the ablations, the cost of measurement and every invalid run.
- Repeated runs show the median and the range, over valid runs only.
- Single `e2e run` runs are not in its tables; read their own `report.md`.

### Trying a small session locally

The results won't count, but you can see how a session works:

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- probe --quick --dir /target/runs/local
```
About 10 s. Measures this machine.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- sweep --kind load --rates 5k,10k --window 5s --dir /target/runs/local
```
12 runs (both modes × 2 rates × 3) of about 12 s each, so about 2.5 minutes plus signing.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- report --dir /target/runs/local
```
Prints the session report.

### The full big-machine session, in order

Follow [`RUNBOOK-PERPSBOX.md`](RUNBOOK-PERPSBOX.md): it has every command ready to paste.
In short, with `O="--dir $S --gateways $G"` on every command:

1. **Locally:** both test builds, `python3 tools/calibrate/calibrate.py generate --check`,
   commit everything, make a git bundle.
2. **Rent** a box to the spec above.
3. **Check the box:** a busy core runs at full clock; NUMA is one node or balancing is off.
   Otherwise rent another.
4. **Build:** pinned toolchain, `cargo fetch`, build `e2e`, run the smoke test.
5. **Probe** and decide: disk OK (fdatasync p99 ≤ 5 ms), clock `tsc`, one package. Set
   `G = physical cores − 4`.
6. **Pass 1, M3 flow, k256 (about 3 h):** `sweep --kind load` (~16 min),
   `sweep --kind headline` (~15 min), `search --name core-path`, `journal`, `signed`
   (~25 min together), `sweep --kind commit-interval` (~10 min), `sweep --kind stamps`
   (a few min), `ablate verify-on-core --search` (~35 min),
   `ablate fsync-per-order --search` (~65 min), `report`.
7. **Pass 2, libsecp256k1 (about 35 min):** the feature build, probe, signed search, headline,
   report, in a second session folder.
8. **Passes 3 and 4, EIP-712 (about 40 min each):** headline and signed search with
   `--auth eip712`, once per verifier.
9. **Passes 5 and 6, Polymarket flow (about 50 and 45 min):** `sweep --kind polymarket`,
   `search --name signed --flow polymarket`, `search --name core-path --flow polymarket`
   (k256 pass only), `search --name signed --flow polymarket --makers 3`, report.
10. **Wrap up:** copy the reports back (journals stay on the box), paste them into
    `BENCHMARKS.md`, then **destroy** the instance (a stopped one still bills for its disk).

Budget: about 7 hours of box time with no reruns. Never use `--allow-generator-limited`
there, and never `--resume` in the EIP-712 passes.

### Microbenchmarks

These time single pieces in isolation with the *criterion* library. They run fine locally
(the M0 to M2 numbers in `BENCHMARKS.md` are local), but on the local machine differences
under about 15% are run-to-run noise.

```sh
./dev cargo bench -p bench --locked --offline
```
Runs all five microbenchmarks. Results print in the terminal.

```sh
./dev cargo bench -p bench --locked --offline --bench book
```
One benchmark by name. The five:

| `--bench` | Measures | How long |
|---|---|---|
| `book` | The order book: the reference (the specification) against the fast production book | deep flow about 2 min |
| `ablation_a` | The O(1) pre-trade margin check against a naive one that walks every order | about 4 min |
| `ablation_b` | The liquidation index against a full rescan on each mark price | about 3 min |
| `risk_cost` | The cost per command of the risk layer on top of the book | about 20 s |
| `pipeline_parts` | Per-call costs: signing and verifying, EIP-712, keccak, the salt table, encoding, CRC32C, the ring, the histogram | not documented |

```sh
./dev cargo bench -p bench --locked --offline --bench pipeline_parts -- keccak
```
Only the groups whose name contains `keccak`.

```sh
./dev cargo bench -p bench --locked --offline --features c-secp256k1 --bench pipeline_parts
```
Adds the libsecp256k1 group.

---

## 7. Correctness checks

These answer "did the pipeline do the right thing?", not "how fast?". They work the same
on a laptop.

### The replay test (`--capture`)

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 10s --gateways 2 --capture
```
- **What it does:** during the run, the gate writes every event it releases to `events.bin`.
  After the run, the journal is read back from disk into a fresh engine. The rebuilt events
  and final state are compared with the live run's, again with a different internal hash
  seed.
- **What `identical` proves:** the journal alone rebuilds exactly what the live run
  released, event for event, and the same final state. The engine is deterministic.
- **Result:** `journal replayed identical` in the RESULT block; "Replay test" in the report.

### The signature audit (`--audit`)

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 10s --gateways 2 --audit
```
- **What it does:** after a signed run, re-checks every signature in the journal, in the
  journal's own scheme. It also checks that each order came from the account's owner and
  that nonces only go up.
- **What `0 failures` proves:** nothing unsigned or wrongly signed got into the journal.
- Signed runs only.

### Replay and recover a kept journal

Normally the journal is deleted after a run (to save disk). `--keep-journal` keeps it.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --rate 10k --window 10s --capture --keep-journal --name det
```
A 10k/s signed run whose journal stays in `/target/runs/session/runs/det/journal/`. About
15 s of orders.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- replay --run-dir /target/runs/session/runs/det
```
- **What it does:** reads the kept journal, recovers it, replays it through a fresh engine,
  writes `events-replay.bin` and `snapshot.txt`, and compares them with the live run.
- **What you'll see:** `key = value` lines such as `replay.records = …`,
  `replay.records_per_s = …`, `replay.events_vs_live = identical: …` and
  `replay.state_vs_live = identical`.
- Exit status 2 with `the replay differs from the live run` if either comparison differs.

```sh
./dev cargo run --release --locked --offline -p bench --bin e2e -- recover --run-dir /target/runs/session/runs/det
```
- **What it does:** crash recovery only. It finds where the journal ends and checks that
  anything after that is just a half-written tail. If there is one, it copies it to
  `torn-<segment>-<offset>.bin`, zeroes it and syncs. Otherwise it changes nothing.
- **What you'll see:** `recovered.records = …`, `recovered.next_seq = …`,
  `recovered.segments = …` and so on.

```sh
./dev rm -rf /target/runs/session/runs/det
```
Deletes that run and its journal when you're done. Needed before running `det` again.

`--smoke` and `--release-log` also keep the journal. A journal written by a build with
different engine rules is refused unless you add `--allow-engine-change`.

### Crash tests in the test suite

```sh
./dev cargo test -p bench --locked --offline --test crash
```
The kill test: starts `e2e`, kills it twice at random moments and restarts it each time
(4 lives in all), then checks that what was released is a prefix of the replay, and that
resume, audit and `e2e replay` all say identical. Its seed is printed at the start.

```sh
./dev cargo test -p pipeline --locked --offline --test crash_recovery
```
Crash recovery on a simulated disk: power loss, a crash inside a write or a sync, a failed
sync. 300 runs of 6 lives. Everything up to the last durable point must survive, and
nothing from an earlier life may come back.

The other test files, all run by the workspace test command:

| File | What it checks |
|---|---|
| `engine/tests/book_equivalence.rs` | The fast book produces exactly the same events as the reference book, over hundreds of random scenarios |
| `engine/tests/engine_equivalence.rs` | 5 engine modes agree event for event; an independent checker and a shadow ledger check the money |
| `engine/tests/no_alloc.rs` | No memory allocations after warm-up, for the book and the engine |
| `pipeline/tests/engine_semantics.rs` | A checksum of the event stream for 100,000 commands; any behaviour change fails it until the version is bumped on purpose |
| `pipeline/tests/journal_regressions.rs` | Journal recovery, torn-tail copies, restarting over a crash-left folder |
| `pipeline/tests/pipeline_e2e.rs` | The whole pipeline in one process: gating, lossless shutdown, replay equality, restart |
| `pipeline/tests/ring_stress.rs` | Nothing lost, repeated, reordered or torn between threads |
| `gateway/tests/signed_pipeline.rs` | Real gateways in front of the real pipeline: rejects for the right reasons, nonces after restart, the audit |
| `gateway/tests/eip712_pipeline.rs` | The same for EIP-712: replay, too old, too early, wrong key, wrong scheme |
| `gateway/tests/eip712_no_alloc.rs` | EIP-712 checks make no allocations |
| `bench/tests/smoke.rs` | The whole pipeline on the smoke flows |
| `bench/tests/crash.rs` | The kill test above |
| `*_regressions.rs` | Named regressions found earlier |

What these don't prove: the report says "Durable as reported by <file system>; not
power-loss tested". A real power cut on real hardware is not tested.

---

## 8. Option reference

Options for `e2e`. Any word starting with `--` is an option. If you give one twice, the
last one wins (except `--cpus`, which repeats).

### Units

| Kind | How to write it | Examples |
|---|---|---|
| Rate | a number, optionally with `k` (thousand) or `M` (million); above 0 | `2500`, `20k`, `1.5M` |
| List of rates | rates separated by commas | `5k,10k,20k` |
| Duration | a number with `ns`, `us` (or `µs`), `ms`, `s`, or `m`/`min` (minutes); or `0` | `250us`, `1ms`, `30s`, `2m` |
| Bytes | a whole number, optionally with `K` (KiB), `M` (MiB) or `G` (GiB); lowercase `m` is not accepted | `4096`, `1M`, `1G` |

### Where runs go

| Option | Values | Default | Meaning | Notes |
|---|---|---|---|---|
| `--dir` | path | `/target/runs/session` | The session folder | Keep it under `/target`; RAM file systems are refused |
| `--run-dir` | path | – | `run`: run right here, outside any session. `replay`/`recover`: the run to read | Required for `replay` and `recover` |
| `--name` | text | `run`: made from the options | `run`: the run's folder name. `search`: which search | `search`: `core-path`, `journal` or `signed` |

### What is sent

| Option | Values | Default | Meaning | Notes |
|---|---|---|---|---|
| `--mode` | `signed`, `preverified` | `signed` for `run` | Signed: through gateways that check signatures. Pre-verified: straight in, no signatures | Sweeps and searches set the mode per point |
| `--rate` | rate | `run`: 100k (smoke: 5k signed, 20k pre-verified) | Offered orders per second. Not a cap | Required for `sweep --kind stamps`; ignored by load sweep, search and ablate |
| `--rates` | list | load: `20k,100k,500k,1M`; verify-on-core: `5k,10k,20k,40k,100k`; fsync-per-order: `1k,2k,5k,10k,20k,50k,100k` | The rates of a load sweep or an ablation | – |
| `--flow` | `m3`, `polymarket`, `smoke`, `polymarket-smoke` | `m3` | Which order flow | Smoke flows are scaled down |
| `--makers` | 1–20 | off (60 makers) | Only K accounts quote, so their gateways carry most traffic | `--flow polymarket` only |
| `--bursts` | `median`, `busiest` | off | The send rate swings like Polymarket's median or busiest hour | Not with `--arrivals uniform` |
| `--shock` | `real`, `stress` | off | Every 10 s of flow time (100 s of real time at 10k/s, [4.8](#48-the-stress-switches)), 14–88 markets move together; `stress` also liquidates a high-leverage group | `--flow polymarket` only |
| `--seed` | number | 1 | Changes every random stream, the keys and the engine's hash seed | – |
| `--arrivals` | `poisson`, `uniform` | `poisson` | Random or evenly spaced send times | `uniform` is for debugging only |
| `--auth` | `perp`, `eip712` | `perp` | The signing scheme. `eip712` is Polymarket Perps' | Signed runs only; warm-up + window + tail at most 3 min; no `--resume` |
| `--verifier` | `k256`, `libsecp256k1` | `k256` | Which library checks signatures | `libsecp256k1` needs the `--features c-secp256k1` build. Not in the run's name: use `--name` or a separate `--dir` |
| `--timed-clients` | number | rate × (warm-up + window + tail) | A fixed number of timed orders | – |
| `--smoke` | flag | off | The small smoke run: 3,000 signed messages at 5k/s (or 30,000 pre-verified at 20k/s), capture on, audit on if signed, journal kept, unpinned | Other options still override it, wherever they stand (capture, audit and the kept journal can't be turned off) |

### Timing

| Option | Values | Default | Meaning | Notes |
|---|---|---|---|---|
| `--warmup` | duration | `5s` | Sent but not measured | – |
| `--window` | duration | `30s` (search 20 s; headline twice the window) | The measured period | Above 0. Never pass it to a search |
| `--tail` | duration | `200ms` | The flow keeps going after the window, so the window's last orders are released under load | – |

### Threads and CPUs

| Option | Values | Default | Meaning | Notes |
|---|---|---|---|---|
| `--gateways` | number ≥ 1 | 2 | Gateway threads (signed) or lanes (pre-verified) | The big machine uses 10 |
| `--unpinned` | flag | off | Don't pin threads to CPUs | Use it if pinning is refused on a small machine |
| `--gateway-smt` | flag | off | Let gateways also use the idle hyperthread siblings of their cores | – |
| `--cpus` | `role=list`, repeatable | automatic layout | Pin by hand. Roles `main`, `sender`, `core`, `sequencer`, `journal`, `gate` take one CPU each; `gateways=<list>` one per gateway | CPUs must be allowed and on one package |
| `--idle` | `spin`, `yield` | `spin` (smoke: `yield`) | Idle threads busy-poll, or spin 64 times then yield | – |

### Journal

| Option | Values | Default | Meaning | Notes |
|---|---|---|---|---|
| `--journal` | `disk`, `discard` | `disk` | `discard` writes nothing to disk | `discard` refuses `--capture`, `--resume` and the durable searches |
| `--commit-interval` | duration | `1ms` | *T*: the longest a record waits before a flush | – |
| `--max-batch` | 1–4096 | 4096 | *B*: the most records per flush | – |
| `--segment-bytes` | bytes | `1G` (smoke: `1M`) | Size of one journal file | – |
| `--deployment` | number | 12451843 (0x00BE0003) | An id written into the journal | – |
| `--keep-journal` | flag | off (smoke: on) | Keep `journal/` after the run, for `replay`, `recover` and `--resume` | Takes disk |
| `--resume` | flag | off | Restart on the journal already in the folder: recover, replay, send the rest | Same options as the first run; `perp` scheme only |
| `--allow-engine-change` | flag | off | Accept a journal written with different engine rules | – |

### Measuring and checking

| Option | Values | Default | Meaning | Notes |
|---|---|---|---|---|
| `--watch` | flag | off | The live panel, recorded to `watch.jsonl` | Needs a terminal of at least 120×30 |
| `--capture` | flag | off (smoke: on) | Capture released events, then run the replay test | Needs the journal on disk |
| `--audit` | flag | off (smoke signed: on) | Re-check every signature in the journal after the run | Signed runs only |
| `--stamps` | `on`, `off` | `on` | Per-stage timestamps. Off: only the release rate is known | – |
| `--release-log` | flag | off | Standard output becomes the binary release log; the report goes to standard error; keeps the journal | Used by the kill test. Redirect standard output to a file |
| `--allow-generator-limited` | flag | off | A late sender is flagged instead of making the run invalid | Local development only |

### Options for one subcommand

| Option | Subcommand | Values | Default | Meaning |
|---|---|---|---|---|
| `--quick` | `probe` | flag | off | A 10-second probe instead of 2 minutes |
| `--kind` | `sweep` | `load`, `commit-interval`, `stamps`, `headline`, `polymarket` | required | Which sweep |
| `--limit` | `search` | duration | core-path 50 µs; others the durable limit | The latency limit |
| `--start` | `search` | rate | 100k | The first rate tried |
| `--search` | `ablate` | flag | off | Also run the searches |

### Run names

A run's folder name is made from its options: `<mode>-<rate>`, then any of
`-polymarket`, `-makers<K>`, `-bursts-<preset>`, `-shock-<size>`, `-eip712`, `-discard`,
`-T<dur>` (when not 1 ms), `-B<n>` (when not 4096), `-stamps-off`, in that order.
Examples: `signed-10k`, `preverified-200k`, `signed-10k-polymarket-makers3-bursts-median-shock-stress`.
The seed, the timing, the gateway count and the verifier are **not** in the name.

---

## 9. Supply-chain and code-quality checks

These check the dependencies (the outside code this project uses). They all need the
network, so they run with `--net` or `--write`. None of them compiles anything.
`docs/SUPPLY-CHAIN.md` is the log of what they found.

```sh
./dev --net cargo audit
```
- **Checks:** every crate in `Cargo.lock` against the RustSec database of known security
  advisories.
- **You'll see:** no vulnerabilities, or a list.

```sh
./dev --net cargo deny check
```
- **Checks** four things, set in `deny.toml`: advisories (yanked crates are denied);
  licenses (only MIT, Apache-2.0, BSD-3-Clause, ISC and a few similar ones); banned crates
  (OpenSSL, native-tls, aws-lc and others; no wildcard versions); sources (crates.io only).
- **You'll see:** ok or failed, per section.

```sh
./dev --write cargo vet --locked
```
- **Checks:** that every locked crate has been reviewed by a trusted organisation (Mozilla,
  Google, Bytecode Alliance, ISRG, Zcash) or is listed as an exemption. May write only
  inside `supply-chain/`.
- **You'll see:** pass or fail with counts (last log: 29 fully audited, 110 exempted).

```sh
./dev --net tools/supply-chain/check-ages.sh
```
- **Checks:** the publish date and download count of every crate in `Cargo.lock` (139 now).
  Fails if any version is younger than 14 days.
- **How long:** at least about 5 minutes (it waits 1 s between requests).
- **You'll see:** a table `crate version age_d downloads`; exit 1 if a version is too young.
- Type the path **without** a leading `./`, or `./dev` refuses it.

```sh
ALLOW="rustls@0.23.45" ./dev --net tools/supply-chain/check-ages.sh
```
The same, reporting that one young version as allowed instead of failing.

```sh
./dev --net tools/supply-chain/list-build-time-code.sh
```
- **Lists:** every crate whose code runs during the build (build scripts, macros and what
  they depend on). That code is the main supply-chain risk.
- **You'll see:** lines of `name`, `version`, and why it runs at build time.

```sh
./dev sh -c 'cat /cargo/registry/src/*/<crate>-<version>/build.rs'
```
Prints one dependency's build script from the download cache, to read it. Replace
`<crate>-<version>`. Offline, in seconds.

```sh
./dev --net cargo tree
```
Prints the dependency tree.

```sh
./dev --write cargo update --dry-run
```
Shows what an update would change, without touching `Cargo.lock` (it only refreshes
cargo's index cache).

```sh
./dev --write cargo update
```
Actually updates `Cargo.lock`. After any lockfile change, run the age check, audit, deny
and vet again, then `./dev --net cargo fetch --locked`.

Code quality (clippy and fmt) is in [section 3](#lint-clippy).

---

## 10. Market data tools

These record Polymarket Perps' public market data. They were used to shape the
Polymarket flow. You don't need them to build, test or run anything.

### The recorder and the history puller

```sh
./dev record build
```
Builds the recorder program. Needs the `cargo fetch` from section 1 first.

```sh
./dev record start
```
- **Starts two background containers:**
  - `recorder`: listens to Polymarket Perps' public websocket (tickers, order books, trades)
    and writes `./data/ws/YYYY-MM-DD/HH.jsonl` (UTC), gzipped when each hour ends. About
    2 GB a day.
  - `history`: pulls instruments, funding and 1-second mark prices from their public API
    into `./data/rest/`, once an hour.
- Both run on the last CPU and restart by themselves after a crash or a reboot.
- Starting again while running briefly interrupts recording.

```sh
./dev record status
```
Shows whether they are running. `./dev record` alone does the same.

```sh
./dev record logs
```
The last 50 log lines of both. The recorder writes a statistics line every minute.

```sh
docker compose logs -f recorder history
```
Follows the logs live (plain Docker, run in the repo folder). Ctrl-C to stop following.

```sh
./dev record stop
```
Stops both. They stay stopped after a reboot.

`./data` is never committed.

### Calibration

This is the one tool that runs on your machine, not in Docker: Python 3, standard library
only, no pip, no network.

```sh
python3 tools/calibrate/calibrate.py generate --check
```
Checks that the Polymarket flow's Rust table matches the committed profile. Writes nothing.
Under a second.

```sh
python3 tools/calibrate/calibrate.py extract --check
```
Checks that the committed profile is what the recorded data produces. Writes nothing.
Needs `./data`. About 3 minutes.

`generate` and `extract` without `--check` rewrite the table or the profile. Use them only
after changing the method.

---

## 11. Where files go and cleaning up

### Where things are

| Docker volume | Seen inside as | Holds |
|---|---|---|
| `perps_target` | `/target` | All build output and every run (`/target/runs/session`) |
| `perps_cargo-home` | `/cargo` | Downloaded crates |
| `perps_advisories` | `/advisories` | The security advisory databases |
| `perps_recorder-target` | `/target` (recorder only) | The recorder program |

On your machine: `./data` holds the market recordings. Test runs use the container's
`/tmp` and vanish when it exits.

`/target` lives inside Docker, so your file browser can't see it. Inside `./dev`, the source
is read-only, so keep every `--dir` and `--run-dir` under `/target`.

### Looking at runs

```sh
./dev ls -R /target/runs/session
```
Lists every file of the default session.

```sh
./dev cat /target/runs/session/runs/signed-10k/report.md
```
Prints one run's report.

### Getting files out

These follow from how `./dev` works. If the first two fail, use the plain-Docker one.

```sh
./dev cat /target/runs/session/report.md > session-report.md
```
Copies one file to your current folder. It arrives byte for byte, because `./dev` turns
the terminal off when output goes to a file.

```sh
./dev tar -C /target/runs -cf - session > session.tar && tar -xf session.tar
```
Copies the whole session folder.

```sh
mkdir -p out && docker run --rm --network none -u "$(id -u):$(id -g)" -v perps_target:/target:ro -v "$PWD/out:/out" perps-dev:1.98.1 cp -r /target/runs/session /out/
```
The same with plain Docker, into `./out/session`. Create `out` first, as here, or Docker
makes it owned by root. Run it from a folder under your home folder (the repo folder is
fine): from a folder under `/tmp` it failed with `Permission denied` on a machine whose
Docker is installed as a snap.

### Cleaning up

```sh
./dev du -sh /target/runs
```
How much disk the runs take.

```sh
docker system df -v
```
How much disk every Docker image and volume takes.

```sh
./dev rm -rf /target/runs/session/runs/signed-5k
```
Deletes one run (here the smoke run, with its kept journal).

```sh
./dev rm -rf /target/runs/session
```
Deletes the whole default session. The next run starts a fresh one.

```sh
docker volume rm perps_target
```
Deletes all build output and all runs. The next build compiles everything again (no
download needed). Fails while a container uses it.

```sh
docker volume rm perps_cargo-home
```
Deletes the downloaded crates. You then need `./dev --net cargo fetch --locked` again.

```sh
docker image rm perps-dev:1.98.1
```
Deletes the image. You then need `./dev image` again (about 10 minutes).

---

## 12. Troubleshooting

### "refusing to run rustc on the host"
```
perps: refusing to run rustc on the host; build inside ./dev (INFO.md 5a)
```
You ran `cargo` directly. Put `./dev` in front: `./dev cargo test ...`. This is on purpose
([section 2](#why-cargo-on-your-machine-is-blocked)).

### The verdict says INVALID on a laptop
Expected, usually `generator-limited`. See [section 4.9](#49-what-the-verdict-means-on-a-laptop).
If the reason is a count mismatch, a replay that differs or an audit failure, that is a bug.

### The panel scrolls or repeats itself
The terminal is smaller than the panel. Make it at least 120 columns × 30 lines
(check with `echo "$(tput cols) x $(tput lines)"`), then run again.

### The panel prints one line a second
Something is not a terminal: you redirected or piped the output (`> file`, `| less`).
`./dev` only gives the container a terminal when both input and output are one. Run the
`--watch` command on its own; the report is saved in the run folder anyway.

### "already holds a journal"
```
e2e: journal: <dir>/journal already holds a journal: recover and resume it (13.5), or start a new journal in a new directory…
```
That run folder kept its journal (smoke runs, `--keep-journal`, `--release-log`, or a run
you stopped half-way). Any one of these:
- give the new run another name: add `--name smoke2`;
- delete the old folder: `./dev rm -rf /target/runs/session/runs/signed-5k`;
- use another session: add `--dir /target/runs/other`.

### "already ran with … but this run has …"
A sweep, search or ablation found a finished run of the same name with other settings.
Use another `--dir`, or delete that run's folder.

### Pinning is refused
```
… too few for the default … layout with N gateways; give --cpus
```
The machine has too few CPUs for one thread per CPU. Add `--unpinned`.

### "the libsecp256k1 verifier is not in this build"
```
--verifier: the libsecp256k1 verifier is not in this build…
```
Add `--features c-secp256k1` to the cargo part of the command (before `--bin e2e`). The
first such build compiles the C library; the compiler is already in the image.

### "--makers needs --flow polymarket" / "--shock needs --flow polymarket"
Add `--flow polymarket`. `--bursts` works with either flow.

### "the gateways refused the harness's own EIP-712 messages"
EIP-712 messages expire 5 minutes after signing. Shorten `--window`. The timed flow
(warm-up + window + tail) can be at most 3 minutes with `--auth eip712`.

### "--net runs only download/inspect commands"
You used `--net` with a command it doesn't allow, or typed `./tools/...`. Type the script
path without the leading `./`: `./dev --net tools/supply-chain/check-ages.sh`.

### "the run directory's file system is refused" / "keeps files in memory only"
```
e2e: <dir>: tmpfs keeps files in memory only; choose another directory
```
The folder is on a RAM file system, where "on disk" means nothing (the first message comes
from `probe`, the second from the other commands). Keep `--dir` under `/target`.

### Out of disk
Kept journals and sessions add up, and so does build output. Check with
`./dev du -sh /target/runs` and `docker system df -v`, then delete old runs
([section 11](#cleaning-up)). Runs without `--keep-journal` delete their journal
themselves.

---

## 13. Glossary

- **Ablation:** a run with one design choice switched off, to show what that choice is worth.
- **Achieved rate:** orders scheduled inside the window that got sequenced, divided by the
  window length. Compare with the offered rate.
- **Audit (signature audit):** after a run, re-checking every signature in the journal.
  Failures must be 0.
- **B (max batch):** the most records one journal flush may hold. Default 4,096.
- **Capture:** the gate writes every released event to `events.bin`, so the replay test can
  compare against it.
- **Ceiling:** the highest rate at which a latency limit still holds. Found by a search.
- **Clippy:** Rust's linter: it warns about likely mistakes.
- **Core:** the one thread that runs the engine: margin checks, matching, liquidations.
- **Core path:** the time the core spends on one command, from sequencing to its result.
- **Crate:** a Rust package. This repo's are `engine`, `pipeline`, `gateway`, `loadgen`,
  `bench` and `recorder`.
- **Criterion:** the library the microbenchmarks use.
- **Deterministic:** the same input always gives exactly the same output. Here: replaying
  the journal rebuilds the exact same events and state.
- **Docker volume:** storage that Docker keeps between containers. `/target` is one.
- **Drain:** after the flow ends, the pipeline finishes what is queued, then stops.
- **Durable ack:** a result released only after its order was flushed to disk.
- **Durable limit:** the latency limit for durable searches and the headline:
  2 × T plus the median fdatasync p99 of a fixed 20k/s pre-verified run (e.g. 2 × 1 ms +
  0.5 ms = 2.5 ms).
- **EIP-712:** the signing format Polymarket Perps uses (`--auth eip712`). The order is
  encoded, hashed with keccak-256, and signed with a salt and a millisecond timestamp.
- **fdatasync:** the Linux call that forces written data onto the disk. Its time is the
  disk's latency.
- **Flags:** notes printed with a run that don't make it invalid.
- **Flow:** the stream of orders the sender plays. *M3* or *Polymarket-shaped*.
- **Gate:** the thread that releases results only once their command is on disk.
- **Gateway:** a thread that decodes a signed message, checks its nonce or timestamp and
  its signature, and passes it on. Account `a` always uses gateway `a mod G`.
- **Generator-limited:** the sender ran more than 5 µs late (p99). The run is invalid
  because the measuring tool, not the pipeline, was the limit.
- **Group commit:** the journal flushes many records with one write and one fdatasync, when
  4,096 are waiting or the oldest is 1 ms old.
- **Headline:** the main claim: 100k signed orders a second, durable, with p99 under the
  durable limit, in 3 valid runs. Answer: yes, no, or not decided yet.
- **Insurance fund:** the account that takes over liquidated positions. It holds only
  $1,000 on purpose, so runs exercise the shortfall path.
- **Jitter:** how much a core's timing wobbles. The probe ranks cores by it.
- **Journal:** the on-disk log of every sequenced command. Replaying it rebuilds the state.
- **k256:** a pure-Rust signature library. The default verifier.
- **Lane:** an input queue into the sequencer. Each gateway fills one; pre-verified runs
  fill them directly.
- **libsecp256k1:** Bitcoin Core's C signature library. Opt-in, roughly twice as fast as k256.
- **Limited by:** the report's label for what stopped a run going faster.
- **Liquidation:** when a price move pushes an account past its margin, its orders are
  cancelled and its position moves to the insurance fund.
- **M3 flow:** the default synthetic flow: 67 markets, about 100k commands per flow-second,
  with sudden 2%–6% price jumps.
- **Mark price:** the price used for margin and liquidation, set by the operator. Each
  update re-checks margin.
- **Nonce:** a number each account's orders carry that must keep going up, so an old
  message can't be replayed.
- **NUMA:** a machine whose memory is split between CPU packages. The benchmark wants a
  single node (or NUMA balancing off).
- **Offered rate:** what `--rate` sets: orders per second the sender sends. Not a cap.
- **Open loop:** send times are fixed in advance; the sender never waits for the pipeline.
  Lateness shows up as latency, not as a lower rate.
- **p50 / p99 / p99.9:** 50%, 99% or 99.9% of orders were at or below this latency.
- **Page fault (minor):** the first touch of a memory page, or a page the kernel moved.
  Hot threads should have none during the window.
- **Pinning:** binding each thread to one CPU, so nothing else runs there.
- **Poisson arrivals:** random send times at an average rate, like real traffic.
- **Polymarket-shaped flow:** Polymarket Perps' 88 real markets, with the traffic shape of
  their recorded data, at about 100 times their volume.
- **Pre-verified:** commands sent unsigned, straight into the pipeline. Measures the core
  and journal without signature checks.
- **Probe:** `e2e probe`, which measures the machine before a session.
- **Replay test:** rebuilding the run from its journal and comparing with what the live run
  released. Must say `identical`.
- **Saturation:** what a search's final run at twice its result achieved, and what limited it.
- **Search:** finding the highest rate at which a latency limit holds.
- **Segment:** one journal file (1 GiB by default).
- **Sequencer:** the one thread that puts every command in a single numbered order.
- **Session:** a folder (`--dir`) holding a probe, runs and one combined report.
- **Setup:** creating markets, funding accounts and seeding the books before the timed
  flow. Journaled but not measured.
- **Signed:** orders signed by their account's key and checked by the gateways. The
  end-to-end case.
- **Smoke run:** a tiny run (`--smoke`) that exercises every stage in seconds.
- **Stamps:** per-stage timestamps used to measure latency (`--stamps`).
- **Sweep:** a set of runs at different settings, each repeated 3 times.
- **T (commit interval):** the longest a journal record waits before a flush. Default 1 ms.
- **Tail:** 200 ms of flow after the window, so the window's last orders are released under
  load.
- **Throttling:** the kernel pausing a container that used more than its CPU quota. Makes
  a run invalid.
- **Torn tail:** the half-written end of a journal after a crash. `recover` copies it aside
  and zeroes it.
- **tsc:** the CPU's own fast clock. Headline numbers need it.
- **Verdict:** `valid` or `invalid` with reasons. Invalid runs don't count.
- **Warm-up:** the first 5 s of the flow. Sent but not measured.
- **Window:** the measured period, 30 s by default. An order counts if its *scheduled*
  send time falls inside it.
- **WSL2:** Windows' Linux layer. Its virtual disk may absorb flushes, so its disk numbers
  are not trustworthy.
