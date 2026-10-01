# tools/calibrate: the Polymarket-shaped flow's profile

Turns ~22 hours of recorded Polymarket Perps public data into the calibrated profile of the
Polymarket-shaped flow (`docs/DECISIONS.md` D-034; spec `docs/PIPELINE.md` 14.12): the 88
real markets and the distributions a flow draws from. Two steps, each a pure function of
its inputs:

| Step | Reads | Writes | Time |
|---|---|---|---|
| `extract` | `data/` (the recordings, never in git) and `inputs/` | `profile-2026-09-30.json`: derived parameters only | about 3 min on 6 processes |
| `generate` | `profile-2026-09-30.json` | `loadgen/src/market_flow/polymarket_profile.rs`: a Rust `static` table | under a second |

```sh
python3 tools/calibrate/calibrate.py generate --check   # the Rust table is what the JSON makes, byte for byte
python3 tools/calibrate/calibrate.py extract --check    # the JSON is what the data makes (needs data/)
python3 tools/calibrate/calibrate.py generate           # after changing the JSON or rust.py
python3 tools/calibrate/calibrate.py extract            # after changing the method (then generate)
python3 tools/calibrate/calibrate.py extract --check --data /path/to/data   # recordings elsewhere
```

`--check` writes nothing and exits 1 at the first differing line; `--data DIR` reads the
recordings from `DIR` instead of the repo's `data/` (only `extract` reads them). Run
`generate --check` after any change to the tool, the JSON or the Rust file: the cargo
tests check the table's invariants (`loadgen/src/market_flow/profile/tests.rs`), not that
it is the tool's output. The generated file is already in rustfmt's layout, so `./dev fmt`
leaves it alone. On 2026-09-30 both checks said "identical" (`extract --check`: 3 min 6 s
on 6 processes). `generate` writes the JSON's SHA-256 into the table (`POLYMARKET.sha256`),
and the flow's digest hashes it, so a regenerated table gives a new digest by itself and an
arena saved from the old table is refused; a cargo test checks the hash against the
committed JSON. (`FLOW_VERSION` in `polymarket.rs` is bumped for changes to the generator's
code.)

**Rules.** Python 3 standard library only, run on the host (the only Python this project
runs; everything Rust builds and tests in Docker, `./dev`). Read-only on `data/`. `extract`
runs its workers off the last CPU, which the recorders use, as `./dev` keeps builds off it.

## Files

| File | What |
|---|---|
| `calibrate.py` | the command line; the JSON layout; the `--check` comparisons |
| `scan.py` | one pass over `data/`, one worker process per hour file |
| `derive.py` | from the scan and `inputs/` to the profile (every fit and table) |
| `rust.py` | the profile to Rust, in rustfmt's layout |
| `inputs/flow.json`, `inputs/book.json` | outputs of the first calibration (2026-09-30, session scratchpad `calib/`), copied verbatim so the repo is self-contained; their SHA-256 is in the profile's `about` |
| `profile-2026-09-30.json` | the profile |

The Rust types, with their units and invariants, are in `loadgen/src/market_flow/profile.rs`.

## Sample

From `data/rest/instruments/2026-09-30.json` (88 instruments; identical to the 2026-09-29
snapshot apart from logo URLs) and the hourly websocket files `data/ws/<day>/<hh>.jsonl.gz`,
where the recorder keeps the last `book::` and `tickers::` frame of each server second and
every `trades::` frame. Four windows:

- **Start prices:** the last `tickers::<iid>` frame at or before T0 = 2026-09-30
  10:00:00.000 UTC, field `mark` (all 88 at 09:59:59.9).
- **Clean window**, for the per-market weights: 2026-09-29 15:04:54.611 UTC (the first frame
  of the multi-connection recorder; before it one connection recorded books for only 50
  markets) to 2026-09-30 15:00:00 UTC: 22.08 effective hours, since 2026-09-29 17:10 to
  19:00 is a recording gap (a DNS outage; `18.jsonl.gz` is empty).
- **Flow sample**, for taker rates and clusters and for jumps: every recorded second of the
  hour files 2026-09-29 14 to 2026-09-30 14 (about 22.5 hours). Jump rates use its 22 full
  hours with all 88 markets (2026-09-29 15, 16, 19 to 23; 2026-09-30 00 to 14).
