# perps: a deterministic perpetual-futures exchange engine in Rust

A small, single-operator perpetual-futures exchange core, built to show with measured
numbers where the time goes at 100k signed orders per second, and to show perp mechanics
(isolated margin, liquidations) on top of an order book. It is modelled on Polymarket
Perps' published rules.

AI-assisted: I designed it, made the major calls and verified the results; Claude Code
wrote the code. Every decision and its evidence is in [`docs/DECISIONS.md`](docs/DECISIONS.md).

**Headline** (AMD Threadripper 9960X, consumer NVMe): 100k signed orders/s end to end,
each acknowledged only once it is on disk, at p99 2.21 ms. Signed ceiling at the 3.11 ms
limit: 400k/s with `k256`, 800k/s with `libsecp256k1`. The matching and risk core alone:
3M orders/s. Every journal replays to an identical state.

| Doc | What's in it |
|---|---|
| [`docs/COMMANDS.md`](docs/COMMANDS.md) | **Start here:** every command, what it does and how to read its output |
| [`docs/PIPELINE.md`](docs/PIPELINE.md) | The pipeline: threads, rings, journal, gating, how it is measured |
| [`docs/RISK.md`](docs/RISK.md) | Margin, liquidation and insurance-fund rules |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | Measured numbers, with machine and commit |
| [`docs/DECISIONS.md`](docs/DECISIONS.md) | Why each choice was made, with the alternatives and the evidence |
| [`docs/SUPPLY-CHAIN.md`](docs/SUPPLY-CHAIN.md) | Dependency review log |

## Running it

The full guide, in plain language, is [`docs/COMMANDS.md`](docs/COMMANDS.md). The short version:

Everything builds and runs inside Docker; nothing is compiled on the host. You need
Docker with Compose v2 and about 2 GB of disk.

```sh
git clone https://github.com/suzuenhasa/perps-engine.git && cd perps-engine
./dev image                                       # build the container image (first time only; ~10 minutes)
./dev --net cargo fetch --locked                  # download the pinned dependencies (compiles nothing)
./dev cargo test --workspace --locked --offline   # build and test: no network, source read-only
```

Then run the pipeline end to end:

```sh
# A smoke run: 3,000 signed orders through every stage
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --smoke

# 10k signed orders/s for 10 s, then the same on the Polymarket-shaped flow
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 10s --gateways 2
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 10s --gateways 2 --flow polymarket

# The same with a live panel (needs a terminal of at least 120 x 30), then the journal
# replay and the signature audit
./dev cargo run --release --locked --offline -p bench --bin e2e -- run --mode signed --rate 10k --window 30s --gateways 2 --watch --capture --audit
```

Each run prints its report: rates, latency per stage and end to end, thread load and a
verdict (the smoke run also replays its journal and re-checks every signature). With
`--watch` it draws the run live instead (stages, rates, fills, thread load), ends with a
short result, and records the panel to `watch.jsonl`. On a laptop or VM the
verdict will usually be INVALID (for example, the sender ran late on a shared CPU): the
harness refuses to count a run it can't trust. The headline numbers need a dedicated
machine with enough cores to pin every thread (`docs/RUNBOOK-PERPSBOX.md`). Runs land in
the container's `/target/runs/session`.

`./dev cargo bench -p bench --locked --offline` runs the microbenchmarks.

Running `cargo` on the host fails on purpose: `.cargo/config.toml` routes every compiler
call through `tools/host-guard/`, which only allows it inside the containers. `./dev`
builds with the source read-only, `.git` hidden and no network, so no dependency's build
script can phone home or touch the source (D-001, D-006).

## Layout

```
engine/          pure deterministic logic, no I/O, no deps: types, commands/events,
                 reference book (the spec), fast book, risk engine
pipeline/        threads, rings, sequencer, group-commit journal, gate, replay
gateway/         decoding, signature verification (perp and EIP-712 schemes), backpressure
loadgen/         deterministic synthetic order flows (M3 and Polymarket-shaped)
bench/           the end-to-end harness (e2e) and criterion microbenchmarks
tools/           Polymarket Perps recorder and history puller, flow calibration,
                 supply-chain checks, host guard
docker/, compose.yaml, dev   the build environment
docs/            pipeline and risk specs, decisions, benchmarks, supply-chain log
```