- **Shape hours**, for the book shape: 2026-09-29 22, 2026-09-30 03 and 2026-09-30 15 UTC (an
  evening hour after the US close, a night hour, a US-session hour): the independent check's
  hold-out hours, whose values are the corrected ones (below).

## Method

Units follow D-004: a market's price is in ticks of `10^-pd` dollars, its size in lots of
`10^-qd` units, `pd + qd = 6`, so ticks × lots are micro-dollars. Polymarket's real price grid
is 5 significant figures: `10^max(0, digits(price) − 5)` of our ticks. No print of the sample,
and no book level of the hours checked, is off it.

Every number a flow draws its content from is an integer (loadgen's rule: `loadgen/src/lib.rs`):
shares in parts per million, summing to exactly 1,000,000 where they split a whole (largest
remainders); fitted parameters in thousandths. A continuous distribution is given twice: its
fitted parameters, and a table of equally likely values, its quantiles at `(i + 0.5) / n`,
which is what a flow samples, with no floats. The burst models' AR(1) parameters are the
only floats: they move send times, never the plan (`docs/PIPELINE.md` 14.9).

**Re-derived from `data/` by `extract`:**

- *Instruments:* id, symbol, `pd`, `qd`, maximum leverage, the tier table (1 to 8 rows, 12
  distinct), `max_market_notional`, as published. Start price and its grid.
- *Classes:* book shape: majors (BTC, ETH, SOL); alt crypto (crypto of 10x or more); long-tail
  crypto (5x or less); tradfi macro (SP500, NAS100, GOLD, SILVER, WTIOIL, BRENTOIL); tradfi
  equities (the other 42). Price moves: by maximum leverage, 50x, 20x, 10x, 3x and 5x.
- *Maker weight:* each market's share of "inner" level changes over the clean window: two
  consecutive 1-s snapshots (at most 5 s apart) compared at the prices both show, a level
  counting once if it appeared, disappeared or changed size. A lower bound on maker messages.
- *Taker weight:* each market's share of taker events over the clean window; a taker event is
  the prints (deduplicated by trade id) of one market, side and millisecond.
- *Spread* (shape hours): a market is tick-bound if its median spread is one real tick (BTC,
  ETH, SOL, SP500, NAS100, GOLD): always one real tick. Otherwise a split lognormal in bps,
  median and log-sds `ln(p50/p10) / z90` below and `ln(p90/p50) / z90` above, pooled over a
  class's other markets (histogram quantiles at 0.01 bps); SILVER, WTIOIL and BRENTOIL each
  get their own.
- *Gaps* between consecutive levels of a side, in real ticks, per class and per bucket (gaps
  after levels 1–4, 5–9, 10–19): the share of each gap of 1 to 10 ticks, and beyond that
  `10 + exp(N(mu, sd))`, fitted to `ln(gap − 10)`. Counted only on the sides that show all
  20 levels (57% of long-tail sides, 69% of equities', all the majors' and macro's): a
  flow's ladder always shows 20, and the thinner sides' far gaps (stale orders, hundreds of
  bps out) would put its deep levels far beyond where a 20-level book has them.
- *Level sizes* (shape hours), per class and per level, 1 to 20: dust ($10 to $12.50); a clip
  from the menu, within ±3% of $6,250 (alt and long-tail crypto) or of $25,000, $50,000 and
  $100,000 (tradfi); else that level's background: its other sizes' own quantiles (log bins
  of 1/200 decade). The majors have no clip. Dust per market, and per class and level. Level
  by level, since depth is not flat: level 1 is thin (a median of $1k to $3.7k), and the
  clips sit at levels 3 to 10. D-034's draft said "a lognormal background": the table is
  empirical, since the background is bimodal (small orders and large ones) and a lognormal
  puts its top values 3 to 10 times too high (tradfi equities' largest of 64 values over all
  levels: $5.9M from the lognormal, $0.86M recorded).
- *Jumps:* persistent 1-s mark moves over 50 bps whose move from the second before to 5 s
  after is still over 25 bps (half the threshold): the rate is the median over the full
  hours of jumps per market-hour, pooled over all markets (the 50x markets had none, the
  20x ones 0.011 per market-hour); the sizes come from all 276.
- *Takers:* share of messages (clean-window taker events against inner level changes plus
  taker events), events a second, buy share; clusters: events chained while each follows the
  previous one, exchange-wide, by at most 50 ms: starts a second, sizes, same market as the
  previous order, same side as the first, gaps.

`extract` reproduces the first calibration exactly where it overlaps: all 88 start prices,
the inner level changes of every market (66,281,072) and its taker events (46,093) and prints
(62,006).

**Taken from `inputs/`** (fits not re-derived here):

- `flow.json`, `price_dynamics_1s`: per max-leverage class, the share of 1-s mark changes of
  zero and the 3-component normal scale mixture of the other moves, standardised by each
  market's RMS; each market's own 1-s standard deviation, turned into the RMS the mixture
  scales (`sigma / sqrt(1 − no-move share)`). The half-normal table is computed here.
- `flow.json`, `takers.notional_usd_sampler`: dust share, the 11 point masses, the
  two-lognormal mixture truncated below $12.50 (its 1,024-value table computed here).
- `flow.json`, `burstiness.rate_multiplier_models`: the median-hour and busiest-hour AR(1)
  pairs.
- `flow.json`, `correlated_shocks`: rate, the markets' Pareto alpha (above 8 markets; turned
  here into shares for 14 to 88 markets, clamped: every draw up to 14 on 14 and every draw
  from 88 on 88, so the median is the recorded 14 and the 90th percentile 38, against 31
  recorded; 4.1% of shocks move more than the recorded maximum of 71), move sizes (in units
  of each market's RMS of nonzero 1-s moves), time spread.
- `book.json`, `overall.requote_1s_lower_bound.all21`: the maker change mix (add, remove, size
  up, size down) and its share by level bucket.

## Corrections from the independent check

Each part of the first calibration was re-checked by an independent script on hold-out hours
(scratchpad `calib/check-*`); where they differed, the check's value is the one used:

| Parameter | First calibration | Used | Why, and how here |
|---|---|---|---|
| Maker activity across markets, lognormal sigma of ln(share) | 0.72 | 0.49 | the sample sd is inflated by a few very quiet markets (NCLD); 0.49 reproduces the observed top-10 share of 23%. The table holds each market's real weight (its top 10: 23.4%), so no sigma is needed |
| Majors' spread | 1 real tick 86% of the time | 1 real tick | 97–99% outside the US open; the 86% came from a sample holding the 13:30 open. `OneTick` |
| Spreads, alt / long-tail / equities | one lognormal per class | split lognormal 5.14/0.85/0.25, 5.30/0.62/0.95, 4.17/0.66/0.34 (median bps, sd below, sd above) | a lognormal can't fit the skew; the check's hold-out hours give these, re-derived here exactly |
| Gap tail beyond 10 ticks | lognormal in bps | `10 + exp(N(mu, sd))` ticks per class and bucket | re-derived on the shape hours |
| Clip menu | greedy peaks ($6.7k, $75k, …) | $6,250 crypto (13% of alt levels); $25k, $50k, $100k tradfi (4%, 10%, 8% pooled); jitter ±3% | the clips are fixed notionals near these; shares within ±3%, re-derived, level by level |
| Persistent jumps over 50 bps | 0.140 per market-hour | 0.051 | the mean is set by one episode: 111 of the 276 jumps fell in 2026-09-29 23h, 92 of them LIT-USD's; the median full hour, re-derived |
| Taker cluster starts | 0.464 a second | 0.451 | the first divided the clusters of all 22.5 hours by the 21.9 full hours; re-derived per recorded second |
| Taker notional | — | 14.6% dust drawn from [$10.50, $11.60), capped at the market's maximum, lots rounded up to reach $10 | a flow's rules (`TakerNotional`) |

## What the table does not hold

A flow's own choices, which the generator makes (`loadgen/src/market_flow/polymarket.rs`,
`docs/PIPELINE.md` 14.12):
- each market's band and price range: Polymarket's bands, `1 / max_leverage` (2% at 50x to
  a third at 3x), break our band rule 1 (RISK.md 5.3), so the flow uses
  `400,000 / max_leverage` ppm, and half to twice the start price;
- fees (the M3 flow's classes), accounts, deposits and leverage;
- the scale: a fixed 100,000 client messages per second of flow time, whatever the offered
  rate (`messages_per_flow_second`), so at an offered 100k/s flow time runs at about real
  time;
- how often shocks come (every 10 s with `--shock`, against the recorded one per 347 s),
  and the calibrated shock size's tail beyond the recorded 90th percentile.

The limits of the data stand: one day (one US open, one macro release, no weekend); book
changes are 1-s lower bounds; the account structure is unknown.
